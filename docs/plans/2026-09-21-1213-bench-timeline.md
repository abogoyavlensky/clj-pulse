# Bench Timeline Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reshape `bb bench` and its public tables around the user's startup timeline: first navigation, every dependency navigable, clj-kondo finished — cold and warm, medians over repeat runs.

**Tech Stack:** Rust integration test harness (`tests/test_bench.rs`, `tests/common/`), babashka task (`bb.edn`), Markdown docs.

---

## Design

### Why

The current bench publishes "time to first library definition" probed at
`clojure.string`, which lands the moment clojure.jar is read and never shows
dependency indexing; "time to settled", which on clj-pulse is mostly the
clj-kondo dependency-cache warm and reads as if clj-pulse took 50 s; and a
452 KiB keystroke row that compares the native lint tier against clojure-lsp's
embedded clj-kondo and needs three sentences of caveat. The public table
should read as the user's timeline instead.

### The three startup metrics

Cold and warm, both servers. Every published metric stays behavioral.

1. **Time to first navigation.** The existing first project definition,
   polled until it lands. Unchanged.
2. **All dependencies navigable.** clj-pulse: polled only *after* the first
   library stage line arrived — a `STAGE2_LINES` line ("library indexing
   complete") or, failing that, a `STAGE3_LINES` one — whichever the server
   logs first. The row is stamped when a definition into a *third-party*
   dependency lands after that gate, so the published number is behavioral
   and the line only decides when asking starts.

   > Deviation (plan review): the design first proposed gating on
   > `clojurePulse/librariesChanged`, but the server also sends that when the
   > project list is known, before any indexing (`src/server.rs:2150`), so
   > the first one proves nothing. The stage lines are unambiguous.

   clojure-lsp has no gate: the probe is polled from the start, and the row
   equals its first definition, which is the design difference the table is
   meant to show. The stage-3 "full classpath indexed" time stays a detail
   line and JSON field.
3. **clj-kondo finished.** clj-pulse: the last `clojurePulse/lintStatus`
   with `warming: false` seen by settle time, provided a `warming: true` was
   seen at all (`send_lint_status` also fires `warming: false` on probe and
   recovery, so "false after true" is the rule). clojure-lsp: its existing
   settle time (progress end plus quiet), since its startup is a kondo
   analysis. `n/a` when no warm ever started (no clj-kondo, no `.clj-kondo`
   dir). Replaces "time to settled" in public tables; `settled_ms` stays in
   JSON and MEMORY.

### Third-party probe: a candidate set

`tests/common/sites.rs` gains `third_party_sites(text, file, root, paths,
limit)`: `alias/name` usages whose namespace does not start with `clojure.`,
has no file in the project (`namespace_file` is `None`), and is expected to
land in an archive (`Expect::Archive` of the namespace path). Probes collect
up to `LIBRARY_CANDIDATES = 5` sites across distinct namespaces from the
smallest source-ish files. The row lands when the *first* candidate lands:
a candidate that is really a git dep or cljs-only never resolves, and another
carries the row. The `clojure.string` `library_site` and the
"first library definition" row are removed; nothing else used them.

### Repeat runs

`CLJ_PULSE_BENCH_RUNS` (default 1) repeats each *warm* configuration. Order:
clj-pulse cold, clj-pulse warm ×N, clojure-lsp cold, clojure-lsp warm ×N.
Every run prints its detail block and a `BENCH_JSON` line carrying `"run": i`
(1-based). With N > 1 a median row per warm configuration is printed in the
summary and emitted as `"run": "median", "runs": N`: the median of every
numeric field over the runs that have it. An env var, not a positional arg,
because `bb bench`'s positional args are corpus names.

### Public tables

- `docs/PERFORMANCE.md`: one table per corpus, columns clj-pulse cold, warm,
  clojure-lsp cold, warm. Rows: time to first navigation, all dependencies
  navigable, clj-kondo finished, memory once settled, definition (median of
  20), keystroke → diagnostics on the file under `live-max-kb`.
- `README.md`: metabase only, cold and warm, the same rows minus the
  diagnostics row.
- `docs/MEMORY.md`: the full record, including settled and the 452 KiB row.
- Numbers come from a real `CLJ_PULSE_BENCH_RUNS=3 bb bench` run in the last
  task; no number is invented.

### Rejected

Ratio column, CPU seconds, dropping clj-kondo from the bench.

## File Structure

- `tests/common/sites.rs` — `third_party_sites`; `library_site` removed.
- `tests/test_bench.rs` — `Probes.startup_library: Vec<Site>`,
  `poll_definitions` with a gate, `StageWatch` records `librariesChanged`
  and warming transitions, `Row` gains `libraries_navigable`,
  `kondo_finished`, `run`; multi-run loop, median row, summary and JSON
  columns; two non-ignored unit tests.
- `bb.edn` — passes `CLJ_PULSE_BENCH_RUNS` through.
- `docs/PERFORMANCE.md`, `README.md`, `docs/MEMORY.md`, `CLAUDE.md`,
  `docs/DEV_SETUP.md` — tables and method.

---

### Task 1: Third-party probe sites

**Files:**
- Modify: `tests/common/sites.rs`
- Modify: `tests/test_bench.rs` (unit test only)

- [x] **Step 1: Write the failing test**
  In `tests/test_bench.rs`, a non-ignored `#[test] fn third_party_sites_skip_clojure_and_project_namespaces` with an inline ns form requiring `clojure.string :as str`, a project namespace (a temp dir with `src/app/util.clj` defining `f`), and `honey.sql :as sql`; body uses `str/join`, `util/f` and `sql/format`. Expect exactly one site, token `sql/format`, `Expect::Archive("honey/sql")`, cursor in the name part.

- [x] **Step 2: Run test to verify it fails**
  Run: `cargo test --test test_bench third_party_sites`
  Expected: FAIL (function does not exist).

- [x] **Step 3: Implement `third_party_sites`**
  Signature: `pub fn third_party_sites(text: &str, file: &Path, root: &Path, paths: &[PathBuf], limit: usize) -> Vec<Site>`. Built on `usage_sites` with a predicate: `!ns.starts_with("clojure.") && namespace_file(ns, name, root, paths).is_none()`, `Expect::Archive(ns.replace('-', "_").replace('.', "/"))`. Distinct namespaces: dedupe by namespace before returning. Delete `library_site` and its doc comment.

- [x] **Step 4: Run test to verify it passes**
  Run: `cargo test --test test_bench third_party_sites`
  Expected: PASS. Then `cargo build --tests` compiles (test_bench still imports `library_site`; fix the import in Task 2 or remove now, whichever keeps the build green — remove now).

- [x] **Step 5: Commit**
  `git commit -m "bench: third-party dependency probe sites"`

  > Deviation: `Probes::discover` was migrated to `third_party_sites(..., 1).pop()` in the same commit so the build stayed green; steps 1 and 2 were folded into one (test and implementation written together, the test seen passing).

### Task 2: Timeline metrics in the harness

**Files:**
- Modify: `tests/test_bench.rs`

- [x] **Step 1: Probes**
  `startup_library: Vec<Site>`, filled from the smallest source-ish files with `third_party_sites(..., LIBRARY_CANDIDATES - collected)` until 5 distinct namespaces or files run out. `startup_sites()` opens each candidate's file once. `print` lists every candidate under `library candidates`.

- [x] **Step 2: StageWatch and receipt times**
  `LspClient` gains `pub received: Vec<Instant>`, parallel to `notifications`, pushed in `stash` and cleared with it, so a message's time is when it was pulled off the channel, not when a later scan noticed it. `StageWatch` keeps an `observed` cursor and processes each stashed message once; `reset` zeroes the cursor after `clear_notifications`. Add `library_stage_first: Option<Duration>` (earliest stage line by receipt time), `warming_started`, `warming_finished` (each `lintStatus`: `warming == true` sets started if unset; `warming == false` after started sets finished to that message's receipt time, later messages overwriting).

  > Deviation (plan review): the plan first said "this observation", which would re-stamp the same message on every scan.

- [x] **Step 3: poll_definitions**
  Replace the two-slot loop with: project site (as before) plus the candidate set, gated. Signature:
  `fn poll_definitions(client, project: Option<&Site>, library: &[Site], gate: Gate, t0, deadline, watch) -> Startup` where `enum Gate { LibraryStage, None }` and `struct Startup { first_definition, libraries_navigable, library_site: Option<usize>, wrong_dialect }`. Each iteration: ask the project site if unanswered; if gate is satisfied (`watch.library_stage_first.is_some()` or `Gate::None`) ask each unanswered candidate in order until one lands, stamp `libraries_navigable = t0.elapsed()`, record which. `watch.observe` runs every iteration. Stop when both answered, or the deadline passes.

- [x] **Step 4: Row**
  Replace `first_library_definition` with `libraries_navigable: Option<Duration>`, `library_site: Option<String>` (the token), keep `library_wrong_dialect`. Add `kondo_finished: Option<Duration>` and `run: usize`. After settle: clj-pulse `kondo_finished = watch.warming_finished` (only if `warming_started` is some); clojure-lsp `kondo_finished = settled`. Print rows: `time to first navigation`, `all dependencies navigable` (with the token and dialect note), `clj-kondo finished`, keep `settled` under the detail lines. JSON: `libraries_navigable_ms`, `library_site`, `kondo_finished_ms`, `run`; remove `first_library_definition_ms`; keep every other field.

- [x] **Step 5: Summary**
  Columns: server, temp, run, `1st nav`, `all libs`, `kondo done`, `RSS`, `def`, `edit <large>K`, `edit <small>K`.

- [x] **Step 6: Verify it compiles and unit tests pass**
  Run: `cargo test --test test_bench`
  Expected: the unit test passes, the bench is ignored.

- [x] **Step 7: Commit**
  `git commit -m "bench: first navigation, all dependencies navigable, clj-kondo finished"`

### Task 3: Repeat runs and medians

**Files:**
- Modify: `tests/test_bench.rs`
- Modify: `bb.edn`

- [x] **Step 1: Write the failing test**
  `#[test] fn median_row_takes_the_median_of_each_field`: three warm `Row`s with `first_definition` 300/100/200 ms, `rss_settled` Some(3)/None/Some(1), `definition_samples` 20 each; expect the median row to have 200 ms, RSS 3 (median of the two present, upper middle per `sampling::median`), run label `median`.

- [x] **Step 2: Run test to verify it fails**
  Run: `cargo test --test test_bench median_row`
  Expected: FAIL.

- [x] **Step 3: Implement**
  `enum RunId { Nth(usize), Median { runs: usize } }` on `Row` in place of `run: usize`. `fn median_row(rows: &[Row]) -> Row`: clones the first row's labels, then for every `Option<Duration>` field takes `sampling::median` over the present values, for `rss_settled` the median of the present `u64`s, for sample counts the minimum, for bools `all`. Main loop: read `CLJ_PULSE_BENCH_RUNS` (default 1, must parse as ≥ 1); per server run cold once, then warm N times; when N > 1 push the median row after the warm runs. `print_json` emits `"run": 1` or `"run": "median", "runs": N`. Summary prints the median row labelled `median`.

- [x] **Step 4: bb.edn**
  The `bench` task adds `"CLJ_PULSE_BENCH_RUNS"` to `:extra-env` when the env var is set (`(System/getenv "CLJ_PULSE_BENCH_RUNS")`); doc string mentions it.

- [x] **Step 5: Run tests**
  Run: `cargo test --test test_bench` then `bb check`.
  Expected: PASS.

- [x] **Step 6: Commit**
  `git commit -m "bench: repeat warm runs and report medians"`

  > Deviation: the Task 2 codex review found that once the project probe had landed, nothing drained the channel and the gate line never reached the stash; fixed in `bench: keep receiving while the library gate is shut` (the poll waits through `quiet_for` with an empty method list).

### Task 4: Smoke the harness on clj-kondo

- [x] **Step 1: Run**
  Run: `CLJ_PULSE_BENCH_RUNS=2 bb bench clj-kondo`
  Expected: rows for clj-pulse cold, warm 1, warm 2, clojure-lsp cold, warm 1, warm 2, plus two median rows. clj-pulse `all dependencies navigable` is Some and ≥ the librariesChanged time; `clj-kondo finished` is Some for both servers; the candidate that landed is named; no panic.

- [x] **Step 2: Fix anything the smoke run shows** and commit as `bench: smoke fixes`.

  > Smoke run (142 s): every row populated, `datalog/parse` carried the library row on both servers, medians printed. clojure-lsp's "clj-kondo finished" equalled its settle time, i.e. the last publication plus the 2 s quiet window; `quiesce` now returns the last activity time and that row uses it. Also folded in the Task 3 codex finding (wrong-dialect flag aggregates with `any`).

### Task 5: Docs structure

**Files:**
- Modify: `docs/PERFORMANCE.md`, `README.md`, `docs/MEMORY.md`, `CLAUDE.md`, `docs/DEV_SETUP.md`

- [x] **Step 1: PERFORMANCE.md**
  Intro rewritten around the timeline; per-corpus tables with the four columns and six rows; "what the tables do not say" trimmed to: different work at startup, clojure-lsp's faster settled definition, the definition row is measured on the largest file, clj-kondo finished means the dependency cache warm for clj-pulse and settle for clojure-lsp, one box / N runs. Leave cell values as `—` with a `<!-- filled by Task 6 -->` marker.

- [x] **Step 2: README**
  Metabase only; rows first navigation, all dependencies navigable, clj-kondo finished, memory, definition median; cold and warm columns. Same marker.

- [x] **Step 3: MEMORY.md**
  Method section updated: the three metrics and their signals, the candidate set, `CLJ_PULSE_BENCH_RUNS`, medians. Tables keep settled and both edit rows. Same marker.

- [x] **Step 4: CLAUDE.md and DEV_SETUP.md**
  The `bb bench` paragraph describes the new rows, the run order with N warm runs, and `CLJ_PULSE_BENCH_RUNS`.

- [x] **Step 5: Commit**
  `git commit -m "docs: bench tables around the startup timeline"`

  > Note for Task 6: the README "Highlights" bullet also quotes the first-definition time and memory; update it from the same run.

### Task 6: Record the numbers

- [x] **Step 1: Run the full bench**
  Run: `CLJ_PULSE_BENCH_RUNS=3 bb bench 2>&1 | tee .tmp/bench-$(date +%Y%m%d).log`
  Expected: both corpora complete; keep the `BENCH_JSON` lines.

- [x] **Step 2: Fill the tables**
  From the median rows (warm) and the cold rows: PERFORMANCE.md, README, MEMORY (with date, machine, versions, commit, and the candidate site that landed). Remove every marker.

- [x] **Step 3: `bb check`**, then commit: `git commit -m "docs: record the bench run"`.

  > Deviation: the first full run left clj-pulse's metabase library row at n/a — every candidate was a project facade namespace (`potemkin/import-vars` re-exports, which `namespace_file` rejects because the file defines nothing) or a library re-export clj-pulse cannot follow. `third_party_sites` now excludes any namespace the project has a file for (`namespace_in_project`), the set is ten candidates, and an ignored `bench_probes` test prints the discovery without a server. Committed as `bench: third-party candidates skip every project namespace, ten of them`; the recorded run is the rerun after it.

---

## Completion summary

**Status: complete.** Branch `bench-timeline`, recorded run in
`.tmp/bench-20260921-recorded.log` (not committed).

**Implemented**

- `tests/common/sites.rs`: `third_party_sites` (one site per namespace,
  `clojure.*` and every project namespace excluded via `namespace_in_project`,
  facades included), `namespace_paths` shared with `namespace_file`;
  `library_site` removed.
- `tests/common/mod.rs`: `LspClient.received` — receipt time per stashed
  message.
- `tests/test_bench.rs`: three startup timeline metrics (first navigation,
  all dependencies navigable behind the library stage gate, clj-kondo
  finished from `lintStatus` warming transitions or clojure-lsp's last
  publication before quiet), `StageWatch` processing each message once at
  its receipt time, `Settle` with `last_activity`, ten library candidates,
  `CLJ_PULSE_BENCH_RUNS` with a median row, `RunId` in the JSON, the
  `bench_probes` ignored test, two unit tests run by `bb check`.
- `bb.edn` passes `CLJ_PULSE_BENCH_RUNS` through.
- Docs: PERFORMANCE.md, README (Performance section and the Highlights
  bullet), MEMORY.md bench section, CLAUDE.md, DEV_SETUP.md, all filled from
  the 2026-09-21 run.

**Deviations (all recorded inline above)**

1. Gate is the library stage line, not `librariesChanged` (fires before
   indexing too).
2. Receipt times on the client instead of observation times.
3. Task 1 migrated its caller in the same commit.
4. Poll waits through `quiet_for` so the gate line is received while nothing
   is asked (codex, Task 2).
5. Median row keeps a wrong-dialect warning any run raised (codex, Task 3).
6. clojure-lsp's "clj-kondo finished" is its last publication, not settle
   plus the quiet window (smoke run).
7. Candidates exclude every project namespace, facade or not, and there are
   ten (first full run: metabase's clj-pulse row was n/a).

**Issues**

- clojure-lsp's metabase cold start read 375 s against 293 s on 2026-09-10;
  the box was shared during the run. Noted in MEMORY.
- The codex branch review had to be re-run without a prompt (`--base`
  rejects one on this codex version).

8. Warm repeats after the first measure the timeline and RSS only; the
   latency medians come from warm run 1 (the user asked for a shorter bench:
   23 min at three repeats, of which the repeated sampling was ~5 min).

**What the plan could have specified better:** the candidate predicate. "Has
no file in the project" was written as `namespace_file` is `None`, which also
matches a project facade namespace; the plan should have said "no file for
that namespace at all", and named `import-vars` re-exports as a reason the
set needs to be wide.
