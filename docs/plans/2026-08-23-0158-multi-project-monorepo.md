# Multi-Project (Monorepo) Support Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Index a monorepo's subprojects (each with its own `deps.edn`/`project.clj`/`lgx.edn`) in one server: sources and `.cpcache` for all of them automatically, with a per-project opt-in for JVM classpath resolution via a verbatim shell command.

**Tech Stack:** Rust, tower-lsp, tokio, ignore (walker), edn-format, serde.

---

## Design

### Problem

The server takes exactly one root (`initialize_root`, `src/server.rs`) and anchors every manifest lookup at `root.join(...)`. A monorepo (root `deps.edn` plus subprojects like `apps/backend`, `apps/worker`, `libs/common` included via `:local/root`) gets no navigation into subprojects: their sources are outside the root's `:paths`, and classpath dir entries under the root are deliberately skipped by the library scanner (`src/index/scanner.rs`, `starts_with(&root)` filter).

### Approach

One server, one `Index`, N projects. Per project there are three stages with very different costs, and only the last is gated:

1. **Source scan** (cheap, always on, all projects): parse each project's own `:paths` and tree-sitter-scan them into the shared project index.
2. **`.cpcache` read** (cheap, always on, all projects): each subproject someone has ever run `clojure` in contributes a resolved classpath with no JVM.
3. **Classpath command** (expensive, opt-in per project): run a verbatim shell command (`clojure -A:dev:test -Spath` by default) with `cwd` = the project dir. Enabled by default only for the workspace root project.

Subprojects are **auto-detected** (manifest glob, gitignore-respecting, depth-capped). Configuration arrives through three channels carrying the same schema — `.clj-pulse/config.edn`, LSP `initializationOptions`, and `workspace/didChangeConfiguration` — merged per project path, editor config winning per key. Toggling a project's classpath resolution is a live config change, not a restart.

### Config schema

EDN (`.clj-pulse/config.edn`) and the JSON mirror. **Envelopes differ by channel**: `initializationOptions` carries the bare object (`{"projects": [...]}` — vscode-languageclient passes it verbatim); `didChangeConfiguration` wraps it in the settings section (`{"clojurePulse": {"projects": [...]}}`) and the handler unwraps `settings.clojurePulse` before parsing. `projects::parse_json` always takes the bare `{"projects": [...]}` object.

```clojure
{:projects [{:path "apps/backend"        ; relative to workspace root; "." = the root project
             :classpath {:enabled true   ; default: true for ".", false for subprojects
                         :cmd "clojure -A:dev:test -Spath"}}]}
```

```json
{"projects": [{"path": "apps/backend",
               "classpath": {"enabled": true, "cmd": "clojure -A:dev:test -Spath"}}]}
```

- Entries are **overrides**: every detected project exists whether or not it is listed; a listed entry overrides the defaults for that path. An entry whose `:path` names a directory with no manifest is ignored with a warning.
- Merge: file config over defaults, editor config over file config — per project path, per key (`:enabled` and `:cmd` independently).
- Default `:cmd` by manifest kind: `deps.edn` → `clojure -A:dev:test -Spath`; `project.clj` → `lein classpath`; lgx projects have no command (the internal `lgx::resolve` stays, stage 3 does not apply).
- The old top-level `:classpath {:enabled … :aliases […]}` syntax is **dropped entirely** — no back-compat parsing.
- `CLJ_PULSE_DISABLE_CLASSPATH_CLI` (non-empty) still forces `enabled = false` for **every** project (the e2e harness depends on this).

### Detection

Walk from the workspace root with `ignore::WalkBuilder` (respects `.gitignore`), max depth 4, collecting directories that contain `deps.edn`, `project.clj`, or `lgx.edn`. The root itself is always a project (path `"."`) even with no manifest at the root. Nested manifests (3-level monorepos) each yield a project. Detection reruns when a watched manifest file is created or deleted.

### Indexing (single shared `Index`)

- **Stage 1**: `config::source_paths(project_dir)` per project, union all, one `scanner::build_index` over the union, `merge_project_from` as today. Namespace collisions across projects (two `dev/user.clj` → ns `user`): **last-writer-wins plus a warning log** naming both files. Scoped resolution is explicitly out of scope for this plan.
- **Stage 2**: per project, `classpath::discover(project_dir)` (`.cpcache`) — or `lgx::resolve` / the Leiningen `~/.m2` heuristic, exactly the existing `resolve_and_index_libs` logic run per project dir. Library entries recorded **per project**; the union is what gets indexed. The scanner's under-root skip keeps using the *workspace* root: in-root classpath dirs are either another project's sources (already indexed as project files in stage 1) or picked up lazily on `didOpen`, as today.
- **Stage 3**: for each project with `:enabled true` and a `:cmd`, run the command serialized on the existing `ClasspathCliLock`. On a changed per-project entry set: rebuild libraries as **full union rebuild**. (Deliberate v1 simplicity; the per-jar disk cache makes rebuilds cheap.)
- **Union rebuild is per-kind, not one flat scan**: a helper `rebuild_libs(projects, states, index)` does `clear_libs()` and then, *per project*, re-indexes that project's current entries with its kind's indexer — deps/lein entries via `index_classpath_libs` (workspace-root skip), lgx entries via `index_dir_libs` plus `lgx::index_letgo_core` (which also re-sets the let-go markers `clear_libs` dropped). A flat `index_classpath_libs` over the union would wrongly skip in-workspace lgx `:local/root` dirs and lose let-go core.
- **Disabling a project only stops stage 3** — it must *not* remove the project's stage-2 libraries. On disable, revert that project's entries to a fresh stage-2 result (`classpath::discover` / lgx / lein heuristic), set status `cached`/`unresolved`, and union-rebuild only if the entry set actually changed.
- **Stale-result guard**: a stage-3 run may finish after the config that launched it changed (disable happens without taking the CLI lock). Keep a config generation counter on `Backend`; a stage-3 task snapshots it at launch and, before applying results (entries, status, rebuild), re-checks under the lock that the generation is unchanged and the project is still enabled with the same `:cmd` — otherwise it discards the result.
- `lib_entries` (currently one `HashSet`) becomes `HashMap<PathBuf /* project abs dir */, HashSet<PathBuf>>` plus a per-project **status**: `disabled` / `cached` / `resolving` / `resolved` / `unresolved` / `error` (with message).

### Command execution

`classpath::resolve_via_cmd(cmd: &str, dir: &Path)`: run through the shell (`sh -c` on Unix, `cmd /C` on Windows), `current_dir(dir)`, 300 s timeout, `kill_on_drop`, classpath = last non-empty stdout line, relative entries resolved against `dir`. Replaces `resolve_via_cli`/`alias_arg`. Any failure degrades to the stage-2 result and sets status `error`.

### Protocol

- **New request `clojurePulse/projects`** → the grouped view:
  ```json
  [{"path": ".", "kind": "deps",
    "classpath": {"enabled": true, "cmd": "clojure -A:dev:test -Spath", "status": "resolved"},
    "libraries": [{"name": "aero", "version": "1.1.6", "path": "...", "kind": "jar"}]}]
  ```
  Each project's `libraries` come from `libraries::from_entries` with **that project's** own source paths and lib entries.
- **`clojurePulse/externalLibraries` unchanged in shape**: flat deduped union across projects (older editors keep working against a newer server).
- **`clojurePulse/librariesChanged`** (existing notification) also fires on project list / status changes; clients re-request.
- **`workspace/didChangeConfiguration`**: parse `settings.clojurePulse.projects`, re-merge, diff against current resolved config; newly enabled projects get a stage-3 run, newly disabled ones drop their lib entries from the union (followed by a union rebuild). Store the editor-config layer so later file-config reloads re-merge correctly.

### Watched files

`did_change_watched_files` currently re-resolves the root on any manifest/`.cpcache` change. Route instead by owning project: longest project-dir prefix of the changed path. A manifest created in an untracked directory triggers re-detection (new project appears, panel notifies).

### Testing strategy

Unit tests per module (detection, config parse/merge, command runner) with `tempfile` fixtures, following the existing test style. One new e2e scenario (`tests/test_e2e.rs` harness): a monorepo fixture — root `deps.edn`, `apps/a` and `libs/common` subprojects, `apps/a` depending on `libs/common` via `:local/root` — asserting cross-project definition, the `clojurePulse/projects` response, and a `didChangeConfiguration` toggle driving stage 3 with a stub command (the verbatim `:cmd` makes stage 3 stubbable without a real `clojure`). Verification gate: `bb check` and `bb e2e` (see AGENTS.md).

## File Structure

- Create: `src/projects.rs` — detection, config model (`ProjectEntry`, resolved `Project`), EDN + JSON parsing, per-key merge, defaults. Pure logic, unit-testable.
- Modify: `src/classpath.rs` — replace `resolve_via_cli`/`alias_arg` with `resolve_via_cmd(cmd, dir)`.
- Modify: `src/settings.rs` — delete `ClasspathConfig`/`parse_classpath` (`:lint-as` loading stays).
- Modify: `src/server.rs` — multi-project state on `Backend`, staged startup over N projects, `clojurePulse/projects` handler, `did_change_configuration`, watched-file routing.
- Modify: `src/libraries.rs` — no logic change; called per project.
- Modify: `src/lib.rs` — export `projects` module.
- Modify: `tests/test_e2e.rs` — monorepo fixture + scenario.
- Modify: `AGENTS.md` (invariants), `README.md` (config docs).

---

### Task 1: Project detection (`src/projects.rs`)

**Files:**
- Create: `src/projects.rs` (with `#[cfg(test)]` tests in-file, matching repo style)
- Modify: `src/lib.rs` (add `pub mod projects;`)

- [ ] **Step 1: Write failing tests for `detect`**
  `pub fn detect(root: &Path) -> Vec<PathBuf>` returning **relative** paths of directories under `root` containing `deps.edn`, `project.clj`, or `lgx.edn` — excluding `root` itself, sorted, max depth 4, honoring `.gitignore`. Tempdir cases: two subprojects found; nested subproject at depth 3 found; gitignored dir skipped; dir past depth 4 skipped; empty repo → empty vec.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test projects::`
  Expected: FAIL (module missing / unresolved).

- [ ] **Step 3: Implement `detect` with `ignore::WalkBuilder`**
  Use `.max_depth(Some(4))`. Only directories; check the three manifest names per dir.

- [ ] **Step 4: Run to verify pass**
  Run: `cargo test projects::`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: detect subproject manifests under the workspace root"`

### Task 2: Config model, parsing, and merge (`src/projects.rs`)

**Files:**
- Modify: `src/projects.rs`

- [ ] **Step 1: Write failing tests for the model and merge**
  Shapes (shared contract with server.rs and the editor — keep exactly):
  ```rust
  pub struct ClasspathOverride { pub enabled: Option<bool>, pub cmd: Option<String> }
  pub struct ProjectEntry { pub path: String, pub classpath: ClasspathOverride }

  pub enum ProjectKindTag { Deps, Lein, Lgx }
  pub struct Project {
      pub rel_path: String,          // "." for the root
      pub dir: PathBuf,              // absolute
      pub kind: ProjectKindTag,
      pub classpath_enabled: bool,
      pub classpath_cmd: Option<String>,  // None for lgx
  }

  pub fn parse_edn(contents: &str) -> Vec<ProjectEntry>          // reads {:projects [...]}
  pub fn parse_json(v: &serde_json::Value) -> Vec<ProjectEntry>  // reads {"projects": [...]}
  pub fn resolve(root: &Path, detected: &[PathBuf],
                 file: &[ProjectEntry], editor: &[ProjectEntry]) -> Vec<Project>
  ```
  Test cases: defaults (root enabled, subproject disabled, default cmd per manifest kind — `deps.edn` → `clojure -A:dev:test -Spath`, `project.clj` → `lein classpath`, `lgx.edn` → `None`); file entry overrides one key, defaults keep the other; editor entry overrides file per key; entry for a path with no manifest is dropped with a warning; root always present as `"."` even without a root manifest (kind: Deps, no cmd run if no `deps.edn` — see Task 5); `CLJ_PULSE_DISABLE_CLASSPATH_CLI` forces every `classpath_enabled` to false; malformed EDN/JSON yields empty overrides, never a panic.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test projects::`
  Expected: FAIL.

- [ ] **Step 3: Implement parsing and merge**
  EDN via `edn_format` + the `crate::edn` helpers (`get`, `kw`); JSON via manual `serde_json::Value` walking (tolerant of partial shapes, like the EDN side). Merge keyed by normalized `rel_path` (treat `""`, `"."`, `"./"` alike).

- [ ] **Step 4: Run to verify pass**
  Run: `cargo test projects::`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: multi-project config model with per-key merge"`

### Task 3: Verbatim classpath command (`src/classpath.rs`, `src/settings.rs`)

**Files:**
- Modify: `src/classpath.rs`
- Modify: `src/settings.rs`

- [ ] **Step 1: Adapt the resolver tests**
  Rewrite the `resolve_with` stub tests to drive `resolve_via_cmd(cmd: &str, dir: &Path, timeout)` semantics: command string run through the shell in `dir`; last non-empty stdout line is the classpath; relative entries resolve against `dir`; stderr surfaces on failure; timeout kills the child. (The stub becomes a command string like `"sh /path/to/stub"` instead of an injected program.)

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test classpath::`
  Expected: FAIL (new signature).

- [ ] **Step 3: Implement `resolve_via_cmd`; delete the old syntax**
  `sh -c <cmd>` on Unix, `cmd /C <cmd>` on Windows; keep `current_dir`, 300 s default timeout, `kill_on_drop`, `parse_entries(dir, line)`. Delete `resolve_via_cli` and `alias_arg`. In `settings.rs`, delete `ClasspathConfig`, `classpath()`, `parse_classpath` and their tests (`:lint-as` loading stays untouched).

- [ ] **Step 4: Run to verify pass (full check)**
  Run: `bb check`
  Expected: PASS — compile errors in `server.rs` from the deleted functions are fixed in Task 5; if `bb check` cannot pass before that, note it and defer the full-green gate to Task 5, keeping `cargo test classpath:: projects::` green here.

- [ ] **Step 5: Commit**
  `git commit -m "feat: verbatim shell command for classpath resolution, drop :aliases syntax"`

### Task 4: Per-project library state on `Backend` (`src/server.rs`)

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Restructure state**
  Replace the single `lib_entries: LibEntries` with a per-project map, and add status + the resolved project list + the stored editor-config layer:
  ```rust
  pub enum ClasspathStatus { Disabled, Cached, Resolving, Resolved, Unresolved, Error(String) }
  struct ProjectState { entries: HashSet<PathBuf>, status: ClasspathStatus }
  // Backend fields:
  //   projects: Mutex<Vec<projects::Project>>
  //   project_state: Mutex<HashMap<String /* rel_path */, ProjectState>>
  //   editor_config: Mutex<Vec<projects::ProjectEntry>>
  //   config_generation: AtomicU64   // bumped on every re-resolve; stage-3 stale-result guard
  ```
  `Backend.root` stays (log dir, config location, watched-file routing). Add helpers: `fn lib_union(&self) -> Vec<PathBuf>` (dedup across projects) and `fn rebuild_libs(&self)` — `clear_libs()` then per-project, kind-appropriate re-indexing of each project's current entries (deps/lein → `index_classpath_libs` with the workspace-root skip; lgx → `index_dir_libs` + `lgx::index_letgo_core`), per the design's "union rebuild is per-kind" rule.

- [ ] **Step 2: Compile**
  Run: `cargo check`
  Expected: errors only in the initialize/watched-files code paths rewritten next.

- [ ] **Step 3: Commit** (with Task 5 if intermediate state doesn't compile standalone)

### Task 5: Staged multi-project startup (`src/server.rs`)

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Rewrite the `initialize` indexing block**
  - Resolve projects: `projects::detect` + `parse_edn` (from `root/.clj-pulse/config.edn`) + `parse_json` (from `params.initialization_options`, stored as the editor layer) → `projects::resolve`. Store in `Backend.projects`.
  - **Stage 1 task**: union `config::source_paths(p.dir)` over all projects; one `build_index` + `merge_project_from` as today. Log per-project source paths.
  - **Collision warning**: in the stage-1 flow, before merging, warn for any namespace present in both the existing index and the new scan with a *different* file (and for duplicates within the scan): `"namespace {ns} defined in both {a} and {b}; last one wins"`.
  - **Stage 2 task**: per project, run the existing `resolve_and_index_libs` logic against `p.dir`, record entries + status per project (`Cached` when non-empty, `Unresolved` when empty), index as it goes (the under-root skip in `scanner::index_classpath_libs` keeps using the *workspace* root — pass it explicitly). Notify `LibrariesChanged` once after the loop.
  - **Stage 3 task**: for each project with `classpath_enabled && classpath_cmd.is_some() && p.dir has the manifest`, serialized on `classpath_cli_lock`: snapshot `config_generation`; set status `Resolving`, notify; run `resolve_via_cmd`; before applying anything, re-check under the lock that the generation is unchanged and the project is still enabled with the same cmd — otherwise discard the result. On success compare with that project's entry set — on change, `rebuild_libs()`, set `Resolved`; on failure set `Error(reason)`, keep stage-2 entries. Notify `LibrariesChanged` after each project so the panel updates progressively.

- [ ] **Step 2: Run the harness**
  Run: `bb check && bb e2e`
  Expected: PASS — existing single-project fixtures behave identically (root project defaults reproduce today's behavior; `CLJ_PULSE_DISABLE_CLASSPATH_CLI` still suppresses stage 3).

- [ ] **Step 3: Commit**
  `git commit -m "feat: staged indexing across detected subprojects"`

### Task 6: `clojurePulse/projects` request (`src/server.rs`)

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Implement the handler**
  Registered alongside the existing custom methods. Response per project (order: root first, then rel_path-sorted):
  ```json
  {"path": ".", "kind": "deps",
   "classpath": {"enabled": true, "cmd": "clojure -A:dev:test -Spath", "status": "resolved"},
   "libraries": [ ...libraries::Library... ]}
  ```
  `libraries` = `libraries::from_entries(source_paths(p.dir), that project's entries)`. Status serialized lowercase; `Error` as `{"status": "error", "message": "..."}`. Keep `externalLibraries` working: it becomes `from_entries(union of all projects' own_paths, lib_union())`.

- [ ] **Step 2: e2e assertion**
  Extend a monorepo e2e (fixture built in Task 9) or add a minimal two-project fixture now: `clojurePulse/projects` returns entries for `"."` and the subproject with expected `enabled`/`status`.

- [ ] **Step 3: Run**
  Run: `bb e2e`
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -m "feat: clojurePulse/projects grouped request"`

### Task 7: Live config via `did_change_configuration` (`src/server.rs`)

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Implement the handler**
  Unwrap `params.settings["clojurePulse"]` and parse the bare object with `projects::parse_json`; store as the editor layer; bump `config_generation`; re-run `projects::resolve`; diff old vs new resolved list:
  - newly enabled (or `:cmd` changed on an enabled project) → spawn a stage-3 run for those projects;
  - newly disabled → set status back to the stage-2 truth: re-run stage-2 discovery for that project (`Cached`/`Unresolved`), and `rebuild_libs()` only if its entry set actually changed (stage-2 libraries stay indexed — disable gates stage 3 only);
  - project set unchanged otherwise → no-op.
  Notify `LibrariesChanged` on any effective change.

- [ ] **Step 2: e2e assertion**
  In the harness: send `workspace/didChangeConfiguration` enabling the subproject with a stub `:cmd` (shell script echoing a classpath); wait for `"full classpath indexed"`-equivalent log; assert `clojurePulse/projects` now shows `"resolved"` and the stubbed library. Then disable it again and assert the project reverts to its stage-2 state (status `cached`/`unresolved`) — stage-2 libraries must not disappear.

- [ ] **Step 3: Run**
  Run: `bb e2e`
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -m "feat: live project toggles via didChangeConfiguration"`

### Task 8: Watched-file routing (`src/server.rs`)

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Route by owning project**
  In `did_change_watched_files`: resolve the owning project of a changed manifest/`.cpcache`/config path by longest `p.dir` prefix. Manifest changed → re-run source-paths rebuild (stage 1 union) + stage 2 for that project (+ stage 3 if enabled). `.cpcache` changed → stage 2 for that project. Manifest **created or deleted** → re-run detection and `projects::resolve` (bumping `config_generation`): a new project gets indexed (stages 1–2); a project whose last manifest disappeared is removed — drop its `ProjectState`, prune its sources from the project index via the stage-1 union rebuild, `rebuild_libs()`, and drop it from the protocol response; a kind change (e.g. `deps.edn` added next to `project.clj`) re-derives the default `:cmd`. `.clj-pulse/config.edn` changed → re-parse file layer, same diff logic as Task 7 (editor layer preserved).

- [ ] **Step 2: Run**
  Run: `bb check && bb e2e`
  Expected: PASS (existing watched-file e2e coverage still green).

- [ ] **Step 3: Commit**
  `git commit -m "feat: route watched manifest changes to their owning project"`

### Task 9: Monorepo e2e scenario (`tests/test_e2e.rs`)

**Files:**
- Modify: `tests/test_e2e.rs` (+ new fixture under `tests/fixtures/` following the harness's `setup_project()` conventions)

- [ ] **Step 1: Build the fixture**
  `monorepo/`: root `deps.edn` (minimal), `apps/a/deps.edn` (`:local/root ../../libs/common` dep, `src/a/core.clj` calling `common.util/helper`), `libs/common/deps.edn` + `src/common/util.clj` defining `helper`.

- [ ] **Step 2: Write the scenario**
  Initialize on the monorepo root; wait for `"Indexed"`; assert: goto-definition from `a/core.clj`'s `common.util/helper` usage lands in `libs/common/src/common/util.clj` (cross-project, no file ever opened from `common`); `clojurePulse/projects` lists `"."`, `"apps/a"`, `"libs/common"` with subprojects disabled.

- [ ] **Step 3: Run**
  Run: `bb e2e`
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -m "test: monorepo cross-project navigation e2e"`

### Task 10: Docs

**Files:**
- Modify: `AGENTS.md` (invariants: graduated indexing is per-project; `:projects` schema; the dropped `:aliases` syntax), `README.md` (configuration section), `ARCHITECTURE.md` (index population)

- [ ] **Step 1: Update the three docs** (use /writing-clearly)
- [ ] **Step 2: Final gate**
  Run: `bb check && bb e2e && bb e2e-nvim`
  Expected: PASS (protocol changed — nvim harness required per AGENTS.md).
- [ ] **Step 3: Commit**
  `git commit -m "docs: multi-project configuration and invariants"`
