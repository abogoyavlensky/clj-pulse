# Index Declared Source Roots at Startup Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make "find references" / rename see usages across the whole project at startup by scanning declared source roots (`deps.edn` top-level `:paths` plus every alias's `:extra-paths`) and always including `src/` and `test/` by default — not just top-level `:paths`.

**Tech Stack:** Rust, tower-lsp, tree-sitter-clojure, `edn-format`.

---

## Design

### The bug

From a `defn` definition, "go to references" misses usages in files that have not been opened yet. Reproduced via the e2e harness: a usage in an unopened `src/` file **is** found at startup, but a usage in a `test/` directory (not in top-level `:paths`) is **not** found until the file is opened.

### Root cause

References and rename already search the full `index.occurrences` map plus live open documents (see `handlers/references.rs::occurrences_for`) — so the lookup is not the problem. The gap is **which directories get scanned at startup**. `config::source_paths` returns only top-level `:paths` from `deps.edn` (or `lgx.edn`), so `test/`, `dev/`, and anything declared via an alias's `:extra-paths` are never indexed until opened. Their occurrences are therefore absent from the index, and references miss them.

### Fix (approved)

Broaden the startup scan set to **declared source roots plus conventional defaults**:

- **Declared roots:** top-level `:paths` **∪ every alias's `:extra-paths`**. In idiomatic `deps.edn`, `test/` lives in `:aliases {:test {:extra-paths ["test"]}}` and `dev/` similarly, so this picks them up.
- **Defaults:** always also scan `src/` and `test/` (relative to root), so a project that declares neither still gets both indexed. Non-existent directories are skipped by the file walker.

This respects the existing "declared paths gate indexing" model (it does not blindly walk the whole root, so it never swallows in-root dependency checkouts such as `:local/root`/lgx vendor dirs), and the only behavior change is a larger, more complete scan set.

### Key decisions

- **Alias `:extra-paths` in, alias `:paths` out.** `:extra-paths` is additive source (test/dev); an alias's `:paths` *replaces* the base paths and is typically build tooling (tools.build `:build`). Keeps the existing `test_alias_paths_are_ignored` behavior.
- **Always include `src` and `test` defaults**, unioned with declared roots and de-duplicated. Covers projects that don't declare `test/` anywhere.
- **Parse `deps.edn` with `edn-format`** instead of the hand-rolled string scanner. The scanner cannot cleanly reach into the `:aliases` map; EDN parsing is robust and is already how `lgx.edn` is read. On a parse error, fall back to defaults (`src`/`test`).
- **DRY the EDN helpers.** Extract the small primitives currently private in `lgx.rs` (`kw`, `kw_ns`, `get`, `as_str`) into a shared `src/edn.rs` module, plus a `str_vec_at` helper for reading a vector-of-strings at a key. `lgx.rs` and `config.rs` both use it.
- **let-go unchanged in spirit:** `lgx.edn` has no aliases, so its declared roots stay its `:paths`; the `src`/`test` defaults now apply uniformly to let-go too.

### Data flow (after change)

```
startup / deps.edn|lgx.edn change
  → config::source_paths(root)
      = declared_roots(root)            // :paths ∪ alias :extra-paths (deps.edn) | :paths (lgx.edn)
        ∪ {"src", "test"}               // always, de-duplicated
        → each joined to root
  → scanner::build_index walks them (non-existent dirs skipped)
  → occurrences for all those files land in the index
  → references/rename find them without opening the files
```

### Testing strategy

- Unit (`edn.rs`): `str_vec_at` reads a string vector at a key; returns `None`/empty for missing or wrong-typed keys.
- Unit (`config.rs`): top-level `:paths` ∪ alias `:extra-paths`; alias `:paths` still ignored; `src`/`test` defaults always present and de-duplicated; all existing parser tests still pass.
- e2e (`test_e2e.rs`):
  - Usage in a `test/` dir declared via `:aliases {:test {:extra-paths ["test"]}}` is found by references from the definition **without opening the test file**.
  - Usage in a `test/` dir that is **not declared anywhere** (covered only by the default) is likewise found.
- `bb check` + `bb e2e`.

## File Structure

- `src/edn.rs` — **new.** Shared EDN helpers: `kw`, `kw_ns`, `get`, `as_str`, `str_vec_at`. One clear responsibility: thin typed accessors over `edn_format::Value`.
- `src/lib.rs` — register `pub mod edn;`.
- `src/lgx.rs` — use the shared helpers; remove the now-duplicated private ones.
- `src/config.rs` — `edn-format`-based `deps.edn` parsing collecting `:paths` ∪ alias `:extra-paths`; `source_paths` unions the `src`/`test` defaults and de-duplicates; remove `find_top_level_paths`; update/extend tests.
- `tests/test_e2e.rs` — references-into-unopened-`test/` regression tests (declared via alias, and via default).

## Task Structure

### Task 1: Shared `edn` helpers module + DRY lgx

**Files:**
- Create: `src/edn.rs`
- Modify: `src/lib.rs`, `src/lgx.rs`

- [ ] **Step 1: Write the failing test**
  In `src/edn.rs`, add a `#[cfg(test)]` test `str_vec_at_reads_string_vector`: parse `{:paths ["a" "b"] :n 1}` with `edn_format::parse_str`, assert `str_vec_at(&map, kw("paths"))` returns `Some(vec!["a","b"])`, and that a missing key and a non-vector value both return `None`.

- [ ] **Step 2: Run test to verify it fails**
  Run: `cargo test --lib edn::tests::str_vec_at_reads_string_vector`
  Expected: FAIL (module/function does not exist).

- [ ] **Step 3: Implement the shared helpers**
  Create `src/edn.rs` with `pub(crate)` fns moved from `lgx.rs`: `kw(name) -> Value`, `kw_ns(ns, name) -> Value`, `get(&BTreeMap<Value,Value>, Value) -> Option<&Value>`, `as_str(&Value) -> Option<&str>`, plus `str_vec_at(&BTreeMap<Value,Value>, Value) -> Option<Vec<String>>` (returns the strings of a `Value::Vector` at the key, `None` if absent or not a vector). Register `pub mod edn;` in `src/lib.rs`.

- [ ] **Step 4: Refactor lgx to use the shared helpers**
  In `src/lgx.rs`, replace the private `kw`/`kw_ns`/`get`/`as_str` with `use crate::edn::{...}`; delete the duplicated definitions. Behavior is unchanged.

- [ ] **Step 5: Run tests to verify they pass**
  Run: `cargo test --lib edn:: lgx::`
  Expected: PASS — new `edn` test plus all existing `lgx` tests.

- [ ] **Step 6: Commit**
  `git commit -m "Add shared edn helper module and reuse it in lgx"`

### Task 2: deps.edn alias `:extra-paths` + default `src`/`test` roots

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing tests**
  In `config.rs` tests:
  - `test_alias_extra_paths_included`: `parse_paths_from_deps_edn(r#"{:paths ["src"] :aliases {:test {:extra-paths ["test"]}}}"#)` returns `Some(vec!["src","test"])`.
  - `test_multiple_alias_extra_paths_unioned`: two aliases each contributing `:extra-paths` are all collected.
  - `test_source_paths_always_includes_src_and_test`: in a temp project whose `deps.edn` is `{:paths ["src"]}`, `source_paths(root)` contains both `root/src` and `root/test` with no duplicate `src`.
  Keep the existing parser tests (`test_alias_paths_are_ignored`, `test_top_level_paths_after_aliases`, `test_extra_paths_not_matched`, comment/string tests, `test_no_paths_returns_none`) — they must still pass under the new implementation.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib config::tests`
  Expected: FAIL on the new tests (alias `:extra-paths` not collected; `test` default not added).

- [ ] **Step 3: Rewrite the parser with `edn-format`**
  Replace `parse_paths_from_deps_edn` internals (and delete `find_top_level_paths`) with an `edn_format::parse_str` implementation using `crate::edn` helpers: collect top-level `:paths` strings, then for each value in the `:aliases` map collect its `:extra-paths` strings; union (top-level first, then aliases). Return `None` when nothing is collected. On parse error return `None`.

- [ ] **Step 4: Add `src`/`test` defaults + de-dup in `source_paths`**
  Update `source_paths` so that, for both project kinds, it takes the declared roots (deps.edn parse, or `lgx::paths` for let-go; empty if none), then appends `"src"` and `"test"` if not already present, de-duplicates, and maps each to `root.join(p)`. Remove the old "return defaults only when `:paths` absent" branch — defaults are now always unioned.

- [ ] **Step 5: Run tests to verify they pass**
  Run: `cargo test --lib config::tests`
  Expected: PASS — new and existing tests.

- [ ] **Step 6: Commit**
  `git commit -m "Scan alias :extra-paths and always include src/test source roots"`

### Task 3: e2e references into unopened test/ directories

**Files:**
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Write the e2e test(s)**
  Add `test_e2e_references_into_unopened_test_dir`: in the `simple_project` fixture, write `deps.edn` as `{:paths ["src"] :aliases {:test {:extra-paths ["test"]}} :deps {...}}`, create `test/core_test.clj` with `(ns simple.core-test (:require [simple.core :as core])) (core/add 1 2)`, start + `initialize`, `wait_for_log("Indexed")`, `did_open` **only** `src/core.clj`, then `references` at the `add` definition (declaration excluded) and assert the result contains a location ending `/test/core_test.clj`. Add a second assertion variant (or sibling test) where `deps.edn` is `{:paths ["src"] :deps {...}}` (no test alias) to prove the `test/` **default** also works.

- [ ] **Step 2: Run the test(s)**
  Run: `cargo test --test test_e2e test_e2e_references_into_unopened_test_dir`
  Expected: PASS (with Tasks 1–2 in place).

- [ ] **Step 3: Commit**
  `git commit -m "e2e: references find usages in unopened test/ dirs"`

### Task 4: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Run the full check suite**
  Run: `bb check`
  Expected: PASS — fmt clean, clippy `-D warnings` clean, all unit tests pass.

- [ ] **Step 2: Run the e2e suite**
  Run: `bb e2e`
  Expected: PASS — including the new references tests and all existing navigation/diagnostics tests.
