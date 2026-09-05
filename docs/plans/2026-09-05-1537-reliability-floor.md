# Reliability Floor Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the server survive a panicking request, measure its behavior on a large real project, and prove it stays up on malformed input (ROADMAP Milestone 1, "Reliability floor").

**Tech Stack:** Rust, tower-lsp 0.20, tower `Service`, `futures` (`catch_unwind`), tokio. Tests: e2e harness (`tests/test_e2e.rs`), a new ignored bench test (`tests/test_bench.rs`), babashka tasks.

---

## Design

### Panics kill the server today

tower-lsp does not spawn request handlers. `Server::serve` (`tower-lsp-0.20.0/src/transport.rs:102`) drives `service.call(req)` futures through `buffer_unordered` inside its own loop. A panic in any handler unwinds through `serve`, through `#[tokio::main]`, and the process exits. The editor then restarts the server, the index is rebuilt from scratch, and every open buffer's state is lost. Panics in `tokio::spawn`ed background tasks (indexing, lint passes) are already isolated, but they vanish silently.

### Panic guard

A tower `Service` wrapper, `PanicGuard<S>`, in a new `src/panic_guard.rs`:

- `call(req)` clones the request id, wraps `inner.call(req)` in `AssertUnwindSafe(...).catch_unwind()`, and on `Err(panic)` returns `Ok(Some(Response::from_error(id, Error::internal_error())))` when the request had an id, or `Ok(None)` for a notification. The response type and error type are those of `LspService` (`Option<Response>` and `ExitedError`), so `Server::serve`'s bounds are met unchanged.
- `main.rs` wraps: `Server::new(stdin, stdout, socket).serve(PanicGuard::new(service)).await`.
- A panic hook installed at startup (`std::panic::set_hook`) logs the payload and location with `tracing::error!`, so both request panics and background-task panics land in `server.log`.
- `AssertUnwindSafe` is justified: the `Backend` state is `DashMap`s and `Arc`s; a panic mid-update can leave one map entry stale, which the next didChange or save corrects, and that is far better than losing the process.

Test hook: when `CLJ_PULSE_TEST_PANIC` is non-empty, `main.rs` registers one extra custom method, `clojurePulse/__testPanic`, whose handler panics. `LspService::build` returns the builder by value, so the registration is a conditional `let builder = if … { builder.custom_method(…) } else { builder }`. The e2e test starts the server with that variable, sends the method, expects an error response, then sends a normal request (hover) and expects a real answer, proving the process survived. The hook is documented in AGENTS.md next to the other `CLJ_PULSE_*` test variables.

`futures = "0.3"` becomes a direct dependency (already in `Cargo.lock` transitively through tower-lsp).

Order of work matters for the red test: the `clojurePulse/__testPanic` method is registered first, so the failing test shows the real failure (the server pipe closes) rather than a method-not-found error.

### Bench

An ignored integration test, `tests/test_bench.rs`, that reuses `LspClient` from the e2e harness (move the client into `tests/common/mod.rs` if sharing is awkward; otherwise `#[path]`-include it). It reads `CLJ_PULSE_BENCH_ROOT` and skips with a message when unset.

The bench must measure production behavior, and `LspClient::start` deliberately does not: it sets `CLJ_PULSE_DISABLE_CLASSPATH_CLI` and `CLJ_PULSE_DISABLE_KONDO`. The bench uses a new constructor, `LspClient::start_production(root)`, that sets neither, so stage 3 classpath resolution runs (`clojure -A:dev:test -Spath`, with whatever the machine has cached) and clj-kondo is spawned when installed. The report names which of the two were active.

Indexing has stages, so the report samples at two points: after the project index (the `Indexed … in …` log line) and after library indexing (`library indexing complete`, or `full classpath indexed` when stage 3 ran). It reports:

| Metric | How |
|---|---|
| Time to project index | wall clock from `initialize` to the `Indexed … in …` log line, plus the elapsed value the server itself logs (`server.rs:1930`) |
| Time to library index | wall clock from `initialize` to `library indexing complete` / `full classpath indexed` |
| Symbol and namespace counts | parsed from the `Indexed` log line |
| Resident memory | sampled twice, after each stage above. Linux: `VmRSS` from `/proc/<pid>/status`; macOS: `ps -o rss= -p <pid>`; otherwise `n/a`. `LspClient.child.id()` gives the pid |
| didOpen → first diagnostics | on the largest `.clj` file under the root (found by size, ties broken by path order) |
| didChange → diagnostics | median over 20 single-character inserts at the end of that file, each waiting for the next `publishDiagnostics` |
| Definition latency | median over 20 `textDocument/definition` requests on the first `alias/name` token in that file whose `alias` is an `:as` alias of its ns form (found with a regex over the file text). When no such token exists the metric prints `n/a` instead of failing |

Results print as a small table to stdout (`cargo test -- --nocapture`). The only assertion is a hang ceiling: the index must finish within 120 s. `bb bench` shallow-clones `https://github.com/metabase/metabase` into `.tmp/bench/metabase` when absent (it is the largest common deps.edn codebase), sets the variable, and runs the test with `--nocapture`. `.tmp/` is already gitignored.

The first numbers go into `docs/MEMORY.md` under a "Performance baseline" heading with the date, machine, and commit, so later runs have something to compare against.

### Malformed input

e2e tests that each open or produce a bad input and then prove the server still answers a normal request:

- An unbalanced buffer: `didOpen` with `(defn f [x] (let [y` and then hover, completion, and definition requests on it return without error.
- One 4 MB line: `didOpen` a file whose single line is a `(def big "…")` string; `documentSymbol` answers within the harness timeout.
- A non-UTF-8 file in the fixture: a `.clj` file containing invalid bytes under `src/`; indexing logs a skip and the project still indexes (`scanner.rs:41` already ignores `read_to_string` errors; the test pins it).
- Empty and invalid `deps.edn`: two temp projects, one with `{}` and one with `{:paths [`; both initialize, index their `src/` (with `:paths` defaulting to `src`) or at least answer `initialize` and `hover` without crashing.
- A `didChange` whose range is past the end of the document: the server ignores or clamps it and the next request answers.

### Fixing what the bench finds

A separate task with a budget: fix anything under a day's work inside this plan; anything larger becomes a roadmap item under Milestone 1 with the measured numbers attached, and this plan is still marked done.

## File Structure

Create:

- `src/panic_guard.rs`: `PanicGuard<S>` and `install_panic_hook()`.
- `tests/test_bench.rs`: the ignored bench test.
- `tests/fixtures/malformed_project/`: `deps.edn`, `src/ok.clj`, `src/bad_bytes.clj` (invalid UTF-8; generate with a script, commit the bytes).

Modify:

- `src/main.rs`: panic hook, `PanicGuard`, conditional `clojurePulse/__testPanic`.
- `src/server.rs`: the `test_panic` handler.
- `Cargo.toml`: `futures = "0.3"`.
- `tests/test_e2e.rs`: panic survival test, malformed-input tests, `LspClient::start_production`, and any client helper they need (`documents_symbol` exists; add `did_change_range` if `did_change_insert` cannot express a bad range).
- `bb.edn`: `bench` task.
- `AGENTS.md`, `README.md`: `CLJ_PULSE_TEST_PANIC`, `bb bench`.
- `docs/MEMORY.md`: performance baseline.
- `docs/ROADMAP.md`: tick the item.

## Tasks

### Task 1: Panic guard

**Files:**
- Create: `src/panic_guard.rs`
- Modify: `src/main.rs`, `src/server.rs`, `src/lib.rs`, `Cargo.toml`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Register the panic-on-demand method**
  In `main.rs`, when `CLJ_PULSE_TEST_PANIC` is non-empty, register `clojurePulse/__testPanic` whose handler in `server.rs` panics with a recognizable message. No guard yet.

- [ ] **Step 2: Write the failing e2e test**
  `test_e2e_server_survives_handler_panic`: start with `LspClient::start_with_env(root, &[("CLJ_PULSE_TEST_PANIC", …)])` (the helper takes paths; add a sibling that takes string values, or pass a dummy path value since only non-emptiness matters), `initialize`, send `clojurePulse/__testPanic` via `request_expect_error`, then `hover` on `core/add` in `utils.clj` and assert a real hover.

- [ ] **Step 3: Run it to verify it fails**
  Run: `cargo test --test test_e2e survives_handler_panic`
  Expected: FAIL with the harness reporting a closed pipe or a missing response: the process died on the panic.

- [ ] **Step 4: Implement the guard**
  `panic_guard.rs` per the design; `install_panic_hook()` called first thing in `main` after logging is initialized; `futures` dependency. Keep `PanicGuard` generic over `S: Service<Request, Response = Option<Response>, Error = ExitedError>`.

- [ ] **Step 5: Run the tests**
  Run: `bb check && cargo test --test test_e2e survives_handler_panic`
  Expected: PASS; `server.log` in the temp project contains a `panicked` line with the location.

- [ ] **Step 6: Commit**
  `git commit -m "Survive handler panics with a catch_unwind service guard"`

### Task 2: Malformed input tests

**Files:**
- Create: `tests/fixtures/malformed_project/…`
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Write the tests**
  One test per bullet in the design's "Malformed input" section. Each ends with a normal request that must succeed. For the non-UTF-8 fixture, write the bytes with a tiny Python one-liner and commit the file; add a comment file next to it explaining why it is binary.

- [ ] **Step 2: Run them**
  Run: `cargo test --test test_e2e malformed`
  Expected: PASS for most. Any failure is a real finding: fix it in the server in the smallest way that keeps the request path alive (clamping a bad range, skipping a bad file with a log line), with the test as the regression guard.

- [ ] **Step 3: Commit**
  `git commit -m "Prove the server answers on malformed input"` (plus one commit per server fix, if any).

### Task 3: Bench harness

**Files:**
- Create: `tests/test_bench.rs`
- Modify: `bb.edn`, `tests/test_e2e.rs` (only if the client is moved to `tests/common/`)

- [ ] **Step 1: Make `LspClient` reusable**
  Prefer `#[path = "test_e2e.rs"]`-free sharing: move `LspClient` and its helpers into `tests/common/mod.rs` and `mod common;` it from both test files. Run `bb e2e` to prove nothing changed.

- [ ] **Step 2: Add `LspClient::start_production`**
  A constructor that sets none of the `CLJ_PULSE_DISABLE_*` variables. Keep it out of the regular e2e tests: a comment explains it exists for the bench only.

- [ ] **Step 3: Write the bench**
  `tests/test_bench.rs` with one `#[test] #[ignore] fn bench_large_project()` implementing the metric table, including the two-stage RSS sampling, the OS-specific RSS readers, and the deterministic qualified-symbol choice. Print with fixed columns. Skip cleanly when `CLJ_PULSE_BENCH_ROOT` is unset.

- [ ] **Step 4: Add `bb bench`**
  Task: clone if `.tmp/bench/metabase` is missing (`git clone --depth 1 https://github.com/metabase/metabase .tmp/bench/metabase`), then `CLJ_PULSE_BENCH_ROOT=$PWD/.tmp/bench/metabase cargo test --release --test test_bench -- --ignored --nocapture`. Use `--release`: the debug build's numbers are not what users see.

- [ ] **Step 5: Run it**
  Run: `bb bench`
  Expected: the table prints and the test passes the hang ceiling. Record the numbers.

- [ ] **Step 6: Commit**
  `git commit -m "Add a large-project bench"`

### Task 4: Act on the bench

**Files:**
- Modify: whatever the numbers point at; `docs/MEMORY.md`; `docs/ROADMAP.md`

- [ ] **Step 1: Read the numbers**
  Anything that would surprise a user: index time over 30 s, RSS over 1 GB, didChange latency over 300 ms (the diagnostics debounce), definition over 50 ms. Profile the worst one with `cargo flamegraph` or `perf` if installed, otherwise with `tracing` timing around the suspect.

- [ ] **Step 2: Fix within budget**
  Fix what fits in a day with a test or a repeat bench run proving the improvement. For anything larger, add a Milestone 1 roadmap item with the measurement and the suspected cause.

- [ ] **Step 3: Record the baseline**
  `docs/MEMORY.md`: "Performance baseline" with date, commit, machine, and the table after fixes.

- [ ] **Step 4: Commit**
  `git commit -m "Record the performance baseline"` (fixes in their own commits before it).

### Task 5: Docs and roadmap

**Files:**
- Modify: `AGENTS.md`, `README.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Update docs**
  AGENTS.md: `CLJ_PULSE_TEST_PANIC` under Testing notes; `bb bench` under Verification with one line on when to run it (before a release, after index or extractor changes). README: `bb bench` in the Development task list, and a sentence in the Status note that a panicking request no longer takes the server down. ROADMAP: tick "Reliability floor", set `Plan:` to `done`. Use /writing-clearly.

- [ ] **Step 2: Final verification**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [ ] **Step 3: Commit**
  `git commit -m "Document the panic hook, test switch, and bench"`
