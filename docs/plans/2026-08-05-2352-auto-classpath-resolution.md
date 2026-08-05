# Automatic Classpath Resolution (deps.edn aliases) Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** clj-pulse resolves the full project classpath itself by running `clojure -A:dev:test -Spath` in the background, so library navigation covers alias deps (`:test`, `:dev`, …) out of the box instead of only whatever `.cp` file the user's last CLI run left in `.cpcache`.

**Tech Stack:** Rust, tokio (process spawning), tower-lsp, edn-format, clojure CLI (runtime dependency, optional).

---

## Design

### Problem

For deps.edn projects clj-pulse does no dependency resolution: it reads `.cpcache/*.cp` and picks the newest file that still resolves (`src/classpath.rs:10`). The indexed classpath is therefore "whatever alias combo the user ran last" — usually plain `:deps` — so go-to-definition into `:test`/`:dev` alias deps silently fails, and which deps are indexed is nondeterministic across sessions.

### Approach: a third, graduated indexing stage

Indexing stays progressive — each stage makes navigation better as soon as it lands:

1. **Project code** (existing task, unchanged).
2. **`.cpcache` classpath** (existing `resolve_and_index_libs`, unchanged) — instant, from whatever cache exists. Logs `library indexing complete` (marker kept, e2e sync depends on it).
3. **New:** in the same background task, sequentially after stage 2, spawn `clojure -A:dev:test -Spath` with cwd = project root and parse the classpath from **stdout**. If the resolved entry set differs from stage 2's, `clear_libs()` + re-index the authoritative set. Log `clj-pulse: full classpath indexed (N entries)` and fire `LibrariesChanged`.

The clojure CLI is itself the staleness check: `-Spath` hashes the deps.edn files + alias combo and, when a fresh `.cpcache/<hash>.cp` exists, prints it from cache without booting a JVM (bash-script speed). A JVM (and possibly dependency downloads) happens only when deps.edn changed or the combo was never resolved — exactly when resolution is wanted. The `.cp` file it writes also warms stage 2 for the next startup.

### Key decisions

- **Stdout, not the file watcher.** The `**/.cpcache/*.cp` watcher (`src/server.rs:444`) requires client-side file-watching support, which minimal clients and the raw e2e harness lack. Stage 3 consumes `-Spath` stdout directly. If the watcher *also* fires (the CLI wrote a new `.cp`), both paths converge on the same entry set and the jar cache makes the second pass cheap — benign.
- **Skip-if-same.** When stage 3's entry set equals stage 2's, skip re-indexing; still log the completion marker. Warm startups cost one subprocess.
- **Config:** `.clj-pulse/config.edn` gains `{:classpath {:enabled false :aliases [:dev :test]}}`. Defaults: `enabled = true`, `aliases = [:dev :test]` (clojure-lsp's precedent). Parsed separately from the `:lint-as` clj-kondo merge — clj-kondo config has no `:classpath` key.
- **Guards / graceful degradation:** stage 3 runs only for Clojure projects with a `deps.edn` (not let-go, not Leiningen-only) and only when enabled. Spawn failure (CLI missing) → WARNING log suggesting the manual `clojure -A:dev:test -Spath`; non-zero exit → log a stderr snippet; timeout (300 s) → child killed (`kill_on_drop(true)` — a dropped timeout future must not orphan a downloading JVM) + log. Every failure keeps the stage-2 result — today's behavior exactly.
- **Test kill-switch:** the env var `CLJ_PULSE_DISABLE_CLASSPATH_CLI` (any non-empty value) forces `enabled = false`. The e2e harness sets it by default — `setup_project()` fixtures contain a `deps.edn`, and without the switch every regular `bb e2e` test would spawn `clojure`, making the suite slow and environment-dependent. Tests that exercise stage 3 start the server without it.
- **Messaging:** before spawning, log `clj-pulse: resolving classpath via 'clojure -A:dev:test -Spath' (may download dependencies)...`. The existing "no classpath found" warning (`src/server.rs:343`) is emitted only when stage 3 is disabled or failed, and now teaches the aliased command.
- **Live reload for config only.** Editing `.clj-pulse/config.edn` (already watched, `src/server.rs:455`) re-runs stage 3 with the new aliases — this is the hook the VS Code extension's future alias-picker UI relies on (it just writes the config file). Only `.clj-pulse/config.edn` changes trigger this (not `.clj-kondo/config.edn`, which shares the watcher branch but has no `:classpath` key). Re-resolving on *deps.edn* saves is deliberately out of scope for v1; the user's next CLI run updates `.cpcache` and the existing watcher covers it.
- **Last-indexed entry set.** `Index` does not retain which classpath entries produced its library symbols, and re-reading `.cpcache` after `-Spath` would observe the file `-Spath` itself just wrote (always "equal", wrongly skipping re-index). The server therefore tracks the last-indexed entry set explicitly (an `Arc<Mutex<HashSet<PathBuf>>>` shared by the startup task and the config-watcher rerun): stage 2 initializes it, every stage-3 run compares against it and updates it after indexing.
- **No `JarCacheEntry::format_version` bump** — extractor output and `Symbol`/`NsMeta` layout are unchanged.

### Data flow (stage 3, happy path)

```
settings::classpath(root)                 -> ClasspathConfig { enabled, aliases }
classpath::resolve_via_cli(root, aliases) -> spawn `clojure -A:dev:test -Spath`
                                             parse stdout (split_paths, resolve
                                             relative entries against root,
                                             keep existing paths)
server task: compare set with stage-2 entries
  equal   -> log marker only
  differs -> index.clear_libs(); scanner::index_classpath_libs(root, entries, index)
             log marker; LibrariesChanged
```

### Shared shapes (tasks must agree)

```rust
// src/settings.rs
#[derive(Debug, Clone, PartialEq)]
pub struct ClasspathConfig {
    pub enabled: bool,        // default true
    pub aliases: Vec<String>, // default ["dev", "test"]; ":" stripped, qualified kept as "ns/name"
}
pub fn classpath(root: &Path) -> ClasspathConfig;

// src/classpath.rs
pub fn alias_arg(aliases: &[String]) -> Option<String>; // ["dev","test"] -> Some("-A:dev:test"); [] -> None
pub async fn resolve_via_cli(root: &Path, aliases: &[String]) -> Result<Vec<PathBuf>, String>;
// internal, for testability (stub program + short timeout injected by tests):
async fn resolve_with(program: &OsStr, root: &Path, aliases: &[String], timeout: Duration)
    -> Result<Vec<PathBuf>, String>;

// src/server.rs — resolve_and_index_libs return shape
struct ResolvedLibs {
    entries: Vec<PathBuf>, // classpath entries / dep dirs indexed
    extra: usize,          // library sources indexed outside `entries` (let-go pinned core)
}
// "indexed anything?" == !entries.is_empty() || extra > 0
```

`resolve_via_cli` is a thin wrapper over `resolve_with("clojure", …, 300 s)` and returns `Err` with a human-readable reason (spawn failure, non-zero exit + stderr snippet, timeout, empty output) — the server task only logs it. Tests inject a stub script path as `program` directly; no process-wide `PATH` mutation (parallel tests would race).

### Testing strategy

- **Unit (settings):** config parsing — missing file/key → defaults; `:enabled false`; alias vectors (simple and qualified keywords, lenient toward strings); malformed EDN → defaults.
- **Unit (classpath):** `alias_arg` formatting; `resolve_with` against a stub script passed as the program (unix-gated), covering success, non-zero exit, absolute/relative entry resolution, and a short-timeout run against a sleeping stub verifying the child is killed.
- **E2E, regular:** `:classpath {:enabled false}` project (kill-switch env var *unset*, so the config itself is what's under test) → the stage-2 outcome log arrives (for a fixture without `.cpcache` that is the "no classpath found" warning — `library indexing complete` is never logged on the zero-entry path) and no "resolving classpath" log ever appears.
- **E2E, ignored (`bb e2e-real`):** deps.edn with a dep only under a `:test` alias's `:extra-deps`; prime `.cpcache` with plain `clojure -Spath` (reproducing the original bug); assert goto-definition into the alias-only jar works after `full classpath indexed`.

### Docs

`docs/MEMORY.md`'s "clj-pulse will not start a JVM" principle is amended: never on the hot path; deps.edn projects may run the clojure CLI in the background, which itself skips the JVM when its cache is warm. README documents the config key. CLAUDE.md invariants updated.

---

## File Structure

- Modify: `src/settings.rs` — `ClasspathConfig`, `classpath(root)` parser + unit tests.
- Modify: `src/edn.rs` — small helper for reading a keyword vector, if needed by settings parsing.
- Modify: `src/classpath.rs` — `alias_arg`, `resolve_via_cli`, stdout-parsing (factored to share the split_paths/relative-resolution logic with `discover`) + unit tests.
- Modify: `src/server.rs` — stage-3 wiring in the library task; `resolve_and_index_libs` returns entries instead of a count; message reshuffle; config-watcher branch re-runs stage 3.
- Modify: `tests/test_e2e.rs` — disabled-mode test (regular) + alias-navigation test (ignored).
- Modify: `docs/MEMORY.md`, `README.md`, `CLAUDE.md`.

---

### Task 1: ClasspathConfig parsing in settings

**Files:**
- Modify: `src/settings.rs`
- Modify: `src/edn.rs` (only if a keyword-vector helper is needed)

- [x] **Step 1: Write failing unit tests**
  In `src/settings.rs` tests: `classpath()` on a temp dir with (a) no `.clj-pulse/config.edn` → `{enabled: true, aliases: ["dev","test"]}`; (b) `{:classpath {:enabled false}}` → disabled, default aliases; (c) `{:classpath {:aliases [:bench :ci/int]}}` → enabled, `["bench", "ci/int"]`; (d) `{:classpath {:aliases ["dev"]}}` (strings, lenient) → `["dev"]`; (e) malformed EDN → defaults.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --lib settings`
  Expected: FAIL (missing `ClasspathConfig`/`classpath`).

- [x] **Step 3: Implement**
  `ClasspathConfig` + `classpath(root)` per the shared shape above, using `edn.rs` helpers (`kw`, `get`) on the parsed top-level map. Keywords render as `name` / `namespace/name`; accept plain strings too. Any missing/unreadable piece falls back to the default for that field.

- [x] **Step 4: Run to verify pass**
  Run: `cargo test --lib settings`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "feat: parse :classpath config from .clj-pulse/config.edn"`

### Task 2: CLI classpath resolver

**Files:**
- Modify: `src/classpath.rs`

- [x] **Step 1: Write failing unit tests**
  `alias_arg`: `["dev","test"]` → `Some("-A:dev:test")`; `["ci/int"]` → `Some("-A:ci/int")`; empty slice → `None` (plain `-Spath`). `resolve_with` (unix-gated, `#[tokio::test]`): write a stub executable script in a temp dir and pass its path as the `program` argument (no `PATH` mutation — parallel tests share the environment); cover (a) stub prints `src:<abs dir path>` where the dir exists → entries returned with relative `src` resolved against root; (b) stub exits 1 with stderr → `Err` containing the stderr snippet; (c) stub prints nothing → `Err`; (d) stub sleeps past a ~1 s injected timeout → `Err` mentioning timeout, and the child no longer runs afterwards.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --lib classpath`
  Expected: FAIL.

- [x] **Step 3: Implement**
  Factor the entry-parsing loop out of `discover` (split_paths, resolve relative entries against root, keep only existing paths) and reuse it. `resolve_with(program, root, aliases, timeout)`: `tokio::process::Command::new(program)`, args `[alias_arg?, "-Spath"]`, `current_dir(root)`, `kill_on_drop(true)` (required — dropping the timed-out `output()` future must not orphan a downloading JVM), capture stdout/stderr, wrap in `tokio::time::timeout`. Trim stdout to its last non-empty line before parsing (the CLI may print download progress lines above the classpath). `resolve_via_cli` = `resolve_with("clojure", root, aliases, 300 s)`.

- [x] **Step 4: Run to verify pass**
  Run: `cargo test --lib classpath`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "feat: resolve full classpath via clojure -Spath subprocess"`

### Task 3: Wire stage 3 into the server's library task

**Files:**
- Modify: `src/server.rs`

- [x] **Step 1: Refactor `resolve_and_index_libs` to return `ResolvedLibs`**
  Return the `ResolvedLibs` struct from the shared-shapes section instead of `usize`. The let-go arm sets `entries = dirs` and `extra = lgx::index_letgo_core(...)` so a pinned project with no deps of its own still counts as "indexed something" (current behavior — a plain `Vec` would lose this). Callers replace `== 0` with the "indexed anything?" predicate. Preserve today's logging exactly.

- [x] **Step 2: Add stage 3 after stage 2 in the library task (`src/server.rs:334`)**
  Honor the kill-switch: `settings::classpath` treats a non-empty `CLJ_PULSE_DISABLE_CLASSPATH_CLI` env var as `enabled = false` (code comment: e2e fixtures contain deps.edn; without this every harness test spawns `clojure`). Guards: `config::project_kind == Clojure`, `root.join("deps.edn").exists()`, `enabled`. Store stage 2's entries in a shared `Arc<Mutex<HashSet<PathBuf>>>` (the last-indexed set). Then log the "resolving classpath via …" INFO message and call `resolve_via_cli`. On `Ok(entries)`: if the set equals the last-indexed set, just log `clj-pulse: full classpath indexed (N entries)`; else `index.clear_libs()`, `scanner::index_classpath_libs(&root, entries, &index)`, update the last-indexed set, log the marker, `LibrariesChanged`. On `Err(reason)`: WARNING log with the reason. Factor this whole flow into a helper taking `(root, index, client, last_indexed)` for reuse in Step 4.

- [x] **Step 3: Move the "no classpath found" warning**
  Emit it only when stage 2 found nothing AND stage 3 is disabled or errored (never while stage 3 might still succeed). Update its text to suggest `clojure -A:dev:test -Spath`.

- [x] **Step 4: Re-run stage 3 on `.clj-pulse/config.edn` changes**
  In the config-watcher branch (`src/server.rs` around line 659): only when the changed path is the `.clj-pulse` config (not `.clj-kondo`, which shares this branch), reload `settings::classpath` and, if enabled, run the Step-2 helper with the shared last-indexed set — comparing against it is what makes this correct: re-reading `.cpcache` here would see the `.cp` file `-Spath` itself just wrote and wrongly conclude "no change".

- [x] **Step 5: Set the kill-switch in the e2e harness**
  In `LspClient::start` (`tests/test_e2e.rs`), set `CLJ_PULSE_DISABLE_CLASSPATH_CLI=1` on the spawned server. Add a variant (e.g. `start_with_classpath_cli`) that leaves it unset for the tests that exercise stage 3.

- [x] **Step 6: Verify existing suites still pass**
  Run: `bb check && bb e2e`
  Expected: all green, with no `clojure` processes spawned by the regular suite (fixtures carry deps.edn; the kill-switch covers them).

- [x] **Step 7: Commit**
  `git commit -m "feat: index full alias classpath via background clojure -Spath"`

> Deviation: `start_with_classpath_cli` moved to Task 4's commit — clippy `-D warnings` rejects the not-yet-used helper.
> Deviation: Task 2's stub tests needed serialization (`STUB_LOCK`, a tokio Mutex) — parallel stub tests raced into ETXTBSY (fork inheriting another test's open write fd); fixed in its own commit.

> Deviation: codex P2 — startup and config-watcher stage-3 runs could overlap, letting a slow run with stale aliases win; added `ClasspathCliLock` (tokio Mutex) serializing runs, with the `:classpath` config read under the lock. Codex P1 (re-resolve on deps.edn saves) is the approved v1 scope cut, not a defect — left as future work.

### Task 4: E2E coverage

**Files:**
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Disabled-mode test (regular, no CLI needed)**
  Fixture project with `deps.edn`, no `.cpcache`, and `.clj-pulse/config.edn` containing `{:classpath {:enabled false}}`. Start via the *no-kill-switch* variant (the config file itself is under test), initialize, then `wait_for_log("no classpath found")` — the zero-entry stage-2 path logs this warning and never logs `library indexing complete` — and assert the collected log messages contain no "resolving classpath". (If the harness doesn't retain past logs, add a small accessor to `LspClient` for messages received so far.)

- [x] **Step 2: Alias-navigation test (ignored, modeled on `test_e2e_real_classpath_navigation`, `tests/test_e2e.rs:3237`)**
  deps.edn: `:paths ["src"]`, empty top-level `:deps`, `:aliases {:test {:extra-deps {org.clojure/data.json {:mvn/version "2.5.0"}}}}`. Prime the cache with plain `clojure -Spath` (main deps only — reproduces the bug). Start via the no-kill-switch variant, `wait_for_log("full classpath indexed")`, open a source file requiring `clojure.data.json`, assert goto-definition returns a `jar:file://…!/clojure/data/json.clj` URI and `workspace/textDocumentContent` serves it. Mark `#[ignore = "requires clojure CLI (downloads deps on first run)"]`.

- [x] **Step 3: Run**
  Run: `bb e2e` then `bb e2e-real`
  Expected: both green (e2e-real needs the clojure CLI + network).

- [x] **Step 4: Commit**
  `git commit -m "test: e2e coverage for alias classpath resolution and opt-out"`

> Deviation: codex round on Task 4 — the alias test now waits with the resolver's 300 s budget (`wait_for_log_within`) instead of the 20 s harness default, and `spawn` strips an inherited `CLJ_PULSE_DISABLE_CLASSPATH_CLI` when stage 3 must stay enabled.

### Task 5: Documentation

**Files:**
- Modify: `docs/MEMORY.md`, `README.md`, `CLAUDE.md`

- [x] **Step 1: Amend MEMORY.md**
  Update the "Stance: best effort, and never a JVM at startup" section: the fixed principle becomes "never a JVM on the hot path". deps.edn projects run `clojure -A<aliases> -Spath` in a background task; the CLI skips the JVM when its cache is warm; failures degrade to `.cpcache` reading. Update the resolver table row for deps.edn. Note the opt-out.

- [x] **Step 2: Document the config key in README**
  `{:classpath {:enabled true :aliases [:dev :test]}}` — defaults, what disabling means, that the CLI may download deps on first resolve.

- [x] **Step 3: Update CLAUDE.md invariants**
  Add: deps.edn classpath = stage-2 `.cpcache` read then authoritative background `clojure -Spath` (config-gated); `library indexing complete` and `full classpath indexed` are the two e2e sync markers for library indexing.

- [x] **Step 4: Verify and commit**
  Run: `bb check`
  Expected: green.
  `git commit -m "docs: document automatic classpath resolution and config"`

---

## Completion Summary (2026-08-06)

**Status: COMPLETED.** All five tasks implemented, reviewed (codex round per task), and verified: `bb check`, `bb e2e` (81 tests), `bb e2e-real` (3 tests incl. the alias-navigation scenario), and `bb e2e-nvim` (real Neovim client) all green.

**What was implemented:** graduated classpath indexing for deps.edn projects. Stage 2 (`.cpcache` read) is unchanged; a new stage 3 runs `clojure -A:dev:test -Spath` in the background (tokio subprocess, 300 s timeout, `kill_on_drop`), parses stdout, and re-indexes when the authoritative classpath differs from the last-indexed entry set. Config: `.clj-pulse/config.edn` `{:classpath {:enabled … :aliases […]}}` (defaults on, `[:dev :test]`, empty vector = no aliases), live-reloaded via the existing config watcher. All failures degrade to stage-2 behavior. Env kill-switch `CLJ_PULSE_DISABLE_CLASSPATH_CLI` keeps the regular e2e suite subprocess-free.

**Issues encountered:**
- Flaky ETXTBSY in the stub-spawning resolver tests (parallel fork inheriting an open write fd) — fixed by serializing them behind a tokio `STUB_LOCK`.
- Codex-found race: overlapping startup/watcher stage-3 runs could let stale aliases win — fixed with `ClasspathCliLock` + reading the config under the lock.

**Deviations (gathered):**
- Task 1: explicitly empty `:aliases []` is honored (codex P2) rather than falling back to defaults.
- Task 3: `start_with_classpath_cli` landed in Task 4's commit (clippy `-D warnings` rejects unused helpers); stub-test serialization added; stage-3 run serialization added (codex P2). Codex P1 (re-resolve on deps.edn saves) is the approved v1 scope cut — future work.
- Task 4: alias test waits with the resolver's 300 s budget (`wait_for_log_within`); harness strips an inherited kill-switch var for stage-3 tests.

**What the plan could have specified better:** concurrency. The plan pinned the last-indexed-set comparison but not serialization of stage-3 runs, and its stub-test design (PATH injection, no fork/exec-interference awareness) needed rework for parallel test execution.
