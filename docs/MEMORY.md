# Memory

Durable findings about clj-pulse worth keeping in one place: known gaps, their
root causes in the code, and what a fix would involve. Complements the
forward-looking [ROADMAP.md](ROADMAP.md).

## Benchmark against clojure-lsp

`bb bench` drives clj-pulse and clojure-lsp through the same client, over
stdio, with the same requests, on two corpora pinned by commit. Re-run it
before a release and after index or extractor changes.

- **Date:** <!-- filled by Task 6 -->
- **Machine:** Linux x86-64 container, 5 cores, 11 GiB RAM (Intel Haswell).
  One box. Nothing here is a claim about a laptop, and the maintainer's macOS
  numbers are not in yet.
- **clj-pulse:** <!-- filled by Task 6: version at commit -->, release build,
  production settings (stage-3 classpath resolution on, clj-kondo
  v2026.08.04 on PATH)
- **clojure-lsp:** 2026.07.06-14.34.19, native static Linux build, defaults —
  no `initializationOptions`, nothing tuned on either side
- **Corpora:** metabase at `42a8e9f7` (43 164 symbols in 2 976 namespaces),
  clj-kondo at `13a32d1c` (2 252 symbols in 225 namespaces)
- **Runs:** `CLJ_PULSE_BENCH_RUNS=3`: cold once per server, warm three times,
  the warm column being the median row. Per-run rows are in the `BENCH_JSON`
  lines of the run log.
- **Reproduce:** `CLJ_PULSE_BENCH_RUNS=3 bb bench metabase`, `… bb bench
  clj-kondo`, or `… bb bench` for both

### The rows

- **Time to first navigation** — the first `textDocument/definition` on a
  project symbol that lands in the file defining it, from a small file open
  since before `initialize` returned. Polled every 100 ms.
- **All dependencies navigable** — the first definition into a *third-party*
  dependency that lands inside an archive. For clj-pulse the asking starts
  only once a library stage line ("library indexing complete" or "full
  classpath indexed") has arrived, so a namespace that happened to be
  indexed early cannot answer for the rest; the published time is when the
  probe landed after that. clojure-lsp has no such gate and is polled from
  the start. The probe is a candidate set of up to five `alias/name` usages
  of distinct namespaces that are neither `clojure.*` nor the project's own,
  taken from the smallest source files; the first to land carries the row
  (`library_site` in the JSON says which), so a candidate that is really a
  git dependency, a ClojureScript-only namespace, or a re-export
  (`potemkin/import-vars` on metabase) costs nothing but a poll.
- **clj-kondo finished** — clj-pulse: the last `clojurePulse/lintStatus`
  with `warming: false` after one with `warming: true`, the end of the
  dependency-cache warm. clojure-lsp: its settle time, since its startup
  *is* a clj-kondo analysis. `n/a` when no warm ever started.
- **Time to settled** — nothing logged (clj-pulse) or no
  `publishDiagnostics`/`$/progress` (clojure-lsp) for 2 s, *and* no child
  process still running. Not a published row any more: for clj-pulse it is
  the clj-kondo warm plus the quiet window, and reads as a wait it is not.
  RSS and every median below are sampled after it.
- **Definition (median of 20)** — on the largest `.clj` in the repo, into the
  project.
- **didChange → diagnostics** — 20 single-character inserts, each timed to
  the `publishDiagnostics` carrying that edit's version, on the largest
  `.clj` (above `:kondo {:live-max-kb 256}` on metabase, so clj-pulse's
  native tier alone) and on the largest under it (clj-kondo in the loop for
  both servers). Only the second is published.

### metabase (large)

Edit target `test/metabase/dashboards_rest/api_test.clj` (452 KiB, the largest
`.clj` in the repo), second edit target `test/metabase/collections_rest/api_test.clj`
(251 KiB, the largest under clj-pulse's `:kondo {:live-max-kb 256}`).
Library probe that landed: <!-- filled by Task 6 -->.

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| Time to first navigation | — | — | — | — |
| All dependencies navigable | — | — | — | — |
| clj-kondo finished | — | — | — | — |
| Time to settled | — | — | — | — |
| RSS settled | — | — | — | — |
| Definition (median of 20) | — | — | — | — |
| didChange → diagnostics, 452 KiB | — | — | — | — |
| didChange → diagnostics, 251 KiB | — | — | — | — |

<!-- filled by Task 6 -->

### clj-kondo (medium)

Edit target `src/clj_kondo/impl/analyzer.clj` (233 KiB — already under
`:live-max-kb`, so there is no second row). Library probe that landed:
<!-- filled by Task 6 -->.

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| Time to first navigation | — | — | — | — |
| All dependencies navigable | — | — | — | — |
| clj-kondo finished | — | — | — | — |
| Time to settled | — | — | — | — |
| RSS settled | — | — | — | — |
| Definition (median of 20) | — | — | — | — |
| didChange → diagnostics, 233 KiB | — | — | — | — |

<!-- filled by Task 6 -->

### What the numbers mean, and what they do not

- **The two servers do different work at startup.** clojure-lsp analyzes the
  whole classpath through clj-kondo and caches the result; clj-pulse indexes
  the project's own sources first, the classpath in the background, and warms
  clj-kondo's dependency cache last, reading JAR entries lazily. The three
  timeline rows are that difference: one moment for clojure-lsp, three for
  clj-pulse.
- **clojure-lsp answers a definition faster once it is up** and it is
  answering from a fuller analysis. That row is the one to watch when the
  extractor changes.
- **That gap is per-request re-analysis, and it scales with the open file.**
  Every position request re-reads the buffer: `references::resolve_fqn_at`
  calls `extractor::extract_full_tree`, which rebuilds every symbol and
  occurrence and then scans them linearly for the cursor. The index lookup
  after it is a hash lookup. Measured on a warm metabase checkout (0.5.0,
  2026-09-10), through one client, 20 samples each:

  | Open file | clj-pulse | clojure-lsp |
  |---|---|---|
  | `src/metabase/classloader/init.clj`, 190 B | 2.1 ms | 2.1 ms |
  | `test/metabase/dashboards_rest/api_test.clj`, 452 KiB | 27.2 ms | 6.3 ms |

  On a small file the two are indistinguishable and the floor is the JSON-RPC
  round trip. The bench probes the largest `.clj` in each repo, so its
  definition row is our worst case and not clojure-lsp's, which reads a stored
  analysis and barely moves. Caching the `Analysis` per (uri, version) would
  close the gap, and the native lint pass on the same buffer would reuse it
  (`unused_requires` is 28 ms of its 65 ms for the same reason). It is in the
  [ROADMAP.md](ROADMAP.md) backlog and deliberately unscheduled: 27 ms is below
  what a user perceives, and a cache serving a stale entry would navigate
  confidently to the wrong place — masking the resolution bugs that list
  already tracks rather than fixing them.
- **Cold and warm are per server.** Cold deletes `.clj-pulse/jar-cache` for
  clj-pulse and `.lsp/.cache`, `.clj-kondo/.cache` *and* clojure-lsp's global
  `$XDG_CACHE_HOME/clojure-lsp` for clojure-lsp — the last one holds ~150 MiB
  of JDK-source analysis and lives outside the project, so the bench points
  `XDG_CACHE_HOME` inside the corpus and clears it. Before that isolation, a
  "cold" clojure-lsp row was reusing an earlier run's JDK analysis and its
  metabase timings swung between 293 s and 700 s; with it they repeat.
  `.cpcache` is *not* cleared: resolving the classpath is preparation both
  servers share, done untimed by `bb bench` before either starts.
- **Timeline times are receipt times.** The client stamps every server
  message as it pulls it off the channel, and the harness keeps receiving
  while it waits between polls, so a stage line or a `lintStatus` is timed
  when it arrived, to within the 100 ms poll.
- **The 452 KiB edit row is clj-pulse's native tier alone**: that file is above
  `:kondo {:live-max-kb 256}`, so clj-kondo sits out the keystroke path. The
  251 KiB row is the same measurement with clj-kondo in it, which is what the
  threshold buys. clojure-lsp lints every keystroke through its embedded
  clj-kondo either way.
- **clojure-lsp takes full document syncs.** It declares
  `TextDocumentSyncKind.Full`, so its own clients resend the whole buffer on
  every keystroke, and the bench does the same; clj-pulse takes incremental
  ranges. Both are what each server asked for.
- **clojure-lsp echoes no document version** in `publishDiagnostics`, so its
  edit rows time the next publication for that file rather than the one
  carrying the edit's version. clj-pulse's rows are version-matched.
- **A definition answer only counts when it lands where it should** — the
  project file that defines the var, or an entry inside a dependency archive
  (`jar:` for clj-pulse, `zipfile:` for clojure-lsp). A null answer is a retry,
  never a fast sample.

## Compare against clj-kondo analysis (2026-09-18)

`bb compare` on the clj-kondo corpus, clj-kondo v2026.08.04, same Linux
container as the benchmark: 2409 var-definitions, 36396 var-usages, 9123
locals and 76810 keywords in the analysis of `src parser resources inlined
extract pod-test src-profile test test-regression`; 5252 probes after the
stride of 200 per bucket and request kind, 215 files visited, answered in
40 s by a server that settled in 14 s. 9403 symbol and keyword tokens no
oracle entry covered (unresolved usages, the `ns` head, unqualified keywords
are counted there).

The run is from the gap fix (discards and comments are gaps for the
extractor, 2026-09-18); the three right-hand columns are the run before it,
on `release 0.5.3`, the same day and machine. The first run of 2026-09-17
had the same numbers except `var-usage/library/aliased` (183 agree here and
in the baseline, 182 there — the dialect item, since fixed).

| Bucket | Probes | Agree | Diverge | Known | Null | Agree before | Diverge before | Null before |
|---|---|---|---|---|---|---|---|---|
| `keyword/alias` | 8 | 8 | 0 | 0 | 0 | 0 | 6 | 2 |
| `keyword/keys` | 5 | 5 | 0 | 0 | 0 | 0 | 5 | 0 |
| `keyword/qualified` | 276 | 142 | 93 | 0 | 41 | 86 | 145 | 45 |
| `local/destructured` | 507 | 507 | 0 | 0 | 0 | 482 | 24 | 1 |
| `local/plain` | 594 | 585 | 3 | 0 | 6 | 542 | 44 | 8 |
| `var-def/declare` | 28 | 27 | 1 | 0 | 0 | 27 | 1 | 0 |
| `var-def/def` | 400 | 394 | 6 | 0 | 0 | 380 | 20 | 0 |
| `var-def/defmacro` | 58 | 50 | 8 | 0 | 0 | 46 | 12 | 0 |
| `var-def/defmulti` | 6 | 3 | 3 | 0 | 0 | 3 | 3 | 0 |
| `var-def/defn` | 372 | 326 | 45 | 0 | 1 | 298 | 73 | 1 |
| `var-def/defn-` | 304 | 304 | 0 | 0 | 0 | 290 | 14 | 0 |
| `var-def/defonce` | 14 | 14 | 0 | 0 | 0 | 14 | 0 | 0 |
| `var-def/defprotocol` | 44 | 16 | 10 | 18 | 0 | 16 | 10 | 0 |
| `var-def/defprotocol+` | 26 | 0 | 26 | 0 | 0 | 0 | 26 | 0 |
| `var-def/defrecord` | 52 | 44 | 6 | 0 | 2 | 44 | 6 | 2 |
| `var-def/deftest` | 346 | 344 | 2 | 0 | 0 | 344 | 2 | 0 |
| `var-def/deftype` | 20 | 0 | 20 | 0 | 0 | 0 | 20 | 0 |
| `var-def/import-vars` | 98 | 23 | 75 | 0 | 0 | 23 | 75 | 0 |
| `var-def/programs` | 6 | 3 | 3 | 0 | 0 | 3 | 3 | 0 |
| `var-usage/core` | 200 | 199 | 0 | 0 | 1 | 187 | 13 | 0 |
| `var-usage/core/macro` | 195 | 193 | 0 | 0 | 2 | 175 | 16 | 5 |
| `var-usage/library` | 167 | 5 | 118 | 0 | 44 | 4 | 120 | 43 |
| `var-usage/library/aliased` | 188 | 183 | 0 | 0 | 5 | 183 | 0 | 5 |
| `var-usage/library/macro` | 199 | 168 | 26 | 0 | 5 | 168 | 26 | 5 |
| `var-usage/library/macro/aliased` | 12 | 6 | 0 | 0 | 6 | 6 | 0 | 6 |
| `var-usage/library/macro/referred` | 169 | 169 | 0 | 0 | 0 | 169 | 0 | 0 |
| `var-usage/library/referred` | 4 | 4 | 0 | 0 | 0 | 4 | 0 | 0 |
| `var-usage/project` | 200 | 192 | 5 | 0 | 3 | 187 | 10 | 3 |
| `var-usage/project/aliased` | 193 | 179 | 0 | 0 | 14 | 179 | 0 | 14 |
| `var-usage/project/macro` | 192 | 191 | 0 | 0 | 1 | 188 | 3 | 1 |
| `var-usage/project/macro/aliased` | 64 | 58 | 0 | 0 | 6 | 58 | 0 | 6 |
| `var-usage/project/macro/referred` | 135 | 134 | 0 | 0 | 1 | 134 | 0 | 1 |
| `var-usage/project/referred` | 170 | 165 | 0 | 0 | 5 | 165 | 0 | 5 |
| **total** | 5252 | 4641 | 450 | 18 | 143 | 4405 | 677 | 153 |

`soft` (same lines, other columns) was 0 in both runs. Every `known` row is
the protocol-method entry of `KNOWN`. One fix, 677 → 450 divergences: a `;`
comment or `#_` discard inside a binding vector, a destructuring map or a
def form was a named child the positional walkers counted as a form, so
every pair after it shifted — which is why the drop reaches
`local/destructured` (24 → 0), `keyword/keys` and `keyword/alias` (5 and 6
→ 0), `var-def/defn-` (14 → 0), `var-usage/core` and `core/macro` (13 and
16 → 0), and a third of `keyword/qualified`, not only the four buckets the
`#_` issue named. The three `null` entries that are new by site (a
definition on a core macro head answering null) are the class the baseline
had at other sites; the probe set shifted by one.

The first-run divergences grouped into fourteen classes, one issue file
each under `docs/backlog/2026-09-17-*.md` (linked from the ROADMAP
backlog); the discard one is archived. The remaining classes: keywords in
binding values and quoted data missing from occurrences, locals under
`binding`+`let`, `{:ns/keys}` renaming the local, `.cljs` core landing in
`clojure/core.clj`, a `defmulti` missing from its own references after a
`declare`, constructor calls not counted for `deftype`/`defrecord`,
`:lint-as` to `defprotocol`/`declare` half-honored, `import-vars`
re-exports, `:require-macros`, a `.clj`/`.cljs` namespace pair sharing an
fqn, defs nested in a wrapping macro. The `var-usage/*/aliased` and
`*/referred` rows (project and library) are the clean ones: 898 of 935
agree, and the misses are `null` answers.

The number to watch on a re-run is `diverge + null` per bucket against this
table; a class fixed in the server should empty its bucket's share, and a new
divergence in a bucket that was clean is a regression the e2e suite did not
see.

## Soak: memory over a long session (2026-09-11)

`bb soak` at 300 rounds on the clj-kondo corpus, seed `17215462345791384795`,
same Linux container as the benchmark: 60 checkpoints, 7.6 minutes, no
divergence from the reference server at any of them, every witness landed.
RSS, sampled quiesced with nothing open at each checkpoint:

| Round | 5 | 15 | 100 | 130 | 185 | 300 |
|---|---|---|---|---|---|---|
| RSS | 104.0 MiB | 116.1 | 117.9 | 124.0 | 124.6 | 124.8 |

A plateau, not a leak. The growth arrives in three steps (rounds 10→15,
100→105, 125→130) with flat stretches between, and the last 115 rounds gained
0.2 MiB — the signature of capacity doublings that are never handed back
(hash tables, ropes, allocator arenas), not of per-edit retention, which would
draw a straight line. 1.20x over 300 rounds, against the gate's 1.5x. The
20-round default climbs monotonically (104 → 116 MiB) and looks like a leak
on its own; it is the first of those steps. When a future run shows a
straight line instead of a staircase, that is the bug this table exists to
recognize.

## Performance baseline (clj-pulse alone, before the comparison)

The history below predates the benchmark above and measures clj-pulse alone,
synchronized on its own log lines rather than on behavior; the numbers are not
directly comparable with the tables above, but the trends are why the code
looks the way it does.

Measured with `bb bench`: the release binary indexing a shallow clone of
[metabase](https://github.com/metabase/metabase) under production settings
(stage-3 classpath resolution on, clj-kondo on PATH). Re-run it before a
release and after index or extractor changes, and compare.

- **Date:** 2026-09-05
- **Machine:** Linux x86-64 container, 8 cores
- **Corpus:** metabase at `.tmp/bench/metabase`, 42 905 symbols in 2 976
  namespaces
- **Edit target:** `test/metabase/dashboards_rest/api_test.clj`, 452 KiB — the
  largest `.clj` in the repo, so the per-edit numbers are a worst case, not a
  typical file

| Metric | Cold caches | Warm `.cpcache` + kondo cache |
|---|---|---|
| Time to project index | 3.0 s | 2.8 s |
| Time to library index (through stage 3, 491 classpath entries) | 7.9 s | 3.0 s |
| RSS after project index | 304 MiB | 289 MiB |
| RSS after library index | 349 MiB | 299 MiB |
| didOpen → first diagnostics | 976 ms | 946 ms |
| didChange → diagnostics (median of 20) | 1243 ms | 1202 ms |
| Definition (median of 20) | 78 ms | 71 ms |

The bench waits on both library tiers at once and samples when stage 3 settles,
never at the stage-2 `library indexing complete` line: on a warm checkout stage
2 finishes seconds before stage 3 has re-resolved and re-indexed, and sampling
there would fold a background reindex into every latency below. Waiting on them
*in sequence* is wrong in the other direction — on a cold checkout stage 2 finds
nothing and stays silent, so a stage-2-first wait burns the whole ceiling.

Re-confirmed against an independent full metabase checkout at a later commit:
project index 2.9 s, library index 3.3 s warm and 7.1 s cold, 1228 ms per edit,
71 ms definition — all within run-to-run noise of the table above.

**2026-09-08, keyword completion: +70 MiB, nothing else.** Recording every
unqualified keyword as an occurrence (`:id`, not just `:my.ns/id`) is the one
measurable cost of keyword completion. Measured on one box against a `master`
build of the same corpus, so the absolute numbers are that box's, not the
table's: RSS after project index 287 → 358 MiB (median of two runs), while
index time (4.1 → 4.2 s), per-edit diagnostics (1340 → 1380 ms) and definition
(91 ms both) did not move. metabase is ~43 000 symbols; the extra bytes are the
occurrence vectors themselves plus the small `keyword_counts` aggregate, and
they buy find-references and completion on unqualified keywords. If it ever
needs winning back, interning occurrence fqns is the lever — most of them
repeat.

### After the tree cache (2026-09-09)

Same box, same corpus and edit target, the release binary at commit 6b9f713
(one incrementally updated tree per open document, and clj-kondo skipped on
keystrokes above `:live-max-kb 256`; the 452 KiB target is above it).

| Metric | Cold caches | Warm `.cpcache` + kondo cache |
|---|---|---|
| Time to project index | 3.2 s | 3.4 s |
| Time to library index (through stage 3, 491 classpath entries) | 7.8 s | 3.7 s |
| RSS after project index | 354 MiB | 346 MiB |
| RSS after library index | 398 MiB | 352 MiB |
| didOpen → first diagnostics | 1010 ms | 1076 ms |
| didChange → diagnostics (median of 20) | 369 ms | 372 ms |
| Definition (median of 20) | 22 ms | 22 ms |

Per edit that is the 300 ms debounce plus a native pass of about 65 ms, down
from about 1200 ms; definition fell from about 75 ms to 22 ms. Index times and
RSS are unchanged (the RSS step from the baseline table is the keyword
occurrences recorded on 2026-09-08, below, not the tree: one cached tree is a
few MiB). didOpen still runs clj-kondo, so it did not move.

What a request costs now, measured step by step on the same file in release
mode: the definitions-and-occurrences walk (`extract_full_tree`) is 21 ms and
is all of a definition request; the native pass is that walk plus
`qualified_usages` (9 ms) and `unused_requires` (28 ms, mostly the bare-symbol
collection). Nothing parses. The next lever, if a large file ever needs it, is
caching the `Analysis` per document version so a request walks nothing; it is
in the ROADMAP Backlog, not scheduled.

### After keyword rename (2026-09-09)

Same box and corpus, the release binary at commit 8e530c7. The extractor gained
one pass over `:keys` destructuring vectors and namespaced maps in EDN configs,
so the bench was re-run to price it: 3.7-4.0 s to project index, 365 MiB RSS,
390 ms per edit, 24-30 ms per definition (median of two runs). That is the tree
cache table within run-to-run noise — the new work is proportional to
destructuring forms, not to file size, and does not show up.

### After qualified def heads (2026-09-10)

Same box and corpus, the release binary at commit 35f1c39. `mu/defn` and its
relatives are now indexed (`extractor::head_def_kind`), which is 2 694 more
symbols on metabase — 37 675 → 40 369 over `src` + `test`, a 7% gain, and it
matches the corpus: 1 547 `(mu/defn`, 1 102 `(mu/defn-`, 243 `(mu/defmethod`.
Roughly one function in fourteen was invisible to navigation before.

The extra symbols cost nothing measurable: 3.5-3.6 s to project index, 3.7-5.3 s
to library index (491 entries, through stage 3), 365 MiB RSS after the project
index and 374 MiB after libraries, ~1 s didOpen → first diagnostics, 380 ms per
edit, 25-27 ms per definition (three runs). That is the keyword-rename table within
run-to-run noise. Indexing more names is proportional to the definitions a file
holds, and definition latency is a hash lookup either way.

### Integrant configs are searched project-wide (2026-09-09)

The EDN scan used to be limited to `:paths`, so a config the classpath does not
name — `resources/config.edn` with `:paths ["src"]`, a Leiningen
`:resource-paths`, a `system.edn` at the project root — was never indexed, and
references and keyword rename silently skipped it. It now walks each project dir
as well. Unbounded, that extra walk costs ~700 ms on metabase (27 556 files in
5 111 dirs) for the 73 `.edn` files it finds: the traversal, not the reads.
Bounded at `EDN_SCAN_MAX_DEPTH = 5` it costs nothing measurable — project index
3.6 s, against 3.2-3.4 s in the table above and 3.7-4.0 s measured on this box
the same afternoon — and still reaches every layout in the wild. The declared
source roots are still walked in full alongside it, since one can sit outside
the project dir (`:paths ["../shared/resources"]`) or below the bound. Deeper or
gitignored configs fall back to `didOpen` indexing.

### On the maintainer's machine (macOS)

The table above is a Linux CI-shaped box. The numbers users actually see are
better, so treat the Linux figures as a pessimistic bound. Measured on macOS
against a metabase checkout that is *not* the same commit — 35 003 symbols in
2 587 namespaces, 477 classpath entries, and a 390 KiB edit target rather than
452 KiB — so compare shapes, not digits:

| Metric | Cold | Warm |
|---|---|---|
| Time to project index | 1.6 s | 1.7 s |
| Time to library index (through stage 3) | 10.3 s | 1.8 s |
| RSS after project / library index | 308 / 332 MiB | 298 / 322 MiB |
| didOpen → first diagnostics | 546 ms | 574 ms |
| didChange → diagnostics (median of 20) | 957 ms | 1053 ms |
| Definition (median of 20) | 33 ms | 30 ms |

Two things this changes. **Definition is ~30 ms on real hardware**, under the
50 ms bar, not the ~75 ms the Linux box reported before the tree cache — the
cache was worth doing for the lint pass, and definition latency was not the
argument for it. And **~400 ms separates the wall clock from the elapsed time the server
logs for itself** (1.6 s vs 1.18 s, consistent across runs), where on Linux the
gap is ~10 ms. That gap is process spawn plus the `initialize` handshake plus
project detection, before indexing starts — worth a look given that instant
startup is a stated differentiator, and not something the current metrics
isolate.

For scale: the same bench against a one-namespace project reports 8 ms to
index, 31 ms to first diagnostics and a 331 ms median per edit — that is the
300 ms debounce plus ~30 ms of work. The numbers above are what file *size*
costs, not fixed overhead.

### What the numbers cost, and what is left

A diagnostics pass used to run its two tiers in sequence — the native lints
(~360 ms on this file), then the clj-kondo subprocess (~840 ms). They are
independent, so they now run concurrently, with the CPU-bound native pass on a
blocking thread: didChange → diagnostics fell from ~1570 ms to ~1200 ms and
didOpen → first diagnostics from ~1330 ms to ~950 ms.

Two costs remained at that point, both above what a user would call
comfortable; both are resolved as of 2026-09-09 (table above):

- **No cached parse tree.** Every diagnostics pass parsed the buffer three
  times (`extract_analysis_with` ~80 ms, `qualified_usages` ~65 ms,
  `unused_requires` ~140 ms on the 452 KiB file), and every position request
  parsed it once — most of the ~75 ms definition latency. Now the document
  store keeps one tree per open buffer, edited and reparsed incrementally on
  every `didChange`, and every handler and the lint pass read it.
- **clj-kondo dominated the remaining per-edit time**, and that is its own
  cost, not ours. Measured directly by taking it off `PATH`: the median per
  edit fell from ~1230 ms to **633 ms** — i.e. 300 ms debounce plus a 333 ms
  native pass, with clj-kondo adding ~595 ms on top. (Its standalone run on
  this file is ~1.0 s; the difference is what the concurrent native pass
  already hides.) Of the design options — a longer debounce for large buffers,
  a size threshold above which the kondo tier is skipped, or publishing the
  native tier first and the kondo tier when it lands — the threshold won: the
  third would have broken the "one publish per pass" invariant, and the first
  only moves the wait. `:kondo {:live-max-kb 256}` skips clj-kondo on the
  didChange pass alone; open and save still run it.

## Leiningen: transitive deps come from `lein classpath`

### How deep each project type goes

clj-pulse resolves dependencies differently per project type, so the transitive
depth it indexes varies:

| Project type | Resolver | Transitive depth |
|---|---|---|
| `deps.edn` | reads `.cpcache/*.cp`, then background `clojure -A:dev:test -Spath` (`src/classpath.rs`) | Full closure, including alias deps |
| let-go `lgx.edn` | `lgx::resolve` (`src/lgx.rs`) | Full transitive - breadth-first walk of each dep's own `:deps` |
| Leiningen `project.clj` | background `lein classpath` (`src/classpath.rs`), else `leiningen::resolve` (`src/leiningen.rs`) | Full closure from the command; direct deps only when it is off or fails |

For `deps.edn`, indexing is graduated. Stage 2 reads whatever classpath a prior
`clojure` invocation left in `.cpcache` — instant, no subprocess. Stage 3 then
runs `clojure -A:dev:test -Spath` in the background (the command is set per
project in `.clj-pulse/config.edn`, `{:projects [{:path "." :classpath
{:enabled true :cmd "…"}}]}`, and enabled by default only for the workspace
root) and re-indexes when the authoritative
classpath differs — this is what makes `:test`/`:dev` alias deps navigable.
The clojure CLI is its own staleness check: with a warm cache it prints the
classpath from a bash script without booting a JVM; only a deps.edn change or
a never-resolved alias combo costs a JVM (and possibly downloads). Every stage-3
failure (CLI missing, offline, bad alias) degrades to the stage-2 result. For
let-go, `lgx::resolve` walks each dependency's own `:deps` until the queue
drains, so depth is unbounded.

### The fallback, and its gap

Leiningen resolves the same way deps.edn does: stage 3 runs the project's
`:cmd` — `lein classpath` by default, enabled by default for the workspace root
— in the background, and the full closure is indexed from its output. What
differs is the *fallback*. A deps.edn project that never reaches stage 3 still
has `.cpcache` to read; a Leiningen project has nothing equivalent, so it falls
back to `leiningen::resolve`, which reads `project.clj` as text and maps only
the direct `:dependencies` to JARs under `~/.m2`. Within that, it skips any
dependency that:

- declares no inline string version - `coord_from` in `src/leiningen.rs`
  requires the `[group/artifact "version"]` shape, or
- is not already downloaded to `~/.m2`.

It never reads a JAR's `pom.xml`, so it cannot discover transitive
dependencies. It never reads `:managed-dependencies` or a `lein-parent`
`:parent-project`, so versions inherited from a parent stay unknown. This is
deliberate: the module inspects `project.clj` only and never shells out to
`lein classpath`, which avoids JVM startup at the cost of completeness.

### Symptom, when the command does not run

With `lein classpath` disabled (`:classpath {:enabled false}`), unavailable, or
failing, go-to-definition fails for any symbol whose namespace lives in a
transitive or version-less dependency, because that JAR is never indexed.

Example, from the `flockman` project: `(defcomponent ...)` uses the
`defcomponent` macro from `defcomponent-0.2.2.jar`. That JAR is a transitive
dependency (pulled in by a `com.flocktory/staff.*` library, absent from
`project.clj`), so it is never indexed and `lookup("defcomponent/defcomponent")`
returns nothing. The same applies to direct deps declared without a version,
such as `[com.flocktory/staff.guards]`, whose version comes from the parent.

This is not specific to macros. A macro is indexed like any other var
(`DefKind::Defmacro`), and a macro call resolves through `:refer` or an alias
exactly like a function call. The symbol is missing only because its JAR sits
off the resolved classpath.

### Stance: never a JVM on the hot path

The original fixed principle here was "clj-pulse will not start a JVM"; it has
since been narrowed to: **never a JVM on the hot path.** Under the narrowed
principle `lein classpath` is fine, and it is what stage 3 runs for a Leiningen
project. `lein` does boot a JVM every run, warm or not, unlike the clojure CLI
with a warm cache — but that cost is paid once per resolution, in a background
task, while the index already serves the project's own sources. Nothing waits
on it: startup, keystrokes and requests never touch a JVM, and a failed or
disabled run degrades to the direct-dependency fallback above.

The two directions below remain worth having, because they improve the
fallback — the path a project takes when the command is off, `lein` is missing,
or the resolve fails.

Neither is urgent:

1. **Resolve version-less direct deps from `~/.m2`.** When a direct dep declares
   no version (the version comes from the parent project), look under
   `~/.m2/repository/<group>/<artifact>/` and take the only, or newest,
   downloaded version. Cheap and subprocess-free; covers parent-managed direct
   deps, but still not transitive ones.
2. **Reimplement enough Maven resolution in Rust.** Parse each dep's `pom.xml`
   and the parent's `:managed-dependencies`, then walk the tree. No subprocess,
   but a large and fragile effort: version ranges, exclusions, and profiles all
   apply.

See also the "Leiningen transitive deps" item under best effort in
[ROADMAP.md](ROADMAP.md).
