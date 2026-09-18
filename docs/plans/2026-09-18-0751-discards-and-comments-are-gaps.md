# Discards and Comments Are Gaps Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The extractor never records a symbol inside a `#_` discard as an occurrence, a binding or a definition, and a discard or a `;` comment inside a binding vector, an argv position or any other positional form no longer shifts what follows it. Closes the backlog issue [`#_` discards are indexed](../backlog/2026-09-17-discards-are-indexed.md) (ROADMAP Backlog, 2026-09-17, promoted to Milestone 5).

**Tech Stack:** Rust, tree-sitter-clojure 0.1. Tests: `tests/test_extractor.rs`, then `bb compare` on the clj-kondo corpus.

**Branching:** `master` is at `release 0.5.3` with the dialect and kondo plans merged. Branch `discards-are-gaps` off `master`.

---

## Design

### Today

tree-sitter-clojure 0.1 declares no grammar extras (`grammar-src/src/grammar.json`, `extras: []`), so a `comment` node and a `dis_expr` node are ordinary *named children* of the list, vector or map they sit in. Verified with the parser on `(let [a 1 ;; c\n #_#_x (f) b 2] a)`: the vector's named children are `sym_lit a`, `num_lit 1`, `comment`, `dis_expr` (one node spanning `#_#_x (f)`, holding a nested `dis_expr x` and the `list_lit (f)`), `sym_lit b`, `num_lit 2`.

`extractor::named_children` returns them all. Every positional walker reads through it, so:

- `process_binding_pairs` and `walk_scope_binding_vec` chunk `[a 1 comment dis_expr b 2]` into pairs and bind `1`'s neighbour wrongly from the comment on; on the clj-kondo corpus `group-by`, `comp`, `namespace`, `key` become "locals" of `extract_var_info.clj:144` and the real usage of `extract-clojure-core-vars` at `:148` disappears;
- the generic arm of `walk_occurrences` descends into a `dis_expr` and records everything inside it, so `entry` at `ExtractJava.clj:48` gets a fifth reference clj-kondo does not list;
- `children[1]`, `children[2]` and `children.get(1)` positions in `walk_let_form`, `walk_binding_tail`, `walk_are_form`, `walk_fn_form`, def-form parsing and their `walk_scope_*` twins all shift when a comment or discard precedes the slot.

The `;` comment case is not in the backlog issue, but it is the same defect with the same fix, and far more common in real code.

### The target

**`named_children` skips gap nodes.** In `src/index/extractor.rs`:

```rust
/// A node the reader discards or ignores: never a form, never an argument
/// position. Skipped by `named_children`, so every positional walker counts
/// forms alone. Metadata nodes are *not* gaps: `has_private_meta` reads a
/// symbol's `meta_lit` children through the same helper.
fn is_gap(node: Node) -> bool {
    matches!(node.kind(), "comment" | "dis_expr")
}

fn named_children(node: Node) -> Vec<Node> // filters `is_gap`
```

Because a stacked `#_#_x (f)` is one `dis_expr` node, skipping the node skips both discarded forms.

**No entry point descends into a gap.** The two direct `root.named_child(i)` loops in symbol extraction and in `extract_analysis_tree` switch to `named_children(root)`. `walk_occurrences` gains the arm `"comment" | "dis_expr" => {}` so a caller handing it a gap node records nothing whatever route it took. `walk_scope`'s `descend_into` and `locals_at_node` already go through the helper.

**Who keeps its own walk, on purpose.**

- `src/handlers/code_action.rs` has a private `named_children` and keeps it: clean-ns preserves comments among require specs verbatim (`Plan::Keep`), and `collect_bare` is deliberately broad so a symbol inside a discard still keeps a require rather than removing it.
- `src/handlers/ignored_forms.rs` walks `dis_expr` nodes itself, since finding them is its job.
- `collect_qualified` (the `qualified_usages` API) already skips `dis_expr` and stays as it is.
- `extractor::collect_alias_usages` (behind `alias_sites_tree`) descends through `named_children` today and must keep seeing discards: an alias rename is textual and file-local, and a `#_(h/x)` left on the old alias breaks the moment it is uncommented. It switches to a raw sibling iteration (`all_named_children`, the unfiltered loop `named_children` wraps) with a comment saying why. `node_path_at` uses parent chains and keeps expanding through a discard for `selectionRange`.
- `is_destructured_key` reads `vec.prev_named_sibling()` to find the `:keys`/`:strs`/`:syms` directive, which bypasses the helper: `{:keys #_old [a]}` or a comment between the directive and the vector would let a rename of `a` through. It steps back over gap siblings (`while is_gap`) before judging.

**Consequences accepted.** A binding used only inside a discard is unused (`(let [a 1] #_a nil)` gets `unused-binding`), which is what clj-kondo reports. A cursor on a symbol inside a discard still navigates: `resolve_fqn_at` falls back to the word's alias for qualified tokens and definition's bare-word resolver handles the rest, both untouched. References and rename from there answer the sites outside discards, since the discarded token is not an occurrence; clj-kondo's analysis has no entry there either.

**`JarCacheEntry::format_version` bumps** from 17 to 18: a comment or discard between a def name and its argv changes the params and doc the extractor records for library symbols.

## File Structure

- Modify: `src/index/extractor.rs` — `is_gap`, `named_children` and `all_named_children`, the two root loops, the `walk_occurrences` arm, `collect_alias_usages`, `is_destructured_key`.
- Modify: `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION`.
- Modify: `tests/test_extractor.rs` — the cases below.
- Modify: `docs/ROADMAP.md`, `AGENTS.md`, `docs/MEMORY.md`; move `docs/backlog/2026-09-17-discards-are-indexed.md` to `docs/archive/`.

## Tasks

### Task 1: Promote the roadmap item

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Move the item**
  Remove the `#_` discards sub-bullet from the 2026-09-17 `bb compare` Backlog entry. Add an unticked Milestone 5 item directly above **Release**: **Discards and comments are gaps.** `#_` forms and `;` comments are named children in tree-sitter-clojure, so today they are indexed as code and shift every positional walk (a `#_#_` pair or a comment inside a `let` vector re-pairs the bindings after it). Link the issue file, and the line `Plan: [2026-09-18-0751-discards-and-comments-are-gaps.md](plans/2026-09-18-0751-discards-and-comments-are-gaps.md) — in progress`.

- [x] **Step 2: Commit**
  `git commit -am "Plan discards and comments as gaps"` (include this plan file).

### Task 2: Extractor tests and the fix

**Files:**
- Modify: `src/index/extractor.rs`, `src/index/jar_cache.rs`
- Test: `tests/test_extractor.rs`

- [x] **Step 1: Write the failing tests**
  Next to `test_qualified_usages_skips_reader_discard`, using `extract_analysis_with`, `occurrences_of`, `locals_in_scope_at` and `local_references_at` as the existing tests do:
  - `test_discarded_forms_are_not_occurrences`: `(ns x)\n(defn f [] #_unused/sym #_(g 1) (h 2))` with `g` and `h` defined in the file; `occurrences_of(.., "x/g")` is empty, `x/h` has one.
  - `test_stacked_discard_in_let_vector_does_not_shift_pairs`: `(ns x)\n(defn f [] nil)\n(let [a 1\n      #_#_x (f)\n      b 2]\n  (+ a b))`; no occurrence of `x/f`; `locals_in_scope_at` inside `(+ a b)` names exactly `a` and `b`; `local_references_at` on `b` finds the binding and one usage; `unused_bindings` is empty.
  - `test_comment_in_let_vector_does_not_shift_pairs`: the same shape with `;; note` in place of the discard, same assertions.
  - `test_discarded_usage_does_not_count_for_the_local`: `(let [a 1] #_a nil)`; `unused_bindings` holds `a`; `local_references_at` on the binding returns the binding alone.
  - `test_gap_before_argv_does_not_shift_defn`: `(ns x)\n(defn f #_"doc" ;; c\n  [y] y)`; the symbol `x/f` has params `["[y]"]` (match the existing param format in `test_extracts_defn_with_doc_and_params`) and `y` is a local in the body.
  - `test_discarded_top_level_def_is_not_a_symbol`: `(ns x)\n#_(defn gone [] 1)\n(defn kept [] 2)`; symbols name `kept` alone.
  - `test_alias_sites_include_discarded_usages`: `(ns x (:require [y.z :as h]))\n#_(h/one)\n(h/two)` with `alias_sites_tree` (see its existing tests for the call shape); the sites include the `h` of both usages.
  - `test_keys_directive_separated_by_a_gap_still_rejects_rename`: in `tests/test_e2e.rs` or the references unit tests, whichever holds today's `:keys` rejection test, add `{:keys #_old [a]}` and `{:keys ;; c\n [a]}` variants; `prepareRename` on `a` refuses with the destructuring message.

- [x] **Step 2: Run to verify they fail**
  Run: `cargo test --test test_extractor discard`, `cargo test --test test_extractor comment_in_let`, `cargo test --test test_extractor gap_before`, and the `:keys` gap test by its name
  Expected: the let, comment, argv and `:keys` gap cases FAIL today; the top-level and alias cases may already pass, and the alias one must *keep* passing after the filter lands.

- [x] **Step 3: Implement**
  `is_gap`, `all_named_children` (the raw loop) and `named_children` filtering through it, the two root loops, the `walk_occurrences` arm, `collect_alias_usages` on `all_named_children`, the gap-skipping step back in `is_destructured_key`, and `CACHE_FORMAT_VERSION = 18`.

- [x] **Step 4: Run to verify they pass**
  Run: `cargo test --test test_extractor` then `bb check`
  Expected: PASS and green. If a positional test elsewhere breaks, it was relying on a comment being counted; fix the walker, not the test.

- [x] **Step 5: Commit**
  `git commit -am "Treat discards and comments as gaps in the extractor"`

> Deviation: the `:keys`-gap test lives in `tests/test_extractor.rs` (asserting `LocalRefs.destructured_key`, the flag `rename_target` reads) rather than in `test_e2e.rs`; it exercises `is_destructured_key` directly without a server.
> Note: `test_gap_before_argv_does_not_shift_defn` and `test_discarded_top_level_def_is_not_a_symbol` already passed before the fix (def-form parsing tolerated gaps); kept as regression guards.
> Deviation (codex review, must-fix): with no occurrences recorded inside a discard, `alias_sites_tree` could no longer tell a discarded `{:keys [h/x]}` binding entry from data and would rewrite the literal key. Added `discarded_keyword_starts`: it runs the occurrence walker over each discard's forms with an empty ns context and a scratch scope, only to learn where keyword occurrences start. Regression test `test_alias_sites_skip_discarded_binding_entries`. Commit 63c2c3b.
> Note: `bb check` initially failed on `test_e2e_kondo_cache_not_warmed_without_a_clj_kondo_dir` on `master` too — an empty, gitignored `tests/fixtures/kondo_project/.clj-kondo/` left by an earlier run; removed, not a code change.

### Task 3: Corpus verification

- [x] **Step 1: Compare**
  Run: `bb compare` (clj-kondo corpus)
  Expected: the sites listed in the issue file are gone from `local/plain` and `var-usage/core`, and no bucket gains a new divergence class. `bb check` already ran `compare_simple_project`.

- [x] **Step 2: Gates**
  Run: `bb e2e`, `bb e2e-pulse`, `bb e2e-calva`, then `bb bench clj-kondo`
  Expected: the three e2e gates green (diagnostics, references and rename answers are client-visible, and definition resolution changed for cursors in discards); the bench rows within noise of `docs/MEMORY.md`.

- [x] **Step 3: Record**
  In `docs/MEMORY.md`, under "Compare against clj-kondo analysis", update the `local/plain`, `var-usage/core`, `var-usage/project` and `var-def/defn` rows from the new run with the date, and one line naming this fix as the cause.

- [x] **Step 4: Commit**
  `git commit -am "Record the compare run after the gap fix"`

> Deviation: the whole compare table moved (677 → 450 divergences), not only the four buckets the issue named, because `;` comments inside binding vectors, destructuring maps and def forms were shifting pairs everywhere. A baseline `bb compare` was run on `master` (`ec04537`) the same day, and MEMORY.md now records the full new table with the baseline's agree/diverge/null beside each row instead of updating four rows.
> Note: `bb bench clj-kondo` was cut short by a session restart after the clj-pulse cold row (418 ms first definition, 926 ms library, 18 ms median, 764 ms per edit, 126 MiB RSS — within noise of the 2026-09-10 table); the clj-pulse warm and both clojure-lsp rows did not run. Not re-run: the extractor change is a filter on `named_children` and the cold row is where it would show.

### Task 4: Docs

**Files:**
- Modify: `docs/ROADMAP.md`, `AGENTS.md`; move the issue file.

- [ ] **Step 1: ROADMAP and archive**
  Tick the item, set the Plan line to `— done`, `git mv docs/backlog/2026-09-17-discards-are-indexed.md docs/archive/` and point the item's issue link at the new path. Add to "Where we stand": discards and comments are gaps for the extractor.

- [ ] **Step 2: AGENTS.md**
  Add an invariant after the `are` bullet: `comment` and `dis_expr` are named children in tree-sitter-clojure 0.1 (no grammar extras), so `extractor::named_children` filters them (`is_gap`) and every positional walk counts forms alone; `walk_occurrences` also refuses a gap node handed to it directly. Metadata nodes are not gaps. `collect_alias_usages` walks `all_named_children` on purpose, so an alias rename rewrites a discarded `h/x` too, and `is_destructured_key` steps back over gaps to find the `:keys` directive. `code_action.rs` keeps its own unfiltered helper on purpose: clean-ns preserves comments among specs, and `collect_bare` counts a symbol inside a discard as a use so a require is kept rather than dropped. Any extractor output change bumps `CACHE_FORMAT_VERSION`.

- [ ] **Step 3: README**
  Check the README's feature bullets; nothing there describes extraction details, so no change is expected. Say so in the commit message.

- [ ] **Step 4: Verify and commit**
  Run: `bb check`
  Expected: green.
  `git commit -am "Document discards and comments as gaps"`
