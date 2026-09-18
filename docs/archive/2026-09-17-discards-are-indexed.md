# `#_` discards are indexed

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `local/plain`, `var-usage/core`, `var-usage/project`, `var-def/defn`.
- **Status:** fixed 2026-09-18 (plan `docs/plans/2026-09-18-0751-discards-and-comments-are-gaps.md`); archived. Sites were `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

Symbols inside a `#_(…)` discard count as occurrences, and a `#_#_` pair
inside a `let` binding vector is not skipped as a form, so every pair after
it shifts by one and the values become binding names.

## Evidence (clj-kondo corpus, pinned commit)

- `extract/clj_kondo/impl/ExtractJava.clj:48` — `#_(assoc-in [name :arities …] {})`:
  references on the local `entry` (`:39`) answer 5 sites, kondo 4; the extra
  one is inside the discard.
- `extract/clj_kondo/impl/extract_var_info.clj:139-151` — the `let` holds
  `#_#_predicates-by-ns (group-by …)` twice; from there on `group-by`,
  `comp`, `namespace`, `key` at `:144` resolve to *the file itself* as if
  bound, and `(extract-clojure-core-vars)` at `:148` resolves to its own line.
  References on `extract-clojure-core-vars` (`:52`) miss that usage.

## Where to look

`extractor::walk_scope` / the binding-vector pair walk: a `dis_expr` node
(and its discarded form) has to be stepped over before pairing; the
occurrence walker has to skip `dis_expr` subtrees altogether.

## Verify

A `test_extractor.rs` case with `#_` in a `let` vector and in a body; `bb compare`
buckets `local/plain` and `var-usage/core` on clj-kondo lose these sites.
