# Phase 1: Document Symbols, Workspace Symbols, UTF-16 Positions ✅ COMPLETED

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Outline view, project-wide symbol search, and correct position handling for non-ASCII source lines.

**Tech Stack:** Rust, tower-lsp 0.20, ropey, tree-sitter.

---

## Design

**documentSymbol.** New handler `src/handlers/symbols.rs`. Extract symbols
from the live document text (DocumentStore) when the file is open — the
extractor costs ~1ms per file — and fall back to the index for unopened
files. Return a flat `DocumentSymbolResponse::Nested` list: `name`,
`detail` (params), `kind` mapped from `DefKind` (defn/defmacro → Function,
def/defonce → Variable, defprotocol → Interface, defrecord/deftype →
Class, defmethod → Method, defmulti → Function), `range` = whole form,
`selection_range` = name range. No nesting.

**workspace/symbol.** Same module. Project symbols only
(`SymbolSource::Project`). Case-insensitive ranked matching: exact >
prefix > substring > subsequence, ties broken by name length. Empty query
returns all project symbols. Cap at 128 `SymbolInformation` results with
`container_name` = namespace.

**UTF-16 positions.** Both directions:

- Incoming: helper in `src/document.rs` converts a UTF-16 column to a char
  index by walking line chars summing `len_utf16()`. Apply in `word_at`,
  `text_up_to`, and `apply_changes`.
- Outgoing: tree-sitter `Point.column` is bytes. `node_to_lsp_range` in
  `src/index/extractor.rs` takes the source text and converts byte columns
  to UTF-16 columns for both ends of the range.

**Error handling.** Handlers return `Ok(None)`/empty list on unknown
files; conversion helpers clamp out-of-range columns to line end.

## File Structure

- Create: `src/handlers/symbols.rs` — documentSymbol + workspaceSymbol
  handlers, fuzzy matcher.
- Modify: `src/handlers/mod.rs` — register module.
- Modify: `src/server.rs` — capabilities + trait methods.
- Modify: `src/document.rs` — UTF-16 → char index conversion.
- Modify: `src/index/extractor.rs` — byte → UTF-16 range conversion.
- Modify: `tests/test_e2e.rs` — e2e coverage.
- Modify: `docs/ROADMAP.md` — tick Phase 1 boxes.

## Implementation Steps

### Task 1: UTF-16 position handling

**Files:** Modify `src/document.rs`, `src/index/extractor.rs`; unit tests in both.

- [x] **Step 1: Write failing unit tests**
  `word_at` on a line with an emoji before the symbol (UTF-16 column);
  `apply_changes` inserting after non-ASCII text; extractor `name_range`
  for a def name following non-ASCII chars on the same line.
- [x] **Step 2: Implement conversions**
  `utf16_col_to_char` helper used by `word_at`/`text_up_to`/`apply_changes`;
  `node_to_lsp_range(node, source)` converting byte columns via
  `len_utf16()` sums over the line slice.
- [x] **Step 3: Verify**
  Run: `cargo test`  Expected: all green.

### Task 2: documentSymbol

**Files:** Create `src/handlers/symbols.rs`; modify `src/handlers/mod.rs`, `src/server.rs`; e2e test.

- [x] **Step 1: Write failing e2e test**
  documentSymbol on fixture `core.clj`: expect `VERSION` (Variable), `add`
  and `multiply` (Function) with correct selection ranges.
- [x] **Step 2: Implement handler + capability**
  Live-text extraction with index fallback; DefKind → SymbolKind mapping.
- [x] **Step 3: Verify**
  Run: `cargo test --test test_e2e`  Expected: all green.

### Task 3: workspace/symbol

**Files:** Modify `src/handlers/symbols.rs`, `src/server.rs`; unit + e2e tests.

- [x] **Step 1: Write failing tests**
  Unit: ranking (exact > prefix > substring > subsequence), library
  symbols excluded, cap respected. E2e: query "add" returns `add` and
  `add-and-double` from the fixture, ordered.
- [x] **Step 2: Implement matcher + handler + capability**
- [x] **Step 3: Verify**
  Run: `cargo test`  Expected: all green.

### Task 4: Wrap-up

**Files:** Modify `docs/ROADMAP.md`.

- [x] **Step 1: Tick Phase 1 checkboxes in the roadmap**
- [x] **Step 2: Full verification**
  Run: `bb check && bb e2e-nvim`  Expected: all green.
- [x] **Step 3: Codex review**
  Run review-with-codex on uncommitted changes; apply important findings;
  max 3 rounds.

---

## Completion Summary (2026-06-12)

All three features implemented and verified (`bb check`: 10 suites green,
`bb e2e-nvim` passed).

- `textDocument/documentSymbol`: live-text extraction with index fallback;
  also outlines non-file documents (`jar:` virtual sources) from open text.
- `workspace/symbol`: project-only, ranked exact > prefix > substring >
  subsequence, capped at 128.
- UTF-16: incoming positions converted in `word_at`/`text_up_to`/
  `apply_changes`; outgoing ranges converted from tree-sitter byte columns
  in `node_to_lsp_range`.

Codex review: 2 rounds. Round 1 found documentSymbol erroring on non-file
URIs (fixed + e2e test). Round 2 found the range-unit change required a
JAR cache format bump (`CACHE_FORMAT_VERSION` 3 → 4, fixed). No round 3
needed.
