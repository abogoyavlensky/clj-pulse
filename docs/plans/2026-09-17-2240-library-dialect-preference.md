# Library Dialect Preference Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `.clj` file navigates into the Clojure copy of a library namespace and a `.cljs` file into the ClojureScript copy, whatever order the classpath lists the JARs (ROADMAP Milestone 5, promoted from the 2026-09-10 Backlog item).

**Tech Stack:** Rust, tower-lsp 0.20, DashMap. Tests: unit tests in `src/index/mod.rs`, e2e in `tests/test_e2e.rs` on `tests/fixtures/simple_project` with two fixture JARs.

---

## Design

### Today

`Index::symbols` and `Index::namespaces` are flat maps keyed by fqn and namespace name. `Index::insert_lib_file` (`src/index/mod.rs`) overwrites any non-project entry with the incoming one, so the last library file inserted owns `clojure.string/trim`. Library JARs are inserted in classpath order (`scanner::index_classpath_jars` collects in parallel, then inserts sequentially over the `jars` vector), so with both `org.clojure/clojure` and `org.clojure/clojurescript` on the classpath a `.clj` file navigates into `clojure/string.cljs` whenever the ClojureScript JAR comes later. The same race exists inside one JAR that ships `foo/core.clj` next to `foo/core.cljs`.

### The target

The primary maps become deterministic and Clojure-preferred, and the ClojureScript copy a Clojure one displaces is kept in a shadow map that only `.cljs` requesters consult.

**Dialect rank at insertion.** A library file is ranked by extension: `.clj` 0, `.cljc` 1, `.cljs` 2, anything else 1. In `insert_lib_file`, per symbol:

- primary entry is a project symbol: skip, as today;
- primary vacant: insert;
- primary holds a library symbol of rank `old`, incoming has rank `new`:
  - `new <= old`: the incoming symbol takes the primary slot. If the displaced symbol was `.cljs` and the incoming is not, the displaced one moves to `cljs_symbols`. Equal rank keeps today's last-writer rule for same-dialect duplicates;
  - `new > old` and incoming is `.cljs`: insert into `cljs_symbols`, replacing whatever it holds;
  - `new > old` otherwise: drop it (a `.cljc` arriving after a `.clj`).

`Index::namespaces` gets the same rule keyed by namespace name, with the file being `meta.file`, into `cljs_namespaces`. `ns_symbols` stays last-writer: it only feeds completion name lists, and every name in it still resolves through the primary map.

**Dialect of the asking file.** `Dialect::of_path(&Path)`: `.cljs` is `Cljs`, everything else (`.clj`, `.cljc`, `.lg`, an EDN file, a `jar:` virtual path judged by its entry name) is `Clj`.

**Read API.** Three new methods on `Index`; every existing call site keeps its meaning.

```rust
pub enum Dialect { Clj, Cljs }
impl Dialect { pub fn of_path(path: &Path) -> Dialect }

impl Index {
    /// `lookup`, but a `.cljs` requester gets the ClojureScript copy when one exists.
    pub fn lookup_for(&self, fqn: &str, dialect: Dialect) -> Option<Symbol>;
    /// `ns_meta`, with the same rule.
    pub fn ns_meta_for(&self, ns: &str, dialect: Dialect) -> Option<NsMeta>;
    /// Swaps an already-resolved library symbol for its ClojureScript copy when
    /// `dialect` is `Cljs` and one exists; project symbols and `Clj` pass through.
    pub fn prefer_dialect(&self, sym: Symbol, dialect: Dialect) -> Symbol;
}
```

For `Clj` the primary is the answer. For `Cljs` the primary is read first: when it holds a project symbol (or, for `ns_meta_for`, a namespace owned by a project file, the `occurrences` check `insert_lib_file` already makes), that is the answer, because `insert_file` overwrites the primary slot but never touches the shadow maps, so a project definition inserted *after* both library copies would otherwise lose to the shadow. Otherwise the shadow map is tried and the primary is the fallback, so a library that ships only `.cljs` still resolves.

**Who switches.** Definition and hover only.

- `handlers/definition.rs`: `lookup_for` after `resolve_fqn_at`; `prefer_dialect` on the `ResolvedSymbol::Project` arm and on the `Core` arm's `lookup_in_ns("clojure.core", …)` result; `ns_meta_for` for the target namespace in `namespace_location`. The dialect comes from `path`, which the handler already computes with `uri::to_index_path`.
- `handlers/hover.rs`: `resolve_and_format` gains a `dialect: Dialect` parameter and applies `prefer_dialect` to the `Project` arm. The two unit tests in that file pass `Dialect::Clj`.

References, rename, completion, signature help, ClojureDocs and workspace symbols stay on the primary maps. They are driven by project occurrences, and the Clojure-preferred primary is the right default there.

**Lifetime.** Both shadow maps hold library entries only. `clear_libs` empties them; `remove_file` and `merge_project_from` never touch them. `Symbol` and `NsMeta` layouts do not change, so `JarCacheEntry::format_version` stays.

### Out of scope

- A project that holds `foo.clj` and `foo.cljs` for one namespace: the Backlog item "Two files, one namespace" (2026-09-10).
- References from a `.cljs` file listing the library declaration: it stays on the `.clj` copy.
- Navigating *from inside* a `.cljs` library file whose ns form differs from its `.clj` twin: the position-aware path (`resolve_fqn_at`) reads aliases from the live buffer's own ns form, so it is already right; only the bare-word fallback (`resolve_symbol`, `namespace_location`'s alias lookup on `current_ns`) reads the primary `NsMeta` of the namespace name, and stays that way.
- `bb bench` keeps accepting either archive entry (`tests/common/sites.rs`, `Expect::Archive`), because clojure-lsp answers through the same check.

## File Structure

- Modify: `src/index/mod.rs` — `Dialect`, the two shadow maps, the rank rule in `insert_lib_file`, `lookup_for`, `ns_meta_for`, `prefer_dialect`, `clear_libs`, unit tests.
- Modify: `src/handlers/definition.rs` — dialect-aware lookups.
- Modify: `src/handlers/hover.rs` — `resolve_and_format` takes a dialect.
- Modify: `tests/test_e2e.rs` — two e2e tests with a Clojure and a ClojureScript JAR.
- Modify: `docs/ROADMAP.md`, `AGENTS.md`, `docs/FEATURES.md`.

## Tasks

### Task 1: Promote the roadmap item

**Files:**
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Move the item**
  Delete the 2026-09-10 Backlog line "A `.clj` file navigates into the ClojureScript copy of a core namespace" and add it to Milestone 5 as an unticked item directly above **Release**, keeping its text, with the line `Plan: [2026-09-17-2240-library-dialect-preference.md](plans/2026-09-17-2240-library-dialect-preference.md) — in progress`.

- [ ] **Step 2: Commit**
  `git commit -am "Plan library dialect preference"` (include this plan file).

### Task 2: Dialect and shadow maps in the index

**Files:**
- Modify: `src/index/mod.rs`
- Test: `src/index/mod.rs` (`mod tests`)

- [ ] **Step 1: Write the failing unit tests**
  Add a helper that builds a library `Symbol` and `NsMeta` for a given fqn and file path with `SymbolSource::Jar`. Tests:
  - `lib_insert_prefers_clj_over_cljs_in_either_order`: insert `clojure/string.cljs` then `clojure/string.clj`, and in a second index the reverse; both `lookup("clojure.string/trim")` answer the `.clj` file, and `ns_meta("clojure.string")` the `.clj` file.
  - `lookup_for_cljs_returns_the_cljs_copy`: after both are inserted in either order, `lookup_for(…, Dialect::Cljs)` answers the `.cljs` file and `ns_meta_for(…, Dialect::Cljs)` its file; `lookup_for(…, Dialect::Clj)` the `.clj` one.
  - `cljs_only_library_resolves_for_both_dialects`: a single `.cljs` file; both dialects answer it.
  - `cljc_after_clj_is_dropped_and_before_clj_is_replaced`.
  - `project_symbol_wins_over_every_dialect`: `insert_file` a project symbol, then a `.clj` and a `.cljs` library symbol with the same fqn; `lookup` and `lookup_for(Cljs)` both answer the project one.
  - `project_inserted_after_both_library_copies_still_wins`: insert the `.clj` and `.cljs` library copies first, then `insert_file` a project symbol and namespace with the same fqn and name; `lookup_for(Cljs)`, `ns_meta_for(Cljs)` and `prefer_dialect` on the project symbol all answer the project one.
  - `prefer_dialect_swaps_only_library_symbols`.
  - `clear_libs_drops_the_cljs_shadow`: after `clear_libs`, `lookup_for(…, Dialect::Cljs)` is `None`.
  - `dialect_of_path`: `.cljs` is `Cljs`; `.clj`, `.cljc`, `.lg`, `.edn` and a `x.jar!/clojure/string.cljs` virtual path (`Cljs`) behave as designed.

- [ ] **Step 2: Run to verify they fail**
  Run: `cargo test --lib index::tests`
  Expected: compile errors for `Dialect`, `lookup_for`, `ns_meta_for`, `prefer_dialect`.

- [ ] **Step 3: Implement**
  In `src/index/mod.rs`: `Dialect` with `of_path`; a private `fn lib_rank(path: &Path) -> u8` (0, 1, 2 per the design); fields `cljs_symbols: DashMap<String, Symbol>` and `cljs_namespaces: DashMap<String, NsMeta>` with doc comments saying they hold the ClojureScript copy a Clojure one displaced; the rank rule in `insert_lib_file` for symbols and for the namespace entry; the three methods; clearing in `clear_libs`. Keep the project-wins check exactly as it is.

- [ ] **Step 4: Run to verify they pass**
  Run: `cargo test --lib index::`
  Expected: PASS, including the existing `clear_libs_resets_letgo_core_marker` and `merge_project_drops_symbols_removed_from_a_rescanned_file`.

- [ ] **Step 5: Commit**
  `git commit -am "Prefer the Clojure copy of a library symbol and keep the ClojureScript one aside"`

### Task 3: Definition and hover ask for their dialect

**Files:**
- Modify: `src/handlers/definition.rs`
- Modify: `src/handlers/hover.rs`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing e2e tests**
  Add a helper `clojure_and_clojurescript_jars_project(cljs_first: bool) -> (TempDir, PathBuf)` next to `two_ns_jar_project`: `setup_project()`, write `clojure-x.jar` holding `clojure/string.clj` (`(ns clojure.string)` and `(defn trim "Clojure" [s] s)`) and `clojurescript-x.jar` holding `clojure/string.cljs` (`(ns clojure.string)` and `(defn trim "ClojureScript" [s] s)`), and write `.cpcache/1.cp` with both paths joined by `std::env::join_paths`, in the order `cljs_first` says. Write two consumers under `src/`: `uses_string.clj` and `uses_string.cljs`, each `(ns uses-string (:require [clojure.string :as str]))` followed by `(str/trim "x")`.
  Tests, each running both orders in a loop:
  - `test_e2e_definition_from_clj_prefers_the_clojure_jar_copy`: `initialize`, `wait_for_log("library indexing complete")`, `did_open` the `.clj` consumer, `goto_definition` on `trim` (via `position_of`); the URI starts with `jar:file://` and ends with `!/clojure/string.clj`.
  - `test_e2e_definition_from_cljs_prefers_the_clojurescript_jar_copy`: same on the `.cljs` consumer; the URI ends with `!/clojure/string.cljs`. Also `hover` on `trim` there and assert the markdown contains `ClojureScript`.

- [ ] **Step 2: Run to verify they fail**
  Run: `cargo test --test test_e2e prefers_the_ -- --nocapture`
  Expected: FAIL. The `.clj` test fails in the `cljs_first = false` iteration (URI ends with `.cljs`), the `.cljs` test in the `cljs_first = true` iteration.

- [ ] **Step 3: Implement**
  `definition.rs`: `let dialect = Dialect::of_path(&path);` after `path` is computed; use `index.lookup_for(&fqn, dialect)`; wrap the `Project(sym)` arm and the `Core` arm's `lookup_in_ns` result in `index.prefer_dialect(sym, dialect)`; give `namespace_location` a `dialect` parameter and resolve the *target* namespace with `index.ns_meta_for(&ns, dialect)`. The alias lookup on `current_ns` stays `ns_meta` (see "Out of scope").
  In the `.cljs` e2e test, also assert navigation on the `clojure.string` symbol in the require clause lands on `!/clojure/string.cljs`, which exercises `ns_meta_for`.
  `hover.rs`: `resolve_and_format(index, word, current_ns, dialect)`, computing the dialect from `path` in `handle` and applying `prefer_dialect` in the `Project` arm; update the two unit tests.

- [ ] **Step 4: Run to verify they pass**
  Run: `cargo test --test test_e2e prefers_the_ -- --nocapture`
  Expected: PASS, both orders.

- [ ] **Step 5: Run the whole suite**
  Run: `bb check`
  Expected: green. If `bb check` flags formatting, run `bb fmt` and re-run.

- [ ] **Step 6: Commit**
  `git commit -am "Navigate into the copy of a library namespace that matches the file's dialect"`

### Task 4: Editor gates

- [ ] **Step 1: Run the editor gates**
  Run: `bb e2e`, `bb e2e-pulse`, `bb e2e-calva`
  Expected: all green. Definition is client-visible and this changes which location it answers, so all three apply.

- [ ] **Step 2: Run the index gates**
  Run: `bb soak` (clj-kondo corpus, the default) and `bb bench clj-kondo`
  Expected: the soak passes with no divergence and RSS growth under 1.5x; the bench definition and dependency-definition rows are within noise of the tables in `docs/MEMORY.md`. The clj-kondo corpus has both `org.clojure/clojure` and `org.clojure/clojurescript` on its `:test` classpath, which is the case this plan fixes. Record the `BENCH_JSON` lines in the PR description; `bb bench metabase` is not required for this change.

### Task 5: Docs

**Files:**
- Modify: `docs/ROADMAP.md`, `AGENTS.md`, `docs/FEATURES.md`, `README.md`

- [ ] **Step 0: README**
  The working rules require README and AGENTS.md to change with a ticked item. Read the README feature bullets; if dependency navigation is described there, add the dialect clause in one sentence. If nothing there is affected, say so in the commit message rather than adding a line.

- [ ] **Step 1: ROADMAP**
  Tick the Milestone 5 item, set the Plan line to `— done`, and add one clause to "Where we stand": a library namespace present in both dialects navigates to the copy matching the asking file.

- [ ] **Step 2: AGENTS.md invariant**
  After the "Classpath libraries come in two shapes" bullet, add one bullet: library symbols and namespace metadata are keyed once per fqn, Clojure-preferred (`.clj` over `.cljc` over `.cljs`, whatever the insertion order); the ClojureScript copy a Clojure one displaces lives in `Index::cljs_symbols` / `cljs_namespaces`, and only `lookup_for` / `ns_meta_for` / `prefer_dialect` with `Dialect::Cljs` read it — definition and hover do, every other handler stays on the primary maps. `Dialect::of_path` decides by extension, `.cljs` alone being ClojureScript.

- [ ] **Step 3: FEATURES.md**
  Under "File types", add a sentence: when a dependency ships a namespace as both `.clj` and `.cljs`, navigation and hover from a `.clj` or `.cljc` file open the Clojure copy and from a `.cljs` file the ClojureScript one.

- [ ] **Step 4: Verify and commit**
  Run: `bb check`
  Expected: green.
  `git commit -am "Document library dialect preference"`
