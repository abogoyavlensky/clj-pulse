//! let-go / lgx dependency resolution.
//!
//! Reads a project's `lgx.edn`, resolves its `:deps` (git + `:local/root`,
//! transitively, first-wins) into dependency source directories under
//! `$LGX_HOME/gitlibs`, and hands them to the library indexer as plain
//! source dirs (`SymbolSource::Dir`).

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use edn_format::Value;

use crate::config::{project_kind, ProjectKind};
use crate::edn::{as_str, get, kw, kw_ns, str_vec_at};
use crate::index::{scanner, Index};

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
    str_vec_at(&top, kw("paths")).unwrap_or_default()
}

/// The top-level `:lg-version` of an `lgx.edn` — the pinned let-go version that
/// tells us `lgx install` has fetched the matching let-go source. `None` when
/// absent, non-string, or blank.
pub fn lg_version(edn: &str) -> Option<String> {
    let Ok(Value::Map(top)) = edn_format::parse_str(edn) else {
        return None;
    };
    let version = as_str(get(&top, kw("lg-version"))?)?;
    (!version.trim().is_empty()).then(|| version.to_string())
}

/// let-go aliases Clojure namespace names to its own built-in ones
/// (`lang.go` `nsAliases`): `(clojure.* name, let-go name)`. A project written
/// against Clojure (`[clojure.string :as str]`) must resolve into the let-go
/// source file whose namespace is the bare let-go name (`string`).
const NS_ALIASES: &[(&str, &str)] = &[
    ("clojure.core", "core"),
    ("clojure.string", "string"),
    ("clojure.set", "set"),
    ("clojure.walk", "walk"),
    ("clojure.edn", "edn"),
    ("clojure.zip", "zip"),
    ("clojure.data", "data"),
    ("clojure.pprint", "pprint"),
    ("clojure.test", "test"),
];

/// Indexes let-go's built-in `core`/stdlib for a pinned let-go project so that
/// definition/hover/completion reach the actual `.lg` source `lgx install`
/// fetched. No-op (returns 0) unless this is a `ProjectKind::LetGo` with a
/// pinned `:lg-version` whose source directory exists. Returns the number of
/// core namespaces indexed, so the caller's "nothing to index" check stays
/// correct even for a project with no other deps.
pub fn index_letgo_core(root: &Path, index: &Index) -> usize {
    if project_kind(root) != ProjectKind::LetGo {
        return 0;
    }
    let Some(version) = std::fs::read_to_string(root.join("lgx.edn"))
        .ok()
        .and_then(|edn| lg_version(&edn))
    else {
        return 0; // unpinned: lgx only fetches let-go source when pinned
    };
    let Some(home) = lgx_home() else {
        return 0;
    };
    let core_dir = home
        .join("let-go")
        .join("source")
        .join(&version)
        .join("pkg/rt/core");
    if !core_dir.is_dir() {
        tracing::warn!(
            "lgx: let-go {} core source not found at {} (run `lgx install`?)",
            version,
            core_dir.display()
        );
        return 0;
    }
    index_core_dir(&core_dir, index)
}

/// Indexes the let-go `core` source directory and registers each indexed
/// namespace a second time under its `clojure.*` alias, then flips the index's
/// let-go-core marker. Threaded through `core_dir` directly (rather than
/// derived from `LGX_HOME`) so tests stay hermetic.
fn index_core_dir(core_dir: &Path, index: &Index) -> usize {
    // Canonicalize the same way `index_dir_libs` does, so indexed file paths
    // are reliably under `core_dir` for the alias-source check below.
    let core_dir = core_dir
        .canonicalize()
        .unwrap_or_else(|_| core_dir.to_path_buf());

    let before = index.namespaces.len();
    scanner::index_dir_libs(std::slice::from_ref(&core_dir), index);
    let indexed = index.namespaces.len().saturating_sub(before);

    for (clojure_ns, letgo_ns) in NS_ALIASES {
        register_alias_copy(index, &core_dir, clojure_ns, letgo_ns);
    }

    // Harvest let-go's Go-native var/fn names from `lang.go` (sibling of the
    // core dir, `pkg/rt/lang.go`) so completion/hover track this version's
    // actual vars — including ones with no `.lg` source like
    // `*command-line-args*` — instead of a hardcoded list. Falls back to the
    // static `NATIVE_NAMES` when `lang.go` isn't present.
    if let Some(lang_go) = core_dir.parent().map(|rt| rt.join("lang.go")) {
        index.set_letgo_native(parse_native_defs(&lang_go));
    }

    index.mark_letgo_core();
    indexed
}

/// Harvests the names let-go defines natively in Go from `lang.go`'s
/// `ns.Def("name", …)` calls — both native fns (`+`, `count`) and plain vars
/// (`*command-line-args*`, `*ns*`). They live in the `core` namespace but have
/// no `.lg` source, so callers surface them for hover/completion only.
/// Commented (`// ns.Def(...)`) and computed (`ns.Def(name, …)`, no string
/// literal) calls are skipped. Returns the names in source order (the caller
/// sorts); empty when the file is absent or unreadable.
fn parse_native_defs(lang_go: &Path) -> Vec<String> {
    let Ok(src) = std::fs::read_to_string(lang_go) else {
        return Vec::new();
    };
    const MARKER: &str = "ns.Def(\"";
    let mut names = Vec::new();
    for line in src.lines() {
        let Some(idx) = line.find(MARKER) else {
            continue;
        };
        // Skip the call when it sits inside a line comment (`// ns.Def(...)`
        // or trailing `… // ns.Def(...)`).
        if line.find("//").is_some_and(|c| c < idx) {
            continue;
        }
        if let Some(name) = read_go_string(&line[idx + MARKER.len()..]) {
            names.push(name);
        }
    }
    names
}

/// Reads a Go double-quoted string body from `s`, which is positioned just
/// after the opening quote, decoding `\"` and `\\`. Returns the body up to the
/// closing quote, or `None` if unterminated.
fn read_go_string(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            other => out.push(other),
        }
    }
    None
}

/// Registers `clojure_ns` as a duplicate of the let-go `letgo_ns`: same source
/// file and ranges, with the namespace and every fqn rewritten to the
/// `clojure.*` name. A no-op when `letgo_ns` was not indexed from the let-go
/// core directory — guarding against cloning an unrelated project/dependency
/// namespace that merely shares a bare name like `core`/`string`/`test`.
/// Inserting via `insert_lib_file` keeps project symbols winning, per the
/// index invariants.
fn register_alias_copy(index: &Index, core_dir: &Path, clojure_ns: &str, letgo_ns: &str) {
    let Some(mut meta) = index.ns_meta(letgo_ns) else {
        return;
    };
    // Only alias namespaces actually sourced from the fetched let-go core dir.
    if !meta.file.starts_with(core_dir) {
        return;
    }
    let Some(fqns) = index.ns_symbols.get(letgo_ns).map(|r| r.clone()) else {
        return;
    };
    meta.name = clojure_ns.to_string();

    let mut symbols = Vec::with_capacity(fqns.len());
    for fqn in &fqns {
        if let Some(mut sym) = index.lookup(fqn) {
            sym.ns = clojure_ns.to_string();
            sym.fqn = format!("{}/{}", clojure_ns, sym.name);
            symbols.push(sym);
        }
    }
    index.insert_lib_file(meta, symbols);
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

    #[test]
    fn lg_version_reads_top_level_pinned_version() {
        assert_eq!(
            lg_version(r#"{:lg-version "1.10.0" :paths ["src"]}"#),
            Some("1.10.0".to_string())
        );
    }

    #[test]
    fn lg_version_absent_or_non_string_is_none() {
        // Absent entirely.
        assert_eq!(lg_version(r#"{:paths ["src"]}"#), None);
        // Present but blank.
        assert_eq!(lg_version(r#"{:lg-version ""}"#), None);
        // Present but not a string (a number).
        assert_eq!(lg_version("{:lg-version 1.10}"), None);
        // Malformed EDN.
        assert_eq!(lg_version("not edn ((("), None);
    }

    #[test]
    fn index_core_dir_aliases_clojure_namespaces_and_sets_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let core_dir = tmp.path().join("pkg/rt/core");
        write(&core_dir.join("core.lg"), "(ns core)\n(defn map [f c] c)\n");
        write(
            &core_dir.join("string.lg"),
            "(ns string)\n(defn join [sep c] sep)\n",
        );

        let index = Index::new();
        assert!(!index.letgo_core(), "marker starts unset");
        index_core_dir(&core_dir, &index);

        // The let-go ns and its clojure.* alias resolve to the same .lg source.
        let lg = index
            .lookup_in_ns("string", "join")
            .expect("string/join indexed");
        let cl = index
            .lookup_in_ns("clojure.string", "join")
            .expect("clojure.string/join alias registered");
        assert_eq!(lg.file, cl.file);
        assert!(lg.file.ends_with("string.lg"), "got {}", lg.file.display());

        // clojure.core is registered as a copy of the let-go `core` ns.
        assert!(index.ns_meta("clojure.core").is_some());
        assert!(index.lookup_in_ns("clojure.core", "map").is_some());

        // The marker flips on so the bare-word resolver prefers let-go core.
        assert!(index.letgo_core());
    }

    #[test]
    fn index_core_dir_does_not_alias_unrelated_namespaces() {
        let tmp = tempfile::TempDir::new().unwrap();
        let core_dir = tmp.path().join("pkg/rt/core");
        write(&core_dir.join("core.lg"), "(ns core)\n(defn map [f c] c)\n");

        let index = Index::new();

        // A pre-existing project namespace that happens to share a bare let-go
        // stdlib name (`data`), living outside the core dir.
        let proj = tmp.path().join("proj/src/data.lg");
        let proj_src = "(ns data)\n(defn parse [s] s)\n";
        write(&proj, proj_src);
        let (meta, symbols, occ) = crate::index::extractor::extract_full(proj_src, &proj).unwrap();
        index.insert_file(meta, symbols, occ);

        index_core_dir(&core_dir, &index);

        // clojure.core is aliased from the real let-go core source...
        assert!(index.ns_meta("clojure.core").is_some());
        // ...but the unrelated project `data` ns must NOT become `clojure.data`.
        assert!(
            index.ns_meta("clojure.data").is_none(),
            "unrelated project ns wrongly aliased as clojure.data"
        );
        // The project's own `data` ns stays intact.
        assert!(index.lookup_in_ns("data", "parse").is_some());
    }

    #[test]
    fn index_core_dir_harvests_native_names_from_lang_go() {
        let tmp = tempfile::TempDir::new().unwrap();
        let core_dir = tmp.path().join("pkg/rt/core");
        write(&core_dir.join("core.lg"), "(ns core)\n(defn map [f c] c)\n");
        // A minimal `lang.go` sibling covering every shape we must handle: a
        // native fn, a plain var, the assignment form, a commented def
        // (skipped), and a computed def with no string literal (skipped).
        write(
            &core_dir.parent().unwrap().join("lang.go"),
            concat!(
                "func registerCore(ns *vm.Namespace) {\n",
                "\tns.Def(\"count\", count)\n",
                "\tns.Def(\"*command-line-args*\", vm.NIL)\n",
                "\tCurrentNS = ns.Def(\"*ns*\", ns)\n",
                "\t// ns.Def(\"and\", and)\n",
                "\tns.Def(name, vm.Symbol(name))\n",
                "}\n",
            ),
        );

        let index = Index::new();
        index_core_dir(&core_dir, &index);

        assert_eq!(index.letgo_native_contains("count"), Some(true));
        assert_eq!(
            index.letgo_native_contains("*command-line-args*"),
            Some(true),
            "the new var is harvested from lang.go"
        );
        assert_eq!(index.letgo_native_contains("*ns*"), Some(true));
        // Commented-out and computed (non-literal) defs are not harvested.
        assert_eq!(index.letgo_native_contains("and"), Some(false));
        assert_eq!(index.letgo_native_contains("name"), Some(false));
    }

    #[test]
    fn index_core_dir_without_lang_go_falls_back_to_static() {
        // No `lang.go` sibling → nothing harvested → callers use the static
        // NATIVE_NAMES list (signalled by `None`).
        let tmp = tempfile::TempDir::new().unwrap();
        let core_dir = tmp.path().join("pkg/rt/core");
        write(&core_dir.join("core.lg"), "(ns core)\n(defn map [f c] c)\n");

        let index = Index::new();
        index_core_dir(&core_dir, &index);
        assert_eq!(index.letgo_native_contains("count"), None);
        assert!(index.letgo_native_names().is_none());
    }

    #[test]
    fn index_letgo_core_is_a_no_op_for_clojure_projects() {
        // Isolation: a Clojure project (deps.edn, no lgx.edn) must be entirely
        // untouched by the let-go core machinery — no marker, no clojure.*
        // alias injection, so resolve_symbol keeps using the static core list.
        let tmp = tempfile::TempDir::new().unwrap();
        write(&tmp.path().join("deps.edn"), r#"{:paths ["src"]}"#);

        let index = Index::new();
        assert_eq!(index_letgo_core(tmp.path(), &index), 0);
        assert!(
            !index.letgo_core(),
            "a Clojure project must never set the let-go-core marker"
        );
        assert!(index.ns_meta("clojure.core").is_none());
        assert!(index.ns_meta("clojure.string").is_none());
    }

    #[test]
    fn index_letgo_core_is_a_no_op_when_unpinned() {
        // A let-go project that does not pin :lg-version gets no core indexing
        // (lgx only fetches the source when pinned), so the marker stays unset.
        let tmp = tempfile::TempDir::new().unwrap();
        write(&tmp.path().join("lgx.edn"), r#"{:paths ["src"]}"#);

        let index = Index::new();
        assert_eq!(index_letgo_core(tmp.path(), &index), 0);
        assert!(!index.letgo_core());
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
