# Macros referred from `.cljs` through `:require-macros`

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-def/defmacro`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

A `defmacro` in a `.clj` file used from `.cljs` files via `:require-macros` /
`:refer-macros` has no references in those files.

## Evidence

- `inlined/clj_kondo/impl/toolsreader/v1v2v2/cljs/tools/reader/reader_types.clj:3`
  `log-source`: 1 site answered, kondo 4 (`…/cljs/tools/reader.cljs:14`, `:397`, …).

## Where to look

`NsMeta` from the ns form: `:require-macros` and `:refer-macros` are not read
into `requires`/refers, so the `.cljs` file cannot resolve the name.

## Verify

`test_extractor.rs` ns-form parse of `(:require-macros [a.b :refer [m]])`;
`bb compare` `var-def/defmacro`.
