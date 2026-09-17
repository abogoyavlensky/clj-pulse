# Defs nested in a wrapping macro are not definitions

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-def/defrecord`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

A `defrecord`/`defn` inside a non-`do` wrapping form
(`(compile-when <=clojure-1-7-alpha5 (defrecord TaggedLiteral [tag form]))`)
is not indexed: references answer null and rename is refused.

## Evidence

- `inlined/clj_kondo/impl/toolsreader/v1v2v2/clojure/tools/reader/impl/utils.clj:34`
  `TaggedLiteral`.

## Where to look

`process_top_level_list` descends into `do` (and reader conditionals) only;
kondo treats an unknown macro's body forms as top level. Descending into
any top-level list whose head is not a known special/def form would match.

## Verify

`test_extractor.rs`: a def inside `(when-feature x (defn f []))`;
`bb compare` `var-def/defrecord`.
