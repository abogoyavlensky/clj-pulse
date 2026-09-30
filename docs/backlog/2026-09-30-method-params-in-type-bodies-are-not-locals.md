# Method params in `deftype`/`defrecord`/`extend-*` bodies are not locals

- **Found:** 2026-09-30, `bb compare` re-run on the clj-kondo corpus while
  closing the two gap-resolved local entries (`docs/MEMORY.md`, "Re-check of
  the two gap-resolved local entries"); bucket `local/plain`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

Definition, references and rename on a parameter of a method implementation
inside a `deftype`, `defrecord`, `extend-protocol` or `extend-type` body
answer null: the parameter is not a local to the scope walker, and it is
not a var either, so nothing resolves.

## Evidence

All nine `local/plain` answers the run flags (3 diverge, 6 null) are this
class:

- `parser/clj_kondo/impl/rewrite_clj/node/coerce.clj:63:7` `v` — definition
  → null; expected `:60`, the `(coerce [v] …)` argv under
  `(extend-protocol NodeCoerceable Object …)`.
- `parser/clj_kondo/impl/rewrite_clj/node/coerce.clj:107:12` `sq` —
  references and rename → null; expected `{:107, :108}`.
- `parser/clj_kondo/impl/rewrite_clj/node/protocols.clj:26:17` `this` —
  definition → null; expected `:26` (`(sexpr [this] this)`).
- `parser/clj_kondo/impl/rewrite_clj/node/reader_macro.clj:66:18` `this` —
  definition → null; expected `:65` (`(toString [this] …)` under `Object`).
- `parser/clj_kondo/impl/rewrite_clj/node/seq.clj:82:27` `children` —
  references and rename → null; expected `{:82, :83}`
  (`(replace-children [this children'] …)`).
- `inlined/clj_kondo/impl/toolsreader/v1v2v2/clojure/tools/reader/reader_types.clj:143:23`
  `reader` — references and rename → null; expected `{:143}`
  (`(get-line-number [reader] …)` in a `deftype`).

## Where to look

`extractor::walk_scope` / `walk_scope_def`. The walker reaches the method
list `(coerce [v] …)` by generic descent: its head is neither a core form nor
a def, so the argv never binds and `locals_at_node` has no `v`. The
`Defrecord | Deftype` arm binds the fields and descends into each child, but
never treats a child list as a method implementation, and `extend-protocol`,
`extend-type` and `reify` have no arm at all. The occurrence walker already
has the twin, `walk_method_impl`, which is what the scope walker should
mirror so the two agree about what binds.

## Verify

`test_extractor.rs`: `(extend-protocol P Object (m [v] v))` with definition
on the body `v` landing on the argv, and a `deftype` method whose param
shadows a field; `bb compare` `local/plain` on clj-kondo (594 probes, 3
diverge, 6 null on 2026-09-30, all this class).
