# A qualified `{:keys [c/x]}` entry resolves as the keyword it reads, not the local it binds

- **Found:** 2026-09-17, first `bb compare` run on the clj-kondo corpus
  (`docs/MEMORY.md`, "Compare against clj-kondo analysis"); buckets `local/destructured`.
- **Status:** open. Sites are `file:line[:col]` in the pinned checkout under
  `.tmp/bench/clj-kondo/`; `bb compare` reprints them.

## Symptom

From the binding token of `{:keys [c/x]}`, references answer the one
keyword occurrence (`:c/x`); from a usage of `x` they answer the local and
its binding. `documentHighlight` follows references, so the two cursors
highlight different things.

## Evidence

- `tests/fixtures/simple_project/src/alias_sites.clj:10` `(defn g [{:keys [c/x]}] x)`:
  references at the binding answer `[10:17-20]`; at the usage `[10:19-20, 10:24-25]`.
  In `KNOWN` (`tests/test_compare.rs`) under this reason; remove the entry
  when fixed.

## Where to look

`references::local_refs_at` never claims a qualified symbol ("locals are
never qualified", `references.rs:94`); a qualified `:keys` entry is the one
exception, and `local_references_at` already knows it binds the name part.

## Verify

The `KNOWN` entry's removal makes `compare_simple_project` the test.
