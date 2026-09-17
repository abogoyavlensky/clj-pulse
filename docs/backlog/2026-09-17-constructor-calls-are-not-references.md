# Constructor calls are not references of a `deftype`/`defrecord`

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `var-def/deftype`, `var-def/defrecord`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

`(StringReader. s)` is not a reference of `StringReader`; neither is
`(ReaderConditional. …)` of the record, nor `->Foo`/`map->Foo` calls of a
`defrecord` (kondo defines those three names at the record's name token).

## Evidence

- `inlined/clj_kondo/impl/toolsreader/v1v2v2/cljs/tools/reader/reader_types.cljs:41`
  `StringReader`: 1 site answered, the constructor call at `:213` missing.
  All 20 `var-def/deftype` probes diverge.
- `inlined/clj_kondo/impl/toolsreader/v1v2v2/cljs/tools/reader/impl/utils.cljs:22`
  `ReaderConditional`: 4 of 5, `:33` missing.

## Where to look

The occurrence walker sees `StringReader.` as one symbol; the trailing dot
has to be stripped to record an occurrence of the type (Java interop
`(java.io.File. …)` must stay untouched — the name resolves to a
`deftype`/`defrecord` in the index or it is not one).

## Verify

`test_extractor.rs` occurrence of `Foo` from `(Foo. 1)`; `bb compare`
`var-def/deftype` on clj-kondo.
