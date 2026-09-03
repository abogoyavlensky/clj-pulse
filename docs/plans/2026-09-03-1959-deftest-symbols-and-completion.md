# deftest Symbols and Completion Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Index `(deftest name …)` forms as project symbols (outline, workspace search, references) and make `deftest` and its siblings complete in test files regardless of how `clojure.test` was required.

**Tech Stack:** Rust (tower-lsp, tree-sitter-clojure), existing e2e harness (`tests/test_e2e.rs`).

**Environment note:** `mise install` in the repo root provides every tool the verification tasks need (rust, babashka, neovim, java, clojure, clj-kondo; see `.mise.toml`). Baselines on 2026-09-03: `bb check`, `bb e2e`, `bb e2e-nvim`, `bb e2e-calva`, `bb e2e-real`, `bb e2e-real-kondo` all green before this work.

---

## Design

### Problem

Two gaps, one root cause: the server knows `defn`/`def`/… by bare name only
(`DefKind::from_def_symbol`), and it knows `clojure.test` only as far as the
`:refer […]` vector goes.

1. **Outline.** `(deftest add-works …)` is not a def form, so the extractor
   emits no `Symbol`. "Go to Symbol in Editor" (Shift+Cmd+O →
   `textDocument/documentSymbol`) shows nothing for a test file, workspace
   symbol search (Cmd+T) cannot find tests, and references treat the test
   name as a usage.
2. **Completion.** `deftest` reaches completion only through Pool B in
   `complete_symbols` (src/handlers/completion.rs), which offers a referred
   name **only if** its fqn is already in the index. That fails whenever the
   clojure JAR is not (yet) indexed, and it never fires at all for
   `[clojure.test :refer :all]` or `(:use clojure.test)`, which the ns parser
   currently drops.

### Approach

Add a `DefKind::Deftest` and a small built-in table of *defining macros keyed
by resolved fqn* (`clojure.test/deftest`, `clojure.test/deftest-`,
`cljs.test/deftest`). The extractor resolves a list head to its fqn exactly as
it already does for user `:lint-as` entries, and consults the user map first,
then the built-in table. That reuses the whole lint-as pipeline: symbol
extraction, occurrence walking (head still recorded as a usage, body args as
usages, the test name as a definition), documentSymbol, workspaceSymbol,
references, rename.

For completion, make `clojure.test` names available in every require style:

- Record `:refer :all` and `(:use ns)` namespaces in a new `NsMeta.refer_all`.
- Pool B offers every referred name even when its fqn is not indexed yet
  (the user explicitly referred it, so it is valid regardless of index state).
- A new Pool B2 offers all indexed symbols of each `refer_all` namespace.
- `resolve_symbol` (hover/definition for bare names) falls back to
  `refer_all` namespaces, so `is`/`testing` navigate with `:refer :all` too.

### Key decisions

- **Resolve by fqn, not by bare name.** `deftest` is *not* added to
  `DefKind::from_def_symbol`. Matching the bare name would make `deftest` a
  def form in any file, and would still miss `t/deftest`. Resolving through
  aliases/refers/refer-all is what the codebase already does for `:lint-as`
  and for `defmethod ig/init-key`, and handles `[clojure.test :as t]`,
  `:refer [deftest]`, `:refer :all`, `:use`, and `clojure.test/deftest`.
- **Built-in table lives next to `from_def_symbol`** as
  `DefKind::from_macro_fqn(fqn) -> Option<DefKind>`, a plain `match`. A user
  `:lint-as` entry for the same fqn wins (checked first).
- **One helper for both extractor call sites.** `process_top_level_list`
  and `walk_list` each duplicate "resolve head fqn → look up in lint_as".
  Replace both with `macro_def_kind(head, ns_meta, source, lint_as)`, which
  also tries `refer_all` namespaces for a bare head. One place to get it right.
- **`Deftest` behaves like `def` in the walkers.** It binds no locals and has
  no params vector, so it takes the existing `_`/`!binds_vector` branches in
  `walk_def_form` and `walk_scope_def`. No walker changes beyond the dispatch.
- **Kinds shown to the editor.** documentSymbol/workspaceSymbol:
  `SymbolKind::FUNCTION` (a deftest defines a fn var). Completion:
  `CompletionItemKind::FUNCTION`. Hover label: `deftest`.
- **Jar cache format bumps to 11.** `DefKind` is serialized into
  `JarCacheEntry`; adding a variant and a `NsMeta` field changes the layout
  (see CLAUDE.md invariant). `NsMeta.refer_all` also gets `#[serde(default)]`.
- **Locals walker unchanged.** `locals_in_scope_at` has no `ExtractConfig`
  and `deftest` binds nothing, so generic descent is already correct there.
- **Scope kept to `clojure.test`.** No generic "every referred macro that
  starts with def" heuristic; a future `defflow`/`defspec` gets a one-line
  addition to `from_macro_fqn` or a user `:lint-as` entry.
- **The macro head's occurrence uses the fqn that matched.** `macro_def_kind`
  returns `(fqn, kind)` and `walk_list` pushes that fqn directly, so `deftest`
  navigates to `clojure.test/deftest` in every require style. Other bare
  names in a `:refer :all` body (`is`, `testing`) keep today's
  `record_occurrence` resolution (refers → project defs → core → current ns):
  the extractor has no index, so it cannot tell which refer-all namespace
  owns an arbitrary name. That pre-existing limitation is out of scope here;
  hover/definition on those names still work through the `resolve_symbol`
  fallback, which does have the index.
- **Refer-all completion needs the namespace indexed.** Pool B2 lists
  `ns_symbols` of each refer-all namespace; before the clojure JAR is
  indexed there is nothing to list. Only explicitly referred names get the
  index-free fallback (Pool B), because only their names are known.

### Data flow (test file `test/app/core_test.clj`)

```
(ns app.core-test (:require [clojure.test :refer :all] [app.core :as c]))
(deftest add-works (is (= 3 (c/add 1 2))))
```

1. `extract_ns` → `refer_all = ["clojure.test"]`, `requires` includes it.
2. `process_top_level_list` sees head `deftest`; `from_def_symbol` → None;
   `macro_def_kind` builds candidates `["clojure.test/deftest"]` from
   `refer_all`, `from_macro_fqn` → `Deftest`; `extract_def` emits
   `Symbol { name: "add-works", fqn: "app.core-test/add-works", kind: Deftest, params: [] }`.
3. Occurrence pass: `walk_list` pushes an occurrence of
   `clojure.test/deftest` for the head (the fqn `macro_def_kind` matched),
   then `walk_def_form(Deftest)` skips the name and walks the body, recording
   `=` (core), `c/add` (alias), and `is` (current-ns fallback, see the
   occurrence decision above).
4. documentSymbol → `add-works` (kind 12). Completion of `deft` → Pool B2
   offers `deftest`, `deftest-` when the JAR is indexed; with
   `:refer [deftest]` Pool B offers `deftest` even before it is.

### Testing

- Extractor unit tests (`tests/test_extractor.rs`) over a new snippet
  `tests/fixtures/snippets/deftest_styles.cljc`: refer vector, refer :all,
  alias, full qualification, `:use`, `deftest-`, cljs alias in a reader
  conditional; plus a negative case (no clojure.test require → no symbol).
- Completion unit tests (`tests/test_completion.rs`) with a hand-built index:
  refer-vec name offered without index entry; refer-all names offered from
  `ns_symbols`.
- e2e (`tests/test_e2e.rs`): a `test/` file in `simple_project` (outside
  `:paths`, indexed on didOpen) → documentSymbol lists the deftests,
  workspaceSymbol finds one, completion of `(deft` offers `deftest` via a
  fake `clojure.test` JAR entry in `.cpcache` (same pattern as
  `test_e2e_completion_from_jar_library`).
- `bb check` and `bb e2e` green before claiming done.

---

## File Structure

- Modify `src/index/mod.rs` — `DefKind::Deftest`; `DefKind::from_macro_fqn`;
  `NsMeta.refer_all: Vec<String>`.
- Modify `src/index/extractor.rs` — parse `:refer :all` / `:use`; new
  `macro_def_kind` helper used by `process_top_level_list` and `walk_list`;
  init `refer_all` in `extract_full_with`.
- Modify `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` 10 → 11 with a
  doc-comment line.
- Modify `src/handlers/symbols.rs` — `Deftest => SymbolKind::FUNCTION`.
- Modify `src/handlers/hover.rs` — `Deftest => "deftest"`.
- Modify `src/handlers/completion.rs` — kind mapping; Pool B unresolved
  fallback; Pool B2 refer-all.
- Modify `src/handlers/mod.rs` — `resolve_symbol` refer-all fallback.
- Create `tests/fixtures/snippets/deftest_styles.cljc` — extractor fixture.
- Create `tests/fixtures/simple_project/test/simple/core_test.clj` — e2e fixture.
- Modify `tests/test_extractor.rs`, `tests/test_completion.rs`,
  `tests/test_e2e.rs` — tests.
- Modify `CLAUDE.md` (Invariants) and `docs/ROADMAP.md` — one line each.

Any other file with an exhaustive `match` on `DefKind` will fail to compile
after Task 1; the compiler lists them. Today those are `hover.rs` and
`symbols.rs` only.

---

### Task 1: `DefKind::Deftest`, built-in macro table, `NsMeta.refer_all`

**Files:**
- Modify: `src/index/mod.rs`
- Modify: `src/index/jar_cache.rs`
- Modify: `src/handlers/symbols.rs`
- Modify: `src/handlers/hover.rs`
- Modify: `src/handlers/completion.rs` (kind mapping only)
- Modify: `src/index/extractor.rs` (only the `NsMeta` literal in `extract_full_with`)

- [ ] **Step 1: Add the variant and the fqn table**
  In `src/index/mod.rs` add `Deftest` to `DefKind` (after `Deftype`, before
  `IntegrantKey`) with a doc comment: a `clojure.test/deftest` var, no params.
  Add beside `from_def_symbol`:

  ```rust
  /// Maps a *resolved* list-head fqn of a well-known defining macro to the
  /// `DefKind` it introduces. Consulted after the user's `:lint-as` map, so
  /// a config entry for the same fqn wins.
  pub(crate) fn from_macro_fqn(fqn: &str) -> Option<DefKind>
  ```
  Matching `"clojure.test/deftest" | "clojure.test/deftest-" | "cljs.test/deftest"` → `Deftest`.

- [ ] **Step 2: Add `refer_all` to `NsMeta`**
  Field `pub refer_all: Vec<String>` with `#[serde(default)]` and a doc
  comment: namespaces whose every public var is referred, from
  `[ns :refer :all]` or `(:use ns)`. Fix the one construction site in
  `extract_full_with` (`refer_all: Vec::new()`); the compiler will point at
  any other struct literal (there is one in `src/handlers/completion.rs`
  tests, `meta()`).

- [ ] **Step 3: Exhaustive matches**
  `src/handlers/symbols.rs` `defkind_to_symbol_kind`: `Deftest => SymbolKind::FUNCTION`.
  `src/handlers/hover.rs` `defkind_str`: `Deftest => "deftest"`.
  `src/handlers/completion.rs` `defkind_to_completion_kind`: add `Deftest` to
  the `FUNCTION` arm.

- [ ] **Step 4: Bump the jar cache format**
  `src/index/jar_cache.rs`: `CACHE_FORMAT_VERSION` → `11`; append a
  doc-comment line `11: DefKind::Deftest + NsMeta.refer_all (layout change).`
  matching the existing list.

- [ ] **Step 5: Verify it compiles and existing tests pass**
  Run: `cargo build && cargo test --lib`
  Expected: PASS (the `test_cache_miss_wrong_format_version` test uses
  `CACHE_FORMAT_VERSION - 1`, so it needs no edit).

- [ ] **Step 6: Commit**
  `git commit -am "Add DefKind::Deftest, built-in macro fqn table, NsMeta.refer_all"`

### Task 2: Parse `:refer :all` and `(:use …)`

**Files:**
- Modify: `src/index/extractor.rs` (`parse_require_vector`, `extract_ns`)
- Create: `tests/fixtures/snippets/deftest_styles.cljc`
- Modify: `tests/test_extractor.rs`

- [ ] **Step 1: Write the fixture**
  `tests/fixtures/snippets/deftest_styles.cljc`, realistic content:

  ```clojure
  (ns ^{:doc "Tests in every require style."} my.core-test
    (:require [clojure.test :refer :all]
              [clojure.string :as str]
              #?(:clj [clojure.test :as t] :cljs [cljs.test :as t :include-macros true]))
    (:use [clojure.set]))

  (deftest refer-all-style (is (= 1 1)))
  (t/deftest alias-style (is (str/blank? "")))
  (clojure.test/deftest qualified-style (is true))
  (deftest- private-style (is true))
  (deftest with-body
    (testing "nested"
      (is (= 2 (+ 1 1)))))
  ```
  Keep the `:use` on `clojure.set` so the `:use` path is tested without
  changing which namespace provides `deftest`.

- [ ] **Step 2: Write the failing ns-meta test**
  In `tests/test_extractor.rs`, `test_ns_refer_all_and_use_recorded`: extract
  the fixture; assert `meta.refer_all` contains `"clojure.test"` and
  `"clojure.set"`, `meta.requires` contains both, and
  `meta.aliases["t"]` is `"clojure.test"` (the `:clj` branch; the extractor
  records every branch, so `cljs.test` also lands in aliases via the last
  write; assert only that `t` resolves to one of the two).

- [ ] **Step 3: Run it to verify it fails**
  Run: `cargo test --test test_extractor refer_all`
  Expected: FAIL, `refer_all` is empty.

- [ ] **Step 4: Implement**
  `parse_require_vector`: in the `":refer"` arm, when the next item is a
  `kwd_lit` with text `:all`, push `ns_name` to `ns_meta.refer_all`.
  `extract_ns`: add a `":use"` arm that handles each spec like `:require`
  (bare `sym_lit` or a `vec_lit` whose first item is a `sym_lit`), pushing
  the ns to both `requires` and `refer_all`. Reuse `process_require_spec` for
  the requires side rather than re-parsing, then add the `refer_all` push
  for the resolved ns name. `:use` with `:only` is not expanded (YAGNI).

- [ ] **Step 5: Run the test to verify it passes**
  Run: `cargo test --test test_extractor`
  Expected: PASS, no other extractor test changes.

- [ ] **Step 6: Commit**
  `git add tests/fixtures/snippets/deftest_styles.cljc && git commit -am "Record :refer :all and :use namespaces in NsMeta"`

### Task 3: Extract `deftest` forms as symbols

**Files:**
- Modify: `src/index/extractor.rs`
- Modify: `tests/test_extractor.rs`

- [ ] **Step 1: Write the failing extraction tests**
  `test_extracts_deftest_in_every_require_style`: extract the fixture from
  Task 2; assert symbols `refer-all-style`, `alias-style`,
  `qualified-style`, `private-style`, `with-body` all exist with
  `kind == DefKind::Deftest`, fqn `my.core-test/<name>`, empty `params`,
  `doc == None`, and `name_range` on the test name.
  `test_deftest_without_clojure_test_is_not_a_symbol`: inline source
  `(ns x)\n(deftest foo (is true))` → no symbol named `foo`.
  `test_deftest_occurrences`: use `extract_full` on the fixture; assert
  occurrences with fqn `clojure.test/deftest` exist on the head of the
  refer-all, alias (`t/deftest`), and qualified forms (three ranges, one per
  head line), one with `clojure.test/deftest-`, and **no** occurrence has
  fqn `my.core-test/refer-all-style` (the name is a definition, not a
  usage). Do not assert on `is`: under `:refer :all` it resolves to the
  current ns today (see the Design decision), and that stays unchanged.
  `test_lint_as_overrides_builtin_deftest`: `extract_full_with` and
  `ExtractConfig { lint_as: {"clojure.test/deftest" → DefKind::Def} }` on
  `(ns x (:require [clojure.test :refer [deftest]]))\n(deftest foo 1)` →
  `foo` has kind `Def`.

- [ ] **Step 2: Run them to verify they fail**
  Run: `cargo test --test test_extractor deftest`
  Expected: FAIL, no symbols extracted.

- [ ] **Step 3: Implement the shared helper**
  In `src/index/extractor.rs` add:

  ```rust
  /// The `DefKind` a macro-headed form introduces, with the fqn that matched:
  /// the head's resolved fqn looked up in the user's `:lint-as` map, then in
  /// the built-in table (`DefKind::from_macro_fqn`). A bare head that is not
  /// `:refer`red is also tried against every `:refer :all` / `:use`
  /// namespace. `None` for core def forms (handled by `from_def_symbol`) and
  /// for ordinary calls.
  fn macro_def_kind(
      head: Node,
      ns_meta: &NsMeta,
      source: &str,
      lint_as: &HashMap<String, DefKind>,
  ) -> Option<(String, DefKind)>
  ```
  Candidate fqns, in order: `resolve_head_fqn(head, …)`; then, only when the
  head is bare and not in `ns_meta.refers`, `format!("{ns}/{name}")` for each
  `ns` in `refer_all`. For each candidate return the first hit of
  `lint_as.get(fqn)` then `DefKind::from_macro_fqn(fqn)`, paired with that
  candidate fqn.
  Note `resolve_head_fqn` returns `current-ns/name` for a bare unreferred
  head; that candidate is harmless (nothing maps it) and keeps today's
  behavior for lint-as keys written as the current ns.

- [ ] **Step 4: Use it at both call sites**
  `process_top_level_list`: replace the `or_else(|| resolve_head_fqn(...).and_then(|fqn| cfg.lint_as.get(&fqn).cloned()))`
  with `or_else(|| macro_def_kind(first, ns_meta, source, &cfg.lint_as).map(|(_, kind)| kind))`.
  `walk_list`: replace the same two-line resolution in the lint-as block with
  `macro_def_kind(*head, ctx.ns_meta, ctx.source, ctx.lint_as)`. Instead of
  `record_occurrence(*head, …)` (which would resolve a refer-all bare head
  to the current ns), push `Occurrence { fqn, name_range: node_to_lsp_range(sym_name_node(*head), ctx.source) }`
  with the returned fqn, then `walk_def_form(kind, …)` unchanged. Update the
  block comment to say "a `:lint-as` or built-in defining-macro head".
  `walk_def_form` needs no edit: `Deftest` is not in `binds_vector`, so the
  body is walked as usages from index 2.

- [ ] **Step 5: Run the extractor tests**
  Run: `cargo test --test test_extractor`
  Expected: PASS, including the existing lint-as tests in
  `src/index/extractor.rs` (`cargo test --lib extractor`).

- [ ] **Step 6: Commit**
  `git commit -am "Extract clojure.test deftest forms as Deftest symbols"`

### Task 4: Completion offers referred and refer-all names

**Files:**
- Modify: `src/handlers/completion.rs`
- Modify: `src/handlers/mod.rs`
- Modify: `tests/test_completion.rs`

- [ ] **Step 1: Write the failing completion tests**
  In `tests/test_completion.rs`, build an `Index::new_with_core()` and insert
  by hand (see the `meta()`/`lib_sym()` helpers in `completion.rs`'s own
  test module for shapes; `Index::insert_file` / `insert_lib_file` are the
  entry points):
  - `test_completes_referred_name_before_library_is_indexed`: ns `a.t` with
    `refers = {"deftest" → "clojure.test/deftest"}` and **no**
    `clojure.test` symbols in the index; `complete_symbols(&index, "deft", "a.t")`
    contains label `deftest` with `detail` `"clojure.test (referred)"`.
  - `test_completes_refer_all_namespace_symbols`: ns `a.t` with
    `refer_all = ["clojure.test"]`; library ns `clojure.test` holding
    `deftest`, `deftest-`, `is` inserted with `insert_lib_file`;
    `complete_symbols(&index, "deft", "a.t")` contains `deftest` and
    `deftest-` and not `is`; the `deftest` item's kind is `FUNCTION`
    (Defmacro maps there already).
  - `test_refer_all_does_not_duplicate_explicit_refers`: both `refers` and
    `refer_all` name `deftest` → exactly one `deftest` label.

- [ ] **Step 2: Run them to verify they fail**
  Run: `cargo test --test test_completion refer`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  In `complete_symbols`, bare-prefix branch:
  - Pool B: when `index.symbols.get(fqn)` is `None`, push a
    `CompletionItem { label: refer_name, detail: Some(format!("{} (referred)", ns)), kind: FUNCTION }`
    where `ns` is the fqn's namespace part.
  - Pool B2 (right after B): for each `ns` in `meta.refer_all`, for each fqn
    in `index.ns_symbols.get(ns)`, push `symbol_to_completion(&sym, None)`
    when the name starts with the prefix and the name is not already a key
    in `meta.refers`. Skip `DefnPrivate` symbols (they are indexed for jar
    navigation but not referable).
  In `src/handlers/mod.rs` `resolve_symbol`, bare branch: after the `refers`
  lookup and before `lookup_in_ns(current_ns, …)`, try
  `index.lookup_in_ns(ns, word)` for each `ns` in `meta.refer_all`, returning
  the first hit. Add a sentence to the function's comment.

- [ ] **Step 4: Run the completion tests**
  Run: `cargo test --test test_completion && cargo test --lib completion`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -am "Complete referred names before indexing and :refer :all namespaces"`

### Task 5: End-to-end coverage

**Files:**
- Create: `tests/fixtures/simple_project/test/simple/core_test.clj`
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Add the fixture**
  ```clojure
  (ns simple.core-test
    (:require [clojure.test :refer [deftest is testing]]
              [simple.core :as core]))

  (deftest add-works
    (testing "adds"
      (is (= 3 (core/add 1 2)))))

  (deftest multiply-works
    (is (= 6 (core/multiply 2 3))))
  ```
  `test/` is outside `:paths ["src"]`, so it is indexed on didOpen. Check
  that no existing e2e asserts an exact workspace-symbol count or file list
  for `simple_project` that this file would change (grep
  `setup_project()` tests for `workspace_symbols`; adjust an assertion only
  if it enumerates every symbol).

- [ ] **Step 2: Write `test_e2e_deftest_outline_and_completion`**
  Follow `test_e2e_completion_from_jar_library`: write a fake JAR with
  `clojure/test.clj` containing `(ns clojure.test)` and
  `(defmacro deftest "Defines a test." [name & body] …)`,
  `(defmacro deftest- [name & body] …)`, `(defmacro is [form] …)`; point
  `.cpcache/1.cp` at it; `LspClient::start`, `initialize`,
  `wait_for_log("library indexing complete")`, `did_open` the fixture.
  Assert:
  - `document_symbols` names == `["add-works", "multiply-works"]`, kind 12,
    `selectionRange` on the name line.
  - `workspace_symbols("add-works")` first hit has `containerName`
    `simple.core-test`.
  - `did_change_insert` a new line `(deft` at end of file; `completion` at
    that position contains labels `deftest` and `deftest-`.
  - references on `add-works` (name position) returns exactly the definition
    (no self-usage); use the existing `references` helper if present, else
    skip this sub-assertion and say so in the commit message.

- [ ] **Step 3: Write `test_e2e_deftest_refer_all_completion`**
  Same JAR; overwrite the fixture text in a temp copy with
  `(:require [clojure.test :refer :all])`; `did_open`; completion of `(deft`
  contains `deftest`; hover on `is` inside a test body is non-null
  (`resolve_symbol` refer-all fallback).

- [ ] **Step 4: Run the e2e suite**
  Run: `bb e2e`
  Expected: PASS, both new tests and every existing one.

- [ ] **Step 5: Commit**
  `git add tests/fixtures/simple_project/test && git commit -am "e2e: deftest outline, workspace search, completion"`

### Task 6: Full check, editor run, docs

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Full verification**
  Run: `bb check` then `bb e2e-nvim`
  Expected: fmt clean, clippy clean under `-D warnings`, all tests pass.
  If `bb e2e-nvim` is unavailable on this box (no `nvim`), state that in the
  final report rather than skipping silently.

- [ ] **Step 2: Docs**
  CLAUDE.md, Invariants: one bullet after the `:lint-as` mention (or at the
  end): "Defining macros resolve by fqn: user `:lint-as` first, then the
  built-in table `DefKind::from_macro_fqn` (`clojure.test/deftest` and
  friends). `NsMeta.refer_all` records `:refer :all`/`:use` namespaces;
  head resolution, completion and `resolve_symbol` all consult it."
  `docs/ROADMAP.md`: tick or add a line for deftest outline/completion.

- [ ] **Step 3: Commit**
  `git commit -am "docs: deftest symbols and refer-all resolution"`
