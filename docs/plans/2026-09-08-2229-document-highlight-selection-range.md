# Document Highlight and Selection Range Implementation Plan — completed

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `textDocument/documentHighlight` (occurrences of the symbol under the cursor in the current buffer, Read/Write/Text) and `textDocument/selectionRange` (expand selection along the parse tree), the two chrome requests Neovim and Calva users notice missing (ROADMAP Milestone 3, part 1 of 2).

**Tech Stack:** Rust, tower-lsp 0.20, tree-sitter-clojure. Tests: handler unit tests, e2e (`tests/test_e2e.rs`), `bb e2e-nvim` and `bb e2e-pulse` (both capabilities are client-visible).

---

## Design

### Shared position helper

The extractor converts tree-sitter points to LSP positions (`point_to_position`, UTF-16 columns) but has no inverse. Add to `src/index/extractor.rs`:

```rust
/// The tree-sitter point for an LSP position: same row, byte column found by
/// walking the line's chars and counting UTF-16 units. Clamps past-the-end
/// columns to the line end. `None` when the line does not exist.
pub fn position_to_point(source: &str, pos: Position) -> Option<tree_sitter::Point>;

/// Named nodes containing `pos`, innermost first, stopping before the root
/// `source` node. Uses `named_descendant_for_point_range` then walks parents.
/// Token-internal nodes are normalized away: `sym_lit` and `kwd_lit` have named
/// children for their namespace and name parts (`sym_ns`/`sym_name`,
/// `kwd_ns`/`kwd_name`), and the descendant lookup can land on one of them, so
/// the path starts at the enclosing literal instead. Callers that want the name
/// part ask for it explicitly (`child_by_field_name("name")`).
pub fn node_path_at<'a>(root: Node<'a>, source: &str, pos: Position) -> Vec<Node<'a>>;

/// The parsed tree for `source`, or `None` when the language fails to load.
pub fn parse_tree(source: &str) -> Option<tree_sitter::Tree>;   // already exists, make it pub
```

Both handlers parse per request, as every handler does today. The Milestone 1 tree-cache plan replaces that in one place; nothing here depends on it.

### documentHighlight

`src/handlers/highlight.rs`, `pub fn document_highlight(index, documents, params) -> Result<Option<Vec<DocumentHighlight>>>`:

1. **Local under the cursor.** `references::local_refs_at` (made `pub(crate)`) returns the declaration and usages: declaration is `DocumentHighlightKind::WRITE`, usages `READ`. This branch is authoritative, as in `references`.
2. **Otherwise** `references::resolve_fqn_at` gives the fqn or nothing. Occurrences come from a live extraction of this buffer only: `file_occurrences_with` for EDN files, `extract_full_with` otherwise (the same split `resolve_fqn_at` uses). Every occurrence with that fqn is `READ`; a non-`Defmethod` symbol in this file with that fqn adds its `name_range` as `WRITE`. Keyword fqns (leading `:`) use `TEXT` for everything, since a keyword has no read/write distinction; the Integrant `init-key` dispatch keyword is both a symbol and an occurrence, so results are de-duplicated by range.
3. Empty result returns `None`.

Ranges are the occurrence `name_range`. For a qualified symbol that is the name part only, the same span rename edits; for a keyword it is the whole token, because keyword occurrences are recorded that way.

### selectionRange

`src/handlers/selection.rs`, `pub fn selection_ranges(documents, params) -> Result<Option<Vec<SelectionRange>>>`:

- Parse the buffer once; for each requested position build one chain.
- Innermost step: when the leaf is a `sym_lit` or `kwd_lit` with a `name` field child and the cursor sits inside that child, the name part alone (so `alias/na|me` expands to `name`, then `alias/name`). A cursor on the qualifier (`al|ias/name`) or on a keyword's `::` marker starts at the whole token; there is no namespace-only step. Then each node from `node_path_at`, leaf to top-level form. Consecutive steps with identical ranges are collapsed, so a `meta_lit` wrapping a form with the same span never produces a no-op expansion.
- The chain is returned as nested `SelectionRange { range, parent }`, innermost outermost. The whole-document step is left to the client.
- A position with no containing named node (whitespace between top-level forms) yields a single zero-width range at that position, because the response must have one entry per requested position.

### Capabilities

`document_highlight_provider: Some(OneOf::Left(true))`, `selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true))`.

### Testing

- Extractor unit tests for `position_to_point` (ASCII, a line with an emoji before the cursor, past-the-end column) and `node_path_at` (cursor inside a nested vector inside a `defn`; cursor on the `ns` part of `alias/name` yields `sym_lit` first, never `sym_ns`).
- Handler unit tests over snippets via a `DocumentStore` with one open document: highlight kinds for a local, a var defined in the file, a var defined elsewhere (occurrences only), a keyword (whole-token ranges); selection chains for a qualified symbol with the cursor in the name, the same with the cursor on the qualifier, a keyword with the cursor on its `::` marker, a cursor right after a closing paren (the enclosing form comes first), a nested form, and whitespace.
- e2e: `initialize` advertises both; highlight on `helper` in `tests/fixtures/simple_project/src/locals.clj` (or the file that has a local with usages) returns a `WRITE` and `READ`s; highlight on `core/add` in `utils.clj` returns `READ`s only; highlight after an unsaved `didChange` that inserts a line above the usage returns the shifted range (live-buffer requirement); selection range at `core/ad|d` returns `add`, then `core/add`, then the enclosing list; selection at a top-level blank line returns one zero-width range.
- `bb e2e-nvim` (adds a highlight and a selection-range check to `scripts/e2e_nvim.lua`) and `bb e2e-pulse`.

## File Structure

Create:

- `src/handlers/highlight.rs`: `document_highlight`.
- `src/handlers/selection.rs`: `selection_ranges`.

Modify:

- `src/index/extractor.rs`: `position_to_point`, `node_path_at`, `parse_tree` made `pub`, tests.
- `src/handlers/mod.rs`: `pub mod highlight; pub mod selection;`.
- `src/handlers/references.rs`: `local_refs_at` visibility.
- `src/server.rs`: two handlers, two capabilities.
- `scripts/e2e_nvim.lua`: two checks.
- `tests/test_e2e.rs`: `document_highlight` and `selection_range` client helpers, four tests.
- `README.md`, `AGENTS.md`, `docs/ROADMAP.md`.

## Tasks

### Task 1: Position helpers in the extractor

**Files:**
- Modify: `src/index/extractor.rs`

- [x] **Step 1: Write the failing tests**
  In `extractor.rs` `mod tests`: `position_to_point_counts_utf16` (a line `"😀 (foo bar)"`, LSP column after the emoji and space maps to byte column 5), `position_to_point_clamps_past_end`, `node_path_at_lists_innermost_first` (in `(defn f [x] (let [y 1] y))`, the cursor on the second `y` yields `sym_lit`, `list_lit` (the `let`), `list_lit` (the `defn`)), and `node_path_at_normalizes_token_parts` (cursor on `str` in `(str/join x)` yields `sym_lit` first, then `list_lit`).

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --lib extractor::tests::position_to_point`
  Expected: FAIL (no such function).

- [x] **Step 3: Implement**
  The three functions from the design. `node_path_at` walks `parent()` from `named_descendant_for_point_range(pt, pt)`, collecting named nodes and stopping at the root.

- [x] **Step 4: Run the tests**
  Run: `bb check`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Add LSP position to tree-sitter node helpers"`

> Deviation: `parse_tree` was already `pub` (the tree-cache work landed in
> dec1175), so only `position_to_point` and `node_path_at` were added.


### Task 2: documentHighlight

**Files:**
- Create: `src/handlers/highlight.rs`
- Modify: `src/handlers/mod.rs`, `src/handlers/references.rs`, `src/server.rs`
- Test: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing tests**
  Unit tests in `highlight.rs` for the four cases in the design. e2e: `document_highlight(path, line, character) -> Value` helper; `test_e2e_document_highlight_local` and `test_e2e_document_highlight_var_usages`; `test_e2e_capabilities_advertise_highlight_and_selection` (extend when Task 3 lands).

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e document_highlight`
  Expected: FAIL (method not found).

- [x] **Step 3: Implement**
  The handler, `local_refs_at` visibility, `server.rs` wiring with the same `internal_error` mapping `references` uses, and the capability.

- [x] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Add textDocument/documentHighlight"`

> Deviation: the tree cache landed before this plan ran (dec1175), and
> AGENTS.md now requires handlers to read `DocumentStore::snapshot` and the
> extractor's `_tree` variants. The handler therefore uses
> `file_occurrences_tree` / `extract_full_tree` off the cached tree instead of
> the plan's `file_occurrences_with` / `extract_full_with` per-request parse.
> Same behavior, no extra parse.
>
> Deviation: added a fourth e2e test,
> `test_e2e_document_highlight_uses_the_live_buffer`, covering the design's
> unsaved-`didChange` requirement (listed under Testing but not in Task 2's
> steps).


### Task 3: selectionRange

**Files:**
- Create: `src/handlers/selection.rs`
- Modify: `src/handlers/mod.rs`, `src/server.rs`
- Test: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing tests**
  Unit tests in `selection.rs` for the four chains in the design, asserting the exact sequence of ranges. e2e: `selection_range(path, positions: &[(u32, u32)]) -> Value` helper; `test_e2e_selection_range_qualified_symbol` and `test_e2e_selection_range_blank_line`; extend the capabilities test.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e selection_range`
  Expected: FAIL.

- [x] **Step 3: Implement**
  The handler and wiring; parse once per request.

- [x] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Add textDocument/selectionRange"`

> Deviation: reads the cached tree via `DocumentStore::snapshot` rather than
> parsing per request, for the same reason as Task 2. `node_to_lsp_range` in
> the extractor became `pub(crate)` so the handler can turn a node into a
> range.
>
> Deviation: the "cursor right after a closing paren" case resolves to the
> *enclosing* form (tree-sitter does not count a position at a node's end as
> inside it), so the chain there starts at the outer form, not the one just
> closed. The test asserts that.


### Task 4: Editor gates

**Files:**
- Modify: `scripts/e2e_nvim.lua`

- [x] **Step 1: Extend the Neovim script**
  After the existing completion check: a `textDocument/documentHighlight` request on `core/add` expecting at least one result, and a `textDocument/selectionRange` request expecting a chain whose outermost range spans more lines than the innermost.

- [x] **Step 2: Run the gates**
  Run: `bb e2e-nvim && bb e2e-pulse`
  Expected: both pass. Pulse needs no new check; it proves the changed capabilities still negotiate and existing features work.

- [x] **Step 3: Commit**
  `git commit -m "Cover highlight and selection range in the Neovim gate"`

### Task 5: Docs and roadmap

**Files:**
- Modify: `README.md`, `AGENTS.md`, `docs/ROADMAP.md`

- [x] **Step 1: Update docs**
  README: two feature bullets. AGENTS.md invariants: highlight reuses `local_refs_at` before `resolve_fqn_at` exactly as references does, and both handlers parse the live buffer per request until the tree cache lands. ROADMAP Milestone 3: tick both items, set the `Plan:` status to `done`. Use /writing-clearly.

- [x] **Step 2: Verify and commit**
  Run: `bb check`
  Expected: PASS.
  `git commit -m "Document highlight and selection range"`

---

## Completion summary

**Status: complete.** Branch `feat/document-highlight-selection-range`,
commits `32d7463`, `d04ce11`, `ab25143`, `71e00f2`, `f225ecc`.

Implemented:

- `extractor::position_to_point` and `extractor::node_path_at`, the inverse of
  the existing `point_to_position` plus the innermost-first node path a
  position sits in.
- `handlers::highlight::document_highlight`: locals resolve structurally
  through `references::local_refs_at` (declaration `WRITE`, usages `READ`),
  everything else through `resolve_fqn_at` against one extraction of the open
  buffer. Keyword fqns are `TEXT` throughout; results are sorted in document
  order and de-duplicated by range, definition ahead of a usage sharing its
  span.
- `handlers::selection::selection_ranges`: one chain per requested position,
  built from `node_path_at` with an optional name-part step for a qualified
  `sym_lit`/`kwd_lit`, equal consecutive ranges collapsed, and a zero-width
  range where no named node contains the position.
- Capabilities `documentHighlightProvider` and `selectionRangeProvider`, both
  wired in `src/server.rs` with the `internal_error` mapping the other
  handlers use.

Verification: `bb check` (all 281 tests), `bb e2e` (151), `bb e2e-nvim` (two
new checks), `bb e2e-pulse`, and `bb e2e-calva` all pass. Both features were
also hand-driven through a headless Neovim client against `src/locals.clj`:
highlighting `base` returns kind 3 at the binding and kind 2 at both usages,
and the selection chain expands `base` → binding vector → `let` → `defn`.
Codex reviewed every task commit and found no actionable defects.

### Deviations

1. **Task 1** — `parse_tree` was already `pub`, so only the two new helpers
   were added.
2. **Tasks 2 and 3** — the tree-cache work landed before this plan ran
   (`dec1175`), and AGENTS.md now requires handlers to read
   `DocumentStore::snapshot` and call the extractor's `_tree` variants. Both
   handlers do that instead of parsing per request, as the plan's design
   assumed. `extractor::node_to_lsp_range` became `pub(crate)` so the
   selection handler can turn a node into a range.
3. **Task 2** — added a fourth e2e test,
   `test_e2e_document_highlight_uses_the_live_buffer`, for the design's
   unsaved-`didChange` requirement, which the task steps had omitted.
4. **Task 3** — with the cursor immediately after a closing paren, tree-sitter
   does not count the position as inside the form it closed, so the chain
   starts at the *enclosing* form. The test asserts that behavior, which is
   what the plan's parenthetical called for.

### What the plan could have specified better

It was written before the tree-cache plan landed and asserted "both handlers
parse per request, as every handler does today" as settled fact. A plan that
overlaps a sibling plan should state which one lands first and what changes if
the order flips, rather than pinning an API that a concurrent plan is about to
replace. Two other small gaps: the Task 2 steps dropped an e2e test the design
section listed, and the closing-paren selection case was described ambiguously
enough ("the enclosing form comes first") to admit two opposite expectations.
