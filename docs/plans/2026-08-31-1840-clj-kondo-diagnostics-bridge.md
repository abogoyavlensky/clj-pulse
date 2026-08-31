# clj-kondo Diagnostics Bridge Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status: COMPLETE** (all 12 tasks; see the summary at the end).

**Goal:** Publish clj-kondo's full diagnostic set alongside clj-pulse's native lints by spawning a `clj-kondo` binary per lint pass, with clear ownership rules, live-reloadable settings, status transparency in the VS Code extension, and background classpath cache warming.

**Tech Stack:** Rust (tower-lsp, tokio, serde_json), clj-kondo CLI (external binary, verified against v2026.08.04), TypeScript (vscode-languageclient 9.x) for `../clojure-pulse-vscode`.

**Repos:** This plan spans two repositories: `clj-pulse` (server, this repo) and `../clojure-pulse-vscode` (extension). Tasks 1–8 are server-side, Tasks 9–10 extension-side, Task 11 docs, Task 12 the cross-repo verification.

---

## Design

### Approach

clj-pulse spawns `clj-kondo` from PATH (or a configured path) on the existing
diagnostics paths — didOpen, didSave, and the 300 ms-debounced didChange —
feeding the live buffer over stdin and parsing JSON findings into LSP
diagnostics. clj-kondo is never embedded, never run as a babashka pod (pod
mode cannot lint stdin — it hangs — and serializes requests), and never run by
the extension. When the binary is absent or disabled, behavior is exactly
today's: native lints only, one log line, no popups.

### Verified clj-kondo facts (v2026.08.04) — implementers rely on these

- Invocation: `clj-kondo --lint - --filename <ABSOLUTE real path> --config '{:output {:format :json}}'` with the buffer on stdin.
- Exit codes: `0` clean, `2` warnings, `3` errors — **all three are success**.
  `1` = crash (empty stdout, Java stack trace on stderr). Unknown flags are
  silently ignored (exit 0), so never validate flags via exit codes.
- JSON: `{"findings": [...], "summary": {...}}`. A finding:
  `{"type":"invalid-arity","level":"error","filename":"...","row":11,"col":5,"end-row":11,"end-col":16,"langs":[],"message":"..."}`.
  Rows/cols are 1-based. **`end-row`/`end-col` are optional** (absent on
  bracket-mismatch `syntax` findings). Extra per-linter keys (`ns`, `name`,
  `duplicate-ns`) are ignored.
- The `.clj-kondo` config/cache dir is resolved by walking up **from the
  linted file's absolute path** (from `--filename` for stdin), not from cwd —
  so a monorepo subproject's own `.clj-kondo` wins over the root's, matching
  `projects::detect` nesting with zero cwd juggling.
- Stdin lints **write** the cache (`.clj-kondo/.cache/...`) when a
  `.clj-kondo` dir exists, so cross-file lints (`invalid-arity`,
  `unresolved-var`) self-warm as files are opened/edited. Without the dir,
  cross-file lints are silently absent; clj-kondo never creates the dir.
- Never pass `--cache-dir`: without a resolvable config dir it NPE-crashes
  (exit 1). We pass no cache/config flags at all — policy (a): users
  `mkdir .clj-kondo` per clj-kondo's own convention to unlock cross-file lints.
- `:linters` levels and `:lint-as` from `.clj-kondo/config.edn` apply to stdin
  lints automatically. `namespace-name-mismatch` keys on `--filename`, so the
  real path must be passed.
- Latency: 20–70 ms for normal files, ~0.5 s for a 4000-line file, ~54 MB RSS
  per spawn. Ten concurrent lints against one cache are safe (lock file).

### Ownership rule (native vs clj-kondo)

When a pass's clj-kondo run **succeeds**, clj-kondo owns every code it can
emit: native diagnostics with codes `unresolved-namespace`,
`unused-namespace`, or `duplicate-require` are dropped for that pass (the
add-require lightbulb keeps working — `code_action.rs` matches incoming
diagnostics by *code*, not source). Since those are the only native codes
today, a successful kondo pass publishes kondo findings only; the rule is
stated per-code so future native lints that kondo doesn't cover survive. When
kondo is absent, disabled, times out, or fails, the native set is published
unchanged. One publish per pass: compute native, await kondo, merge, publish
once — no squiggle flicker.

### Diagnostic mapping (exact, tasks must agree)

| kondo | LSP |
|---|---|
| `type` | `code` (string) |
| `level` `error`/`warning`/`info` | `ERROR`/`WARNING`/`INFORMATION` |
| `row`,`col` (1-based) | `range.start` = (row−1, col−1) |
| `end-row`,`end-col` present | `range.end` = (end-row−1, end-col−1) — kondo's end-col is 1-based exclusive, e.g. `other/thing` at col 6/end-col 17 → LSP chars 5..16 |
| `end-row`/`end-col` absent | `range.end` = (row−1, col) — one-char range |
| — | `source` = `"clj-kondo"` |
| `type` starting with `unused-` | tag `UNNECESSARY` |
| `type` = `deprecated-var` | tag `DEPRECATED` |

`langs` is dropped in v1 (no message suffix — YAGNI). Findings for files other
than the linted one don't occur with `--lint -`; `filename` is ignored.

### File gating

kondo lints only `.clj`/`.cljs`/`.cljc`/`.bb` buffers. `.lg` (let-go has a
different core — false positives) and `.edn` are native-only. `.bb` passes
`--lang clj` (kondo can't derive it from the extension).

### Settings & discovery

`:kondo` rides the existing two-layer config merge (file layer first, editor
layer wins per key — the `projects.rs` `resolve` pattern):

- `.clj-pulse/config.edn`: `{:kondo {:enabled true :path "clj-kondo"}}`
- VS Code: `clojurePulse.kondo.enabled` (default `true`), `clojurePulse.kondo.path`
  (default `"clj-kondo"`), sent in `initializationOptions` as
  `{"projects": [...], "kondo": {"enabled": ..., "path": ...}}` and pushed on
  change in the `{"clojurePulse": {...}}` envelope like `clojurePulse.projects`.
  Live reload, no restart. **Replace semantics:** the server replaces its
  stored editor layer on every push (`server.rs` `did_change_configuration`),
  so the extension always sends the complete `{projects, kondo}` object
  whenever *either* setting changes — a partial push would erase the other
  key's overrides.
- Defaults: `enabled: true` means *use when found*. Discovery = spawn
  `<path> --version` (2 s timeout); success + parseable `clj-kondo v<X>` line
  = found. `Command::new` resolves bare names via PATH — no hand-rolled which.
  `enabled: false` → never probe, never spawn.
- Env kill-switch `CLJ_PULSE_DISABLE_KONDO` (non-empty) forces
  `enabled: false`, twin of `CLJ_PULSE_DISABLE_CLASSPATH_CLI`. The e2e harness
  sets it by default so no fixture depends on a host clj-kondo.
- Config re-probe on: `initialize`, `did_change_configuration`, and the
  existing `.clj-pulse/config.edn` file-watch reload.

### Transparency

- Startup/reload log lines (they double as e2e `wait_for_log` sync points):
  - `clj-kondo <version> found (<command>) — linting: clj-kondo + native`
  - `clj-kondo not found — linting: native lints only`
  - `clj-kondo disabled — linting: native lints only`
- New custom notification `clojurePulse/lintStatus` (the `LibrariesChanged`
  pattern), params:
  `{"engine": "kondo+native" | "native", "version": string?, "warming": bool}`.
  Sent after every probe and at warming begin/end. The extension renders it as
  an extra tooltip line on the existing status-bar item — never flips the item
  into the starting/spinner state (that state means "server unavailable";
  warming doesn't degrade anything). Warming visibility comes from LSP
  workDoneProgress, which vscode-languageclient renders as a transient
  status-bar spinner (ProgressLocation.Window). No popups anywhere.

### Classpath cache warming (last, separately landable)

After a project's library entries are (re)built from a resolved classpath
(stage 2 or stage 3), if kondo is enabled+found **and** a `.clj-kondo` dir
exists at the project dir or an ancestor up to the workspace root, spawn
`clj-kondo --lint <classpath> --dependencies --parallel` in the background
(cwd = project dir). `--dependencies` populates the cache without findings;
kondo skips already-cached JARs on later runs. Guards: entry-set comparison
vs the last warmed set per project; its own serialization mutex (one warm at a
time); 10-minute timeout with the classpath.rs process-group kill; the
stage-3 stale-generation guard pattern; any failure logs a warning and
degrades silently. Progress via workDoneProgress
(`"Linting classpath (clj-kondo): <project>"`); `lintStatus` carries
`warming: true/false` around it. **No `--copy-configs` in v1** (writes into
the repo).

### Testing strategy

- Unit: pure JSON→`Vec<Diagnostic>` parser; merge/ownership; `:kondo` config
  parse+merge; runner exit-code/timeout semantics via a scripted fake binary.
- e2e (`bb e2e`): a committed fake `clj-kondo` shell script emits canned JSON;
  `LspClient` gains kondo-enabled constructors that clear the kill-switch and
  prepend the fake's dir to `PATH`. Tests: kondo diagnostics published with
  `source: "clj-kondo"` and native codes ceded; native-only when absent;
  disabled via `.clj-pulse/config.edn`; add-require action still binds to a
  kondo `unresolved-namespace` diagnostic; warming invoked with
  `--dependencies` (fake records argv).
- `bb e2e-nvim` re-run (client-visible protocol change: new notification —
  must not break a real client; unknown notifications are ignored).
- Extension: unit tests for `statusPresentation` tooltip states and the kondo
  settings mapping; `make check`.
- Final cross-repo verification with the real kondo binary (Task 12).

## File Structure

**clj-pulse (create):**
- `tests/fixtures/fake-clj-kondo/clj-kondo` — executable fake binary for e2e.
- `tests/fixtures/kondo_project/` — fixture (deps.edn, src file, `.clj-kondo/` dir).

**clj-pulse (modify):**
- `src/kondo.rs` — stays the clj-kondo boundary: add JSON→Diagnostic parsing, the async runner, the `--version` probe, `:kondo` config parsing.
- `src/diagnostics.rs` — add the merge/ownership function.
- `src/settings.rs` — load `:kondo` from `.clj-pulse/config.edn`.
- `src/server.rs` — KondoState field, probe + log + `lintStatus` wiring, lint pipeline integration, warming task.
- `tests/test_e2e.rs` — harness kill-switch + kondo constructors + new tests.
- `README.md`, `docs/ROADMAP2.md`, `CLAUDE.md` — docs.

**clojure-pulse-vscode (modify):**
- `package.json` — `clojurePulse.kondo.enabled` / `clojurePulse.kondo.path`.
- `src/extension.ts` — kondo settings → initializationOptions + change push; `lintStatus` subscription.
- `src/statusBar.ts` — lint state in `StatusDetail` + tooltip line.
- `src/test/` — statusPresentation cases.
- `README.md`, `CHANGELOG.md`.

---

### Task 1: JSON→Diagnostic parser (`src/kondo.rs`)

**Files:** Modify: `src/kondo.rs`

- [x] **Step 1: Write failing unit tests**
  `parse_findings(json: &str) -> Option<Vec<Diagnostic>>` per the mapping
  table: severity, string code, `source: "clj-kondo"`, 1-based→0-based, the
  `other/thing` col 6/end-col 17 → chars 5..16 example, missing
  `end-row`/`end-col` → one-char range, `unused-namespace` → UNNECESSARY tag,
  `deprecated-var` → DEPRECATED tag, unparseable/empty input → `None`,
  `{"findings":[]}` → `Some(vec![])`.
- [x] **Step 2: Run** `cargo test kondo` — expect FAIL.
- [x] **Step 3: Implement** with serde_json (already a dependency). Pure, no IO.
- [x] **Step 4: Run** `cargo test kondo` — expect PASS.
- [x] **Step 5: Commit** `git commit -m "Parse clj-kondo JSON findings into LSP diagnostics"`

### Task 2: Subprocess runner + version probe (`src/kondo.rs`)

**Files:** Modify: `src/kondo.rs`

- [x] **Step 1: Write failing tests** (tokio tests, like `classpath.rs`'s)
  using shell-script fakes in tempdirs: success with exit 3 + JSON on stdout →
  `Ok(diags)`; exit 1/empty stdout → `Err`; sleeping child → timeout `Err`
  and the child killed (marker-file assertion, copy
  `resolve_via_cmd_kills_child_on_timeout`); `probe_version` parses
  `clj-kondo v2026.08.04` → `Some("v2026.08.04")`, missing binary → `None`.
- [x] **Step 2: Run** `cargo test kondo` — expect FAIL.
- [x] **Step 3: Implement**
  `lint(bin: &str, source: &str, abs_path: &Path, timeout: Duration) -> Result<Vec<Diagnostic>, String>`:
  spawn `<bin> --lint - --filename <abs_path> --config '{:output {:format :json}}'`
  (append `--lang clj` for `.bb`), write source to stdin, close it, await with
  timeout. Follow `classpath.rs::resolve_via_cmd` for `kill_on_drop`,
  process-group setup, and the unix/windows group-kill on timeout (extract a
  shared helper only if it stays clean — a small duplicate is acceptable; the
  stdin feed is the difference). Treat exit 0/2/3 + parseable JSON as success.
  `probe_version(bin) -> Option<String>` runs `--version` with a 2 s timeout.
- [x] **Step 4: Run** `cargo test kondo` — expect PASS.
- [x] **Step 5: Commit** `git commit -m "Spawn clj-kondo per lint with timeout and group kill"`

> Deviation: `kondo::warm` (Task 7's spawner) landed here rather than in Task 7 —
> it is three lines over the same `run` helper, and splitting it would have
> duplicated the process-group/timeout plumbing.
> Codex review: two must-fix findings, both real and fixed in `b523457` — `warm`
> and `probe_version` treated a non-zero child exit as success. Its third
> finding (lint must run with cwd = the owning project dir, because clj-kondo
> resolves `.clj-kondo` from cwd) was **wrong**: verified empirically against
> clj-kondo v2026.05.25 that both the config *and* the cache dir resolve by
> walking up from `--filename`, even when cwd is a different project. No change.

### Task 3: `:kondo` settings, two layers + kill-switch

**Files:** Modify: `src/kondo.rs` (EDN+JSON parse), `src/settings.rs` (file load), `src/server.rs` (editor layer plumb)

- [x] **Step 1: Write failing tests** for
  `KondoOverride { enabled: Option<bool>, path: Option<String> }`: EDN parse
  from `{:kondo {...}}`, JSON parse from `{"kondo": {...}}` (both the bare
  initializationOptions object and the `{"clojurePulse": {...}}` envelope —
  mirror `projects::parse_json`'s tolerance), merge (file first, editor wins
  per key, defaults `enabled: true`, `path: "clj-kondo"`), kill-switch forces
  disabled (injectable like `resolve_with_disable`).
- [x] **Step 2: Run** `cargo test` — expect FAIL.
- [x] **Step 3: Implement.** Resolution result:
  `KondoConfig { enabled: bool, path: String }`.
- [x] **Step 4: Run** `cargo test` — expect PASS.
- [x] **Step 5: Commit** `git commit -m "Add :kondo config (file + editor layers, env kill-switch)"`

> Deviation: the `src/server.rs` editor-layer plumbing moved into Task 4's
> commit. Stored but never read, the field trips `dead_code` under clippy
> `-D warnings`; landing it beside its first reader keeps every commit green.

### Task 4: Probe, log lines, `lintStatus` notification (`src/server.rs`)

- [x] **Step 1: Implement** `KondoState { config: KondoConfig, found: Option<String /*version*/> }`
  behind a mutex on `Backend`. A `probe_and_announce` fn: resolve config
  (file + editor + kill-switch), run `probe_version` when enabled, store,
  emit exactly one of the three log lines from the design, and send
  `clojurePulse/lintStatus` (struct with `METHOD = "clojurePulse/lintStatus"`,
  the `LibrariesChanged` pattern; params
  `{engine, version?, warming: false}`). Call it from `initialize` (after
  editor config lands), `did_change_configuration` (which now parses **both**
  `projects` and `kondo` from the envelope before replacing the stored
  layers), and the `.clj-pulse/config.edn` watch/reload branch. When a probe
  changes the effective engine (enabled/found transition either way), re-lint
  every open document so the toggle applies immediately, and hand the
  transition to the warming scheduler (Task 7).
- [x] **Step 2: Verify** `bb check` passes (unit-level behavior is covered in
  Task 6's e2e; keep this task wiring-only).
- [x] **Step 3: Commit** `git commit -m "Probe clj-kondo and announce lint engine via log + lintStatus"`

> Deviation: this commit also carries Task 3's `server.rs` editor-layer
> plumbing (see Task 3's note).
> Codex review: one must-fix, real and fixed in `1e26fd9` — overlapping config
> changes each spawned an independent probe, so a slow one carrying obsolete
> settings could land last. Probes are now serialized under a lock, the way
> `ClasspathCliLock` serializes stage-3 runs.

### Task 5: Lint pipeline integration (ownership merge)

**Files:** Modify: `src/diagnostics.rs` (merge fn + tests), `src/server.rs` (`lint_and_publish`, did_change debounce closure)

- [x] **Step 1: Write failing unit tests** for
  `merge(native: Vec<Diagnostic>, kondo: Result<Vec<Diagnostic>, String>) -> Vec<Diagnostic>`:
  kondo `Ok` → kondo findings + native minus codes
  {`unresolved-namespace`, `unused-namespace`, `duplicate-require`}; kondo
  `Err` → native unchanged.
- [x] **Step 2: Run** `cargo test diagnostics` — expect FAIL.
- [x] **Step 3: Implement** the merge; then in `server.rs` extend both publish
  paths (`lint_and_publish` and the debounce closure): compute native; if
  kondo enabled+found and the file qualifies (`.clj`/`.cljs`/`.cljc`/`.bb` —
  not `.lg`/`.edn`), `kondo::lint` with a 2 s timeout under a global
  `tokio::sync::Semaphore(4)`; after the await re-check
  `documents.current_version(&uri)` and abort if superseded; merge; publish
  once with `Some(version)`.
- [x] **Step 4: Run** `cargo test` and `bb check` — expect PASS.
- [x] **Step 5: Commit** `git commit -m "Publish merged native + clj-kondo diagnostics"`

> Codex review: three must-fix findings, all real and fixed in `66a0556` —
> (1) a `.clj-kondo/config.edn` change altered what clj-kondo reports but never
> re-linted open buffers; (2) a lint queued against a since-retired engine could
> publish over the current one, which the document-version check alone misses
> because a settings-triggered re-lint reuses the same version; (3)
> `probe_and_announce` compared only active-vs-inactive, so swapping `:path`
> between two working binaries skipped the re-lint.

### Task 6: e2e coverage with a fake clj-kondo

**Files:** Create: `tests/fixtures/fake-clj-kondo/clj-kondo`, `tests/fixtures/kondo_project/`; Modify: `tests/test_e2e.rs`

- [x] **Step 1: Write the fake** (committed executable, `#!/bin/sh`):
  `--version` → print `clj-kondo v0.0.0-fake`; `--dependencies` present →
  append `"$@"` to `$FAKE_KONDO_LOG` (when set) and exit 0; otherwise read
  stdin and, if it contains the marker `kondo-finding-here`, print a canned
  finding JSON (fixed row/col, `"type":"unresolved-symbol"`,
  `"level":"error"`) and exit 3, else print `{"findings":[]}` and exit 0.
- [x] **Step 2: Harness:** `LspClient::start` sets `CLJ_PULSE_DISABLE_KONDO=1`
  (mirroring the classpath kill-switch) so every existing test is untouched;
  add `start_with_kondo(project_root)` that clears it and prepends
  `tests/fixtures/fake-clj-kondo` to the child's `PATH`.
- [x] **Step 3: Write failing e2e tests:**
  (a) open a file containing the marker → `wait_for_log("clj-kondo v0.0.0-fake found")`,
  then a `publishDiagnostics` with one `source: "clj-kondo"` finding and **no**
  native `unresolved-namespace` duplicates;
  (b) same fixture via plain `start` → log `linting: native lints only` and
  native diagnostics only;
  (c) fixture with `.clj-pulse/config.edn` `{:kondo {:enabled false}}` →
  `clj-kondo disabled` log, native only;
  (d) code_action at a kondo `unresolved-namespace` diagnostic still returns
  the add-require fix (fake emits that code for this test's marker);
  (e) live toggle: overwrite the fixture's `.clj-pulse/config.edn` with
  `{:kondo {:enabled false}}` (didChangeWatchedFiles) → `wait_for_log`
  (`clj-kondo disabled`) and the re-lint publishes native-only diagnostics.
- [x] **Step 4: Run** `bb e2e` — expect the new tests PASS, all old tests PASS.
- [x] **Step 5: Commit** `git commit -m "e2e: clj-kondo bridge via fake binary on PATH"`

> Deviation: `.clj-kondo/` and `.clj-pulse/` are created inside the copied temp
> fixture at test time rather than committed — the repo's `.gitignore` excludes
> `.clj-kondo`, and re-including it would have meant gitignore surgery for two
> empty-ish files. The fake grew a second marker (`kondo-unresolved-ns-here`)
> so test (d) gets an `unresolved-namespace` finding, and it exits 2 for
> warnings and 3 for errors so both success codes are exercised.
> Codex review: one finding, fixed in the Task 7 commit — `clear_notifications`
> dropped only the stash, leaving in-flight messages able to satisfy the next
> wait. It now drains the channel first.

### Task 7: Classpath cache warming

**Files:** Modify: `src/server.rs`, `src/kondo.rs`; Modify: `tests/test_e2e.rs`

- [x] **Step 1: Implement** `kondo::warm(bin, classpath: &str, project_dir, timeout)`
  spawning `--lint <classpath> --dependencies --parallel` (10 min timeout,
  same kill pattern). In `server.rs`, after a project's library entries are
  applied (the `rebuild_libs` call sites for stage-2/stage-3 results): gate on
  kondo enabled+found ∧ a `.clj-kondo` dir at the project dir or an ancestor
  up to the workspace root ∧ entry set differs from the last warmed set
  (per-project map); run under a dedicated warm mutex with the
  stale-generation guard; workDoneProgress
  `"Linting classpath (clj-kondo): <project>"`; `lintStatus` with
  `warming: true` before / `false` after; log
  `clj-kondo cache warm complete: <project>` (or a warning on failure).
  Two triggers share this path: (1) a project's library entries being
  (re)built, and (2) a probe transition to enabled+found (Task 4) — which
  walks the already-resolved projects and schedules any whose entry set is
  unwarmed, so enabling kondo live doesn't require a re-index to warm.
- [x] **Step 2: Write failing e2e test:** kondo-enabled client on a fixture
  with a fake `.cpcache` (reuse the stage-2 fixture pattern) and
  `FAKE_KONDO_LOG` set → `wait_for_log("clj-kondo cache warm complete")`,
  then assert the log file contains `--dependencies` and the classpath.
- [x] **Step 3: Run** `bb e2e` — expect PASS.
- [x] **Step 4: Commit** `git commit -m "Warm clj-kondo cache from resolved classpath in background"`

> Deviation: warming hangs off `apply_project_diff` and the startup indexing
> task rather than each individual `rebuild_libs` call site — several of those
> are sync functions holding a `std::sync::Mutex` and cannot await. The
> warmed-set guard makes the call idempotent, so the coarser placement covers
> the same triggers. A second e2e test was added for the `.clj-kondo`-absent
> case, which is the gate most likely to regress silently.
> Codex review: one must-fix, real and fixed in `3880e97` — the two triggers
> race by design and the post-lock re-check covered only the classpath, so both
> could run the same minutes-long scan.

### Task 8: Server docs & roadmap

**Files:** Modify: `README.md`, `docs/ROADMAP2.md`, `CLAUDE.md`

- [x] **Step 1:** README: a **Linting** section — the two tiers (native
  always-on: `unresolved-namespace`/`unused-namespace`/`duplicate-require`;
  clj-kondo when present: its full linter set + the user's
  `.clj-kondo/config.edn`), the ownership rule, `:kondo {:enabled :path}`
  config, and the `mkdir .clj-kondo` note for cross-file lints. Use
  /writing-clearly.
- [x] **Step 2:** ROADMAP2 §1.1: mark the bridge done, note warming shipped and
  `--copy-configs` deliberately deferred. CLAUDE.md: add
  `CLJ_PULSE_DISABLE_KONDO` to the invariants (harness sets it; kondo tests
  opt in via `start_with_kondo`).
- [x] **Step 3: Commit** `git commit -m "Document clj-kondo linting bridge"`

### Task 9: Extension settings + initializationOptions

**Files (in `../clojure-pulse-vscode`):** Modify: `package.json`, `src/extension.ts`; Test: existing suite location under `src/test/`

- [x] **Step 1:** Declare `clojurePulse.kondo.enabled` (boolean, default
  `true`, description: "Use clj-kondo for diagnostics when the binary is
  found") and `clojurePulse.kondo.path` (string, default `"clj-kondo"`; bare
  name = resolved from PATH by the server, or an absolute path).
- [x] **Step 2:** A `kondoServerConfig()` beside `projectsServerConfig()`;
  include `kondo` in `initializationOptions` (`{projects, kondo}`). Merge the
  two existing change triggers into one: when
  `event.affectsConfiguration("clojurePulse.projects")` **or**
  `("clojurePulse.kondo")`, push the **complete** `{projects, kondo}` object
  in the `{clojurePulse: {...}}` envelope (`extension.ts:646`) — the server
  replaces its whole editor layer per push, so partial payloads would erase
  the other key.
- [x] **Step 3: Run** `make check` — expect PASS.
- [x] **Step 4: Commit** `git commit -m "Add clojurePulse.kondo settings, plumb to server"`

> Codex review: one must-fix, real and fixed in `574a5cd` — `get()` returns the
> contributed default for an untouched setting, so the extension sent
> `{enabled: true, path: "clj-kondo"}` as explicit editor overrides and
> silently beat a project's own `.clj-pulse/config.edn` `:kondo`. Now only
> user-set values are sent (`src/configValue.ts`). Also dropped
> `scope: "resource"` from both keys: the server holds one editor layer for the
> whole workspace, so folder-scoped values were never resolvable.

### Task 10: Extension lintStatus → status-bar tooltip

**Files (in `../clojure-pulse-vscode`):** Modify: `src/statusBar.ts`, `src/extension.ts`, `README.md`, `CHANGELOG.md`; Test: statusPresentation unit tests

- [x] **Step 1: Write failing tests** for `statusPresentation` with a new
  `StatusDetail.lint?: {engine: "kondo+native" | "native", version?: string, warming?: boolean}`:
  running-state tooltip gains one line —
  `Linting: clj-kondo + native (v2026.08.04)`, `Linting: native lints only`,
  and the warming suffix `— warming dependency cache…`; absent `lint` (older
  server) → tooltip unchanged. The item's icon/state never changes for
  warming.
- [x] **Step 2: Run** `make test` — expect FAIL.
- [x] **Step 3: Implement:** subscribe to `clojurePulse/lintStatus` next to
  the `librariesChanged` subscription (`extension.ts:457`), cache the last
  payload, re-render the status bar on receipt and on state changes.
- [x] **Step 4:** README **Linting** section (mirror of the server's, plus the
  settings) and a CHANGELOG entry under Unreleased, noting older servers
  simply never send `lintStatus`.
- [x] **Step 5: Run** `make check` — expect PASS.
- [x] **Step 6: Commit** `git commit -m "Show lint engine status from clojurePulse/lintStatus"`

### Task 11: Real-binary smoke test (optional, ignored)

**Files:** Modify: `tests/test_e2e.rs`, `bb.edn`

- [x] **Step 1:** An `#[ignore]`d e2e test (pattern: `e2e-real`) that skips
  with a message unless a real `clj-kondo` is on PATH: open a fixture file
  with a genuine unresolved symbol, assert a `source: "clj-kondo"` diagnostic
  arrives. Add `bb e2e-real-kondo` running
  `cargo test --test test_e2e real_kondo -- --ignored`.
- [x] **Step 2: Run it** on a machine with clj-kondo installed (CI box or
  maintainer's laptop): `bb e2e-real-kondo` — expect PASS (or SKIP w/o binary).
- [x] **Step 3: Commit** `git commit -m "Ignored e2e against a real clj-kondo binary"`

> Deviation: the harness's `enable_kondo: bool` became a three-way `Kondo`
> enum (`Off` / `Fake` / `Real`), since the real-binary test must leave PATH
> alone rather than prepend the fake. The test asserts on `unresolved-symbol`,
> a code no native lint can produce, so a skipped or inert clj-kondo fails it
> rather than passing vacuously.

### Task 12: Final cross-repo verification

Run everything, in this order, and fix regressions before calling the work done:

- [x] **Step 1 (server):** `bb check` — fmt + clippy `-D warnings` + all unit
  tests PASS.
- [x] **Step 2 (server):** `bb e2e` — all e2e including Tasks 6/7 tests PASS.
- [x] **Step 3 (server):** `bb e2e-nvim` — a real editor client tolerates the
  new `lintStatus` notification; definition/diagnostics still work.
- [x] **Step 4 (extension):** in `../clojure-pulse-vscode`: `make check`
  (lint + compile + tests, including the new statusPresentation cases) PASS.
- [x] **Step 5 (integrated, real kondo):** with a real `clj-kondo` on PATH:
  `bb e2e-real-kondo` PASS; then `bb e2e-calva` (real VS Code + Calva under
  Xvfb) still green. If the environment lacks Xvfb/clj-kondo, record which
  steps were skipped and note the manual check for the maintainer's Calva
  setup: open a Clojure project with kondo installed, confirm kondo-sourced
  squiggles, the status-bar tooltip's `Linting:` line, `mkdir .clj-kondo` →
  cross-file arity warnings appear, and `clojurePulse.kondo.enabled: false`
  reverting to native-only live.
- [x] **Step 6: Commit** any fixes; both repos' working trees clean.

---

## Completion summary

All 12 tasks are implemented, reviewed, and verified. 12 commits in `clj-pulse`
and 3 in `clojure-pulse-vscode`, both on branch `clj-kondo-diagnostics`, both
working trees clean.

**What shipped.** clj-pulse spawns a `clj-kondo` binary once per lint pass on
the didOpen, didSave, and 300 ms-debounced didChange paths, feeding it the
unsaved buffer on stdin and publishing its findings as LSP diagnostics with
`source: "clj-kondo"`. A successful run owns the three codes the native lints
also emit, so nothing is squiggled twice; every failure mode (absent, disabled,
timed out, crashed, unparseable output) degrades to the native set unchanged,
and each pass publishes exactly once. `:kondo {:enabled :path}` rides the
existing two-layer merge from `.clj-pulse/config.edn` and the new
`clojurePulse.kondo.*` VS Code settings, live-reloaded, with
`CLJ_PULSE_DISABLE_KONDO` as the harness kill-switch. The engine is announced
by a log line and a new `clojurePulse/lintStatus` notification, which the
extension renders as a `Linting:` line in the status-bar tooltip. When a
`.clj-kondo` directory exists, the resolved classpath is scanned with
`--dependencies --parallel` in the background so the cross-file linters work
before the user has opened enough files to fill the cache by hand.

**Verification.** `bb check`, `bb e2e` (97 tests, 9 of them new), `bb e2e-nvim`,
`bb e2e-real-kondo`, and `bb e2e-calva` all pass, as does the extension's
`make check` (677 tests, 9 new). `nvim`, a JDK, and the `clojure` CLI were not
installed on this box; they were added via mise so that every Task 12 step could
actually run rather than be recorded as skipped. `bb e2e-nvim` and `bb e2e-calva`
both ran with a real clj-kondo on PATH, so the new notification was exercised
against two real editor clients.

**Deviations** are noted inline under their tasks. In brief: `kondo::warm`
landed in Task 2 rather than Task 7 (same `run` helper); Task 3's `server.rs`
plumbing moved into Task 4's commit so no commit trips clippy's `dead_code`;
fixture `.clj-kondo`/`.clj-pulse` directories are created at test time because
`.gitignore` excludes `.clj-kondo`; warming hangs off `apply_project_diff` and
the startup task rather than each `rebuild_libs` call site, several of which
are sync functions that cannot await; and the harness's kondo flag became a
three-way enum for the real-binary test.

**Codex review** ran after every task. Seven must-fix findings across Tasks 2,
4, 5, 6, 7, and 9 were real and fixed in their own commits: non-zero exits
treated as success in `warm`/`probe_version`; unserialized probes letting a
stale one win; `.clj-kondo/config.edn` changes not re-linting; stale-engine
results publishing over current ones; an engine-changed check too narrow to
catch a `:path` swap; a test helper that cleared only the stash and not the
channel; a warm double-scan race; and the extension sending contributed
defaults as explicit overrides. One finding was **rejected**: codex claimed
clj-kondo resolves `.clj-kondo` from cwd, so lints needed the owning project as
cwd. Tested directly against clj-kondo v2026.05.25 — with cwd set to a parent
project whose config disables a linter, both the config *and* the cache dir
still resolved by walking up from `--filename`. The plan's stated fact was
right.

**What the plan could have specified better.** Two things.

The plan pinned its verified clj-kondo facts to v2026.08.04, but named no way to
re-confirm them. The box had v2026.05.25, so every fact had to be re-derived by
hand before Task 1, and again mid-Task-2 to settle a review dispute. A tiny
"paste this shell snippet to re-verify" block would have made that a
thirty-second check instead of an investigation.

More substantively, Task 7 said to hook warming into "the `rebuild_libs` call
sites", but several of those are synchronous functions holding a
`std::sync::Mutex`, so they cannot await a subprocess. The instruction was
unimplementable as literally written and had to be re-aimed at the two async
boundaries that actually bracket them. A plan step that names a specific call
site is worth checking against that site's async-ness while writing the plan.

