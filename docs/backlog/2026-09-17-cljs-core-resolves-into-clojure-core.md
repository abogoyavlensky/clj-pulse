# `.cljs` core resolves into `clojure/core.clj`

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-usage/library`, `var-usage/library/macro`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

In a `.cljs` file, `not`, `declare`, `defn-`, `when`, … navigate to
`clojure/core.clj` in the Clojure jar, never `cljs/core.cljs` in the
ClojureScript jar. The twin of the 2026-09-10 backlog item (a `.clj` file
landing in `clojure/string.cljs`), which `bb compare` now reports as
`wrong dialect` (`edn/read-string` at `extract/clj_kondo/impl/ExtractJava.clj:63`
→ `clojure/edn.cljs`).

## Evidence

- `inlined/clj_kondo/impl/toolsreader/v1v2v2/cljs/tools/reader/edn.cljs:30:9` `not`,
  `:27:2` `declare`, `:163:2` `defn-` → `…/clojure-1.11.4.jar!/clojure/core.clj`.
- `var-usage/library` on the first run: 4 agree, 120 diverge, 43 null.

## Where to look

Library symbol lookup keys by fqn and the last dialect indexed wins; the
resolver has to prefer the asking file's dialect (`Dialect` in
`tests/common/sites.rs` states the acceptable set), and `cljs.core` has to be
the core namespace of a `.cljs` file.

## Verify

`test_e2e.rs` with both jars on a `.cpcache` classpath; `bb compare`
`var-usage/library*` on clj-kondo (the `inlined/` cljs tree) and the
`wrong dialect` lines going away.
