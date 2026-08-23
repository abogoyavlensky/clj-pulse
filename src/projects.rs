//! Multi-project (monorepo) support: subproject detection, the per-project
//! config model, EDN/JSON parsing, and per-key merge into resolved projects.

use std::path::{Path, PathBuf};

/// Default stage-3 command for deps.edn projects.
pub const DEPS_CMD: &str = "clojure -A:dev:test -Spath";
/// Default stage-3 command for Leiningen projects.
pub const LEIN_CMD: &str = "lein classpath";

/// Per-key classpath overrides from one config entry. `None` = not specified,
/// keep the value from the layer below (file config or the defaults).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClasspathOverride {
    pub enabled: Option<bool>,
    pub cmd: Option<String>,
}

/// One `:projects` entry as configured (file or editor layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    /// Relative to the workspace root; `"."` (or `""`/`"./"`) = the root.
    pub path: String,
    pub classpath: ClasspathOverride,
}

/// The manifest flavor of a resolved project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKindTag {
    Deps,
    Lein,
    Lgx,
}

/// A fully resolved project: detection + defaults + config layers merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// `"."` for the root.
    pub rel_path: String,
    /// Absolute directory.
    pub dir: PathBuf,
    pub kind: ProjectKindTag,
    pub classpath_enabled: bool,
    /// `None` for lgx projects (stage 3 does not apply).
    pub classpath_cmd: Option<String>,
}

/// Parses `{:projects [...]}` from `.clj-pulse/config.edn` contents. Tolerant:
/// malformed EDN or shapes yield an empty (or partial) list, never a panic.
pub fn parse_edn(contents: &str) -> Vec<ProjectEntry> {
    use crate::edn::{get, kw};
    use edn_format::Value;

    let Ok(Value::Map(top)) = edn_format::parse_str(contents) else {
        return Vec::new();
    };
    let Some(Value::Vector(entries)) = get(&top, kw("projects")) else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|e| {
            let Value::Map(spec) = e else { return None };
            let Some(Value::String(path)) = get(spec, kw("path")) else {
                tracing::warn!("projects config: entry without :path ignored");
                return None;
            };
            let mut classpath = ClasspathOverride::default();
            if let Some(Value::Map(cp)) = get(spec, kw("classpath")) {
                if let Some(Value::Boolean(enabled)) = get(cp, kw("enabled")) {
                    classpath.enabled = Some(*enabled);
                }
                if let Some(Value::String(cmd)) = get(cp, kw("cmd")) {
                    classpath.cmd = Some(cmd.clone());
                }
            }
            Some(ProjectEntry {
                path: path.clone(),
                classpath,
            })
        })
        .collect()
}

/// Parses `{"projects": [...]}` from a JSON settings object (the bare object —
/// callers unwrap any channel envelope first). Tolerant of partial shapes.
pub fn parse_json(v: &serde_json::Value) -> Vec<ProjectEntry> {
    let Some(entries) = v.get("projects").and_then(|p| p.as_array()) else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|e| {
            let Some(path) = e.get("path").and_then(|p| p.as_str()) else {
                tracing::warn!("projects config: entry without \"path\" ignored");
                return None;
            };
            let cp = e.get("classpath");
            let classpath = ClasspathOverride {
                enabled: cp.and_then(|c| c.get("enabled")).and_then(|v| v.as_bool()),
                cmd: cp
                    .and_then(|c| c.get("cmd"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            };
            Some(ProjectEntry {
                path: path.to_string(),
                classpath,
            })
        })
        .collect()
}

/// Merges detection, defaults, and the two config layers into the resolved
/// project list: root (`"."`) first, then the rest sorted by `rel_path`.
///
/// Entries are overrides *and additions*: a listed path that detection missed
/// (e.g. under a gitignored dir) is added as a full project with subproject
/// defaults, provided it has a manifest; a listed path with no manifest is
/// dropped with a warning. A non-empty `CLJ_PULSE_DISABLE_CLASSPATH_CLI`
/// forces `classpath_enabled = false` for every project.
pub fn resolve(
    root: &Path,
    detected: &[PathBuf],
    file: &[ProjectEntry],
    editor: &[ProjectEntry],
) -> Vec<Project> {
    let disable = std::env::var("CLJ_PULSE_DISABLE_CLASSPATH_CLI").is_ok_and(|v| !v.is_empty());
    resolve_with_disable(root, detected, file, editor, disable)
}

/// [`resolve`] with the env kill-switch injectable for tests.
fn resolve_with_disable(
    root: &Path,
    detected: &[PathBuf],
    file: &[ProjectEntry],
    editor: &[ProjectEntry],
    disable: bool,
) -> Vec<Project> {
    use std::collections::{BTreeSet, HashMap};

    // Merge the two config layers per path, per key: file first, editor over it.
    let mut overrides: HashMap<String, ClasspathOverride> = HashMap::new();
    for entry in file.iter().chain(editor) {
        let over = overrides.entry(normalize(&entry.path)).or_default();
        if let Some(enabled) = entry.classpath.enabled {
            over.enabled = Some(enabled);
        }
        if let Some(cmd) = &entry.classpath.cmd {
            over.cmd = Some(cmd.clone());
        }
    }

    // The project set: everything detected, plus configured additions whose
    // dir actually has a manifest (the gitignored-subproject case).
    let mut rels: BTreeSet<String> = detected
        .iter()
        .map(|p| normalize(&p.to_string_lossy()))
        .filter(|r| r != ".")
        .collect();
    for rel in overrides.keys() {
        if rel == "." || rels.contains(rel) {
            continue;
        }
        if !is_workspace_relative(rel) {
            tracing::warn!(
                "projects config: {} escapes the workspace root — entry ignored",
                rel
            );
            continue;
        }
        if kind_of(&root.join(rel)).is_some() {
            rels.insert(rel.clone());
        } else {
            tracing::warn!(
                "projects config: {} has no deps.edn/project.clj/lgx.edn — entry ignored",
                rel
            );
        }
    }

    // Root first, then rel_path-sorted (BTreeSet iterates sorted).
    let mut projects = Vec::with_capacity(rels.len() + 1);
    projects.push(build_project(root, ".", true, &overrides, disable));
    for rel in &rels {
        projects.push(build_project(root, rel, false, &overrides, disable));
    }
    projects
}

/// Resolves one project from its defaults and any override for its path.
fn build_project(
    root: &Path,
    rel: &str,
    is_root: bool,
    overrides: &std::collections::HashMap<String, ClasspathOverride>,
    disable: bool,
) -> Project {
    let dir = if is_root {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    // A root with no manifest still resolves (kind Deps); stage 3 additionally
    // checks the manifest exists before running the command.
    let kind = kind_of(&dir).unwrap_or(ProjectKindTag::Deps);
    let default_cmd = match kind {
        ProjectKindTag::Deps => Some(DEPS_CMD.to_string()),
        ProjectKindTag::Lein => Some(LEIN_CMD.to_string()),
        ProjectKindTag::Lgx => None,
    };
    let over = overrides.get(rel);
    let enabled = over.and_then(|o| o.enabled).unwrap_or(is_root);
    // lgx projects never run a stage-3 command (`lgx::resolve` is internal);
    // a configured `:cmd` cannot override that.
    let cmd = if kind == ProjectKindTag::Lgx {
        None
    } else {
        over.and_then(|o| o.cmd.clone()).or(default_cmd)
    };
    Project {
        rel_path: rel.to_string(),
        dir,
        kind,
        classpath_enabled: enabled && !disable,
        classpath_cmd: cmd,
    }
}

/// The manifest kind of `dir`, `None` when it has no manifest. `lgx.edn` wins
/// (matching [`crate::config::project_kind`]), then `deps.edn` over
/// `project.clj` (deps.edn is authoritative elsewhere too).
fn kind_of(dir: &Path) -> Option<ProjectKindTag> {
    if dir.join("lgx.edn").exists() {
        Some(ProjectKindTag::Lgx)
    } else if dir.join("deps.edn").exists() {
        Some(ProjectKindTag::Deps)
    } else if dir.join("project.clj").exists() {
        Some(ProjectKindTag::Lein)
    } else {
        None
    }
}

/// Whether a normalized project path stays inside the workspace: relative,
/// with no `..` components. Configured paths are workspace-relative by
/// contract; anything else must not index external sources or run commands
/// in unintended directories.
fn is_workspace_relative(rel: &str) -> bool {
    let path = Path::new(rel);
    !path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Normalizes a configured project path so `""`, `"."`, and `"./"` all mean
/// the root, trailing slashes are dropped, and separators are `/`.
fn normalize(path: &str) -> String {
    let p = path.trim().replace('\\', "/");
    let p = p.strip_prefix("./").unwrap_or(&p);
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        ".".to_string()
    } else {
        p.to_string()
    }
}

/// Detects subproject directories under `root`: directories containing a
/// `deps.edn`, `project.clj`, or `lgx.edn`, excluding `root` itself. Paths are
/// returned relative to `root`, sorted. The walk respects `.gitignore` and is
/// capped at depth 4.
pub fn detect(root: &Path) -> Vec<PathBuf> {
    const MANIFESTS: [&str; 3] = ["deps.edn", "project.clj", "lgx.edn"];

    let mut found: Vec<PathBuf> = Vec::new();
    // `require_git(false)`: gitignore rules apply whether or not a `.git`
    // exists, matching the scanner's scoped walks — pruning `target/`,
    // `node_modules/`, vendored checkouts is the desired default everywhere.
    let mut builder = ignore::WalkBuilder::new(root);
    builder.max_depth(Some(4)).require_git(false);
    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if !path.is_dir() || path == root {
            continue;
        }
        if MANIFESTS.iter().any(|m| path.join(m).exists()) {
            if let Ok(rel) = path.strip_prefix(root) {
                found.push(rel.to_path_buf());
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const MANIFESTS: [&str; 3] = ["deps.edn", "project.clj", "lgx.edn"];

    /// Creates `dir` under `root` with the given manifest file.
    fn mk_project(root: &Path, dir: &str, manifest: &str) {
        let d = root.join(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(manifest), "{}").unwrap();
    }

    #[test]
    fn detect_finds_subprojects_for_each_manifest_kind() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        for (i, m) in MANIFESTS.iter().enumerate() {
            mk_project(root, &format!("apps/p{i}"), m);
        }

        assert_eq!(
            detect(root),
            vec![
                PathBuf::from("apps/p0"),
                PathBuf::from("apps/p1"),
                PathBuf::from("apps/p2"),
            ]
        );
    }

    #[test]
    fn detect_excludes_the_root_itself() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        assert!(detect(root).is_empty());
    }

    #[test]
    fn detect_finds_nested_subproject_at_depth_three() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        mk_project(root, "libs/group/common", "deps.edn");
        assert_eq!(detect(root), vec![PathBuf::from("libs/group/common")]);
    }

    #[test]
    fn detect_skips_dirs_past_depth_four() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // Manifest at depth 5 — the project dir is past the depth-4 cap.
        mk_project(root, "a/b/c/d/e", "deps.edn");
        // Depth 4 is still within the cap.
        mk_project(root, "a/b/c/d", "deps.edn");
        assert_eq!(detect(root), vec![PathBuf::from("a/b/c/d")]);
    }

    #[test]
    fn detect_honors_gitignore() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // gitignore rules only apply inside a git repo.
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        mk_project(root, "ignored/sub", "deps.edn");
        mk_project(root, "kept", "deps.edn");
        assert_eq!(detect(root), vec![PathBuf::from("kept")]);
    }

    #[test]
    fn detect_empty_repo_yields_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(detect(tmp.path()).is_empty());
    }

    fn entry(path: &str, enabled: Option<bool>, cmd: Option<&str>) -> ProjectEntry {
        ProjectEntry {
            path: path.to_string(),
            classpath: ClasspathOverride {
                enabled,
                cmd: cmd.map(str::to_string),
            },
        }
    }

    fn resolve_plain(
        root: &Path,
        detected: &[PathBuf],
        file: &[ProjectEntry],
        editor: &[ProjectEntry],
    ) -> Vec<Project> {
        resolve_with_disable(root, detected, file, editor, false)
    }

    fn find<'a>(projects: &'a [Project], rel: &str) -> &'a Project {
        projects
            .iter()
            .find(|p| p.rel_path == rel)
            .unwrap_or_else(|| panic!("no project {rel} in {projects:?}"))
    }

    #[test]
    fn resolve_defaults_per_manifest_kind() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        mk_project(root, "a", "deps.edn");
        mk_project(root, "b", "project.clj");
        mk_project(root, "c", "lgx.edn");
        let detected = detect(root);

        let projects = resolve_plain(root, &detected, &[], &[]);

        // Root first, then rel_path-sorted.
        let rels: Vec<&str> = projects.iter().map(|p| p.rel_path.as_str()).collect();
        assert_eq!(rels, vec![".", "a", "b", "c"]);

        let dot = find(&projects, ".");
        assert_eq!(dot.kind, ProjectKindTag::Deps);
        assert!(dot.classpath_enabled, "root defaults to enabled");
        assert_eq!(dot.classpath_cmd.as_deref(), Some(DEPS_CMD));
        assert_eq!(dot.dir, root);

        let a = find(&projects, "a");
        assert_eq!(a.kind, ProjectKindTag::Deps);
        assert!(!a.classpath_enabled, "subprojects default to disabled");
        assert_eq!(a.classpath_cmd.as_deref(), Some(DEPS_CMD));
        assert_eq!(a.dir, root.join("a"));

        let b = find(&projects, "b");
        assert_eq!(b.kind, ProjectKindTag::Lein);
        assert_eq!(b.classpath_cmd.as_deref(), Some(LEIN_CMD));

        let c = find(&projects, "c");
        assert_eq!(c.kind, ProjectKindTag::Lgx);
        assert_eq!(c.classpath_cmd, None, "lgx projects have no command");
    }

    #[test]
    fn resolve_file_entry_overrides_one_key_keeps_other_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        mk_project(root, "a", "deps.edn");
        let detected = detect(root);

        let file = vec![entry("a", Some(true), None)];
        let projects = resolve_plain(root, &detected, &file, &[]);

        let a = find(&projects, "a");
        assert!(a.classpath_enabled, "file :enabled override applies");
        assert_eq!(
            a.classpath_cmd.as_deref(),
            Some(DEPS_CMD),
            ":cmd keeps the default"
        );
    }

    #[test]
    fn resolve_editor_overrides_file_per_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        mk_project(root, "a", "deps.edn");
        let detected = detect(root);

        let file = vec![entry("a", Some(true), Some("file-cmd"))];
        let editor = vec![entry("a", None, Some("editor-cmd"))];
        let projects = resolve_plain(root, &detected, &file, &editor);

        let a = find(&projects, "a");
        assert!(
            a.classpath_enabled,
            "file :enabled survives (editor silent)"
        );
        assert_eq!(
            a.classpath_cmd.as_deref(),
            Some("editor-cmd"),
            "editor :cmd wins over file"
        );
    }

    #[test]
    fn resolve_drops_entry_for_path_with_no_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        fs::create_dir(root.join("plain")).unwrap();

        let file = vec![
            entry("plain", Some(true), None),
            entry("missing", None, None),
        ];
        let projects = resolve_plain(root, &detect(root), &file, &[]);

        let rels: Vec<&str> = projects.iter().map(|p| p.rel_path.as_str()).collect();
        assert_eq!(
            rels,
            vec!["."],
            "manifest-less entries dropped: {projects:?}"
        );
    }

    #[test]
    fn resolve_root_always_present_even_without_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let projects = resolve_plain(root, &[], &[], &[]);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].rel_path, ".");
        assert_eq!(projects[0].kind, ProjectKindTag::Deps);
        assert!(projects[0].classpath_enabled);
    }

    #[test]
    fn resolve_adds_undetected_entry_with_manifest_using_subproject_defaults() {
        // The gitignored-dir case: repos/b exists with a manifest but detection
        // (gitignore-respecting) never saw it. An explicit entry adds it.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        mk_project(root, "repos/b", "deps.edn");

        let file = vec![entry("repos/b", None, None)];
        let projects = resolve_plain(root, &[], &file, &[]);

        let b = find(&projects, "repos/b");
        assert_eq!(b.kind, ProjectKindTag::Deps);
        assert!(
            !b.classpath_enabled,
            "added projects get subproject defaults"
        );
        assert_eq!(b.classpath_cmd.as_deref(), Some(DEPS_CMD));
        assert_eq!(b.dir, root.join("repos/b"));
    }

    #[test]
    fn resolve_normalizes_root_path_spellings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();

        for spelling in ["", ".", "./"] {
            let file = vec![entry(spelling, Some(false), None)];
            let projects = resolve_plain(root, &[], &file, &[]);
            let dot = find(&projects, ".");
            assert!(
                !dot.classpath_enabled,
                "spelling {spelling:?} must override the root"
            );
        }
    }

    #[test]
    fn resolve_disable_env_forces_every_project_off() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        mk_project(root, "a", "deps.edn");

        let file = vec![entry("a", Some(true), None)];
        let projects = resolve_with_disable(root, &detect(root), &file, &[], true);
        assert!(
            projects.iter().all(|p| !p.classpath_enabled),
            "kill-switch must win over config: {projects:?}"
        );
    }

    #[test]
    fn resolve_rejects_paths_escaping_the_workspace() {
        // A sibling dir outside the workspace with a real manifest: neither a
        // `..` path nor an absolute path may pull it in.
        let outer = tempfile::TempDir::new().unwrap();
        let root = outer.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("deps.edn"), "{}").unwrap();
        mk_project(outer.path(), "sibling", "deps.edn");

        let abs = outer.path().join("sibling").display().to_string();
        let file = vec![entry("../sibling", None, None), entry(&abs, None, None)];
        let projects = resolve_plain(&root, &[], &file, &[]);

        let rels: Vec<&str> = projects.iter().map(|p| p.rel_path.as_str()).collect();
        assert_eq!(rels, vec!["."], "escaping entries dropped: {projects:?}");
    }

    #[test]
    fn resolve_lgx_project_never_gets_a_cmd_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        mk_project(root, "c", "lgx.edn");

        let file = vec![entry("c", Some(true), Some("evil -Spath"))];
        let projects = resolve_plain(root, &detect(root), &file, &[]);

        let c = find(&projects, "c");
        assert_eq!(
            c.classpath_cmd, None,
            "lgx projects must ignore :cmd overrides"
        );
    }

    #[test]
    fn parse_edn_reads_entries_and_partial_overrides() {
        let entries = parse_edn(
            r#"{:projects [{:path "apps/backend"
                            :classpath {:enabled true :cmd "clojure -A:dev -Spath"}}
                           {:path "." :classpath {:enabled false}}
                           {:path "libs/x"}]}"#,
        );
        assert_eq!(
            entries,
            vec![
                entry("apps/backend", Some(true), Some("clojure -A:dev -Spath")),
                entry(".", Some(false), None),
                entry("libs/x", None, None),
            ]
        );
    }

    #[test]
    fn parse_edn_tolerates_malformed_input() {
        assert!(parse_edn("{:projects").is_empty());
        assert!(parse_edn("[1 2 3]").is_empty());
        assert!(parse_edn("{:projects [{:classpath {:enabled true}}]}").is_empty());
        assert!(parse_edn("{:projects \"nope\"}").is_empty());
    }

    #[test]
    fn parse_json_reads_entries_and_partial_overrides() {
        let v = serde_json::json!({
            "projects": [
                {"path": "apps/backend",
                 "classpath": {"enabled": true, "cmd": "clojure -A:dev -Spath"}},
                {"path": ".", "classpath": {"enabled": false}},
                {"path": "libs/x"}
            ]
        });
        assert_eq!(
            parse_json(&v),
            vec![
                entry("apps/backend", Some(true), Some("clojure -A:dev -Spath")),
                entry(".", Some(false), None),
                entry("libs/x", None, None),
            ]
        );
    }

    #[test]
    fn parse_json_tolerates_malformed_input() {
        assert!(parse_json(&serde_json::json!(null)).is_empty());
        assert!(parse_json(&serde_json::json!("nope")).is_empty());
        assert!(parse_json(&serde_json::json!({"projects": "nope"})).is_empty());
        // Entries without a path are skipped, valid siblings kept.
        let v = serde_json::json!({"projects": [{"classpath": {}}, {"path": "a"}]});
        assert_eq!(parse_json(&v), vec![entry("a", None, None)]);
    }
}
