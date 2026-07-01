# Semantic Tokens — Tier 1 (Syntactic) Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `textDocument/semanticTokens/full` provider that colors the lexical structure of a buffer from the tree-sitter parse — comments, strings, regexes, numbers, keywords, and crucially `#_` discard forms and `(comment …)` blocks — without any name resolution.

**Tech Stack:** Rust, tower-lsp, tree-sitter-clojure. Reuses `index::extractor`'s parser (`language()`) and UTF-16 position conversion (`point_to_position`), both already `pub(crate)`.

---

## Design

### Approach

clj-pulse advertises a semantic-tokens capability and answers `semantic_tokens/full` by parsing the live document with the existing tree-sitter setup, walking the tree, and emitting one token per lexical node. No `Index`, no resolution — Tier 1 is purely syntactic. Two form-aware cases give the visible payoff a TextMate grammar cannot: `#_` discard forms and `(comment …)` blocks both render as a single grey comment span (including nested and multi-line). The work also stands up the whole pipeline (legend, walk, encoding, capability, tests) so Tier 2 (functions/macros/defs/locals/unused, driven by the index) is an additive change.

The VS Code extension (`clojure-pulse-vscode`) needs **no changes** — `vscode-languageclient` registers a semantic-tokens provider automatically once the server advertises the capability with a legend. Semantic tokens layer over the TextMate grammar, which remains the fallback for untokenized ranges.

### Key decisions

- **Reuse, don't rebuild.** Use `crate::index::extractor::language()` (cached parser language) and `crate::index::extractor::point_to_position()` (tree-sitter byte offset → UTF-16 LSP position) — both already `pub(crate)`. **No changes to `extractor.rs`.** This avoids re-solving the UTF-16 column problem.
- **Legend (token types):** `comment`, `string`, `regexp`, `number`, `keyword`. No modifiers in Tier 1. The legend is defined once and shared by the capability and the encoder (a node's type maps to its index in this list).
- **Node → token mapping** (emit a token and **do not recurse**):
  - `comment` → `comment`
  - `dis_expr` (`#_ form`) → `comment` (whole form; covers stacked `#_ #_` and multi-line)
  - `list_lit` whose head form is the symbol `comment` / `clojure.core/comment` → `comment` (whole list)
  - `str_lit`, `char_lit` → `string`
  - `regex_lit` → `regexp`
  - `num_lit` → `number`
  - `kwd_lit` → `keyword` (whole `:ns/name`, including namespaced/auto-resolved)
- **Overlap rule (correctness crux):** on any tokenized node, emit its token(s) and stop — never descend into it. This guarantees no overlapping tokens (e.g. a `str_lit` inside a `#_` form, or `kwd_ns` inside a `kwd_lit`, is never re-tokenized). Recurse only through non-tokenized container nodes (`source`, `vec_lit`, `map_lit`, `set_lit`, `list_lit` that is not a comment form, `anon_fn_lit`, `read_cond_lit`, quoting/unquoting nodes, `meta_lit`, `tagged_or_ctor_lit`, …). Pre-order traversal yields tokens already sorted by position.
- **`(comment …)` detection is a syntactic heuristic**, not resolution: at a `list_lit`, take the first named child form; match only if it is a `sym_lit` whose name is exactly `comment` and whose namespace is absent or `clojure.core`. Exact match guards against `(commentary …)` / `(comment-foo …)`.
- **Multi-line tokens are split per line** (LSP tokens cannot span lines). Split the node's source text on `\n`; for each segment emit `(line, startChar, utf16_len)` — first segment starts at the node's start column, later segments at column 0. Strip a trailing `\r` from each segment before counting so CRLF files don't color the carriage return.
- **`full` only.** Advertise `full: true`, `range: false`, no delta. Recomputing a file is ~1 ms (same cost as extraction); the client debounces requests.
- **Never crash** (house invariant): on parse failure or a missing document, log a warning and return an empty token set — same contract as `extractor::extract`.

### Deferred to Tier 2 (noted, not built here)

Symbols (function/macro/def/namespace/Java-class classification), locals and unused-binding modifiers, booleans/`nil`, `range`/delta requests, and any extension-side theme defaults.

### Components & structure

One new file plus thin wiring, following the `handlers/` pattern (pure core + a ~15-line server delegate):

- `handlers/semantic_tokens.rs`
  - `LEGEND_TYPES: &[SemanticTokenType]` and `legend() -> SemanticTokensLegend` (shared by capability + encoder).
  - `semantic_tokens_full(documents: &DocumentStore, params) -> Result<Option<SemanticTokensResult>>` — reads live text, calls the pure core, wraps the result. No `Index`.
  - `compute_tokens(source: &str) -> Vec<AbsToken>` — **pure**: parse, walk, collect absolute `(line, start_char, len, type_index)`.
  - `encode(&[AbsToken]) -> Vec<SemanticToken>` — relative delta encoding.
- `server.rs` — advertise `semantic_tokens_provider`; add the `semantic_tokens_full` trait method delegating to the handler.

### Testing & verification

- **Unit tests** (inline `#[cfg(test)]`, per house style) on `compute_tokens` / `encode`.
- **e2e** round-trip in `tests/test_e2e.rs`.
- **Gates:** `bb check` (fmt + clippy `-D warnings` + tests) and `bb e2e`; as a client-visible protocol change, also `bb e2e-nvim` where the environment has headless Neovim.

## File Structure

```
clj-pulse/
├─ src/handlers/semantic_tokens.rs   NEW — legend, pure compute_tokens + encode, handler
├─ src/handlers/mod.rs               MODIFY — `pub mod semantic_tokens;`
├─ src/server.rs                     MODIFY — capability + semantic_tokens_full trait method
├─ tests/test_e2e.rs                 MODIFY — semantic_tokens_full helper + round-trip test
├─ docs/ROADMAP.md                   MODIFY — move semantic tokens out of "out of scope"
└─ README.md                         MODIFY — mention syntax highlighting via semantic tokens
```

## Tasks

### Task 1: Pure core — legend, lexical tokens, `#_`, encoding (TDD)

**Files:**
- Create: `src/handlers/semantic_tokens.rs`
- Modify: `src/handlers/mod.rs`

- [ ] **Step 1: Write failing unit tests**
  In `semantic_tokens.rs` `#[cfg(test)]`, test `compute_tokens` on: a line `comment`; a `str_lit`; a multi-line `str_lit` (asserts per-line split with correct UTF-16 lengths); a `regex_lit`; a `num_lit` (int, float, ratio); a `kwd_lit` (plain and `:ns/name`); a single-line and a multi-line `#_` discard (asserts one `comment` span, and that a `str_lit`/`num_lit` inside it is **not** separately tokenized); stacked `#_ #_ a b`. Add a focused `encode` test: a known `Vec<AbsToken>` → expected flat `u32` deltas (including a same-line delta and a line-advance that resets `start_char`). Include a non-ASCII case (e.g. `"café"` or a comment with `→`) asserting UTF-16 lengths.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib semantic_tokens`
  Expected: FAIL — module/functions not found.

- [ ] **Step 3: Implement the pure core**
  Define `AbsToken { line, start_char, len, type_index }`, `LEGEND_TYPES` (`comment`, `string`, `regexp`, `number`, `keyword`) and `legend()`. Implement `compute_tokens`: create a `Parser`, `set_language(extractor::language())`, `parse` (on `None` tree → return empty). Walk recursively: match tokenized node kinds (`comment`, `dis_expr`, `str_lit`, `char_lit`, `regex_lit`, `num_lit`, `kwd_lit`) → push token(s) via a `push_node` helper and **return without recursing**; otherwise recurse over children. `push_node` computes start/end via `extractor::point_to_position`, then splits the node's source text on `\n` (stripping a trailing `\r`) into per-line `AbsToken`s using `encode_utf16().count()` for lengths. Implement `encode` (sort by `(line, start_char)` defensively, then delta-encode to `[Δline, Δstart, len, type, 0]`).

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib semantic_tokens`
  Expected: PASS.

- [ ] **Step 5: Format, lint, commit**
  Run: `bb fmt && bb lint`
  `git commit -m "feat: syntactic semantic-token computation core"`

### Task 2: `(comment …)` form detection (TDD)

**Files:**
- Modify: `src/handlers/semantic_tokens.rs`

- [ ] **Step 1: Write failing tests**
  Add cases to `compute_tokens` tests: `(comment (+ 1 2) :x)` → a single `comment` span over the whole list, with the inner `num_lit`/`kwd_lit` **not** separately tokenized; a multi-line `(comment …)` block (per-line grey split); `(comment)` (empty) still a comment; `(clojure.core/comment …)` matches; and negative guards `(commentary 1)` and `(comment-foo 1)` do **not** become comments (their inner tokens are emitted normally).

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib semantic_tokens`
  Expected: FAIL — `(comment …)` not yet special-cased.

- [ ] **Step 3: Implement**
  In the walk, when the node is a `list_lit`, first check its head: the first named child form; if it is a `sym_lit` whose `name` field text is exactly `comment` and whose `namespace` field is absent or `clojure.core`, push a `comment` token over the whole `list_lit` (via `push_node`) and return without recursing. Otherwise recurse as a normal container.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib semantic_tokens`
  Expected: PASS.

- [ ] **Step 5: Format, lint, commit**
  Run: `bb fmt && bb lint`
  `git commit -m "feat: render (comment …) blocks as comment tokens"`

### Task 3: Server wiring — capability + handler

**Files:**
- Modify: `src/handlers/semantic_tokens.rs` (add the handler entry point)
- Modify: `src/handlers/mod.rs`, `src/server.rs`

- [ ] **Step 1: Add the handler entry point**
  Implement `semantic_tokens_full(documents: &DocumentStore, params: SemanticTokensParams) -> Result<Option<SemanticTokensResult>>`: resolve the URI, get live text via `documents.text(&uri)` (None → `Ok(None)`), run `compute_tokens` + `encode`, return `Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data }))`. On any internal error, log a warning and return `Ok(None)` — never panic.

- [ ] **Step 2: Register the module and advertise the capability**
  Add `pub mod semantic_tokens;` to `handlers/mod.rs`. In `server.rs` `ServerCapabilities`, set `semantic_tokens_provider` to `SemanticTokensOptions { legend: handlers::semantic_tokens::legend(), full: Some(SemanticTokensFullOptions::Bool(true)), range: Some(false), .. }` wrapped in `SemanticTokensServerCapabilities::SemanticTokensOptions`.

- [ ] **Step 3: Add the trait method**
  Add `async fn semantic_tokens_full(&self, params) -> Result<Option<SemanticTokensResult>>` to the `LanguageServer` impl, delegating to the handler with `&self.documents` (≤15 lines, matching `document_symbol`). Map errors to an internal LSP error like the other handlers.

- [ ] **Step 4: Build and lint**
  Run: `bb build && bb lint`
  Expected: compiles; no clippy warnings.

- [ ] **Step 5: Commit**
  `git commit -m "feat: advertise and serve textDocument/semanticTokens/full"`

### Task 4: End-to-end test

**Files:**
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Add a request helper**
  Add `fn semantic_tokens_full(&mut self, path: &Path) -> Value` sending `textDocument/semanticTokens/full` with the file URI, mirroring the existing `document_symbols` helper.

- [ ] **Step 2: Write the round-trip test**
  Using `setup_project()` / `initialize` / `did_open` on a fixture containing a line comment, a string, a number, a keyword, a `#_` form, and a `(comment …)` block, assert the response `.result.data` is a non-empty array whose length is a multiple of 5, and that decoding the first token matches the first construct's position/type. Assert the `initialize` result advertises `semanticTokensProvider`.

- [ ] **Step 3: Run the e2e suite**
  Run: `bb e2e`
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -m "test: e2e for semantic tokens full request"`

### Task 5: Docs and final gate

**Files:**
- Modify: `docs/ROADMAP.md`, `README.md`

- [ ] **Step 1: Update the roadmap**
  Move "Semantic tokens" out of "Out of scope for now"; record Tier 1 (syntactic: comments, strings, regexes, numbers, keywords, `#_` discard, `(comment …)`) as done, and note Tier 2 (resolution-based symbol classification, locals, unused) as the follow-up.

- [ ] **Step 2: Update the README**
  Under features, note that clj-pulse now emits semantic tokens for syntax highlighting (editors with semantic highlighting on get grey `#_`/`(comment …)` blocks for free).

- [ ] **Step 3: Full verification**
  Run: `bb check`
  Expected: fmt clean, clippy `-D warnings` clean, all tests pass.
  Run: `bb e2e` (and `bb e2e-nvim` if headless Neovim is available)
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -m "docs: record Tier 1 semantic tokens"`
