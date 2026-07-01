# Dim Ignored Forms (`#_` / `(comment …)`) Implementation Plan

> **Status: ✅ Implemented 2026-07-01** across `clj-pulse` (branch `feat/dim-ignored-forms`) and `clojure-pulse-vscode` (branch `feat/dim-ignored-forms`). All automated gates green; the on-screen visual check (Task 7 Step 2) is the one maintainer action left. See the [Implementation summary](#implementation-summary) at the end.

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This plan spans **two repos**: the server `clj-pulse` (`/Users/andrew/Projects/clj-pulse`, Rust) and the VS Code extension `clojure-pulse-vscode` (`/Users/andrew/Projects/clojure-pulse-vscode`, TypeScript). Each task states which repo it touches.

**Goal:** Dim `#_` discard forms and `(comment …)` blocks in the clojure-pulse-vscode editor — brackets included — by having the server expose their ranges over a custom LSP request and the extension lay an opacity decoration over them (Calva's mechanism, fed by our server instead of a client-side parser).

**Tech Stack:** Rust + tower-lsp + tree-sitter-clojure (server); TypeScript + vscode-languageclient 9.x + esbuild (extension).

---

## Design

### Background

This supersedes the **Tier-1 semantic-tokens** approach on the (unmerged) `feat/semantic-tokens-tier1` branch. That approach greyed `#_`/`(comment …)` via `textDocument/semanticTokens/full`, but semantic tokens **cannot** override bracket-pair colorization — a separate VS Code render layer driven by the base tokenizer, not semantic tokens — so brackets inside the greyed forms stayed rainbow-colored. Calva solves the same problem with a **`TextEditorDecorationType` at `opacity: 0.5`** (`/Users/andrew/Projects/calva/src/highlight/src/extension.ts:156`): a decoration composites *above* the grammar, semantic tokens, and bracket colorization, dimming the whole range uniformly — brackets included.

Because this is an unmerged branch, no shipped behavior is being reverted — the same feature is being refined. The hard part (the `#_`/`(comment …)` detection with quote-guards, built and tested on the branch) carries over verbatim; only the rendering mechanism changes.

### Approach

The server already locates `#_` and `(comment …)` forms (the Tier-1 walk). Instead of encoding them as semantic tokens, it returns their **ranges** through a custom request `clojurePulse/ignoredForms`. The extension requests those ranges on open/edit (debounced) and applies a single opacity decoration over them.

### Key decisions

- **Custom request `clojurePulse/ignoredForms` → `Range[]`**, not a reuse of `semanticTokens/full`. A dedicated request returns exactly the wanted ranges as whole-form (multi-line) `Range`s — no flat-token-stream decoding, no legend coupling, no client-side `;` filtering. Mirrors the existing `clojure/dependencyContents` custom method (`src/main.rs:66`).
- **Retire the Tier-1 semantic-tokens capability.** Remove `semanticTokensProvider` and the `semantic_tokens_full` handler/trait method; drop the legend, `encode`, `AbsToken`, and per-line splitting. Keep and repurpose the walk + `is_comment_form`/`is_quote_form`/quote-tracking to emit ranges. Semantic tokens return in **Tier 2** with a real legend (macros/unused/locals) — the right use of the capability. *Rationale:* keeping the server emitting `comment` tokens **and** decorating would double-treat (grey + dim) for anyone whose theme has semantic highlighting on. One mechanism owns the visual.
- **Scope = `#_` + `(comment …)` only, not `;` line comments.** The grammar already colors `;` comments and suppresses their brackets, so decorating them adds nothing and would over-dim already-grey text. `#_` dims even inside quoted data (it is a reader-level discard); `(comment …)` dims only when *not* inside quoted data (the existing quote-guard, preserved).
- **One opacity decoration** — `textDecoration: 'none; opacity: 0.5'` (Calva's default), `rangeBehavior: ClosedClosed` — for both form kinds, gated by a single setting `clojurePulse.dimIgnoredForms` (default `true`). Configurable opacity / per-form styles are deferred (YAGNI).
- **Refresh** on active-editor change + `didOpen` + debounced (~250 ms) `didChange`, and once when the client reaches `Running`; only for `clojure` documents; skipped (caught) when no client is running.

### Calva coexistence

The maintainer also runs clj-pulse behind Calva (`calva.clojureLspPath`). This design is safe and beneficial there:

- The custom method is **inert unless called** — Calva never sends `clojurePulse/ignoredForms`, so it is never invoked. Custom methods cannot affect other clients.
- **Retiring the semantic tokens helps the Calva path.** Calva does its own `#_`/`(comment …)` dimming and consumes its LSP's semantic tokens for macro coloring (its `semanticTokenScopes` `macro` mapping). A clj-pulse that emitted only `comment` semantic tokens would overlap Calva's dimming while giving Calva none of the `macro` tokens it expects — retiring them keeps clj-pulse out of Calva's lane.
- **No coverage gap:** under Calva → Calva's own dimming; under clojure-pulse-vscode → our decoration. Both dim; neither conflicts.

### Components & structure

**Server (`clj-pulse`):**
- `handlers/semantic_tokens.rs` → renamed `handlers/ignored_forms.rs`: one public fn `ignored_form_ranges(source: &str) -> Vec<Range>` (whole-form ranges; reuses detection + quote-guard).
- `server.rs`: drops the semantic-tokens capability + trait method; adds `IgnoredFormsParams` and inherent `Backend::ignored_forms` reading live text from `self.documents`.
- `main.rs`: registers the custom method.

**Extension (`clojure-pulse-vscode`):**
- `src/ignoredForms.ts` (new): pure `toRanges(raw) → vscode.Range[]` + `createIgnoredFormDecorator(sendRanges)` (opacity decoration, `refresh`, `dispose`), with an injected `sendRanges` for testability — mirrors `src/jarContentProvider.ts`.
- `src/extension.ts`: creates the decorator, wires the debounced listeners, feeds it via `client.sendRequest`, disposes on deactivate.
- `package.json`: adds the `clojurePulse.dimIgnoredForms` setting; removes the `configurationDefaults` semantic-highlighting block.

### Data flow

edit/open → extension debounces → `sendRequest("clojurePulse/ignoredForms", { uri })` → server parses live text, walks, returns `Range[]` → extension `setDecorations(opacityType, ranges)` → forms dim, brackets included.

### Testing

Server: inline `#[cfg(test)]` unit tests on `ignored_form_ranges`; an e2e custom-request round-trip in `tests/test_e2e.rs`. Gates: `bb check`, `bb e2e`. Extension: `npm test` (vscode-test host) on `toRanges` + the decorator's request/error behavior. Final task is manual visual verification (dimming is a rendering effect).

## File Structure

**Server — `/Users/andrew/Projects/clj-pulse`:**
```
src/handlers/ignored_forms.rs   RENAMED from semantic_tokens.rs — ignored_form_ranges + tests
src/handlers/mod.rs             MODIFY — `pub mod ignored_forms;` (was semantic_tokens)
src/server.rs                   MODIFY — remove ST capability + trait method; add IgnoredFormsParams + Backend::ignored_forms
src/main.rs                     MODIFY — .custom_method("clojurePulse/ignoredForms", Backend::ignored_forms)
tests/test_e2e.rs               MODIFY — replace ST helper+test with ignored_forms request+test
README.md                       MODIFY — reword feature (dim ignored/comment forms via client decoration)
docs/ROADMAP.md                 MODIFY — reword; semantic tokens deferred to Tier 2
docs/plans/2026-07-01-semantic-tokens-tier1.md  MODIFY — add a "superseded by" note
```

**Extension — `/Users/andrew/Projects/clojure-pulse-vscode`:**
```
src/ignoredForms.ts             NEW — toRanges (pure) + createIgnoredFormDecorator (opacity decoration)
src/extension.ts                MODIFY — create decorator, wire debounced listeners, dispose
src/test/ignoredForms.test.ts   NEW — toRanges + decorator request/error behavior
package.json                    MODIFY — add clojurePulse.dimIgnoredForms; remove configurationDefaults ST block
```

## Tasks

### Task 1: Server — `ignored_form_ranges` (TDD)

**Repo:** `/Users/andrew/Projects/clj-pulse`
**Files:**
- Rename: `src/handlers/semantic_tokens.rs` → `src/handlers/ignored_forms.rs`
- Modify: `src/handlers/mod.rs`

> **Execution note:** Task 2 Step 1 (removing the `semantic_tokens_provider` capability + `semantic_tokens_full` trait method from `server.rs`) was pulled into this task's commit — renaming the module structurally breaks the binary until those references are gone, so `bb lint` can't pass otherwise.

- [x] **Step 1: Rename the module and strip the semantic-token machinery**
  `git mv src/handlers/semantic_tokens.rs src/handlers/ignored_forms.rs`. In `mod.rs`, change `pub mod semantic_tokens;` to `pub mod ignored_forms;`. In the renamed file, delete `LEGEND_TYPES`, the `TYPE_*` constants except keep none needed, `legend()`, `AbsToken`, `compute_tokens`, `token_type_for`, `push_node`, `encode`, and `semantic_tokens_full`. Keep `is_comment_form`, `is_quote_form`, `head_symbol`, `node_text`, and the quote-tracking logic. Update the module doc comment to describe range collection.

- [x] **Step 2: Rewrite the unit tests for ranges**
  Replace the test module so it asserts `ignored_form_ranges` returns `Vec<Range>`. Cases: `#_ x` → one range over the whole `#_ x`; multi-line `#_ (a\nb)` → one range spanning both lines (start line 0, end line 1); `(comment (+ 1 2))` → one whole-list range; multi-line `(comment\n  :x)` → one range spanning both lines; stacked `#_ #_ 1 2` → one range; `'(comment 1)` and `` `(comment 1) `` and `(quote (comment 1))` → **no** range (quoted data); `(commentary 1)` / `(comment-foo 1)` → no range; a plain `; line comment` → **no** range (excluded); a bare `42` → no range. Assert ranges by `(start.line, start.character, end.line, end.character)` tuples where exact, else by count + line membership.

- [x] **Step 3: Run tests to verify they fail**
  Run: `cargo test --lib ignored_forms`
  Expected: FAIL — `ignored_form_ranges` not defined.

- [x] **Step 4: Implement `ignored_form_ranges`**
  Add `use tower_lsp::lsp_types::Range;`. Implement `pub fn ignored_form_ranges(source: &str) -> Vec<Range>`: create a `Parser`, `set_language(extractor::language())` (empty vec on error), `parse` (empty on `None`), then `walk(root, source, false, &mut out)`. The walk: if `node.kind() == "dis_expr"` push `node_range(node, source)` and return (dims regardless of `quoted`); else if `!quoted && node.kind() == "list_lit" && is_comment_form(node, source)` push `node_range` and return; else compute `quoted = quoted || matches!(node.kind(), "quoting_lit" | "syn_quoting_lit") || (node.kind() == "list_lit" && is_quote_form(node, source))` and recurse over named children. Add `fn node_range(node: Node, source: &str) -> Range` using `extractor::point_to_position` for start (`start_position`/`start_byte`) and end (`end_position`/`end_byte`). Plain `comment` (`;`) nodes are never matched → excluded.

- [x] **Step 5: Run tests to verify they pass**
  Run: `cargo test --lib ignored_forms`
  Expected: PASS.

- [x] **Step 6: Format, lint, commit**
  Run: `bb fmt && bb lint`
  `git commit -m "refactor: replace semantic-token core with ignored_form_ranges"`

### Task 2: Server — custom request wiring

**Repo:** `/Users/andrew/Projects/clj-pulse`
**Files:**
- Modify: `src/server.rs`, `src/main.rs`

- [x] **Step 1: Remove the semantic-tokens capability and trait method** *(done in Task 1's commit — see the execution note there)*
  In `src/server.rs` `ServerCapabilities`, delete the `semantic_tokens_provider: Some(SemanticTokensServerCapabilities::…)` block. Delete the `async fn semantic_tokens_full(&self, …)` method from the `LanguageServer` impl.

- [x] **Step 2: Add the `ignored_forms` handler**
  In `src/server.rs`, add `IgnoredFormsParams { uri: String }` (serde `Deserialize`, `pub(crate)`, mirroring `TextDocumentContentParams`). Add an inherent `impl Backend` method `pub async fn ignored_forms(&self, params: IgnoredFormsParams) -> tower_lsp::jsonrpc::Result<Vec<Range>>`: parse `params.uri` to `Url` (invalid → `Ok(vec![])`), `self.documents.text(&uri)` (None → `Ok(vec![])`), else `Ok(handlers::ignored_forms::ignored_form_ranges(&text))`. Model it on `text_document_content` (`src/server.rs:120`).

- [x] **Step 3: Register the custom method**
  In `src/main.rs`, add `.custom_method("clojurePulse/ignoredForms", Backend::ignored_forms)` to the `LspService::build(...)` chain (before `.finish()`), alongside the existing custom methods (`src/main.rs:60-66`).

- [x] **Step 4: Build and lint**
  Run: `bb build && bb lint`
  Expected: compiles; no clippy warnings.

- [x] **Step 5: Commit**
  `git commit -m "feat: serve clojurePulse/ignoredForms custom request"`

### Task 3: Server — e2e round-trip

**Repo:** `/Users/andrew/Projects/clj-pulse`
**Files:**
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Replace the request helper**
  Replace the `semantic_tokens_full` helper with `fn ignored_forms(&mut self, path: &Path) -> Value` sending `self.request("clojurePulse/ignoredForms", json!({ "uri": format!("file://{}", path.display()) }))`.

- [x] **Step 2: Rewrite the round-trip test**
  Replace `test_e2e_semantic_tokens_full` with `test_e2e_ignored_forms`. Using `setup_project()` / `initialize` / `did_open` on a written fixture `src/tokens.clj` containing a `; line comment`, a `(def n 42)`, a `#_(unused 1)` on a known line, and a multi-line `(comment …)` block: assert the result is a JSON array of ranges; assert a range starts on the `#_` line and one on the `(comment` line; assert **no** range covers the `;` comment line or the `(def n 42)` line. Also assert the `initialize` result no longer advertises `semanticTokensProvider` (it is absent/null).

- [x] **Step 3: Run the e2e suite**
  Run: `bb e2e`
  Expected: PASS.

- [x] **Step 4: Commit**
  `git commit -m "test: e2e for clojurePulse/ignoredForms"`

### Task 4: Server — docs

**Repo:** `/Users/andrew/Projects/clj-pulse`
**Files:**
- Modify: `README.md`, `docs/ROADMAP.md`, `docs/plans/2026-07-01-semantic-tokens-tier1.md`

- [ ] **Step 1: Update README and ROADMAP**
  In `README.md`, replace the "Semantic tokens" feature bullet with one describing that clj-pulse dims `#_` discards and `(comment …)` blocks in the editor by serving their ranges (`clojurePulse/ignoredForms`) for the extension to decorate — brackets included, no theme config. In `docs/ROADMAP.md`, reword the semantic-tokens "Done" entry to this decoration approach and note resolution-based semantic tokens (macros/unused/locals) remain the Tier-2 follow-up. Use /writing-clearly.

- [ ] **Step 2: Mark the Tier-1 semantic-tokens plan superseded**
  In `docs/plans/2026-07-01-semantic-tokens-tier1.md`, add a short note under the status banner: superseded for `#_`/`(comment …)` by `docs/plans/2026-07-01-dim-ignored-forms.md` (semantic tokens couldn't override bracket-pair colorization); the detection logic was reused.

- [ ] **Step 3: Full verification**
  Run: `bb check`
  Expected: fmt clean, clippy `-D warnings` clean, all tests pass.
  Run: `bb e2e`
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -m "docs: record ignored-form dimming approach"`

### Task 5: Extension — decorator module (TDD)

**Repo:** `/Users/andrew/Projects/clojure-pulse-vscode`
**Files:**
- Create: `src/ignoredForms.ts`
- Test: `src/test/ignoredForms.test.ts`

- [x] **Step 1: Write the failing tests**
  In `src/test/ignoredForms.test.ts` (mirror `src/test/jarContentProvider.test.ts`), test `toRanges`: a well-formed `[{start:{line,character},end:{…}}]` payload maps to `vscode.Range[]` with matching positions; `undefined`/`null`/non-array/malformed entries yield `[]` (no throw). Test `createIgnoredFormDecorator`: `refresh(editor)` calls the injected `sendRanges` with the editor's document uri; a rejected `sendRanges` is swallowed (no throw).

- [x] **Step 2: Run tests to verify they fail**
  Run: `npm test` (in the extension repo)
  Expected: FAIL — `./ignoredForms` module not found.

- [x] **Step 3: Implement the module**
  In `src/ignoredForms.ts`: `export type SendRanges = (uri: string) => Thenable<unknown>;`. `export function toRanges(raw: unknown): vscode.Range[]` — defensively map (guard array + numeric fields), returning `[]` on anything malformed. `export function createIgnoredFormDecorator(sendRanges: SendRanges)` returning `{ refresh(editor: vscode.TextEditor): Promise<void>; dispose(): void }`: create the decoration type once via `vscode.window.createTextEditorDecorationType({ textDecoration: 'none; opacity: 0.5', rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed })`; `refresh` calls `sendRanges(editor.document.uri.toString())`, maps with `toRanges`, and `editor.setDecorations(type, ranges)` — wrapped in try/catch so a server error clears rather than throws; `dispose` disposes the type.

- [x] **Step 4: Run tests to verify they pass**
  Run: `npm test`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "feat: ignored-form dimming decorator"`

### Task 6: Extension — activation wiring + config

**Repo:** `/Users/andrew/Projects/clojure-pulse-vscode`
**Files:**
- Modify: `src/extension.ts`, `package.json`

> **Execution note:** codex review of the wiring caught two real bugs, both fixed: (1) the first paint fired on the `Running` state, which can race ahead of the client's initial `didOpen` sync — moved to fire after `start()` resolves; (2) the refresh touched only the active editor — made it refresh all visible editors so split views of the same document update. A P3 (single debounce timer dropping a pending refresh across two docs) was resolved by coalescing the debounced pass into `refreshAllVisible()`.

- [x] **Step 1: Add the setting; remove the semantic-highlighting default**
  In `package.json`: add `clojurePulse.dimIgnoredForms` (boolean, default `true`, description: dim `#_` discard forms and `(comment …)` blocks). Remove the `contributes.configurationDefaults` block added earlier (the `[clojure]` `editor.semanticHighlighting.enabled` one).

- [x] **Step 2: Wire the decorator into activation**
  In `src/extension.ts`: when `clojurePulse.dimIgnoredForms` is enabled, create the decorator with a `sendRanges` closure `(uri) => client ? client.sendRequest("clojurePulse/ignoredForms", { uri }) : Promise.reject(new Error("server not running"))` (mirror the jar-provider closure). Register listeners (all pushed to `context.subscriptions`): `window.onDidChangeActiveTextEditor` → refresh if clojure; `workspace.onDidOpenTextDocument` → refresh active if clojure; `workspace.onDidChangeTextDocument` → debounced (~250 ms) refresh when it is the active clojure document; refresh the active editor once the client reaches `State.Running` (in the existing `onDidChangeState` handler) for first paint. Only act on documents with `languageId === "clojure"`. Dispose the decorator in `deactivate`. Keep helpers small; a refresh should no-op safely when there is no active clojure editor.

- [x] **Step 3: Build, lint, test**
  Run: `npm run compile && npm run lint && npm test`
  Expected: compiles; no eslint errors; tests pass.

- [x] **Step 4: Commit**
  `git commit -m "feat: dim ignored forms via editor decoration"`

### Task 7: Manual verification

**Repo:** both

- [x] **Step 1: Point the extension at the branch server**
  Server built (`cargo build` → `target/debug/clj-pulse`). Pointing VS Code's `clojurePulse.server.path` at that binary + F5 + **Clojure Pulse: Restart Server** is the maintainer's action (needs the real editor).

- [ ] **Step 2: Verify the behavior** *(maintainer visual check — see verification note below)*
  Open a `.clj` file containing `#_(prn "test")`, a multi-line `(comment (let [a 1]))`, a `; line comment`, and a plain `42`. Confirm: `#_` form and `(comment …)` block are dimmed **including their brackets**; the `;` comment and `42` are unaffected; no double-dimming; toggling `clojurePulse.dimIgnoredForms` off removes the dimming (after reload). Confirm quoted `'(comment 1)` is **not** dimmed.

- [x] **Step 3: Record the outcome**
  See the verification note in the summary below.

## Implementation summary

Implemented 2026-07-01. The `#_`/`(comment …)` rendering moved off semantic tokens
(which can't override bracket-pair colorization) onto a server range request +
client opacity decoration, matching Calva's approach but sourcing ranges from the
server instead of a client-side parser.

**Server (`clj-pulse`, branch `feat/dim-ignored-forms`, 4 commits):**
- `handlers/ignored_forms.rs` — `ignored_form_ranges(source) -> Vec<Range>`: the
  reused tree-sitter walk (with `is_comment_form`/`is_quote_form`/quote-guards)
  now returns whole-form ranges for `#_` discards (always) and `(comment …)`
  blocks (only outside quoted data); plain `;` comments excluded. 8 unit tests.
- `server.rs` / `main.rs` — retired the `semanticTokensProvider` capability +
  `semantic_tokens_full`; added `Backend::ignored_forms` reading live text from
  the document store, registered as the custom `clojurePulse/ignoredForms` method.
- `tests/test_e2e.rs` — round-trip asserting the ranges cover the `#_`/`(comment …)`
  lines, exclude `;`/plain code, and that semantic tokens are no longer advertised.
- README / ROADMAP reworded; the Tier-1 semantic-tokens plan marked superseded.

**Extension (`clojure-pulse-vscode`, branch `feat/dim-ignored-forms`, 2 commits):**
- `src/ignoredForms.ts` — pure `toRanges` + `createIgnoredFormDecorator` (opacity
  `0.5`, `ClosedClosed`), injected `sendRanges` for tests. 5 unit tests.
- `src/extension.ts` — creates the decorator (gated on `clojurePulse.dimIgnoredForms`),
  refreshes on active-editor change / didOpen / debounced didChange / first paint;
  disposes on deactivate.
- `package.json` — added the `dimIgnoredForms` setting; removed the earlier
  `configurationDefaults` semantic-highlighting default (no longer needed).

**Codex review fixes (during Task 6):** first paint moved off the `Running` state
(which can race the client's initial `didOpen` sync) to fire after `start()`
resolves; refresh made split-view-aware (all visible editors, not just the active
one); the debounced pass coalesced into `refreshAllVisible()` so a single timer
never drops a pending refresh.

**Verification.** All automated gates pass: `bb check` (242 lib + 79 e2e) and
`bb e2e`; the extension's `npm run compile`/`lint`/`test` (18 tests). A direct
stdio handshake against the built binary confirmed the extension's exact request
(`didOpen` → `clojurePulse/ignoredForms`) returns the two expected ranges — the
`#_` form and the multi-line `(comment …)` block — with the `;` comment and
`(def n 42)` excluded. **Not automatable here:** the on-screen appearance (that
the opacity decoration visibly dims the forms, brackets included) — the maintainer
should confirm this in real VS Code/Calva per Task 7 Step 2.

**Calva coexistence** (as designed): the custom method is inert unless called, so
Calva is unaffected; retiring the semantic tokens keeps clj-pulse from overlapping
Calva's own ignored-form dimming.

**Neither branch is pushed or merged.** Both sit on `feat/dim-ignored-forms` in
their repos, ready for review.
