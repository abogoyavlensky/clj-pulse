# `:lint-as` to `defprotocol` and `declare` is only half honored

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-def/defprotocol+`, `var-def/programs`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

A macro mapped by `:lint-as` to `clojure.core/defprotocol` defines its
protocol name but its methods miss their callers; one mapped to
`clojure.core/declare` (`me.raynes.conch/programs`) declares nothing, so
rename on a declared name is refused.

## Evidence

- `parser/clj_kondo/impl/rewrite_clj/node/protocols.clj:10:4` `tag`
  (`defprotocol+`, `:lint-as … clojure.core/defprotocol` in the corpus's
  `.clj-kondo/config.edn`): references answer 26 sites, 16 of them
  implementations kondo does not list, and miss 6 callers such as
  `parser/clj_kondo/impl/rewrite_clj/node.clj:122`, `node/indent.clj:29`. All
  26 `var-def/defprotocol+` probes diverge.
- `test/clj_kondo/test_utils.clj:216` `(programs rm mkdir mv)`: rename of
  `rm`/`mkdir`/`mv` refused; kondo has the usages at `:238`, `:243`, `:248`.

## Where to look

`DefKind` from `:lint-as`: `Defprotocol` extraction of the method list and a
`Declare` mapping for the head are the two paths the config table does not
reach (`macro_def_kind` / `process_top_level_list`).

## Verify

`lint_as_project` fixture with a `defprotocol`-mapped and a `declare`-mapped
macro; `bb compare` `var-def/defprotocol+` and `var-def/programs`.
