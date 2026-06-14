# Skip Diagnostics on EDN Config Files Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop clj-lsp from flagging dependency coordinates in `deps.edn` / `lgx.edn` as unresolved namespaces by restricting diagnostics and on-open indexing to actual Clojure source files.

**Tech Stack:** Rust, tower-lsp, tree-sitter-clojure.

---

## Design

### The bug

Opening `lgx.edn` (or `deps.edn`) produces spurious `unresolved-namespace` warnings, e.g. `Unresolved namespace: my`, `Unresolved namespace: ext` (or `org.clojure` in a `deps.edn`).

Reproduced via the e2e harness against `tests/fixtures/letgo_project`: opening `lgx.edn` publishes two `unresolved-namespace` diagnostics, one per dependency coordinate.

### Root cause

`server.rs` runs `diagnostics::compute` on every opened/changed/saved document with no file-type filter (`did_open` → `lint_and_publish`, `did_save` → `lint_and_publish`, `did_change` → inline debounced closure).

`diagnostics::compute` calls `extractor::qualified_usages`, which walks **every** namespaced `sym_lit` in the parse tree — including symbols inside vectors and maps. In an EDN dependency map, a coordinate like `my/loc` or `org.clojure/clojure` is exactly such a namespaced symbol (`prefix/name`), structurally indistinguishable from a qualified namespace usage. None of these prefixes resolve from the (nonexistent) `ns` form, so each is flagged.

Separately, `did_open` also tries to *index* the opened file when it isn't already in the index. For an `.edn` file with no `(ns …)` form this inserts a junk empty-namespace entry into the index.

### Fix

Restrict both linting and on-open indexing to real Clojure source files: `.clj`, `.cljs`, `.cljc`, `.lg`. EDN config files (`deps.edn`, `lgx.edn`, any `.edn`) get no diagnostics and are never indexed — we provide no language intelligence for them, so we should not lint them.

Decisions locked in during design:

- **Gate centrally in `diagnostics::compute`.** Add an early `return vec![]` for non-source paths. One chokepoint covers all three lint call sites. Publishing an empty diagnostics array for an `.edn` file is correct: it clears any squiggles.
- **One shared helper, `config::is_clojure_source(&Path) -> bool`,** as the single source of truth for the source-extension set. The same `clj`/`cljs`/`cljc`/`lg` check is currently duplicated inline in `scanner.rs` (`collect_clojure_files`) and `server.rs` (`did_change_watched_files`). Route those through the helper too — a small, low-risk DRY win.
- **Gate `did_open` indexing** on `is_clojure_source` so opening `deps.edn` / `lgx.edn` no longer inserts a junk empty-namespace.
- **`project.clj` is out of scope.** It is a real `.clj` source file and stays linted; clj-lsp does not support Leiningen `project.clj` projects yet, so its `:dependencies` vector is not a concern here.

### Testing strategy

- Unit test in `diagnostics.rs`: `compute` on a `deps.edn` / `lgx.edn` path returns empty even when dep-coordinate symbols are present — proves the gate independent of the server.
- e2e regression in `test_e2e.rs`: open `lgx.edn` from the let-go fixture and assert the published diagnostics list is empty (the inverse of the reproduction).
- Full `bb check` (fmt + clippy `-D warnings` + unit tests) and `bb e2e`.

## File Structure

- `src/config.rs` — add `pub fn is_clojure_source(path: &Path) -> bool`. Config/file classification already lives here (`project_kind`, `source_paths`), so the extension predicate belongs here too.
- `src/diagnostics.rs` — early-return empty from `compute` for non-source paths; add unit test.
- `src/server.rs` — gate `did_open` indexing on `is_clojure_source`; replace the inline extension check in `did_change_watched_files` with the helper.
- `src/index/scanner.rs` — replace the inline extension check in `collect_clojure_files` with the helper.
- `tests/test_e2e.rs` — add an e2e regression test that opening `lgx.edn` yields no diagnostics.

## Task Structure

### Task 1: `is_clojure_source` helper

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**
  In `config.rs` tests, add `test_is_clojure_source`: asserts `true` for paths ending `.clj`, `.cljs`, `.cljc`, `.lg`; asserts `false` for `deps.edn`, `lgx.edn`, `foo.edn`, and an extensionless path.

- [ ] **Step 2: Run test to verify it fails**
  Run: `cargo test --lib config::tests::test_is_clojure_source`
  Expected: FAIL (function does not exist / does not compile).

- [ ] **Step 3: Write minimal implementation**
  Add `pub fn is_clojure_source(path: &Path) -> bool` returning whether the file extension is one of `clj`, `cljs`, `cljc`, `lg` (lowercase, matching the existing inline checks).

- [ ] **Step 4: Run test to verify it passes**
  Run: `cargo test --lib config::tests::test_is_clojure_source`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Add config::is_clojure_source helper"`

### Task 2: Gate diagnostics on source files

**Files:**
- Modify: `src/diagnostics.rs`

- [ ] **Step 1: Write the failing test**
  In `diagnostics.rs` tests, add `no_flag_on_edn_dependency_coordinates`: build the source string of an `lgx.edn`-style map with dep coordinates (e.g. `{:deps {my/loc {:local/root "v"} ext/lib {:git/url "u" :git/sha "s"}}}`) and assert `compute(src, Path::new("lgx.edn"))` is empty. Add a sibling assertion for a `deps.edn` path with `org.clojure/clojure`.

- [ ] **Step 2: Run test to verify it fails**
  Run: `cargo test --lib diagnostics::tests::no_flag_on_edn_dependency_coordinates`
  Expected: FAIL — currently returns two `unresolved-namespace` diagnostics.

- [ ] **Step 3: Write minimal implementation**
  At the top of `compute`, add `if !crate::config::is_clojure_source(path) { return vec![]; }`. Keep the rest unchanged. Confirm existing `diags("…")` helper tests (which use `test.clj`) still pass.

- [ ] **Step 4: Run test to verify it passes**
  Run: `cargo test --lib diagnostics::tests`
  Expected: PASS (new test passes, all existing diagnostics tests still pass).

- [ ] **Step 5: Commit**
  `git commit -m "Skip diagnostics for non-Clojure-source files"`

### Task 3: Gate did_open indexing and DRY the extension checks

**Files:**
- Modify: `src/server.rs`
- Modify: `src/index/scanner.rs`

- [ ] **Step 1: Gate `did_open` indexing**
  In `server.rs` `did_open`, only run the `extract_full` / `insert_file` indexing branch when `config::is_clojure_source(&path)`. Opening an `.edn` file must not insert a namespace. Linting is already handled by Task 2's central gate.

- [ ] **Step 2: Route existing extension checks through the helper**
  Replace the inline `e == "clj" || e == "cljs" || e == "cljc" || e == "lg"` check in `server.rs` `did_change_watched_files` with `config::is_clojure_source(&path)`. Replace the equivalent inline check in `scanner.rs` `collect_clojure_files` with the helper (adjust to operate on the file `Path`). Behavior is unchanged; this removes duplication.

- [ ] **Step 3: Verify the build and unit tests**
  Run: `cargo test --lib`
  Expected: PASS, no clippy/compile errors.

- [ ] **Step 4: Commit**
  `git commit -m "Gate did_open indexing and reuse is_clojure_source"`

### Task 4: e2e regression test

**Files:**
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing-then-passing e2e test**
  Add `test_e2e_no_diagnostics_on_lgx_edn`: start the server against the `letgo_project` fixture with `LGX_HOME` set (mirror `test_e2e_letgo_navigation_into_lgx_deps`), `did_open` the project's `lgx.edn`, wait for its `publishDiagnostics`, and assert the `diagnostics` array is empty. Use the existing `wait_for_diagnostics("/lgx.edn")` helper.

- [ ] **Step 2: Run the test**
  Run: `cargo test --test test_e2e test_e2e_no_diagnostics_on_lgx_edn`
  Expected: PASS (with the fix from Tasks 1–3 in place).

- [ ] **Step 3: Commit**
  `git commit -m "e2e: lgx.edn produces no unresolved-namespace diagnostics"`

### Task 5: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Run the full check suite**
  Run: `bb check`
  Expected: PASS — fmt clean, clippy `-D warnings` clean, all unit tests pass.

- [ ] **Step 2: Run the e2e suite**
  Run: `bb e2e`
  Expected: PASS — including the new `lgx.edn` diagnostics test and the existing let-go navigation test.
