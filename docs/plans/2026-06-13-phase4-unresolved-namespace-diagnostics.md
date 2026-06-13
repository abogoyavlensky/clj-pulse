# Unresolved-Namespace Diagnostics Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish a native `unresolved-namespace` warning for qualified usages whose namespace prefix isn't required, so the editor shows a squiggle that triggers the existing add-require quickfix lightbulb.

**Tech Stack:** Rust, tower-lsp, tree-sitter (tree-sitter-clojure), existing namespace index.

---

## Design

### Detection (pure, index-free)

A new top-level module `src/diagnostics.rs` exposes
`compute(source: &str, path: &Path) -> Vec<Diagnostic>`. Availability is purely
file-local, so this needs no index — a project without `.cpcache` therefore
produces no false positives (a required namespace is never flagged regardless
of whether its target is indexed).

Detection flags qualified symbol usages (`prefix/name`) when the prefix is
neither resolvable nor Java/JS interop:

- **Qualified usages** come from a new
  `extractor::qualified_usages(source) -> Vec<QualifiedUsage>` where
  `QualifiedUsage { prefix: String, name: String, range: Range }` and `range`
  covers the whole symbol (`str/join`). The walker collects `sym_lit` nodes
  that carry a `namespace:` field, recursing generally but skipping `'`-quoted
  data (`quoting_lit`) and `(quote …)` lists — syntax-quote is **not** skipped
  (macro bodies reference real vars). Instance methods (`.foo`), constructors
  (`Foo.`), and keywords (`:a/b`) have no `namespace:` field and are naturally
  excluded. Usages with an empty name (`str/` mid-type) are skipped.
- **Resolvable** is a shared `NsMeta::resolves_prefix(prefix) -> bool`:
  `prefix == self.name` (own ns), an `:as` alias key, or a member of
  `requires`. `code_action::candidates` is refactored to use this same method
  so the squiggle and the add-require fix can never disagree. `clojure.core` is
  also always available.
- **Interop skip**: the prefix's last dot-segment starts with an uppercase
  letter (`Math`, `System`, `java.util.Date`, `clojure.lang.RT`), or the prefix
  is the cljs global `js`.

Each remaining usage becomes a `Diagnostic` over the whole-symbol range:
`severity = WARNING`, `source = "clj-lsp"`, `code = "unresolved-namespace"`,
`message = "Unresolved namespace: <prefix>"`.

### Publishing and debounce

- `DocumentStore` gains `versions: DashMap<Url, i32>` with `set_version` /
  `current_version`, cleared on close. The server records the LSP document
  version on open and change.
- `server.rs`:
  - `did_open`, `did_save`: compute from the live buffer and publish
    immediately.
  - `did_change`: after applying the edit and recording the new version, spawn
    a task that sleeps 300 ms, then lints only if `current_version(uri)` still
    equals the captured version (superseded edits self-cancel — no explicit
    timer cancellation).
  - `did_close`: publish empty diagnostics to clear.
  - All publishes pass `Some(version)` to `publish_diagnostics`.

### Lightbulb linkage

`code_action::handle` copies any incoming `params.context.diagnostics` whose
`code` is `unresolved-namespace` onto the returned `CodeAction.diagnostics`, so
VS Code binds the add-require fix to the squiggle. The action still resolves
the token via `word_at`, so behavior without diagnostics is unchanged.

### Testing

- `diagnostics.rs` unit tests: flags an unrequired `str/join`; skips a required
  alias, a plain require, the current ns, `clojure.core`, `Math/PI`,
  `js/console`, and `java.util.Date/from`; asserts whole-symbol range,
  `WARNING`, and the code; skips empty-name.
- extractor unit tests: `qualified_usages` finds qualified syms, skips
  `'`-quoted data, and descends into reader conditionals.
- e2e: open a file using `helpers/greet` unrequired → assert a
  `publishDiagnostics` carrying `unresolved-namespace`; then a `code_action` at
  that position returns the add-require fix with the diagnostic attached. Adds a
  `wait_for_diagnostics` helper to `LspClient`.
- Per CLAUDE.md, done means `bb check` and `bb e2e` pass; as a client-visible
  protocol change, `bb e2e-nvim` must pass too.

## File Structure

- Create: `src/diagnostics.rs` — pure `compute` + unit tests.
- Modify: `src/lib.rs` — register the `diagnostics` module.
- Modify: `src/index/extractor.rs` — `QualifiedUsage` + `qualified_usages`.
- Modify: `src/index/mod.rs` — `NsMeta::resolves_prefix`.
- Modify: `src/handlers/code_action.rs` — use `resolves_prefix`; attach
  diagnostics to the action.
- Modify: `src/document.rs` — version tracking.
- Modify: `src/server.rs` — publish on open/change/save/close; debounce.
- Modify: `tests/test_e2e.rs` — `wait_for_diagnostics` + e2e test.
- Modify: `tests/test_extractor.rs` — `qualified_usages` tests.
- Modify: `docs/ROADMAP.md` — note the native unresolved-namespace lint landed.

## Implementation Steps

### Task 1: Qualified-usage extraction

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write focused tests**
  In `test_extractor.rs`, assert `qualified_usages` on a snippet with
  `(str/join …)`, `(clojure.set/union …)`, a quoted `'foo/bar` (excluded), and
  a qualified usage inside a reader conditional returns the expected
  (prefix, name) pairs with whole-symbol ranges and excludes the quoted one.

- [ ] **Step 2: Run the focused test (expect failure)**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --test test_extractor qualified`
  Expected: fails to compile (function missing).

- [ ] **Step 3: Implement `qualified_usages`**
  Add `pub struct QualifiedUsage { pub prefix, pub name, pub range }` and
  `pub fn qualified_usages(source: &str) -> Vec<QualifiedUsage>`. Parse with the
  existing `language()`, walk recursively collecting `sym_lit` with a
  `namespace:` child, skip `quoting_lit` and `(quote …)`, skip empty names, and
  build ranges via `node_to_lsp_range`/`point_to_position`.

- [ ] **Step 4: Run verification**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --test test_extractor`
  Expected: all extractor tests pass.

### Task 2: Shared prefix-resolution + diagnostics compute

**Files:**
- Modify: `src/index/mod.rs`
- Modify: `src/handlers/code_action.rs`
- Create: `src/diagnostics.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write focused unit tests in `diagnostics.rs`**
  Cover: flags `str/join` when unrequired; no flag when `str` is aliased, when
  the ns is plainly required, for the current ns, for `clojure.core/x`, for
  `Math/PI`, for `js/console`, for `java.util.Date/from`; whole-symbol range,
  `WARNING` severity, `unresolved-namespace` code; empty-name skipped.

- [ ] **Step 2: Run the focused test (expect failure)**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib diagnostics`
  Expected: fails to compile (module/function missing).

- [ ] **Step 3: Implement**
  Add `NsMeta::resolves_prefix`. Refactor `code_action::candidates` to use it
  (keeping the `clojure.core` guard). Create `src/diagnostics.rs` with
  `compute(source, path)` using `extractor::extract` (for `NsMeta`) +
  `extractor::qualified_usages`, the `resolves_prefix`/`clojure.core`/interop
  filters, and the interop helper (uppercase last segment or `js`). Register
  `pub mod diagnostics;` in `lib.rs`.

- [ ] **Step 4: Run verification**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo test --lib diagnostics code_action`
  Expected: diagnostics + code_action unit tests pass.

### Task 3: Document versions + server publishing/debounce

**Files:**
- Modify: `src/document.rs`
- Modify: `src/server.rs`

- [ ] **Step 1: Implement version tracking**
  Add `versions: DashMap<Url, i32>` to `DocumentStore` with `set_version` and
  `current_version`; clear in `close`.

- [ ] **Step 2: Wire publishing + debounce in `server.rs`**
  Record the doc version on `did_open`/`did_change`. Publish immediately on
  `did_open` and `did_save`; on `did_change`, spawn a 300 ms debounce task that
  lints only if `current_version` matches the captured version. Clear on
  `did_close`. Use `client.publish_diagnostics(uri, diags, Some(version))`.

- [ ] **Step 3: Attach diagnostics to the code action**
  In `code_action::handle`, set the returned `CodeAction.diagnostics` to the
  incoming `context.diagnostics` whose `code` is `unresolved-namespace`.

- [ ] **Step 4: Run full check**
  Run: `CARGO_TARGET_DIR=/tmp/clj-lsp-target cargo build && bb check`
  Expected: builds; fmt/clippy clean; all tests pass.

### Task 4: e2e test

**Files:**
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Add `wait_for_diagnostics` + test**
  Add an `LspClient` helper that reads notifications until a
  `textDocument/publishDiagnostics` for a given uri arrives and returns its
  params. Add `test_e2e_unresolved_namespace_diagnostic`: open `consumer.clj`
  (uses `helpers/greet`, not required), assert a diagnostic with code
  `unresolved-namespace` over the usage, then assert `code_action` at that
  position returns the `[simple.helpers :as helpers]` fix.

- [ ] **Step 2: Run e2e**
  Run: `bb e2e`
  Expected: the new test passes with the suite.

- [ ] **Step 3: Run the editor-client e2e**
  Run: `bb e2e-nvim`
  Expected: passes.

### Task 5: Roadmap + final verification

**Files:**
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Note progress**
  Mark the native unresolved-namespace lint as landed in Phase 4 (the broader
  item also covers unused require / unresolved symbol, which remain).

- [ ] **Step 2: Final verification**
  Run: `bb check && bb e2e`
  Expected: green.
