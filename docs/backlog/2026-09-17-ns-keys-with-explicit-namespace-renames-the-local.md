# `{:ns/keys [a]}` with an explicit namespace renames the local

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `keyword/keys`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

`::keys` and `::alias/keys` entries refuse rename (the destructuring message),
but a `{:clj-kondo/keys [config ignore]}` entry — a literal namespace, no
alias — is renamed as a plain local: the binding and its usages are edited
while the key it reads stays `:clj-kondo/config`.

## Evidence

- `src/clj_kondo/impl/analyzer/namespace.clj:571:46` — rename of `config`
  answers an edit of `:571`, `:572`; expected refused. Same for `ignore`
  (`:571:53`) and for `{:clj-kondo/keys [config lint-as ignore]}` at
  `src/clj_kondo/impl/analyzer.clj:1179`.

## Where to look

The occurrence walker's `:keys` detection recognizes `:keys`, `::keys` and
`::alias/keys`; a `:full.ns/keys` form is missed, so no keyword occurrence is
recorded at the entry and `rename_target` never sees a destructuring site.

## Verify

`test_e2e.rs` rename refusal on `{:full.ns/keys [x]}`; `bb compare`
`keyword/keys` (5 of 5 diverge on the first run).
