# Memory

Durable findings about clj-pulse worth keeping in one place: known gaps, their
root causes in the code, and what a fix would involve. Complements the
forward-looking [ROADMAP.md](ROADMAP.md).

## Benchmark against clojure-lsp

`bb bench` drives clj-pulse and clojure-lsp through the same client, over
stdio, with the same requests, on two corpora pinned by commit. Re-run it
before a release and after index or extractor changes.

- **Date:** 2026-09-10
- **Machine:** Linux x86-64 container, 5 cores, 11 GiB RAM (Intel Haswell).
  One box. Nothing here is a claim about a laptop, and the maintainer's macOS
  numbers are not in yet.
- **clj-pulse:** 0.5.0 at `e497295`, release build, production settings
  (stage-3 classpath resolution on, clj-kondo v2026.08.04 on PATH)
- **clojure-lsp:** 2026.07.06-14.34.19, native static Linux build, defaults —
  no `initializationOptions`, nothing tuned on either side
- **Corpora:** metabase at `42a8e9f7` (43 164 symbols in 2 976 namespaces),
  clj-kondo at `13a32d1c` (2 252 symbols in 225 namespaces)
- **Reproduce:** `bb bench metabase`, `bb bench clj-kondo`, or `bb bench` for
  both

### metabase (large)

Edit target `test/metabase/dashboards_rest/api_test.clj` (452 KiB, the largest
`.clj` in the repo), second edit target `test/metabase/collections_rest/api_test.clj`
(251 KiB, the largest under clj-pulse's `:kondo {:live-max-kb 256}`).

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| Time to first definition | 3.4 s | 3.1 s | 293 s | 60 s |
| Time to first library definition | 6.0 s | 3.9 s | 293 s | 60 s |
| Time to settled | 64 s | 50 s | 295 s | 62 s |
| RSS settled | 366 MiB | 353 MiB | 2 289 MiB | 1 798 MiB |
| Definition (median of 20) | 25 ms | 34 ms | 6 ms | 7 ms |
| didChange → diagnostics, 452 KiB | 373 ms | 381 ms | 1 185 ms | 1 371 ms |
| didChange → diagnostics, 251 KiB | 895 ms | 911 ms | 795 ms | 925 ms |

Repeated on the same box: clj-pulse 3.7 s / 3.4 s to first definition, 387 ms
and 378 ms per edit; clojure-lsp 288 s cold and 57 s warm, 1 520 ms and
1 312 ms per edit. The two runs agree within a few percent.

### clj-kondo (medium)

Edit target `src/clj_kondo/impl/analyzer.clj` (233 KiB — already under
`:live-max-kb`, so there is no second row).

| Metric | clj-pulse cold | clj-pulse warm | clojure-lsp cold | clojure-lsp warm |
|---|---|---|---|---|
| Time to first definition | 526 ms | 520 ms | 16.8 s | 2.4 s |
| Time to first library definition | 928 ms | 521 ms | 16.8 s | 2.4 s |
| Time to settled | 14.3 s | 4.4 s | 18.8 s | 4.4 s |
| RSS settled | 125 MiB | 90 MiB | 238 MiB | 273 MiB |
| Definition (median of 20) | 17 ms | 16 ms | 7 ms | 3 ms |
| didChange → diagnostics, 233 KiB | 790 ms | 789 ms | 921 ms | 759 ms |

A third run of this corpus: clj-pulse 419 ms / 513 ms, clojure-lsp 17.2 s /
2.5 s. This corpus is stable run to run; metabase's clojure-lsp startup is the
only number that moved much, and only before its global cache was isolated
(see below).

### What the numbers mean, and what they do not

- **The two servers do different work at startup.** clojure-lsp analyzes the
  whole classpath through clj-kondo and caches the result; clj-pulse indexes
  the project's own sources first and the classpath in the background, and
  reads JAR entries lazily. The "time to first definition" row is that
  difference, not a difference in speed at the same task.
- **clojure-lsp answers a definition faster once it is up** — 3-7 ms against
  our 16-34 ms — and it is answering from a fuller analysis. That row is the
  one to watch when the extractor changes.
- **That gap is per-request re-analysis, and it scales with the open file.**
  Every position request re-reads the buffer: `references::resolve_fqn_at`
  calls `extractor::extract_full_tree`, which rebuilds every symbol and
  occurrence and then scans them linearly for the cursor. The index lookup
  after it is a hash lookup. Measured on the same warm metabase checkout,
  through one client, 20 samples each:

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
- **"Settled" is not "answering".** It is: nothing logged (clj-pulse) or no
  `publishDiagnostics`/`$/progress` (clojure-lsp) for 2 s, *and* no child
  process of the server still running. For clj-pulse on metabase that is ~50 s,
  almost all of it the clj-kondo dependency-cache warm over 491 classpath
  entries — background work that does not block an answer, which is why the
  first-definition row is 3 s. RSS and every median are sampled after it.
- **The 452 KiB edit row is clj-pulse's native tier alone**: that file is above
  `:kondo {:live-max-kb 256}`, so clj-kondo sits out the keystroke path. The
  251 KiB row is the same measurement with clj-kondo in it — 373 ms against
  895 ms, which is what the threshold buys. clojure-lsp lints every keystroke
  through its embedded clj-kondo either way.
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
