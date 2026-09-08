# Parse-Tree Cache and clj-kondo Size Threshold Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop re-parsing open buffers on every request and every lint pass by caching one incrementally-updated tree-sitter tree per document, and keep clj-kondo off the keystroke path for very large buffers, so a didChange pass on the bench's 452 KiB file drops from about 1.2 s to under 400 ms measured from the edit (the 300 ms debounce plus a native-only pass) (ROADMAP Milestone 1, the two bench-driven items). Runs before the Milestone 3 plans.

**Tech Stack:** Rust, tower-lsp 0.20, tree-sitter 0.25 (`Tree` is `Send + Sync`; `Parser::parse_with`, `Tree::edit`), ropey. Tests: `src/document.rs` unit tests, extractor tests, e2e (`tests/test_e2e.rs`), `bb bench`.

---

## Design

### What the bench showed

`docs/MEMORY.md` "Performance baseline", on `test/metabase/dashboards_rest/api_test.clj` (452 KiB):

- The native lint pass parses the buffer three times: `diagnostics::compute` calls `extract_analysis_with`, then `code_action::unused_requires` and `code_action::duplicate_requires` each parse again. About 285 ms of its 360 ms.
- Every position request (definition, hover, references, completion locals) parses once more through `extract_full_with`, `file_occurrences_with`, or `locals_in_scope_at`.
- clj-kondo costs about 840 ms on that file. That is its own runtime; we can only decide when to run it.
- Total didChange to diagnostics: about 1.2 s. The debounce alone is 300 ms.

### Tree cache in the document store

`DocumentStore` holds, per open document, the rope and a `tree_sitter::Tree` for the same version. `Tree` is `Send + Sync` in tree-sitter 0.25 (`binding_rust/lib.rs`, `unsafe impl Sync for Tree`), and cloning one is a reference-counted copy, so handing it out is cheap.

- `open` parses the full text once.
- `apply_changes` converts each incremental change into a `tree_sitter::InputEdit` before mutating the rope: `start_byte` and `old_end_byte` from the rope's char-to-byte mapping of the change range, `new_end_byte = start_byte + change.text.len()`, and the three `Point`s (row, byte column) from the rope's line/byte lookups, with the new end computed by walking the inserted text for newlines. It calls `tree.edit(&edit)`, applies the rope change, and then reparses with `parser.parse_with(&mut |byte, _| rope chunk at byte, Some(&old_tree))`, reading straight from rope chunks so no `String` is built. A full-text change (`range == None`) replaces the tree with a fresh parse.
- Reparse happens eagerly inside `apply_changes`. An incremental reparse after a one-character edit is milliseconds even on the bench file, and eager keeps the invariant simple: the tree in the store always matches the rope.
- One `Parser` per store, behind a `Mutex`; `Parser` is not shared across threads otherwise.

Reading API: `pub fn snapshot(&self, uri) -> Option<Snapshot>` where

```rust
pub struct Snapshot {
    pub text: String,          // rope materialized once
    pub tree: tree_sitter::Tree,
}
```

Text and tree come from one lock acquisition, so a handler can never pair a tree with text from another version. `text(uri)` stays for callers that need only text.

### Extractor entry points take a tree

Each public function that parses gains a `_tree` variant taking `(&Tree, &str, …)`; the string version becomes a thin wrapper that parses and delegates, so the scanner, JAR indexing, `extract_edn`, and every existing test are untouched:

- `extractor`: `extract_full_with`, `extract_analysis_with`, `file_occurrences_with`, `qualified_usages`, `locals_in_scope_at`, `local_references_at`.
- `code_action`: `unused_requires`, `duplicate_requires`, `clean_ns_edits`, `require_edit`.
- `ignored_forms::ignored_form_ranges`.

`diagnostics::compute` gains `compute_tree(&Tree, &str, path, cfg)` that parses zero times; `compute` wraps it for the unit tests and the scanner-side callers, if any. The blocking task in `lint_and_publish_doc` receives the snapshot (the tree clone travels to the thread; `Tree: Send`).

Handlers on open buffers switch to `documents.snapshot(&uri)` plus the `_tree` variants: definition, hover, references (`resolve_fqn_at`, `occurrences_for` for open documents), rename, prepareRename, completion locals, document symbols, code actions, ignored forms, and the lint pass. Closed files (index scans) keep parsing from disk. The Milestone 3 highlight and selection handlers, written to parse per request, switch to the snapshot in their own plans once this lands.

### clj-kondo size threshold

Two publishes per pass (native first, kondo later) is rejected: the one-publish-per-pass invariant is what keeps squiggles from flickering and every layer above it assumes it. Instead:

- `KondoConfig` gains `live_max_kb: u32`, default 256, `0` meaning no limit. Configured as `:kondo {:live-max-kb 256}` in `.clj-pulse/config.edn`, `"kondo": {"liveMaxKb": 256}` in editor settings, parsed by both `parse_config_edn` and `parse_config_json` with the same override-and-merge rule as `enabled` and `path`.
- `lint_and_publish_doc` gains a `trigger: LintTrigger` parameter (`Open`, `Save`, `Change`, `EngineChange`). On `Change`, when the buffer's byte length exceeds the threshold, the kondo pass is skipped (the same "clj-kondo has no say in this pass" `Err` path the missing-binary case uses), so the native set publishes alone. `Open`, `Save`, and `EngineChange` always run kondo.
- The lint-status notification the extension shows (`clojurePulse/lintStatus`) is unchanged: the engine is still active, it just sits out keystrokes on that buffer. The README states this next to the setting.

### Acceptance

`bb bench` after both changes, same corpus and file: didChange to diagnostics median under 400 ms as the bench measures it, from the edit, which is the 300 ms debounce plus a native-only pass of under 100 ms; definition under 20 ms on the Linux box. Numbers go into `docs/MEMORY.md` as a second table under the baseline, with the commit.

### Testing

- `document.rs` unit tests for the edit conversion: insert on one line, delete across lines, insert containing newlines, an edit after an emoji on the same line (UTF-16 columns to byte columns), a full-text replacement, and a sequence of edits whose final tree equals a fresh parse of the final text (`tree.root_node().to_sexp()` equality).
- Extractor and code-action tests that the `_tree` variants give the same output as the string versions on the existing fixtures (one parametric test per pair).
- e2e: `test_e2e_diagnostics_stable_across_edits`: open a fixture file, apply twenty inserts and deletes that add and then remove an unused require, and assert the diagnostics after each publish match a fresh-server open of the same final text. `test_e2e_definition_after_edits` for a position request through the cache.
- e2e threshold: with the fake kondo (`start_with_kondo`) and `live-max-kb` set to `1`, a didChange on a fixture file publishes diagnostics with `source: "clj-pulse"` only, and a didSave publishes the kondo set; with `0`, didChange publishes the kondo set.
- `bb bench` before and after, table recorded.

## File Structure

Modify:

- `src/document.rs`: tree per document, `Parser` behind a mutex, `InputEdit` conversion, `Snapshot`, `snapshot()`, tests.
- `src/index/extractor.rs`: `_tree` variants, `parse_tree` made `pub`.
- `src/handlers/code_action.rs`, `src/handlers/ignored_forms.rs`: `_tree` variants.
- `src/diagnostics.rs`: `compute_tree`.
- `src/handlers/{definition,hover,references,completion,symbols}.rs`: use snapshots.
- `src/server.rs`: `LintTrigger`, threshold gate, callers pass the trigger, handlers pass snapshots.
- `src/kondo.rs`: `live_max_kb` in `KondoOverride`, `KondoConfig`, both parsers.
- `tests/test_e2e.rs`: the four tests above.
- `docs/MEMORY.md`: after-table. `README.md`, `AGENTS.md`, `ARCHITECTURE.md`, `docs/ROADMAP.md`.

The Clojure Pulse extension setting `clojurePulse.kondo.liveMaxKb` is a one-line addition to its `package.json` and settings push; it is a separate change in `../clojure-pulse-vscode`, noted in the final task, not part of this plan's gates.

## Tasks

### Task 1: Tree per document

**Files:**
- Modify: `src/document.rs`

- [ ] **Step 1: Write the failing tests**
  In `document.rs` `mod tests`, next to `test_apply_changes_utf16_after_emoji`: the six edit-conversion cases from the design, each asserting that the cached tree equals a fresh parse of `snapshot(uri).text`: same `root_node().to_sexp()` *and* the same start and end byte offsets and points for every named node in a pre-order walk. Structure alone would not prove the coordinates navigation and diagnostics consume.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --lib document::tests::snapshot`
  Expected: FAIL (no `snapshot`).

- [ ] **Step 3: Implement**
  Store `(Rope, Tree)` per document, the parser mutex, `InputEdit` conversion, eager reparse through `parse_with` over rope chunks, `Snapshot` and `snapshot()`. Keep `text()`, `word_at`, `keyword_at`, and the rest reading the rope as today.

- [ ] **Step 4: Run the tests**
  Run: `bb check`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Keep an incrementally updated parse tree per open document"`

### Task 2: Tree-taking extractor and code-action entry points

**Files:**
- Modify: `src/index/extractor.rs`, `src/handlers/code_action.rs`, `src/handlers/ignored_forms.rs`, `src/diagnostics.rs`
- Test: `tests/test_extractor.rs`, `src/handlers/code_action.rs` tests

- [ ] **Step 1: Write the equivalence tests**
  For each pair, parse a fixture once and assert the `_tree` variant equals the string variant. Put extractor pairs in `tests/test_extractor.rs`, code-action pairs in `code_action.rs` tests, `compute_tree` in `diagnostics.rs` tests.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor tree_variant`
  Expected: FAIL (no such functions).

- [ ] **Step 3: Implement**
  Add the variants; the string versions delegate. No behavior change.

- [ ] **Step 4: Run the tests**
  Run: `bb check`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Let extraction and lints run on an existing parse tree"`

### Task 3: Handlers and the lint pass use the snapshot

**Files:**
- Modify: `src/server.rs`, `src/handlers/{definition,hover,references,completion,symbols}.rs`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing e2e tests**
  `test_e2e_diagnostics_stable_across_edits` and `test_e2e_definition_after_edits` from the design. They pass today by construction; they are regression guards for the switch. Add a debug log line in `lint_and_publish_doc` reporting whether the pass parsed (`parsed=0`) so the test can assert the cache was used via `wait_for_log`.

- [ ] **Step 2: Switch the callers**
  Every open-buffer handler and `lint_and_publish_doc` take `documents.snapshot(&uri)` and call the `_tree` variants. `occurrences_for` uses snapshots for open documents. Search for remaining `documents.text(` callers that then parse and convert them; `word_at`, `keyword_at`, indent, and text-only uses stay.

- [ ] **Step 3: Run the tests**
  Run: `bb check && bb e2e && bb e2e-pulse && bb e2e-calva`
  Expected: PASS; the lint log shows `parsed=0`. Calva is required here: the definition handler changed how it reads the buffer, and the Calva gate is the one that proves `jar:` locations still come out right.

- [ ] **Step 4: Commit**
  `git commit -m "Serve requests and lints from the cached parse tree"`

### Task 4: clj-kondo size threshold

**Files:**
- Modify: `src/kondo.rs`, `src/server.rs`
- Test: `tests/test_e2e.rs`, `src/kondo.rs` tests

- [ ] **Step 1: Write the failing tests**
  `kondo.rs`: `parse_config_edn` and `parse_config_json` read `live-max-kb` / `liveMaxKb`; merge keeps the lower layer when absent. e2e: the two threshold tests from the design (`start_with_kondo_env` with a config file setting `:live-max-kb 1`, then `0`).

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e kondo_threshold`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  `live_max_kb` through both config layers, `LintTrigger`, the gate in `lint_and_publish_doc`, and each caller passing its trigger (`did_open`, `did_save`, the debounced `did_change`, the engine-reload re-lint).

- [ ] **Step 4: Run the tests**
  Run: `bb check && bb e2e && bb e2e-pulse`
  Expected: PASS. The Pulse run proves the extension's lint-status display still reports the engine as active while a large buffer sits out keystrokes.

- [ ] **Step 5: Commit**
  `git commit -m "Skip clj-kondo on keystrokes for buffers above a size threshold"`

### Task 5: Bench and docs

**Files:**
- Modify: `docs/MEMORY.md`, `README.md`, `AGENTS.md`, `ARCHITECTURE.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Run the bench**
  Run: `bb bench`
  Expected: didChange to diagnostics median under 400 ms on the Linux box, definition under 20 ms. If not, profile before touching docs: add `tracing` timings around the native pass, the snapshot call (text materialization of a 452 KiB rope), the occurrence walk, and the publish, and read the bench's own breakdown. A leftover parse in Task 3 is one candidate, not the only one; tree traversal, text materialization, and tokio scheduling can each account for tens of milliseconds on that file.

- [ ] **Step 2: Record and document**
  MEMORY.md: an "After the tree cache" table with the commit. README: the `:kondo {:live-max-kb}` setting with one sentence on what large files lose until save; name the extension setting `clojurePulse.kondo.liveMaxKb` as *pending* until the extension change ships, so the README never claims a setting the Marketplace build lacks. AGENTS.md invariants: the tree in the store always matches the rope (eager reparse), handlers must take a `Snapshot` never text plus a separate parse, and the threshold applies to `Change` only. ARCHITECTURE "Data Flow": the snapshot. ROADMAP: tick both Milestone 1 items, `Plan:` to `done`. Use /writing-clearly.

- [ ] **Step 3: Extension follow-up**
  Note in the commit message and in the roadmap Backlog: `clojurePulse.kondo.liveMaxKb` to be added to `../clojure-pulse-vscode` (package.json setting plus the settings push), a separate change.

- [ ] **Step 4: Verify and commit**
  Run: `bb check && bb e2e`
  Expected: PASS.
  `git commit -m "Record the post-cache bench and document the kondo threshold"`
