# Keywords in binding values and quoted data are not occurrences

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `keyword/qualified`, `keyword/alias`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

A qualified keyword that sits in the *value* of a `let`/`if-let` binding, or
inside quoted data, is missing from references and rename, and a cursor on it
answers null. A keyword rename that skips quoted config is a rename that
breaks it.

## Evidence

- `src/clj_kondo/impl/analyzer.clj:1577` — `generated? (:clj-kondo.impl/generated expr)`
  and `:1587`: absent from the 48-site group; references from
  `src/clj_kondo/hooks_api.clj:18` answer 46.
- `src/clj_kondo/impl/analyzer.clj:3023` — `(if-let [ic (some-> (first cs) meta ::types/infer-call)]`:
  references at that position answer null; from `src/clj_kondo/impl/analyzer/usages.clj:179`
  the group lacks `:3023` and `:3025`.
- `test/clj_kondo/analysis/java_test.clj:32` — `'{:deps {… :mvn/version "1.10.3"}}`:
  12 of the 15 `:mvn/version` sites kondo knows are in quoted maps and missing.

## Where to look

The occurrence walker (`extractor::walk_list` / `walk_scope`): binding-vector
values and `quote` forms are not descended into for `kwd_lit`.

## Verify

`test_extractor.rs`: keyword occurrences inside `(let [x (:k m)])`,
`(if-let [x (some-> m ::k)])` and `'{:k 1}`; `bb compare` bucket
`keyword/qualified` (97 diverge + 31 null on the first run).
