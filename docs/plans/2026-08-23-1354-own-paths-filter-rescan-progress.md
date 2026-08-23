# Own-Paths Filter, Rescan Request, and Classpath Progress Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop showing a project's own source dirs as "external libraries", add a `clojurePulse/rescan` request that forces re-detection and re-resolution, and report stage-3 classpath resolution via LSP `$/progress`.

**Tech Stack:** Rust, tower-lsp 0.20, lsp-types (via tower-lsp), bb tasks for verification.

**Companion plan:** the VS Code extension consumes `clojurePulse/rescan` and the progress signal — see `clojure-pulse-vscode/docs/plans/2026-08-23-1354-project-form-rescan-ui.md`. This plan is self-contained; the extension degrades gracefully without it.

---

## Design

### 1. Filter project-own dirs from library lists (display bug)

A resolved classpath (`clojure -A:dev:test -Spath`) includes alias `:extra-paths` (`dev`, `test/clj`, per-platform source dirs). `libraries::from_entries` filters "own" paths by exact match against `config::source_paths`, which reads only top-level `:paths` plus default `src`/`test` — so alias paths leak into the External Libraries lists as dir "libraries" named by basename (`clj`, `cljc`, `dev`). In a monorepo, the root's classpath can also list subproject dirs, duplicating project nodes as fake libraries.

Fix: `from_entries` gains a `project_dirs: &[PathBuf]` parameter and additionally excludes **owned** non-jar entries. A bare `starts_with` prefix rule is wrong here: the root project's dir is the workspace root, so it would swallow every in-workspace dir entry — including a gitignored vendored `:local/root` checkout that belongs to no project and must stay listed (it is only navigable through this panel). The ownership rule instead:

> For a non-jar entry inside some project dir, walk up from the entry toward (and including) that project dir to the **nearest ancestor holding a manifest** (`deps.edn`/`project.clj`/`lgx.edn`). The entry is excluded iff that ancestor is one of `project_dirs` — or no manifest ancestor exists within the project dir (a manifest-less root still owns its bare source dirs).

Worked cases: root's `dev` or `src/clj` → nearest manifest ancestor is the root → excluded. A detected subproject's `libs/x/src` on the root's classpath → nearest is `libs/x`, a known project → excluded (it has its own project node). A gitignored vendored checkout's `vendor/y/src` → nearest is `vendor/y`, *not* a known project → kept. The existing exact-match `own_paths` filter stays. Jar entries are never filtered (e.g. a jar built into `target/`). Both call sites pass **every** resolved project's `dir`:

- `server.rs` `external_libraries` (flat list, ~line 864)
- `server.rs` `projects_info` (per-project lists, ~line 893)

Display-only by construction: `from_entries` builds the panel lists; indexing consumes raw entries elsewhere. Navigation is unaffected.

### 2. `clojurePulse/rescan` request

Today re-resolution triggers only on config/manifest changes. There is no retry for `status: error` projects and no way to pick up a new gitignored subproject (ignored dirs fire no watchers). New custom method:

- Registered in `main.rs` alongside the other `custom_method` calls.
- Handler `Backend::rescan(&self, _params: Option<Value>) -> jsonrpc::Result<Value>`: spawns the work and returns `Ok(Value::Null)` immediately. Completion is conveyed by `$/progress` and `clojurePulse/librariesChanged`, not the response.
- The spawned task mirrors `did_change_configuration`'s task: take `config_apply_lock`, snapshot `old` projects, call `refresh_projects` (it already re-runs `projects::detect`, re-reads `.clj-pulse/config.edn`, bumps the config generation — which correctly discards in-flight stage-3 results), then `apply_project_diff`.
- `apply_project_diff` returns the `stage3_runs` it performed. After it, force stage-3 for every project that is enabled, has a cmd and a manifest, and is **not** in that returned list — via `run_stage3_project` per project. Nothing runs twice; error-status projects retry because the diff won't have run them (unchanged config) but the forced pass will.
- The task ends with one **unconditional** `librariesChanged`: on a fully unchanged workspace with nothing to resolve, `apply_project_diff` sends no notification and no progress runs, and a client would otherwise have no completion signal at all.

### 3. `$/progress` around stage-3 resolution

The editor's own status quiets down after source indexing while classpath resolution still runs — the user can't tell why libraries don't navigate yet. Standard LSP work-done progress fixes this in every editor at once.

- tower-lsp 0.20 has no progress helper (`service/client.rs` carries a TODO), but its generic `Client::send_request::<R>` / `send_notification::<N>` cover it: `lsp_types::request::WorkDoneProgressCreate`, then `lsp_types::notification::Progress` with `WorkDoneProgress::Begin` / `End`.
- Capability-gated: capture `params.capabilities.window.work_done_progress == Some(true)` at `initialize` into a field on `Backend` (`AtomicBool`). When false, send nothing. A failed `workDoneProgress/create` request downgrades to silence (skip Begin/End), never an error.
- Placement: inside `run_stage3_project` — one place covers startup resolution, config-change runs, and rescan. One token per project **run**, unique per operation as the spec requires (rescans repeat runs for the same project): `clj-pulse/classpath/<rel_path>/<n>` with `n` from an `AtomicU64` counter on `Backend`. Begin title `Resolving classpath: <rel_path>` sent just before `classpath::resolve_via_cmd`, End sent on **every** exit after Begin: the stale-result discard, the `Ok` branch, and the `Err` branch. (Three explicit End sends — an RAII guard can't `await` in Drop.)

### Testing strategy

- Unit tests in `src/libraries.rs` for the new filter parameter.
- E2E via the existing `LspClient` harness (`tests/test_e2e.rs`): its notification stash captures `$/progress` and `librariesChanged`; `initialize` can advertise `window.workDoneProgress`. Stage-3 e2e uses a trivial classpath cmd (e.g. `echo <path>`), per the harness's `start_with_classpath_cli` pattern; the filter e2e can work stage-2-only via a `.cpcache` fixture.
- Gates: `bb check` (fmt + clippy `-D warnings` + tests) and `bb e2e` per AGENTS.md; client-visible protocol changes (rescan, progress) should also pass `bb e2e-nvim`.

## File Structure

- Modify: `src/libraries.rs` — `from_entries` signature + prefix filter + unit tests.
- Modify: `src/server.rs` — call sites; `Backend` capability field; progress in `run_stage3_project`; `rescan` handler.
- Modify: `src/main.rs` — register `clojurePulse/rescan`.
- Modify: `tests/test_e2e.rs` — filter, rescan, and progress e2e cases.
- Modify: `README.md` — document rescan under the protocol/configuration notes (one short paragraph).

---

### Task 1: Prefix filter in `from_entries` + call sites

**Files:**
- Modify: `src/libraries.rs`, `src/server.rs`

- [x] **Step 1: Write failing unit tests**
  In `src/libraries.rs` tests: new signature `from_entries(own_paths: &[PathBuf], project_dirs: &[PathBuf], entries: &[PathBuf])` with the nearest-manifest-ancestor ownership rule (tests use tempdir fixtures, since the rule reads manifests from disk). Cases: a bare source dir under the root project (`dev`, `src/cljc`) is excluded; a detected subproject's source dir (`libs/x/src`, `libs/x` in `project_dirs` with a manifest) is excluded; **an in-workspace non-project `:local/root` checkout's source dir (`vendor/y/src`, `vendor/y` has a manifest but is not in `project_dirs`) is kept**; a jar under a project dir (`/ws/target/lib.jar`) is kept; a dir outside every project dir (gitlib checkout in `~/.gitlibs`) is kept; a manifest-less root still owns its bare dirs; the existing exact-match `own_paths` behavior is unchanged. Update existing tests for the new parameter (empty `project_dirs` preserves old behavior).

- [x] **Step 2: Run to verify failure**
  Run: `cargo test -p clj-pulse --lib libraries` (or `cargo test from_entries`)
  Expected: FAIL (signature mismatch / new cases).

- [x] **Step 3: Implement**
  Add the parameter and the ownership check per the design (non-jar + inside a project dir + nearest manifest ancestor is a known project or absent). Update both `server.rs` call sites to pass all resolved projects' `dir`s: in `external_libraries` build `project_dirs` from `project_list`; in `projects_info` pass the full list's dirs (not just the current project's).

- [x] **Step 4: Run to verify pass**
  Run: `bb check`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "fix: exclude project-own dirs from external library lists"`

> Deviation (codex on Task 1): entries and project dirs are lexically normalized (`..`/`.` removed) before the ownership check — `lgx::resolve` keeps sibling `:local/root "../common"` verbatim, which defeated the prefix/equality comparisons. The scanner's `normalize_lexically` was made `pub(crate)` and reused.

### Task 2: E2E — own dirs no longer listed

**Files:**
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Write the e2e case**
  Fixture: a project whose `.cpcache/<n>.cp` lists an alias-style own dir (e.g. `<root>/dev`, must exist on disk) plus an out-of-project dir path. After `initialize`, request `clojurePulse/externalLibraries` and `clojurePulse/projects`: neither response contains the own dir; the outside dir is present. Follow the harness template (`setup_project()` + `initialize`, per AGENTS.md testing notes).

- [x] **Step 2: Run**
  Run: `bb e2e`
  Expected: PASS (new case included).

- [x] **Step 3: Commit**
  `git commit -m "test: e2e for own-dir filtering in library lists"`

> Deviation (codex on Task 2): the fixture classpath uses the platform separator (`;` on Windows) like the unit tests do.

### Task 3: Work-done progress around stage-3

**Files:**
- Modify: `src/server.rs`, `tests/test_e2e.rs`

- [x] **Step 1: Capability capture**
  Add `work_done_progress: AtomicBool` to `Backend` (default false), set in `initialize` from `params.capabilities.window.and_then(|w| w.work_done_progress).unwrap_or(false)`.

- [x] **Step 2: Progress in `run_stage3_project`**
  When the capability is set: `send_request::<WorkDoneProgressCreate>` with a per-run unique token `clj-pulse/classpath/<rel_path>/<n>` (`AtomicU64` counter on `Backend`); on `Ok`, send `Progress` Begin (`title: "Resolving classpath: <rel_path>"`) before `resolve_via_cmd`, and End on all three exits after Begin (stale-discard return, `Ok`, `Err`). On create `Err`, skip Begin/End silently.

- [x] **Step 3: E2E**
  New case: `initialize` advertising `window: {workDoneProgress: true}` on a fixture with the classpath CLI enabled and a stub cmd (`echo <existing dir>`); wait for resolution, then assert the stash holds `$/progress` Begin and End for the token. Also assert a client *without* the capability receives no `$/progress`.

- [x] **Step 4: Run**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "feat: report classpath resolution via LSP work-done progress"`

> Deviation: capability + counter live in one `ProgressState` (cloned Arc pair) threaded through the stage-3 functions instead of two separate `Backend` atomics. The End notification is sent *before* the "full classpath indexed"/failure log lines, so the log doubles as a reliable completion barrier (the e2e was otherwise racy).

### Task 4: `clojurePulse/rescan`

**Files:**
- Modify: `src/server.rs`, `src/main.rs`, `tests/test_e2e.rs`

- [x] **Step 1: Handler + registration**
  `Backend::rescan` per the design: immediate `Ok(Value::Null)`; spawned task takes `config_apply_lock`, snapshots `old`, runs `refresh_projects` + `apply_project_diff`, then `run_stage3_project` for each enabled+cmd+manifest project not in the returned `stage3_runs`, and finishes with one unconditional `librariesChanged` (the completion signal even when nothing changed and nothing resolved). Register in `main.rs`.

- [x] **Step 2: E2E**
  Cases: (a) rescan on a plain fixture returns null and fires `librariesChanged` even with nothing to resolve (the unconditional final notification); (b) with the classpath CLI and a stub cmd, a second rescan re-runs the command for an already-resolved project (assert via the "resolving classpath" log message count or a second progress Begin — with a *different* token than the first run's); (c) the real target scenario: a **gitignored** subdir with a `deps.edn`, listed in config, created after initialize, appears in `clojurePulse/projects` after a rescan.

- [x] **Step 3: Run**
  Run: `bb check && bb e2e && bb e2e-nvim`
  Expected: PASS (protocol change → nvim harness too, per AGENTS.md).

- [x] **Step 4: Commit**
  `git commit -m "feat: clojurePulse/rescan forces re-detection and re-resolution"`

> Deviation (codex on Task 4): forced reruns first revert their project to stage-2 truth (`reconcile_projects` with the forced list) so a rerun whose command now fails degrades to stage-2 data instead of keeping the previous run's stage-3 entries. The gitignored-subproject e2e polls `clojurePulse/projects` (startup's own `librariesChanged` can still be in flight when the rescan is requested).

### Task 5: Docs

**Files:**
- Modify: `README.md`

- [x] **Step 1: Document rescan + progress** (use /writing-clearly)
  Short additions where custom methods/configuration are described: `clojurePulse/rescan` (what it re-runs, immediate null response, completion via notifications) and the work-done progress behavior (capability-gated).

- [x] **Step 2: Final gate**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 3: Commit**
  `git commit -m "docs: rescan request and classpath progress"`

---

## Completion Summary (2026-08-23)

**Status: completed.** All five tasks on the `monorepo` branch; gates green: `bb check` (310 unit tests, clippy `-D warnings`), `bb e2e` (90 tests, six new), `bb e2e-nvim` (real editor client, run at the Task-4 protocol gate).

**What was implemented**
- `libraries::from_entries` gained a `project_dirs` parameter with the nearest-manifest-ancestor ownership rule: alias `:extra-paths` and other projects' source dirs no longer appear as fake dir "libraries", while vendored non-project checkouts stay listed. Paths are lexically normalized first (lgx keeps `../sibling` verbatim).
- `$/progress` around every stage-3 run: capability-gated (`window.workDoneProgress`), per-run unique tokens (`clj-pulse/classpath/<rel>/<n>`), Begin before the command, End on all exits (sent before the completion log so the log is a reliable barrier), create-failure downgrades to silence.
- `clojurePulse/rescan`: immediate null response; background task re-detects, re-resolves, applies the diff, then force-runs stage 3 for every eligible project the diff skipped (after reverting them to stage-2 truth), ending with one unconditional `librariesChanged`.
- README documents both; `librariesChanged` described as a progress signal, not a single completion event.

**Issues encountered:** none blocking. Codex flagged 5 findings across the tasks (lexical `..` defeating ownership checks, Windows classpath separator in a fixture, the progress/log ordering race, stale entries on failed forced reruns, and the completion-signal doc wording) — all fixed in follow-up commits.

**What the plan could have specified better:** the ordering contract between progress End and the "full classpath indexed" log (the e2e strategy assumed both observable, but their order matters for deterministic tests), and that rescan's forced reruns need the same stage-2 revert the cmd-change path already does — the design's own "failure degrades to stage-2" invariant implied it, but the task steps didn't say it.
