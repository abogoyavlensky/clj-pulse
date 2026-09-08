# Memory

Durable findings about clj-pulse worth keeping in one place: known gaps, their
root causes in the code, and what a fix would involve. Complements the
forward-looking [ROADMAP.md](ROADMAP.md).

## Performance baseline

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
50 ms bar, not the ~75 ms the Linux box reports — the parse-cache item below is
still worth doing for the lint pass, but definition latency is not the argument
for it. And **~400 ms separates the wall clock from the elapsed time the server
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

Two costs remain, both above what a user would call comfortable, and both
larger than a single fix:

- **No cached parse tree.** Every diagnostics pass parses the buffer three
  times (`extract_analysis_with` ~80 ms, `qualified_usages` ~65 ms,
  `unused_requires` ~140 ms on the 452 KiB file), and every position request
  parses it once — which is most of the ~75 ms definition latency. A parse
  cached per document version, ideally updated incrementally from the
  `didChange` ranges tree-sitter already accepts, would cut the native pass to
  roughly one parse and take position requests to single-digit milliseconds.
- **clj-kondo dominates the remaining per-edit time**, and that is its own cost,
  not ours. Measured directly by taking it off `PATH`: the median per edit falls
  from ~1230 ms to **633 ms** — i.e. 300 ms debounce plus a 333 ms native pass,
  with clj-kondo adding ~595 ms on top. (Its standalone run on this file is
  ~1.0 s; the difference is what the concurrent native pass already hides.)
  Options are all design changes: a longer debounce for large buffers, skipping
  the kondo tier above a size threshold, or publishing the native tier first and
  the kondo tier when it lands — which the "one publish per pass" invariant
  currently forbids.

Both are tracked as Milestone 1 items in [ROADMAP.md](ROADMAP.md).

## Leiningen indexes only direct dependencies

### How deep each project type goes

clj-pulse resolves dependencies differently per project type, so the transitive
depth it indexes varies:

| Project type | Resolver | Transitive depth |
|---|---|---|
| `deps.edn` | reads `.cpcache/*.cp`, then background `clojure -A:dev:test -Spath` (`src/classpath.rs`) | Full closure, including alias deps |
| let-go `lgx.edn` | `lgx::resolve` (`src/lgx.rs`) | Full transitive - breadth-first walk of each dep's own `:deps` |
| Leiningen `project.clj` | `leiningen::resolve` (`src/leiningen.rs`) | Direct deps only |

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

### The gap

Leiningen is the exception. `leiningen::resolve` reads `project.clj` as text and
maps only the direct `:dependencies` to JARs under `~/.m2`. Within that, it
skips any dependency that:

- declares no inline string version - `coord_from` in `src/leiningen.rs`
  requires the `[group/artifact "version"]` shape, or
- is not already downloaded to `~/.m2`.

It never reads a JAR's `pom.xml`, so it cannot discover transitive
dependencies. It never reads `:managed-dependencies` or a `lein-parent`
`:parent-project`, so versions inherited from a parent stay unknown. This is
deliberate: the module inspects `project.clj` only and never shells out to
`lein classpath`, which avoids JVM startup at the cost of completeness.

### Symptom

In a Leiningen project, go-to-definition fails for any symbol whose namespace
lives in a transitive or version-less dependency, because that JAR is never
indexed.

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

### Stance: best effort, and never a JVM on the hot path

Leiningen is not a primary target for clj-pulse - `deps.edn` and let-go come
first - so its dependency support stays best effort. The original fixed
principle here was "clj-pulse will not start a JVM"; it has since been
narrowed to: **never a JVM on the hot path.** deps.edn projects now run
`clojure -Spath` in a background task (see the resolver table above) because
the clojure CLI itself skips the JVM when its cache is warm and accurate alias
navigation outranks JVM purity. The narrowed principle still rules out shelling
out to `lein classpath` — `lein` boots a JVM *every* run, warm or not — which
is the only fully accurate way to get Leiningen's transitive and
parent-inherited deps. Startup stays fast and self-contained, so we accept the
Leiningen gap rather than pay unconditional JVM cost.

Best-effort directions that respect the no-JVM rule, none urgent:

1. **Resolve version-less direct deps from `~/.m2`.** When a direct dep declares
   no version (the version comes from the parent project), look under
   `~/.m2/repository/<group>/<artifact>/` and take the only, or newest,
   downloaded version. Cheap and subprocess-free; covers parent-managed direct
   deps, but still not transitive ones.
2. **Reimplement enough Maven resolution in Rust.** Parse each dep's `pom.xml`
   and the parent's `:managed-dependencies`, then walk the tree. No subprocess,
   but a large and fragile effort: version ranges, exclusions, and profiles all
   apply.

For complete, accurate Leiningen support, clojure-lsp (which embeds clj-kondo
and resolves the real classpath) remains the better tool. See also the
"Leiningen transitive deps" item under best effort in [ROADMAP.md](ROADMAP.md).
