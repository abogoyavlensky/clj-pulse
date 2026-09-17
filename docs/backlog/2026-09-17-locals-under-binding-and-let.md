# Locals inside `(binding […] (let […] …))` resolve wrong

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `local/plain`, `local/destructured`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

Inside a `let` nested in `binding`, references on a local find the binding
alone, and a rebinding of a name that an outer `:keys` destructuring also
binds resolves to the outer binding instead of its own usages.

## Evidence

`src/clj_kondo/core.clj`, the `run!` function (`:105-145`): params
`[{:keys [… config … copy-configs …] :or {cache true} :as args}]`, then
`(let [copy-configs …] (binding [hooks/*debug* debug] (let [… cfg-dir (cond …) config (core-impl/resolve-config cfg-dir …) …] …)))`.

- `cfg-dir` at `:133:13` — references answer 1 site (the binding); kondo has 5
  (`:139`, `:143`, `:187`, `:245`).
- `config` at `:139:13` — references answer 2 sites, one of them the outer
  `:keys` entry at `:113`; kondo has 4 (`:140`, `:141`, `:142` plus the binding).
  Rename there is refused (the `:keys` message), though the cursor is on the
  inner `let` binding.
- `copy-configs` at `:126:9` — 1 of 2.

## Where to look

`extractor::local_references_at` / `walk_scope`: the binding whose name and
value sit on different lines (`cfg-dir\n (cond …)`) and the `binding` form
between the two `let`s are the two shapes to isolate.

## Verify

`test_extractor.rs` with that exact nesting; `bb compare` `local/plain` on
clj-kondo (36 diverge on the first run).
