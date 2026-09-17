# One namespace in a `.clj` and a `.cljs` file shares one fqn

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-def/defn`, `var-def/defprotocol`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

When the same namespace name is defined in a `.clj` file (macros) and a
`.cljs` file, references and rename on a var in one file also answer sites
from the other, and the index's one-namespace-per-name rule makes the last
file win for definitions.

## Evidence

- `inlined/clj_kondo/impl/toolsreader/v1v2v2/cljs/tools/reader/reader_types.cljs:260`
  `source-logging-reader?`: references answer an extra site in
  `reader_types.clj:7`, the macro file of the same namespace. Protocol methods
  there (`read-char` `:20`, `peek-char` `:22`) merge the same way.

## Where to look

`Index` keys namespaces and fqns without a dialect; the `namespaces`
map's "last one wins" warning fires for this pair. Whether the answer is a
dialect-aware key or a per-file filter on references depends on the
`.cljs`-core item (same backlog date).

## Verify

`test_index.rs` with `a/b.clj` and `a/b.cljs`; `bb compare`
`var-def/defprotocol` on the `inlined/` tree.
