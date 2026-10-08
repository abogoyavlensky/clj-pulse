# A var-quote of a shadowed name is not recorded as the var

- **Found:** 2026-10-07, while making a cursor on a reader prefix resolve the
  symbol (`docs/plans/2026-10-07-2258-cursor-on-reader-prefix.md`).
- **Status:** open.

## Symptom

```clojure
(def state (atom 0))
(defn f [] (let [state 2] [#'state state]))
```

Clojure's `var` form ignores locals, so `#'state` is `#'user/state`, the
global. clj-pulse records no occurrence for it: definition and references from
`#'state` answer `null`, references from the `def` omit it, and a rename of
the var leaves `#'state` pointing at the old name. clj-kondo is wrong the other
way: it records the `state` as a use of the local.

## Evidence

`tests/fixtures/simple_project/src/prefixes.clj`, `pfx-shadow`. The e2e test
`test_e2e_prefix_cursor_var_quote_skips_a_shadowing_local` only pins that the
answer is never the local; `test_compare.rs` allowlists kondo's local reading
(`after_var_quote`).

## Where to look

The occurrence walker treats a symbol whose name is a local in scope as the
local and records nothing. A `var_quoting_lit` value should skip that check
and resolve as a var, the way the head of a call does not. The locals walker
must likewise not count `#'x` as a use of the local `x`, or the unused-binding
lint and local references disagree with the var.

## Verify

Tighten the e2e test to assert definition lands on the `def`, and that a rename
of `pfx-state` from the `def` rewrites `#'pfx-state` inside `pfx-shadow`.
