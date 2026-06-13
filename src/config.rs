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

pub fn source_paths(root: &Path) -> Vec<PathBuf> {
    // let-go projects declare their source paths in lgx.edn.
    if project_kind(root) == ProjectKind::LetGo {
        if let Ok(contents) = std::fs::read_to_string(root.join("lgx.edn")) {
            let paths = crate::lgx::paths(&contents);
            if !paths.is_empty() {
                return paths.into_iter().map(|p| root.join(p)).collect();
            }
        }
        return vec![root.join("src"), root.join("test")];
    }

    let deps_edn = root.join("deps.edn");
    if let Ok(contents) = std::fs::read_to_string(&deps_edn) {
        if let Some(paths) = parse_paths_from_deps_edn(&contents) {
            return paths.into_iter().map(|p| root.join(p)).collect();
        }
    }

    vec![root.join("src"), root.join("test")]
}

fn parse_paths_from_deps_edn(contents: &str) -> Option<Vec<String>> {
    let paths_idx = find_top_level_paths(contents)?;
    let after_paths = &contents[paths_idx + ":paths".len()..];
    let bracket_start = after_paths.find('[')?;
    let bracket_end = after_paths[bracket_start..].find(']')?;
    let inside = &after_paths[bracket_start + 1..bracket_start + bracket_end];

    let paths: Vec<String> = inside
        .split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, s)| s.to_string())
        .collect();

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// Finds `:paths` at the top level of the deps.edn map. A plain substring
/// search would match `:paths` nested inside `:aliases` (e.g. a tools.build
/// `:build` alias), so track nesting depth and skip strings and comments.
fn find_top_level_paths(contents: &str) -> Option<usize> {
    let bytes = contents.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'"' => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b';' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => depth -= 1,
            b':' if depth == 1 && contents[i..].starts_with(":paths") => {
                let next = bytes.get(i + ":paths".len());
                let at_boundary = next
                    .map(|c| c.is_ascii_whitespace() || *c == b'[')
                    .unwrap_or(true);
                if at_boundary {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
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
        let edn = r#"{:extra-paths ["dev"]}"#;
        assert_eq!(parse_paths_from_deps_edn(edn), None);
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
