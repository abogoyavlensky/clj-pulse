# ClojureScript Core, `:require-macros` and Twin Namespaces Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `.cljs` file resolves its core to `cljs.core`, reads macros brought in through `:require-macros` / `:refer-macros`, and a namespace split across a `.clj` and a `.cljs` file keeps both halves indexed and navigable — closing the three ClojureScript Backlog items of 2026-09-17.

**Tech Stack:** Rust, tower-lsp 0.20, tree-sitter-clojure, DashMap. Tests: unit tests in `src/index/mod.rs`, `tests/test_extractor.rs`, `tests/test_e2e.rs`, the `bb compare` oracle in `tests/common/oracle.rs` / `tests/test_compare.rs`.

---

## Design

### What clj-kondo's analysis says (ground truth, measured 2026-09-27)

A baseline `bb compare clj-kondo` (log kept at `.tmp/compare-baseline.log`, 58 s) and clj-kondo run by hand on the corpus files behind each Backlog item:

- **`.cljs` core.** Kondo attributes every bare core name in a `.cljs` file to `cljs.core`; the oracle then expects `<library>/cljs/core.{cljs,cljc}`. The ClojureScript JAR (`clojurescript-1.12.145.jar` on the corpus classpath) ships `cljs/core.cljs` (the fns: `not` at line 262) and `cljs/core.cljc` (the macros, written `(core/defmacro when …)`, which `qualified_head_def_kind` already reads as `Defmacro`). Both files declare `(ns cljs.core …)`. Today `var-usage/library` is 5 agree / 118 diverge / 44 null, all of them `.cljs` sites under `inlined/…/cljs/tools/reader/`: the 118 land in `clojure-1.11.4.jar!/clojure/core.clj`, the 44 are special forms (`if`, `do`, `recur`, `==` through `js*`) that kondo files under `cljs.core` and that have no source anywhere.
- **`:require-macros`.** The corpus has one shape, `(:require-macros [… reader-types :refer [log-source]])` in `reader.cljs:14`, with the same namespace also in `:require`. `log-source` (a `defmacro` in `reader_types.clj:3`) answers 1 site where kondo has 4 (`reader.cljs:14`, `:397`, `:864` plus the definition), for references and rename alike — the whole `var-def/defmacro` divergence.
- **Twins.** `reader_types.clj` (one macro) and `reader_types.cljs` (the runtime namespace) share one ns name. The "extra site" clj-pulse answers for `source-logging-reader?` is `reader_types.clj:7`, inside the **syntax-quoted body** of `log-source`. Kondo resolves nothing there (`to: clj-kondo/unknown-namespace` for every symbol of the template, because the `.clj` namespace defines no such var). Semantically it *is* a usage of the cljs var — the macro emits a call to it, and renaming the var without editing the template breaks the macro — so clj-pulse keeps the site and the compare gate allowlists it.

  The twin item's real damage is in the index, not the answer set: `Index::ns_symbols` and `Index::namespaces` are keyed per namespace name, so with two files of one namespace (a) `remove_file` on one twin (every save, every watcher event) deletes the *other* twin's symbols and ns metadata until a rescan, (b) the losing twin's outline, completion pool and bare-word resolution (`resolve_symbol` reads `ns_meta(current_ns)`) use the winner's aliases and refers, and (c) a name both twins define — the usual platform-specific `(defn foo …)` pair — navigates to whichever file indexed last. Kondo's oracle keys definitions by `(ns, name)` too, so (c) is invisible to `bb compare` and is verified by e2e alone.

### 1. `cljs.core` is the core namespace of a `.cljs` file

- `OccurrenceCtx` (`src/index/extractor.rs`) gains `dialect: Dialect`, computed once from `ns_meta.file` with `Dialect::of_path`. A private `fn core_ns(dialect: Dialect) -> &'static str` in `src/index/mod.rs` (next to `Dialect`) answers `"cljs.core"` for `Cljs` and `"clojure.core"` otherwise; the extractor, the resolver and the definition handler all call it, so the string lives in one place.
- `record_occurrence`: a bare core name resolves to `{core_ns}/{name}`; a qualified usage whose namespace (after alias resolution) is `clojure.core` in a `Cljs` file is recorded under `cljs.core` (ClojureScript aliases `clojure.core` to `cljs.core`). `head_is_core_form` accepts `cljs.core` as well as `clojure.core`. `parse_refer_clojure` writes `:rename` refers under `core_ns(Dialect::of_path(&ns_meta.file))`; `resolve_symbol`'s renamed-core hover fallback strips either prefix.
- `handlers/definition.rs`, `Core` arm: `lookup_in_ns(core_ns(dialect), &core.name)`, and when that is `None` for `Cljs`, `lookup_in_ns("clojure.core", …)` — the floor is today's behavior for a project with no ClojureScript JAR on the classpath. The position-aware path needs nothing: the occurrence now says `cljs.core/not`, and `lookup_for` finds it in the primary slot when the JAR is indexed, or falls through to the bare-word resolver when it is not.
- `.cljc` is `Dialect::Clj` (`Dialect::of_path` is unchanged) and keeps recording `clojure.core`. Hover keeps showing the static `clojure.core` docstring for either dialect.
- Oracle: `NO_SOURCE_DEFINITION` in `tests/common/oracle.rs` applies to `u.to == "cljs.core"` as well as `clojure.core`, so a special form is not a probe in either dialect.

### 2. `:require-macros`, `:refer-macros`, `:include-macros`

- `extract_ns`: a `(:require-macros …)` clause runs every spec through `process_require_spec`, exactly like `:require` — aliases, refers, `requires`, reader conditionals, prefix lists. `parse_libspec_items` pushes the namespace onto `requires` only when it is not already there, since `reader.cljs` names the same namespace in both clauses and a duplicate entry would surface twice in every consumer of `requires`.
- `parse_libspec_items`: `:refer-macros [m …]` records refers like `:refer`; `:include-macros true` records nothing (the namespace is already required). Both may appear inside an ordinary `:require` libspec.
- `collect_refer_occurrences` (refer entries are occurrences, so rename rewrites the clause) reads `:require-macros` clauses and `:refer-macros` keys as well; `collect_alias_declarations` (alias-rename sites) accepts a `:require-macros` clause next to `:require` and `:use`. The clean-ns and add-require code actions and the three tree-based lints (`unused_requires_tree`, `duplicate_requires_tree`, the unresolved-namespace check) keep scanning `:require` clauses only — a `:require-macros` clause is neither cleaned, deduplicated nor reported, which is today's behavior and out of scope.
- `CACHE_FORMAT_VERSION` (`src/index/jar_cache.rs`) bumps: the cached `NsMeta` of a `.cljs` JAR entry changes.

### 3. Twins: one fqn, two dialect slots, file-owned removal

The library rule of 2026-09-17 (Clojure-preferred primary, ClojureScript copy in `cljs_symbols` / `cljs_namespaces`) is generalized to project files instead of adding a second mechanism.

**One rank for both owners.** `fn slot_rank(source: &SymbolSource, file: &Path) -> u8`: project `.clj` 0, project other (`.cljc`, `.lg`, no `ns`) 1, project `.cljs` 2, library `.clj` 3, library other 4, library `.cljs` 5. A namespace entry ranks by whether its file is a project path (`is_project_path`) and its extension. `rank_insert` keeps its shape with two amendments: the `old_rank` closure never answers `None` (project entries rank, they are no longer skipped), and a `.cljs` entry (rank 2 or 5) that loses the primary, or is displaced from it by a non-`.cljs` entry, goes to the shadow only when the shadow is vacant or holds a rank not lower than its own — a library `.cljs` never evicts a project `.cljs` from the shadow. Project beats library in both slots. `insert_lib_file`'s `ns_owned_by_project` early return stays (it also keeps `ns_symbols` / `file_to_ns` clean).

**Reads.** `lookup_for(fqn, Cljs)`, `prefer_dialect` and `ns_meta_for(ns, Cljs)`: the shadow wins unless the primary is project-owned and the shadow is not. New `pub fn lookup_all(&self, fqn: &str) -> Vec<Symbol>`: primary then shadow, only when their files differ — every definition of the fqn, in both dialects.

**File-owned removal.** New field `files: DashMap<PathBuf, FileRecord>` with `pub struct FileRecord { pub meta: NsMeta, pub fqns: Vec<String> }` — what each source file (project or library) contributed, whatever the slots did with it. The record keeps its own `NsMeta` because the slots cannot: two same-dialect files of one namespace (two projects both defining `user`) or a `.clj`/`.cljc` pair keep one metadata entry between them, and today the loser's is simply gone. `remove_file(path)`:
1. for each fqn in `files.remove(path)`: remove the primary entry when its `file == path`, then, if the primary is now empty and the shadow holds the fqn, move the shadow entry into the primary (a `.cljs`-only twin resolves for every asker, as a `.cljs`-only library already does); else remove the shadow entry when its `file == path`;
2. the namespace pair (`namespaces` / `cljs_namespaces`) by the same rule, keyed by `file_to_ns.remove(path)`;
3. `ns_symbols[ns]` retains the fqns still present in either symbol map, and the entry is dropped when that leaves it empty **and** no other file maps to the ns in `file_to_ns`;
4. occurrences and keyword counts as today.

`insert_file` and `insert_lib_file` route every symbol and the ns entry through `rank_insert`, record `files[file]`, and *extend* `ns_symbols[ns]` (de-duplicated) instead of replacing it. Twins share one `ns_symbols` list, so completion's current-namespace pool sees both halves; the closed-file `documentSymbol` fallback in `src/handlers/symbols.rs` resolves each fqn with `lookup_all` before filtering by `sym.file`, since a shared name's `.cljs` copy sits in the shadow.

**Merge and clear.** `merge_project_from(new_index, keep)` becomes: stale files (as today, by `occurrences` keys) → `remove_file`; then for every file of `new_index` (a new `Index::file_entries(&self) -> Vec<FileEntry>` yields `FileEntry::Source { meta, symbols, occurrences }` — `meta` from the file's `FileRecord`, `symbols` the record's fqns resolved through both symbol maps and kept when `sym.file` is this file — and `FileEntry::Edn { file, occurrences }` for `EDN_NS_SENTINEL` files) → `remove_file` then `insert_file` / `insert_edn_file`. The ns-keyed splicing goes away. A same-dialect loser's *shared* fqn is not in either map and is not reconstructed, which is exactly what the index held before the merge; its unique symbols and its metadata survive, which today's merge also loses only through `ns_symbols`. `clear_libs` retains project entries in `cljs_symbols` and `cljs_namespaces` (by `SymbolSource::Project` and `is_project_path`) and drops library paths from `file_symbols`; the rest of its retain chain is unchanged.

**Who asks with a dialect.** `resolve_symbol(index, word, current_ns, dialect)` reads `ns_meta_for(current_ns, dialect)` and resolves through `lookup_for` (a private `lookup_in_ns_for` helper) so the `.cljs` twin's aliases, refers and definitions answer in a `.cljs` buffer; its four callers (`definition`, `hover`, `signature`, `clojuredocs`) pass `Dialect::of_path(&path)`, and hover's `prefer_dialect` in the `Project` arm becomes redundant and is removed. `resolve_fqn_at`'s alias fallback and `namespace_location`'s alias table read `ns_meta_for(current_ns, dialect)`. Completion's `ns_meta(current_ns)` and its `index.symbols.get(fqn)` reads in the current-namespace and alias pools switch to the `_for` variants. `references` lists every project definition from `lookup_all` when `include_declaration` is set; `rename` builds a declaration edit for every project symbol in `lookup_all(fqn)` (each through `live_definition_range`), with `RenameTarget::Global` carrying the one `rename_target` resolved for its source check. Occurrence sites stay merged across dialects — the twins are one logical var, kondo merges them too, and renaming half of a var is never what anyone wants.

**Warnings.** The scanner's and `warn_ns_collisions`' "namespace X defined in both A and B; last one wins" lines fire only when `Dialect::of_path` agrees for both files: a `.clj`/`.cljs` pair is a design, not a collision.

**Compare gate.** `Verdict::Diverge` gains `extra_files: BTreeSet<PathBuf>` — the files of the sites only clj-pulse answered — so a `KNOWN` matcher reads structure, not the `got` string. The entry: bucket prefix `var-def/`, matching a probe whose file is `.cljs` with `Diverge { missing: 0, extra_files, .. }` where `extra_files` is non-empty and every file in it is the probe file's `.clj` twin (same parent directory and file stem, extension `clj`). Any other extra `.clj` site stays a divergence. Reason "a `.cljs` var used in the syntax-quoted template of its `.clj` macro twin is a site in clj-pulse; kondo resolves nothing inside that template (2026-09-27)".

### Out of scope

shadow-cljs classpaths, `goog.*` prefixes, `.cljc` answering as both dialects (the corpus has no `.cljc` source; `.cljc` keeps asking as Clojure), clean-ns / lints over `:require-macros` clauses, `bb bench`'s `Expect::Archive` (still accepts either dialect, because clojure-lsp is judged through it too).

### Expected `bb compare clj-kondo` movement

Against the table in `docs/MEMORY.md` (2026-09-18): `var-usage/library` and `var-usage/library/macro` lose their diverge and null columns (the 44 special-form nulls stop being probes, so the probe count drops); `var-def/defmacro` 8 → 0 diverge; `var-def/defn` and `var-def/defprotocol` move the `reader_types` sites from diverge to known. Everything else within the run-to-run noise the memo describes.

## File Structure

- Modify: `src/index/mod.rs` — `core_ns`, `slot_rank` (replaces `lib_rank`), amended `rank_insert`, `files` / `FileRecord`, `lookup_all`, `FileEntry` + `file_entries`, rewritten `remove_file` / `insert_file` / `insert_lib_file` / `merge_project_from` / `clear_libs`, dialect rule in `lookup_for` / `prefer_dialect` / `ns_meta_for`; unit tests.
- Modify: `src/index/extractor.rs` — `OccurrenceCtx.dialect`, `record_occurrence`, `head_is_core_form`, `parse_refer_clojure`, `extract_ns` (`:require-macros`), `parse_libspec_items` (`:refer-macros`, `:include-macros`, de-duplicated `requires`), `collect_refer_occurrences`, `collect_alias_declarations`.
- Modify: `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` 18 → 19.
- Modify: `src/index/scanner.rs`, `src/server.rs` — collision warning skips twin pairs.
- Modify: `src/handlers/mod.rs` — `resolve_symbol` takes `Dialect`; `lookup_in_ns_for`.
- Modify: `src/handlers/definition.rs`, `hover.rs`, `signature.rs`, `clojuredocs.rs`, `references.rs`, `completion.rs`, `symbols.rs` — dialect-aware reads; `lookup_all` in references, rename and the closed-file outline.
- Modify: `tests/common/oracle.rs`, `tests/test_compare.rs` — `cljs.core` special forms, `Verdict::Diverge.extra_files`, the `KNOWN` entry.
- Modify: `tests/test_extractor.rs`, `tests/test_e2e.rs`, `tests/test_definition.rs`, `tests/test_hover.rs`, `tests/test_jar_definition.rs` — new tests; call-site updates.
- Modify: `docs/ROADMAP.md`, `AGENTS.md`, `docs/FEATURES.md`, `README.md`, `docs/MEMORY.md`; move the three Backlog files to `docs/archive/` (where the discards issue went).

## Tasks

### Task 1: Promote the roadmap item

**Files:**
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Move the three Backlog lines into Milestone 5**
  Delete the three 2026-09-17 Backlog bullets (`.cljs` core, `:require-macros`, twins) and add one unticked item directly above **Release** in Milestone 5: **ClojureScript: `cljs.core`, `:require-macros`, twin namespaces.** Two or three sentences from the Design's ground-truth paragraph (what each item was, the `var-usage/library` numbers), the three backlog links, and `Plan: [2026-09-27-2031-clojurescript-core-macros-and-twins.md](plans/2026-09-27-2031-clojurescript-core-macros-and-twins.md) — in progress`.

- [ ] **Step 2: Commit**
  `git add docs && git commit -m "Plan ClojureScript core, require-macros and twin namespaces"`

### Task 2: `cljs.core` occurrences and definitions

**Files:**
- Modify: `src/index/mod.rs` (`core_ns`), `src/index/extractor.rs`, `src/handlers/mod.rs`, `src/handlers/definition.rs`
- Test: `tests/test_extractor.rs`, `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing extractor tests**
  In `tests/test_extractor.rs`, next to `test_occurrence_refer_usage_and_vector_entry`:
  - `cljs_core_names_resolve_to_cljs_core`: extract `(ns x)\n(not (when true 1))\n` at path `x.cljs`; occurrences hold `cljs.core/not` and `cljs.core/when` and no `clojure.core/…`. The same source at `x.clj` and at `x.cljc` yields `clojure.core/not`.
  - `cljs_qualified_clojure_core_is_cljs_core`: `(ns x (:require [clojure.core :as cc]))\n(cc/inc 1)\n(clojure.core/dec 1)\n` at `x.cljs` → `cljs.core/inc` and `cljs.core/dec`.
  - `cljs_refer_clojure_rename_refers_cljs_core`: `(ns x (:refer-clojure :rename {map cmap}))` at `x.cljs` → `refers["cmap"] == "cljs.core/map"`.

- [ ] **Step 2: Run to verify they fail**
  Run: `cargo test --test test_extractor cljs_ -- --nocapture`
  Expected: the three new tests FAIL on the `clojure.core` fqns.

- [ ] **Step 3: Implement**
  `src/index/mod.rs`: `pub fn core_ns(dialect: Dialect) -> &'static str` beside `Dialect`.
  `src/index/extractor.rs`: `dialect` on `OccurrenceCtx` (set where the ctx is built from `ns_meta.file`); `record_occurrence` bare-core branch and the qualified branch's `clojure.core` → `cljs.core` rewrite for `Cljs`; `head_is_core_form` accepts either core ns; `parse_refer_clojure` uses `core_ns(Dialect::of_path(&ns_meta.file))`.
  `src/handlers/mod.rs`: the renamed-core fallback in `resolve_symbol` strips `clojure.core/` or `cljs.core/`.
  `src/handlers/definition.rs`, `Core` arm: `core_ns(dialect)` first, `clojure.core` as the `Cljs` floor.

- [ ] **Step 4: Run to verify they pass**
  Run: `cargo test --test test_extractor && cargo test --lib`
  Expected: PASS.

- [ ] **Step 5: Write the failing e2e test**
  In `tests/test_e2e.rs`, extend `clojure_and_clojurescript_jars_project` (or add a sibling `core_jars_project`) so `clojure-x.jar` also carries `clojure/core.clj` with `(ns clojure.core)\n(defn not [x] x)\n(defmacro when [t & b] nil)\n` and `clojurescript-x.jar` carries `cljs/core.cljs` with `(ns cljs.core)\n(defn not [x] x)\n` and `cljs/core.cljc` with `(ns cljs.core)\n(core/defmacro when [t & b] nil)\n`. Consumers: `src/uses_core.clj` and `src/uses_core.cljs`, each `(ns uses-core)\n(not (when true 1))\n`.
  `test_e2e_cljs_core_navigates_into_the_clojurescript_jar`, both classpath orders: `initialize`, `wait_for_log("library indexing complete")`, open the `.cljs` consumer; definition on `not` ends with `!/cljs/core.cljs`, on `when` with `!/cljs/core.cljc`; open the `.clj` consumer; both end with `!/clojure/core.clj`.

- [ ] **Step 6: Run, verify fail then pass**
  Run: `cargo test --test test_e2e cljs_core -- --nocapture`
  Expected: FAIL before the Step 3 change is in place for the `Core` arm, PASS after. (If Step 3 is already complete, it passes at once; note that in the plan.)

- [ ] **Step 7: Commit**
  `git commit -am "Resolve the core of a .cljs file to cljs.core"`

### Task 3: `:require-macros`, `:refer-macros`, `:include-macros`

**Files:**
- Modify: `src/index/extractor.rs`, `src/index/jar_cache.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write the failing tests**
  In `tests/test_extractor.rs`, next to `test_ns_refer_all_and_use_recorded`:
  - `require_macros_binds_aliases_and_refers`: `(ns x (:require-macros [a.macros :as m :refer [defthing]]) (:require [a.macros :refer [helper]]))` at `x.cljs`; `aliases["m"] == "a.macros"`, `refers["defthing"] == "a.macros/defthing"`, `refers["helper"]` present, `requires` contains `a.macros` exactly once.
  - `refer_macros_and_include_macros_in_a_require_spec`: `(ns x (:require [a.macros :refer-macros [defthing] :include-macros true]))` → `refers["defthing"]`, `requires == ["a.macros"]`.
  - `require_macros_usage_is_an_occurrence`: the first ns form followed by `(defthing foo 1)\n(m/other 2)`; occurrences hold `a.macros/defthing` twice (the refer entry in the clause and the usage) and `a.macros/other`.
  - `require_macros_alias_is_a_rename_site`: whatever the existing alias-site test uses (`alias_sites_tree`), with the alias bound by `:require-macros` — the declaration site is found.

- [ ] **Step 2: Run to verify they fail**
  Run: `cargo test --test test_extractor macros -- --nocapture`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  `extract_ns`: `":require-macros"` arm identical to `":require"`. `parse_libspec_items`: `:refer-macros` vector → refers; `:include-macros` + next item skipped; the final `requires.push` guarded by `!contains`. `collect_refer_occurrences`: accept `:require-macros` clauses and `:refer-macros` keys. `collect_alias_declarations`: add `":require-macros"` to the clause match. Bump `CACHE_FORMAT_VERSION` to 19 and its test if it pins the number.

- [ ] **Step 4: Run to verify they pass**
  Run: `cargo test --test test_extractor && cargo test --lib`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -am "Read :require-macros, :refer-macros and :include-macros"`

### Task 4: Index — ranks for project files, file-owned removal

**Files:**
- Modify: `src/index/mod.rs`
- Test: `src/index/mod.rs` (`mod tests`)

- [ ] **Step 1: Write the failing unit tests**
  Add a `project_symbol(fqn, file)` helper beside `lib_symbol`, and a `project_meta(ns, file)`. Insert project files through `insert_file` with a non-empty occurrences vector (so `is_project_path` holds). Tests:
  - `project_twins_keep_both_definitions`: `insert_file` `a/b.clj` defining `a.b/foo` and `a.b/clj-only`, then `a/b.cljs` defining `a.b/foo` and `a.b/cljs-only`, and the reverse order in a second index. In both: `lookup("a.b/foo")` is the `.clj` copy, `lookup_for("a.b/foo", Cljs)` the `.cljs` copy, `lookup_for("a.b/cljs-only", Clj)` resolves (no twin, primary), `lookup_all("a.b/foo")` has two files, `ns_meta("a.b")` is the `.clj` file and `ns_meta_for("a.b", Cljs)` the `.cljs` one, `ns_symbols["a.b"]` holds all three fqns.
  - `removing_one_twin_keeps_the_other`: after both inserted, `remove_file(a/b.clj)`: `lookup("a.b/foo")` is now the `.cljs` copy (promoted), `lookup("a.b/clj-only")` is `None`, `ns_meta("a.b")` is the `.cljs` file, `ns_symbols["a.b"]` has two fqns. Re-insert `a/b.clj`: the `.clj` copy is primary again and `lookup_for(Cljs)` the `.cljs` one. Then `remove_file(a/b.cljs)`: `lookup_for("a.b/foo", Cljs)` is the `.clj` copy, `cljs-only` is gone.
  - `project_cljs_twin_beats_a_library_in_both_slots`: library `.clj` and `.cljs` copies of `a.b/foo` inserted first, then project `a/b.cljs`: `lookup("a.b/foo")` and `lookup_for(Cljs)` both answer the project file. Then project `a/b.clj`: `lookup` is project `.clj`, `lookup_for(Cljs)` project `.cljs`. A library `.cljs` inserted last does not reach either slot.
  - `merge_project_from_replaces_each_file_and_keeps_twins`: index with both twins; a `new_index` where `a/b.clj` defines `a.b/foo` only (dropped `clj-only`) and `a/b.cljs` is unchanged; after merge, `clj-only` is gone, `cljs-only` and both `foo` copies remain, occurrences of both files present.
  - `clear_libs_keeps_project_entries_in_the_shadow`: project twins plus library copies; after `clear_libs`, `lookup_for("a.b/foo", Cljs)` is still the project `.cljs` copy; the library `.cljs`-only symbol is gone.
  - `file_entries_round_trip`: `file_entries()` of an index with a `.clj`, a `.cljs` twin and an EDN file yields three entries whose metadata, symbols and occurrences match what was inserted.
  - `same_dialect_collision_keeps_the_losers_record`: `a/user.clj` (project A) and `b/user.clj` (project B) both define `user/shared`, and `b/user.clj` alone defines `user/b-only`; `file_entries()` yields both files, the loser's entry with its own `NsMeta` and `b-only`; after `remove_file(a/user.clj)`, `user/b-only` still resolves and `ns_meta("user")` is `b/user.clj`'s.
  Keep every existing test in the module; `project_inserted_after_both_library_copies_still_wins` still holds under the new rule (primary project, shadow library → primary).

- [ ] **Step 2: Run to verify they fail**
  Run: `cargo test --lib index::tests`
  Expected: compile errors for `lookup_all` / `file_entries`; then assertion failures.

- [ ] **Step 3: Implement**
  Per the Design: `slot_rank` replaces `lib_rank` (keep `CLJS_RANK`-style helpers as `is_cljs_rank(r)`: `r % 3 == 2`), the amended `rank_insert`, `files` / `FileRecord`, `lookup_all`, `FileEntry` / `file_entries`, and the rewritten `remove_file`, `insert_file`, `insert_lib_file`, `merge_project_from`, `clear_libs`, `lookup_for`, `prefer_dialect`, `ns_meta_for`. Update the doc comments on the two shadow maps: they hold project entries too now. `Symbol` and `NsMeta` do not change.

- [ ] **Step 4: Run to verify they pass**
  Run: `cargo test --lib && cargo test --test test_index`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -am "Index a namespace split across .clj and .cljs as two dialect slots with file-owned removal"`

### Task 5: Handlers ask with their dialect; references and rename cover both twins

**Files:**
- Modify: `src/handlers/mod.rs`, `definition.rs`, `hover.rs`, `signature.rs`, `clojuredocs.rs`, `references.rs`, `completion.rs`, `symbols.rs`, `src/index/scanner.rs`, `src/server.rs`
- Modify: `tests/test_definition.rs`, `tests/test_hover.rs`, `tests/test_jar_definition.rs` (call sites: pass `Dialect::Clj`)
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing e2e test**
  Helper `twin_namespace_project()`: `setup_project()` plus `src/app/shared.clj` = `(ns app.shared)\n\n(defmacro with-thing [& body] `(do ~@body))\n\n(defn platform [] :clj)\n`, `src/app/shared.cljs` = `(ns app.shared (:require-macros [app.shared :refer [with-thing]]))\n\n(defn platform [] :cljs)\n\n(defn only-cljs [] (with-thing (platform)))\n`, `src/app/use_clj.clj` = `(ns app.use-clj (:require [app.shared :as sh]))\n(sh/platform)\n`, `src/app/use_cljs.cljs` = `(ns app.use-cljs (:require [app.shared :as sh]))\n(sh/platform)\n(sh/only-cljs)\n`.
  `test_e2e_twin_namespaces`: `initialize`, `wait_for_log("Indexed")`; before opening anything, `textDocument/documentSymbol` on `shared.cljs` (closed, so the index fallback answers) lists `platform` and `only-cljs`, and on `shared.clj` lists `with-thing` and `platform`. Then open all four.
  1. definition on `platform` in `use_cljs.cljs` → `shared.cljs` line 2; in `use_clj.clj` → `shared.clj` line 4.
  2. definition on `with-thing` in `shared.cljs` line 4 → `shared.clj` line 2 (the `:require-macros` refer).
  3. references on `platform` from `use_cljs.cljs` with declarations → contains both `shared.clj` and `shared.cljs` definition ranges plus both usages.
  4. rename `platform` → `plat` from `use_cljs.cljs`: the edit set touches four files, both definitions included.
  5. `did_save` of `shared.clj` (write the same text, send `textDocument/didSave`), `wait_for_log("re-indexed")`; definition on `only-cljs` from `use_cljs.cljs` still lands in `shared.cljs`, and hover on `sh/only-cljs` still answers.
  6. `server.log` contains no `last one wins` line for `app.shared`.

- [ ] **Step 2: Run to verify it fails**
  Run: `cargo test --test test_e2e twin_namespaces -- --nocapture`
  Expected: FAIL (step 1 or 3 first, depending on scan order).

- [ ] **Step 3: Implement**
  `resolve_symbol(index, word, current_ns, dialect)` with `ns_meta_for` and a private `lookup_in_ns_for`; the five in-crate callers and the unit tests in `src/handlers/mod.rs` (`Dialect::Clj`). `definition.rs`: drop the `prefer_dialect` on the `Project` arm (resolver already prefers) — keep it on the `Core` arm's `lookup_in_ns` result. `hover.rs`: remove `prefer_dialect`, keep the `dialect` parameter. `references.rs`: `resolve_fqn_at` alias fallback via `ns_meta_for` (dialect from `path`); `references` pushes every `lookup_all` declaration; `rename_target` resolves the source-check symbol with `lookup_for(fqn, dialect)`; `rename`'s `Global` arm builds one declaration edit per project symbol in `lookup_all(&fqn)`. `definition.rs` `namespace_location`: alias table from `ns_meta_for(current_ns, dialect)`. `completion.rs`: `ns_meta_for` for `current_ns` and `lookup_for` where the current-ns and alias pools read `index.symbols.get`. `symbols.rs`: the closed-file fallback maps each fqn through `lookup_all` and keeps the symbols whose `file` is the document. `scanner.rs` and `server.rs::warn_ns_collisions`: warn only when `Dialect::of_path` agrees for both files.

- [ ] **Step 4: Run to verify it passes**
  Run: `cargo test --test test_e2e -- twin_namespaces prefers_the_ --nocapture && bb check`
  Expected: PASS, `bb check` green (run `bb fmt` first if it flags formatting).

- [ ] **Step 5: Commit**
  `git commit -am "Navigate, reference and rename a twin namespace by the asking file's dialect"`

### Task 6: Compare gate — oracle and allowlist

**Files:**
- Modify: `tests/common/oracle.rs`, `tests/test_compare.rs`

- [ ] **Step 1: Oracle**
  The `NO_SOURCE_DEFINITION` skip applies when `u.to` is `clojure.core` *or* `cljs.core`; adjust the doc comment. Extend the existing unit test near `core_usage_expects_a_library_definition_in_the_asking_dialect` with a `.cljs` file whose `if` usage produces no probe.

- [ ] **Step 2: `extra_files` and the `KNOWN` entry**
  `judge_sites` fills `Verdict::Diverge.extra_files` with the files of `got.per_line` keys absent from `expected.per_line`; the other `Diverge` constructors pass an empty set. The `KNOWN` entry per the Design (bucket prefix `var-def/`, `.cljs` probe file, `missing == 0`, every extra file the probe file's `.clj` twin by parent and stem), dated reason, after the protocol-method entry. Unit-test the matcher: the `reader_types` pair matches; an extra site in an unrelated `.clj` file, or in the twin with `missing > 0`, does not.

- [ ] **Step 3: Run the fixture pipeline and the compare gate**
  Run: `cargo test --test test_compare` (the simple_project pipeline runs inside `bb check`; here it runs alone), then `bb compare clj-kondo > .tmp/compare-after.log 2>&1`.
  Expected: `compare_simple_project` green. In the after log: `var-usage/library` and `var-usage/library/macro` with 0 diverge and 0 null (probe counts lower than the baseline's 167 / 199 by the special forms), `var-def/defmacro` 0 diverge, the `reader_types` sites under `known`. Diff the `COMPARE_JSON` lines against `.tmp/compare-baseline.log` and record both tables for Task 8.

- [ ] **Step 4: Commit**
  `git commit -am "Compare gate: cljs.core special forms and the macro-twin template"`

### Task 7: Gates

- [ ] **Step 1: Behavior and editor gates**
  Run: `bb check`, `bb e2e`, `bb e2e-pulse`, `bb e2e-calva`.
  Expected: all green. Definition targets change for `.cljs` files, which is client-visible.

- [ ] **Step 2: Index gates**
  Run: `bb soak` (clj-kondo default; the corpus's `inlined/` tree holds the `reader_types` twin pair and the soak's checkpoint restore exercises removal and re-insertion), then `bb bench clj-kondo`.
  Expected: soak passes (no divergence, no `panicked at`, RSS under 1.5x); bench rows within noise of `docs/MEMORY.md`. Record the seed and the `BENCH_JSON` lines for the PR description.

### Task 8: Docs and closing the items

**Files:**
- Modify: `docs/ROADMAP.md`, `AGENTS.md`, `docs/FEATURES.md`, `README.md`, `docs/MEMORY.md`, `docs/backlog/`

- [ ] **Step 1: ROADMAP**
  Tick the Milestone 5 item, Plan line `— done`, one clause in "Where we stand" (a `.cljs` file resolves its core to `cljs.core` and reads `:require-macros`; a namespace split over `.clj` and `.cljs` is indexed as two dialect slots). Update the "ClojureScript is best effort" Direction bullet (drop `:require-macros` from the not-supported list) and the Best-effort ClojureScript bullet likewise.

- [ ] **Step 2: Backlog files**
  Move the three issue files to `docs/archive/` (`git mv`), set each `Status` to `done (2026-09-27)` with a one-line resolution, and point the Milestone 5 item's links at `archive/`. Use the `/backlog` skill's close flow if it applies.

- [ ] **Step 3: AGENTS.md**
  Rewrite the "Library symbols and namespace metadata are keyed once per fqn" invariant: both slots now hold project entries too (the rank order, file-owned removal through `file_symbols`, shadow promotion on removal, `lookup_all` for references and rename, who reads with a dialect). Add one bullet for `cljs.core` (`core_ns`, `.cljc` stays `clojure.core`, the `Core` arm's floor) and one for `:require-macros` / `:refer-macros` (what is read, what the lints and clean-ns still skip). Note the cache bump in the existing `format_version` bullet is not needed (it already says to bump).

- [ ] **Step 4: FEATURES.md and README**
  FEATURES "File types": a `.cljs` file's core is `cljs.core`, `:require-macros` / `:refer-macros` bring in macros, and a namespace split across `.clj` and `.cljs` navigates to the copy of the asking dialect while references and rename cover both. README line 96–97: drop `:require-macros` from the unsupported list (shadow-cljs stays).

- [ ] **Step 5: MEMORY.md**
  Add a dated compare section (or a new column pair on the 2026-09-18 table, whichever the memo's own rule prefers) with the Task 6 after-table and the one-paragraph reading: which buckets emptied, that the `reader_types` sites are `known`, the probe-count drop from the special forms.

- [ ] **Step 6: Verify and commit**
  Run: `bb check`
  Expected: green.
  `git commit -am "Document ClojureScript core, require-macros and twin namespaces"`
