# Dependency Readiness Benchmark Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure navigation across resolved Clojure dependencies with honest coverage, alongside project readiness and cold/warm performance.

**Tech Stack:** Rust integration harness, tree-sitter, ZIP, JSON-RPC, Babashka.

---

## Design

The user approved replacing the single-library headline measurement with
navigation across tested dependencies. Keep the existing clojure.string probe
as a smoke check, and keep settled time for memory/latency sampling.

Before timing, resolve the pinned corpus classpath and inspect its entries.
Choose one ordinary public top-level definition per Clojure source-bearing
JAR or external source directory. Use syntax inspection independent of either
server's index. Report entries with no Clojure sources separately from entries
with sources but no safe probe, and report discovery errors. Deduplicate paths
and namespace shadowing according to classpath order. Do not claim exhaustive
symbol coverage. Project source roots are excluded from dependency coverage.

Open one synthetic Clojure buffer requiring the selected namespaces and
referencing their vars. Both servers receive identical probes. Poll them with
bounded requests alongside the project and single-library probes. Validate
returned artifact/source path and definition position, not just a non-null
answer. Only report dependency readiness when every selected probe resolves;
otherwise emit null plus resolved/total counts and each unresolved target,
including errors/timeouts. Empty probe sets never become a zero-time success.
Keep the overall startup deadline authoritative. Close the synthetic buffer
before measuring settled memory and edit latency.

Cold clears each server's index cache and the project clj-kondo cache; warm
reuses the preceding cold run's caches. Classpath resolution and downloads
remain untimed. Preserve existing JSON keys and add coverage, per-probe
outcomes, discovery exclusions/errors, and the new readiness time.

## File Structure

- `tests/bench/dependencies.rs`: independent dependency discovery, expected
  locations, coverage tracking, and focused tests.
- `tests/common/mod.rs`: optional initialization settings for the benchmark.
- `tests/test_bench.rs`: startup polling, JSON/table reporting, synthetic buffer
  lifecycle, cold cache handling.
- `bb.edn`: capture the exact prepared classpath and pass it to the benchmark.
- `README.md`, `docs/PERFORMANCE.md`, `docs/MEMORY.md`: metric definitions and
  measured cold/warm results with environment and coverage.
- `AGENTS.md`, `docs/ROADMAP.md`: benchmark contract and task status.

### Task 1: Implement and validate dependency readiness

- [x] Add fixture-based tests for independent probe selection, JAR/directory
      inventory, exclusions, duplicates, location checking, incomplete/empty
      coverage, and timeout/error handling.
- [x] Implement discovery and bounded polling; extend text/JSON output and
      capture the prepared classpath. Keep changes out of production code.
- [x] Run `mise exec -- cargo test --test test_bench` and `mise exec -- bb check`.
- [ ] Run `mise exec -- bb bench` on both pinned corpora. Inspect coverage and
      missing probes; never tune the probe set based on which server succeeds.
- [ ] Update public performance docs and README with measured cold/warm results.
      Keep the README short, no Rust or roadmap link.
- [ ] Run an independent Codex review, address substantive findings, verify
      changed behavior again as needed, and complete the plan/roadmap.

Server/editor gates and soak are unnecessary: no server behavior, extractor,
index, watcher, or document store changes. The real benchmark run exercises the
changed harness end to end. Leave changes uncommitted for the current branch's
review; no publishing or release changes.

## Implementation findings

- The first real run found five dependency-version mismatches: clojure-lsp
  also discovers the corpus's bb.edn. Set its documented `project-specs`
  initialization option to the same `clojure -A:dev:test -Spath` command used
  by clj-pulse and preparation. Otherwise keep server defaults; disclose this
  selection in reports. The exploratory mismatched run is not publishable.
- A blocked stdin write bypassed the initial request timeout. Use nonblocking,
  deadline-aware benchmark writes, tested against a stopped real server with
  more data than its pipe can hold. Kill/reap an unresponsive peer instead of
  sending cleanup into a potentially partial frame.
- If startup probes remain incomplete, settled time is unavailable: waiting
  out the probe deadline must not be presented as actual background work time.

- The second completed review found resource precedence was wrong when an
  earlier JAR had foo.cljc and a later one had foo.clj. Discovery now resolves
  .clj before .cljc across the whole classpath, then uses classpath order within
  each extension. A focused JAR regression covers both rules.
- The review retry initially hit the Codex CLI usage limit; the subsequent
  review ran successfully. Both substantive review findings were addressed.

- Final `bb check` passed. An earlier run hit the existing large-buffer lint
  timing test; both an isolated rerun and the full final suite passed.
- Benchmark preparation exhausted disk space while fetching Metabase.
  `cargo clean --profile dev` reclaimed regenerable build artifacts; preparation
  then succeeded. No source files or benchmark data were removed.
- The Metabase run exposed incorrect library source selection in production:
  four probes returned a shadowed resource or another language's source.
  Recorded the reproducible cases in `docs/backlog/dependency-navigation-source-precedence.md`.
  Production indexing remains outside this plan. The entry stays uncommitted
  with the requested reviewable work; commit it separately when committing.
- Metabase's `.lsp/config.edn` overrides initializationOptions. The first full
  run therefore used a larger classpath on clojure-lsp and timed out during
  initialize. Override only `project-specs` in the disposable corpus config
  with `try/finally` restoration, and seed the newest stage-2 `.cp` from the
  captured classpath. Initialization now uses deadline-aware transport too;
  failure yields an incomplete row and does not abort later runs.
- After the initialization and config fixes, `CARGO_INCREMENTAL=0 mise exec --
  bb check` passed, including all nine benchmark regression tests. A separate
  Babashka check verified config preservation, creation/removal, and restoration
  after an exception. Incremental compilation was disabled to fit the disk.
