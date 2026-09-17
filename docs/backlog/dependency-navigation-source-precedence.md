# Dependency navigation can select a shadowed source

**Status: open**

## Problem

`bb bench metabase` at pinned corpus `42a8e9f7` reproduced four incorrect
source selections on 2026-09-17. The cold clj-pulse run resolved 157 of 161
independently selected dependency probes within 120 seconds. The other four
returned locations, but in different resources from those Clojure would load:

| Var | Expected | Observed |
|---|---|---|
| `medley.core/find-first` | `medley/medley` 1.4.0, `medley/core.cljc` | `dev.weavejester/medley` 1.9.0, same resource |
| `riddley.compiler/tag-of` | riddley 0.2.0, `riddley/compiler.clj` | same JAR, `compiler.clj` |
| `cognitect.transit/tagged-value` | transit-clj 1.0.333, `cognitect/transit.clj` | transit-cljs 0.8.280, `cognitect/transit.cljs` |
| `clojure.spec.alpha/spec?` | spec.alpha 0.5.238, `clojure/spec/alpha.clj` | clojure-future-spec 1.9.0, same resource |

The harness reads `clojure -A:dev:test -Spath`, selects public definitions from
unshadowed namespace resources, and checks source and position. See
`tests/bench/dependencies.rs`; `BENCH_JSON` records exact expected locations
and observed responses. Both servers use that classpath command.

The production index has no matching library precedence rule:
`Index::insert_lib_file` in `src/index/mod.rs:522` replaces an existing library
symbol whenever another library supplies its FQN. `index_classpath_jars` in
`src/index/scanner.rs:291` inserts every extracted namespace, and
`src/index/jar.rs` scans `.clj`, `.cljc`, and `.cljs` resources together,
including paths that do not match the namespace. This explains how shadowed
resources reach definition lookup. Project-over-library precedence already
exists and must be preserved.

## Proposed fix

Define deterministic resource selection for a Clojure request before inserting
library symbols: `.clj` before `.cljc` across the classpath, classpath order
within an extension, and namespace-relative resource paths. Preserve language
information so ClojureScript support does not override a Clojure definition.
Check directory dependencies, multi-project classpaths, and cold/warm JAR cache
reconstruction as well as the direct JAR scan. Add focused fixtures for these
collisions, then rerun the unchanged benchmark probes.

## Why it is left alone

The current task changes benchmark measurement, not production indexing.
Correcting library selection affects navigation and completion across projects
and languages and needs its own plan and server/editor verification.

## Origin

Found while executing
[the dependency readiness benchmark plan](../plans/2026-09-16-2149-dependency-readiness-benchmark.md),
2026-09-17. The incomplete result is retained in the performance report.
