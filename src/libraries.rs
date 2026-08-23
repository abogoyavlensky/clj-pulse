//! Maps resolved classpath entries to the external-library list the editor's
//! "External Libraries" panel renders.
//!
//! Mostly pure path logic; the one filesystem touch is the ownership rule's
//! manifest probe (which dir "owns" an in-workspace classpath entry). The
//! handler feeds it the same entries stage 2/3 derive (deps.edn `.cpcache`
//! classpath, Leiningen direct-dep JARs, or lgx source dirs) and serializes
//! the result over the custom LSP request.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// A resolved external library, as sent to the editor.
#[derive(serde::Serialize, PartialEq, Eq, Debug)]
pub struct Library {
    /// `group/artifact`, collapsed to `artifact` when group == artifact.
    pub name: String,
    /// Omitted when unknown (a `:local/root` dir). Short sha for git deps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub path: String,
    pub kind: LibraryKind,
}

#[derive(serde::Serialize, PartialEq, Eq, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum LibraryKind {
    Jar,
    Dir,
}

/// Derives the library list from resolved classpath entries.
///
/// Entries that are one of `own_paths` (the project's own source roots, e.g.
/// `src`/`resources`) are excluded. The match is exact, not prefix-based: a
/// classpath entry for the project's own source is always exactly a declared
/// root, whereas an in-workspace `:local/root` dependency is a *deeper* path —
/// even one nested under `test/` (which `source_paths` always unions in) — so
/// exact matching keeps it.
///
/// Non-jar entries *owned* by one of `project_dirs` are also excluded: a
/// resolved classpath lists alias `:extra-paths` (`dev`, `src/cljc`) and, in
/// a monorepo, other projects' source dirs — none of which are external
/// libraries. See [`owned_by_project`] for the ownership rule; jars are never
/// ownership-filtered (a jar built into `target/` is a real artifact).
///
/// Duplicate absolute paths collapse to one library, and the result is sorted
/// by name, then version, then path (a total order, so the panel is
/// deterministic).
pub fn from_entries(
    own_paths: &[PathBuf],
    project_dirs: &[PathBuf],
    entries: &[PathBuf],
) -> Vec<Library> {
    let mut seen: HashSet<&PathBuf> = HashSet::new();
    let mut libs: Vec<Library> = Vec::new();
    for entry in entries {
        if own_paths.iter().any(|p| entry == p) {
            continue;
        }
        let is_jar = entry.extension().and_then(|e| e.to_str()) == Some("jar");
        if !is_jar && owned_by_project(entry, project_dirs) {
            continue;
        }
        // Resolved classpaths can repeat entries; keep the first.
        if !seen.insert(entry) {
            continue;
        }
        libs.push(classify(entry));
    }
    libs.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.version.cmp(&b.version))
            .then_with(|| a.path.cmp(&b.path))
    });
    libs
}

/// Whether a directory entry belongs to one of the workspace's resolved
/// projects. Walking up from the entry toward (and including) its outermost
/// enclosing project dir, the **nearest ancestor holding a manifest**
/// (`deps.edn`/`project.clj`/`lgx.edn`) decides: the entry is owned iff that
/// ancestor is itself one of `project_dirs`, or no manifest ancestor exists at
/// all (a manifest-less root still owns its bare source dirs). A vendored
/// in-workspace checkout (manifest present, but not a resolved project) is
/// therefore *not* owned and stays listed — the panel is the only way to
/// browse it.
fn owned_by_project(entry: &Path, project_dirs: &[PathBuf]) -> bool {
    // Lexical `..`/`.` defeat prefix and equality checks (`lgx::resolve`
    // keeps a sibling `:local/root "../common"` verbatim): compare clean
    // paths only.
    let entry = crate::index::scanner::normalize_lexically(entry);
    let project_dirs: Vec<PathBuf> = project_dirs
        .iter()
        .map(|d| crate::index::scanner::normalize_lexically(d))
        .collect();

    // Outermost enclosing project dir; entries outside every project dir are
    // never owned (gitlibs/m2 checkouts).
    let Some(outermost) = project_dirs
        .iter()
        .filter(|dir| entry.starts_with(dir))
        .min_by_key(|dir| dir.components().count())
    else {
        return false;
    };

    let mut dir = entry.as_path();
    loop {
        let has_manifest = ["deps.edn", "project.clj", "lgx.edn"]
            .iter()
            .any(|m| dir.join(m).is_file());
        if has_manifest {
            return project_dirs.iter().any(|p| p == dir);
        }
        if dir == outermost.as_path() {
            // No manifest between the entry and its project root: the
            // (manifest-less) project still owns the dir.
            return true;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return true,
        }
    }
}

fn classify(path: &Path) -> Library {
    let path_str = path.to_string_lossy().into_owned();
    if path.extension().and_then(|e| e.to_str()) == Some("jar") {
        let (name, version) = parse_maven_jar(path).unwrap_or_else(|| (file_stem(path), None));
        Library {
            name,
            version,
            path: path_str,
            kind: LibraryKind::Jar,
        }
    } else {
        let (name, version) = parse_deps_gitlib(path)
            .or_else(|| parse_lgx_gitlib(path))
            .unwrap_or_else(|| (file_name(path), None));
        Library {
            name,
            version,
            path: path_str,
            kind: LibraryKind::Dir,
        }
    }
}

/// The `Normal` components of a path as `&str`, dropping the root and any
/// `.`/`..` — enough to recognize the Maven/gitlibs layouts by shape.
fn normal_components(path: &Path) -> Vec<&str> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect()
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Shortens a full 40-char git sha to 7 chars; leaves tags/other refs as-is.
fn short_ref(reff: &str) -> String {
    if reff.len() >= 40 && reff.chars().all(|c| c.is_ascii_hexdigit()) {
        reff.chars().take(7).collect()
    } else {
        reff.to_string()
    }
}

/// `<name>` from a group and artifact, collapsed to `artifact` when equal
/// (Cursive convention: `aero`, not `aero/aero`).
fn coord_name(group: &str, artifact: &str) -> String {
    if group == artifact {
        artifact.to_string()
    } else {
        format!("{group}/{artifact}")
    }
}

/// Maven jar: `<repo>/repository/<group segments…>/<artifact>/<version>/<artifact>-<version>.jar`.
/// Group is the segments between the `repository` anchor and the artifact dir,
/// joined with dots. Returns `None` for any jar that doesn't match this shape.
fn parse_maven_jar(path: &Path) -> Option<(String, Option<String>)> {
    let comps = normal_components(path);
    let n = comps.len();
    if n < 3 {
        return None;
    }
    let stem = comps[n - 1].strip_suffix(".jar")?;
    let version = comps[n - 2];
    let artifact = comps[n - 3];
    // Maven names its artifact `<artifact>-<version>.jar`; this identifies the
    // layout without a filesystem check.
    if stem != format!("{artifact}-{version}") {
        return None;
    }
    // Anchor on the `repository` dir *above* the artifact — searching only the
    // ancestors avoids matching an artifact literally named `repository` (which
    // would make the group slice below start past its end and panic).
    let repo_idx = comps[..n - 3].iter().rposition(|c| *c == "repository")?;
    let group_segs = &comps[repo_idx + 1..n - 3];
    if group_segs.is_empty() {
        return None;
    }
    let group = group_segs.join(".");
    Some((coord_name(&group, artifact), Some(version.to_string())))
}

/// tools.deps git checkout: `<home>/.gitlibs/libs/<group>/<artifact>/<sha>[/…]`.
fn parse_deps_gitlib(path: &Path) -> Option<(String, Option<String>)> {
    let comps = normal_components(path);
    let idx = comps.iter().position(|c| *c == ".gitlibs")?;
    if comps.get(idx + 1) != Some(&"libs") {
        return None;
    }
    let group = comps.get(idx + 2)?;
    let artifact = comps.get(idx + 3)?;
    let sha = comps.get(idx + 4)?;
    Some((coord_name(group, artifact), Some(short_ref(sha))))
}

/// lgx git checkout: `<LGX_HOME>/gitlibs/<host>/…/<repo>/<reff>[/src]`. The
/// resolver appends a `src` source subdir when present; drop it so the last
/// component is the reff and the one before it is the repo. `:deps/root`
/// subdirs other than `src` can't be recovered from the path, so those degrade
/// to using the subdir as the "reff" — acceptable for a best-effort panel.
fn parse_lgx_gitlib(path: &Path) -> Option<(String, Option<String>)> {
    let comps = normal_components(path);
    let idx = comps.iter().position(|c| *c == "gitlibs")?;
    // Distinguish a real lgx checkout from an incidental `gitlibs/` dir: it sits
    // directly under `.lgx/` (the default `$LGX_HOME`) or its first segment is a
    // git host (contains a dot, e.g. `github.com`). Otherwise fall through to
    // the basename fallback rather than inventing a bogus name/version.
    let under_lgx_home = idx >= 1 && comps[idx - 1] == ".lgx";
    let host_like = comps.get(idx + 1).is_some_and(|c| c.contains('.'));
    if !under_lgx_home && !host_like {
        return None;
    }
    let mut after = &comps[idx + 1..];
    if after.last() == Some(&"src") {
        after = &after[..after.len() - 1];
    }
    let m = after.len();
    if m < 2 {
        return None;
    }
    Some((after[m - 2].to_string(), Some(short_ref(after[m - 1]))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(name: &str, version: Option<&str>, path: &str, kind: LibraryKind) -> Library {
        Library {
            name: name.to_string(),
            version: version.map(|v| v.to_string()),
            path: path.to_string(),
            kind,
        }
    }

    // A realistic 40-char git sha; its short form is the first 7 chars.
    const SHA: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
    const SHORT: &str = "a1b2c3d";

    /// Writes a manifest so a dir counts as a project root for the ownership
    /// rule.
    fn mk_manifest(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("deps.edn"), "{}").unwrap();
    }

    #[test]
    fn own_alias_dirs_under_the_root_project_are_excluded() {
        // Alias :extra-paths (dev, src/cljc) leak into resolved classpaths;
        // their nearest manifest ancestor is the root project → excluded.
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        mk_manifest(ws);
        std::fs::create_dir_all(ws.join("dev")).unwrap();
        std::fs::create_dir_all(ws.join("src/cljc")).unwrap();

        let out = from_entries(
            &[],
            &[ws.to_path_buf()],
            &[ws.join("dev"), ws.join("src/cljc")],
        );
        assert!(out.is_empty(), "own dirs must be excluded: {out:?}");
    }

    #[test]
    fn detected_subproject_dirs_are_excluded() {
        // A subproject's source dir on the root's classpath: nearest manifest
        // ancestor is libs/x, a known project → excluded (it has its own node).
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        mk_manifest(ws);
        mk_manifest(&ws.join("libs/x"));
        std::fs::create_dir_all(ws.join("libs/x/src")).unwrap();

        let out = from_entries(
            &[],
            &[ws.to_path_buf(), ws.join("libs/x")],
            &[ws.join("libs/x/src")],
        );
        assert!(out.is_empty(), "subproject dirs must be excluded: {out:?}");
    }

    #[test]
    fn vendored_non_project_checkout_is_kept() {
        // A gitignored in-workspace :local/root checkout: vendor/y has a
        // manifest but is NOT a resolved project → its dir stays listed (the
        // panel is the only way to browse it).
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        mk_manifest(ws);
        mk_manifest(&ws.join("vendor/y"));
        std::fs::create_dir_all(ws.join("vendor/y/src")).unwrap();

        let out = from_entries(&[], &[ws.to_path_buf()], &[ws.join("vendor/y/src")]);
        assert_eq!(out.len(), 1, "vendored checkout must be kept: {out:?}");
        assert_eq!(out[0].path, ws.join("vendor/y/src").display().to_string());
    }

    #[test]
    fn parent_relative_sibling_project_entry_is_excluded() {
        // lgx keeps `:local/root "../common"` verbatim: the entry arrives as
        // `<ws>/app/../common/src` and must still resolve to the sibling
        // project `common` (a resolved project → excluded).
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        mk_manifest(&ws.join("app"));
        mk_manifest(&ws.join("common"));
        std::fs::create_dir_all(ws.join("common/src")).unwrap();

        let out = from_entries(
            &[],
            &[ws.join("app"), ws.join("common")],
            &[ws.join("app/../common/src")],
        );
        assert!(
            out.is_empty(),
            "parent-relative sibling project entry must be excluded: {out:?}"
        );
    }

    #[test]
    fn jar_inside_a_project_dir_is_kept() {
        // A jar built into target/ is a real dependency artifact; jars are
        // never ownership-filtered.
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        mk_manifest(ws);
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join("target/lib.jar"), b"").unwrap();

        let out = from_entries(&[], &[ws.to_path_buf()], &[ws.join("target/lib.jar")]);
        assert_eq!(out.len(), 1, "in-project jars must be kept: {out:?}");
    }

    #[test]
    fn dir_outside_every_project_dir_is_kept() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        mk_manifest(ws);
        let gitlib = tempfile::TempDir::new().unwrap();
        let dep = gitlib.path().join(".gitlibs/libs/g/a/abc");
        std::fs::create_dir_all(&dep).unwrap();

        let out = from_entries(&[], &[ws.to_path_buf()], std::slice::from_ref(&dep));
        assert_eq!(out.len(), 1, "out-of-workspace dirs must be kept: {out:?}");
    }

    #[test]
    fn manifest_less_root_still_owns_its_bare_dirs() {
        // The root project exists even with no manifest at the root; its bare
        // source dirs (no manifest ancestor at all) are still its own.
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("src")).unwrap();

        let out = from_entries(&[], &[ws.to_path_buf()], &[ws.join("src")]);
        assert!(out.is_empty(), "manifest-less root owns its dirs: {out:?}");
    }

    #[test]
    fn maven_jar_parses_group_artifact_version() {
        let p = "/home/u/.m2/repository/babashka/fs/0.5.30/fs-0.5.30.jar";
        let out = from_entries(&[], &[], &[PathBuf::from(p)]);
        assert_eq!(
            out,
            vec![lib("babashka/fs", Some("0.5.30"), p, LibraryKind::Jar)]
        );
    }

    #[test]
    fn maven_jar_collapses_group_equal_artifact() {
        let p = "/home/u/.m2/repository/aero/aero/1.1.6/aero-1.1.6.jar";
        let out = from_entries(&[], &[], &[PathBuf::from(p)]);
        assert_eq!(out, vec![lib("aero", Some("1.1.6"), p, LibraryKind::Jar)]);
    }

    #[test]
    fn maven_jar_joins_multi_segment_group_with_dots() {
        let p = "/home/u/.m2/repository/org/clojure/clojure/1.11.1/clojure-1.11.1.jar";
        let out = from_entries(&[], &[], &[PathBuf::from(p)]);
        assert_eq!(
            out,
            vec![lib(
                "org.clojure/clojure",
                Some("1.11.1"),
                p,
                LibraryKind::Jar
            )]
        );
    }

    #[test]
    fn non_maven_jar_falls_back_to_file_stem() {
        let p = "/opt/vendored/some-lib.jar";
        let out = from_entries(&[], &[], &[PathBuf::from(p)]);
        assert_eq!(out, vec![lib("some-lib", None, p, LibraryKind::Jar)]);
    }

    #[test]
    fn maven_jar_with_artifact_named_repository_does_not_panic() {
        // The artifact dir is literally `repository`; anchoring on the m2 root
        // (not the artifact) must avoid a slice-out-of-order panic.
        let p = "/home/u/.m2/repository/com/acme/repository/1.0.0/repository-1.0.0.jar";
        let out = from_entries(&[], &[], &[PathBuf::from(p)]);
        assert_eq!(
            out,
            vec![lib(
                "com.acme/repository",
                Some("1.0.0"),
                p,
                LibraryKind::Jar
            )]
        );
    }

    #[test]
    fn deps_gitlib_dir_yields_group_artifact_and_short_sha() {
        let p = format!("/home/u/.gitlibs/libs/io.github.foo/bar/{SHA}");
        let out = from_entries(&[], &[], &[PathBuf::from(&p)]);
        assert_eq!(
            out,
            vec![lib("io.github.foo/bar", Some(SHORT), &p, LibraryKind::Dir)]
        );
    }

    #[test]
    fn lgx_gitlib_dir_yields_repo_and_short_ref_ignoring_src_subdir() {
        let p = format!("/home/u/.lgx/gitlibs/github.com/some/cool-lib/{SHA}/src");
        let out = from_entries(&[], &[], &[PathBuf::from(&p)]);
        assert_eq!(
            out,
            vec![lib("cool-lib", Some(SHORT), &p, LibraryKind::Dir)]
        );
    }

    #[test]
    fn unrecognized_dir_yields_basename_no_version() {
        let p = "/home/u/checkouts/my-local-lib";
        let out = from_entries(&[], &[], &[PathBuf::from(p)]);
        assert_eq!(out, vec![lib("my-local-lib", None, p, LibraryKind::Dir)]);
    }

    #[test]
    fn incidental_gitlibs_dir_falls_back_to_basename() {
        // An ordinary local dir that merely contains a `gitlibs` component (no
        // `.lgx` parent, no host-like segment) must not be parsed as a checkout.
        let p = "/repo/vendor/gitlibs/foo/bar";
        let out = from_entries(&[], &[], &[PathBuf::from(p)]);
        assert_eq!(out, vec![lib("bar", None, p, LibraryKind::Dir)]);
    }

    #[test]
    fn lgx_gitlib_under_custom_home_uses_host_like_segment() {
        // A custom `$LGX_HOME` (no `.lgx` parent) is still recognized because
        // the first segment after `gitlibs` is a git host.
        let p = format!("/opt/cache/gitlibs/github.com/some/cool-lib/{SHA}/src");
        let out = from_entries(&[], &[], &[PathBuf::from(&p)]);
        assert_eq!(
            out,
            vec![lib("cool-lib", Some(SHORT), &p, LibraryKind::Dir)]
        );
    }

    #[test]
    fn project_own_source_paths_are_excluded_but_in_workspace_local_dep_is_kept() {
        // The project's own source roots — not an in-workspace `:local/root`
        // dependency, which lives outside them.
        let own_paths = vec![
            PathBuf::from("/home/u/project/src"),
            PathBuf::from("/home/u/project/resources"),
        ];
        let entries = vec![
            PathBuf::from("/home/u/project/src"),
            PathBuf::from("/home/u/project/resources"),
            PathBuf::from("/home/u/project/vendored-lib"), // in-workspace local dep
            PathBuf::from("/home/u/.m2/repository/aero/aero/1.1.6/aero-1.1.6.jar"),
        ];
        let out = from_entries(&own_paths, &[], &entries);
        let names: Vec<&str> = out.iter().map(|l| l.name.as_str()).collect();
        // `src`/`resources` dropped; the local dep under the workspace is kept.
        assert_eq!(names, vec!["aero", "vendored-lib"]);
    }

    #[test]
    fn local_dep_nested_under_a_source_path_is_kept() {
        // `source_paths` always unions in `root/test`; a `:local/root` dep whose
        // classpath entry sits *under* `test/` must not be excluded by prefix.
        let own_paths = vec![
            PathBuf::from("/home/u/project/src"),
            PathBuf::from("/home/u/project/test"),
        ];
        let entries = vec![
            PathBuf::from("/home/u/project/test"), // own test root → excluded
            PathBuf::from("/home/u/project/test/fixtures/my-lib/src"), // local dep → kept
        ];
        let out = from_entries(&own_paths, &[], &entries);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "src"); // basename fallback for a bare local dir
    }

    #[test]
    fn duplicate_paths_collapse_to_one_library() {
        let p = "/home/u/.m2/repository/aero/aero/1.1.6/aero-1.1.6.jar";
        let out = from_entries(&[], &[], &[PathBuf::from(p), PathBuf::from(p)]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn sorted_by_name_then_version() {
        let a = "/home/u/.m2/repository/aero/aero/1.1.6/aero-1.1.6.jar";
        let b = "/home/u/.m2/repository/babashka/fs/0.5.30/fs-0.5.30.jar";
        let c1 = "/home/u/.m2/repository/org/clojure/clojure/1.10.0/clojure-1.10.0.jar";
        let c2 = "/home/u/.m2/repository/org/clojure/clojure/1.11.1/clojure-1.11.1.jar";
        let out = from_entries(
            &[],
            &[],
            &[
                PathBuf::from(c2),
                PathBuf::from(b),
                PathBuf::from(c1),
                PathBuf::from(a),
            ],
        );
        let names: Vec<(&str, Option<&str>)> = out
            .iter()
            .map(|l| (l.name.as_str(), l.version.as_deref()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("aero", Some("1.1.6")),
                ("babashka/fs", Some("0.5.30")),
                ("org.clojure/clojure", Some("1.10.0")),
                ("org.clojure/clojure", Some("1.11.1")),
            ]
        );
    }

    #[test]
    fn serializes_kind_lowercase_and_omits_absent_version() {
        let jar = lib("aero", Some("1.1.6"), "/x/aero.jar", LibraryKind::Jar);
        let v = serde_json::to_value(&jar).unwrap();
        assert_eq!(v["kind"], "jar");
        assert_eq!(v["version"], "1.1.6");

        let dir = lib("local", None, "/x/local", LibraryKind::Dir);
        let v = serde_json::to_value(&dir).unwrap();
        assert_eq!(v["kind"], "dir");
        assert!(
            v.get("version").is_none(),
            "version must be omitted when None"
        );
    }
}
