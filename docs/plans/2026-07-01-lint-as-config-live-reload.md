# Live reload of `:lint-as` config Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pick up `:lint-as` changes in `.clj-kondo/config.edn` and
`.clj-pulse/config.edn` without restarting the server, by watching those files
and re-indexing the project on change.

**Tech Stack:** Rust, tower-lsp (`workspace/didChangeWatchedFiles`), tree-sitter.

---

## Design

Today `clj-pulse` loads `:lint-as` once at startup into an `OnceLock`, so a
config edit needs a restart (see `docs/plans/2026-06-30-clj-kondo-lint-as-config.md`).
This makes it live.

The file watcher already registers `**/*.edn` (`src/server.rs`), so change
events for `.clj-kondo/config.edn` and `.clj-pulse/config.edn` already reach
`did_change_watched_files`; they are just not acted on (they fall into the
Integrant-EDN branch and no-op). So the work is: make the config reloadable,
detect those files in the handler, and reload + re-index on change. Library
indexing is left alone, since `:lint-as` only affects project-file extraction.

### Key decisions

1. **`Index.extract_config`: `OnceLock<ExtractConfig>` → `RwLock<ExtractConfig>`.**
   `OnceLock` is set-once; reload needs replace. The accessor returns an owned
   **clone** (`read().unwrap().clone()`) instead of `&ExtractConfig`. The clone
   is off the hot path: `build_index` takes `cfg: &ExtractConfig` once and its
   rayon loop reuses that borrow, so it is one clone per index build / per
   request, and the `lint_as` map is tiny. Matches the existing
   `letgo_native: RwLock<…>` pattern. Ripples a `&` to the ~8 call sites of
   `index.extract_config()`.

2. **Config change → reload `settings::load(root)` + full project re-index**,
   reusing `scanner::build_index` + `Index::merge_project_from` (the same path
   `source_paths_changed` already takes). No library re-index for a config-only
   change.

3. **Detect config files before the `is_edn` branch.** `config.edn` is `.edn`,
   so it must be intercepted early (mirroring the manifest check at the top of
   the loop) or it gets treated as a possible Integrant config. Match
   `file_name == "config.edn"` with a parent dir of `.clj-kondo` or
   `.clj-pulse`; set a `config_changed` flag and `continue`. Only the single
   `config.edn` files (what `settings::load` reads); not `.clj-kondo/config/`.

4. **Emit a `clj-pulse: config reloaded` log** after the rebuild — editor
   feedback and an e2e synchronization point. It fires before the (optional)
   library re-index branch so it is never skipped by that branch's early
   return.

5. **Add explicit `**/.clj-kondo/config.edn` and `**/.clj-pulse/config.edn`
   watchers** next to `**/*.edn`, so dot-dir matching is robust on editors that
   do not glob dotfiles under `**/*.edn`.

### Post-loop control flow (server.rs)

Run the spawned task when `classpath_changed || config_changed`:

```
if config_changed            -> index.set_extract_config(settings::load(&root))
if source_paths_changed
   || config_changed         -> build_index(&root, &paths, &index.extract_config())
                                 then merge_project_from(.., open_paths)
if config_changed            -> log "clj-pulse: config reloaded"
if classpath_changed         -> clear_libs(); resolve_and_index_libs(); log
```

A config-only edit reloads config + re-indexes project and skips the library
branch. A manifest edit keeps today's behavior (config unchanged, project
rebuild + library re-index). Both together do all three.

### Error handling

`settings::load` already returns an empty config on missing/unparseable files,
so a malformed edit degrades to "no lint-as" rather than crashing. A
`build_index` error is logged (as today) and leaves the previous index in place.

### Testing

An e2e before/after test on the existing `tests/fixtures/lint_as_project`
fixture: goto-def on `widget` resolves to the `defthing` line; rewrite the
fixture's `.clj-kondo/config.edn` to `{}`; send `didChangeWatchedFiles`
(CHANGED); `wait_for_log("config reloaded")`; goto-def on `widget` now returns
null. The existing suite must stay green (the accessor change is
behavior-preserving at the default empty config).

## File Structure

**Modified:**
- `src/index/mod.rs` — `extract_config` field `OnceLock` → `RwLock`; accessor
  returns a clone; setter replaces under a write lock; drop the now-unused
  `LazyLock` import if nothing else uses it.
- `src/server.rs` — two extra watcher globs in `initialized`; `config_changed`
  flag + early detection in `did_change_watched_files`; restructured post-loop
  spawn; `&` on the `extract_config()` call sites here.
- `src/handlers/references.rs`, `src/handlers/symbols.rs` — `&` on the
  `extract_config()` call sites.
- `README.md`, `docs/ROADMAP.md` — config now live-reloads (no restart).
- `tests/test_e2e.rs` — new reload test.

No new files.

---

## Task 1: Make `Index.extract_config` reloadable

**Files:**
- Modify: `src/index/mod.rs`
- Modify: `src/server.rs`, `src/handlers/references.rs`, `src/handlers/symbols.rs`
  (call sites)

- [ ] **Step 1: Switch storage to `RwLock`**
  In `src/index/mod.rs`: change the field to `extract_config: RwLock<ExtractConfig>`;
  in `Default`, init `RwLock::new(ExtractConfig::default())`; change the accessor
  to `pub fn extract_config(&self) -> ExtractConfig { self.extract_config.read().unwrap().clone() }`;
  change the setter to replace: `*self.extract_config.write().unwrap() = cfg;`.
  Remove the `LazyLock` import if it is now unused (`RwLock` is already imported).

- [ ] **Step 2: Fix the call sites**
  The accessor now returns an owned `ExtractConfig`, so each caller that passed
  it as `&ExtractConfig` needs a `&`. Update every `index.extract_config()` /
  `self.index.extract_config()` argument to `&index.extract_config()` /
  `&self.index.extract_config()`: in `src/server.rs` (the two `build_index`
  calls and the three `extract_full_with` handlers), `src/handlers/references.rs`
  (the `extract_full_with` and `file_occurrences_with` calls), and
  `src/handlers/symbols.rs` (the `extract_full_with` call).

- [ ] **Step 3: Verify (behavior unchanged)**
  Run: `cargo check --all-targets` then `cargo clippy --all-targets -- -D warnings`
  then `cargo test --lib`
  Expected: all clean / pass. (On a memory-constrained box, linking the full
  binary can OOM; `clippy`/`check` do not link and are the gate here.)

- [ ] **Step 4: Commit**
  `git commit -m "refactor: make Index.extract_config reloadable (RwLock)"`

## Task 2: Reload config + re-index on watched config-file change

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Register explicit config-file watchers**
  In `initialized`, add two `FileSystemWatcher`s to the `watchers` vec:
  `GlobPattern::String("**/.clj-kondo/config.edn")` and
  `GlobPattern::String("**/.clj-pulse/config.edn")`, so dot-dir config files are
  watched even on clients that do not match them under `**/*.edn`.

- [ ] **Step 2: Detect config files early in the handler**
  In `did_change_watched_files`, add a `let mut config_changed = false;` next to
  the existing flags. In the per-event loop, before the `config::is_edn(&path)`
  branch, detect a config file: `path.file_name() == "config.edn"` and the
  parent directory name is `.clj-kondo` or `.clj-pulse`. When matched, set
  `config_changed = true;` and `continue;` (do not let it reach the Integrant
  branch).

- [ ] **Step 3: Restructure the post-loop rebuild**
  Change the gate from `if classpath_changed` to `if classpath_changed || config_changed`.
  Inside the spawned task, in order: if `config_changed`,
  `index.set_extract_config(settings::load(&root));`. If
  `source_paths_changed || config_changed`, rebuild the project
  (`build_index(&root, &source_paths, &index.extract_config())` then
  `merge_project_from`, as the existing `source_paths_changed` arm does). If
  `config_changed`, `client.log_message(MessageType::INFO, "clj-pulse: config reloaded")`.
  Then the existing `if classpath_changed { clear_libs(); resolve_and_index_libs(); … }`
  block, unchanged. `use crate::settings;` is already imported.

- [ ] **Step 4: Verify**
  Run: `cargo clippy --all-targets -- -D warnings` then `cargo test --lib`
  Expected: clean / pass.

- [ ] **Step 5: Commit**
  `git commit -m "feat: live-reload :lint-as config on watched-file change"`

## Task 3: End-to-end reload test

**Files:**
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Write the test**
  Add `test_e2e_lint_as_config_live_reload`, near the existing lint-as test.
  Use `setup_named("lint_as_project")`; `initialize`; `did_open` `src/app/core.clj`.
  Assert goto-def on the first `widget` (the usage) lands on the
  `defthing widget` line (mirror the existing lint-as test's assertions). Then
  overwrite `<root>/.clj-kondo/config.edn` with `{}` (via `std::fs::write`), send
  a `workspace/didChangeWatchedFiles` notification with that file's URI and
  `FileChangeType::CHANGED` (`typ` = 2), `wait_for_log("config reloaded")`, and
  assert goto-def on `widget` is now `null` (no lint-as → `widget` is not a def).
  Add a `did_change_watched_files` helper on `LspClient` if none exists (a
  `notify` with `{"changes":[{"uri":…,"type":2}]}`).

- [ ] **Step 2: Run the e2e suite**
  Run: `bb e2e` (≡ `cargo test --test test_e2e`).
  Expected: PASS, including the new test. (Memory-constrained box: prefix with
  `RUSTFLAGS="-C debuginfo=0"` to avoid the linker OOM.)

- [ ] **Step 3: Commit**
  `git commit -m "test: e2e live reload of :lint-as config"`

## Task 4: Documentation

**Files:**
- Modify: `README.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Update docs (use `/writing-clearly`)**
  In `README.md`'s Configuration section, replace the "reads them once at
  startup, so a config change needs a server restart" sentence with a note that
  clj-pulse watches the config files and reloads `:lint-as` on change, no
  restart needed. In `docs/ROADMAP.md`, update the clj-kondo-config entry: drop
  "live reload" from its Future list and note it is now supported.

- [ ] **Step 2: Commit**
  `git commit -m "docs: note live config reload"`

---

## Conventions

- `cargo clippy --all-targets -- -D warnings` + `cargo test --lib` before each
  commit; `bb e2e` after Task 3.
- Exact file paths; no full code inlined.
- DRY (reuse the existing rebuild path), YAGNI (single `config.edn` files only,
  project re-index only), frequent commits.
- Use `/writing-clearly` for prose.

---

## Implementation Summary (completed 2026-07-01)

**Status: done.** Tasks 1-4 implemented, verified, and committed
(`4b3daf1` → `66ead8d`).

**What shipped:**
- `src/index/mod.rs` — `extract_config` `OnceLock` → `RwLock`; accessor returns a
  clone; setter replaces. Callers gained a `&`.
- `src/server.rs` — explicit `.clj-kondo`/`.clj-pulse` `config.edn` watcher
  globs; a `config_changed` flag detected before the EDN branch; the post-loop
  spawn now runs on `classpath_changed || config_changed`, reloading
  `settings::load`, re-indexing the project, and logging `config reloaded`
  before the (optional) library branch.
- `tests/test_e2e.rs` — before/after reload test.
- `README.md`, `docs/ROADMAP.md` — config now reloads live.

**Issue found and fixed (unplanned):** the reload e2e test initially failed —
`widget` still resolved after the `:lint-as` mapping was removed.
`merge_project_from` removed only *stale files*; a file present in both scans
that lost a symbol (exactly the `:lint-as` case) left the old symbol lingering
in `symbols`, since the fqn-keyed insert never overwrote it. Fixed by dropping
each re-scanned namespace's previous symbols before inserting the new set
(`src/index/mod.rs`), with a unit test. This also hardens the existing
`source_paths_changed` rebuild path. Committed as `f705fec`.

**Verification:** `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt`
clean, 218 lib tests (incl. the new merge test), and the full `test_e2e` suite
75 passed / 0 failed / 2 ignored (incl. the new reload test), built with
`RUSTFLAGS="-C debuginfo=0"` to avoid the memory-constrained linker OOM. A
second-opinion codex review was run against `fb0728a..HEAD`.
