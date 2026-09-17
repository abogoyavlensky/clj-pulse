# Performance

[Back to README](../README.md)

`bb bench` compares clj-pulse and clojure-lsp through the same JSON-RPC client
on pinned Metabase and clj-kondo checkouts. It measures cold and warm startup,
dependency navigation, memory, and request latency. Each run prints a summary
and `BENCH_JSON` records with raw measurements and probe outcomes.

## Recorded runs

Recorded 2026-09-17 on one Linux x86-64 container: 2 CPUs (Intel Skylake),
3.7 GiB RAM, and 4 GiB swap enabled. clj-pulse 0.5.1 (`0932829`, release build)
was compared with clojure-lsp 2026.07.06-14.34.19 using the revised benchmark.
These are one cold run and one warm run per server, not averages across starts.
They are not directly comparable with the older 5-core, 11 GiB measurements.

The corpora are pinned to Metabase `42a8e9f77d330c90c5a4d71927fed2823897f949`
and clj-kondo `13a32d1caf6278b7e4a8c4ad4936d4fc33db6cf2`.

### clj-kondo

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First project definition | 1.4 s | 1.3 s | 22.2 s | 3.3 s |
| Tested dependency navigation | 4.3 s | 1.4 s | 22.3 s | 3.3 s |
| Server settled | 26.2 s | 6.7 s | 24.3 s | 5.3 s |
| Memory once settled | 74 MiB | 61 MiB | 274 MiB | 268 MiB |
| Definition request | 31 ms | 29 ms | 12 ms | 10 ms |
| Keystroke to diagnostics (184 KiB) | 971 ms | 949 ms | 1.4 s | 1.2 s |

All four runs resolved **36/36 probes**. Of 102 classpath entries, 39 contained
Clojure dependency sources; three had no eligible unshadowed public definition.
The other entries were six project source roots and 57 entries without `.clj`
or `.cljc` sources. There were no discovery errors. The latency rows are medians
of 20 successful requests or edits in `src/clj_kondo/impl/analyzer.clj`.

## Methodology

Startup timing begins before initialization. Project readiness is the first
successful definition request in a project source file. Dependency readiness
is the time when **every selected dependency probe** has returned the expected
source and position. The old single `clojure.string` probe remains in the raw
output as a smoke check; it does not measure dependency coverage.

A server that completes dependency analysis during initialization can have
nearly identical project and dependency times. The broader probe set still
matters: it verifies coverage and catches wrong-source answers that the single
smoke check misses.

Preparation captures `clojure -A:dev:test -Spath`. clojure-lsp receives that
same command through its documented [project-specs setting](https://clojure-lsp.io/settings/#classpath-scan).
The task temporarily overrides that key in the benchmark checkout's
`.lsp/config.edn`, then restores the original file. This prevents project
settings or `bb.edn` from selecting different dependencies. Other settings are
unchanged. Pulse's stage-2 cache is seeded with the same prepared classpath. Classpath preparation and dependency downloads
are outside the timed run.

### Dependency coverage

Before starting either server, the harness independently inspects the prepared
classpath. It selects one ordinary public top-level `def`, `defn`, or
`defmacro` per eligible Clojure dependency JAR or external source directory.
Selection is deterministic and does not use either server's index. Resource
resolution prefers `.clj` over `.cljc` across the classpath, then classpath
order within an extension.

Private vars, var metadata, reader-conditional definitions, and custom defining
macros are excluded from selection. Java-only and ClojureScript-only entries
have no eligible Clojure source. Project source roots are counted separately.
Dependencies with Clojure sources but no safe candidate are reported as
exclusions, alongside discovery errors.

Both servers receive the same unsaved buffer requiring the selected namespaces
and referring to their vars. The harness checks the exact artifact or source
file and the definition position, rather than accepting any non-null result.
It keeps at most eight startup requests in flight, rotates through pending
probes, and retries no faster than every 100 ms.

Incomplete coverage produces an unavailable readiness time, resolved/total
counts, and each unresolved target's last failure. An empty probe set never
becomes a zero-time success. Startup reads and writes, including initialization, have deadlines. An
initialization error or timeout produces an incomplete row and the remaining
runs continue, including when a server stops consuming its input. Deadline-aware writes require Unix.

This measures navigation across the **tested dependencies**, not every symbol
or every library. Read the inventory, exclusions, and errors alongside the
timing; `BENCH_JSON` preserves them all.

### Cold and warm

Cold runs clear each server's analysis cache and the project's clj-kondo cache.
Warm runs reuse the caches left by the preceding cold run. Downloaded
dependencies, the prepared classpath, and the operating system's file cache
remain available. Preparation and source inspection can warm that file cache,
so “cold” means cold analysis caches, not a freshly booted machine.

### Memory and request latency

After startup probing, the synthetic buffer is closed. The harness waits for
background work to settle before sampling memory and taking medians of 20
requests. Settling requires a quiet window of two seconds and no child
processes; clj-pulse's classpath stage logs also participate in that check.

“Server settled” includes background analysis and linting. It is not the time
when navigation first works. If startup probes remain incomplete, settled time
is unavailable: waiting out a probe deadline must not inflate that number.

The servers do different amounts of work at startup. clj-pulse exposes project
navigation while dependencies index. Some request handlers read source on
demand, whereas clojure-lsp retains more analysis; startup and steady-state
latency therefore measure different tradeoffs.

### Keystroke to diagnostics

This measures the full delay from an edit to a diagnostics notification:
debounce, analysis, and delivery. Below the default 256 KiB live-lint limit,
clj-pulse invokes an external clj-kondo process. clojure-lsp embeds clj-kondo
and adds its own linters. The recorded versions also differ: the external
clj-kondo is `2026.08.04`; clojure-lsp embeds `2026.05.26-SNAPSHOT`.
Shared analysis work can produce similar timings;
these measurements do not isolate subprocess overhead.

Above that limit, clj-pulse runs its native lints alone on keystrokes, while
still using clj-kondo on open and save. The 452 KiB Metabase edit therefore
compares different diagnostic coverage. The separate 251 KiB edit stays below
the limit, allowing clj-kondo to run on both sides.

clj-pulse's diagnostic version must match the edit. When clojure-lsp omits the
version, the harness measures the next notification for that file, which could
belong to earlier work. Each server receives its advertised synchronization
format. These limits matter when interpreting small differences.

## Reproduce

```sh
bb bench             # both pinned corpora
bb bench metabase    # large project
bb bench clj-kondo   # smaller project
```

Preparation requires the Clojure CLI and must resolve the classpath successfully.
The task downloads a pinned clojure-lsp binary and verifies its checksum; if that
binary is unavailable, it reports clj-pulse results alone. Historical results
and their environments are retained in [MEMORY.md](MEMORY.md#benchmark-against-clojure-lsp).
