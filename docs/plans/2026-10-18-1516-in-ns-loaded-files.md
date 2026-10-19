# Files Loaded Into a Namespace with `(in-ns '…)` Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A file with no `ns` form whose top level switches namespace with `(in-ns 'x)` — the files a namespace pulls in with `(load "…")` — defines its vars in `x`, so definition, hover, completion and references reach them, without that file taking `x`'s metadata slot from the file that has the real `ns` form.

**Tech Stack:** Rust, tree-sitter-clojure, the index's slot-ranking rules (`src/index/mod.rs`).

---

## Design

### The bug

`bb compare` (2026-10-17): about 35 answers across penpot and clj-kondo are
null definitions on `clojure.pprint` vars — `pprint`, `print-table`,
`with-pretty-writer`, `write-option-table`, `*print-base*`. `clojure/pprint.clj`
is `(ns clojure.pprint …)` plus `(load "pprint/pprint_base")` and friends; each
loaded file starts with `(in-ns 'clojure.pprint)` and holds the actual `defn`s.
The extractor knows only `ns` forms, so a loaded file has an empty namespace
name and its definitions are filed under bare names that nothing resolves to.
`clojure.core` is split the same way (`core_deftype.clj`, `core_print.clj`, …
all `(in-ns 'clojure.core)`), so `defrecord`, `deftype` or `print-method` never
navigate into the clojure JAR either.

### Extraction

`process_top_level_list` recognises a top-level `(in-ns 'x)` or
`(in-ns (quote x))` whose argument is a quoted plain symbol. When the file has no
namespace yet (`ns_meta.name` empty), it sets `ns_meta.name = x` and
`ns_meta.in_ns = true`; every later definition is then `x/…`, as the occurrence
pass already qualifies bare names with `ns_meta.name`. An `in-ns` after a real
`ns` form changes nothing (REPL-style namespace switching in a source file is out
of scope), and an `ns` form after an `in-ns` sets the name as usual and clears
`in_ns`.

One helper recognises the form, `fn in_ns_target(children: &[Node], source: &str)
-> Option<String>`, and both metadata readers use it: `process_top_level_list`
and `ns_meta_of_tree`, the locals walker's own reader. Otherwise the locals walker
would see a nameless namespace in a loaded file, so a bare head resolved by fqn
through the current namespace (a `:lint-as` binding macro the namespace defines)
would bind for the occurrence walker and not for local definition and rename.

The loaded file's other metadata stays empty: at run time it shares the loading
namespace's aliases and refers, which a single-file extraction cannot know.
Qualified usages through those aliases stay unresolved in that file, as they are
today; the definitions, which carry the bug, are what this plan fixes.

### `NsMeta.in_ns` and the metadata slot

```rust
/// The file names its namespace with a top-level `(in-ns 'x)` rather than an
/// `ns` form — a file another one `(load …)`s. Its metadata says nothing about
/// the namespace's requires or aliases, so it ranks below every `ns` form.
#[serde(default)]
pub in_ns: bool,
```

Two files of one namespace share one metadata slot (`namespaces` /
`cljs_namespaces`), ranked by `slot_rank` and filled by `rank_insert`, last writer
among equals. An `in-ns` file at the same rank as the main file would displace the
main file's metadata — losing its aliases and moving `namespace_location` to the
loaded file. So the namespace-metadata rank of an `in_ns` file is a new
`IN_NS_RANK = 6`, worse than every real rank (project 0–2, library 3–5) and not a
`.cljs` rank (`6 % 3 == 0`), so it never goes to the shadow:

- `meta_rank` returns `IN_NS_RANK` for an `in_ns` meta; `insert_file` and
  `insert_lib_file` use `meta_rank`-equivalent ranking for the namespace slot
  instead of `slot_rank` directly. Symbols keep `slot_rank` — a loaded file's
  `defn` is as real as any.
- Result: an `in-ns` file fills the slot only when it is vacant, and the file
  with the `ns` form displaces it whenever it arrives; `refill_namespace` (after
  a project file is removed) goes through `meta_rank`, so it prefers a real `ns`
  form too.
- `insert_lib_file`'s "namespace owned by the project" check ignores an
  `in_ns` slot holder. Otherwise a project file that patches a library
  namespace with `(in-ns 'clojure.core)`, indexed before the JAR, would make the
  whole library namespace skip indexing. The library's `ns` file then displaces
  the project `in_ns` metadata (rank 3 against 6), while the project file's
  symbols keep outranking the library's for the fqns it redefines.

- `clear_libs` (classpath change) keeps a namespace's metadata entry while the
  namespace still has project symbols, whoever's metadata it is. Before
  `in_ns`, a namespace with project files always held project metadata (ranks
  0–2 beat 3–5); now a project `in_ns` file's namespace can hold a library's.
  After its retains, `clear_libs` drops every metadata entry whose file is not a
  project file and refills the namespace from its project records
  (`refill_namespace`), so navigation never points into a removed dependency.
- The scanner's "namespace … defined in both … and …; last one wins" warning
  (`src/index/scanner.rs`) is for two `ns` forms colliding; it skips a pair in
  which either file is `in_ns`, since a loaded file shares its namespace on
  purpose.

`file_to_ns`, `ns_files` and `ns_symbols` treat the loaded file as a file of `x`,
so completion's current-namespace pool and `remove_file` work as for any second
file of a namespace.

`NsMeta` and extractor output change: `CACHE_FORMAT_VERSION` 26 → 27.

### Testing

- Extractor: `(in-ns 'clojure.pprint)` then `(defn pprint …)` → symbol
  `clojure.pprint/pprint`, `in_ns` true; `(in-ns (quote x))`; an `in-ns` after
  `(ns a)` leaves `a`; an `ns` form after an `in-ns` sets its name and clears
  `in_ns`; a file with neither stays nameless; an occurrence of a bare
  name defined in the file is qualified with `x`; with
  `:lint-as {x/my-let clojure.core/let}`, `(my-let [a 1] a)` in the loaded file
  binds `a` for both the occurrence walker and `local_references_at` (local
  definition and rename agree).
- Index (`src/index/mod.rs` tests): main-then-loaded and loaded-then-main both
  leave the main file's metadata in the slot (aliases intact, `file` the main
  file); a loaded file alone fills the slot; removing the main project file
  refills from another real `ns` file before an `in_ns` one; the loaded file's
  symbols are indexed under `x` in both orders, for `insert_file` and
  `insert_lib_file`; the project/library combinations: a project `in_ns` file of
  `clojure.core` indexed before the library `clojure.core` leaves the library's
  metadata in the slot and its symbols indexed, the project file's redefinition
  still winning its own fqn; a library `in_ns` file never displaces a project
  `ns` file's metadata; after a project `in_ns` file's namespace took a library's
  metadata, `clear_libs` leaves the project file's own metadata in the slot (file
  is the project file) and the project symbols intact.
- E2E: a fixture namespace `loaded.core` (`(ns loaded.core) (load "core_extra")`)
  plus `src/loaded/core_extra.clj` (`(in-ns 'loaded.core) (defn helper …)`) and a
  caller requiring `[loaded.core :as l]` and calling `(l/helper)`: definition
  lands in `core_extra.clj`; references on `helper` list the call; definition on
  the namespace symbol `loaded.core` in the caller's `ns` form lands in `core.clj`.
- JAR: a `bb e2e-real` test (ignored in plain `cargo test`, like the existing
  real-classpath tests) where `(:require [clojure.pprint :refer [pprint]])` and
  definition on `pprint` lands in the clojure JAR's `clojure/pprint/pprint_base.clj`.
- Gates: `bb check`, `bb e2e`, `bb e2e-real`, `bb e2e-calva` (definition into the JAR changes),
  `bb e2e-pulse`, `bb compare` (the `pprint` nulls on both corpora should agree),
  `bb bench` (extractor change; clojure.core gains the loaded files' vars),
  `bb soak` (index change).

## File Structure

- Modify `src/index/mod.rs` — `NsMeta.in_ns`; `IN_NS_RANK`; namespace-slot ranking
  in `meta_rank`, `insert_file`, `insert_lib_file`; index tests. Every `NsMeta { … }`
  literal in the crate gains `in_ns: false` (or `..Default::default()` where
  `NsMeta` derives `Default`; check first).
- Modify `src/index/extractor.rs` — the `in-ns` arm of `process_top_level_list`;
  `extract_ns` clears `in_ns`.
- Modify `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` 27 with its doc line.
- Modify `tests/test_extractor.rs`, `tests/test_e2e.rs`; create
  `tests/fixtures/simple_project/src/loaded/core.clj`,
  `tests/fixtures/simple_project/src/loaded/core_extra.clj`,
  `tests/fixtures/simple_project/src/loaded_caller.clj`.
- Modify `AGENTS.md`, `docs/FEATURES.md`, `docs/MEMORY.md`, `docs/ROADMAP.md`.

## Tasks

### Task 0: Open the ROADMAP item

- [ ] **Step 1:** Add a Milestone 5 item beside the 2026-10-18 `:refer :all`
  item: "**Files loaded with `(in-ns '…)`**", one line of evidence
  (`clojure.pprint/pprint` answers null), and
  `Plan: [2026-10-18-1516-in-ns-loaded-files.md](plans/2026-10-18-1516-in-ns-loaded-files.md) — in progress`.
- [ ] **Step 2: Commit** `git commit -m "roadmap: plan in-ns loaded files"`

### Task 1: `NsMeta.in_ns` and its rank

**Files:**
- Modify: `src/index/mod.rs` (and every `NsMeta` literal the compiler flags)

- [ ] **Step 1: Add the field** (Design) and fix every literal
  (`cargo build --all-targets` lists them).
- [ ] **Step 2: Write failing index tests** (Design → Testing, the index bullets).
  Run: `cargo test --lib index::tests` — Expected: the order-dependent tests FAIL.
- [ ] **Step 3: Implement** `IN_NS_RANK`, an in-ns-aware namespace rank used by
  `meta_rank`, `insert_file` and `insert_lib_file` for the namespace slot only,
  and make `insert_lib_file`'s project-ownership guard ignore an `in_ns` holder;
  in `clear_libs`, drop non-project metadata entries and `refill_namespace`
  from project records; in `scanner.rs`, skip the collision warning when either
  file is `in_ns` (unit test if the warning has one, else assert through the
  log in the e2e of Task 3).
- [ ] **Step 4: Run** `cargo test --lib` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "index: an in-ns file never takes its namespace's metadata slot from an ns form"`

### Task 2: Extraction

**Files:**
- Modify: `src/index/extractor.rs`, `src/index/jar_cache.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write failing extractor tests** (Design → Testing, the extractor
  bullet). Run: `cargo test --test test_extractor in_ns` — Expected: FAIL.
- [ ] **Step 2: Implement** `in_ns_target` and use it in `process_top_level_list`
  and `ns_meta_of_tree`: head text `in-ns`, exactly one
  argument, a `quoting_lit` whose single named child is an unqualified `sym_lit`,
  or a `(quote sym)` list; only when `ns_meta.name` is empty. `extract_ns` sets
  `in_ns = false`.
- [ ] **Step 3: Bump** `CACHE_FORMAT_VERSION` to 27: "27: a file naming its
  namespace with `(in-ns 'x)` defines its vars in `x` (`NsMeta.in_ns`)".
- [ ] **Step 4: Run** `cargo test --test test_extractor` and `cargo test --lib` —
  Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "extractor: a top-level in-ns names the file's namespace"`

### Task 3: E2E

**Files:**
- Create: the three fixture files (File Structure)
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Write the fixture and the test** (Design → Testing, E2E), modeled
  on an existing definition-plus-references e2e test. Then run the whole e2e suite
  once to catch tests that count fixture files or symbols, and adjust only those.
- [ ] **Step 2: Write the JAR test** (Design → Testing, JAR) beside the existing
  `bb e2e-real` tests in `tests/test_e2e.rs`.
- [ ] **Step 3: Run** `bb e2e` and `bb e2e-real` — Expected: PASS.
- [ ] **Step 4: Commit** `git commit -m "e2e: definition and references into an in-ns loaded file"`

### Task 4: Gates, records, docs

- [ ] **Step 1:** `bb check` — Expected: green.
- [ ] **Step 2:** `bb e2e`, `bb e2e-calva`, `bb e2e-pulse` — Expected: green.
- [ ] **Step 3:** `bb compare` — Expected: penpot `var-usage/library/referred`
  (`pprint`, `print-table`) and clj-kondo `var-usage/library/macro/aliased` /
  `library/aliased` (`pprint/with-pretty-writer`, `pprint/write-option-table`,
  `pprint/*print-base*`) move to agree; no row loses agreement beyond
  resampling. Record both tables in `docs/MEMORY.md`.
- [ ] **Step 4:** `bb bench` — compare with the 2026-10-17 A/B; note symbol count
  and RSS for both corpora (clojure.core gains its loaded files).
- [ ] **Step 5:** `bb soak` — Expected: pass.
- [ ] **Step 6: Docs.** AGENTS.md: an invariant for `in-ns` files (name,
  `IN_NS_RANK`, empty aliases by design). `docs/FEATURES.md`: navigation reaches
  vars defined in `load`ed files, `clojure.pprint` and `clojure.core`'s split
  files among them. README: review its support-boundaries lines in the same change
  and add the loaded-files coverage where navigation coverage is listed. `docs/ROADMAP.md`: tick the item, status `done`.
- [ ] **Step 7: Commit** `git commit -m "docs: in-ns loaded files"`
