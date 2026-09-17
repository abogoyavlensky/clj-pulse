# A `defmulti` is missing from its own references

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-def/defmulti`, `var-def/declare`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

References (with `includeDeclaration`) and rename on a multimethod answer
every `defmethod` and caller, but not the `defmulti` line itself — a rename
would leave the multimethod under the old name.

## Evidence

- `src/clj_kondo/impl/analyzer/re_frame.clj:102:11` `analyze-dispatch-type`
  (no `declare` before it): 5 sites answered, kondo 6; the missing one is `:102`.
- `inlined/clj_kondo/impl/toolsreader/v1v2v2/clojure/tools/reader/impl/inspect.clj:37:11`
  `inspect*` (a `declare` at `:11`): 17 of 18, `:37` missing.

## Where to look

The `defmulti` definition's symbol range, or the references handler's
declaration site: a `Defmulti` kind may not be counted as a declaration the
way `Defn` is.

## Verify

`test_e2e.rs` references on a `defmulti` name including its own line;
`bb compare` `var-def/defmulti` (3 of 6 on the first run).
