# `one-of` in a `for` `:let` resolves to its own line

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-usage/project/macro`.
- **Status:** fixed 2026-09-18 by the gaps change (plan `docs/plans/2026-09-18-0751-discards-and-comments-are-gaps.md`); regression test added 2026-09-30 (plan `docs/plans/2026-09-30-0907-binding-let-locals-and-for-let-macro-heads.md`); archived. Sites were `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

`(one-of require-kw [:require :require-macros :use :require-global])` inside
a `for` `:let` vector: definition on `one-of` answers the usage's own line,
and later usages in the same file answer the *first* usage — something in
that `for` binds the head symbol.

Nothing in the `for` binds it: the mechanism was the `;; nested syntax
quotes …` comment inside the `let` vector at `usages.clj:138`, which shifted
the pairs so `(one-of t […])` landed in a name slot; the `:let` rebinding
was a bystander (re-check of 2026-09-30).

## Evidence

- `src/clj_kondo/impl/analyzer/namespace.clj:625:45` → `:625`; expected
  `src/clj_kondo/impl/utils.clj:419`.
- `src/clj_kondo/impl/analyzer/usages.clj:140:24` → `:140`, and `:290`, `:299`,
  `:315`, `:317`, `:326` → `:140`.
- 29 `var-usage/project/macro` divergences on the first run, all `one-of`.

The shape at `:622-626`: `(for [?require-clause clauses :let [require-kw-node (-> … first) require-kw (:k require-kw-node) require-kw (one-of require-kw […])] :when require-kw] …)`.
The corpus's `.clj-kondo/config.edn` has no `:lint-as` for `one-of`
(it is a `defmacro` in `clj-kondo.impl.utils`, `:refer`red).

## Where to look

`walk_scope` on `for`/`doseq` modifiers: the `:let` vector's pairs, or the
rebinding of `require-kw` to a value that starts with the head symbol.

## Verify

`test_extractor.rs`: a `for` with `:let [a (f a) a (g a)]`, definition on `g`;
`bb compare` `var-usage/project/macro`.
