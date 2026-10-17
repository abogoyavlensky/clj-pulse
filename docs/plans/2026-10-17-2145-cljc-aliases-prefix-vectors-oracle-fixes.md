# `.cljc` Split Aliases, Vector Prefix Lists and Two Oracle Fixes Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `.cljc` ns form whose alias is split by a reader conditional resolves through its Clojure branch; a prefix list written as a vector (`[a.b [c :as x]]`) binds like the list form; and `bb compare` stops reporting two oracle-side differences as new divergences.

**Tech Stack:** Rust, tree-sitter-clojure, the `bb compare` harness (`tests/common/oracle.rs`, `tests/test_compare.rs`).

---

## Design

### Evidence (2026-10-17 `bb compare`, `docs/MEMORY.md` "Compare after oracle triage")

- **Split alias, penpot, about 87 answers.** `common/src/app/common/data/macros.cljc` requires
  `#?(:clj [clojure.core :as c] :cljs [cljs.core :as c])`. `c/get` lands in
  `cljs/core.cljs`; kondo expects `clojure/core.clj`. `s/def` and `s/with-gen` in
  `common/src/app/common/spec.cljc` land in the ClojureScript jar the same way.
- **Vector prefix list, clj-kondo, about 40 answers.**
  `parser/clj_kondo/impl/rewrite_clj/node/indent.clj` requires
  `[clj-kondo.impl.rewrite-clj.node [protocols :as node]]`. Clojure's `load-libs`
  reads a vector whose second element is not a keyword as a prefix list, so `node`
  names `…node.protocols`. We read it as a libspec of `…node` with no alias:
  `node/string`, `node/sexprs` and `ws/with-newline-fn` answer null (16), and the
  `var-def/defprotocol+` rows miss the `node/Node` callers (26).
- **Oracle: `.cljs` `declare`, 7 answers.** kondo's `defined-by` for a declare in a
  `.cljs` file is `cljs.core/declare`; `oracle::probes` only recognises
  `clojure.core/declare`, so the declare becomes the primary definition and the
  expected landing (`edn.cljs:27` for `macros` instead of the real `defn`).
- **Oracle: sites in `(comment …)`, 13 answers.** The oracle runs kondo with
  `:skip-comments true`; clj-pulse lists usages inside `(comment …)` (and a rename
  must rewrite them). metabase's `sql-format-quoted` has 13 such extra lines in
  `semantic_search/index.clj`.

### 1. A `.cljc` file reads the Clojure branch of a split ns form

A `.cljc` file asks as Clojure (AGENTS.md), but three readers walk a reader
conditional's branches in source order, so the last branch — usually `:cljs` —
wins every `aliases` / `refers` slot the branches share:

- `process_require_spec`'s `read_cond_lit | splicing_read_cond_lit` arm (a
  conditional *inside* `:require`);
- `process_reader_conditional` (a conditional around top-level forms, including
  `#?(:clj (ns …) :cljs (ns …))`);
- `ns_meta_of_tree`'s `read_cond_lit` arm (the locals walker's copy of the ns
  metadata).

All three switch to one helper that orders the branches so the one the Clojure
reader takes is applied **last**:

```rust
/// The branch forms of a reader conditional, the one the Clojure reader takes
/// (the first `:clj` or `:default` branch) last, so it wins every slot the
/// branches share; the others still record what only they bind.
fn reader_branches_clojure_last<'a>(node: Node<'a>, source: &str) -> Vec<Node<'a>>
```

Every branch is still read, so a `:cljs`-only alias stays bound (no
`unresolved-namespace` on a `.cljc` file's ClojureScript half), and rename's
alias sites (`collect_alias_declarations_in_spec`) keep reading every branch as
they do now. Requires and `:use` are unions, so their order does not matter.
The occurrence walker reads `ns_meta`, so `c/get` is recorded under
`clojure.core/get` and references follow. Extractor output changes for library
`.cljc` files, so `CACHE_FORMAT_VERSION` goes 25 → 26.

### 2. A vector prefix list binds like a list prefix list

Clojure's `libspec?` is a symbol, or a vector whose second element is absent or a
keyword; any other sequential spec is a prefix list. Today only `list_lit` prefix
lists expand. A `vec_lit` whose first item is a `sym_lit` and whose second item
exists and is a `sym_lit` or `vec_lit` is a prefix list too, in the three readers
that expand list prefix lists:

- `process_require_spec` (aliases, refers, requires);
- `collect_use_namespaces` (`:use`);
- `collect_alias_declarations_in_spec` (rename's alias declarations).

Each moves its list-arm body into a shared path both kinds reach, so the vector
form behaves exactly like the list form, including what it does not do (a
reader conditional inside a prefix list stays out of scope).
`collect_refer_occurrences` handles no prefix list today, list or vector; that
stays as is. clean-ns and the unused-namespace lint read a vector prefix list as
a plain side-effecting libspec of the prefix, which they never flag or remove;
that stays as is too.

### 3. Oracle: a `.cljs` declare is a declare

`is_declare` in `oracle::probes` accepts `clojure.core/declare` and
`cljs.core/declare`.

### 4. Oracle: sites inside `(comment …)` are known

`Verdict::Diverge` gains `extra_lines: BTreeSet<(PathBuf, u32)>` beside
`missing_lines`, filled by `judge_sites` (lines the answer counts more often than
the oracle). A `KNOWN` entry for `var-def/`, `local/` and `keyword/` buckets
matches a `Diverge` with `missing == 0`, a non-empty `extra_lines`, and every
extra line inside a `(comment …)` form of its file. "Inside" is decided by a
helper that parses the file (`clj_pulse::index::extractor::parse_tree`) and
collects the line spans of every `list_lit` headed by the symbol `comment`,
nested ones included. Reason: "a site inside `(comment …)`: clj-pulse lists it
and a rename must rewrite it; the oracle runs kondo with `:skip-comments true`
(2026-10-17)".

### Not in scope

`:refer :all` references, `(load …)` / `(in-ns …)` files, keywords in
non-Integrant `.edn`, defs nested in wrapping forms, and names kondo hooks define
— the other findings of the same triage, tracked in ROADMAP.

### Testing

- Extractor tests: split alias in `:require` (Clojure branch wins, a `:cljs`-only
  alias stays bound, a `:default` branch counts as Clojure), split `ns` forms,
  `ns_meta_of_tree` agreeing with `extract`, vector prefix lists in `:require`,
  `:use` and alias sites.
- A definition e2e test on a `.cljc` fixture file with a split alias to a project
  namespace (no JAR needed): `x/f` lands in the `:clj` namespace's file.
- Harness unit tests for the two oracle changes.
- Gates: `bb check`, `bb e2e`, `bb compare` (clj-kondo and penpot; record the
  tables), `bb bench` (extractor change).

## File Structure

- Modify `src/index/extractor.rs` — `reader_branches_clojure_last`; the three
  reader-conditional readers; vector prefix lists in the three prefix-list readers.
- Modify `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` 26 with its doc line.
- Modify `tests/test_extractor.rs` — extractor tests.
- Modify `tests/test_e2e.rs` and `tests/fixtures/simple_project/…` — one `.cljc`
  fixture file and its e2e test.
- Modify `tests/common/oracle.rs` — `is_declare`; a `comment_lines` helper.
- Modify `tests/test_compare.rs` — `extra_lines`, the `KNOWN` entry, unit tests.
- Modify `AGENTS.md`, `docs/FEATURES.md`, `docs/MEMORY.md`, `docs/ROADMAP.md`.

## Tasks

### Task 0: Open the ROADMAP item

- [x] **Step 1:** Add a Milestone 5 item to `docs/ROADMAP.md` (beside the
  2026-10-16 compare items): "**`.cljc` split aliases and vector prefix
  lists**", one line of evidence, and
  `Plan: [2026-10-17-2145-cljc-aliases-prefix-vectors-oracle-fixes.md](plans/2026-10-17-2145-cljc-aliases-prefix-vectors-oracle-fixes.md) — in progress`.
- [x] **Step 2: Commit** with the plan: `git commit -m "roadmap: plan .cljc split aliases and vector prefix lists"`

### Task 1: Clojure branch wins in a `.cljc` ns form

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [x] **Step 1: Write failing extractor tests** next to the existing `#?@` require
  tests (search for `#?@(:clj [[e :as h]]`): in a `t.cljc` file,
  - `(ns t (:require #?(:clj [clojure.core :as c] :cljs [cljs.core :as c])))` →
    `ns_meta.aliases["c"] == "clojure.core"`;
  - the same with the branches written `:cljs` first → still `clojure.core`;
  - `#?(:cljs [goog.object :as gobj])` alone → `gobj` bound to `goog.object`;
  - `#?(:default [a :as x] :cljs [b :as x])` → `x` → `a`;
  - `#?(:default [a :as x] :clj [b :as x])` → `x` → `a` (the *first* of
    `:clj`/`:default` is the Clojure branch);
  - `#?(:clj (ns t (:require [a :as h])) :cljs (ns t (:require [b :as h])))` →
    `h` → `a`;
  - an occurrence of `c/get` in the first file is recorded under `clojure.core/get`;
  - `ns_meta_of_tree` agrees (through whatever public entry the existing tests use
    for the locals walker's ns metadata; if none, assert through a local that
    binds via a `c/let` head under the split alias).
  Run: `cargo test --test test_extractor cljc` — Expected: the new tests FAIL.
- [x] **Step 2: Add `reader_branches_clojure_last`** (signature in Design §1) and
  use it in `process_require_spec`, `process_reader_conditional` and
  `ns_meta_of_tree`. A branch's key is the `kwd_lit` before it; the Clojure branch
  is the first whose key is `:clj` or `:default`.
- [x] **Step 3: Run** `cargo test --test test_extractor` — Expected: PASS.
- [x] **Step 4: Bump** `CACHE_FORMAT_VERSION` to 26 in `src/index/jar_cache.rs`
  with a doc line: "26: a `.cljc` reader conditional in an ns form resolves
  through its Clojure branch".
- [x] **Step 5: Commit** `git commit -m "extractor: a .cljc split alias resolves through its Clojure branch"`

> Deviation: the helper is `reader_branches_clojure_wins` and yields the Clojure branch both first and last: codex found that last alone reversed `refer_all`, which is searched first-to-last (`:clj clojure.test :refer :all` / `:default cljs.test …` made `deftest` resolve to `cljs.test`). Every list the ns readers fill is de-duplicated, so reading the branch twice is idempotent.
> Deviation: `process_reader_conditional` orders only the `ns` branches that way; def branches stay in source order, since documentSymbol is unsorted and reordering them would shuffle the outline.
> Deviation: `test_deftest_occurrences` pinned the old last-branch-wins behavior (`t/deftest` → `cljs.test`); updated to `clojure.test`.


### Task 2: Vector prefix lists

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [x] **Step 1: Write failing extractor tests**:
  - `(ns t (:require [clj-kondo.impl.rewrite-clj.node [protocols :as node] [seq :as s] utils]))`
    → aliases `node` → `clj-kondo.impl.rewrite-clj.node.protocols`, `s` →
    `…node.seq`; requires hold `…node.protocols`, `…node.seq`, `…node.utils` and
    not `clj-kondo.impl.rewrite-clj.node`;
  - `[a.b [c :refer [f]]]` → refer `f` → `a.b.c`;
  - `[a.b :as x]`, `[a.b]` and `[a.b :refer [f]]` unchanged (libspecs);
  - `(:use [a.b c [d :only [f]]])` → refer-all `a.b.c`, `a.b.d`;
  - `alias_sites_tree` finds the `node` declaration inside the vector prefix list.
  Run: `cargo test --test test_extractor prefix` — Expected: the new tests FAIL.
- [x] **Step 2: Implement.** Add `fn is_prefix_vector(items: &[Node]) -> bool`
  (first `sym_lit`, second present and `sym_lit` or `vec_lit`). In each of the
  three readers, route a `vec_lit` that `is_prefix_vector` accepts to the same
  code the `list_lit` arm runs, extracting that body into a shared function per
  reader where it is not one already.
- [x] **Step 3: Run** `cargo test --test test_extractor` and
  `cargo test --lib` (diagnostics' `prefix_list_binds_the_joined_name_only` and
  code-action tests) — Expected: PASS. Add a vector case to
  `prefix_list_binds_the_joined_name_only` in `src/diagnostics.rs`.
- [x] **Step 4: Commit** `git commit -m "extractor: a vector prefix list binds like a list prefix list"`
  (Task 1 already bumped the cache version; one bump covers both.)

### Task 3: E2E on a `.cljc` split alias

**Files:**
- Create: `tests/fixtures/simple_project/src/fixture/split.cljc` (or the
  fixture's existing source dir — check `tests/fixtures/simple_project`)
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Write the fixture and test.** The `.cljc` file requires
  `#?(:clj [<a project ns> :as x] :cljs [<another project ns> :as x])`, both
  defining `f`, and calls `(x/f)`. The test opens it, asks definition on `f` in
  `x/f`, and asserts the location is the `:clj` namespace's file. Model it on an
  existing definition e2e test in `tests/test_e2e.rs`. Check that adding a file to
  the fixture does not change other tests' expectations (symbol counts,
  workspace/symbol lists); adjust only those that count fixture files.
- [x] **Step 2: Run** `bb e2e` — Expected: PASS, the new test included.
- [x] **Step 3: Commit** `git commit -m "e2e: definition through a .cljc split alias"`

> Deviation: the `:cljs` branch names `goog.object`, not a second project namespace. With a project `simple.split-js`, `compare_simple_project` reported two new divergences: kondo also reads the `.cljc` as ClojureScript, and we read only its Clojure half. The e2e still separates old from new behavior (the old one answered null).

### Task 4: Oracle fixes

**Files:**
- Modify: `tests/common/oracle.rs`, `tests/test_compare.rs`

- [ ] **Step 1: `is_declare`** accepts `cljs.core/declare` too.
- [ ] **Step 2: `extra_lines`.** Add the field to `Verdict::Diverge`, fill it in
  `judge_sites` (mirror of `missing_lines`), `BTreeSet::new()` elsewhere.
- [ ] **Step 3: `comment_lines`.** In `oracle.rs`, `pub fn comment_lines(file:
  &Path) -> Vec<(u32, u32)>`: 0-based inclusive line spans of every `list_lit`
  whose first named child is the symbol `comment`, at any depth.
- [ ] **Step 4: `KNOWN` entry** (Design §4), with bucket prefix `""` and the
  bucket filter inside the matcher (`var-def/`, `local/`, `keyword/`).
- [ ] **Step 5: Unit tests** in `test_compare.rs`'s test module: a minimal
  `Analysis` with a `cljs.core/declare` var-definition and a later `defn` of the
  same fqn yields a usage probe expecting the `defn`'s line; a references
  probe whose only extra line falls in a `(comment …)` of a temp file is known;
  an extra line outside it is new; a missing line keeps it new. A test for
  `comment_lines` on a nested comment.
  Run: `cargo test --release --test test_compare` — Expected: PASS.
- [ ] **Step 6: Commit** `git commit -m "compare: cljs.core/declare is a declare; sites in (comment …) are known"`

### Task 5: Gates, records, docs

- [ ] **Step 1:** `bb check` — Expected: green.
- [ ] **Step 2:** `bb e2e`, `bb e2e-pulse` and `bb e2e-calva` (definition
  answers change) — Expected: green.
- [ ] **Step 3:** `bb compare` (clj-kondo, then penpot). Expected: penpot
  `var-usage/core/aliased` and `var-usage/library/macro/aliased` divergences drop;
  clj-kondo `var-usage/project/aliased` and `project/macro/aliased` nulls drop,
  `var-def/defprotocol+` moves to known; the `edn.cljs` declare probes agree.
  Record both tables in `docs/MEMORY.md` under a new dated section, and check
  `agree` did not fall in any row beyond resampling.
- [ ] **Step 4:** `bb bench` (extractor change). Compare with the 2026-10-17 A/B
  in `docs/MEMORY.md`; record a one-line note.
- [ ] **Step 5: Docs.** AGENTS.md: the `.cljc` invariant gains "an ns form's
  reader conditional resolves through its Clojure branch
  (`reader_branches_clojure_last`)", and the prefix-list sentence names vector
  prefix lists; the compare testing notes mention the comment-sites entry.
  `docs/FEATURES.md`: the same two behaviours where `.cljc` and requires are
  described. README: check its support-boundaries lines against the change
  (likely no edit). `docs/ROADMAP.md`: tick the item, status `done`, and trim
  the "bb compare leftovers" line (the `sql-format-quoted` sites are explained).
- [ ] **Step 6: Commit** `git commit -m "docs: .cljc split aliases, vector prefix lists, compare records"`
