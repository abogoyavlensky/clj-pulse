//! Leiningen `project.clj` dependency resolution.
//!
//! Reads a project's `project.clj`, extracts its declared Maven coordinates,
//! and maps them to the JARs already downloaded under `~/.m2/repository` (or a
//! `:local-repo` override), handing the ones that exist on disk to the JAR
//! indexer (`SymbolSource::Jar`, navigated via `jar:` URIs). No `java`, no
//! `lein classpath` — we only inspect the file.
//!
//! `project.clj` is Clojure, not EDN, and real files use metadata (`^…`) and
//! regex (`#"…"`) literals that `edn_format` rejects. So we never parse the
//! whole `(defproject …)` form. Instead we mask strings/comments, then
//! EDN-parse only the small plain-data vectors we target (`:dependencies`,
//! `:source-paths`, `:test-paths`, `:local-repo`), unioning every occurrence
//! (top-level + `:profiles` + `:cljsbuild`). `parse_str` stops after one value,
//! so the reader-macro junk elsewhere in the file is simply ignored.

use std::path::{Path, PathBuf};

use edn_format::Value;

use crate::edn::{as_str, prepare};

/// A Maven coordinate from a `:dependencies` entry.
#[derive(Debug, Clone, PartialEq)]
struct Coord {
    group: String,
    artifact: String,
    version: String,
}

/// For each token-boundary occurrence of `keyword` in `locator`, seek the next
/// `open` delimiter and EDN-parse one value from `parse_buf` at that point.
/// `parse_str` reads exactly one form and stops, so trailing reader-macro junk
/// is ignored. Slices that fail to parse are skipped.
///
/// Seeking the opening delimiter (rather than the next non-space char) lets us
/// step over Leiningen metadata such as `^:replace` or `^{:protect false}`,
/// which `edn_format` cannot parse, that may sit between the key and its value.
fn forms_after(parse_buf: &[char], locator: &[char], keyword: &str, open: char) -> Vec<Value> {
    let word: Vec<char> = keyword.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < locator.len() {
        if at_word(locator, i, &word) {
            let mut j = i + word.len();
            while j < locator.len() && locator[j] != open {
                j += 1;
            }
            if j < locator.len() {
                let slice: String = parse_buf[j..].iter().collect();
                if let Ok(v) = edn_format::parse_str(&slice) {
                    out.push(v);
                }
            }
            i += word.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Whether `word` sits at `i` in `masked` flanked by token boundaries, so that
/// `:test-paths` does not match inside a longer token.
fn at_word(masked: &[char], i: usize, word: &[char]) -> bool {
    if i + word.len() > masked.len() || masked[i..i + word.len()] != *word {
        return false;
    }
    let before_ok = i == 0 || is_boundary(masked[i - 1]);
    let after = i + word.len();
    let after_ok = after >= masked.len() || is_boundary(masked[after]);
    before_ok && after_ok
}

fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '"')
}

/// A `[group/artifact "version" & opts]` entry → `Coord`. Reads only the
/// leading symbol and version string; trailing options are ignored. `None`
/// when the symbol or string version is missing.
fn coord_from(parts: &[Value]) -> Option<Coord> {
    let Value::Symbol(sym) = parts.first()? else {
        return None;
    };
    let version = as_str(parts.get(1)?)?.to_string();
    let (group, artifact) = match sym.namespace() {
        Some(ns) => (ns.to_string(), sym.name().to_string()),
        None => (sym.name().to_string(), sym.name().to_string()),
    };
    Some(Coord {
        group,
        artifact,
        version,
    })
}

/// Coordinates from every `:dependencies` vector in `src` (top-level and
/// nested under `:profiles`/`:cljsbuild`), de-duplicated, first-wins.
fn parse_deps(src: &str) -> Vec<Coord> {
    let (locator, parse_buf) = prepare(src);
    let mut out: Vec<Coord> = Vec::new();
    for form in forms_after(&parse_buf, &locator, ":dependencies", '[') {
        let Value::Vector(items) = form else { continue };
        for item in items {
            if let Value::Vector(parts) = item {
                if let Some(c) = coord_from(&parts) {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
        }
    }
    out
}

/// Union of all `:source-paths` and `:test-paths` declared in `edn`
/// (top-level and within `:profiles`/`:cljsbuild`).
pub fn source_paths(edn: &str) -> Vec<String> {
    let (locator, parse_buf) = prepare(edn);
    let mut out: Vec<String> = Vec::new();
    for key in [":source-paths", ":test-paths"] {
        for form in forms_after(&parse_buf, &locator, key, '[') {
            let Value::Vector(items) = form else { continue };
            for item in items {
                if let Some(s) = as_str(&item) {
                    if !out.iter().any(|p| p == s) {
                        out.push(s.to_string());
                    }
                }
            }
        }
    }
    out
}

/// The `~/.m2`-style JAR path for a coordinate:
/// `<repo>/<group dots→slashes>/<artifact>/<version>/<artifact>-<version>.jar`.
fn jar_path(repo: &Path, coord: &Coord) -> PathBuf {
    repo.join(coord.group.replace('.', "/"))
        .join(&coord.artifact)
        .join(&coord.version)
        .join(format!("{}-{}.jar", coord.artifact, coord.version))
}

fn read_project_clj(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join("project.clj")).ok()
}

/// The local Maven repository for the project: `:local-repo` if declared
/// (absolute, or relative to `root`), else `~/.m2/repository`.
fn m2_repo(root: &Path, edn: &str) -> Option<PathBuf> {
    let (locator, parse_buf) = prepare(edn);
    if let Some(s) = forms_after(&parse_buf, &locator, ":local-repo", '"')
        .iter()
        .find_map(as_str)
    {
        let p = PathBuf::from(s);
        return Some(if p.is_absolute() { p } else { root.join(p) });
    }
    default_m2()
}

/// `~/.m2/repository`, located via `HOME` (or `USERPROFILE` on Windows).
fn default_m2() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(|h| PathBuf::from(h).join(".m2").join("repository"))
}

/// Maps the project's direct deps to JAR paths under `repo`, keeping only the
/// ones present on disk (undownloaded deps are silently skipped).
fn resolve_with_repo(root: &Path, repo: &Path) -> Vec<PathBuf> {
    let Some(edn) = read_project_clj(root) else {
        return vec![];
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for coord in parse_deps(&edn) {
        let jar = jar_path(repo, &coord);
        if jar.exists() && !out.contains(&jar) {
            out.push(jar);
        }
    }
    out
}

/// Resolves a Leiningen project's direct dependencies to the JARs that exist
/// under its local Maven repository (`:local-repo` or `~/.m2/repository`).
/// Returns paths suitable for `SymbolSource::Jar` indexing; empty when there
/// is no `project.clj`.
pub fn resolve(root: &Path) -> Vec<PathBuf> {
    let Some(edn) = read_project_clj(root) else {
        tracing::debug!("leiningen: no project.clj at {}", root.display());
        return vec![];
    };
    let Some(repo) = m2_repo(root, &edn) else {
        tracing::warn!("leiningen: cannot locate local Maven repo (no HOME?)");
        return vec![];
    };
    resolve_with_repo(root, &repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Modeled on `../tickets/project.clj`: metadata (`^{…}`, `^:replace`),
    /// a regex literal (`#"user"`), profile-level `:dependencies`, a
    /// `:dependencies` token hiding in a string and in a comment.
    const SAMPLE: &str = r#"(defproject tickets "0.1.0-SNAPSHOT"
  :description "mentions :dependencies [\"nope\"] inside a string"
  :dependencies [[org.clojure/clojure "1.10.3"]
                 [org.clojure/clojurescript "1.10.879" :scope "provided"]
                 [ring "1.7.1"]
                 [no-version-dep]]
  :source-paths ["src/clj" "src/cljs"]
  :test-paths ["test/clj"]
  :clean-targets ^{:protect false} [:target-path "dev-target"]
  :local-repo "m2"
  :profiles {:dev {:dependencies [[etaoin "0.4.6"]]
                   :source-paths ["dev"]}
             :uberjar {:source-paths ^:replace ["src/clj"]}
             :coverage {:cloverage {:ns-exclude-regex [#"user"]}}})
;; :dependencies [[commented-out "9.9.9"]]
"#;

    fn coord(deps: &[Coord], artifact: &str) -> Option<Coord> {
        deps.iter().find(|c| c.artifact == artifact).cloned()
    }

    #[test]
    fn parses_top_level_and_profile_deps_through_reader_macros() {
        let deps = parse_deps(SAMPLE);
        // Survives ^{…}, ^:replace and #"user" elsewhere in the file.
        assert!(!deps.is_empty(), "expected deps, got none");

        let clj = coord(&deps, "clojure").expect("clojure dep");
        assert_eq!(clj.group, "org.clojure");
        assert_eq!(clj.version, "1.10.3");

        // No namespace => group == artifact.
        let ring = coord(&deps, "ring").expect("ring dep");
        assert_eq!(ring.group, "ring");
        assert_eq!(ring.version, "1.7.1");

        // Profile (:dev) deps are unioned in.
        assert!(coord(&deps, "etaoin").is_some(), "expected :dev etaoin dep");
    }

    #[test]
    fn ignores_extra_options_and_bad_entries() {
        let deps = parse_deps(SAMPLE);
        // Trailing :scope option ignored, version still read.
        let cljs = coord(&deps, "clojurescript").expect("clojurescript dep");
        assert_eq!(cljs.version, "1.10.879");
        // Entry without a string version is skipped.
        assert!(coord(&deps, "no-version-dep").is_none());
    }

    #[test]
    fn masking_skips_strings_and_comments() {
        let deps = parse_deps(SAMPLE);
        // The :dependencies inside :description's string must not be parsed.
        assert!(coord(&deps, "nope").is_none(), "picked up string content");
        // The commented-out :dependencies line must not be parsed.
        assert!(
            coord(&deps, "commented-out").is_none(),
            "picked up commented content"
        );
    }

    #[test]
    fn source_paths_union_includes_profiles() {
        let mut paths = source_paths(SAMPLE);
        paths.sort();
        let mut want = vec!["dev", "src/clj", "src/cljs", "test/clj"];
        want.sort();
        assert_eq!(paths, want);
    }

    #[test]
    fn skips_reader_discarded_forms() {
        // `#_` discards the next form. Cases: a discarded entry inside the
        // vector (also handled by edn), a discarded vector right after the key,
        // and a discarded key + vector.
        let edn = r#"(defproject x "1"
          :dependencies [[active "1.0"] #_[disabled "9.9"]]
          :source-paths #_["discarded"] ["src"]
          #_:test-paths #_["nope"])"#;
        let deps = parse_deps(edn);
        assert!(coord(&deps, "active").is_some(), "active dep dropped");
        assert!(coord(&deps, "disabled").is_none(), "discarded dep parsed");

        let paths = source_paths(edn);
        // The real ["src"] wins; the discarded ["discarded"] and the
        // discarded :test-paths key are ignored.
        assert_eq!(paths, vec!["src".to_string()]);
    }

    #[test]
    fn empty_or_trivial_input_yields_nothing() {
        assert!(parse_deps("(defproject x \"1\")").is_empty());
        assert!(source_paths("(defproject x \"1\")").is_empty());
        assert!(parse_deps("not clojure (((").is_empty());
    }

    #[test]
    fn maps_coord_to_maven_jar_path() {
        let repo = Path::new("/repo");
        let c = Coord {
            group: "org.clojure".to_string(),
            artifact: "clojure".to_string(),
            version: "1.11.1".to_string(),
        };
        assert_eq!(
            jar_path(repo, &c),
            Path::new("/repo/org/clojure/clojure/1.11.1/clojure-1.11.1.jar")
        );
    }

    use std::fs;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"jar").unwrap();
    }

    #[test]
    fn resolve_with_repo_returns_existing_jars_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let repo = root.join("repo");
        touch(&repo.join("org/clojure/clojure/1.11.1/clojure-1.11.1.jar"));
        touch(&repo.join("hiccup/hiccup/1.0.5/hiccup-1.0.5.jar"));
        // `ring` is declared but never laid down on disk → must be omitted.
        fs::write(
            root.join("project.clj"),
            r#"(defproject app "0.1.0"
                 :dependencies [[org.clojure/clojure "1.11.1"]
                                [hiccup "1.0.5"]
                                [ring "1.7.1"]])"#,
        )
        .unwrap();

        let jars = resolve_with_repo(root, &repo);
        assert!(jars.contains(&repo.join("org/clojure/clojure/1.11.1/clojure-1.11.1.jar")));
        assert!(jars.contains(&repo.join("hiccup/hiccup/1.0.5/hiccup-1.0.5.jar")));
        assert_eq!(jars.len(), 2, "absent ring jar leaked in: {:?}", jars);
    }

    #[test]
    fn m2_repo_honors_relative_and_absolute_local_repo() {
        let root = Path::new("/proj");
        assert_eq!(
            m2_repo(root, r#"(defproject a "1" :local-repo "m2")"#),
            Some(PathBuf::from("/proj/m2"))
        );
        assert_eq!(
            m2_repo(root, r#"(defproject a "1" :local-repo "/abs/repo")"#),
            Some(PathBuf::from("/abs/repo"))
        );
    }
}
