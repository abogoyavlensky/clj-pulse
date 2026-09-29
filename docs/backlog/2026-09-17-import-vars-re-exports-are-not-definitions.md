# `import-vars` re-exports are not definitions

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-def/import-vars`, `var-usage/project/aliased`, `var-def/defmacro`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

kondo's `potemkin/import-vars` config makes each re-exported name a var of
the importing namespace; clj-pulse knows nothing of it, so a usage through
the facade alias has no definition, and the original's references miss every
caller that went through the facade.

## Evidence

- `parser/clj_kondo/impl/rewrite_clj/node.clj:26` `(import-vars [… coerce children …])`:
  references on `coerce` answer 2 sites (one is `src/clj_kondo/hooks_api.clj:106`,
  a usage of the facade), rename refused; 75 of 98 `var-def/import-vars`
  probes diverge.
- `parser/clj_kondo/impl/rewrite_clj/node/indent.clj:40:25` `node/string`:
  definition null; kondo points at `node/protocols.clj:19`.
- `parser/clj_kondo/impl/rewrite_clj/node/protocols.clj:101` `make-printable!`
  (a `defmacro`): references miss the 5 callers that spell it
  `node/make-printable!` (`node/indent.clj:46`, `node/meta.clj:35`, …).

- `var-def/defprotocol+` (0 of 26, 2026-09-29): the protocol
  `parser/clj_kondo/impl/rewrite_clj/node/protocols.clj` defines through
  `defprotocol+` is indexed with its methods, and every caller the probes
  miss spells a method or `Node` through the facade namespace
  `clj-kondo.impl.rewrite-clj.node`: `node/tag` at `node/indent.clj:29`,
  `parser/core.clj:174`, `hooks_api.clj:91`; bare `tag` at `node.clj:122`
  beside `(import-vars …)`; `node/Node` in `defrecord` specs at `node.clj:38`,
  `node/indent.clj:27`, `parser/core.clj:193`, `node/meta.clj:9`,
  `node/reader_macro.clj:11`, `:42`, `:69`. The 16 extra sites per method
  are implementations, the protocol entry of `KNOWN`.

Also the metabase shape (`potemkin/import-vars` in `metabase.events.core`,
which `sites::namespace_file` already works around).

## Where to look

A `:lint-as … potemkin/import-vars` (and a bare `potemkin/import-vars`)
form could define alias symbols pointing at the originals; pairs with the
2026-09-05 backlog line on kondo analysis as an enrichment source.

## Verify

A fixture with `import-vars`; `bb compare` `var-def/import-vars`.
