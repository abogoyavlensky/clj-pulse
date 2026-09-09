# Keyword Rename Implementation Plan

**Status: complete** (2026-09-09, branch `keyword-rename`).

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

- [x] **Step 1: Write the failing tests**
  Extractor: `test_namespaced_keys_entries_are_keyword_occurrences` covering the four forms in the design and the `:strs` exclusion, asserting fqn and that `name_range` is the entry symbol. e2e: references on `::db` in a fixture file that destructures it lists the entry.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor namespaced_keys_entries`
  Expected: FAIL.

- [x] **Step 3: Implement**
  In the walker where the namespaced directive is recognized, push an `Occurrence` per entry alongside the binding it already records. Check that `unused-private-var` and references counts in existing tests still hold; adjust deliberately if a fixture now has one more occurrence.

- [x] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Record namespaced destructuring entries as keyword occurrences"`

> Deviation: `:syms` vectors are excluded alongside `:strs`. `clojure.core/destructure`
> reads a `:syms` entry as a quoted *symbol* key, so `{::syms [a]}` never reads the
> keyword `::a` — recording it would put unrelated sites in find-references and make
> keyword rename refuse for a site it does not touch. Only `:keys` is recorded.
>
> Deviation: when both the directive and the entry are qualified, the directive's
> namespace wins — `destructure` builds the key as
> `(keyword (or directive-ns (namespace entry)) (name entry))`, so `{:foo/keys [bar/a]}`
> reads `:foo/a`. Both found by the codex review of the task commit.

### Task 2: Keyword target, sites, and prepareRename

**Files:**
- Modify: `src/handlers/references.rs`, `src/index/mod.rs`
- Test: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing tests**
  e2e: `test_e2e_prepare_rename_keyword_returns_name_suffix` on `::db` in `integrant_project/src/readx/db.clj`; `test_e2e_rename_refuses_unqualified_keyword`; `test_e2e_rename_refuses_library_keyword` (`:clojure.string/x` typed into an open buffer of `simple_project`).

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e rename_keyword`
  Expected: FAIL (keywords still refused with the old message).

- [x] **Step 3: Implement**
  `RenameTarget::Keyword { fqn, sites }`, the two scope rules, the project-path filter, and the site collection with token checks in `rename_target` (`fn name_suffix_range(range: Range, token: &str, name: &str) -> Option<Range>`, UTF-16 lengths), `is_library_namespace`, and `prepare_rename` answering with the site at the cursor. Add `test_e2e_prepare_rename_refuses_keys_destructuring` here as well.

- [x] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS; existing `rename` tests for vars and locals unchanged.

- [x] **Step 5: Commit**
  `git commit -m "Accept qualified project keywords as rename targets"`

> Deviation: the `Keyword` branch of `rename` and the colon-in-new-name message
> (Task 3, step 3) landed here — the `match` on `RenameTarget` does not compile
> without the branch, and Task 2's own refusal tests go through `rename`. Task 3
> keeps its tests, the fixture and the Pulse gate.
>
> Deviation: `tests/fixtures/simple_project/src/kw_destructure.clj` was added here
> rather than in Task 3, since `test_e2e_prepare_rename_refuses_keys_destructuring`
> needs it.
>
> Deviation: `RenameTarget::Keyword` carries `sites: Vec<KeywordSite>` — each site
> holds the whole token range as well as the name sub-range — instead of
> `{ fqn, sites: Vec<(Url, Range)> }`. `prepareRename` needs the token range: a
> cursor on the `::` marker sits outside the suffix the edit replaces. `fqn` had no
> reader left once the refusals moved into `rename_target`.
>
> Deviation: `test_e2e_prepare_rename_rejects_what_rename_rejects` used
> `::cfg/port` as its keyword case, which is now renameable; it checks the
> unqualified `:id` instead.
>
> Deviation (codex review): a keyword token must now *start* with `:` as well as
> end with the name. A qualified destructuring entry (`{:keys [app/id]}`) ends
> with `/id` but binds the local `id`, so rewriting its suffix would rename the
> binding and orphan every usage of it; it is refused with the destructuring
> message, like the bare entry.
>
> Deviation (codex review): definition sites are collected from every open
> project buffer, not only from `index.lookup`. A dispatch keyword typed but not
> yet saved has no indexed symbol and is not an occurrence, so it would have been
> left dispatching on the old key — and, with the cursor on it, `prepareRename`
> would have refused what `rename` accepts.

### Task 3: Keyword edits

**Files:**
- Modify: `src/handlers/references.rs`
- Test: `tests/test_e2e.rs`, `tests/fixtures/simple_project/src/kw_destructure.clj`

- [x] **Step 1: Write the failing tests**
  Unit tests for `name_suffix_range` on `::db`, `::ig/db`, `:readx.db/db`, and a bare `db` token (returns `None`). e2e: `test_e2e_rename_keyword_across_clj_and_edn` (the integrant case from the design, asserting every edit's range and text), `test_e2e_rename_keyword_refuses_colon_in_new_name`, `test_e2e_rename_keyword_refuses_keys_destructuring` with the new fixture file.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e rename_keyword`
  Expected: FAIL.

- [x] **Step 3: Implement**
  The `Keyword` branch of `rename`: validate the new name, map `sites` to `TextEdit`s grouped by URI, return the `WorkspaceEdit`. All site validation already happened in `rename_target`.

- [x] **Step 4: Run the tests**
  Run: `bb check && bb e2e && bb e2e-pulse`
  Expected: PASS. In the Pulse gate, add a check that renames `::db` in the fixture through `vscode.executeDocumentRenameProvider`, applies the returned `WorkspaceEdit` with `vscode.workspace.applyEdit`, then reads both documents back and asserts `::store` in the `.clj` and `:readx.db/store` in the `.edn` (the Pulse fixture needs an Integrant-style pair; copy the two files from `integrant_project` into `scripts/pulse-e2e/fixture/`).

- [x] **Step 5: Commit**
  `git commit -m "Rename qualified keywords across Clojure and EDN files"`

> Deviation: `test_e2e_rename_keyword_refuses_keys_destructuring` is folded into
> `test_e2e_prepare_rename_refuses_keys_destructuring`, which asserts that
> `rename` and `prepareRename` refuse with the *same* message — the invariant
> worth pinning. Two extra e2e tests came out of the codex reviews:
> `test_e2e_rename_refuses_qualified_keys_destructuring` and
> `test_e2e_rename_keyword_sees_unsaved_definition`.
>
> Deviation: the namespaced-map key case is in scope after all —
> `record_ns_map_key` already records `#:readx.db{:db 1}` as `:readx.db/db`, so
> `test_e2e_rename_keyword_rewrites_namespaced_map_keys` covers it.
>
> Deviation (codex review): `extract_edn` now applies namespaced-map prefixes
> too (`collect_edn_ns_map`). It did not, so `#:my.app{:db …}` in an Integrant
> config was invisible to references *and* to rename — the silent-miss the
> all-or-nothing rule exists to prevent, on the very file type this plan is
> about. Pre-existing, fixed here because the feature makes it user-visible.
>
> Two assertions codex called weak were tightened: the unsaved-edit test now
> pins all three edits to exact shifted ranges, and the `#ig/ref` assertion pins
> the edit column rather than only its line.
>
> Deviation (codex review, round 3): the Integrant-config gate
> (`has_namespaced_top_level_key`) only recognised `map_lit`, so a ref-less
> config written as `#:my.app{…}` was never scanned at all — the previous fix
> would have been dead code for exactly that file. A top-level namespaced map
> with a literal prefix now counts as the signature.

### Task 4: Docs and roadmap

**Files:**
- Modify: `README.md`, `AGENTS.md`, `ARCHITECTURE.md`, `docs/ROADMAP.md`

- [x] **Step 1: Update docs**
  README Rename bullet: qualified keywords, every notation, EDN included, what is refused. AGENTS.md invariants: replace the "keyword rename is rejected" sentence with the suffix rule and the two refusals. ARCHITECTURE keyword section likewise. ROADMAP Milestone 3: tick keyword rename, set `Plan:` to `done`. Use /writing-clearly.

- [x] **Step 2: Verify and commit**
  Run: `bb check`
  Expected: PASS.
  `git commit -m "Document keyword rename"`

---

## Completion summary

All four tasks are done. `bb check` (448 unit + 162 e2e), `bb e2e`, `bb e2e-real`,
`bb e2e-pulse`, `bb e2e-calva`, `bb e2e-nvim` and `bb bench` all pass.

**What shipped.** A qualified project keyword renames across the whole project.
Each site is rewritten in the notation it was written in, because the edit
replaces only the name the token ends with: `::db`, `::alias/db` and
`:my.app/db` become `::store`, `::alias/store` and `:my.app/store`. Integrant
`config.edn` files are rewritten with the sources, `#ig/ref` values and
namespaced-map keys included. Sites come from occurrences, the `IntegrantKey`
definition, and the live definitions of every open buffer. `rename_target` holds
every refusal that does not need the new name, so `prepareRename` refuses
exactly what `rename` would: unqualified keywords, keywords of a library
namespace, and any site a suffix edit cannot rewrite (a `{::keys [db]}` or
`{:keys [app/db]}` entry, which reads the key while binding a local of that
name). A refusal is whole; nothing is half-renamed.

**Verified by hand.** The real binary was driven over stdio against a copy of
the Integrant fixture: renaming `::db` from its `assert-key` defmethod returned
7 edits across 3 files, and applying them produced exactly the three notations
rewritten in place with the unqualified `:db` key in `config.edn` untouched. The
same project with a `{::db/keys [db]}` consumer refused with the destructuring
message instead.

**Issues encountered.** All were found by the codex review checkpoints, none by
the plan's own tests:

1. `:syms` reads quoted symbol keys, not keywords, so recording its entries as
   keyword occurrences was wrong (the plan asked for it).
2. `clojure.core/destructure` builds the key as
   `(keyword (or directive-ns (namespace entry)) (name entry))`, so a qualified
   directive wins over a qualified entry. The first implementation had it
   backwards.
3. A qualified `:keys` entry (`{:keys [app/id]}`) *ends* with the name, so the
   suffix rule accepted it and would have renamed the local binding while
   leaving its usages behind. A keyword token must also *start* with `:`.
4. A dispatch keyword typed but not yet saved has no indexed symbol and is not
   an occurrence, so it was missed — and with the cursor on it, `prepareRename`
   refused what `rename` accepted.
5. `extract_edn` never applied namespaced-map prefixes, so `#:my.app{:db …}` in
   an Integrant config was invisible to references *and* rename. Its gate
   (`has_namespaced_top_level_key`) did not recognise such a config either, so
   fixing one without the other would have been dead code.

Every deviation note is inline under its task above.

**What the plan could have specified better.** The design derived the suffix
rule from what keyword notations have in common (they all *end* with the name)
and never stated what they have in common at the front — that a keyword token
always *starts* with a colon. That missing half is what let a qualified
destructuring entry through, and it is also what made the plan treat
`token == name` as the way to recognise a destructuring site. A rule stated from
both ends would have been correct as written. Relatedly, the plan asserted the
extractor facts it depended on ("`:syms` entries read the same key", "the
`IntegrantKey` symbol is also an occurrence", "namespaced-map keys are already
recorded") without pinning any of them to a test or a line of code; three of the
five issues above are one of those assertions being false.
