//! Maps resolved classpath entries to the external-library list the editor's
//! "External Libraries" panel renders.
//!
//! Pure path logic — no filesystem access — so it is fully unit-testable with
//! fabricated paths. The handler feeds it the same entries `resolve_and_index_libs`
//! derives (deps.edn `.cpcache` classpath, Leiningen direct-dep JARs, or lgx
//! source dirs) and serializes the result over the custom LSP request.

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
/// Entries under one of `own_paths` (the project's own source roots, e.g.
/// `src`/`resources`) are excluded — but an in-workspace `:local/root`
/// dependency, which lives outside those roots, is kept. Duplicate absolute
/// paths collapse to one library, and the result is sorted by name, then
/// version, then path (a total order, so the panel is deterministic).
pub fn from_entries(own_paths: &[PathBuf], entries: &[PathBuf]) -> Vec<Library> {
    let mut seen: HashSet<&PathBuf> = HashSet::new();
    let mut libs: Vec<Library> = Vec::new();
    for entry in entries {
        if own_paths.iter().any(|p| entry.starts_with(p)) {
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

    #[test]
    fn maven_jar_parses_group_artifact_version() {
        let p = "/home/u/.m2/repository/babashka/fs/0.5.30/fs-0.5.30.jar";
        let out = from_entries(&[], &[PathBuf::from(p)]);
        assert_eq!(
            out,
            vec![lib("babashka/fs", Some("0.5.30"), p, LibraryKind::Jar)]
        );
    }

    #[test]
    fn maven_jar_collapses_group_equal_artifact() {
        let p = "/home/u/.m2/repository/aero/aero/1.1.6/aero-1.1.6.jar";
        let out = from_entries(&[], &[PathBuf::from(p)]);
        assert_eq!(out, vec![lib("aero", Some("1.1.6"), p, LibraryKind::Jar)]);
    }

    #[test]
    fn maven_jar_joins_multi_segment_group_with_dots() {
        let p = "/home/u/.m2/repository/org/clojure/clojure/1.11.1/clojure-1.11.1.jar";
        let out = from_entries(&[], &[PathBuf::from(p)]);
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
        let out = from_entries(&[], &[PathBuf::from(p)]);
        assert_eq!(out, vec![lib("some-lib", None, p, LibraryKind::Jar)]);
    }

    #[test]
    fn maven_jar_with_artifact_named_repository_does_not_panic() {
        // The artifact dir is literally `repository`; anchoring on the m2 root
        // (not the artifact) must avoid a slice-out-of-order panic.
        let p = "/home/u/.m2/repository/com/acme/repository/1.0.0/repository-1.0.0.jar";
        let out = from_entries(&[], &[PathBuf::from(p)]);
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
        let out = from_entries(&[], &[PathBuf::from(&p)]);
        assert_eq!(
            out,
            vec![lib("io.github.foo/bar", Some(SHORT), &p, LibraryKind::Dir)]
        );
    }

    #[test]
    fn lgx_gitlib_dir_yields_repo_and_short_ref_ignoring_src_subdir() {
        let p = format!("/home/u/.lgx/gitlibs/github.com/some/cool-lib/{SHA}/src");
        let out = from_entries(&[], &[PathBuf::from(&p)]);
        assert_eq!(
            out,
            vec![lib("cool-lib", Some(SHORT), &p, LibraryKind::Dir)]
        );
    }

    #[test]
    fn unrecognized_dir_yields_basename_no_version() {
        let p = "/home/u/checkouts/my-local-lib";
        let out = from_entries(&[], &[PathBuf::from(p)]);
        assert_eq!(out, vec![lib("my-local-lib", None, p, LibraryKind::Dir)]);
    }

    #[test]
    fn incidental_gitlibs_dir_falls_back_to_basename() {
        // An ordinary local dir that merely contains a `gitlibs` component (no
        // `.lgx` parent, no host-like segment) must not be parsed as a checkout.
        let p = "/repo/vendor/gitlibs/foo/bar";
        let out = from_entries(&[], &[PathBuf::from(p)]);
        assert_eq!(out, vec![lib("bar", None, p, LibraryKind::Dir)]);
    }

    #[test]
    fn lgx_gitlib_under_custom_home_uses_host_like_segment() {
        // A custom `$LGX_HOME` (no `.lgx` parent) is still recognized because
        // the first segment after `gitlibs` is a git host.
        let p = format!("/opt/cache/gitlibs/github.com/some/cool-lib/{SHA}/src");
        let out = from_entries(&[], &[PathBuf::from(&p)]);
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
        let out = from_entries(&own_paths, &entries);
        let names: Vec<&str> = out.iter().map(|l| l.name.as_str()).collect();
        // `src`/`resources` dropped; the local dep under the workspace is kept.
        assert_eq!(names, vec!["aero", "vendored-lib"]);
    }

    #[test]
    fn duplicate_paths_collapse_to_one_library() {
        let p = "/home/u/.m2/repository/aero/aero/1.1.6/aero-1.1.6.jar";
        let out = from_entries(&[], &[PathBuf::from(p), PathBuf::from(p)]);
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
