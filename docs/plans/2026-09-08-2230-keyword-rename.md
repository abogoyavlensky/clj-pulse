# Keyword Rename Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename a qualified keyword across the project, rewriting every occurrence in its own notation (`::name`, `::alias/name`, `:ns/name`), Integrant EDN files included, and refusing only what cannot be rewritten safely (ROADMAP Milestone 3, part 2 of 2).

**Tech Stack:** Rust, tower-lsp 0.20. Tests: unit tests in `src/handlers/references.rs`, e2e (`tests/test_e2e.rs`) on `tests/fixtures/integrant_project` and `simple_project`, `bb e2e-pulse` (rename is client-visible).

---

## Design

### Today

`references::rename_target` refuses any colon-prefixed fqn ("renaming keywords is not yet supported"). Keyword occurrences are recorded with the whole token as `name_range` (`record_keyword_occurrence`), for both Clojure sources and Integrant EDN files (`extract_edn`), and the `ig/init-key` dispatch keyword is additionally a `DefKind::IntegrantKey` symbol. After the keyword-completion plan, unqualified keywords are occurrences too, with fqn `:name`.

One usage shape is *not* an occurrence today: the entries of a namespaced destructuring vector. `{::keys [name]}`, `{:my.ns/keys [name]}`, and `{:keys [my.ns/name]}` all read the key `:my.ns/name`, but `collect_binding_names` records `name` as a local binding only. A rename that scanned occurrences alone would rewrite every other site and silently leave these reading the old key.

### Destructuring sites become occurrences

The occurrence walker records one keyword occurrence per entry of a namespaced `:keys`/`:syms` vector (and per namespaced entry of a plain `:keys` vector), with fqn `:ns/name` and `name_range` = the entry symbol's range. The walker already identifies these directives (`extractor.rs`, the `{:user/keys [name]}` handling around line 2766). Two effects, both wanted: find-references on `::name` now lists destructuring sites, and rename can see them. `:strs` vectors are excluded (they read string keys).

### Scope

- **Qualified keywords only.** `:name` is refused with "cannot rename an unqualified keyword": the same name in unrelated maps is not one thing.
- **Project keywords only.** When the keyword's namespace is an indexed *library* namespace (an `NsMeta` whose file is not a project path), refuse with "cannot rename a keyword of library namespace `ns`", the keyword analogue of the library-symbol rule.
- The new name is a bare name: no `/`, no leading `:`. `is_valid_symbol_name` covers the first; a leading colon gets its own message ("type the new name without the colon").

### The edit: replace the name suffix

Every notation ends with the name: `::name`, `::alias/name`, `:ns/name`. So each edit replaces only the trailing `name` of the token, and the notation takes care of itself. Concretely, for an occurrence with `name_range` and token text `tok`:

- `tok` must end with `/name` or `:name` (the old name preceded by its separator). If it does, the edit range is the last `name.len()` UTF-16 units of `name_range` on its end line, with `new_text = new_name`.
- If it does not, the rename is refused as a whole (all-or-nothing), with a message naming the file and line. The known shape is a destructuring entry (now an occurrence whose token is the bare symbol); that case gets a targeted message: "rename would break `{::keys [name]}` destructuring at `file:line`; rewrite it as `{new-name ::name}` first". It mirrors the existing `:keys` local rejection. Any other non-conforming token gets a generic refusal.

Lengths are in UTF-16 units (`name.encode_utf16().count()`), never bytes, so a keyword like `:ns/naïve` edits the right columns. Token text comes from the live buffer when the file is open (`documents.text`), otherwise from disk (`std::fs::read_to_string`), sliced on the range's line by UTF-16 columns. Keyword tokens never span lines.

### Which occurrences

`references::occurrences_for(index, documents, fqn)` returns every occurrence per file, open buffers re-extracted live, EDN files included (their occurrences live in `index.occurrences` through `insert_edn_file`). It also includes open *library* buffers (`jar:` documents), so the keyword path filters to `index.is_project_path(&file)`; a library file is never edited even when the user has it open. The `IntegrantKey` symbol's `name_range` is the same span as its occurrence, so edits are de-duplicated by `(uri, range)` after collection.

### `rename_target` and `prepareRename`

`RenameTarget` gains `Keyword { fqn: String, sites: Vec<(Url, Range)> }`, where `sites` is the complete, validated, de-duplicated list of suffix ranges. Every rejection that does not depend on the new name lives in `rename_target`, per the repository invariant that `rename` and `prepareRename` share their rejections: the unqualified and library-namespace rules, the project-path filter, and the token checks including the destructuring refusal. `prepare_rename` therefore rejects exactly what `rename` would, and answers with the site range at the cursor. `rename` only validates the new name and maps `sites` to edits. The cost is that `prepareRename` scans occurrences too; rename is rare enough that this does not matter.

### Testing

- Extractor: destructuring entries recorded as keyword occurrences for `{::keys [a]}`, `{:my.ns/keys [a]}`, `{:keys [my.ns/a]}`, and `{::syms [a]}`; not for `:strs`; and `references` on the keyword now lists the entry.
- Unit (`references.rs` tests): the suffix rule on each notation including a non-ASCII name, the refusal for a bare-symbol token, the library-namespace refusal, the unqualified refusal, and the project-path filter.
- e2e on `integrant_project`: rename `::db` from `db.clj` to `store` and assert edits at all three `defmethod` dispatch keywords in `db.clj` and both `:readx.db/db` occurrences in `resources/config.edn` (including the one after `#ig/ref`), each edit covering exactly the `db` suffix; `prepareRename` on `::db` returns the suffix range; rename with new name `:store` is refused with the colon message.
- e2e on `simple_project`: add a file with `{::keys [thing]}` destructuring of a keyword used elsewhere, and assert both rename and prepareRename are refused with the destructuring message; `:clojure.string/x` (library namespace, the clojure JAR is on the cached classpath) is refused; an unqualified `:id` is refused; a rename after an unsaved `didChange` that moves the keyword edits the shifted range; a namespaced-map key (`#:readx.db{:db 1}`) is covered only if the extractor already records it as `:readx.db/db`, otherwise it is out of scope and noted.
- `bb e2e-pulse`: VS Code applies a multi-file `WorkspaceEdit` that includes an `.edn` file, and the test reads both documents back and asserts the new keyword text.

## File Structure

Modify:

- `src/index/extractor.rs`: destructuring entries as keyword occurrences, tests.
- `src/handlers/references.rs`: `RenameTarget::Keyword` with validated sites, scope checks and token checks in `rename_target`, `prepare_rename` keyword range, `rename` keyword branch, tests.
- `src/index/mod.rs`: a `pub fn is_library_namespace(&self, ns: &str) -> bool` helper if none exists (an `NsMeta` whose file is not a project path).
- `tests/test_e2e.rs`: keyword rename tests and the `prepare_rename` keyword test.
- `tests/fixtures/simple_project/src/kw_destructure.clj` (new).
- `README.md`, `AGENTS.md`, `ARCHITECTURE.md` (the "keyword rename is rejected" note), `docs/ROADMAP.md`.

## Tasks

### Task 1: Destructuring entries as keyword occurrences

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`, `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing tests**
  Extractor: `test_namespaced_keys_entries_are_keyword_occurrences` covering the four forms in the design and the `:strs` exclusion, asserting fqn and that `name_range` is the entry symbol. e2e: references on `::db` in a fixture file that destructures it lists the entry.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor namespaced_keys_entries`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  In the walker where the namespaced directive is recognized, push an `Occurrence` per entry alongside the binding it already records. Check that `unused-private-var` and references counts in existing tests still hold; adjust deliberately if a fixture now has one more occurrence.

- [ ] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Record namespaced destructuring entries as keyword occurrences"`

### Task 2: Keyword target, sites, and prepareRename

**Files:**
- Modify: `src/handlers/references.rs`, `src/index/mod.rs`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing tests**
  e2e: `test_e2e_prepare_rename_keyword_returns_name_suffix` on `::db` in `integrant_project/src/readx/db.clj`; `test_e2e_rename_refuses_unqualified_keyword`; `test_e2e_rename_refuses_library_keyword` (`:clojure.string/x` typed into an open buffer of `simple_project`).

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e rename_keyword`
  Expected: FAIL (keywords still refused with the old message).

- [ ] **Step 3: Implement**
  `RenameTarget::Keyword { fqn, sites }`, the two scope rules, the project-path filter, and the site collection with token checks in `rename_target` (`fn name_suffix_range(range: Range, token: &str, name: &str) -> Option<Range>`, UTF-16 lengths), `is_library_namespace`, and `prepare_rename` answering with the site at the cursor. Add `test_e2e_prepare_rename_refuses_keys_destructuring` here as well.

- [ ] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS; existing `rename` tests for vars and locals unchanged.

- [ ] **Step 5: Commit**
  `git commit -m "Accept qualified project keywords as rename targets"`

### Task 3: Keyword edits

**Files:**
- Modify: `src/handlers/references.rs`
- Test: `tests/test_e2e.rs`, `tests/fixtures/simple_project/src/kw_destructure.clj`

- [ ] **Step 1: Write the failing tests**
  Unit tests for `name_suffix_range` on `::db`, `::ig/db`, `:readx.db/db`, and a bare `db` token (returns `None`). e2e: `test_e2e_rename_keyword_across_clj_and_edn` (the integrant case from the design, asserting every edit's range and text), `test_e2e_rename_keyword_refuses_colon_in_new_name`, `test_e2e_rename_keyword_refuses_keys_destructuring` with the new fixture file.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e rename_keyword`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  The `Keyword` branch of `rename`: validate the new name, map `sites` to `TextEdit`s grouped by URI, return the `WorkspaceEdit`. All site validation already happened in `rename_target`.

- [ ] **Step 4: Run the tests**
  Run: `bb check && bb e2e && bb e2e-pulse`
  Expected: PASS. In the Pulse gate, add a check that renames `::db` in the fixture through `vscode.executeDocumentRenameProvider`, applies the returned `WorkspaceEdit` with `vscode.workspace.applyEdit`, then reads both documents back and asserts `::store` in the `.clj` and `:readx.db/store` in the `.edn` (the Pulse fixture needs an Integrant-style pair; copy the two files from `integrant_project` into `scripts/pulse-e2e/fixture/`).

- [ ] **Step 5: Commit**
  `git commit -m "Rename qualified keywords across Clojure and EDN files"`

### Task 4: Docs and roadmap

**Files:**
- Modify: `README.md`, `AGENTS.md`, `ARCHITECTURE.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Update docs**
  README Rename bullet: qualified keywords, every notation, EDN included, what is refused. AGENTS.md invariants: replace the "keyword rename is rejected" sentence with the suffix rule and the two refusals. ARCHITECTURE keyword section likewise. ROADMAP Milestone 3: tick keyword rename, set `Plan:` to `done`. Use /writing-clearly.

- [ ] **Step 2: Verify and commit**
  Run: `bb check`
  Expected: PASS.
  `git commit -m "Document keyword rename"`
