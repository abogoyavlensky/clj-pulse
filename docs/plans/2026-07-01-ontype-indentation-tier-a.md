# Indent-on-Enter (Tier A, Option A) — server-side `onTypeFormatting`

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the user presses Enter inside a Clojure form, indent the new line to the structurally-correct column (Clojure Sublimed / tonsky rule). Implemented in clj-pulse as `textDocument/onTypeFormatting` on `\n`; the VS Code extension just turns on `editor.formatOnType` for Clojure.

**Tech Stack:** Rust, tower-lsp (`on_type_formatting`, confirmed at tower-lsp-0.20 lib.rs:1072), tree-sitter-clojure. Reuses `index::extractor::language()` and `index::extractor::point_to_position()`. Extension side: one `configurationDefaults` entry.

**Relationship to Plan B-client:** This plan is standalone and shippable. Plan `clojure-pulse-vscode/docs/plans/2026-07-01-indent-on-enter-client-side.md` (Option B-client) is a *follow-on* to try only if the `onTypeFormatting` cursor "settle" is too visible — it owns the Enter key in the extension for a jump-free feel while **keeping this server-side indenter** for other editors and explicit formatting.

---

## Design

### Approach

Register `onTypeFormatting` triggered on `\n`. On Enter, compute the new line's indent from the buffer's structure and return one `TextEdit` that sets the leading whitespace. Config-free, no external tools, reuses clj-pulse's parser and UTF-16 conversion.

### The rule (faithful port of Clojure Sublimed `cs_indent.py:indent`)

`indent = (column just after the open delimiter) + offset`, where `offset = 1` **iff** the delimiter is `(` / `#(` **and** the first form inside is a symbol:

- `[] {} #{}` → align to first element (offset 0). `(let [a 1⏎` → under `a`.
- `(` / `#()` with a **symbol head** → 2-space body indent. `(when x⏎` → 2 spaces.
- `(` with a non-symbol head (`((f) …`, `(:k …`) → align to first element.
- inside a `str_lit` / `regex_lit` → **no edit** (return nothing).
- no enclosing bracket → **top level → 0**.

This is uniform 2-space for symbol-headed lists (no argument alignment) — the Sublimed default, deliberately simpler than cljfmt's symbol table (that is Tier B).

### Key decisions

- **Prefix-parse, not full-document.** Indentation depends only on the *preceding* context (which brackets are open to the left). Parse `&source[..cursor_byte]` and find the innermost **unclosed** bracket/string at the end of the prefix. This is robust to unbalanced/mid-edit code, immune to anything after the cursor, and — importantly — **plays well with Parinfer** (it ignores the closing brackets Parinfer manages on the right). Task 1 must pin how tree-sitter-clojure represents an unclosed form on a prefix (an incomplete `*_lit` with a missing close vs. an `ERROR` node whose first child is the open delimiter) and match on it.
- **Reuse position helpers:** `point_to_position` for the delimiter column; a small local `position_to_byte(&str, Position)` (same UTF-16 logic as `document.rs::position_to_char`, but over a `&str`). **No `extractor.rs` changes.**
- **Pure, testable core:** `indent_at(source: &str, pos: Position) -> Option<u32>` (`None` = don't reindent). The handler wraps it into a `TextEdit` on the new line's leading whitespace, returning nothing when the indent already matches.
- **Trigger `\n` only** in Tier A.
- **Never crash** (house invariant): parse/edge failures return no edits.

### Known caveat (why Plan B-client exists)

`onTypeFormatting` runs *after* the newline is inserted: VS Code first places the cursor with its own auto-indent, then applies our edit — so the cursor can visibly land at one column and hop to the correct one. This is worst for **alignment cases** (e.g. `(let [a 1`), where the editor cannot pre-guess the column, and the LSP round-trip can push the hop across a render frame. It is smaller than Calva's whole-form reformat, but it is the same class of visible settle. If it is too noticeable, switch to Plan B-client (owns Enter → one atomic edit → no hop).

### Components & structure

- `handlers/indent.rs`: `indent_at` (pure core), `position_to_byte` helper, `on_type_formatting(documents, params) -> Result<Option<Vec<TextEdit>>>`.
- `handlers/mod.rs`: register the module.
- `server.rs`: advertise `document_on_type_formatting_provider { first_trigger_character: "\n", more_trigger_character: None }` + a ~15-line `on_type_formatting` trait method delegating to the handler.
- `clojure-pulse-vscode/package.json`: `configurationDefaults` → `"[clojure]": { "editor.formatOnType": true }` (separate repo; final task).

### Testing & verification

- **Unit tests** on `indent_at` and `position_to_byte`.
- **e2e** round-trip in `tests/test_e2e.rs`.
- **Gates:** `bb check` + `bb e2e` (and `bb e2e-nvim` where available). onTypeFormatting is a client-visible protocol change.

## File Structure

```
clj-pulse/
├─ src/handlers/indent.rs   NEW — indent_at (pure) + position_to_byte + on_type_formatting handler
├─ src/handlers/mod.rs      MODIFY — `pub mod indent;`
├─ src/server.rs            MODIFY — capability + on_type_formatting trait method
├─ tests/test_e2e.rs        MODIFY — on_type_formatting helper + round-trip test
├─ docs/ROADMAP.md          MODIFY — record indent-on-type (Tier A)
└─ README.md                MODIFY — mention indent-on-Enter + Parinfer note

clojure-pulse-vscode/
└─ package.json             MODIFY — configurationDefaults: "[clojure]".editor.formatOnType = true
```

## Tasks

### Task 1: Pure indent core (TDD)

**Files:**
- Create: `src/handlers/indent.rs`
- Modify: `src/handlers/mod.rs`

- [ ] **Step 1: Pin the unclosed-form node shape**
  Add a throwaway `#[test]` that parses `"(let [a 1"` (prefix, no closers) and prints the node kinds/structure around the last byte, to confirm whether the open `(`/`[` appear as incomplete `list_lit`/`vec_lit` (missing close) or as `ERROR` nodes. Record the finding as a comment; it drives Step 3's matching. Then remove/replace the throwaway test.

- [ ] **Step 2: Write failing unit tests**
  Test `indent_at(source, pos)` (pos is the cursor after the newline, i.e. start of the new line):
  - `(let [a 1\n` → `Some(col of a)` (align, vector).
  - `(when x\n` and `(foo bar\n` → `Some(2-space)` (symbol-headed list).
  - `[a 1\n` / `{:a 1\n` → align; `#{a\n` and `#(foo\n` → align / 2-space (delimiter length handled via the opener token's end column).
  - non-symbol head `((f)\n` → align.
  - nested `(a (b c\n` → 2-space from the inner `(`.
  - inside string `"ab\n` → `None`.
  - top level (between forms) → `Some(0)`.
  - `position_to_byte` UTF-16 cases (a line with `café` / a `→` before the cursor).

- [ ] **Step 3: Run tests to verify they fail**
  Run: `cargo test --lib indent`
  Expected: FAIL — module/functions not found.

- [ ] **Step 4: Implement the pure core**
  `position_to_byte(source, pos)`: walk to `pos.line`, convert `pos.character` (UTF-16) to a byte offset within the line, clamp to `source.len()`. `indent_at`: slice the prefix `&source[..cursor_byte]`, parse with `extractor::language()`, take the deepest node at the prefix end and walk up (`parent()`) to the nearest enclosing **unclosed** bracket form (`list_lit`/`vec_lit`/`map_lit`/`set_lit`/`anon_fn_lit`/`ns_map_lit`, or the `ERROR`/incomplete shape from Step 1) or string (`str_lit`/`regex_lit`); apply the rule: string → `None`; none → `Some(0)`; else `col_after = point_to_position(opener_token.end_position(), opener_token.end_byte(), prefix).character` and `offset = 1` iff kind ∈ {`list_lit`,`anon_fn_lit`} and the first named child is a `sym_lit`, returning `Some(col_after + offset)`.

- [ ] **Step 5: Run tests to verify they pass**
  Run: `cargo test --lib indent`
  Expected: PASS.

- [ ] **Step 6: Format, lint, commit**
  Run: `bb fmt && bb lint`
  `git commit -m "feat: structural indent computation for indent-on-type"`

### Task 2: `onTypeFormatting` handler + capability

**Files:**
- Modify: `src/handlers/indent.rs`, `src/handlers/mod.rs`, `src/server.rs`

- [ ] **Step 1: Handler entry point**
  Implement `on_type_formatting(documents, params) -> Result<Option<Vec<TextEdit>>>`: only act when `params.ch == "\n"`; get live text via `documents.text(&uri)` (None → `Ok(None)`); call `indent_at(&text, params.text_document_position.position)`; `None` → `Ok(None)`. Otherwise build a `TextEdit` replacing the new line's leading whitespace (`[line,0]..[line, first_non_ws_col]`) with `" ".repeat(indent)`; if that equals the current text, return `Ok(None)`.

- [ ] **Step 2: Register module + advertise capability**
  `pub mod indent;` in `handlers/mod.rs`. In `server.rs` `ServerCapabilities`, set `document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions { first_trigger_character: "\n".into(), more_trigger_character: None })`.

- [ ] **Step 3: Trait method**
  Add `async fn on_type_formatting(&self, params) -> Result<Option<Vec<TextEdit>>>` to the `LanguageServer` impl, delegating to `handlers::indent::on_type_formatting(&self.documents, params)` (≤15 lines, matching `document_symbol`).

- [ ] **Step 4: Build + lint**
  Run: `bb build && bb lint`
  Expected: compiles; no clippy warnings.

- [ ] **Step 5: Commit**
  `git commit -m "feat: serve textDocument/onTypeFormatting for indent-on-Enter"`

### Task 3: End-to-end test

**Files:**
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Helper**
  Add `fn on_type_formatting(&mut self, path, line, character, ch) -> Value` sending `textDocument/onTypeFormatting`, mirroring existing helpers.

- [ ] **Step 2: Round-trip test**
  Open a fixture whose post-Enter state is `(let [a 1\n])` (cursor at line 1, col 0), request `onTypeFormatting` with `ch = "\n"`, assert the returned edit sets the new line's indent to align under `a`; assert `initialize` advertises `documentOnTypeFormattingProvider`.

- [ ] **Step 3: Run e2e**
  Run: `bb e2e`
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -m "test: e2e for indent-on-type"`

### Task 4: Enable it in the VS Code extension

**Files:**
- Modify: `clojure-pulse-vscode/package.json` (separate repo)

- [ ] **Step 1: Add the default**
  In `contributes.configurationDefaults`, add `"editor.formatOnType": true` under the existing `"[clojure]"` block (which already sets `editor.semanticHighlighting.enabled`).

- [ ] **Step 2: Verify**
  Run: `cd clojure-pulse-vscode && npm run compile && npx @vscode/vsce ls | grep package.json` (manifest still valid). Manually: F5, open a `.clj`, press Enter inside `(let [a 1|])`, confirm the new line aligns under `a`.

- [ ] **Step 3: Commit** (in the extension repo)
  `git commit -m "feat: enable formatOnType for Clojure (indent-on-Enter)"`

### Task 5: Docs + final gate

**Files:**
- Modify: `docs/ROADMAP.md`, `README.md`

- [ ] **Step 1: Roadmap**
  Record indent-on-type (Tier A, structural) as done; note Tier B (cljfmt `:indents` table + `.cljfmt.edn`) and whole-document/range formatting as follow-ups.

- [ ] **Step 2: README + Parinfer note**
  Document indent-on-Enter, and add: "Using Parinfer in Paren/Smart mode? Set `editor.formatOnType: false` for Clojure — Parinfer manages indentation there. Indent Mode is complementary (Clojure Pulse indents; Parinfer places brackets)."

- [ ] **Step 3: Full gate**
  Run: `bb check` and `bb e2e` (and `bb e2e-nvim` if available)
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -m "docs: record indent-on-type (Tier A)"`
