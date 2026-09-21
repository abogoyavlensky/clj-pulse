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
the median of three runs, cold is one run. One Linux container (5 cores,
11 GiB, 2026-09-21): clj-pulse 0.5.4, clojure-lsp 2026.07.06-14.34.19,
clj-kondo v2026.08.04, metabase at `42a8e9f7`, clj-kondo at `13a32d1c`.

**metabase** (1 400+ files, 43 164 symbols):

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First navigation | 4.0 s | 3.5 s | 336 s | 56 s |
| All dependencies navigable | 7.0 s | 4.2 s | 336 s | 56 s |
| clj-kondo finished | 70 s | 47 s | 336 s | 56 s |
| Memory once settled | 368 MiB | 363 MiB | 2 426 MiB | 1 800 MiB |
| Definition (median of 20) | 32 ms | 24 ms | 13 ms | 7 ms |
| Keystroke -> diagnostics, 251 KiB file | 924 ms | 923 ms | 855 ms | 867 ms |

**clj-kondo** (400 files, 2 252 symbols):

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First navigation | 538 ms | 530 ms | 18.5 s | 2.9 s |
| All dependencies navigable | 942 ms | 532 ms | 18.5 s | 2.9 s |
| clj-kondo finished | 11.7 s | 2.3 s | 18.6 s | 2.9 s |
| Memory once settled | 129 MiB | 88 MiB | 272 MiB | 276 MiB |
| Definition (median of 20) | 17 ms | 18 ms | 5 ms | 8 ms |
| Keystroke -> diagnostics, 233 KiB file | 771 ms | 755 ms | 911 ms | 909 ms |

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
- clojure-lsp answers a definition faster once it is up, from a fuller
  analysis. The definition row is measured on the largest file in each repo,
  which is a worst case for clj-pulse and not for clojure-lsp: clj-pulse
  re-reads the open buffer's definitions and usages on every request, while
  clojure-lsp looks the answer up in the analysis it stored at startup. On a
  190-byte metabase file, a definition takes 2.1 ms from either server.
- The keystroke row is measured on the largest file under clj-pulse's
  `:kondo {:live-max-kb 256}`, so clj-kondo runs on every keystroke for both
  servers. What happens above that threshold, where clj-pulse lints a
  keystroke with its built-in tier alone, is in the benchmark records.
- One Linux container per table above. The same run on the maintainer's
  Apple Silicon Mac is below.

## macOS

The same `CLJ_PULSE_BENCH_RUNS=3 bb bench`, 2026-09-22, on an Apple Silicon
Mac (aarch64), clj-pulse 0.5.4 and the same clojure-lsp, clj-kondo and corpus
commits.

**metabase**:

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First navigation | 1.9 s | 1.4 s | 384 s | 28 s |
| All dependencies navigable | 2.9 s | 1.6 s | 384 s | 28 s |
| clj-kondo finished | 22 s | 16 s | 384 s | 28 s |
| Memory once settled | 432 MiB | 412 MiB | 2 738 MiB | 2 151 MiB |
| Definition (median of 20) | 11 ms | 11 ms | 3 ms | 8 ms |
| Keystroke -> diagnostics, 251 KiB file | 584 ms | 585 ms | 410 ms | 379 ms |

**clj-kondo**:

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| First navigation | 787 ms | 212 ms | 19.4 s | 1.3 s |
| All dependencies navigable | 1.2 s | 212 ms | 19.4 s | 1.3 s |
| clj-kondo finished | 5.0 s | 940 ms | 19.4 s | 1.3 s |
| Memory once settled | 95 MiB | 89 MiB | 275 MiB | 272 MiB |
| Definition (median of 20) | 7 ms | 7 ms | 1 ms | 1 ms |
| Keystroke -> diagnostics, 233 KiB file | 524 ms | 523 ms | 298 ms | 301 ms |

What differs from the Linux tables:

- clojure-lsp's cold start on metabase was no faster on the Mac, and its
  first warm run took 129 s before the next two settled at 27 s and 28 s;
  clj-pulse's first warm run was also its slowest (clj-kondo finished 22 s,
  then 16 s twice). The medians stand, the cold column is one run.
- On the Mac the keystroke row favours clojure-lsp: its embedded clj-kondo
  answers in about 300 ms, while clj-pulse starts a clj-kondo process per
  keystroke. Above `:live-max-kb` clj-pulse's built-in tier answers in
  360 ms; see the records.
- Memory is read with `ps` on macOS and `/proc` on Linux, so the two are not
  the same measure.

The full method, the per-run rows, and the caveats in detail are in
[benchmark records](MEMORY.md#benchmark-against-clojure-lsp). To reproduce:
`CLJ_PULSE_BENCH_RUNS=3 bb bench`.
