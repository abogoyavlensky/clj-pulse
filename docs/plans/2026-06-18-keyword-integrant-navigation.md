# Keyword Indexing & Integrant Navigation Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Index namespaced Clojure keywords as references, and navigate from an Integrant component keyword (e.g. `:readx.db/db` in a `config.edn` system map, including `#ig/ref` values) to its definition `(defmethod ig/init-key ::db …)`, with find-references spanning the lifecycle defmethods, config keys, and `#ig/ref`s.

**Tech Stack:** Rust, tower-lsp, tree-sitter-clojure, existing `Index`/`Symbol`/`Occurrence` model.

---

## Design

### Why / prior art

clojure-lsp's keyword support comes from clj-kondo's `:keywords` analysis: it indexes keyword *occurrences* in `.clj/.cljs/.cljc` (enabling references/rename across same-named qualified keywords) and treats a keyword as a *definition* only when a clj-kondo hook marks it (`reg-keyword!`) — which ships for re-frame `reg-*` and spec `s/def`. **Integrant `defmethod ig/init-key ::x` is not recognized as a keyword definition, and `.edn` config files are not analyzed at all.** So config.edn → component navigation is genuinely additive.

### Core idea

`::db` inside namespace `readx.db` *is* the keyword `:readx.db/db` — the same canonical keyword used as the `config.edn` map key and the `#ig/ref` value. That canonical printed form `:ns/name` is the join key. The feature is: make every keyword resolve to its canonical `:ns/name`, treat the `ig/init-key` defmethod's dispatch keyword as that key's definition, and index the EDN config as more occurrences of it.

### Architecture (reuse, don't fork)

- **Colon-prefixed fqns.** A var's fqn is `readx.db/db`; a keyword's canonical fqn is `:readx.db/db`. The leading `:` makes them collision-free in `Index.symbols` (a var fqn can never start with `:`). Keyword *definitions* are `Symbol`s keyed by `:ns/name`; keyword *usages* are `Occurrence`s with that same fqn. Everything flows through the existing `resolve_fqn_at` → `index.lookup`/`occurrences_for` pipeline, so goto-definition and references need no new handler logic.
- **One new `DefKind::IntegrantKey`.** Carries semantic meaning for future hover; the two exhaustive `match`es on `DefKind` (`hover.rs::defkind_str`, `symbols.rs::defkind_to_symbol_kind`) get new arms. `completion.rs` already has a `_` wildcard.

### Tree-sitter shapes (verified by prototype)

- `(defmethod ig/init-key ::db …)`: dispatch is `kwd_lit { marker: "::", name: kwd_name "db" }` (no namespace).
- `::alias/db`: `marker "::"`, `namespace kwd_ns "alias"`, `name "db"`. `:readx.db/db`: `marker ":"`, `namespace "readx.db"`, `name "db"`. `:db`: `marker ":"`, no namespace.
- `config.edn` parses with `has_error: false`. `#ig/ref :readx.db/db` → `tagged_or_ctor_lit { tag: sym_lit "ig/ref", value: kwd_lit ":readx.db/db" }`. Aero tags (`#or`/`#env`/`#profile`/`#free-port`) are plain `tagged_or_ctor_lit`s. `map_lit` children are alternating key/value `value:` nodes.

### Resolution primitive

`keyword_fqn(kwd_node, ns_meta, source) -> Option<String>`:
- `marker ":"` + namespace present → `:{namespace}/{name}` (namespace is **literal**, never alias-resolved).
- `marker "::"` + no namespace → `:{ns_meta.name}/{name}` (current ns); `None` if current ns is empty.
- `marker "::"` + namespace → resolve via `ns_meta.aliases` (fallback: literal) → `:{full_ns}/{name}`.
- `marker ":"` + no namespace (unqualified `:db`) → `None` (skipped everywhere — too noisy, matches clojure-lsp's effective behavior).

The occurrence/definition `name_range` is the **`kwd_name` node** range (the name part only, not the `::`/namespace), matching the existing `Occurrence` convention so a future rename only edits the name.

### Definition detection (Integrant)

In the symbol pass, when a `defmethod`'s multifn alias-resolves to `integrant.core/init-key` and its dispatch is a qualified `kwd_lit`, emit an `IntegrantKey` definition `Symbol`: `fqn = :ns/name`, `ns`/`name` split from the fqn, `name_range` = the dispatch `kwd_name`, `range` = the defmethod form. **`ig/init-key` is the sole definition target** (every real component has one; no cross-file "first defmethod" fallback — keeps the per-file extractor simple).

In the occurrence pass, the init-key dispatch keyword is **suppressed** (not walked) so references doesn't list it twice (declaration + occurrence). Every *other* qualified keyword — `assert-key`/`halt-key!` dispatch keys, non-Integrant defmethod dispatch keys, map values, etc. — is recorded as an occurrence by the general `kwd_lit` arm.

Multifn resolution helper `defmethod_multifn_fqn(children, ns_meta, source)`: resolve `children[1]` sym_lit via `ns_meta.aliases` (qualified) or `ns_meta.refers` (bare `:refer`ed) → e.g. `integrant.core/init-key`. This is the single extensible hook point.

### EDN config indexing

`extract_edn(source, file) -> Vec<Occurrence>`: tree-sitter-parse, walk the whole tree, record every **qualified `:`-marker** keyword (`keyword_fqn` with an empty `NsMeta`, so only literal `:ns/name` qualifies; `::` and unqualified are skipped — standard EDN has no `::`). Keywords inside `tagged_or_ctor_lit` (e.g. `#ig/ref :readx.db/db`) are reached by the generic walk.

**Gate:** only index an `.edn` file whose source `contains("#ig/ref")` (strong Integrant signal; matches `#ig/ref` and `#ig/refset`). This excludes `deps.edn`/`bb.edn`/`shadow-cljs.edn`.

**Index entry:** `Index::insert_edn_file(file, occurrences)` inserts into `occurrences` and registers the file in `file_to_ns` with a NUL sentinel (`const EDN_NS_SENTINEL: &str = "\0edn"`). The sentinel keeps `merge_project_from`'s stale-filter (`!new_index.file_to_ns.contains_key(path)`) from dropping EDN files on re-scan, and — being NUL-prefixed — can never collide with a real namespace or the empty-string ns of a no-`ns` `.clj` file. It deliberately does **not** populate `namespaces`/`ns_symbols`; `remove_file` no-ops cleanly on the absent sentinel ns.

### Glue

- `extractor::file_occurrences(source, path)` dispatches: `.edn` → `extract_edn`, else `extract_full`'s occurrences. Used by `references.rs::occurrences_for` for open files.
- `references.rs::resolve_fqn_at` gains an `.edn` branch (occurrences only, no symbols) so a cursor on a keyword in `config.edn` resolves to its canonical fqn.
- goto-definition (`definition.rs`) and references already call `resolve_fqn_at` → `index.lookup`/`occurrences_for`; no change beyond the above.
- `did_open`/`did_save` (`server.rs`) gain an `.edn` branch using the same gate; startup `build_index` collects gated `.edn` files under the source paths.
- Bump `jar_cache::CACHE_FORMAT_VERSION` 8 → 9 (Symbol/DefKind layout change).

### Acceptance

`bb e2e` with a new `integrant_project` fixture:
1. goto-definition on `:readx.db/db` in `resources/config.edn` lands on the `(defmethod ig/init-key ::db …)` line in `src/readx/db.clj`.
2. references on `:readx.db/db` (include declaration) returns 5 locations: the init-key declaration + `assert-key` + `halt-key!` in db.clj, and the config-map key + the `#ig/ref` value in config.edn.

### Out of scope (future)

Rename-on-keyword (likely works via `resolve_fqn_at`, but untested/unverified here — do **not** advertise it), keyword completion, keyword hover/doc, re-frame `reg-*` & spec `s/def` definitions (the hook table is built to accept them), and watching arbitrary-named `config.edn` via file-watcher globs (`did_save`/`did_open` cover editing).

---

## File Structure

- `src/index/mod.rs` — add `DefKind::IntegrantKey`; add `Index::insert_edn_file` + `EDN_NS_SENTINEL`.
- `src/index/extractor.rs` — `keyword_fqn`, `defmethod_multifn_fqn`, general `kwd_lit` occurrence arm, init-key definition extraction + suppression, `extract_edn`, `file_occurrences` dispatcher.
- `src/index/scanner.rs` — collect gated `.edn` files in `build_index` and insert via `insert_edn_file`.
- `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` 8 → 9.
- `src/handlers/hover.rs` — `defkind_str` arm for `IntegrantKey`.
- `src/handlers/symbols.rs` — `defkind_to_symbol_kind` arm for `IntegrantKey`.
- `src/handlers/references.rs` — `.edn` dispatch in `resolve_fqn_at` and `occurrences_for`.
- `src/server.rs` — `.edn` branch in `did_open` and `did_save`.
- `tests/test_extractor.rs` — unit tests for keyword fqn, occurrences, init-key definition, `extract_edn`.
- `tests/fixtures/integrant_project/` — new fixture (`deps.edn`, `src/readx/db.clj`, `resources/config.edn`).
- `tests/test_e2e.rs` — goto-definition + references e2e tests.

---

## Task 1: Foundation — `DefKind::IntegrantKey`, match arms, cache bump

**Files:**
- Modify: `src/index/mod.rs`, `src/handlers/hover.rs`, `src/handlers/symbols.rs`, `src/index/jar_cache.rs`

- [ ] **Step 1: Add the variant**
  Add `IntegrantKey` to the `DefKind` enum in `src/index/mod.rs`.

- [ ] **Step 2: Fix the two exhaustive matches**
  `hover.rs::defkind_str`: add `DefKind::IntegrantKey => "defmethod"`.
  `symbols.rs::defkind_to_symbol_kind`: add `DefKind::IntegrantKey => SymbolKind::KEY`.
  (`completion.rs` has a `_` wildcard → no change.)

- [ ] **Step 3: Bump cache format version**
  `src/index/jar_cache.rs`: `CACHE_FORMAT_VERSION` 8 → 9.

- [ ] **Step 4: Verify it compiles**
  Run: `cargo build`
  Expected: builds clean (no non-exhaustive-match errors).

- [ ] **Step 5: Commit**
  `git commit -m "feat: add DefKind::IntegrantKey and bump jar cache version"`

## Task 2: Keyword fqn resolver

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write failing tests**
  Add tests calling a new `pub(crate)` helper (or test via a thin wrapper). Cases over a parsed `kwd_lit` with an `NsMeta { name: "readx.db", aliases: {"db2"->"other.db"} }`:
  `::db` → `Some(":readx.db/db")`; `::db2/x` → `Some(":other.db/x")`; `:lit.ns/x` → `Some(":lit.ns/x")`; `:plain` → `None`; `::x` with empty ns name → `None`.

- [ ] **Step 2: Run to verify fail**
  Run: `cargo test --test test_extractor keyword_fqn`
  Expected: FAIL (unresolved name / assertion).

- [ ] **Step 3: Implement**
  `keyword_fqn(node, ns_meta, source) -> Option<String>` reading `marker`/`namespace`/`name` fields per the Design. Add a small test-only constructor path if needed to parse a snippet and grab the first `kwd_lit`.

- [ ] **Step 4: Run to verify pass**
  Run: `cargo test --test test_extractor keyword_fqn`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: resolve keywords to canonical colon-prefixed fqns"`

## Task 3: General qualified-keyword occurrences

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write failing test**
  `extract_full` on `(ns my.ns) (def x {:my.ns/a 1 :other/b ::a})` should yield occurrences containing `:my.ns/a`, `:other/b`, and `:my.ns/a` (from `::a`). Unqualified keys (none here) excluded. Assert by filtering occurrences whose `fqn` starts with `:`.

- [ ] **Step 2: Run to verify fail**
  Run: `cargo test --test test_extractor keyword_occurrence`
  Expected: FAIL (no `:`-fqn occurrences yet).

- [ ] **Step 3: Implement**
  Add a `"kwd_lit" => …` arm to `walk_occurrences` that calls `keyword_fqn(node, ctx.ns_meta, ctx.source)` and, when `Some(fqn)`, pushes `Occurrence { fqn, name_range: <kwd_name node range> }`. Quoted data (`quoting_lit`) and ns-form keywords remain excluded (existing structure already skips them).

- [ ] **Step 4: Run to verify pass**
  Run: `cargo test --test test_extractor keyword_occurrence`
  Expected: PASS.

- [ ] **Step 5: Run the full extractor suite (no regressions)**
  Run: `cargo test --test test_extractor`
  Expected: PASS (existing symbol-occurrence tests unaffected).

- [ ] **Step 6: Commit**
  `git commit -m "feat: record qualified keyword occurrences"`

## Task 4: Integrant `ig/init-key` definition

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write failing tests**
  Source: `(ns readx.db (:require [integrant.core :as ig])) (defmethod ig/init-key ::db [_ o] o) (defmethod ig/halt-key! ::db [_ d] nil)`.
  Assert: `symbols` contains exactly one with `kind == DefKind::IntegrantKey`, `fqn == ":readx.db/db"`, `ns == "readx.db"`. `occurrences` contains a `:readx.db/db` for `halt-key!` but **not** a second one for `init-key` (count of `:readx.db/db` occurrences == 1). Also: a non-Integrant `(defmethod area ::circle …)` produces **no** `IntegrantKey` symbol (its keyword is only an occurrence).

- [ ] **Step 2: Run to verify fail**
  Run: `cargo test --test test_extractor integrant`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  Add `defmethod_multifn_fqn(children, ns_meta, source)` (alias/refer resolution). In `process_top_level_list`/`extract_def`, when head is `defmethod` and the multifn resolves to `integrant.core/init-key` and dispatch (`children[2]`) is a qualified `kwd_lit`, push the `IntegrantKey` symbol. In `walk_def_form`'s `Defmethod` branch, **skip walking the dispatch** when it is that same init-key qualified keyword; otherwise walk it as today.

- [ ] **Step 4: Run to verify pass**
  Run: `cargo test --test test_extractor integrant`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: index ig/init-key defmethod as Integrant component definition"`

## Task 5: EDN occurrence extraction

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write failing test**
  `extract_edn` on a config-map string `{:readx.db/db {:url "x"} :readx.server/server {:db #ig/ref :readx.db/db}}` returns occurrences whose fqns include `:readx.db/db` (twice — key + `#ig/ref`), `:readx.server/server` (once); unqualified keys (`:url`, `:db`) excluded.

- [ ] **Step 2: Run to verify fail**
  Run: `cargo test --test test_extractor extract_edn`
  Expected: FAIL (unresolved name).

- [ ] **Step 3: Implement**
  `pub fn extract_edn(source, file) -> Vec<Occurrence>`: parse, walk every node, on `kwd_lit` call `keyword_fqn(node, &<empty NsMeta>, source)` and push qualified ones. Keywords inside `tagged_or_ctor_lit` are reached by the generic descent.

- [ ] **Step 4: Run to verify pass**
  Run: `cargo test --test test_extractor extract_edn`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: extract qualified keyword occurrences from EDN"`

## Task 6: Index + scanner + references glue

**Files:**
- Modify: `src/index/mod.rs`, `src/index/scanner.rs`, `src/index/extractor.rs`, `src/handlers/references.rs`
- Test: `tests/test_index.rs` (or `tests/test_extractor.rs` for the dispatcher)

- [ ] **Step 1: Write failing test**
  In `tests/test_index.rs`: build an `Index`, `insert_edn_file(path, occs)`, assert `occurrences_for`-style lookup finds them and `is_project_path(path)` is true; then `remove_file(path)` clears them without panicking. Also assert a no-`ns` `.clj` file inserted via `insert_file` (empty ns) and an EDN file can coexist and be removed independently (no shared-ns clobber).

- [ ] **Step 2: Run to verify fail**
  Run: `cargo test --test test_index insert_edn`
  Expected: FAIL (unresolved `insert_edn_file`).

- [ ] **Step 3: Implement**
  - `Index::insert_edn_file(file, occurrences)` + `EDN_NS_SENTINEL` per Design (occurrences + `file_to_ns` sentinel only).
  - `extractor::file_occurrences(source, path)` dispatcher (`.edn` → `extract_edn`, else `extract_full` occurrences).
  - `scanner::build_index`: collect `.edn` files under `source_paths`, gate on `contains("#ig/ref")`, insert via `insert_edn_file`.
  - `references.rs`: `resolve_fqn_at` `.edn` branch (occurrences from `extract_edn`, no symbols); `occurrences_for` uses `file_occurrences` for open files.

- [ ] **Step 4: Run to verify pass**
  Run: `cargo test --test test_index insert_edn`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "feat: index EDN config files and resolve keywords through references pipeline"`

## Task 7: Server wiring (didOpen / didSave)

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Implement**
  `did_open`: add an `else if` branch — when `path` ends in `.edn`, `text.contains("#ig/ref")`, and `file_ns(path).is_none()` → `insert_edn_file(path, extract_edn(&text, &path))`.
  `did_save`: add an `else if` branch for `.edn` — `remove_file(&path)`, then if the re-read source `contains("#ig/ref")`, `insert_edn_file(path, extract_edn(&source, &path))`.
  (Diagnostics already no-op for non-Clojure-source, so no `.edn` diagnostic noise.)

- [ ] **Step 2: Verify build + existing e2e**
  Run: `cargo build && bb e2e`
  Expected: builds clean; existing e2e tests still PASS.

- [ ] **Step 3: Commit**
  `git commit -m "feat: index Integrant EDN configs on open and save"`

## Task 8: E2E fixture + acceptance tests

**Files:**
- Create: `tests/fixtures/integrant_project/deps.edn`, `tests/fixtures/integrant_project/src/readx/db.clj`, `tests/fixtures/integrant_project/resources/config.edn`
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Create the fixture**
  `deps.edn`: `{:paths ["src" "resources"]}`.
  `src/readx/db.clj`: `(ns readx.db (:require [integrant.core :as ig]))` + three defmethods `ig/assert-key`, `ig/init-key`, `ig/halt-key!`, all dispatching `::db`.
  `resources/config.edn`: a map `{:readx.db/db {:jdbc-url "…"} :readx.server/server {:db #ig/ref :readx.db/db}}`.

- [ ] **Step 2: Write the e2e tests**
  Using `LspClient`/`setup_named("integrant_project")` + `wait_for_log("Indexed")`:
  - `test_e2e_integrant_goto_definition_from_config`: `did_open(config.edn)`, `goto_definition` at the `:readx.db/db` key → response uri ends `src/readx/db.clj`, range line == the `ig/init-key ::db` line (locate with `position_of`).
  - `test_e2e_integrant_references`: `did_open(db.clj)`, `references` at `::db` on the init-key line with `include_declaration=true` → 5 locations: db.clj init-key/assert-key/halt-key! + config.edn key + `#ig/ref`.

- [ ] **Step 3: Run to verify fail-then-pass**
  Run: `cargo test --test test_e2e integrant`
  Expected: PASS (both tests). If a test was written before Task 6/7 wiring it would FAIL; here all deps are in place, so confirm PASS.

- [ ] **Step 4: Commit**
  `git commit -m "test: e2e for Integrant config.edn keyword navigation"`

## Task 9: Full gate + docs

**Files:**
- Modify: `ARCHITECTURE.md` and/or `CLAUDE.md` (brief note on keyword/EDN indexing)

- [ ] **Step 1: Full check**
  Run: `bb check`
  Expected: fmt clean, clippy `-D warnings` clean, all tests PASS.

- [ ] **Step 2: Full e2e**
  Run: `bb e2e`
  Expected: PASS.

- [ ] **Step 3: Document**
  Add a short note to `ARCHITECTURE.md` (keyword occurrences + Integrant key definitions; EDN config indexed when it contains `#ig/ref`) and, if relevant, the `bb e2e` coverage line in `CLAUDE.md`. Use /writing-clearly.

- [ ] **Step 4: Commit**
  `git commit -m "docs: note keyword indexing and Integrant EDN navigation"`
