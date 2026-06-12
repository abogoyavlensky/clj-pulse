# Phase 2: Occurrence Index, References, Rename, Watched Files

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Index every resolved symbol usage so the server can answer find-references, perform cross-file rename, and stay correct across git operations.

**Tech Stack:** Rust, tower-lsp 0.20, tree-sitter, dashmap.

---

## Design

**Occurrence collection.** Second extraction pass in
`src/index/extractor.rs` sharing the single tree-sitter parse. Records each
symbol usage as `Occurrence { fqn, name_range }`, resolved at index time
through aliases/refers/current-ns/clojure.core (same rules as
`resolve_symbol`). Details:

- Qualified usages (`core/add`): `name_range` covers only the name part,
  never the alias.
- Locals shadowing: walker keeps a scope stack of locally bound names (fn
  params, `let`/`loop`/`for`/`doseq` binding vectors, names inside
  destructuring forms). Bare symbols bound locally are not recorded.
- `:refer` vector entries are recorded as occurrences so rename fixes
  require clauses.
- Def names themselves are NOT occurrences (the definition is in
  `Symbol.name_range`).

**Storage.** `Index.occurrences: DashMap<PathBuf, Vec<Occurrence>>` —
per-file for trivial invalidation. Project files only; JAR/dir libraries
excluded. `insert_file`/`remove_file` extended; updated on save and open.
Queries scan per-file lists filtering by fqn (O(total occurrences), fine).

**References.** Resolve word under cursor to an fqn — from a usage via
`resolve_symbol`, or directly when the cursor is inside a definition's
`name_range`. Collect occurrence locations across files; prepend the
definition location when `includeDeclaration`. Works for project, library,
and core fqns.

**Rename.** Same resolution; only project symbols are renameable (library/
core targets return a JSON-RPC error). `WorkspaceEdit.changes` with edits
at the definition `name_range` plus every occurrence `name_range`. New name
validated as a legal Clojure symbol.

**Unsaved edits.** At references/rename request time, re-extract
occurrences from live DocumentStore text for open files, overriding the
indexed version (handful of files, ~1ms each).

**Watched files.** Dynamic registration of `workspace/didChangeWatchedFiles`
in `initialized` (glob `**/*.{clj,cljs,cljc}` plus `deps.edn` and
`.cpcache/**`). Created/Changed → re-extract + insert; Deleted →
`remove_file`; deps.edn/.cpcache events → re-run classpath discovery and
library indexing. Clients without dynamic registration degrade gracefully.

**Error handling.** Handlers return `Ok(None)`/empty on unresolved words;
rename of non-project symbols returns `invalid_params` with a clear
message; watched-file read failures log and skip.

## File Structure

- Modify: `src/index/extractor.rs` — occurrence walker with scope stack.
- Modify: `src/index/mod.rs` — `Occurrence` type, `occurrences` map,
  insert/remove plumbing.
- Modify: `src/index/scanner.rs`, `src/server.rs` — wiring (project scan,
  did_save, did_open, watched files registration + handler).
- Create: `src/handlers/references.rs` — references + rename handlers.
- Modify: `src/handlers/mod.rs` — register module.
- Test: `tests/test_extractor.rs`, `tests/test_e2e.rs`.
- Modify: `docs/ROADMAP.md` — tick Phase 2 boxes.

## Implementation Steps

### Task 1: Occurrence extraction

**Files:** Modify `src/index/extractor.rs`, `src/index/mod.rs`; test `tests/test_extractor.rs`.

- [ ] **Step 1: Write failing unit tests**
  Qualified usage resolves through alias with name-only range; bare usage
  resolves to current ns; refer'd usage resolves to source ns; `:refer`
  vector entry recorded; locally bound names (params, let, destructuring)
  not recorded; core usage resolves to `clojure.core/...`.
- [ ] **Step 2: Implement walker**
  `extract` returns `(NsMeta, Vec<Symbol>, Vec<Occurrence>)` (or a struct);
  scope-stack walker over the tree; resolution mirroring `resolve_symbol`.
  Update existing callers.
- [ ] **Step 3: Verify**
  Run: `cargo test`  Expected: all green.

### Task 2: Index storage and wiring

**Files:** Modify `src/index/mod.rs`, `src/index/scanner.rs`, `src/server.rs`.

- [ ] **Step 1: Extend Index**
  `occurrences` map; `insert_file` takes occurrences; `remove_file` clears
  them; `insert_lib_file` inserts none. Bump `CACHE_FORMAT_VERSION` only if
  the cached `Symbol`/`NsMeta` layout changes (occurrences are not cached).
- [ ] **Step 2: Wire project scan, did_save, did_open**
- [ ] **Step 3: Verify**
  Run: `cargo test`  Expected: all green.

### Task 3: textDocument/references

**Files:** Create `src/handlers/references.rs`; modify `src/handlers/mod.rs`, `src/server.rs`; test `tests/test_e2e.rs`.

- [ ] **Step 1: Write failing e2e test**
  References for `add` from its definition in core.clj and from a usage in
  utils.clj; `includeDeclaration` true/false both asserted.
- [ ] **Step 2: Implement handler + capability**
  Resolution from usage or definition name; live-text re-extraction for
  open files.
- [ ] **Step 3: Verify**
  Run: `cargo test --test test_e2e`  Expected: all green.

### Task 4: textDocument/rename

**Files:** Modify `src/handlers/references.rs`, `src/server.rs`; test `tests/test_e2e.rs`.

- [ ] **Step 1: Write failing e2e tests**
  Cross-file rename of `add` → asserts exact edits in core.clj (definition)
  and utils.clj (alias-qualified usage, name part only); rename of a
  refer'd symbol fixes the `:refer` vector; rename of `map` (core) returns
  an error; rename with unsaved edits uses live ranges.
- [ ] **Step 2: Implement handler + capability**
  Symbol-name validation; project-only guard; `WorkspaceEdit.changes`.
- [ ] **Step 3: Verify**
  Run: `cargo test --test test_e2e`  Expected: all green.

### Task 5: workspace/didChangeWatchedFiles

**Files:** Modify `src/server.rs`; test `tests/test_e2e.rs`.

- [ ] **Step 1: Write failing e2e test**
  Simulate branch switch: create a new file on disk + send Created event →
  its symbols resolve; delete a file + Deleted event → its symbols gone.
- [ ] **Step 2: Implement**
  Dynamic registration in `initialized`; event handler; classpath refresh
  on deps.edn/.cpcache events.
- [ ] **Step 3: Verify**
  Run: `cargo test --test test_e2e`  Expected: all green.

### Task 6: Wrap-up

**Files:** Modify `docs/ROADMAP.md`.

- [ ] **Step 1: Tick Phase 2 checkboxes in the roadmap**
- [ ] **Step 2: Full verification**
  Run: `bb check && bb e2e-nvim`  Expected: all green.
- [ ] **Step 3: Codex review**
  Run review-with-codex on uncommitted changes; apply important findings;
  max 3 rounds.
