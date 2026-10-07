# Cursor on a Reader Prefix Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A cursor on the `@` of `@state`, the `#'` of `#'state`, or the `` ` ``/`~`/`~@` before a symbol answers hover, definition, references, rename, prepareRename and documentHighlight exactly as a cursor on the symbol does, while every reported range stays on the symbol.

**Tech Stack:** Rust, tower-lsp, tree-sitter-clojure 0.1 (`derefing_lit`, `var_quoting_lit`, `syn_quoting_lit`, `unquoting_lit`, `unquote_splicing_lit` nodes, each with a `marker` and a `value` field).

---

## Design

### The problem

Every position-based handler decides what the cursor is on in one of two ways: `DocumentStore::word_at` (text scan over `is_clj_ident_char`, used by hover, definition's bare-word fallback, `local_name_at`, `resolve_fqn_at`'s alias fallback, and `clojure_docs_var_at` in `src/server.rs`), or a range match of the position against occurrence and definition `name_range`s (`resolve_fqn_at`, `src/handlers/references.rs:826`). Both span the symbol only. `@` and `#` are not identifier characters, and the walker records the `sym_lit` inside a `derefing_lit`, not the prefix. So on the `@` of `@pfx-state`:

| request | today |
|---|---|
| hover, definition, references, rename, prepareRename, documentHighlight | `null` / "nothing to rename here" |

Verified against the debug binary built from `master` at a822f79, on `(def state (atom 0)) (defn read-it [] @state)` and on `#'state`. A cursor on `pfx-state` already works. Keywords already have the behavior we want: a cursor on the `:`/`::` marker resolves because the keyword occurrence's `name_range` spans the whole token (comment at `references.rs:836`).

### The fix: normalize the position once, at each handler's entry

One helper, `extractor::prefixed_symbol_start`, answers the tree-level question:

```rust
/// When `pos` sits on the reader prefix of a symbol — the `@` of `@x`, the
/// `#'` of `#'x`, the `` ` `` of `` `x ``, the `~`/`~@` of `~x`/`~@x` — the
/// start of that symbol, so the cursor resolves what it visibly points at.
/// `None` when the innermost node at `pos` is not such a prefix node, when
/// its value is not a `sym_lit`, or when `pos` is already inside the value.
pub fn prefixed_symbol_start(tree: &tree_sitter::Tree, source: &str, pos: Position) -> Option<Position>
```

Implementation: `node_path_at(root, source, pos)` first element; kind in `derefing_lit | var_quoting_lit | syn_quoting_lit | unquoting_lit | unquote_splicing_lit`; `child_by_field_name("value")` is a `sym_lit`; return the start of the symbol's **`name` field when it has one** (`@h/state` → the `s` of `state`), else of the `sym_lit`. The name half matters: `rename_target` tries the alias before the var, and a position on the `h` of `h/state` renames the alias `h` and its `:as` declaration. A deref or var-quote is an operation on the var, so the cursor on its prefix must land on the var's half. The value start is the position every downstream path accepts (range match is start-inclusive, `word_at` scans right from it). `quoting_lit` (`'x`) is deliberately excluded: a quoted symbol records no occurrence (CLAUDE.md, "a symbol records nothing" under quoted data), so nothing would resolve from either character and the two positions stay consistent. Node kinds with a non-symbol value (`@(f)`, `` `(f x) ``) return `None`, as do stacked prefixes whose immediate value is another prefix node (`@#'x` from the `@` — YAGNI; the `#'` still works).

A `DocumentStore`-level wrapper keeps the handlers one line each:

```rust
/// `pos`, moved onto the symbol when it sits on that symbol's reader prefix
/// (`extractor::prefixed_symbol_start`); `pos` unchanged otherwise, and for
/// an unopened document.
pub fn symbol_position(&self, uri: &Url, pos: Position) -> Position
```

in `src/document.rs`, reading `snapshot(uri)` — the cached tree, never a fresh parse (CLAUDE.md document-store invariant).

Each handler normalizes `pos` as its first act, before any branch (locals first, then alias, then fqn — none of those should see the raw position):

- `handlers::hover::hover`
- `handlers::definition::goto_definition` (before `local_definition`)
- `handlers::references::references`
- `handlers::references::rename` and `handlers::references::prepare_rename` — in the two public entries, not inside the shared `rename_target`: `prepare_rename` matches `pos` against site ranges *after* `rename_target` returns (`references.rs:566-590`), so the position it keeps must already be the normalized one.
- `handlers::highlight::document_highlight`
- `Backend::clojure_docs_var_at` in `src/server.rs`

Why at the entry and not inside `word_at` / `resolve_fqn_at`: the three token finders (`word_at`, `is_keyword_at`, the range match) would each need the same tree lookup, and `local_name_at` calls two of them. One normalization per request is one tree lookup per request and no finder learns about prefixes. Completion, code actions and signature help are left alone: a cursor on `@` is not typing a name, and signature help finds its call by brackets.

### `word_at` must not spell the prefix

Moving the cursor is not enough for the word-based handlers: `is_clj_ident_char` (`src/document.rs:393`) counts `#` and `'` as symbol characters (for `x#` gensyms and `x'`), so `word_at` on the `s` of `#'state` scans back over the marker and answers `#'state`. That is why hover on `#'state` fails *today even from the symbol* (verified: hover at the `s` → `null`, while definition, which goes through the range match, works). `clojuredocs::resolve_var` already works around it by stripping `#'` and `'` (`src/handlers/clojuredocs.rs:65`).

Fix it at the source: `word_at` strips a leading `#'` or `'` from the span it found. No Clojure symbol starts with `#` or `'`, so the strip can never eat part of a name, and `x#`/`x'` keep their trailing characters. One place, and every `word_at` caller (hover, `local_name_at`, the alias fallback of `resolve_fqn_at`, `clojure_docs_var_at`, the code-action token, the completion prefix) stops seeing a reader marker as part of a name. The `clojuredocs.rs` strip becomes redundant; leave it, it is harmless and has its own tests. Completion typing `#'ma` now completes `ma`, which is what the user is doing; items with a `text_edit` are unaffected and the rest replace the client's own word, which never included `#'`.

The other prefixes (`@`, `` ` ``, `~`, `~@`) are not identifier characters, so `word_at` from the symbol already excludes them; from the prefix character itself `word_at` finds nothing, which is what the position normalization fixes.

### Ranges stay on the symbol

`prefixed_symbol_start` moves the *question*; every *answer* still comes from occurrence and definition ranges, which span the symbol. So highlight underlines `pfx-state` not `@pfx-state`, references list the symbol range, and a rename of `pfx-state` to `pfx-counter` yields `@pfx-counter`. The deref is an operation on the var, not part of its name. This is the decision discussed and taken before planning.

### Locals

`local_refs_at` and `local_definition` both go through `local_name_at`, which uses `word_at`, so `(let [a (atom 1)] @a)` from the `@` resolves the local once the position is normalized at the handler entry. No extractor change beyond the helper.

### Testing

Unit tests for the helper in `src/index/extractor.rs` (`#[cfg(test)] mod` beside `node_path_at`'s tests): each prefix kind, cursor on each character of a two-character prefix, non-symbol value → `None`, position already inside the symbol → `None`, `'x` → `None`. e2e tests in `tests/test_e2e.rs` driving the real binary through every handler from the prefix, plus `test_extractor.rs` not needed (the walker is unchanged). No `CACHE_FORMAT_VERSION` bump: extractor *output* is unchanged — the helper is a read-only tree query.

## File Structure

- Modify `src/index/extractor.rs` — add `prefixed_symbol_start` next to `node_path_at` (the "token parts" normalizer it extends in spirit) and its unit tests.
- Modify `src/document.rs` — add `DocumentStore::symbol_position`.
- Modify `src/handlers/hover.rs`, `src/handlers/definition.rs`, `src/handlers/references.rs`, `src/handlers/highlight.rs`, `src/server.rs` — one normalization line at each entry.
- Modify `tests/fixtures/simple_project/src/prefixes.clj` (new fixture file) — a small file with a `def` of an atom, `@`, `#'`, `` ` ``, `~` sites and a local deref. Check first that nothing in `test_e2e.rs` counts fixture files or workspace symbols in a way a new file breaks (`grep -n "simple_project" tests/test_e2e.rs | grep -i "count\|len()"`); if it does, append the forms to an existing fixture instead and adjust the needles.
- Modify `tests/test_e2e.rs` — the e2e tests.
- Modify `docs/FEATURES.md`, `docs/ROADMAP.md`, `CLAUDE.md`.

## Tasks

### Task 0: Roadmap link

- [ ] **Step 1:** In `docs/ROADMAP.md`, under "Milestone 1 — correctness of shipped features", add the item unticked with its `Plan:` line pointing at this file (working rule: link the plan when starting). Task 4 ticks it. The working rule says to update README and CLAUDE.md on completion: CLAUDE.md gets its invariant in Task 4; README is checked in Task 4 and left unchanged if, as expected, nothing in it enumerates cursor positions — it is the short public introduction, and `docs/FEATURES.md` owns this detail. `AGENTS.md` is a symlink to `CLAUDE.md`, so it needs no separate edit.

### Task 1: `extractor::prefixed_symbol_start` with unit tests

**Files:**
- Modify: `src/index/extractor.rs`

- [ ] **Step 1: Write the failing unit tests**
  In the existing `#[cfg(test)] mod tests` of `src/index/extractor.rs`, beside `node_path_at_normalizes_token_parts`. Parse with `parse_tree` and call `prefixed_symbol_start(&tree, src, Position { line, character })`. Cases:
  - `(f @state)` at the `@` → `Some` start of `state`; at the `s` → `None`.
  - `(f #'state)` at `#` and at `'` → both `Some` start of `state`.
  - `` (f `state) `` at the backquote → `Some`.
  - `(f ~state ~@xs)` at `~`, and at both characters of `~@` → `Some` the respective symbol start.
  - `(f @(deref x))` at `@` → `None` (value is a list).
  - `(f 'state)` at `'` → `None`.
  - `(f @h/state)` at `@`, and `(f #'h/state)` at `#` → `Some` the start of `state` (the `name` field), never of `h`.
  Run: `cargo test --lib prefixed_symbol_start` — Expected: compile error (function missing).

- [ ] **Step 2: Implement**
  As in the Design: `node_path_at` first element, kind match on the five prefix kinds, `child_by_field_name("value")` must be `sym_lit`, return `node_to_lsp_range(value, source).start`. Return `None` when `pos` is at or past the value's start (the position is already inside the symbol; let the caller's finders work unmodified). Doc comment as in the Design.
  Run: `cargo test --lib prefixed_symbol_start` — Expected: PASS.

- [ ] **Step 3: Commit**
  `git commit -am "extractor: locate the symbol a reader prefix points at"`

### Task 2: `DocumentStore::symbol_position`, and `word_at` without reader markers

**Files:**
- Modify: `src/document.rs`

- [ ] **Step 0: `word_at` strips `#'` and `'`**
  Add a failing unit test in `document.rs`'s `mod tests` first: `word_at` on the `s` of `(f #'state)` → `"state"`, on `(f 'state)` → `"state"`, on `(f x#)` → `"x#"`, on `(f x')` → `"x'"`. Run `cargo test --lib word_at` — Expected: FAIL on the first two. Then, in `word_at`, after `start`/`end` are found: advance `start` past a leading `#'` pair or a single leading `'` (`chars[start..]` starts with `['#','\'']` or `['\'']`); return `None` if that empties the span. Doc comment: no symbol starts with `#` or `'`, so the characters are reader markers, not the name. Run again — Expected: PASS.

- [ ] **Step 1: Implement**
  `pub fn symbol_position(&self, uri: &Url, pos: Position) -> Position`: `self.snapshot(uri)` → `extractor::prefixed_symbol_start(&snap.tree, &snap.text, pos).unwrap_or(pos)`; `pos` when the document is not open. Doc comment as in the Design. Check how `document.rs` already names the extractor module in its imports and follow it.

- [ ] **Step 2: Build**
  Run: `cargo build` — Expected: clean (the function is unused until Task 3; add `#[allow(dead_code)]` only if clippy in `bb check` would fail between commits — it does not for `pub` items).

- [ ] **Step 3: Commit**
  `git commit -am "document: a cursor on a reader prefix asks about the symbol, and word_at drops #' and '"`

### Task 3: Normalize at every handler entry, with e2e tests

**Files:**
- Create: `tests/fixtures/simple_project/src/prefixes.clj`
- Modify: `tests/test_e2e.rs`, `src/handlers/hover.rs`, `src/handlers/definition.rs`, `src/handlers/references.rs`, `src/handlers/highlight.rs`, `src/server.rs`

- [ ] **Step 1: Fixture**
  Run the grep from File Structure first. Then write `tests/fixtures/simple_project/src/prefixes.clj`:
  ```clojure
  (ns simple.prefixes)

  (def pfx-state (atom 0))
  (defn read-it [] @pfx-state)
  (defn var-of [] #'pfx-state)
  (defmacro m [] `(deref ~pfx-state))
  (defn local-deref [] (let [pfx-a (atom 1)] @pfx-a))
  ```
  For the qualified case, append `(def pfx-var-via-alias #'c/blend)` to `src/alias_sites.clj`, which already requires `simple.core :as c`. Append only; its existing needles are asserted by the alias-rename tests. Check `test_e2e_rename_alias_skips_literal_keyword_and_destructuring_entry` afterwards: it lists the lines an alias rename of `c` edits, and the new `#'c/blend` line is a legitimate new alias site, so add it to that test's expected lines.
  Names carry a `pfx-` prefix so no other fixture's `state` or `a` shows up in a `workspace/symbol` or hover assertion elsewhere; the needles (`@pfx-state`, `#'pfx-state`, `~pfx-state`, `@pfx-a`) are unique to this file.

- [ ] **Step 2: Write the failing e2e tests**
  In `tests/test_e2e.rs`, following `test_e2e_rename_alias_from_its_binding`'s shape (`setup_project`, `LspClient::start`, `initialize`, `wait_for_log("Indexed")`, `did_open`, `start_of(&text, needle)`). One test per handler, each asking from the prefix character and asserting the same answer a cursor on the symbol gives:
  - `test_e2e_prefix_cursor_hover`: `hover` at `@` of `@pfx-state` → contents mention `simple.prefixes/pfx-state`; also at `#` and at `'` of `#'pfx-state`.
  - `test_e2e_prefix_cursor_definition`: `definition` at `@` → the `pfx-state` range on the `def` line, character range of `pfx-state` only.
  - `test_e2e_prefix_cursor_references`: `references` at `#` of `#'pfx-state` (includeDeclaration true) → the same set as from `pfx-state` on the `def` line; every range's width equals `"pfx-state".len()`, so no range swallows a prefix.
  - `test_e2e_prefix_cursor_highlight`: `document_highlight` at `@` → equal to the answer at the `s`; the `def` entry kind 3 (WRITE), the others 2 (READ); widths all 9.
  - `test_e2e_prefix_cursor_rename`: `rename` at `@` to `pfx-counter`; apply with `apply_edits`; the result contains `@pfx-counter`, `#'pfx-counter`, `~pfx-counter`, `(def pfx-counter`, and no `pfx-state`; `prepare_rename` at `@` returns the range of `pfx-state` (start character = `@` column + 1, width 9).
  - `test_e2e_prefix_cursor_qualified_targets_the_var`: in `src/alias_sites.clj`, `rename` at the `#` of `#'c/blend` to `pfx-mix` → the edits touch `blend` sites (the `def` in `src/core.clj` or wherever `simple.core/blend` is defined, and every `c/blend` usage) and never the `:as c]` declaration; `prepare_rename` from the same `#` returns the range of `blend`, start character = column of `#` + 4. This pins the `name`-field rule.
  - `test_e2e_prefix_cursor_rename` also asks `references` from the backquote of `` `(deref ~pfx-state) `` → `null` is acceptable there only if the walker records nothing under syntax-quote; check `grep -n syn_quoting_lit src/index/extractor.rs` — the earlier grep found no special casing, so it is walked as ordinary code and the `~pfx-state` site must be in the answer. Ask from the `~` of `~pfx-state` and assert the set equals the one from the `s`.
  - `test_e2e_prefix_cursor_local_deref`: `references` at the `@` of `@pfx-a` → two ranges, the binding `pfx-a` and the usage `pfx-a`, both width 5; `definition` from the same `@` → the binding.
  - `test_e2e_prefix_cursor_hover` also asserts hover from the `s` of `#'pfx-state` (the pre-existing bug the `word_at` strip fixes) and from the `@` of `@pfx-state`.
  Run: `cargo test --test test_e2e prefix_cursor` — Expected: FAIL (null answers / "nothing to rename here").

- [ ] **Step 3: Normalize in the handlers**
  First statement that uses `pos` in each: `let pos = documents.symbol_position(&uri, pos);`
  - `src/handlers/hover.rs` `hover`, before `word_at`.
  - `src/handlers/definition.rs` `goto_definition`, before `local_definition`.
  - `src/handlers/references.rs` `references`, before `local_references`; `rename` (line ~659) and `prepare_rename` (line ~564), each before its `rename_target` call — `prepare_rename` reuses `pos` for the `range_contains` matches after the call, so the shadowed `pos` carries through. Do not normalize inside `rename_target` as well: one tree lookup per request.
  - `src/handlers/highlight.rs` `document_highlight`, before `local_highlights`.
  - `src/server.rs` `clojure_docs_var_at`, before `word_at`.
  Each site gets a one-line comment: the cursor on a reader prefix asks about the symbol it points at.
  Run: `cargo test --test test_e2e prefix_cursor` — Expected: PASS.

- [ ] **Step 4: Full gate**
  Run: `bb check` — Expected: green. Then `bb e2e` — Expected: green.

- [ ] **Step 5: Commit**
  `git add tests/fixtures/simple_project/src/prefixes.clj && git commit -am "handlers: a cursor on @, #', \` or ~ resolves the symbol it prefixes"`

### Task 4: Docs

**Files:**
- Modify: `docs/FEATURES.md`, `docs/ROADMAP.md`, `CLAUDE.md`

- [ ] **Step 1: FEATURES.md**
  In the "Highlight occurrences" bullet (and once, in the navigation/hover area above it if it lists what the cursor may sit on), add one sentence: the cursor may sit on a symbol's reader prefix — `@pfx-state`, `#'pfx-state`, `` `state ``, `~pfx-state` — and the answer is the symbol's; the underline and every edit stay on the name, so renaming `pfx-state` leaves the `@`.

- [ ] **Step 2: ROADMAP.md**
  Tick the item Task 0 added; final text:
  `- [x] **Cursor on a reader prefix.** Hover, definition, references, rename and highlight from the `@`, `#'`, `` ` `` or `~` before a symbol answer as from the symbol; ranges stay on the name.` with `Plan: [2026-10-07-2258-cursor-on-reader-prefix.md](plans/2026-10-07-2258-cursor-on-reader-prefix.md) — done`.

- [ ] **Step 3: CLAUDE.md**
  Under Invariants, after the `documentHighlight` bullet, add: every position-based handler (hover, definition, references, rename, prepareRename, highlight, the ClojureDocs lookup) first moves the cursor through `DocumentStore::symbol_position`, so a cursor on `@`, `#'`, `` ` ``, `~` or `~@` asks about the symbol they prefix (`extractor::prefixed_symbol_start`); `'x` is left alone because a quoted symbol records nothing; completion, code actions and signature help do not normalize. Answers always come from occurrence and definition ranges, so no range ever includes a prefix.

- [ ] **Step 4: Verify and commit**
  Run: `bb check` — Expected: green.
  `git commit -am "docs: cursor on a reader prefix"`

### Task 5: Gates

Per the CLAUDE.md gate table: `bb e2e` ran in Task 3. This change touches definition (`bb e2e-calva`), references and rename (`bb compare`), and is client-visible (`bb e2e-pulse`). `bb bench` and `bb soak` are not owed: extractor *output* is unchanged (no `CACHE_FORMAT_VERSION` bump, the helper is a read-only tree query), and the document store gains a read method without any change to how documents are opened, edited or snapshotted.

- [ ] **Step 1:** `bb compare clj-kondo` — Expected: zero *new* divergences versus the tables in `docs/MEMORY.md` (`compare_simple_project` in `bb check` already covers the fixture).
- [ ] **Step 2:** `bb e2e-calva` — Expected: green.
- [ ] **Step 3:** `bb e2e-pulse` — Expected: green.
  If the host lacks Xvfb or VS Code for either, say so in the final report rather than skipping silently.
