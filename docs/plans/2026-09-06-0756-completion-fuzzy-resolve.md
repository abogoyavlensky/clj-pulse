# Completion Core: Fuzzy Matching, Resolve, `/` Trigger Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make completion rank like a modern editor expects: fuzzy candidate matching with tiered ranking, lazy documentation through `completionItem/resolve`, and `/` as a trigger character (ROADMAP Milestone 2, part 1 of 2).

**Tech Stack:** Rust, tower-lsp 0.20. Tests: unit tests in `src/handlers/matching.rs` and `tests/test_completion.rs`, e2e in `tests/test_e2e.rs`, `bb e2e-nvim` and `bb e2e-pulse` for the capability change.

---

## Design

### Today

`handlers/completion.rs` filters every pool with `starts_with` and returns full `documentation` on every item. `CompletionOptions::default()` is advertised (`src/server.rs`, the `completion_provider` line), so no trigger characters and no resolve. `handlers/symbols.rs` already has the four-tier matcher (`match_score`: exact 0, prefix 1, substring 2, subsequence 3) that workspace/symbol uses.

### Shared matcher

Move `match_score` and `is_subsequence` with their tests into a new `src/handlers/matching.rs` (`pub fn match_score(name: &str, query: &str) -> Option<u8>`). `symbols.rs` imports it. No behavior change there.

### Ranking

Every completion pool calls `match_score(name, prefix)` and drops non-matches. Each item gets `sort_text = format!("{tier}-{pool}-{name}")` where `pool` is a single digit: locals 0, current namespace 1, refers and `:refer :all` 2, core and special forms 3, aliases and namespaces 4, Java 5. So an exact match anywhere beats a prefix match anywhere, and within a tier the most local pool wins. Locals keep their existing `0-` prefix semantics by being pool 0.

Guardrails, applied in one helper `fn tier_allowed(tier: u8, prefix: &str, pool: Pool) -> bool`:

- An empty prefix is exempt from every rule below: `match_score` returns tier 3 for everything and the pools keep today's behavior, so completion right after `core/` still lists every var of that namespace. `tier_allowed` returns `true` first thing when the prefix is empty.
- Substring and subsequence (tiers 2 and 3) need a prefix of at least two characters. A one-character prefix stays a prefix search.
- The namespace pool (Pool E) never uses subsequence, and is capped at 50 items after sorting by tier, then name. `str` would otherwise subsequence-match hundreds of library namespaces.
- The Java class pool keeps its existing prefix-only `class_names_with_prefix` lookup and cap; fuzzy over the whole JDK is out of scope.
- Empty-prefix behavior is unchanged (`match_score` returns tier 3 for everything, Pools D and E still skip empty prefixes).

`filter_text` stays unset (defaults to the label). Clients apply their own filtering on top; the server contributes candidates and ranking.

### Resolve

Advertise `resolve_provider: Some(true)`. Initial items keep `label`, `kind`, `detail`, and `sort_text`, but no `documentation`. Items that have documentation to offer carry `data`:

```json
{ "src": "symbol", "fqn": "clojure.string/join" }
{ "src": "core",   "name": "map" }
{ "src": "special", "name": "if", "letgo": false }
{ "src": "native", "name": "count" }
```

`pub fn resolve(index: &Index, item: CompletionItem) -> CompletionItem` in `completion.rs` reads `data`, looks the source up (`index.lookup`, `index.core_symbols`, `builtins::special_forms`, the let-go native table), and fills `documentation` as the current builders do. Unknown or missing `data` returns the item unchanged. `server.rs` wires `completion_resolve` to it; errors never surface, the item is returned as-is.

The existing builders (`symbol_to_completion`, `core_symbol_to_completion`, `special_form_to_completion`, `letgo_native_to_completion`) split into "item" and "documentation" halves so resolve reuses the same rendering.

### Trigger character `/`

`CompletionOptions { trigger_characters: Some(vec!["/".into()]), resolve_provider: Some(true), .. }`. With the cursor right after `alias/`, `word_at` already returns `alias/` (`/` is an identifier character), so the qualified path runs with an empty name prefix. The `:` trigger is added by the keyword-completion plan; without keyword candidates it would only dump every symbol.

### Testing

- `matching.rs` unit tests move with the code.
- `tests/test_completion.rs`: fuzzy tests through `complete_symbols` (substring and subsequence matches appear with the right `sort_text` tiers; one-character prefixes do not subsequence-match; namespace pool capped), and resolve tests through `resolve` (a `symbol` item gains its docstring, a `core` item gains its doc, an item without `data` is returned unchanged).
- `tests/test_e2e.rs`: `initialize` advertises `triggerCharacters: ["/"]` and `resolveProvider: true`; completion on `core/ad` returns `core/add` without `documentation` and with `data`; `completionItem/resolve` on that item returns `documentation`; a subsequence query returns the expected item ranked below a prefix match.
- `bb e2e-nvim` and `bb e2e-pulse`: capability change is client-visible.

## File Structure

Create:

- `src/handlers/matching.rs`: `match_score`, `is_subsequence`, tests.

Modify:

- `src/handlers/mod.rs`: `pub mod matching;`.
- `src/handlers/symbols.rs`: use `matching::match_score`; delete the local copies and their tests.
- `src/handlers/completion.rs`: matcher in every pool, `sort_text`, guardrails, `data`, documentation split, `resolve`.
- `src/server.rs`: `CompletionOptions`, `completion_resolve`.
- `tests/test_completion.rs`, `tests/test_e2e.rs`.
- `README.md`, `AGENTS.md`, `docs/ROADMAP.md`.

## Tasks

### Task 1: Shared matcher

**Files:**
- Create: `src/handlers/matching.rs`
- Modify: `src/handlers/mod.rs`, `src/handlers/symbols.rs`

- [x] **Step 1: Move the matcher**
  Create `matching.rs` with `match_score` and `is_subsequence` (both `pub`), moving the three existing tests. Point `symbols.rs` at it.

- [x] **Step 2: Verify nothing changed**
  Run: `bb check`
  Expected: PASS, including the moved tests under `handlers::matching::tests`.

- [x] **Step 3: Commit**
  `git commit -m "Extract the symbol matcher into handlers::matching"`

### Task 2: Fuzzy completion with tiered ranking

**Files:**
- Modify: `src/handlers/completion.rs`
- Test: `tests/test_completion.rs`

- [x] **Step 1: Write the failing tests**
  In `tests/test_completion.rs`: `test_fuzzy_substring_match` (prefix `dd` in `simple.core` finds `add` with `sort_text` starting `2-`); `test_fuzzy_subsequence_ranks_below_prefix` (a query that prefix-matches one symbol and subsequence-matches another, ordered by `sort_text`; note `ad` prefix-matches `add-and-double`, so use a query like `add` against a prefix match `add-more` and a subsequence match `a-d-d`, adding both to a hand-built index or the fixture); `test_single_char_prefix_is_prefix_only` (`d` does not return `add`); `test_namespace_pool_is_capped` (build an index with 60 namespaces sharing a substring, assert at most 50 namespace items). Check the fixture's symbol names in `tests/fixtures/simple_project/src/` before choosing queries.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_completion fuzzy`
  Expected: FAIL.

- [x] **Step 3: Implement**
  Introduce `enum Pool` with its digit, `tier_allowed`, and a small `fn push(items, item, tier, pool)` helper that sets `sort_text`. Replace each `starts_with` in `complete_symbols` and `local_completions` with `match_score` plus `tier_allowed`. Cap the namespace pool after sorting.

- [x] **Step 4: Run the tests**
  Run: `cargo test --test test_completion && bb e2e`
  Expected: PASS. Existing e2e completion tests assert on labels only, so ranking changes do not break them; if one asserts on order, update it deliberately.

- [x] **Step 5: Commit**
  `git commit -m "Rank completion candidates by match tier and pool"`

### Task 3: Resolve

**Files:**
- Modify: `src/handlers/completion.rs`, `src/server.rs`
- Test: `tests/test_completion.rs`, `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing tests**
  Unit: `test_items_carry_data_not_documentation` (a `symbol` item from `complete_symbols` has `data` and no `documentation`), `test_resolve_fills_symbol_documentation`, `test_resolve_fills_core_documentation`, `test_resolve_fills_special_form_documentation`, `test_resolve_fills_letgo_native_documentation` (a let-go index; see how `test_completion.rs` builds one, or add a minimal case), `test_resolve_passes_unknown_item_through`, and `test_resolve_ignores_malformed_data` (`data` that is a string, or an object missing `src`). The existing `test_completion_item_has_doc_and_detail` in `tests/test_completion.rs` asserts documentation on the initial item; change it to assert `detail` on the item and documentation after `resolve`. e2e: add a `completion_resolve(item: Value) -> Value` helper to `LspClient`; `test_e2e_completion_resolve_adds_documentation` requests completion on `core/ad`, picks `core/add`, asserts no `documentation`, resolves it, asserts `documentation.value` contains the docstring from `core.clj`.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_completion resolve && cargo test --test test_e2e completion_resolve`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  Split the builders, add `data`, write `resolve`, wire `completion_resolve` in `server.rs`, set `resolve_provider: Some(true)`.

- [ ] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Defer completion documentation to completionItem/resolve"`

### Task 4: `/` trigger and client gates

**Files:**
- Modify: `src/server.rs`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing test**
  `test_e2e_completion_capabilities`: `initialize` result has `completionProvider.triggerCharacters == ["/"]` and `resolveProvider == true`. `test_e2e_completion_after_slash_trigger`: completion at the position right after `core/` (empty name prefix) returns `core/add`.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e completion_capabilities`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  Set `trigger_characters` in `CompletionOptions`.

- [ ] **Step 4: Run every gate**
  Run: `bb check && bb e2e && bb e2e-nvim && bb e2e-pulse`
  Expected: PASS. The nvim run proves the changed `completionProvider` shape negotiates; the Pulse run proves VS Code still completes through the extension.

- [ ] **Step 5: Commit**
  `git commit -m "Trigger completion on / and advertise resolve"`

### Task 5: Docs and roadmap

**Files:**
- Modify: `README.md`, `AGENTS.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Update docs**
  README Autocomplete bullet: fuzzy matching and ranking, docs loaded on demand. AGENTS.md invariants: one line that `sort_text` is `tier-pool-name` and every pool goes through `handlers::matching`. ROADMAP Milestone 2: tick the trigger-`/` half of the trigger item (leave `:` for the next plan), fuzzy matching, and resolve. Use /writing-clearly.

- [ ] **Step 2: Verify and commit**
  Run: `bb check`
  Expected: PASS.
  `git commit -m "Document fuzzy completion and resolve"`
