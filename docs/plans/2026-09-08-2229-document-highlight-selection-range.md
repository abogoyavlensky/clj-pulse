# Document Highlight and Selection Range Implementation Plan

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

- [ ] **Step 1: Write the failing tests**
  In `extractor.rs` `mod tests`: `position_to_point_counts_utf16` (a line `"😀 (foo bar)"`, LSP column after the emoji and space maps to byte column 5), `position_to_point_clamps_past_end`, `node_path_at_lists_innermost_first` (in `(defn f [x] (let [y 1] y))`, the cursor on the second `y` yields `sym_lit`, `list_lit` (the `let`), `list_lit` (the `defn`)), and `node_path_at_normalizes_token_parts` (cursor on `str` in `(str/join x)` yields `sym_lit` first, then `list_lit`).

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --lib extractor::tests::position_to_point`
  Expected: FAIL (no such function).

- [ ] **Step 3: Implement**
  The three functions from the design. `node_path_at` walks `parent()` from `named_descendant_for_point_range(pt, pt)`, collecting named nodes and stopping at the root.

- [ ] **Step 4: Run the tests**
  Run: `bb check`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Add LSP position to tree-sitter node helpers"`

### Task 2: documentHighlight

**Files:**
- Create: `src/handlers/highlight.rs`
- Modify: `src/handlers/mod.rs`, `src/handlers/references.rs`, `src/server.rs`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing tests**
  Unit tests in `highlight.rs` for the four cases in the design. e2e: `document_highlight(path, line, character) -> Value` helper; `test_e2e_document_highlight_local` and `test_e2e_document_highlight_var_usages`; `test_e2e_capabilities_advertise_highlight_and_selection` (extend when Task 3 lands).

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e document_highlight`
  Expected: FAIL (method not found).

- [ ] **Step 3: Implement**
  The handler, `local_refs_at` visibility, `server.rs` wiring with the same `internal_error` mapping `references` uses, and the capability.

- [ ] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Add textDocument/documentHighlight"`

### Task 3: selectionRange

**Files:**
- Create: `src/handlers/selection.rs`
- Modify: `src/handlers/mod.rs`, `src/server.rs`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing tests**
  Unit tests in `selection.rs` for the four chains in the design, asserting the exact sequence of ranges. e2e: `selection_range(path, positions: &[(u32, u32)]) -> Value` helper; `test_e2e_selection_range_qualified_symbol` and `test_e2e_selection_range_blank_line`; extend the capabilities test.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e selection_range`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  The handler and wiring; parse once per request.

- [ ] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Add textDocument/selectionRange"`

### Task 4: Editor gates

**Files:**
- Modify: `scripts/e2e_nvim.lua`

- [ ] **Step 1: Extend the Neovim script**
  After the existing completion check: a `textDocument/documentHighlight` request on `core/add` expecting at least one result, and a `textDocument/selectionRange` request expecting a chain whose outermost range spans more lines than the innermost.

- [ ] **Step 2: Run the gates**
  Run: `bb e2e-nvim && bb e2e-pulse`
  Expected: both pass. Pulse needs no new check; it proves the changed capabilities still negotiate and existing features work.

- [ ] **Step 3: Commit**
  `git commit -m "Cover highlight and selection range in the Neovim gate"`

### Task 5: Docs and roadmap

**Files:**
- Modify: `README.md`, `AGENTS.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Update docs**
  README: two feature bullets. AGENTS.md invariants: highlight reuses `local_refs_at` before `resolve_fqn_at` exactly as references does, and both handlers parse the live buffer per request until the tree cache lands. ROADMAP Milestone 3: tick both items, set the `Plan:` status to `done`. Use /writing-clearly.

- [ ] **Step 2: Verify and commit**
  Run: `bb check`
  Expected: PASS.
  `git commit -m "Document highlight and selection range"`
