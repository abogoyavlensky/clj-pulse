use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::classpath;
use crate::config;
use crate::document::DocumentStore;
use crate::handlers;
use crate::index::extractor;
use crate::index::scanner;
use crate::index::Index;
use crate::jar_content;
use crate::leiningen;
use crate::lgx;
use crate::libraries;
use crate::projects;
use crate::settings;

/// Per-project classpath resolution status, as reported to the editor over
/// `clojurePulse/projects` (serialized lowercase).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClasspathStatus {
    /// Stage 3 is off for this project and stage 2 hasn't produced anything yet.
    Disabled,
    /// Stage 2 found a cached classpath (`.cpcache` / lgx / lein heuristic).
    Cached,
    /// A stage-3 command is running.
    Resolving,
    /// Stage 3 produced the authoritative classpath.
    Resolved,
    /// Nothing resolved (no cache, no successful command).
    Unresolved,
    /// The stage-3 command failed; stage-2 entries are kept.
    Error(String),
}

/// One project's library state: the classpath entries its libraries were last
/// derived from, plus the resolution status. Stage 3 compares against
/// `entries` to skip no-op re-indexing; re-reading `.cpcache` instead would
/// observe the `.cp` file `clojure -Spath` itself just wrote and always
/// conclude "no change".
#[derive(Debug, Clone)]
struct ProjectState {
    entries: std::collections::HashSet<std::path::PathBuf>,
    status: ClasspathStatus,
    /// Whether stage 2 indexed library sources *outside* the entries (let-go's
    /// pinned core). A project can contribute libraries with zero entries;
    /// removing it must still trigger a union rebuild.
    extra_indexed: bool,
}

impl ProjectState {
    fn empty(status: ClasspathStatus) -> Self {
        ProjectState {
            entries: std::collections::HashSet::new(),
            status,
            extra_indexed: false,
        }
    }
}

type SharedProjects = Arc<std::sync::Mutex<Vec<projects::Project>>>;
type SharedState = Arc<std::sync::Mutex<std::collections::HashMap<String, ProjectState>>>;
type SharedEditorConfig = Arc<std::sync::Mutex<Vec<projects::ProjectEntry>>>;
/// Bumped on every project-list re-resolve; a stage-3 run snapshots it at
/// launch and discards its result if the config changed mid-run.
type ConfigGeneration = Arc<std::sync::atomic::AtomicU64>;

/// Serializes stage-3 runs. Startup resolution and a config-watcher rerun can
/// overlap (a cold resolve may download for minutes); without the lock the
/// slower run — possibly carrying an obsolete command — would clear and
/// rebuild the index last, overwriting the newer result.
type ClasspathCliLock = Arc<tokio::sync::Mutex<()>>;

/// Serializes whole config-application tasks (`didChangeConfiguration`,
/// watched-file reruns, the startup library task). Without it two
/// back-to-back config notifications interleave their refresh/rescan/stage
/// work and the slower, staler task can apply last. Always acquired *before*
/// [`ClasspathCliLock`] (stage 3 runs inside an application task).
type ConfigApplyLock = Arc<tokio::sync::Mutex<()>>;

/// Stage-1 scan over the union of every project's source paths, merged into
/// the shared index; files no longer covered by any project are pruned by the
/// merge (open buffers kept). Returns the scan's (symbols, namespaces) counts.
fn rescan_all_sources(
    project_list: &[projects::Project],
    index: &Index,
    documents: &DocumentStore,
) -> anyhow::Result<(usize, usize)> {
    let mut seen = std::collections::HashSet::new();
    let mut scan_roots = Vec::new();
    for p in project_list {
        let paths = config::source_paths(&p.dir);
        tracing::info!("project {}: source paths: {:?}", p.rel_path, paths);
        for path in paths {
            if seen.insert(path.clone()) {
                scan_roots.push(scanner::ScanRoot {
                    project_dir: p.dir.clone(),
                    path,
                });
            }
        }
    }
    let new_index = scanner::build_index_scoped(&scan_roots, &index.extract_config())?;
    let counts = (new_index.symbols.len(), new_index.namespaces.len());
    Backend::warn_ns_collisions(index, &new_index);
    index.merge_project_from(new_index, &Backend::open_paths(documents));
    Ok(counts)
}

/// A Clojure project's classpath: deps.edn's `.cpcache` is authoritative (full
/// transitive classpath); only when it is empty do we consult a Leiningen
/// `project.clj` for its direct dependencies.
fn clojure_classpath(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut classpath = classpath::discover(root);
    if classpath.is_empty() && root.join("project.clj").exists() {
        classpath = leiningen::resolve(root);
    }
    classpath
}

/// Whether the project's manifest actually exists on disk — the gate for
/// running its stage-3 command (a root project with no manifest resolves with
/// deps defaults but must not spawn `clojure`).
fn has_manifest(p: &projects::Project) -> bool {
    match p.kind {
        projects::ProjectKindTag::Deps => p.dir.join("deps.edn").exists(),
        projects::ProjectKindTag::Lein => p.dir.join("project.clj").exists(),
        projects::ProjectKindTag::Lgx => false,
    }
}

/// Stage 2 for one project: discovers its cached classpath / dep dirs and
/// indexes them into the shared index. Returns the entries plus the count of
/// library sources indexed outside them (let-go's pinned core stdlib — a
/// pinned project with no deps of its own must still count as indexed).
fn stage2_index_project(
    workspace_root: &std::path::Path,
    p: &projects::Project,
    index: &Index,
) -> (Vec<std::path::PathBuf>, usize) {
    match p.kind {
        projects::ProjectKindTag::Lgx => {
            let dirs = lgx::resolve(&p.dir);
            scanner::index_dir_libs(&dirs, index);
            // Also index let-go's built-in core/stdlib from the source `lgx
            // install` fetched (only when `:lg-version` is pinned).
            let extra = lgx::index_letgo_core(&p.dir, index);
            (dirs, extra)
        }
        _ => {
            let classpath = clojure_classpath(&p.dir);
            if !classpath.is_empty() {
                // The under-root skip uses the *workspace* root: in-root
                // classpath dirs are another project's sources (indexed in
                // stage 1) or picked up lazily on didOpen.
                scanner::index_classpath_libs(workspace_root, classpath.clone(), index);
            }
            (classpath, 0)
        }
    }
}

/// The deduplicated union of every project's current lib entries.
fn lib_union(state: &std::collections::HashMap<String, ProjectState>) -> Vec<std::path::PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for ps in state.values() {
        for entry in &ps.entries {
            if seen.insert(entry.clone()) {
                out.push(entry.clone());
            }
        }
    }
    out
}

/// Rebuilds the library index from every project's current entries — per
/// project, per kind. A flat classpath scan over the union would wrongly skip
/// in-workspace lgx `:local/root` dirs and lose let-go core (which
/// `clear_libs` drops and only `index_letgo_core` re-sets).
fn rebuild_libs(
    workspace_root: &std::path::Path,
    project_list: &[projects::Project],
    state: &mut std::collections::HashMap<String, ProjectState>,
    index: &Index,
) {
    index.clear_libs();
    for p in project_list {
        let entries: Vec<std::path::PathBuf> = state
            .get(&p.rel_path)
            .map(|ps| ps.entries.iter().cloned().collect())
            .unwrap_or_default();
        match p.kind {
            projects::ProjectKindTag::Lgx => {
                scanner::index_dir_libs(&entries, index);
                // Even with no dep dirs: pinned core is indexed outside the
                // entries, and `clear_libs` dropped its marker. Track the
                // outcome so removing a core-only project later still forces
                // a union rebuild.
                let extra = lgx::index_letgo_core(&p.dir, index);
                if let Some(ps) = state.get_mut(&p.rel_path) {
                    ps.extra_indexed = extra > 0;
                }
            }
            _ if !entries.is_empty() => {
                scanner::index_classpath_libs(workspace_root, entries, index)
            }
            _ => {
                if let Some(ps) = state.get_mut(&p.rel_path) {
                    ps.extra_indexed = false;
                }
            }
        }
    }
}

/// Sets one project's status, creating the state entry if needed.
fn set_status(state_arc: &SharedState, rel_path: &str, status: ClasspathStatus) {
    let mut state = state_arc.lock().unwrap();
    state
        .entry(rel_path.to_string())
        .or_insert_with(|| ProjectState::empty(ClasspathStatus::Unresolved))
        .status = status;
}

/// Re-detects and re-resolves the project list from the current config layers,
/// bumping the config generation (the stale-result guard for in-flight stage-3
/// runs) and pruning state of removed projects. Returns the new list, whether
/// any pruned project had contributed libraries, and whether the list changed
/// (so callers can notify the panel).
fn refresh_projects(
    root: &std::path::Path,
    projects_arc: &SharedProjects,
    editor_config: &SharedEditorConfig,
    state_arc: &SharedState,
    generation: &ConfigGeneration,
) -> (Vec<projects::Project>, bool, bool) {
    let detected = projects::detect(root);
    let file = std::fs::read_to_string(root.join(".clj-pulse").join("config.edn"))
        .map(|src| projects::parse_edn(&src))
        .unwrap_or_default();
    let editor = editor_config.lock().unwrap().clone();
    let resolved = projects::resolve(root, &detected, &file, &editor);
    generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let list_changed = {
        let mut projects = projects_arc.lock().unwrap();
        let changed = *projects != resolved;
        *projects = resolved.clone();
        changed
    };
    let mut state = state_arc.lock().unwrap();
    let mut pruned_any = false;
    state.retain(|rel, ps| {
        let keep = resolved.iter().any(|p| p.rel_path == *rel);
        // A pruned project only forces a union rebuild when it actually
        // contributed libraries (entries, or let-go core outside them).
        if !keep && (!ps.entries.is_empty() || ps.extra_indexed) {
            pruned_any = true;
        }
        keep
    });
    (resolved, pruned_any, list_changed)
}

/// Reverts projects whose stage 3 is no longer active (disabled by a config
/// change) to their stage-2 truth, and rebuilds the library union when any
/// entry set changed — including entries dropped by removed projects
/// (`pruned_any`). Stage-2 libraries stay indexed: disabling only gates
/// stage 3. `force_revert` names projects to revert even while enabled — a
/// changed command must not keep the old command's entries indexed if the new
/// one fails. Returns whether the union was rebuilt.
fn reconcile_projects(
    workspace_root: &std::path::Path,
    resolved: &[projects::Project],
    state_arc: &SharedState,
    index: &Index,
    pruned_any: bool,
    force_revert: &[String],
) -> bool {
    let mut changed = pruned_any;
    for p in resolved
        .iter()
        .filter(|p| !p.classpath_enabled || force_revert.contains(&p.rel_path))
    {
        // Only projects still carrying stage-3 state need reverting.
        let had_stage3 = {
            let state = state_arc.lock().unwrap();
            state.get(&p.rel_path).is_some_and(|ps| {
                matches!(
                    ps.status,
                    ClasspathStatus::Resolving
                        | ClasspathStatus::Resolved
                        | ClasspathStatus::Error(_)
                )
            })
        };
        if !had_stage3 {
            continue;
        }
        // Fresh stage-2 discovery, without indexing (the rebuild below does it).
        let entries: std::collections::HashSet<std::path::PathBuf> =
            stage2_discover(p).into_iter().collect();
        let status = if entries.is_empty() {
            ClasspathStatus::Unresolved
        } else {
            ClasspathStatus::Cached
        };
        let mut state = state_arc.lock().unwrap();
        let entry = state
            .entry(p.rel_path.clone())
            .or_insert_with(|| ProjectState::empty(ClasspathStatus::Unresolved));
        if entry.entries != entries {
            entry.entries = entries;
            changed = true;
        }
        entry.status = status;
    }
    if changed {
        let mut state = state_arc.lock().unwrap();
        rebuild_libs(workspace_root, resolved, &mut state, index);
    }
    changed
}

/// A project's stage-2 classpath discovery, without indexing.
fn stage2_discover(p: &projects::Project) -> Vec<std::path::PathBuf> {
    match p.kind {
        projects::ProjectKindTag::Lgx => lgx::resolve(&p.dir),
        _ => clojure_classpath(&p.dir),
    }
}

/// The project whose own dir directly contains this manifest file. Manifests
/// live at project roots, so exact-parent matching is precise — and a deleted
/// subproject's manifest (its project already pruned from the list) resolves
/// to `None` instead of being misattributed to a surviving ancestor.
fn manifest_owner<'a>(
    project_list: &'a [projects::Project],
    manifest: &std::path::Path,
) -> Option<&'a projects::Project> {
    let dir = manifest.parent()?;
    project_list.iter().find(|p| p.dir == dir)
}

/// The project owning `path`: the one with the longest dir prefix.
fn owning_project<'a>(
    project_list: &'a [projects::Project],
    path: &std::path::Path,
) -> Option<&'a projects::Project> {
    project_list
        .iter()
        .filter(|p| path.starts_with(&p.dir))
        .max_by_key(|p| p.dir.components().count())
}

/// Re-runs stage-2 discovery for the given projects and rebuilds the library
/// union when any entry set changed. A project whose entries are unchanged
/// keeps its status untouched — the `.cpcache` write being reacted to may be
/// stage 3's own `-Spath` output, and downgrading `resolved` → `cached` on it
/// would misreport. Returns whether the union was rebuilt.
fn stage2_refresh(
    workspace_root: &std::path::Path,
    affected: &[projects::Project],
    all_projects: &[projects::Project],
    state_arc: &SharedState,
    index: &Index,
) -> bool {
    let mut changed = false;
    for p in affected {
        let entries: std::collections::HashSet<std::path::PathBuf> =
            stage2_discover(p).into_iter().collect();
        let mut state = state_arc.lock().unwrap();
        let entry = state
            .entry(p.rel_path.clone())
            .or_insert_with(|| ProjectState::empty(ClasspathStatus::Unresolved));
        if entry.entries != entries {
            entry.status = if entries.is_empty() {
                ClasspathStatus::Unresolved
            } else {
                ClasspathStatus::Cached
            };
            entry.entries = entries;
            changed = true;
        }
    }
    if changed {
        let mut state = state_arc.lock().unwrap();
        rebuild_libs(workspace_root, all_projects, &mut state, index);
    }
    changed
}

/// Stage 2 across the given projects: indexes every project's cached
/// classpath and records per-project entries + status. Returns whether
/// anything was indexed. Callers log completion and notify.
fn run_stage2_all(
    workspace_root: &std::path::Path,
    project_list: &[projects::Project],
    state_arc: &SharedState,
    index: &Index,
) -> bool {
    let mut any = false;
    for p in project_list {
        let (entries, extra) = stage2_index_project(workspace_root, p, index);
        any |= !entries.is_empty() || extra > 0;
        let status = if entries.is_empty() {
            ClasspathStatus::Unresolved
        } else {
            ClasspathStatus::Cached
        };
        state_arc.lock().unwrap().insert(
            p.rel_path.clone(),
            ProjectState {
                entries: entries.into_iter().collect(),
                status,
                extra_indexed: extra > 0,
            },
        );
    }
    any
}

/// Stage 3 for one project: runs its classpath command (serialized on the CLI
/// lock) and, on a changed entry set, rebuilds the library union. The config
/// is re-read under the lock so a run queued behind a slow one applies the
/// freshest enablement/command; the generation snapshot guards against a
/// config change happening *during* the run (disable does not take the lock).
#[allow(clippy::too_many_arguments)]
async fn run_stage3_project(
    workspace_root: &std::path::Path,
    rel_path: &str,
    index: &Index,
    client: &Client,
    projects_arc: &SharedProjects,
    state_arc: &SharedState,
    generation: &ConfigGeneration,
    cli_lock: &ClasspathCliLock,
) -> bool {
    use std::sync::atomic::Ordering;

    let _serial = cli_lock.lock().await;

    let project = projects_arc
        .lock()
        .unwrap()
        .iter()
        .find(|p| p.rel_path == rel_path)
        .cloned();
    let Some(project) = project else {
        return false;
    };
    if !project.classpath_enabled || !has_manifest(&project) {
        return false;
    }
    let Some(cmd) = project.classpath_cmd.clone() else {
        return false;
    };
    let gen = generation.load(Ordering::SeqCst);

    set_status(state_arc, rel_path, ClasspathStatus::Resolving);
    client.send_notification::<LibrariesChanged>(()).await;
    let msg = format!(
        "clj-pulse: resolving classpath via '{cmd}' in {rel_path} (may download dependencies)..."
    );
    tracing::info!("{}", msg);
    client.log_message(MessageType::INFO, msg).await;

    let result = classpath::resolve_via_cmd(&cmd, &project.dir, classpath::CMD_TIMEOUT).await;

    // Stale-result guard: apply nothing (entries, status, rebuild) if the
    // config changed while the command ran.
    let fresh = generation.load(Ordering::SeqCst) == gen
        && projects_arc.lock().unwrap().iter().any(|p| {
            p.rel_path == rel_path
                && p.classpath_enabled
                && p.classpath_cmd.as_deref() == Some(cmd.as_str())
        });
    if !fresh {
        tracing::info!(
            "stage-3 result for {} discarded (config changed mid-run)",
            rel_path
        );
        return false;
    }

    match result {
        Ok(entries) => {
            let set: std::collections::HashSet<std::path::PathBuf> =
                entries.iter().cloned().collect();
            let project_list = projects_arc.lock().unwrap().clone();
            {
                let mut state = state_arc.lock().unwrap();
                let changed = state
                    .get(rel_path)
                    .map(|s| s.entries != set)
                    .unwrap_or(true);
                let entry = state
                    .entry(rel_path.to_string())
                    .or_insert_with(|| ProjectState::empty(ClasspathStatus::Unresolved));
                entry.entries = set;
                entry.status = ClasspathStatus::Resolved;
                if changed {
                    rebuild_libs(workspace_root, &project_list, &mut state, index);
                }
            }
            let msg = format!(
                "clj-pulse: full classpath indexed ({} entries)",
                entries.len()
            );
            tracing::info!("{}", msg);
            client.log_message(MessageType::INFO, msg).await;
            client.send_notification::<LibrariesChanged>(()).await;
            true
        }
        Err(reason) => {
            // Keep stage-2 entries; only the status records the failure.
            set_status(state_arc, rel_path, ClasspathStatus::Error(reason.clone()));
            let msg = format!("clj-pulse: classpath resolution failed: {reason}");
            tracing::warn!("{}", msg);
            client.log_message(MessageType::WARNING, msg).await;
            client.send_notification::<LibrariesChanged>(()).await;
            false
        }
    }
}

/// Stage 3 across every enabled project with a command and a manifest.
/// Notifies progressively (per project) via [`run_stage3_project`].
async fn run_stage3_all(
    workspace_root: &std::path::Path,
    index: &Index,
    client: &Client,
    projects_arc: &SharedProjects,
    state_arc: &SharedState,
    generation: &ConfigGeneration,
    cli_lock: &ClasspathCliLock,
) -> bool {
    let candidates: Vec<String> = projects_arc
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.classpath_enabled && p.classpath_cmd.is_some() && has_manifest(p))
        .map(|p| p.rel_path.clone())
        .collect();
    let mut any_ok = false;
    for rel_path in candidates {
        any_ok |= run_stage3_project(
            workspace_root,
            &rel_path,
            index,
            client,
            projects_arc,
            state_arc,
            generation,
            cli_lock,
        )
        .await;
    }
    any_ok
}

/// Applies the difference between the previous and freshly resolved project
/// lists: indexes added projects (sources + stage 2) — or rescans the whole
/// source union when projects were removed, unless the caller already did —
/// reverts disabled / command-changed projects to stage-2 truth, notifies the
/// panel, and runs stage 3 for newly enabled (or command-changed) projects.
/// Returns the rel_paths stage 3 was run for. Call with the
/// [`ConfigApplyLock`] held.
#[allow(clippy::too_many_arguments)]
async fn apply_project_diff(
    root: &std::path::Path,
    old: &[projects::Project],
    resolved: &[projects::Project],
    pruned_any: bool,
    list_changed: bool,
    sources_rescanned: bool,
    index: &Index,
    client: &Client,
    documents: &DocumentStore,
    projects_arc: &SharedProjects,
    state_arc: &SharedState,
    generation: &ConfigGeneration,
    cli_lock: &ClasspathCliLock,
) -> Vec<String> {
    // Diff old vs new resolved config per project.
    let mut stage3_runs: Vec<String> = Vec::new();
    let mut force_revert: Vec<String> = Vec::new();
    let mut added: Vec<projects::Project> = Vec::new();
    let mut kind_changed = false;
    for p in resolved {
        let old_p = old.iter().find(|o| o.rel_path == p.rel_path);
        if old_p.is_none() {
            added.push(p.clone());
        }
        kind_changed |= old_p.is_some_and(|o| o.kind != p.kind);
        let was_active = old_p.is_some_and(|o| o.classpath_enabled && o.classpath_cmd.is_some());
        let cmd_changed = old_p.is_none_or(|o| o.classpath_cmd != p.classpath_cmd);
        if p.classpath_enabled && p.classpath_cmd.is_some() && (!was_active || cmd_changed) {
            stage3_runs.push(p.rel_path.clone());
            // A changed command reverts to stage-2 truth first: if the new
            // command fails, the old command's entries must not stay indexed
            // behind the error status.
            if was_active && cmd_changed {
                force_revert.push(p.rel_path.clone());
            }
        }
    }
    let removed_any = old
        .iter()
        .any(|o| !resolved.iter().any(|p| p.rel_path == o.rel_path));

    if removed_any && !sources_rescanned {
        // A removed project's sources must leave the index: rescan the whole
        // union (the merge prunes files no longer covered).
        if let Err(e) = rescan_all_sources(resolved, index, documents) {
            tracing::error!("project re-index failed: {}", e);
        }
    } else if !added.is_empty() && !sources_rescanned {
        // A project newly added by config (a gitignored dir listed live):
        // index its sources so the panel entry isn't an empty shell.
        let scan_roots: Vec<scanner::ScanRoot> = added
            .iter()
            .flat_map(|p| {
                let dir = p.dir.clone();
                config::source_paths(&p.dir)
                    .into_iter()
                    .map(move |path| scanner::ScanRoot {
                        project_dir: dir.clone(),
                        path,
                    })
            })
            .collect();
        match scanner::build_index_scoped(&scan_roots, &index.extract_config()) {
            Ok(new_index) => {
                Backend::warn_ns_collisions(index, &new_index);
                // Keep every existing file: this scan covers only the added
                // projects, not the whole workspace.
                let keep: std::collections::HashSet<std::path::PathBuf> =
                    index.occurrences.iter().map(|e| e.key().clone()).collect();
                index.merge_project_from(new_index, &keep);
            }
            Err(e) => tracing::error!("added-project index failed: {}", e),
        }
    }
    if !added.is_empty() {
        run_stage2_all(root, &added, state_arc, index);
    }

    // Newly disabled projects revert to stage-2 truth (their stage-2
    // libraries stay indexed — disabling only gates stage 3).
    let rebuilt = reconcile_projects(root, resolved, state_arc, index, pruned_any, &force_revert);
    if kind_changed && !rebuilt {
        // Per-kind indexing differs even with identical entries (lgx dirs vs
        // classpath, let-go core): a kind flip forces the union rebuild that
        // the entry-set comparison would skip.
        let mut state = state_arc.lock().unwrap();
        rebuild_libs(root, resolved, &mut state, index);
    }
    if rebuilt || kind_changed || list_changed || !added.is_empty() {
        client.send_notification::<LibrariesChanged>(()).await;
    }

    for rel_path in &stage3_runs {
        run_stage3_project(
            root,
            rel_path,
            index,
            client,
            projects_arc,
            state_arc,
            generation,
            cli_lock,
        )
        .await;
    }
    stage3_runs
}

#[derive(serde::Deserialize)]
pub(crate) struct TextDocumentContentParams {
    uri: String,
}

#[derive(serde::Serialize)]
pub(crate) struct TextDocumentContentResult {
    text: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct IgnoredFormsParams {
    uri: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct LibraryEntriesParams {
    path: String,
}

/// clj-pulse custom notification, pushed whenever library (re)indexing
/// completes — including the zero-entries case, so the External Libraries panel
/// refreshes (and clears when deps disappear) without polling. Zero params.
enum LibrariesChanged {}

impl tower_lsp::lsp_types::notification::Notification for LibrariesChanged {
    type Params = ();
    const METHOD: &'static str = "clojurePulse/librariesChanged";
}

pub struct Backend {
    pub client: Client,
    pub index: Arc<Index>,
    pub documents: Arc<DocumentStore>,
    /// The workspace root (log dir, config location, watched-file routing).
    root: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// The resolved project list (root + detected/configured subprojects).
    projects: SharedProjects,
    /// Per-project lib entries + status, keyed by `rel_path`.
    project_state: SharedState,
    /// The editor config layer (`initializationOptions` /
    /// `didChangeConfiguration`), kept so file-config reloads re-merge it.
    editor_config: SharedEditorConfig,
    /// See [`ConfigGeneration`].
    config_generation: ConfigGeneration,
    /// See [`ClasspathCliLock`].
    classpath_cli_lock: ClasspathCliLock,
    /// See [`ConfigApplyLock`].
    config_apply_lock: ConfigApplyLock,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            index: Arc::new(Index::new_with_core()),
            documents: Arc::new(DocumentStore::new()),
            root: std::sync::Mutex::new(None),
            projects: SharedProjects::default(),
            project_state: SharedState::default(),
            editor_config: SharedEditorConfig::default(),
            config_generation: ConfigGeneration::default(),
            classpath_cli_lock: ClasspathCliLock::default(),
            config_apply_lock: ConfigApplyLock::default(),
        }
    }

    /// Paths of currently open documents — kept indexed even when they live
    /// outside deps.edn `:paths`.
    fn open_paths(documents: &DocumentStore) -> std::collections::HashSet<std::path::PathBuf> {
        documents
            .open_uris()
            .into_iter()
            .filter_map(|uri| uri.to_file_path().ok())
            .collect()
    }

    /// Warns about namespaces the new scan defines in a *different* file than
    /// the existing index (last one wins on merge). Intra-scan duplicates are
    /// warned by the scanner itself; lib-owned namespaces are skipped — a
    /// project ns shadowing a library ns is normal and resolved by precedence.
    fn warn_ns_collisions(index: &Index, new_index: &Index) {
        for entry in new_index.namespaces.iter() {
            if let Some(existing) = index.namespaces.get(entry.key()) {
                if existing.file != entry.value().file && index.is_project_path(&existing.file) {
                    tracing::warn!(
                        "namespace {} defined in both {} and {}; last one wins",
                        entry.key(),
                        existing.file.display(),
                        entry.value().file.display()
                    );
                }
            }
        }
    }

    /// Reads the text of a `jar:` URI entry. Shared by the LSP 3.17
    /// `workspace/textDocumentContent` method and clojure-lsp's
    /// `clojure/dependencyContents`.
    fn read_jar_uri(uri: &str) -> tower_lsp::jsonrpc::Result<String> {
        let (jar_path, entry_path) = jar_content::parse_jar_uri(uri).map_err(|e| {
            tracing::warn!("jar content: bad URI {}: {}", uri, e);
            tower_lsp::jsonrpc::Error::invalid_params(e.to_string())
        })?;

        if !jar_path.exists() {
            return Err(tower_lsp::jsonrpc::Error {
                code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32801),
                message: std::borrow::Cow::Owned(format!("JAR not found: {}", jar_path.display())),
                data: None,
            });
        }

        jar_content::extract_content(&jar_path, &entry_path).map_err(|e| {
            tracing::warn!("jar content: failed to extract {}: {}", uri, e);
            let msg = e.to_string();
            if msg.contains("not found") {
                tower_lsp::jsonrpc::Error::invalid_params(msg)
            } else {
                tower_lsp::jsonrpc::Error::internal_error()
            }
        })
    }

    /// LSP 3.17 `workspace/textDocumentContent` — used by clients that support
    /// the standardized content-provider method.
    pub async fn text_document_content(
        &self,
        params: TextDocumentContentParams,
    ) -> tower_lsp::jsonrpc::Result<TextDocumentContentResult> {
        Self::read_jar_uri(&params.uri).map(|text| TextDocumentContentResult { text })
    }

    /// clojure-lsp-compatible `clojure/dependencyContents`: returns the raw
    /// content string for a `jar:` URI. Calva (and other clojure-lsp clients)
    /// register a `jar`-scheme content provider that calls this; without it they
    /// cannot open any library or clojure.core navigation target, since plain
    /// vscode-languageclient does not support `workspace/textDocumentContent`.
    pub async fn dependency_contents(
        &self,
        params: TextDocumentContentParams,
    ) -> tower_lsp::jsonrpc::Result<String> {
        Self::read_jar_uri(&params.uri)
    }

    /// clj-pulse custom `clojurePulse/ignoredForms`: the whole-form ranges of
    /// `#_` discard forms and `(comment …)` blocks in the live buffer, for the
    /// editor to dim. An unparseable or unopened URI yields an empty list. Never
    /// errors — dimming is best-effort.
    pub async fn ignored_forms(
        &self,
        params: IgnoredFormsParams,
    ) -> tower_lsp::jsonrpc::Result<Vec<Range>> {
        let Ok(uri) = Url::parse(&params.uri) else {
            return Ok(Vec::new());
        };
        let Some(text) = self.documents.text(&uri) else {
            return Ok(Vec::new());
        };
        Ok(handlers::ignored_forms::ignored_form_ranges(&text))
    }

    /// clj-pulse custom `clojurePulse/externalLibraries`: the resolved external
    /// libraries for the panel — the flat deduped union across all projects
    /// (older editors keep working against the multi-project server). No
    /// project list yet → empty list, never an error.
    pub async fn external_libraries(
        &self,
        _params: Option<serde_json::Value>,
    ) -> tower_lsp::jsonrpc::Result<Vec<libraries::Library>> {
        let project_list = self.projects.lock().unwrap().clone();
        if project_list.is_empty() {
            return Ok(Vec::new());
        }
        let entries = lib_union(&self.project_state.lock().unwrap());
        // source_paths reads manifests from disk; keep it off the LSP executor
        // so it can't stall hover/completion/definition.
        tokio::task::spawn_blocking(move || {
            let own_paths: Vec<std::path::PathBuf> = project_list
                .iter()
                .flat_map(|p| config::source_paths(&p.dir))
                .collect();
            let project_dirs: Vec<std::path::PathBuf> =
                project_list.iter().map(|p| p.dir.clone()).collect();
            libraries::from_entries(&own_paths, &project_dirs, &entries)
        })
        .await
        .map_err(|e| {
            tracing::error!("externalLibraries task panicked: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    /// clj-pulse custom `clojurePulse/projects`: the grouped per-project view —
    /// each project's kind, classpath resolution config + status, and its own
    /// libraries. Root first, then rel_path-sorted (the resolve order). No
    /// project list yet → empty list, never an error.
    pub async fn projects_info(
        &self,
        _params: Option<serde_json::Value>,
    ) -> tower_lsp::jsonrpc::Result<Vec<serde_json::Value>> {
        let project_list = self.projects.lock().unwrap().clone();
        let state = self.project_state.lock().unwrap().clone();
        // source_paths reads manifests from disk; keep it off the LSP executor.
        tokio::task::spawn_blocking(move || {
            // Every project's dir, not just the current one: the root's
            // resolved classpath lists subproject source dirs too.
            let project_dirs: Vec<std::path::PathBuf> =
                project_list.iter().map(|p| p.dir.clone()).collect();
            project_list
                .iter()
                .map(|p| {
                    let project_state = state.get(&p.rel_path);
                    let entries: Vec<std::path::PathBuf> = project_state
                        .map(|s| s.entries.iter().cloned().collect())
                        .unwrap_or_default();
                    let own_paths = config::source_paths(&p.dir);
                    let libs = libraries::from_entries(&own_paths, &project_dirs, &entries);

                    let kind = match p.kind {
                        projects::ProjectKindTag::Deps => "deps",
                        projects::ProjectKindTag::Lein => "lein",
                        projects::ProjectKindTag::Lgx => "lgx",
                    };
                    let status =
                        project_state
                            .map(|s| &s.status)
                            .unwrap_or(if p.classpath_enabled {
                                &ClasspathStatus::Unresolved
                            } else {
                                &ClasspathStatus::Disabled
                            });
                    let mut classpath = serde_json::json!({
                        "enabled": p.classpath_enabled,
                        "status": match status {
                            ClasspathStatus::Disabled => "disabled",
                            ClasspathStatus::Cached => "cached",
                            ClasspathStatus::Resolving => "resolving",
                            ClasspathStatus::Resolved => "resolved",
                            ClasspathStatus::Unresolved => "unresolved",
                            ClasspathStatus::Error(_) => "error",
                        },
                    });
                    if let Some(cmd) = &p.classpath_cmd {
                        classpath["cmd"] = serde_json::json!(cmd);
                    }
                    if let ClasspathStatus::Error(message) = status {
                        classpath["message"] = serde_json::json!(message);
                    }
                    serde_json::json!({
                        "path": p.rel_path,
                        "kind": kind,
                        "classpath": classpath,
                        "libraries": libs,
                    })
                })
                .collect()
        })
        .await
        .map_err(|e| {
            tracing::error!("projects task panicked: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    /// clj-pulse custom `clojurePulse/libraryEntries`: the file entries of a jar
    /// library, for the panel to fold into a browsable tree. Rejects anything
    /// that is not an existing `.jar` file with `invalid_params`.
    pub async fn library_entries(
        &self,
        params: LibraryEntriesParams,
    ) -> tower_lsp::jsonrpc::Result<Vec<String>> {
        let path = std::path::PathBuf::from(&params.path);
        if path.extension().and_then(|e| e.to_str()) != Some("jar") || !path.is_file() {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "not an existing .jar file: {}",
                params.path
            )));
        }
        // Reading a jar's central directory can be slow for large or
        // network-mounted archives; run it off the LSP executor.
        tokio::task::spawn_blocking(move || jar_content::list_entries(&path))
            .await
            .map_err(|e| {
                tracing::error!("libraryEntries task panicked: {}", e);
                tower_lsp::jsonrpc::Error::internal_error()
            })?
            .map_err(|e| {
                tracing::warn!("libraryEntries: failed to list {}: {}", params.path, e);
                tower_lsp::jsonrpc::Error::internal_error()
            })
    }

    /// Computes unresolved-namespace diagnostics from the live buffer and
    /// publishes them for `uri`.
    async fn lint_and_publish(&self, uri: Url, version: i32) {
        let Some(text) = self.documents.text(&uri) else {
            return;
        };
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        let diags = crate::diagnostics::compute(&text, &path);
        self.client
            .publish_diagnostics(uri, diags, Some(version))
            .await;
    }
}

/// Idle time after the last edit before re-linting a changed document.
const DIAGNOSTIC_DEBOUNCE_MS: u64 = 300;

/// The project root from an `initialize` request. Prefers the modern
/// `workspaceFolders` over the deprecated `rootUri`: newer clients (Zed and
/// others) send only `workspaceFolders`, and reading just `rootUri` left the
/// project unindexed — so same-file navigation worked but cross-file silently
/// failed.
fn initialize_root(params: &InitializeParams) -> Option<std::path::PathBuf> {
    params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .and_then(|folder| folder.uri.to_file_path().ok())
        .or_else(|| {
            params
                .root_uri
                .as_ref()
                .and_then(|uri| uri.to_file_path().ok())
        })
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(root_path) = initialize_root(&params) {
            {
                *self.root.lock().unwrap() = Some(root_path.clone());
                // The editor config layer arrives in initializationOptions as
                // the bare `{"projects": [...]}` object. Anything else (Calva's
                // clojure-lsp settings) parses to no entries.
                let editor_entries = params
                    .initialization_options
                    .as_ref()
                    .map(projects::parse_json)
                    .unwrap_or_default();
                *self.editor_config.lock().unwrap() = editor_entries.clone();

                let index = self.index.clone();
                let client = self.client.clone();
                let documents = self.documents.clone();
                let projects_arc = self.projects.clone();
                let state_arc = self.project_state.clone();
                let generation = self.config_generation.clone();
                let cli_lock = self.classpath_cli_lock.clone();
                let apply_lock = self.config_apply_lock.clone();
                tokio::spawn(async move {
                    let start = std::time::Instant::now();

                    // Resolve the project list: detection + config layers.
                    let detected = projects::detect(&root_path);
                    let file_entries =
                        std::fs::read_to_string(root_path.join(".clj-pulse").join("config.edn"))
                            .map(|src| projects::parse_edn(&src))
                            .unwrap_or_default();
                    let resolved =
                        projects::resolve(&root_path, &detected, &file_entries, &editor_entries);
                    tracing::info!(
                        "workspace root: {}, projects: {:?}",
                        root_path.display(),
                        resolved.iter().map(|p| &p.rel_path).collect::<Vec<_>>()
                    );
                    *projects_arc.lock().unwrap() = resolved.clone();
                    {
                        let mut state = state_arc.lock().unwrap();
                        for p in &resolved {
                            state.insert(
                                p.rel_path.clone(),
                                ProjectState::empty(if p.classpath_enabled {
                                    ClasspathStatus::Unresolved
                                } else {
                                    ClasspathStatus::Disabled
                                }),
                            );
                        }
                    }
                    // The project list is now available: tell the panel even
                    // when no libraries will ever be indexed (all disabled /
                    // empty classpaths), so it doesn't stay permanently empty.
                    client.send_notification::<LibrariesChanged>(()).await;

                    // Background task: index libraries from each project's
                    // classpath, concurrent with the source scan below.
                    // Graduated: stage 2 indexes whatever `.cpcache` already
                    // holds (instant), then stage 3 resolves the authoritative
                    // classpath per enabled project and re-indexes on change.
                    {
                        let index = index.clone();
                        let client = client.clone();
                        let projects_arc = projects_arc.clone();
                        let state_arc = state_arc.clone();
                        let generation = generation.clone();
                        let cli_lock = cli_lock.clone();
                        let apply_lock = apply_lock.clone();
                        let root = root_path.clone();
                        let resolved = resolved.clone();
                        tokio::spawn(async move {
                            // Serialize with config-application tasks (see
                            // `ConfigApplyLock`).
                            let _serial = apply_lock.lock().await;
                            let stage2_ok = run_stage2_all(&root, &resolved, &state_arc, &index);
                            if stage2_ok {
                                let msg = format!(
                                    "clj-pulse: library indexing complete ({} total symbols)",
                                    index.symbols.len()
                                );
                                tracing::info!("{}", msg);
                                client.log_message(MessageType::INFO, msg).await;
                                client.send_notification::<LibrariesChanged>(()).await;
                            }

                            let stage3_ok = run_stage3_all(
                                &root,
                                &index,
                                &client,
                                &projects_arc,
                                &state_arc,
                                &generation,
                                &cli_lock,
                            )
                            .await;

                            if !stage2_ok && !stage3_ok {
                                let msg = match config::project_kind(&root) {
                                    config::ProjectKind::LetGo => {
                                        "clj-pulse: no lgx deps resolved (no ~/.lgx/gitlibs, or \
                                         deps not fetched — run `lgx run`/`lgx build` once) — \
                                         library symbols will not be indexed."
                                    }
                                    config::ProjectKind::Clojure => {
                                        "clj-pulse: no classpath found (no .cpcache/ in project \
                                         root?) — library symbols will not be indexed. Run \
                                         `clojure -A:dev:test -Spath` or start a REPL once to \
                                         generate it."
                                    }
                                };
                                tracing::warn!("{}", msg);
                                client.log_message(MessageType::WARNING, msg).await;
                                client.send_notification::<LibrariesChanged>(()).await;
                            }
                        });
                    }

                    // Stage 1: one scan over the union of every project's own
                    // source paths (first project wins a shared path — the
                    // root is first).
                    index.set_extract_config(settings::load(&root_path));
                    match rescan_all_sources(&resolved, &index, &documents) {
                        Ok((sym_count, ns_count)) => {
                            let elapsed = start.elapsed();
                            let msg = format!(
                                "Indexed {} symbols in {} namespaces in {:?}",
                                sym_count, ns_count, elapsed
                            );
                            tracing::info!("{}", msg);
                            client.log_message(MessageType::INFO, msg).await;
                        }
                        Err(e) => {
                            tracing::error!("failed to build index: {}", e);
                            client
                                .log_message(
                                    MessageType::ERROR,
                                    format!("clj-pulse: index build failed: {}", e),
                                )
                                .await;
                        }
                    }
                });

                // Background task: discover and index the JDK's bundled Java
                // source (`src.zip`) for built-in Java navigation/completion.
                let index_jdk = self.index.clone();
                let client_jdk = self.client.clone();
                tokio::spawn(async move {
                    match crate::index::jdk::JdkIndex::discover() {
                        Some(jdk) => {
                            let count = jdk.class_count();
                            index_jdk.set_jdk(jdk);
                            let msg = format!("JDK source indexed: {} classes", count);
                            tracing::info!("{}", msg);
                            client_jdk.log_message(MessageType::INFO, msg).await;
                        }
                        None => {
                            tracing::debug!(
                                "no JDK source (src.zip) found — built-in Java navigation disabled"
                            );
                        }
                    }
                });
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions::default()),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), " ".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".to_string(),
                    more_trigger_character: None,
                }),
                experimental: Some(serde_json::json!({
                    "textDocumentContentProvider": { "schemes": ["jar"] }
                })),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "clj-pulse".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("clj-pulse initialized");

        // Watch source files so git pulls / branch switches keep the index
        // fresh without editor saves. Clients without dynamic registration
        // simply reject this; everything else still works.
        let watchers = vec![
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.{clj,cljs,cljc,lg}".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                // All EDN files: manifests (deps.edn / lgx.edn → classpath) and
                // Integrant system configs (→ keyword occurrences). The handler
                // routes each by name/content.
                glob_pattern: GlobPattern::String("**/*.edn".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.cpcache/*.cp".to_string()),
                kind: None,
            },
            // clj-pulse and clj-kondo config: reload `:lint-as` on change. Named
            // explicitly so clients that skip dotfiles under `**/*.edn` still
            // watch them.
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.clj-kondo/config.edn".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.clj-pulse/config.edn".to_string()),
                kind: None,
            },
        ];
        let registration = Registration {
            id: "clj-pulse-watched-files".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers,
            })
            .ok(),
        };
        if let Err(e) = self.client.register_capability(vec![registration]).await {
            tracing::info!("watched-files registration not supported: {}", e);
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // The settings envelope wraps the bare config object in its section:
        // {"clojurePulse": {"projects": [...]}}. No section → not for us.
        let Some(section) = params.settings.get("clojurePulse") else {
            return;
        };
        let entries = projects::parse_json(section);
        *self.editor_config.lock().unwrap() = entries;
        let Some(root) = self.root.lock().unwrap().clone() else {
            return;
        };

        let index = self.index.clone();
        let client = self.client.clone();
        let documents = self.documents.clone();
        let projects_arc = self.projects.clone();
        let state_arc = self.project_state.clone();
        let editor_config = self.editor_config.clone();
        let generation = self.config_generation.clone();
        let cli_lock = self.classpath_cli_lock.clone();
        let apply_lock = self.config_apply_lock.clone();
        tokio::spawn(async move {
            // Serialize whole applications: a slower, staler task must not
            // finish after — and overwrite — a newer one. (The editor layer
            // was stored synchronously above, so even the older queued task
            // resolves against the newest config.)
            let _serial = apply_lock.lock().await;

            let old = projects_arc.lock().unwrap().clone();
            let (resolved, pruned_any, list_changed) = refresh_projects(
                &root,
                &projects_arc,
                &editor_config,
                &state_arc,
                &generation,
            );

            apply_project_diff(
                &root,
                &old,
                &resolved,
                pruned_any,
                list_changed,
                false,
                &index,
                &client,
                &documents,
                &projects_arc,
                &state_arc,
                &generation,
                &cli_lock,
            )
            .await;
        });
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("clj-pulse shutting down");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        // Files outside deps.edn :paths (dev/, scratch files, test dirs that
        // only appear in alias :extra-paths) are not indexed at startup;
        // index them on open so navigation from them works.
        if let Ok(path) = uri.to_file_path() {
            if config::is_clojure_source(&path) && self.index.file_ns(&path).is_none() {
                match extractor::extract_full_with(&text, &path, &self.index.extract_config()) {
                    Ok((meta, symbols, occurrences)) => {
                        tracing::info!("indexed opened file {}", path.display());
                        self.index.insert_file(meta, symbols, occurrences);
                    }
                    Err(e) => {
                        tracing::debug!("failed to index opened {}: {}", path.display(), e)
                    }
                }
            } else if extractor::is_integrant_edn(&path, &text)
                && self.index.file_ns(&path).is_none()
            {
                // Integrant config opened from outside the scanned paths.
                tracing::info!("indexed opened EDN config {}", path.display());
                self.index
                    .insert_edn_file(path.clone(), extractor::extract_edn(&text));
            }
        }

        self.documents.open(uri.clone(), text);
        self.documents.set_version(&uri, version);
        self.lint_and_publish(uri, version).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.close(&uri);
        // Clear diagnostics for the closed document.
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        // Only re-index Clojure source files; saving an EDN config file
        // (deps.edn / lgx.edn) must not insert a junk empty namespace.
        if config::is_clojure_source(&path) {
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    self.index.remove_file(&path);
                    match extractor::extract_full_with(&source, &path, &self.index.extract_config())
                    {
                        Ok((meta, symbols, occurrences)) => {
                            let count = symbols.len();
                            self.index.insert_file(meta, symbols, occurrences);
                            tracing::info!("re-indexed {} ({} symbols)", path.display(), count);
                        }
                        Err(e) => tracing::warn!("failed to re-index {}: {}", path.display(), e),
                    }
                }
                Err(e) => tracing::warn!("failed to read {}: {}", path.display(), e),
            }
        } else if config::is_edn(&path) {
            // Re-index Integrant EDN configs. The file is always removed first
            // (so an edit that drops `#ig/ref` de-indexes it) and re-inserted
            // only when it still looks like an Integrant system.
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    self.index.remove_file(&path);
                    if extractor::is_integrant_edn(&path, &source) {
                        self.index
                            .insert_edn_file(path.clone(), extractor::extract_edn(&source));
                        tracing::info!("re-indexed EDN config {}", path.display());
                    }
                }
                Err(e) => tracing::warn!("failed to read {}: {}", path.display(), e),
            }
        }

        let version = self.documents.current_version(&uri).unwrap_or(0);
        self.lint_and_publish(uri, version).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // Manifest / `.cpcache` events are routed to their owning project (by
        // longest project-dir prefix) after the project list is re-resolved.
        let mut manifest_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut cpcache_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut config_changed = false;
        // `.clj-pulse/config.edn` specifically: only it carries `:projects`,
        // so only it triggers a stage-3 re-resolution (`.clj-kondo` does not).
        let mut pulse_config_changed = false;
        for event in params.changes {
            let Ok(path) = event.uri.to_file_path() else {
                continue;
            };

            // deps.edn / lgx.edn / project.clj affect both the classpath/deps
            // and the owning project's own :paths; .cpcache only the classpath.
            // A created or deleted manifest also changes the project *list* —
            // covered by the unconditional re-detection below.
            let manifest = path
                .file_name()
                .map(|n| n == "deps.edn" || n == "lgx.edn" || n == "project.clj")
                .unwrap_or(false);
            if manifest {
                manifest_paths.push(path);
                continue;
            }
            if path.components().any(|c| c.as_os_str() == ".cpcache") {
                cpcache_paths.push(path);
                continue;
            }

            // clj-pulse / clj-kondo config: reload `:lint-as` and re-index the
            // project. Intercept before the EDN branch below, since `config.edn`
            // is itself `.edn` but is not an Integrant config.
            let config_dir = path
                .file_name()
                .map(|n| n == "config.edn")
                .unwrap_or(false)
                .then(|| path.parent().and_then(|p| p.file_name()))
                .flatten();
            if config_dir.is_some_and(|d| d == ".clj-kondo" || d == ".clj-pulse") {
                config_changed = true;
                pulse_config_changed |= config_dir.is_some_and(|d| d == ".clj-pulse");
                continue;
            }

            // Integrant EDN configs: keep keyword occurrences fresh on external
            // edits (git pull / branch switch). Remove first so dropping
            // `#ig/ref` — or the file — de-indexes it; re-insert only when it
            // still looks like an Integrant system. Manifests (deps.edn/lgx.edn)
            // already `continue`d above as classpath changes.
            if config::is_edn(&path) {
                self.index.remove_file(&path);
                if event.typ != FileChangeType::DELETED {
                    if let Ok(source) = std::fs::read_to_string(&path) {
                        if extractor::is_integrant_edn(&path, &source) {
                            self.index
                                .insert_edn_file(path.clone(), extractor::extract_edn(&source));
                            tracing::info!("watched re-index EDN config: {}", path.display());
                        }
                    }
                }
                continue;
            }

            if !config::is_clojure_source(&path) {
                continue;
            }

            if event.typ == FileChangeType::DELETED {
                tracing::info!("watched delete: {}", path.display());
                self.index.remove_file(&path);
                continue;
            }

            // CREATED or CHANGED
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    self.index.remove_file(&path);
                    match extractor::extract_full_with(&source, &path, &self.index.extract_config())
                    {
                        Ok((meta, symbols, occurrences)) => {
                            tracing::info!("watched re-index: {}", path.display());
                            self.index.insert_file(meta, symbols, occurrences);
                        }
                        Err(e) => {
                            tracing::warn!("failed to extract {}: {}", path.display(), e)
                        }
                    }
                }
                Err(e) => tracing::warn!("failed to read {}: {}", path.display(), e),
            }
        }

        let classpath_changed = !manifest_paths.is_empty() || !cpcache_paths.is_empty();
        let source_paths_changed = !manifest_paths.is_empty();
        if classpath_changed || config_changed {
            let root = self.root.lock().unwrap().clone();
            if let Some(root) = root {
                let index = self.index.clone();
                let client = self.client.clone();
                let documents = self.documents.clone();
                let projects_arc = self.projects.clone();
                let state_arc = self.project_state.clone();
                let editor_config = self.editor_config.clone();
                let generation = self.config_generation.clone();
                let cli_lock = self.classpath_cli_lock.clone();
                let apply_lock = self.config_apply_lock.clone();
                tokio::spawn(async move {
                    // Serialize with other config-application tasks (see
                    // `ConfigApplyLock`).
                    let _serial = apply_lock.lock().await;

                    // A config change reloads `:lint-as` before re-indexing, so
                    // the rebuild extracts project files with the new mapping.
                    if config_changed {
                        index.set_extract_config(settings::load(&root));
                    }

                    // A manifest or config change can add/remove projects or
                    // retoggle their classpath resolution: re-resolve the list
                    // (this also bumps the stage-3 stale-result generation).
                    let old = projects_arc.lock().unwrap().clone();
                    let (resolved, pruned_any, list_changed) = refresh_projects(
                        &root,
                        &projects_arc,
                        &editor_config,
                        &state_arc,
                        &generation,
                    );

                    // Rebuild project sources when :paths changed, the project
                    // list changed (created/deleted manifest), or the config
                    // changed (lint-as affects how every project file extracts).
                    let sources_rescanned = source_paths_changed || config_changed || list_changed;
                    if sources_rescanned {
                        if let Err(e) = rescan_all_sources(&resolved, &index, &documents) {
                            tracing::error!("project re-index failed: {}", e);
                        }
                    }

                    // Open buffers outside :paths were indexed on didOpen with
                    // the previous config; re-extract each open project buffer so
                    // the reload reaches them too. Jar/dir-lib files have no
                    // occurrences entry, so `is_project_path` skips them.
                    if config_changed {
                        let cfg = index.extract_config();
                        for uri in documents.open_uris() {
                            let Ok(path) = uri.to_file_path() else {
                                continue;
                            };
                            if !config::is_clojure_source(&path) || !index.is_project_path(&path) {
                                continue;
                            }
                            if let Some(text) = documents.text(&uri) {
                                if let Ok((meta, symbols, occ)) =
                                    extractor::extract_full_with(&text, &path, &cfg)
                                {
                                    index.remove_file(&path);
                                    index.insert_file(meta, symbols, occ);
                                }
                            }
                        }
                    }

                    // Log the reload before the (optional) library branch, whose
                    // early return could otherwise skip it.
                    if config_changed {
                        client
                            .log_message(MessageType::INFO, "clj-pulse: config reloaded")
                            .await;
                    }

                    // Project list / config toggles: index added projects,
                    // prune removed ones, revert disabled or command-changed
                    // ones, and run stage 3 for newly enabled ones.
                    let stage3_ran = if pulse_config_changed || list_changed {
                        apply_project_diff(
                            &root,
                            &old,
                            &resolved,
                            pruned_any,
                            list_changed,
                            sources_rescanned,
                            &index,
                            &client,
                            &documents,
                            &projects_arc,
                            &state_arc,
                            &generation,
                            &cli_lock,
                        )
                        .await
                    } else {
                        Vec::new()
                    };

                    // Routed stage 2: re-discover only the projects owning the
                    // changed manifest / `.cpcache` paths; rebuild the union
                    // when any entry set changed.
                    let affected: Vec<projects::Project> = {
                        let mut seen = std::collections::HashSet::new();
                        manifest_paths
                            .iter()
                            .filter_map(|path| manifest_owner(&resolved, path))
                            .chain(
                                cpcache_paths
                                    .iter()
                                    .filter_map(|path| owning_project(&resolved, path)),
                            )
                            .filter(|p| seen.insert(p.rel_path.clone()))
                            .cloned()
                            .collect()
                    };
                    if classpath_changed {
                        let stage2_changed =
                            stage2_refresh(&root, &affected, &resolved, &state_arc, &index);
                        if stage2_changed {
                            let msg = "clj-pulse: library re-indexing complete";
                            tracing::info!("{}", msg);
                            client.log_message(MessageType::INFO, msg).await;
                        }
                        // Notify either way — on nothing resolved the panel
                        // must clear.
                        client.send_notification::<LibrariesChanged>(()).await;
                    }

                    // A changed manifest re-runs stage 3 for its (enabled)
                    // owning project — unless the diff above already did.
                    let mut ran: std::collections::HashSet<String> =
                        stage3_ran.into_iter().collect();
                    for path in &manifest_paths {
                        let Some(rel_path) =
                            manifest_owner(&resolved, path).map(|p| p.rel_path.clone())
                        else {
                            continue;
                        };
                        if !ran.insert(rel_path.clone()) {
                            continue;
                        }
                        run_stage3_project(
                            &root,
                            &rel_path,
                            &index,
                            &client,
                            &projects_arc,
                            &state_arc,
                            &generation,
                            &cli_lock,
                        )
                        .await;
                    }
                });
            }
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        if let Err(e) = self.documents.apply_changes(&uri, params.content_changes) {
            tracing::warn!("failed to apply changes to {}: {}", uri, e);
            return;
        }
        self.documents.set_version(&uri, version);

        // Debounced re-lint: only the latest edit (matching version) survives
        // the sleep, so bursts of keystrokes collapse to one diagnostic pass.
        let documents = self.documents.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(DIAGNOSTIC_DEBOUNCE_MS)).await;
            if documents.current_version(&uri) != Some(version) {
                return;
            }
            let Some(text) = documents.text(&uri) else {
                return;
            };
            let Ok(path) = uri.to_file_path() else {
                return;
            };
            let diags = crate::diagnostics::compute(&text, &path);
            client.publish_diagnostics(uri, diags, Some(version)).await;
        });
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        handlers::definition::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("definition error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        handlers::completion::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("completion error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        handlers::hover::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("hover error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        handlers::signature::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("signature help error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        // Rename errors are user-facing (invalid name, library symbol, …)
        handlers::references::rename(&self.index, &self.documents, params)
            .map_err(|e| tower_lsp::jsonrpc::Error::invalid_params(e.to_string()))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        handlers::references::references(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("references error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        handlers::symbols::document_symbols(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("document symbol error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        Ok(Some(handlers::symbols::workspace_symbols(
            &self.index,
            &params.query,
        )))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        handlers::code_action::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("code action error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        handlers::indent::on_type_formatting(&self.documents, params).map_err(|e| {
            tracing::error!("on type formatting error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }
}
