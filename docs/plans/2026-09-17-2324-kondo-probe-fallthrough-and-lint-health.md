# clj-kondo Probe Fall-through and Lint Health Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The clj-kondo probe tries every candidate binary until one answers, resolves mise shims to the real binary at probe time, and a lint pass that fails is reported once on `clojurePulse/lintStatus` and in the log instead of silently publishing the native set; the lint timeout becomes realistic because a superseded run is killed the moment a newer pass starts. Closes the ROADMAP Backlog items of 2026-09-10 ("A failing clj-kondo candidate ends the probe instead of falling through to the next one") and 2026-09-11 ("A failed clj-kondo lint pass is silent"), promoted to Milestone 5.

**Tech Stack:** Rust, tower-lsp 0.20, tokio. Tests: unit tests in `src/kondo.rs` and `src/tools.rs`, e2e in `tests/test_e2e.rs` on `tests/fixtures/kondo_project` with the committed fake `tests/fixtures/fake-clj-kondo/clj-kondo`.

**Branching:** the library dialect plan (`2026-09-17-2240`) is on branch `library-dialect-preference`, complete. Branch `kondo-probe-and-lint-health` off `master` once that PR merges, otherwise off that branch. Files this plan shares with it: `tests/test_e2e.rs`, `docs/ROADMAP.md`, `AGENTS.md`. No overlap in `src/`.

---

## Design

### Today

- `kondo::probe_version` calls `tools::resolve`, which returns the *first* executable named `clj-kondo` on the augmented PATH, runs it once, and returns `Err` when it fails. On metabase the mise shim comes first, mise refuses the untrusted `mise.toml`, the shim exits 1, and Homebrew's binary behind it is never tried. The lint tier then depends on how the editor was launched.
- `kondo::lint` runs from the file's own directory so a shim picks the version the nearest mise config pins; the same shim can pick an untrusted config for one subdirectory and exit without output.
- A failed lint pass (`Err` from `kondo::lint`) publishes the native set and logs at `debug`. `clojurePulse/lintStatus` carries `detail` for probe failures only, so the status bar keeps saying `clj-kondo + native` while nothing from clj-kondo arrives.
- `LINT_TIMEOUT` is 2 s. A large file under load trips it; a superseded pass runs to completion under the `KONDO_LIMIT` semaphore and is only discarded at publish time.

### Part A: the probe tries every candidate

**`tools::resolve_all(program, base) -> Vec<PathBuf>`.** Every executable of that name across the augmented PATH, in PATH order, de-duplicated by canonical path (`fs::canonicalize`, falling back to the path itself). A name with a path separator is one candidate, `base.join(program)`, as today. `tools::resolve` becomes `resolve_all(..).into_iter().next()`.

**`tools::is_mise_shim(path) -> bool`.** The file's parent directory is named `shims` and its first 256 bytes contain `mise`. Testable with a fixture file, independent of `MISE_DATA_DIR`.

**`kondo::probe_version(bin, cwd)`** iterates the candidates. Per candidate:

1. When `is_mise_shim`, resolve `mise` through `tools::resolve("mise", base)` and run `mise which <program>` from `cwd` under `PROBE_TIMEOUT`. A successful run printing an existing executable path replaces the candidate with that path. On any failure the shim itself stays the candidate, so a working shim setup keeps working.
2. Run `<candidate> --version` as today. The first candidate that exits 0 and prints a `clj-kondo <version>` line wins; `Probe.bin` is that path.
3. Otherwise record `"<candidate>: <reason>"` and continue.

When every candidate fails, the error is the recorded reasons joined with `; `. When there is none, the error stays `` `clj-kondo` not found on <describe_search()> ``. `not_found_message` is unchanged and still wraps it.

**The lint runs from the workspace root.** `kondo::lint` gains `cwd: Option<&Path>`; `KondoState` gains `root: Option<PathBuf>` set by `probe_and_announce`, and `lint_and_publish_doc` passes it. The probe proved the binary from the root, so the same binary and mise resolution apply to every file; clj-kondo reads its config from `--filename`, so nothing else changes. `warm` already runs from the project dir.

### Part B: a failed pass is visible, once

**Lint health.** `KondoWarmer` gains `health: SharedLintHealth` where

```rust
/// The last failed pass, `None` when healthy.
struct LintFailure {
    reason: String,  // what `kondo::lint` returned; the de-duplication key
    message: String, // the formatted line: file, reason, "native lints only…"; the `detail`
}
type SharedLintHealth = Arc<std::sync::Mutex<Option<LintFailure>>>;
```

It is separate from `KondoState` on purpose: `KondoState` is compared to retire stale passes, and a failure must not retire in-flight ones. `lint_and_publish_doc` takes `&KondoWarmer` instead of `&SharedKondoState`.

**Transitions, not events.** After the kondo tier of a pass:

- failed with reason `r`, and `health` holds no failure with that `reason`: store `LintFailure { reason: r, message }` where `message` is `clj-kondo failed on <path relative to root>: <r> — native lints only until it succeeds`; `tracing::warn!` and `client.log_message(WARNING, ..)` the message; push `lintStatus` with `engine: "kondo+native"`, `version`, and the message as `detail`.
- failed with the same reason: nothing beyond the existing `debug` line. This is the rate limit: one warning per distinct reason.
- succeeded and `health` is `Some`: clear it, `tracing::info!` and `log_message(INFO, "clj-kondo recovered")`, push `lintStatus` without `detail`.
- `probe_and_announce` clears `health` before announcing, so a re-probe starts clean.

`send_lint_status` reads `detail` as the probe's `state.detail` first, else the health `message`. "Not in use" and "over live-max-kb" outcomes are not failures and never touch health.

**Timeout 2 s to 10 s, with cancellation of superseded passes.**

- `LINT_TIMEOUT` becomes 10 s with its doc comment rewritten: the bound for a wedged binary, not a budget for a normal run, because a superseded run is killed when the next pass starts.
- One registry for every trigger: `type SharedLintTasks = Arc<DashMap<Url, tokio::task::AbortHandle>>`, held by `KondoWarmer` so the engine-reload task has it too. A free function `spawn_lint_pass(client, documents, index, warmer, uri, pass, delay: Duration)` aborts and removes the previous handle for that document, spawns a task that sleeps `delay`, re-checks version and epoch (as `did_change` does today), runs `lint_and_publish_doc`, removes its own handle when done, and stores the task's abort handle. `did_open` and `did_save` call it with zero delay, `did_change` with `DIAGNOSTIC_DEBOUNCE_MS`, `relint_open_documents` with zero delay for each open document (it no longer awaits the passes), and `did_close` aborts and removes. `lint_and_publish` (the inline helper) goes away. So an engine-change pass can be superseded by an edit and an edit's pass by an engine change, and no stale process holds a `KONDO_LIMIT` permit for the timeout.
- `kondo::run` arms a drop guard after `spawn` that calls `kill_group(pid)`; on the normal completion paths it is disarmed. Aborting a pass drops the future, the guard kills the process group, and a shim plus whatever it exec'd dies with it, not only the direct child. The timeout branch keeps its explicit `kill_group`.

### Out of scope

- The Clojure Pulse extension renders `engine`, `version` and `warming` from `lintStatus` and not `detail` (`src/statusBar.ts`, `lintLine`). A Backlog line asks the extension to show it.
- A user setting for the timeout. 10 s plus cancellation makes one unnecessary.
- Windows: `kill_group` already has a `taskkill` branch; the mise shim detection reads the file the same way.

## File Structure

- Modify: `src/tools.rs` — `resolve_all`, `is_mise_shim`, tests.
- Modify: `src/kondo.rs` — candidate iteration and `mise which` in `probe_version`, `cwd` on `lint`, `LINT_TIMEOUT`, the drop guard in `run`, tests.
- Modify: `src/server.rs` — `KondoState.root`, `SharedLintHealth` and `SharedLintTasks` on `KondoWarmer`, health transitions in `lint_and_publish_doc`, `send_lint_status` reading it, the free `spawn_lint_pass`, `did_open`/`did_save`/`did_change`/`did_close`/`relint_open_documents` routing.
- Modify: `tests/fixtures/fake-clj-kondo/clj-kondo` — two new stdin markers.
- Modify: `tests/test_e2e.rs` — five e2e tests.
- Modify: `docs/ROADMAP.md`, `AGENTS.md`, `docs/LINTING.md`.

## Tasks

### Task 1: Promote the roadmap items

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Move the items**
  Delete the 2026-09-10 "A failing clj-kondo candidate ends the probe…" and 2026-09-11 "A failed clj-kondo lint pass is silent" Backlog lines. Add one unticked Milestone 5 item directly above **Release**, titled **clj-kondo discovery and failure reporting**, with two sub-bullets carrying each item's text, and the line `Plan: [2026-09-17-2324-kondo-probe-fallthrough-and-lint-health.md](plans/2026-09-17-2324-kondo-probe-fallthrough-and-lint-health.md) — in progress`.
  Add a Backlog line: `2026-09-17 **Clojure Pulse tooltip shows the lintStatus detail.** The server now sends a per-pass failure reason as detail on clojurePulse/lintStatus; the extension's status-bar lint line renders engine, version and warming only.`

- [x] **Step 2: Commit**
  `git commit -am "Plan clj-kondo probe fall-through and lint health"` (include this plan file).

### Task 2: `resolve_all` and `is_mise_shim`

**Files:**
- Modify: `src/tools.rs`
- Test: `src/tools.rs` (`mod tests`)

- [x] **Step 1: Write the failing tests**
  - `resolve_all_lists_every_executable_in_path_order`: two temp dirs each holding an executable `clj-kondo`, PATH built from `CLJ_PULSE_TOOL_DIRS` is not available inside a unit test, so build the candidate list through a private `resolve_all_in(program, base, path: &OsStr)` that `resolve_all` calls with `augmented_path()`; assert both paths in order.
  - `resolve_all_dedupes_the_same_file_listed_twice`: the same dir twice in the PATH yields one entry; a symlink to the first dir's binary in the second dir also collapses (canonical path).
  - `resolve_all_takes_an_explicit_path_as_the_only_candidate`.
  - `is_mise_shim_needs_a_shims_dir_and_a_mise_mention`: `shims/clj-kondo` whose content is `#!/bin/sh\nexec mise x -- clj-kondo "$@"` is a shim; the same content under `bin/` is not; `shims/clj-kondo` holding an ELF-like blob without `mise` is not.

- [x] **Step 2: Run to verify they fail**
  Run: `cargo test --lib tools::`
  Expected: compile errors for the new functions.

- [x] **Step 3: Implement**
  `resolve_all_in`, `resolve_all`, `resolve` delegating to it, `is_mise_shim` reading at most 256 bytes with `std::fs::File` + `read`.

- [x] **Step 4: Run to verify they pass**
  Run: `cargo test --lib tools::`
  Expected: PASS, existing tests included.

- [x] **Step 5: Commit**
  `git commit -am "List every candidate binary and recognize mise shims"`
  > Deviation: the branch was cut from `master` (PR #41 had merged), and the commit landed in two parts — the first missed a clippy `nonminimal_bool` lint, fixed in the follow-up commit. `tools::is_executable` was made `pub(crate)` here rather than in Task 3, since it is where the visibility lives.

### Task 3: The probe falls through and resolves shims

**Files:**
- Modify: `src/kondo.rs`
- Test: `src/kondo.rs` (`mod tests`)

- [x] **Step 1: Write the failing tests**
  The existing `fake_bin(dir, script)` helper writes `dir/clj-kondo`. Add `fake_named(dir, name, script)` beside it for a `mise` fake. Tests, each building PATH through the `CLJ_PULSE_TOOL_DIRS` variable is not safe in a parallel test binary, so pass an explicit PATH the way `bare_names_resolve_only_to_executables` does not: give `probe_version` a private sibling `probe_version_in(bin, cwd, path: &OsStr)` that the public one calls with `augmented_path()`, and test the sibling.
  - `probe_falls_through_a_failing_candidate`: dir A's `clj-kondo` prints `mise ERROR: config not trusted` to stderr and exits 1; dir B's prints `clj-kondo v2026.1.1`. PATH `A:B`. The probe answers `v2026.1.1` with `bin` = B's path.
  - `probe_error_lists_every_candidate_tried`: both fail; the error contains both paths and both reasons.
  - `probe_resolves_a_mise_shim_through_mise_which`: dir S is named `shims` and its `clj-kondo` contains `mise` and exits 1 whatever the argument; dir R holds the real fake answering `--version`; dir M holds a `mise` fake that on `which clj-kondo` prints R's `clj-kondo` path. PATH `S:M`. The probe answers with `bin` = R's path.
  - `probe_keeps_the_shim_when_mise_which_fails`: same, but the `mise` fake exits 1 and the shim answers `--version` itself; `bin` is the shim path.

- [x] **Step 2: Run to verify they fail**
  Run: `cargo test --lib kondo::tests::probe`
  Expected: FAIL (compile error on `probe_version_in`).

- [x] **Step 3: Implement**
  In `probe_version_in`: candidates from `tools::resolve_all_in`; for each, the shim step then the `--version` step as described in the design; collect reasons; return the first success. Keep the existing "wrapper prints a banner then fails" rejection per candidate. `mise which` runs through `run` with `PROBE_TIMEOUT`, cwd = `cwd`, and its stdout trimmed must name an existing executable (`tools::is_executable` made `pub(crate)`).

- [x] **Step 4: Run to verify they pass**
  Run: `cargo test --lib kondo::`
  Expected: PASS, including `probe_version_of_a_missing_binary_says_where_it_looked`.

- [x] **Step 5: Commit**
  `git commit -am "Probe every clj-kondo candidate and resolve mise shims to the real binary"`
  > Deviation: codex found that the plan's `is_mise_shim` (first 256 bytes contain `mise`) misses the shape mise actually installs — every shim is a *symlink to the `mise` binary* (verified on this host: `shims/clj-kondo -> ~/.local/bin/mise`, an ELF with no `mise` in its header). Fixup commit `655de87`: `is_mise_shim` also accepts a symlink under `shims/` whose canonical target is named `mise`, and `behind_mise_shim` asks *that* mise (`tools::mise_behind_symlink`) before searching PATH for one. Tests cover both shapes.

### Task 4: Lint from the root, longer timeout, kill on drop

**Files:**
- Modify: `src/kondo.rs`, `src/server.rs`
- Test: `src/kondo.rs` (`mod tests`)

- [x] **Step 1: Write the failing tests**
  - `lint_runs_from_the_given_cwd`: a fake that prints `pwd` into the findings message; call `lint(bin, src, path, Some(tmp), ..)` and assert the message names `tmp`, not the file's directory.
  - `lint_child_group_dies_when_the_future_is_dropped`: a fake that runs `sh -c 'echo $$ > <file>; sleep 30'` so the recorded pid is a grandchild; start `lint` in a task, wait for the pid file, abort the task, then poll `libc::kill(pid, 0)` until it fails, within 2 s. `kill_on_drop` already kills the direct child, so a direct-child pid would not test the guard.

- [x] **Step 2: Run to verify they fail**
  Run: `cargo test --lib kondo::tests::lint_`
  Expected: the first fails to compile (new parameter); once it compiles, the second fails because the grandchild outlives the drop.

- [x] **Step 3: Implement**
  `lint(bin, source, abs_path, cwd: Option<&Path>, timeout)`, setting `current_dir(cwd)` when given and dropping the per-file directory logic and its comment. `LINT_TIMEOUT = 10 s` with the new doc comment. In `run`, a `struct KillOnDrop(Option<u32>)` with `Drop` calling `kill_group`, armed after `spawn`, disarmed (`.0 = None`) once `wait_with_output` returns. In `server.rs`, `KondoState.root: Option<PathBuf>` set from `root` in `probe_and_announce`, and `lint_and_publish_doc` passes `engine.root.as_deref()`.

- [x] **Step 4: Run to verify they pass**
  Run: `cargo test --lib kondo::` and `cargo build`
  Expected: PASS; the build is clean.

- [x] **Step 5: Commit**
  `git commit -am "Lint from the workspace root, allow 10 s, kill a dropped clj-kondo"`

### Task 5: Lint health on `lintStatus`

**Files:**
- Modify: `src/server.rs`, `tests/fixtures/fake-clj-kondo/clj-kondo`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Extend the fake**
  Two stdin markers before the default branch: `*kondo-fail-here*` prints `boom: simulated crash` to stderr and exits 1 with no stdout; `*kondo-hang-here*` runs `sh -c 'echo $$ > "$FAKE_KONDO_PID_FILE"; sleep 30'` when the variable is set, so the recorded pid is a *grandchild* of clj-pulse: `kill_on_drop` alone would leave it alive, and only the process-group kill takes it down. Update the header comment.

- [ ] **Step 2: Write the failing e2e tests**
  Using `setup_kondo_project`, `start_with_kondo` / `start_with_kondo_env`, and the existing `wait_for_notification_where`:
  - `test_e2e_kondo_lint_failure_is_reported_once_on_lint_status`: open `src/app.clj` with `kondo-fail-here` inserted; expect a `lintStatus` with `engine == "kondo+native"` and `detail` containing `clj-kondo failed on src/app.clj` and `simulated crash`; expect one `window/logMessage` with `clj-kondo failed`; make a second edit that keeps the marker, wait for its diagnostics, and assert the count of such log messages is still 1; remove the marker, expect diagnostics carrying the fake's `unresolved-symbol` (use `kondo-finding-here`) and a `lintStatus` with no `detail` and a `clj-kondo recovered` log message.
  - `test_e2e_superseded_lint_pass_is_killed`: `start_with_kondo_env` with `FAKE_KONDO_PID_FILE`; insert `kondo-hang-here`, wait until the pid file exists (poll 5 s); insert a second change removing the marker; assert diagnostics for the second version arrive within 5 s and that `kill(pid, 0)` fails within 2 s of them.

- [ ] **Step 3: Run to verify they fail**
  Run: `cargo test --test test_e2e kondo_lint_failure -- --nocapture` and `cargo test --test test_e2e superseded_lint -- --nocapture`
  Expected: FAIL. No `detail` appears in the first; the second times out waiting for the process to die.

- [ ] **Step 4: Implement**
  `SharedLintHealth` and `SharedLintTasks` on `KondoWarmer`; `lint_and_publish_doc(client, documents, index, warmer: &KondoWarmer, uri, pass)` with the transition logic after the join and *after* the staleness check, so a retired pass never reports; `send_lint_status(client, warmer, warming)` reading probe detail first, then the health message; `probe_and_announce` clearing health. The free `spawn_lint_pass`; route `did_open`, `did_save`, `did_change`, `did_close` and `relint_open_documents` through it; delete `lint_and_publish`. Update the warm-cache calls to the new signatures.

- [ ] **Step 5: Run to verify they pass**
  Run: `cargo test --test test_e2e kondo -- --nocapture`
  Expected: PASS, every existing kondo e2e test included.

- [ ] **Step 6: Commit**
  `git commit -am "Report a failed clj-kondo pass once on lintStatus and cancel superseded passes"`

### Task 6: Discovery e2e

**Files:**
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the e2e tests**
  Next to `test_e2e_kondo_found_in_a_well_known_dir_off_path`, using `well_known_dir_with_fake_kondo`, `BARE_PATH`, and `LspClient::spawn(.., Kondo::Real)`:
  - `test_e2e_kondo_probe_falls_through_a_failing_candidate`: dir A holds a `clj-kondo` script exiting 1 with `mise ERROR: config not trusted` on stderr; dir B is `well_known_dir_with_fake_kondo()`. `CLJ_PULSE_TOOL_DIRS` = `A:B` (join with `std::env::join_paths`). Expect the log `clj-kondo v0.0.0-fake found (<B>/clj-kondo)`, then open `src/app.clj` and expect the fake's `unresolved-symbol`.
  - `test_e2e_kondo_not_found_lists_every_candidate`: both dirs fail; the `clj-kondo not found` log line names both paths.
  - `test_e2e_kondo_mise_shim_resolves_to_the_real_binary`: dir S = `<tmp>/shims` with a `clj-kondo` containing `mise` that exits 1; dir M with a `mise` script answering `which clj-kondo` with the path of dir B's fake; `CLJ_PULSE_TOOL_DIRS` = `S:M`. Expect `found (<B>/clj-kondo)` and kondo diagnostics on open, which proves the resolved path lints and not the shim.

- [ ] **Step 2: Run them**
  Run: `cargo test --test test_e2e kondo -- --nocapture`
  Expected: PASS. The implementation landed in Task 3, so these pass on first run; if one fails, the probe is wrong, not the test.

- [ ] **Step 3: Full check and gates**
  Run: `bb check`, then `bb e2e`, then `bb e2e-pulse`
  Expected: all green. The status change is client-visible, so `bb e2e-pulse` applies; no location shape changes, so `bb e2e-calva` does not.

- [ ] **Step 4: Commit**
  `git commit -am "Cover probe fall-through and mise shim resolution end to end"`

### Task 7: Docs

**Files:**
- Modify: `docs/ROADMAP.md`, `AGENTS.md`, `docs/LINTING.md`, `README.md`

- [ ] **Step 1: ROADMAP**
  Tick the Milestone 5 item, set the Plan line to `— done`, and add to "Where we stand": the clj-kondo probe tries every candidate and resolves mise shims, and a failed pass is reported on `lintStatus`.

- [ ] **Step 2: AGENTS.md**
  Rewrite the "Child processes…" invariant's last sentence: a bare `:kondo {:path}` is resolved at probe time by trying *every* executable of that name on the augmented PATH in order (`tools::resolve_all`), a mise shim being replaced by what `mise which` prints when that works; the winning path is what lints, from the workspace root, never from the file's directory. Add one bullet: a lint pass that fails stores its reason in `KondoWarmer.health` (separate from `KondoState`, which retires in-flight passes) and reports it once per distinct reason as `detail` on `clojurePulse/lintStatus` plus a warning log, cleared on the next success and on every re-probe; every pass, engine-change re-lints included, is spawned through `spawn_lint_pass` over the one `KondoWarmer.tasks` registry, which aborts the previous pass for that document, and `kondo::run` group-kills its child on drop, which is why `LINT_TIMEOUT` can be 10 s.

- [ ] **Step 3: LINTING.md**
  Add a short section "When clj-kondo fails": what the status bar and log say, that native lints stay, that the message appears once per distinct reason, the 10 s bound, and that the probe tries every install it can find and resolves mise shims through `mise which`, so a `mise.toml` mise will not trust no longer hides a Homebrew install.

- [ ] **Step 4: README**
  Check the README's diagnostics sentence; add nothing unless it describes discovery. State in the commit message that it was checked.

- [ ] **Step 5: Verify and commit**
  Run: `bb check`
  Expected: green.
  `git commit -am "Document clj-kondo discovery and failure reporting"`
