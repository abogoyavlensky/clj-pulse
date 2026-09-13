# Performance

[Back to README](../README.md)

`bb bench` runs clj-pulse and [clojure-lsp](https://clojure-lsp.io) through the
same client, over stdio, with the same requests, on two corpora pinned by
commit. Every metric is behavioral. The startup rows time the wait until a
`textDocument/definition` lands where it should; memory and the latency
medians are sampled only after the server goes quiet and no child process of
it is still running. Both servers run at their defaults.

Warm runs - a second start with what the first left cached - on one Linux
container (5 cores, 11 GiB, 2026-09-10): clj-pulse 0.5.0, clojure-lsp
2026.07.06-14.34.19, metabase at `42a8e9f7`, clj-kondo at `13a32d1c`.

**metabase** (1 400+ files, 43 164 symbols):

| Metric | clj-pulse | clojure-lsp |
|---|---|---|
| Time to first definition | 3.1 s | 60 s |
| Time to first definition inside a dependency | 3.9 s | 60 s |
| Memory once settled | 353 MiB | 1 798 MiB |
| Definition (median of 20) | 34 ms | 7 ms |
| Keystroke -> diagnostics, 452 KiB file | 381 ms | 1 371 ms |
| Keystroke -> diagnostics, 251 KiB file | 911 ms | 925 ms |

**clj-kondo** (400 files, 2 252 symbols):

| Metric | clj-pulse | clojure-lsp |
|---|---|---|
| Time to first definition | 520 ms | 2.4 s |
| Time to first definition inside a dependency | 521 ms | 2.4 s |
| Memory once settled | 90 MiB | 273 MiB |
| Definition (median of 20) | 16 ms | 3 ms |
| Keystroke -> diagnostics, 233 KiB file | 789 ms | 759 ms |

What the tables do not say:

- The two servers do different work at startup. clojure-lsp analyzes the whole
  classpath through clj-kondo before it answers; clj-pulse indexes the
  project's sources first and the classpath in the background, and reads JAR
  entries lazily. The first two rows are that difference. From cold, with the
  server caches cleared, the first project definition on Metabase takes
  3.4 s against 293 s. Dependencies and `.cpcache` are prepared before timing
  both cold and warm runs.
- clojure-lsp answers a definition faster once it is up, from a fuller
  analysis.
- The definition row is measured on the largest file in each repo, which is a
  worst case for clj-pulse and not for clojure-lsp: we re-read the open
  buffer's definitions and usages on every request, while it looks the answer
  up in the analysis it stored at startup. On the same metabase checkout, a
  definition in a 190-byte file takes 2.1 ms from either server; in the 452 KiB
  file it is 27 ms from clj-pulse and 6 ms from clojure-lsp.
- The 452 KiB row is clj-pulse's native lint tier alone: that file is above
  `:kondo {:live-max-kb 256}`, so clj-kondo sits out the keystroke path. The
  251 KiB row is the same measurement with clj-kondo in it. clojure-lsp runs
  its embedded clj-kondo on every keystroke either way.
- One Linux container, one run each. macOS numbers are not in yet.

Cold tables, the full method, and the caveats in detail are in
[benchmark records](MEMORY.md#benchmark-against-clojure-lsp). To reproduce: `bb bench`.
