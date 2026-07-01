# Indent-on-Enter (Tier A, Option A) — server-side `onTypeFormatting`

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the user presses Enter inside a Clojure form, indent the new line to the structurally-correct column (Clojure Sublimed / tonsky rule). Implemented in clj-pulse as `textDocument/onTypeFormatting` on `\n`; the VS Code extension just turns on `editor.formatOnType` for Clojure.

**Tech Stack:** Rust, tower-lsp (`on_type_formatting`, confirmed at tower-lsp-0.20 lib.rs:1072), a small hand-written prefix scanner (no tree-sitter on this path). Reuses `Documents::text` / `Documents::text_up_to` (document.rs:133/143). Extension side: one `configurationDefaults` entry.

**Relationship to Plan B-client:** This plan is standalone and shippable, and the server-side indenter stays regardless — it serves other editors (nvim/zed/emacs) and backs future explicit reindent/range formatting. Plan `clojure-pulse-vscode/docs/plans/2026-07-01-indent-on-enter-client-side.md` (Option B-client) was originally a contingency for a too-visible `onTypeFormatting` cursor "settle"; it is now planned regardless, because the maintain-relative-indentation feature (`clojure-pulse-vscode/docs/plans/2026-07-01-maintain-relative-indentation.md`) needs the same client-side scanner — and both client features and this plan must implement the **identical rule and scanner algorithm** so all paths agree.

---

## Design

### Approach

Register `onTypeFormatting` triggered on `\n`. On Enter, compute the new line's indent from the buffer's structure and return one `TextEdit` that sets the leading whitespace. Config-free, no external tools; the indent core is a self-contained prefix scanner using clj-pulse's UTF-16 position conventions.

### The rule (faithful port of Clojure Sublimed `cs_indent.py:indent`)

`indent = (column just after the open delimiter) + offset`, where `offset = 1` **iff** the delimiter is `(` / `#(` **and** the first form inside is a symbol:

- `[] {} #{}` → align to first element (offset 0). `(let [a 1⏎` → under `a`.
- `(` / `#()` with a **symbol head** → 2-space body indent. `(when x⏎` → 2 spaces.
- `(` with a non-symbol head (`((f) …`, `(:k …`) → align to first element.
- inside a string / regex → **no edit** (return nothing). *Deliberate deviation from Sublimed, which aligns the new line under the open quote (`cs_indent.py:59`) — adding alignment spaces changes the string's value, so we leave string content alone.*
- no enclosing bracket → **top level → 0**.

This is uniform 2-space for symbol-headed lists (no argument alignment) — the Sublimed default, deliberately simpler than cljfmt's symbol table (that is Tier B).

### Key decisions

- **Prefix-scan with a hand-written tokenizer, not tree-sitter.** Indentation depends only on the *preceding* context (which brackets are open to the left). Scan `&source[..cursor_byte]` once, maintaining a stack of open delimiters; each frame records the delimiter kind, the UTF-16 column just after it, and whether the first inner form is a symbol. Skip Clojure lexical constructs: `;`→EOL comments, `"…"` / `#"…"` strings (honoring `\"`), `\c` char literals (so `\(` is not an opener); treat `#_` as transparent for bracket balance. Push on `(` `[` `{` `#{` `#(`, pop on `)` `]` `}`. The stack top at the cursor is the innermost open context → apply the rule (string frame → `None`). *Why not tree-sitter:* error recovery on a truncated prefix produces undocumented, version-dependent `ERROR` shapes (an unclosed opener can be dropped or nested arbitrarily) — too brittle for something that runs on every Enter. A ~100-line scanner is deterministic, robust to unbalanced mid-edit code, immune to anything after the cursor, **plays well with Parinfer** (ignores the closers Parinfer manages on the right), and is the *same algorithm* as Plan B-client's TS scanner — rule parity by construction. Sublimed likewise indents off its own parser's explicit unmatched-opener nodes (`cs_indent.py:indent`), not a general grammar.
- **Reuse position machinery, don't extend it:** a small local `position_to_byte(&str, Position)` (same UTF-16 logic as `document.rs::position_to_char`, but over a `&str`) slices the prefix; the scanner tracks columns in UTF-16 code units as it decodes. `Documents::text_up_to` (document.rs:143) already exists as an alternative prefix source. **No `extractor.rs` changes.**
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

- [ ] **Step 1: Write failing unit tests**
  Test `indent_at(source, pos)` (pos is the cursor after the newline, i.e. start of the new line):
  - `(let [a 1\n` → `Some(col of a)` (align, vector).
  - `(when x\n` and `(foo bar\n` → `Some(2-space)` (symbol-headed list).
  - `[a 1\n` / `{:a 1\n` → align; `#{a\n` and `#(foo\n` → align / 2-space (delimiter length handled via the column after the full opener token `#{` / `#(`).
  - non-symbol head `((f)\n` → align.
  - nested `(a (b c\n` → 2-space from the inner `(`.
  - inside string `"ab\n` → `None`.
  - top level (between forms) → `Some(0)`.
  - scanner-skip cases: an opener inside a preceding `;` comment (`; (a\n(foo x\n`), inside a closed string (`"(a" x\n` context), a `\(` char literal, and `#_`-discarded forms counting normally for bracket balance — none of these may corrupt the stack.
  - `position_to_byte` UTF-16 cases (a line with `café` / a `→` before the cursor).

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib indent`
  Expected: FAIL — module/functions not found.

- [ ] **Step 3: Implement the pure core**
  `position_to_byte(source, pos)`: walk to `pos.line`, convert `pos.character` (UTF-16) to a byte offset within the line, clamp to `source.len()`. `indent_at`: slice the prefix `&source[..cursor_byte]` and run the scanner from the Design section: one forward pass, stack of frames `{kind, col_after_opener (UTF-16), first_form_is_symbol}`; skip comments/strings/regexes/char literals; push `(` `[` `{` `#{` `#(`, pop `)` `]` `}` (ignore unmatched closers). At the end: in-string frame → `None`; empty stack → `Some(0)`; else `Some(col_after_opener + offset)` with `offset = 1` iff kind ∈ {`(`, `#(`} and the first inner form is a symbol (first non-whitespace token that is not itself an opener/closer/literal-start, per Sublimed's `is_symbol` check).

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib indent`
  Expected: PASS.

- [ ] **Step 5: Format, lint, commit**
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
  `package.json` has **no** `configurationDefaults` section yet — add one under `contributes`: `"configurationDefaults": { "[clojure]": { "editor.formatOnType": true } }`.

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
