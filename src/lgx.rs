//! let-go / lgx dependency resolution.
//!
//! Reads a project's `lgx.edn`, resolves its `:deps` (git + `:local/root`,
//! transitively, first-wins) into dependency source directories under
//! `$LGX_HOME/gitlibs`, and hands them to the library indexer as plain
//! source dirs (`SymbolSource::Dir`).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use edn_format::{Keyword, Value};

/// Resolves a let-go project's lgx dependencies to their source directories,
/// following transitive `:deps` breadth-first with first-wins on lib name.
/// Returns directories suitable for `SymbolSource::Dir` library indexing.
pub fn resolve(project_root: &Path) -> Vec<PathBuf> {
    resolve_with_home(project_root, lgx_home().as_deref())
}

fn resolve_with_home(project_root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Each item carries the base dir its `:local/root` is resolved against.
    let mut queue: VecDeque<(String, Dep, PathBuf)> = VecDeque::new();
    for (lib, dep) in read_deps(project_root) {
        queue.push_back((lib, dep, project_root.to_path_buf()));
    }

    while let Some((lib, dep, base)) = queue.pop_front() {
        if !seen.insert(lib.clone()) {
            continue; // first-wins: a later coord for the same lib is skipped
        }
        let Some(dep_root) = dep_root_dir(&dep.coord, &base, home) else {
            tracing::warn!("lgx: cannot locate dep {} (no LGX_HOME?)", lib);
            continue;
        };
        if !dep_root.is_dir() {
            tracing::warn!("lgx: dep {} not fetched at {}", lib, dep_root.display());
            continue;
        }
        let src = source_dir(&dep_root, &dep.deps_root);
        if src.is_dir() {
            out.push(src);
        }
        // Transitive: consult only this dep's own `:deps`.
        for (tlib, tdep) in read_deps(&dep_root) {
            if !seen.contains(&tlib) {
                queue.push_back((tlib, tdep, dep_root.clone()));
            }
        }
    }
    out
}

/// Parses the top-level `:paths` vector of an `lgx.edn`. Empty when absent.
pub fn paths(edn: &str) -> Vec<String> {
    let Ok(Value::Map(top)) = edn_format::parse_str(edn) else {
        return vec![];
    };
    let Some(Value::Vector(v)) = get(&top, kw("paths")) else {
        return vec![];
    };
    v.iter().filter_map(as_str).map(str::to_string).collect()
}

fn read_deps(root: &Path) -> Vec<(String, Dep)> {
    std::fs::read_to_string(root.join("lgx.edn"))
        .ok()
        .map(|s| parse_deps(&s))
        .unwrap_or_default()
}

/// `$LGX_HOME`, else `~/.lgx`.
fn lgx_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("LGX_HOME") {
        return Some(PathBuf::from(h));
    }
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(|h| PathBuf::from(h).join(".lgx"))
}

fn dep_root_dir(coord: &Coord, base: &Path, home: Option<&Path>) -> Option<PathBuf> {
    match coord {
        Coord::Git { url, reff } => Some(gitlib_dir(home?, url, reff)),
        Coord::Local { root } => {
            let p = PathBuf::from(root);
            Some(if p.is_absolute() { p } else { base.join(p) })
        }
    }
}

/// `$LGX_HOME/gitlibs/<url sans scheme sans .git>/<reff>`.
fn gitlib_dir(home: &Path, url: &str, reff: &str) -> PathBuf {
    let no_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let path = no_scheme.strip_suffix(".git").unwrap_or(no_scheme);
    home.join("gitlibs").join(path).join(reff)
}

/// The source dir within a dep: explicit `:deps/root`, else `src` if present,
/// else the repo root.
fn source_dir(root: &Path, deps_root: &Option<String>) -> PathBuf {
    if let Some(dr) = deps_root {
        root.join(dr)
    } else if root.join("src").is_dir() {
        root.join("src")
    } else {
        root.to_path_buf()
    }
}

/// A dependency coordinate: a git checkout or a local directory.
#[derive(Debug, Clone, PartialEq)]
enum Coord {
    /// Git dep cached under `$LGX_HOME/gitlibs/<url>/<reff>`. `reff` is the
    /// sha verbatim, or a tag with `/` replaced by `_`.
    Git { url: String, reff: String },
    /// `:local/root` dep — a directory on disk (relative to the dep's own
    /// root, or absolute).
    Local { root: String },
}

#[derive(Debug, Clone, PartialEq)]
struct Dep {
    coord: Coord,
    /// `:deps/root` — source subdir inside the dep. `None` means default
    /// (`src` if present, else the repo root).
    deps_root: Option<String>,
}

/// Parses the `:deps` map of an `lgx.edn`, returning `(lib-name, dep)` pairs.
/// Malformed input yields an empty vec.
fn parse_deps(edn: &str) -> Vec<(String, Dep)> {
    let Ok(Value::Map(top)) = edn_format::parse_str(edn) else {
        return vec![];
    };
    let Some(Value::Map(deps)) = get(&top, kw("deps")) else {
        return vec![];
    };

    let mut out = Vec::new();
    for (key, spec) in deps {
        let Value::Symbol(sym) = key else { continue };
        let Value::Map(spec) = spec else { continue };

        let lib = match sym.namespace() {
            Some(ns) => format!("{}/{}", ns, sym.name()),
            None => sym.name().to_string(),
        };
        let deps_root = get(spec, kw_ns("deps", "root"))
            .and_then(as_str)
            .map(str::to_string);

        let coord = if let Some(root) = get(spec, kw_ns("local", "root")).and_then(as_str) {
            Coord::Local {
                root: root.to_string(),
            }
        } else if let Some(url) = get(spec, kw_ns("git", "url")).and_then(as_str) {
            let reff = if let Some(sha) = get(spec, kw_ns("git", "sha")).and_then(as_str) {
                sha.to_string()
            } else if let Some(tag) = get(spec, kw_ns("git", "tag")).and_then(as_str) {
                tag.replace('/', "_")
            } else {
                continue; // git coord without sha or tag
            };
            Coord::Git {
                url: url.to_string(),
                reff,
            }
        } else {
            continue; // neither git nor local
        };

        out.push((lib, Dep { coord, deps_root }));
    }
    out
}

fn kw(name: &str) -> Value {
    Value::Keyword(Keyword::from_name(name))
}

fn kw_ns(namespace: &str, name: &str) -> Value {
    Value::Keyword(Keyword::from_namespace_and_name(namespace, name))
}

fn get(map: &BTreeMap<Value, Value>, key: Value) -> Option<&Value> {
    map.get(&key)
}

fn as_str(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_sha_coord() {
        let edn = r#"{:deps {nooga/let-go {:git/url "https://github.com/nooga/let-go"
                                           :git/sha "46ce159c"}}}"#;
        let deps = parse_deps(edn);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].0, "nooga/let-go");
        assert_eq!(
            deps[0].1.coord,
            Coord::Git {
                url: "https://github.com/nooga/let-go".to_string(),
                reff: "46ce159c".to_string(),
            }
        );
        assert_eq!(deps[0].1.deps_root, None);
    }

    #[test]
    fn parses_git_tag_coord_encoding_slashes() {
        let edn = r#"{:deps {a/b {:git/url "https://x/a/b" :git/tag "rel/v1.0"}}}"#;
        let deps = parse_deps(edn);
        assert_eq!(
            deps[0].1.coord,
            Coord::Git {
                url: "https://x/a/b".to_string(),
                reff: "rel_v1.0".to_string(),
            }
        );
    }

    #[test]
    fn parses_local_root_coord() {
        let edn = r#"{:deps {my/lib {:local/root "../my-lib"}}}"#;
        let deps = parse_deps(edn);
        assert_eq!(
            deps[0].1.coord,
            Coord::Local {
                root: "../my-lib".to_string(),
            }
        );
    }

    #[test]
    fn parses_deps_root() {
        let edn = r#"{:deps {org.clojure/tools.cli
                             {:git/url "https://github.com/clojure/tools.cli"
                              :git/sha "abc" :deps/root "src/main/clojure"}}}"#;
        let deps = parse_deps(edn);
        assert_eq!(deps[0].0, "org.clojure/tools.cli");
        assert_eq!(deps[0].1.deps_root.as_deref(), Some("src/main/clojure"));
    }

    #[test]
    fn empty_or_absent_deps_yields_empty() {
        assert!(parse_deps("{:paths [\"src\"]}").is_empty());
        assert!(parse_deps("{:deps {}}").is_empty());
        assert!(parse_deps("not edn (((").is_empty());
    }

    use std::fs;
    use std::path::Path;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Lays out a gitlib checkout `home/gitlibs/<url-path>/<ref>/src/<file>`.
    fn gitlib(home: &Path, url_path: &str, reff: &str, file: &str, body: &str) -> PathBuf {
        let root = home.join("gitlibs").join(url_path).join(reff);
        write(&root.join("src").join(file), body);
        root
    }

    #[test]
    fn resolves_git_and_local_source_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("lgxhome");
        gitlib(
            &home,
            "github.com/x/lib",
            "SHA1",
            "lib/core.lg",
            "(ns lib.core)",
        );

        // Local dep sibling of the project.
        let local = tmp.path().join("local-dep");
        write(&local.join("src").join("loc/core.lg"), "(ns loc.core)");

        let project = tmp.path().join("proj");
        write(
            &project.join("lgx.edn"),
            r#"{:paths ["src"]
                :deps {x/lib {:git/url "https://github.com/x/lib" :git/sha "SHA1"}
                       my/loc {:local/root "../local-dep"}}}"#,
        );

        // Local-root deps resolve with a `..` segment; downstream indexing
        // canonicalizes, so compare canonical paths here too.
        let canon: Vec<PathBuf> = resolve_with_home(&project, Some(&home))
            .iter()
            .map(|d| d.canonicalize().unwrap())
            .collect();
        let want_git = home
            .join("gitlibs/github.com/x/lib/SHA1/src")
            .canonicalize()
            .unwrap();
        assert!(canon.contains(&want_git));
        assert!(canon.contains(&local.join("src").canonicalize().unwrap()));
    }

    #[test]
    fn resolves_transitive_deps() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("lgxhome");

        // Direct dep `a`, which itself depends on `b`.
        let a_root = gitlib(&home, "h/a", "ASHA", "a/core.lg", "(ns a.core)");
        write(
            &a_root.join("lgx.edn"),
            r#"{:deps {h/b {:git/url "https://h/b" :git/sha "BSHA"}}}"#,
        );
        gitlib(&home, "h/b", "BSHA", "b/core.lg", "(ns b.core)");

        let project = tmp.path().join("proj");
        write(
            &project.join("lgx.edn"),
            r#"{:deps {h/a {:git/url "https://h/a" :git/sha "ASHA"}}}"#,
        );

        let dirs = resolve_with_home(&project, Some(&home));
        assert!(dirs.contains(&home.join("gitlibs/h/a/ASHA/src")));
        assert!(dirs.contains(&home.join("gitlibs/h/b/BSHA/src")));
    }

    #[test]
    fn first_wins_on_conflict() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("lgxhome");

        // Project pins lib at ASHA; transitive dep pins same lib at BSHA.
        gitlib(&home, "h/lib", "ASHA", "lib/core.lg", "(ns lib.core)");
        gitlib(&home, "h/lib", "BSHA", "lib/core.lg", "(ns lib.core)");
        let mid = gitlib(&home, "h/mid", "MSHA", "mid/core.lg", "(ns mid.core)");
        write(
            &mid.join("lgx.edn"),
            r#"{:deps {h/lib {:git/url "https://h/lib" :git/sha "BSHA"}}}"#,
        );

        let project = tmp.path().join("proj");
        write(
            &project.join("lgx.edn"),
            r#"{:deps {h/lib {:git/url "https://h/lib" :git/sha "ASHA"}
                      h/mid {:git/url "https://h/mid" :git/sha "MSHA"}}}"#,
        );

        let dirs = resolve_with_home(&project, Some(&home));
        assert!(dirs.contains(&home.join("gitlibs/h/lib/ASHA/src")));
        assert!(!dirs.contains(&home.join("gitlibs/h/lib/BSHA/src")));
    }

    #[test]
    fn deps_root_defaults_to_repo_root_without_src() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("lgxhome");
        // No src/ dir: file sits at the repo root.
        let root = home.join("gitlibs/h/flat/SHA");
        write(&root.join("flat.lg"), "(ns flat)");

        let project = tmp.path().join("proj");
        write(
            &project.join("lgx.edn"),
            r#"{:deps {h/flat {:git/url "https://h/flat" :git/sha "SHA"}}}"#,
        );

        let dirs = resolve_with_home(&project, Some(&home));
        assert_eq!(dirs, vec![root]);
    }

    #[test]
    fn explicit_deps_root_is_used() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("lgxhome");
        let root = home.join("gitlibs/h/sub/SHA");
        write(&root.join("src/main/clojure/x.clj"), "(ns x)");

        let project = tmp.path().join("proj");
        write(
            &project.join("lgx.edn"),
            r#"{:deps {h/sub {:git/url "https://h/sub" :git/sha "SHA"
                              :deps/root "src/main/clojure"}}}"#,
        );

        let dirs = resolve_with_home(&project, Some(&home));
        assert_eq!(dirs, vec![root.join("src/main/clojure")]);
    }
}
