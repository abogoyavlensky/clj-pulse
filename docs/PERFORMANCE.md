# Performance

[Back to README](../README.md)

`bb bench` runs clj-pulse and [clojure-lsp](https://clojure-lsp.io) through the
same client, over stdio, with the same requests, on two corpora pinned by
commit. Both servers run at their defaults, with clj-kondo installed. Every
metric is behavioral: a startup row is the wait until a
`textDocument/definition` lands where it should, and memory and the latency
medians are sampled only after the server has gone quiet with no child process
still working.

The first three rows are the timeline a user sits through after opening a
project:

1. **First navigation** — a definition into the project's own source lands.
2. **All dependencies navigable** — a definition into a third-party
   dependency lands, asked only once the server has said every classpath
   entry is indexed.
3. **clj-kondo finished** — the last of the startup work: for clj-pulse the
   clj-kondo dependency-cache warm, for clojure-lsp the analysis its startup
   consists of.

Cold is the first open of a fresh checkout, with the server's caches cleared;
warm is the next open, with what the first run left behind. Warm numbers are
the median of three runs, cold is one run.
`CLJ_PULSE_BENCH_RUNS=3 bb bench` on a 2021 MacBook Pro (Apple M1 Pro, 16 GB,
macOS Tahoe 26.5.2), 2026-10-27: clj-pulse 0.5.6 at `8944e89`,
clojure-lsp 2026.07.06-14.34.19, clj-kondo v2026.08.04, metabase at
`42a8e9f7`, clj-kondo at `13a32d1c`. Keystroke cells give the wait each server
puts before it lints a change: clj-pulse 50 ms, clojure-lsp 75 ms (50 ms for
the change plus 25 ms for diagnostics).

**metabase** (1 400+ files, 51 082 symbols):

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First navigation | 2.4 s | 1.8 s | 118 s | 26 s |
| All dependencies navigable | 3.9 s | 2.1 s | 118 s | 26 s |
| clj-kondo finished | 25 s | 17 s | 118 s | 26 s |
| Memory once settled | 88 MiB\* | 523 MiB\* | 2 647 MiB | 1 766 MiB |
| Definition (median of 20) | < 1 ms | < 1 ms | 5 ms | 2 ms |
| Keystroke -> diagnostics, 251 KiB file | 260 ms (50 ms debounce) | 261 ms (50 ms debounce) | 349 ms (75 ms debounce) | 389 ms (75 ms debounce) |

**clj-kondo** (400 files, 2 252 symbols):

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First navigation | 751 ms | 212 ms | 8.0 s | 1.2 s |
| All dependencies navigable | 1.4 s | 212 ms | 8.0 s | 1.2 s |
| clj-kondo finished | 5.0 s | 902 ms | 8.0 s | 1.2 s |
| Memory once settled | 101 MiB | 95 MiB | 324 MiB | 272 MiB |
| Definition (median of 20) | < 1 ms | < 1 ms | 1 ms | 1 ms |
| Keystroke -> diagnostics, 233 KiB file | 217 ms (50 ms debounce) | 214 ms (50 ms debounce) | 290 ms (75 ms debounce) | 290 ms (75 ms debounce) |

\* macOS reported clj-pulse's metabase memory erratically on this run: the
three warm runs read 545, 523 and 255 MiB and the cold run 88 MiB, against
462 MiB before and after the change on the Linux baseline box (`/proc`, not
`ps`). Treat the two clj-pulse cells as unreliable until a re-run settles
them; the clj-kondo corpus rows were steady.

What the tables do not say:

- The two servers do different work at startup. clojure-lsp analyzes the
  whole classpath through clj-kondo before it answers anything, so its three
  timeline rows are one moment. clj-pulse indexes the project's sources
  first, then the classpath in the background, and warms clj-kondo's
  dependency cache last; navigation is available at each step, which is why
  its rows are three different moments. Dependencies and `.cpcache` are
  prepared before timing both cold and warm runs.
- "clj-kondo finished" is not a wait for anything: with clj-pulse, definitions
  and completion answer throughout, and lint diagnostics arrive from the
  built-in lints until clj-kondo's cache is warm. A workspace without
  clj-kondo has no such row.
- The definition row is measured on the largest file in each repo. clj-pulse
  keeps each open document's analysis per revision, as clojure-lsp keeps its
  own stored one, and answers from it: a request is a scan of that analysis
  plus the JSON-RPC round trip, below the bench's 1 ms resolution. Before
  that cache (0.5.4) the same row read 11 ms on metabase and 7 ms on the
  clj-kondo corpus.
- The keystroke row is measured on the largest file under clj-pulse's
  `:kondo {:live-max-kb 256}`, so clj-kondo runs on every keystroke for both
  servers. Both servers debounce a change before linting it: clj-pulse waits
  50 ms and clojure-lsp 50 ms plus 25 ms for diagnostics; the tables show
  each wait. clj-pulse used to wait 300 ms (0.5.4: 570 and 524 ms), which
  was most of what separated the two. The bench cannot confirm from a
  publication alone that clj-kondo ran, so the tier it reports for these
  rows reads "native only" for both servers. Above the threshold clj-pulse
  lints a keystroke with its built-in tier alone: 87 ms on the 452 KiB
  file, against 501 ms for clojure-lsp, which lints it with clj-kondo.
- The same run on a 5-core Linux container keeps the ratios, with every
  number slower; it is the regression baseline and lives in the records.

The full method, the per-run rows, the Linux run, and the caveats in detail
are in [benchmark records](MEMORY.md#benchmark-against-clojure-lsp). To
reproduce: `CLJ_PULSE_BENCH_RUNS=3 bb bench`.
