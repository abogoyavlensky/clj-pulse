# Add-Missing-Require Code Action Implementation Plan

> **Status: COMPLETED (2026-06-13).** See the summary at the end.

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Offer a `quickfix` code action that adds a missing `:require` clause when the cursor sits on an alias- or namespace-qualified symbol whose prefix is not yet required.

**Tech Stack:** Rust, tower-lsp, tree-sitter (tree-sitter-clojure), existing namespace/symbol index.

---

## Design

### Trigger and detection

A new `textDocument/codeAction` handler resolves the qualified symbol at the
request range. We reuse `DocumentStore::word_at`, which already returns a whole
`prefix/name` token (`/` and `.` are identifier chars), so `str/join` and
`clojure.set/union` come back intact.

The handler works against the **live buffer**, not the saved index: the file's
`:require` clause may have changed since the last save. We re-extract the
current buffer's `NsMeta` with `extractor::extract` (parsing only the `ns`
form is enough) and use that for "is the prefix already required?" checks.

A token is a candidate for the action only when:

1. It contains a `/` splitting into `prefix` / `name`.
2. `prefix` is **not** already resolvable: not a key in the live
   `NsMeta.aliases`, not already a required namespace, and not `clojure.core`.

### Candidate namespaces

For a missing `prefix`, gather candidate namespaces from three sources, then
filter and rank:

- **Fully-qualified namespace** — if `prefix` itself is a known namespace name
  in `Index.namespaces` (the `clojure.set/union` case), the candidate is
  `prefix` required plainly, no alias: `[clojure.set]`.
- **Last-segment match** — every key of `Index.namespaces` whose final
  dot-segment equals `prefix` (`set` → `clojure.set`, `helpers` →
  `simple.helpers`). Suggested with `:as prefix`.
- **Curated table** — a small static `&[(&str, &str)]` for idioms where the
  conventional alias differs from the last segment: `str → clojure.string`,
  `io → clojure.java.io`, `async → clojure.core.async`, `edn → clojure.edn`,
  `set → clojure.set`, `walk → clojure.walk`, `pp → clojure.pprint`,
  `sh → clojure.java.shell`. Suggested with `:as prefix`.

**Verification filter:** a candidate namespace survives only if the symbol
after the slash is actually defined there — `index.lookup_in_ns(candidate,
name).is_some()`. This applies to the fully-qualified case too. It removes
wrong guesses at the cost of dropping candidates whose namespace is not
indexed (e.g. a library with no `.cpcache`); that trade-off is accepted.

**Ranking & dedup:** dedup candidates by namespace; curated-table hits sort
first, then last-segment / fully-qualified. Each surviving candidate becomes
one `CodeAction`.

### Edit construction

Each action carries an inline `WorkspaceEdit` (no resolve roundtrip). We
re-parse the live buffer with tree-sitter, find the top-level `(ns …)` form,
and build a single `TextEdit`:

- **Existing `(:require …)` clause:** insert the new spec on its own line
  immediately before the clause's closing paren, indented to the column of the
  first existing require spec (fallback: two spaces past `(:require `).
- **No `:require` clause:** insert `\n  (:require [foo :as f])` before the ns
  form's closing paren. This lands after name/docstring/attr-map whatever is
  present.
- All positions go through the extractor's existing UTF-16-aware
  `point_to_position`, so non-ASCII text above the insertion point does not
  shift the edit.

Spec text: `[ns.name :as prefix]` for alias candidates, `[ns.name]` for the
fully-qualified case.

Action title: ``Add require `[clojure.string :as str]` `` (the spec text in
backticks). `kind = CodeActionKind::QUICKFIX`.

### Wiring

- `src/handlers/code_action.rs` — pure `handle(index, documents, params) ->
  Result<Option<CodeActionResponse>>`, mirroring the other handlers.
- `handlers/mod.rs` — add `pub mod code_action;`.
- `server.rs` — `code_action` trait method delegating to the handler;
  `code_action_provider: Some(CodeActionProviderCapability::Simple(true))` in
  `ServerCapabilities`.

No diagnostics exist yet, so the action is cursor-driven; that is how Calva /
Neovim surface the code-action menu regardless.

### Testing

- **Unit (in `code_action.rs`):** candidate resolution and edit construction
  against a hand-built `Index` so library namespaces can be present without a
  classpath. Cases: last-segment hit, curated hit, fully-qualified ns,
  verification filtering out a namespace lacking the symbol, no action when the
  alias is already required, no action for `clojure.core`. Edit cases: ns with
  existing multi-spec `:require`, ns with docstring and no `:require`, ns with
  metadata.
- **e2e (`tests/test_e2e.rs`):** project-to-project so it needs no `.cpcache`.
  Add a `simple.helpers` fixture namespace with a `defn`, and a consumer file
  that calls `helpers/<fn>` without requiring it. Send `textDocument/codeAction`
  at that position; assert one action titled with `[simple.helpers :as helpers]`
  and that its edit inserts that spec.

Per CLAUDE.md, done means `bb check` and `bb e2e` pass; since this adds a
client-visible capability, `bb e2e-nvim` must pass too.

## File Structure

- Create: `src/handlers/code_action.rs` — detection, candidate resolution,
  edit construction, unit tests.
- Modify: `src/handlers/mod.rs` — register the module.
- Modify: `src/server.rs` — `code_action` method + capability.
- Modify: `tests/test_e2e.rs` — `code_action` client helper + e2e test.
- Modify: `tests/fixtures/simple_project/src/` — add `helpers.clj`; add a
  consumer usage (new file or existing) for the e2e test.
- Modify: `docs/ROADMAP.md` — check off the Phase 3 item.

## Implementation Steps

### Task 1: Candidate resolution (detection + namespaces)

**Files:**
- Create: `src/handlers/code_action.rs`
- Modify: `src/handlers/mod.rs`

- [x] **Step 1: Write focused unit tests for candidate resolution**
  In `code_action.rs` `#[cfg(test)]`, build an `Index` by hand
  (`Index::new`, `insert_file`/`insert_lib_file`) containing `clojure.string`
  with `join`, `clojure.set` with `union`, and `simple.helpers` with `greet`.
  Assert a helper `candidates(index, &live_ns_meta, "str/join")` returns
  `[("clojure.string", Some("str"))]` (curated); `"set/union"` returns
  `clojure.set` (curated + last-segment, deduped); `"clojure.set/union"`
  returns `[("clojure.set", None)]` (fully-qualified, no alias);
  `"helpers/greet"` returns `[("simple.helpers", Some("helpers"))]`
  (last-segment). Assert empty when the symbol is absent in the candidate
  (`"str/nope"`), when the alias is already in `NsMeta.aliases`, and for
  `clojure.core/x`.

- [x] **Step 2: Run the focused test (expect failure)**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib code_action`
  Expected: fails to compile / tests fail (function not implemented yet).

- [x] **Step 3: Implement detection + candidate resolution**
  Add the curated table, `split prefix/name`, the "already required" guard
  using a passed-in `&NsMeta`, last-segment scan over `index.namespaces`,
  fully-qualified check, the `lookup_in_ns` verification filter, dedup by
  namespace, and curated-first ranking. Keep candidate logic in a pure
  function taking `(&Index, &NsMeta, &str)` so it is unit-testable without a
  server.

- [x] **Step 4: Run verification**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib code_action`
  Expected: all candidate-resolution tests pass.

### Task 2: Edit construction

**Files:**
- Modify: `src/handlers/code_action.rs`

- [x] **Step 1: Write focused unit tests for the require edit**
  Assert a helper `require_edit(source, "[clojure.string :as str]")` returns a
  `TextEdit` whose application yields the expected text for: (a) an ns with an
  existing `(:require [a.b :as b])` clause — new spec appended as a new
  indented line before `)`; (b) an ns with a docstring and no `:require` — a
  new `(:require …)` clause inserted before the ns closing paren; (c) an ns
  with metadata (`^{:doc "…"}`). Verify by applying the edit to the source
  string and comparing.

- [x] **Step 2: Run the focused test (expect failure)**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib code_action`
  Expected: edit tests fail (not implemented).

- [x] **Step 3: Implement the edit builder**
  Parse with tree-sitter, locate the `(ns …)` list and any `(:require …)`
  child, compute the insertion `Position` via `point_to_position`, and return
  the `TextEdit`. Reuse extractor helpers where practical; if they are private,
  add a small local parse rather than widening their visibility unnecessarily.

- [x] **Step 4: Run verification**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib code_action`
  Expected: all `code_action` unit tests pass.

### Task 3: Server wiring

**Files:**
- Modify: `src/handlers/code_action.rs` (the `handle` entry point)
- Modify: `src/server.rs`

- [x] **Step 1: Implement `handle` and wire the server**
  Write `handle(index, documents, params)` that takes the request range start,
  resolves the token, re-extracts the live `NsMeta`, gathers candidates, and
  returns one `CodeActionResponse` entry per candidate (each a `CodeAction`
  with title, `QUICKFIX` kind, and inline `WorkspaceEdit`). Add the
  `code_action` trait method in `server.rs` (map errors to
  `internal_error()`), and add `code_action_provider:
  Some(CodeActionProviderCapability::Simple(true))`.

- [x] **Step 2: Run full check**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo build && bb check`
  Expected: builds, fmt/clippy clean, all unit + integration tests pass.

### Task 4: e2e test + fixture

**Files:**
- Modify: `tests/test_e2e.rs`
- Create: `tests/fixtures/simple_project/src/helpers.clj`
- Modify: a consumer fixture file (new or existing) using `helpers/<fn>`

- [x] **Step 1: Add fixture + e2e test**
  Add `helpers.clj` (`(ns simple.helpers)` + a `defn`). Add a consumer file
  that uses `helpers/<fn>` with no require. Add a `code_action` client helper
  to `LspClient` and a test that opens the consumer, sends
  `textDocument/codeAction` at the usage, and asserts one action titled with
  `[simple.helpers :as helpers]` and an edit inserting that spec.

- [x] **Step 2: Run e2e**
  Run: `bb e2e`
  Expected: the new test passes alongside the existing suite.

- [x] **Step 3: Run the editor-client e2e**
  Run: `bb e2e-nvim`
  Expected: passes (capability is advertised and the action is returned).

### Task 5: Roadmap

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Check off the item**
  Mark "Add-missing-require code action" done in Phase 3.

- [x] **Step 2: Final verification**
  Run: `bb check && bb e2e`
  Expected: green.

---

## Completion Summary

Implemented as planned. The `textDocument/codeAction` handler offers an
"Add require `[…]`" quickfix when the cursor is on a qualified symbol whose
prefix isn't yet required, drawing candidates from a curated alias table, the
fully-qualified-namespace case, and last-segment matches — each verified
against the index (the symbol must actually exist in the candidate namespace)
and rendered as an inline `WorkspaceEdit`.

**What shipped**

- `src/handlers/code_action.rs` — candidate resolution (`candidates`), edit
  construction (`require_edit`), and the `handle` entry point, with 15 unit
  tests.
- `NsMeta.requires` — new field recording every required namespace regardless
  of `:as`/`:refer`, so already-required namespaces (including plain
  `[clojure.set]` and bare `(:require clojure.set)`) aren't re-suggested.
  Bumped `CACHE_FORMAT_VERSION` 4 → 5 per the jar-cache invariant.
- `server.rs` — `code_action` method + `code_action_provider` capability.
- `tests/test_e2e.rs` — `code_action` client helper + `test_e2e_add_missing_require`,
  with `simple.helpers` / `simple.consumer` fixtures.

**Issues found and fixed during review** (codex second-opinion, one per task):

1. Self-require via fully-qualified prefix (`simple.helpers/greet` inside
   `simple.helpers`) — added a `prefix == ns_meta.name` guard.
2. Self-require via last-segment match (`helpers/greet` inside
   `simple.helpers`) — added a `ns != ns_meta.name` filter on resolved
   candidates.
3. Bare-symbol requires `(:require clojure.set)` weren't recorded in
   `requires`, risking duplicate suggestions — `extract_ns` now handles
   `sym_lit` specs.

**Known limitation:** legacy prefix-list libspecs `(:require (clojure set))`
are not expanded (consistent with the extractor's existing alias/refer
handling); a fully-qualified usage of such a namespace could still be
suggested. Out of scope for v1.

**Verification:** `bb check` (fmt + clippy `-D warnings` + 60 lib / all
integration tests), `bb e2e` (30 passed, 1 ignored), and `bb e2e-nvim` (real
Neovim client) all green.
