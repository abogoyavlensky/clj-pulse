# Release Plan 2: Benchmark Against clojure-lsp Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn `bb bench` into a reproducible, honest comparison of clj-pulse and clojure-lsp on two real corpora, and publish the result in a README "Performance" section with every caveat next to the numbers (ROADMAP Milestone 5, second of three release plans). Runs after release plan 1 so the numbers come from the code that ships.

**Tech Stack:** Rust (`tests/test_bench.rs`, the shared `LspClient` in `tests/common/mod.rs`), babashka (`bb.edn`), the clojure-lsp native binary pinned by version. No server changes.

---

## Design

### What exists

`bb bench` clones metabase, runs the release binary through `LspClient::start_production`, and prints one table: project and library index time, RSS after each, didOpen and didChange to diagnostics, definition latency. The baseline lives in `docs/MEMORY.md`.

### Corpora

Two, chosen for being real, popular, deps.edn projects that a reader can clone:

| Name | Repository | Why |
|---|---|---|
| `metabase` (large) | `metabase/metabase`, shallow clone | about 1 400 source files, 43 000 symbols, the existing corpus |
| `clj-kondo` (medium) | `clj-kondo/clj-kondo`, shallow clone | about 200 source files, JVM Clojure plus `.cljc`, a codebase most readers know |

`bb bench` takes the corpus name as its argument (`bb bench metabase`, `bb bench clj-kondo`, default both) and exports `CLJ_PULSE_BENCH_ROOT` and `CLJ_PULSE_BENCH_CORPUS`. Both corpora are pinned by commit in `bb.edn` and checked out at that commit after the clone, so a re-run months later measures the same code.

### The comparison

The harness gains a second server: clojure-lsp, driven through the same `LspClient` over stdio with the same requests. `LspClient::start_binary(path, root, env)` spawns an arbitrary binary; `start_production` becomes a call to it with the clj-pulse path. `bb bench` downloads the clojure-lsp native archive for the host platform into `.tmp/bench/tools/clojure-lsp-<version>/` on first use, version pinned in `bb.edn`, sha256 checked against the release's checksum file; `CLJ_PULSE_BENCH_CLOJURE_LSP` points the test at it and the comparison is skipped with a note when unset.

Metrics have to mean the same thing for both servers, and the two log different things, so the synchronization changes from log lines to behavior:

| Metric | Definition, both servers |
|---|---|
| Time to first definition | from `initialize` until a `textDocument/definition` on a project symbol returns a location, polled every 100 ms |
| Time to first library definition | the same, on a symbol from a JAR dependency (`clojure.string/join`) |
| RSS after startup | resident memory of the server process in the *settled* state defined below |
| Definition latency | median of 20 requests on the largest source file's first alias-qualified symbol (the existing rule) |
| didChange to diagnostics | median of 20 single-character inserts at the end of that file, time to the next `publishDiagnostics` |

**Settled state.** Two answered definitions do not mean indexing is over: `docs/MEMORY.md` records stage 3 re-resolving and re-indexing after stage 2 has already answered. RSS and the latency medians are sampled only once the server is settled, defined per server:

- clj-pulse: the stage-3 log line (`full classpath indexed` or `classpath resolution failed`, the existing `STAGE3_LINES`) has arrived and no `clojure` child of the server remains (checked through `/proc/<pid>/task/*/children` on Linux, `pgrep -P` on macOS), then 2 s of no further log lines.
- clojure-lsp: its `$/progress` stream for the analysis has reported `end` (it reports classpath analysis as work-done progress; confirm the token shape in its source before relying on it) and no `publishDiagnostics` has arrived for 2 s.

Missing settle within the ceiling is reported as such in the row, never silently sampled.

**Dependency preparation, untimed.** Before any timed run of either server, the corpus is prepared once: `clojure -A:dev:test -Spath` in the corpus root so `.cpcache` and `~/.m2` hold every artifact, and `clj-kondo --lint <classpath> --dependencies` is *not* run, since that is part of what a cold clojure-lsp run does itself. The prepared state is the same for both servers and both temperatures.

**Cold and warm**, defined per server and restored before each cold run: cold deletes `.clj-pulse/jar-cache` for clj-pulse, and `.lsp/.cache` plus `.clj-kondo/.cache` for clojure-lsp; `.cpcache` stays, being part of the prepared state. Warm is the second run with everything the first left behind. Run order within a corpus is fixed (clj-pulse cold, clj-pulse warm, clojure-lsp cold, clojure-lsp warm), and the table states it. Each configuration runs once; the harness prints the table and a JSON line per row so a later run can be diffed.

**Diagnostics timing.** Each didChange carries a document version, and `publishDiagnostics` carries it back. A sample is the time from the edit to the publication *with that version*; publications for older versions or from startup are skipped. A sample that does not arrive within the request timeout is recorded as missing, the row shows `n/20` samples, and a row with fewer than 15 samples prints its median with a warning rather than as a clean number.

**The clj-kondo row.** Besides the largest file, which sits above `:live-max-kb` and so measures the native tier only, the harness picks the largest source file *under* the threshold (deterministic: size descending, path ascending, first under 256 KiB) and measures didChange to diagnostics there. That row is labeled "with clj-kondo" only when the publication actually carried a diagnostic with `source: "clj-kondo"` or the server's `clojurePulse/lintStatus` reported the kondo tier active; otherwise it is labeled "native only" and the README says why.

**Definition validation.** A definition answer counts only when it lands where it should: the project symbol's own file, and for the library case a `jar:` URI (clj-pulse) or clojure-lsp's equivalent (`jar:` or `zipfile:` depending on its `:dependency-scheme`). A wrong or empty answer is a retry, not a sample.

**Timeouts.** The shared client's 20 s request timeout is made configurable (`LspClient::with_request_timeout`), and the bench sets it to the indexing ceiling for the startup polls.

### Honesty rules, stated in the README

- clojure-lsp analyzes the whole classpath through clj-kondo at startup and caches it under `.lsp`; its cold startup is doing more work than ours, and the "first library definition" row is where that shows. The table says so.
- clj-pulse's didChange number on the large file is native-only because the file is above `:live-max-kb`; the row is footnoted, and a second row on a file under the threshold shows the number with clj-kondo included.
- Versions of both binaries, the corpus commits, the machine, and the date sit above each table.
- Numbers come from a Linux CI-shaped box and from the maintainer's macOS machine; both are shown, neither is called "typical".
- The reproduce command is one line: `bb bench <corpus>`.

### README section

"Performance", after Features and before Linting: one paragraph on what is measured, the two warm tables (large and medium) with both servers, the caveat list, and a link to `docs/MEMORY.md` for cold numbers and history. No adjectives.

## File Structure

Modify:

- `tests/common/mod.rs`: `start_binary`, `start_production` delegating to it.
- `tests/test_bench.rs`: corpus and server parameters, behavior-based synchronization, both servers, JSON row output, cold and warm runs.
- `bb.edn`: corpus argument, pinned commits, clojure-lsp download with checksum, both-corpora default.
- `.gitignore`: `.tmp/` is already ignored.
- `README.md`: Performance section. `docs/MEMORY.md`: full tables and the rules above. `AGENTS.md`: `bb bench` description. `docs/ROADMAP.md`.

## Tasks

### Task 1: Corpus parameter and pinned commits

**Files:**
- Modify: `bb.edn`, `tests/test_bench.rs`

- [x] **Step 1: Parametrize**
  `bb bench` accepts `metabase`, `clj-kondo`, or nothing for both; clones shallowly and checks out the pinned commit; exports the two variables. The test reads `CLJ_PULSE_BENCH_CORPUS` for the report header.

- [x] **Step 2: Run**
  Run: `bb bench clj-kondo`
  Expected: the existing table prints for the new corpus within the ceiling.

- [x] **Step 3: Commit**
  `git commit -m "Bench two pinned corpora"`

> Deviation: metabase is pinned at `42a8e9f7` (the commit already checked out
> under `.tmp/bench/`, the one the MEMORY baseline was measured on) and
> clj-kondo at `13a32d1c`. The corpus is fetched by commit into a shallow
> `git init` + `git fetch --depth 1` checkout rather than `git clone --depth 1`,
> which cannot pin.

### Task 2: Behavior-based metrics

**Files:**
- Modify: `tests/test_bench.rs`, `tests/common/mod.rs`

- [x] **Step 1: Replace log-line waits**
  Implement the five metrics, the settled-state check, the version-matched diagnostics sampling with missing-sample reporting, the below-threshold clj-kondo row with its label rule, definition validation, and the configurable request timeout, for clj-pulse only. Keep the old log-based rows for one run to check the new "first definition" numbers agree with "Indexed" plus a few hundred milliseconds.

- [x] **Step 2: Run both corpora**
  Run: `bb bench`
  Expected: two tables; each JSON row parses.

- [x] **Step 3: Commit**
  `git commit -m "Measure the bench by observable behavior, not log lines"`

> Deviation: the startup probes are chosen from the corpus's *top-level*
> `src`/`test` only, the namespace-to-file match is anchored at that source root,
> and the target file must contain a `(def… name)` for that name. Without all
> three the probe picks something no server can resolve — clj-kondo's `corpus/`
> holds deliberately broken sample projects (with their own `src/` dirs),
> clj-kondo has a `src/clj_kondo/impl/types/clojure/string.clj` that a suffix
> match reads as `clojure.string`, and metabase's `metabase.events.core`
> re-exports its vars through `potemkin/import-vars`, so a definition on one
> lands anywhere but the file the require names.
> Deviation: the library expectation is `clojure/string.clj`, `.cljc` *or*
> `.cljs`. clj-pulse answers `clojure/string.cljs` out of the ClojureScript jar
> for a `.clj` file when the classpath carries both (a real finding, filed in
> the ROADMAP backlog); either is a definition inside a dependency, which is
> what the metric times.
> Deviation: `clojurePulse/lintStatus` qualifies a "native only" label but never
> produces a "with clj-kondo" one. The plan allowed it as positive evidence, but
> it reports whether the *engine* is live, not whether it ran for this pass — on
> the 452 KiB file, which is above `:live-max-kb`, it says `kondo+native` while
> clj-kondo sits out every keystroke, so trusting it mislabels exactly the row
> the plan wants footnoted.
> Deviation: the settle check gets its own ceiling rather than what is left of
> the startup one, so a probe that never resolves cannot also make the settle
> check report a failure. It also waits on "no child process of the server",
> which on metabase means the ~45 s clj-kondo dependency-cache warm — work the
> server is doing, however quiet its log has gone.

### Task 3: clojure-lsp in the harness

**Files:**
- Modify: `bb.edn`, `tests/common/mod.rs`, `tests/test_bench.rs`

- [x] **Step 1: Download and pin**
  `bb bench` fetches the pinned clojure-lsp native release for the platform, verifies the checksum, and exports `CLJ_PULSE_BENCH_CLOJURE_LSP`.

- [x] **Step 2: Drive it**
  `start_binary`; the bench runs the same metric set against clojure-lsp when the variable is set. Read clojure-lsp's `initialize` needs (it wants `rootUri` and may need `initializationOptions` for `:dependency-scheme`, check its docs) and match them; do not tune either server beyond defaults.

- [x] **Step 3: Preparation, cold and warm**
  The untimed preparation step in `bb bench`, the per-server cache clearing, the fixed run order, and the clojure-lsp settled-state check; run each configuration and print all.

- [x] **Step 4: Run**
  Run: `bb bench`
  Expected: four tables (two corpora, cold and warm), both servers in each. If clojure-lsp's cold run exceeds the 120 s ceiling on metabase, raise the ceiling for that server rather than dropping the row; the number is the point.

- [x] **Step 5: Commit**
  `git commit -m "Compare against clojure-lsp in the bench"`

> Deviation: cold also clears clojure-lsp's *global* cache. It keeps ~150 MiB
> of JDK-source analysis under `$XDG_CACHE_HOME/clojure-lsp`, outside the
> project, so the plan's two project directories were not enough: a "cold" row
> was reusing an earlier run's analysis, and metabase's clojure-lsp startup
> swung between 293 s and 700 s. The bench now points `XDG_CACHE_HOME` inside
> the corpus and clears it with the rest; the two runs after that agree within
> a few percent. Found by the codex review.
> Deviation: clojure-lsp declares `TextDocumentSyncKind.Full`, so the bench
> sends it the whole buffer per keystroke and clj-pulse an incremental range —
> each what it asked for. Sending a range to a full-sync server would have made
> it lint a one-character document and report a meaningless number.
> Deviation: clojure-lsp emits no `$/progress` for its analysis even with
> `window.workDoneProgress` advertised (checked against the binary), and it
> echoes no document version in `publishDiagnostics`. So its settle check rests
> on the 2 s quiet window plus the no-child-process rule, and its edit rows time
> the next publication for the file rather than a version-matched one. Both are
> stated in the row and in MEMORY.md.
> Deviation: clojure-lsp gets a 900 s ceiling, clj-pulse keeps 120 s. A cold
> clojure-lsp run on metabase needs ~5 minutes before it answers anything, and
> the plan says the number is the point.

### Task 4: Publish

**Files:**
- Modify: `README.md`, `docs/MEMORY.md`, `AGENTS.md`, `docs/ROADMAP.md`

- [x] **Step 1: Record**
  MEMORY.md: the full tables with versions, commits, machine, date, and the honesty rules. The README section is not published until the maintainer has run `bb bench` on the macOS machine and that table is in MEMORY.md too; the plan stays open at this step until then.

- [x] **Step 2: README**
  The Performance section as designed. Use /writing-clearly; no superlatives.

- [x] **Step 3: Roadmap and invariants**
  AGENTS.md: `bb bench` now compares two servers and takes a corpus. ROADMAP Milestone 5: tick the benchmark item, status `done`.

- [x] **Step 4: Verify and commit**
  Run: `bb check`
  Expected: PASS.
  `git commit -m "Publish the benchmark against clojure-lsp"`

> Deviation: the README section is published with the Linux table alone rather
> than held until the macOS run, at the maintainer's direction. It says so in
> the section ("One Linux container, one run each. macOS numbers are not in
> yet") and in the ROADMAP item, and the macOS column is an added row, not a
> rewrite.

---

## Completion summary

**Status: complete** (2026-09-10), except the macOS table the maintainer adds
after running `bb bench` on the Mac.

`bb bench` now compares clj-pulse with clojure-lsp on two corpora pinned by
commit, four configurations each (both servers, cold and warm), with every
metric defined by what a client can observe rather than by a log line. The
harness fetches the pinned clojure-lsp release and checks its sha256, prepares
the corpus classpath untimed, clears each server's caches before its cold run,
and prints a table plus a `BENCH_JSON` line per row. `docs/MEMORY.md` holds the
full tables and the method; `README.md` gained a "Performance" section with the
warm numbers and the caveats beside them.

What the numbers say on this box: clj-pulse answers its first definition on
metabase in 3.1 s warm against clojure-lsp's 60 s (3.4 s against 293 s cold),
in 353 MiB against 1 798 MiB; clojure-lsp answers a definition faster once it
is up (7 ms against 34 ms) from a fuller analysis, and lints a 452 KiB buffer
per keystroke in 1 371 ms against our 381 ms native-tier pass.

Two things the bench found rather than measured, both filed: a `.clj` file
navigates into the ClojureScript copy of `clojure.string` when both jars are on
the classpath (ROADMAP backlog), and clj-pulse spends ~50 s after startup on
metabase warming the clj-kondo dependency cache in a child process - background
work that does not block an answer, but it is what "settled" waits for.

**Deviations** are noted inline under each task above: probe selection needed
three extra rules to pick a symbol either server can resolve; `lintStatus` was
demoted from positive to negative evidence for the kondo tier label;
clojure-lsp's global cache had to be isolated for cold to mean cold; full vs
incremental document sync is now per server; clojure-lsp gets its own ceiling;
and the README ships with Linux numbers alone.

**What the plan could have specified better:** it defined the probes by rule
("the largest source file's first alias-qualified symbol") without asking
whether a server can *answer* at that position. Three of the four re-runs in
task 2 went into that: a corpus of deliberately broken sample files, a project
file named `clojure/string.clj`, and a `potemkin/import-vars` facade all
produce a position that never resolves, and each one costs a full ceiling to
discover. A plan step that said "validate the probe against the running server
before timing anything, and pick another site when it does not answer" would
have collapsed those iterations into one. The same applies to the second
server: the plan assumed `$/progress` and version-matched diagnostics without a
fallback, and both assumptions turned out to be wrong for clojure-lsp.
