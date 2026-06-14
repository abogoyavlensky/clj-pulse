use std::path::{Path, PathBuf};

/// The dependency-management flavor of a project, decided by its manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    /// Clojure project (`deps.edn` / `project.clj`, classpath via `.cpcache`).
    Clojure,
    /// let-go project (`lgx.edn`, deps via `~/.lgx/gitlibs`).
    LetGo,
}

/// A project with an `lgx.edn` is a let-go project; otherwise Clojure.
pub fn project_kind(root: &Path) -> ProjectKind {
    if root.join("lgx.edn").exists() {
        ProjectKind::LetGo
    } else {
        ProjectKind::Clojure
    }
}

/// Whether `path` is a Clojure source file we provide language intelligence
/// for (`.clj`, `.cljs`, `.cljc`, and let-go `.lg`). Config files are not
/// source and must not be indexed or linted: EDN ones (`deps.edn` / `lgx.edn`)
/// are excluded by extension, and Leiningen's `project.clj` — a build manifest
/// whose dependency coordinates would otherwise be flagged as unresolved
/// namespaces — is excluded by name despite its `.clj` extension.
pub fn is_clojure_source(path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) == Some("project.clj") {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("clj") | Some("cljs") | Some("cljc") | Some("lg")
    )
}

pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        if dir.join("deps.edn").exists()
            || dir.join("project.clj").exists()
            || dir.join("lgx.edn").exists()
        {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The directories to index as the project's own source. Declared roots
/// (deps.edn top-level `:paths` plus every alias's `:extra-paths`, or
/// `lgx.edn` `:paths`) are unioned with the conventional `src`/`test`
/// defaults so that test/dev usages are indexed at startup even when not
/// opened. Non-existent directories are skipped later by the file walker.
pub fn source_paths(root: &Path) -> Vec<PathBuf> {
    let declared = if project_kind(root) == ProjectKind::LetGo {
        std::fs::read_to_string(root.join("lgx.edn"))
            .ok()
            .map(|c| crate::lgx::paths(&c))
            .unwrap_or_default()
    } else {
        // deps.edn `:paths` is authoritative; fall back to a Leiningen
        // `project.clj`'s `:source-paths`/`:test-paths` only when deps.edn
        // declares nothing.
        std::fs::read_to_string(root.join("deps.edn"))
            .ok()
            .and_then(|c| parse_paths_from_deps_edn(&c))
            .unwrap_or_else(|| {
                std::fs::read_to_string(root.join("project.clj"))
                    .ok()
                    .map(|c| crate::leiningen::source_paths(&c))
                    .unwrap_or_default()
            })
    };

    let mut rel: Vec<String> = Vec::new();
    for p in declared
        .into_iter()
        .chain(["src".to_string(), "test".to_string()])
    {
        if !rel.contains(&p) {
            rel.push(p);
        }
    }
    rel.into_iter().map(|p| root.join(p)).collect()
}

/// Declared source roots in a `deps.edn`: top-level `:paths` plus every
/// alias's `:extra-paths`. An alias's own `:paths` *replaces* the base paths
/// (typically build tooling, e.g. tools.build `:build`) and is intentionally
/// ignored. Returns `None` when nothing is declared or the EDN is malformed.
fn parse_paths_from_deps_edn(contents: &str) -> Option<Vec<String>> {
    use crate::edn::{get, kw, str_vec_at};

    let Ok(edn_format::Value::Map(top)) = edn_format::parse_str(contents) else {
        return None;
    };

    let mut paths: Vec<String> = str_vec_at(&top, kw("paths")).unwrap_or_default();

    if let Some(edn_format::Value::Map(aliases)) = get(&top, kw("aliases")) {
        for alias in aliases.values() {
            if let edn_format::Value::Map(spec) = alias {
                if let Some(extra) = str_vec_at(spec, kw("extra-paths")) {
                    for p in extra {
                        if !paths.contains(&p) {
                            paths.push(p);
                        }
                    }
                }
            }
        }
    }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_top_level_paths() {
        let edn = r#"{:paths ["src" "resources"]
                      :deps {org.clojure/clojure {:mvn/version "1.11.1"}}}"#;
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string(), "resources".to_string()])
        );
    }

    #[test]
    fn test_alias_paths_are_ignored() {
        // tools.build convention: :paths inside the :build alias must not be
        // mistaken for the project's source paths.
        let edn = r#"{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}
                      :aliases {:build {:paths ["build"]
                                        :deps {io.github.clojure/tools.build {:mvn/version "0.9.6"}}}}}"#;
        assert_eq!(parse_paths_from_deps_edn(edn), None);
    }

    #[test]
    fn test_top_level_paths_after_aliases() {
        let edn = r#"{:aliases {:build {:paths ["build"]}}
                      :paths ["src" "lib"]}"#;
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string(), "lib".to_string()])
        );
    }

    #[test]
    fn test_paths_in_comment_ignored() {
        let edn = ";; :paths [\"nope\"]\n{:paths [\"src\"]}";
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string()])
        );
    }

    #[test]
    fn test_paths_in_string_ignored() {
        let edn = r#"{:description ":paths [\"nope\"]"
                      :paths ["src"]}"#;
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string()])
        );
    }

    #[test]
    fn test_no_paths_returns_none() {
        let edn = r#"{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}}"#;
        assert_eq!(parse_paths_from_deps_edn(edn), None);
    }

    #[test]
    fn test_extra_paths_not_matched() {
        // Top-level :extra-paths is not a real deps.edn key; only :paths and
        // alias :extra-paths count.
        let edn = r#"{:extra-paths ["dev"]}"#;
        assert_eq!(parse_paths_from_deps_edn(edn), None);
    }

    #[test]
    fn test_alias_extra_paths_included() {
        let edn = r#"{:paths ["src"]
                      :aliases {:test {:extra-paths ["test"]}}}"#;
        assert_eq!(
            parse_paths_from_deps_edn(edn),
            Some(vec!["src".to_string(), "test".to_string()])
        );
    }

    #[test]
    fn test_multiple_alias_extra_paths_unioned() {
        let edn = r#"{:paths ["src"]
                      :aliases {:test {:extra-paths ["test"]}
                                :dev {:extra-paths ["dev" "env/dev"]}}}"#;
        let paths = parse_paths_from_deps_edn(edn).unwrap();
        for p in ["src", "test", "dev", "env/dev"] {
            assert!(
                paths.contains(&p.to_string()),
                "missing {} in {:?}",
                p,
                paths
            );
        }
    }

    #[test]
    fn test_source_paths_always_includes_src_and_test() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("deps.edn"),
            r#"{:paths ["src"] :deps {org.clojure/clojure {:mvn/version "1.11.1"}}}"#,
        )
        .unwrap();
        let paths = source_paths(root);
        assert!(
            paths.contains(&root.join("src")),
            "missing src: {:?}",
            paths
        );
        assert!(
            paths.contains(&root.join("test")),
            "missing test: {:?}",
            paths
        );
        // No duplicate `src`.
        assert_eq!(
            paths.iter().filter(|p| **p == root.join("src")).count(),
            1,
            "duplicate src: {:?}",
            paths
        );
    }

    #[test]
    fn test_is_clojure_source() {
        for ext in ["clj", "cljs", "cljc", "lg"] {
            let p = format!("foo.{}", ext);
            assert!(is_clojure_source(Path::new(&p)), "{} should be source", p);
        }
        assert!(!is_clojure_source(Path::new("deps.edn")));
        assert!(!is_clojure_source(Path::new("lgx.edn")));
        assert!(!is_clojure_source(Path::new("foo.edn")));
        assert!(!is_clojure_source(Path::new("Makefile")));
        // project.clj is a Leiningen build manifest, not a namespace.
        assert!(!is_clojure_source(Path::new("project.clj")));
        assert!(!is_clojure_source(Path::new("/a/b/project.clj")));
        // build.clj (tools.build) is real source and must stay linted.
        assert!(is_clojure_source(Path::new("build.clj")));
    }

    #[test]
    fn test_project_kind_detects_letgo_and_clojure() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("lgx.edn"), "{:paths [\"src\"]}").unwrap();
        assert_eq!(project_kind(dir.path()), ProjectKind::LetGo);

        let dir2 = tempfile::TempDir::new().unwrap();
        std::fs::write(dir2.path().join("deps.edn"), "{:paths [\"src\"]}").unwrap();
        assert_eq!(project_kind(dir2.path()), ProjectKind::Clojure);
    }

    #[test]
    fn test_find_project_root_stops_at_lgx_edn() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("lgx.edn"), "{}").unwrap();
        let nested = root.join("src").join("app");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            find_project_root(&nested).unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_source_paths_from_lgx_edn() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("lgx.edn"), r#"{:paths ["src" "test"] :deps {}}"#).unwrap();
        assert_eq!(
            source_paths(root),
            vec![root.join("src"), root.join("test")]
        );
    }

    #[test]
    fn test_source_paths_from_project_clj() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("project.clj"),
            r#"(defproject app "0.1.0" :source-paths ["src/main/clojure"])"#,
        )
        .unwrap();
        let paths = source_paths(root);
        assert!(
            paths.contains(&root.join("src/main/clojure")),
            "missing project.clj source-path: {:?}",
            paths
        );
        // src/test defaults still unioned in.
        assert!(paths.contains(&root.join("src")));
        assert!(paths.contains(&root.join("test")));
    }

    #[test]
    fn test_source_paths_standard_project_clj_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("project.clj"), r#"(defproject app "0.1.0")"#).unwrap();
        let paths = source_paths(root);
        assert_eq!(paths, vec![root.join("src"), root.join("test")]);
    }

    #[test]
    fn test_deps_edn_paths_win_over_project_clj() {
        // A project carrying both files: deps.edn :paths is authoritative; the
        // project.clj :source-paths must be ignored.
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("deps.edn"), r#"{:paths ["xyz"]}"#).unwrap();
        std::fs::write(
            root.join("project.clj"),
            r#"(defproject app "0.1.0" :source-paths ["abc"])"#,
        )
        .unwrap();
        let paths = source_paths(root);
        assert!(paths.contains(&root.join("xyz")), "missing deps.edn path");
        assert!(
            !paths.contains(&root.join("abc")),
            "project.clj path leaked despite deps.edn: {:?}",
            paths
        );
    }

    #[test]
    fn test_source_paths_letgo_defaults_without_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("lgx.edn"), "{:deps {}}").unwrap();
        assert_eq!(
            source_paths(root),
            vec![root.join("src"), root.join("test")]
        );
    }
}
