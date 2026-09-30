# Method-param locals, `:lint-as` in the locals walker, and `:refer … :rename` sites Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Close three backlog items so the locals walker and the occurrence walker agree about what binds, and a var rename stops corrupting a `:refer … :rename` require.

**Status:** completed 2026-09-30.

**Tech Stack:** Rust, tree-sitter-clojure, tower-lsp; tests in `tests/test_extractor.rs`, `tests/test_e2e.rs`, `bb compare`.

---

## Design

Three backlog items, one theme: two walkers in `src/index/extractor.rs` must
agree about what binds, and rename must edit only tokens that spell the name.

| # | Backlog item | Date |
|---|---|---|
| A | Method params in `deftype`/`defrecord`/`extend-protocol`/`extend-type`/`reify` bodies are not locals ([issue](../backlog/2026-09-30-method-params-in-type-bodies-are-not-locals.md)) | 2026-09-30 |
| B | `:lint-as` in the locals walker (and bare `are` without clojure.test) | 2026-09-10 |
| C | Renaming a var referred under another name (`:refer [foo] :rename {foo f}`) | 2026-09-29 |

"Defs nested in a wrapping macro" (2026-09-17) is **out of scope** and stays
in the backlog.

### Background: the two walkers

- The **occurrence walker** (`walk_occurrences` → `walk_list`, around
  `extractor.rs:2045-2214`) runs at extraction with an `OccurrenceCtx`
  (`source`, `ns_meta`, `lint_as`, …). It classifies a list head through
  `head_def_kind` (`:lint-as`, the built-in macro table, then the name-part
  rule), `are_head_fqn`, and `head_is_core_form` (alias-resolved), and it
  binds method params through `walk_type_specs` / `walk_method_impl`.
- The **locals walker** (`walk_scope` and its `walk_scope_*` helpers, around
  `extractor.rs:3189-3571`) answers "which locals are in scope at `pos`" for
  definition, completion, references, rename and highlight. It takes only
  `source`, so it matches `are` by name part, applies the name-part def rule
  alone, has no `:lint-as`, does not resolve a `clojure.core` alias, and has
  no arm for method implementations.

### A. Method params bind in the locals walker

Add the scope-walker twin of `walk_type_specs`/`walk_method_impl`:

- A helper (suggested name `walk_scope_type_specs`) takes the spec nodes and
  finds the one containing `pos`. A `list_lit` spec whose first named child is
  a `sym_lit` is a method implementation: hand everything after the method
  name to the existing `walk_scope_fn_tail`, which already handles a single
  `[params] body…` tail, multi-arity `([params] body…)` lists, `:or` defaults
  and `:-` schema annotations (the same reuse `walk_scope_letfn` makes). Any
  other spec (a protocol/type symbol, a non-method list) descends through
  `walk_scope` as today.
- Call sites, mirroring `walk_list` exactly:
  - `walk_scope_def`, `Defrecord | Deftype` arm: fields bind first (as now),
    then specs are `children[3..]` through the new helper. A method param
    that shadows a field wins because it is pushed later and every reader
    takes the last match (`.rev().find`).
  - New core-form heads in `walk_scope`: `extend-type` (specs `children[2..]`,
    `children[1]` descends generically), `extend-protocol` (specs
    `children[2..]`, `children[1]` generically), `reify` (specs
    `children[1..]`).
- No protocol-namespace bookkeeping is needed here: the locals walker only
  binds, it records nothing.
- `proxy`, `definterface` and `defprotocol` are not touched: the occurrence
  walker has no method arm for them either, and the point is agreement.

### B. The locals walker resolves heads like the occurrence walker

Thread a context through the locals walker instead of a bare `source`:

```rust
struct ScopeCtx<'a> {
    source: &'a str,
    ns_meta: &'a NsMeta,
    lint_as: &'a HashMap<String, DefKind>,
}
```

- Every private `walk_scope*` function, `descend_into` and `locals_at_node`
  take `&ScopeCtx` where they took `source`. Helpers that only read text
  (`collect_binding_targets`, `pos_in_or_default`, …) keep `source`.
- `walk_scope` dispatch becomes the same sequence `walk_list` uses:
  1. `head_def_kind(&children, ns_meta, source, lint_as)` → `walk_scope_def`
     with that kind. This covers `:lint-as`, the built-in table (`deftest`)
     and the qualified name-part rule in one call, so a head `:lint-as` maps
     to a non-fn kind (`def`, `declare`, …) no longer binds its vector.
  2. `are`: second child is a `vec_lit` **and** `are_head_fqn(head, ns_meta,
     source)` is some → `walk_scope_are`. A bare `are` in a file that never
     pulls in `clojure.test`/`cljs.test` stops binding — agreement with the
     occurrence walker, which already treats it as a plain call.
  3. Core forms: the head is unqualified, or its qualifier resolves *through
     `ns_meta.aliases`* to `clojure.core`/`cljs.core` — the rule of
     `head_is_core_form`, so `cc/let` now binds too. Factor the shared check
     so both walkers call one function taking `(head, ns_meta, source)`.
- **Where `ns_meta` comes from:** the live tree, never the index — the
  buffer may hold an unsaved `ns` edit and the tree is what every other
  local answer is read from. Add an ns-only pass (suggested
  `ns_meta_tree(tree, source, path)`): it reuses the same code
  `extract_analysis_tree` runs for the `ns` form (top-level and
  reader-conditional branches) but extracts no symbols and walks no
  occurrences. If the existing code cannot be reused without also building
  symbols, extract the ns-handling branch of `process_top_level_list` into a
  function both call; do not write a second ns parser. It is computed
  **once** per public call — `local_references_at_tree` calls
  `locals_at_node` once per occurrence of the name, so the context must be
  built before that loop.
- **Public API** (follows the `file_occurrences` / `file_occurrences_with` /
  `_tree` pattern already in the file):

```rust
pub fn locals_in_scope_at(source: &str, pos: Position) -> Vec<LocalBinding>;            // default config, unchanged signature
pub fn locals_in_scope_at_tree(tree: &Tree, source: &str, path: &Path, pos: Position, cfg: &ExtractConfig) -> Vec<LocalBinding>;
pub fn local_references_at(source: &str, pos: Position, name: &str) -> Option<LocalRefs>; // default config, unchanged signature
pub fn local_references_at_tree(tree: &Tree, source: &str, path: &Path, pos: Position, name: &str, cfg: &ExtractConfig) -> Option<LocalRefs>;
```

  `path` is there only if the ns pass needs it (dialect, file for `NsMeta`);
  drop the parameter if it does not. The non-tree variants parse and use
  `ExtractConfig::default()` with a placeholder path, so the many existing
  unit tests keep compiling. Add `_with` variants only if a test needs a
  config without a tree.
- **Callers to update** (each already holds `index` or can reach it;
  `index.extract_config()` is the config): `handlers/definition.rs:140`,
  `handlers/completion.rs:88`, `handlers/references.rs:134`
  (`local_refs_at`) and `:151` (`reject_local_capture`), and whatever
  `highlight.rs` reaches through `local_refs_at`. `local_refs_at` and
  `local_name_at` gain an `index: &Index` (or `&ExtractConfig`) parameter;
  follow it through their callers.
- Other internal users of the scope walk (`are_template_of_argv`,
  `is_destructured_key`, anything else calling `locals_at_node`) get the
  context passed down; none may rebuild it.
- Existing tests that rely on a bare `are` binding without an `ns` form that
  requires clojure.test must gain the require; that behavior change is the
  point of the item.
- Update the doc comment above `locals_in_scope_at` (it documents both
  limitations this plan removes) and the comments inside `walk_scope`.

### C. `:refer [foo] :rename {foo f}`

Today `(f)` is recorded as an occurrence of `a/foo` over the token `f`, and
the `:rename` map's key is no occurrence. Renaming `foo` → `bar` rewrites
`(f)` to `(bar)` and leaves `{foo f}` — broken code.

- **The key is a site.** In `collect_refer_occurrences`, for each libspec
  also read `:rename {from to}` maps and record each `from` symbol as an
  occurrence of `ns/from` over its name range. The `to` symbol records
  nothing. Prefix-list libspecs: handle whatever shapes the function handles
  today for `:refer`, no more. `(:refer-clojure :rename …)` is untouched —
  core vars cannot be renamed.
- **The call keeps its occurrence** (references and highlight should still
  list `(f)` as a use of `a/foo`, as clj-kondo does), but **rename edits only
  tokens that spell the old name.** In `references::rename`'s `Global` arm,
  for each file's occurrences get the file's text (the open document's
  snapshot, else `std::fs::read_to_string`) and keep an occurrence only when
  `token_at(text, occ.name_range)` equals the old name (`sym.name`).
  Constructor sites are already narrowed to the type name and qualified
  usages' ranges cover the name part, so both pass. When a file's text
  cannot be read, keep its occurrences (today's behavior). `occurrences_for`
  already holds live text and reads constructor-calling files; extend it or
  add a rename-only sibling rather than reading files twice.
- **A cursor on `f` refuses, from both `prepareRename` and `rename`.** In
  `rename_target`, once the fqn path resolved a project `sym`: if
  `occurrence_range_at(…, &fqn)` answers a range whose text in this document
  is not `sym.name`, bail with

  `cannot rename '{token}' here: it is '{fqn}' under a :rename in this namespace's require — rename '{name}' at its definition or a plain usage`

  Renaming the local name `f` itself (a file-local rename, like an alias) is
  **not** built: nothing asked for it.
- The cursor on the `:rename` key `foo` now resolves (it is an occurrence)
  and renames the var like any usage: the definition, the `:refer [foo]`
  entry, the key, and every `foo`-spelled usage change; `f` and `(f)` stay.

### Cache, gates, docs

- Extractor output changes (C adds occurrences): bump
  `CACHE_FORMAT_VERSION` 21 → 22 in `src/index/jar_cache.rs`.
- Gates: `bb check`, `bb e2e`, `bb e2e-pulse` (client-visible answers
  change), `bb e2e-calva` (definition answers change), `bb compare clj-kondo` (extractor/resolver/rename), and
  `bb bench clj-kondo` for one reason: B adds an ns-only pass to every
  locals question, so the definition median must not move noticeably. If it
  does, the ns pass is doing more than reading the `ns` form.
- Expected compare movement on clj-kondo: `local/plain` from 585 agree / 3
  diverge / 6 null of 594 to 594 agree. No `KNOWN` entry covers these nine,
  so none is removed; if any of the nine remains, triage it — do not
  allowlist it.

## File Structure

| File | Change |
|---|---|
| `src/index/extractor.rs` | `ScopeCtx`; `walk_scope*` take it; type-spec/method arm; shared core-head check; ns-only pass; new public signatures; `:rename` key occurrences in `collect_refer_occurrences`; doc comments |
| `src/index/jar_cache.rs` | `CACHE_FORMAT_VERSION` 22 |
| `src/handlers/references.rs` | `local_refs_at`/`local_name_at`/`reject_local_capture` pass config; spelling filter in `rename`; refusal in `rename_target` |
| `src/handlers/definition.rs`, `completion.rs`, `highlight.rs` | pass path + config to the locals API |
| `tests/test_extractor.rs` | unit tests for A, B, C |
| `tests/test_e2e.rs` | rename/prepareRename over `:refer … :rename`; definition on a method param |
| `docs/ROADMAP.md` | new Milestone 5 item with `Plan:` line; three Backlog lines removed at the end |
| `docs/backlog/2026-09-30-method-params-in-type-bodies-are-not-locals.md` | moved to `docs/archive/`, status closed |
| `docs/MEMORY.md` | compare table/notes for the re-run |
| `CLAUDE.md` (and `AGENTS.md` if it is a separate file — check; keep them in step) | invariants updated |
| `docs/FEATURES.md` | one line each where locals and rename behavior are described, if they are |

## Tasks

### Task 0: Branch and ROADMAP entry

**Files:** `docs/ROADMAP.md`

- [x] **Step 1:** `git checkout -b locals-method-params-lint-as-refer-rename`
- [x] **Step 2:** In Milestone 5, above the `Release` item, add an unticked
  item "**Method-param locals, `:lint-as` in the locals walker, and
  `:refer … :rename` sites.**" summarizing A, B, C in the style of its
  neighbors, naming the three Backlog entries by date, with
  `Plan: [2026-09-30-2257-method-params-lint-as-locals-refer-rename.md](plans/2026-09-30-2257-method-params-lint-as-locals-refer-rename.md) — in progress`.
  Leave the Backlog lines in place until Task 6.
- [x] **Step 3:** `git commit -m "roadmap: schedule method-param locals, lint-as locals and refer-rename sites"`

### Task 1: Method params bind in the locals walker (A)

**Files:** `src/index/extractor.rs`, `tests/test_extractor.rs`

- [x] **Step 1: Failing tests** in `tests/test_extractor.rs` (a new `mod
  method_params` beside the existing locals tests, using
  `locals_in_scope_at` / `local_references_at`):
  - `(extend-protocol P Object (m [v] v))`: the body `v` resolves to the
    argv `v`; references from the argv list the body use.
  - `(extend-type T P (m [this x] (f x)))`: `x` in scope in the body.
  - `(deftype T [reader] P (get-line [reader] reader))`: the body `reader`
    resolves to the **param**, not the field; a second method without that
    param resolves `reader` to the field.
  - `(defrecord R [a] P (m [_ b] (+ a b)))`: both `a` (field) and `b` in
    scope.
  - `(reify P (m [this y] y))`.
  - Multi-arity method `(m ([x] x) ([x y] y))`: `y` only in the second arity.
  - A cursor on the protocol symbol or the method name yields no param.
  - Realistic shape from the issue: `(replace-children [this children']
    (assoc this :children children'))`.
- [x] **Step 2:** `cargo test --test test_extractor method_params` — FAIL.
- [x] **Step 3: Implement** the helper and the four call sites per Design A.
  Keep the head checks inside the existing `core_form` block (Task 2 reworks
  that block; do not pre-empt it).
- [x] **Step 4:** `cargo test --test test_extractor` — PASS; `cargo test --lib` — PASS.
- [x] **Step 5:** `git commit -m "locals: bind method params in type and protocol bodies"`

### Task 2: `ScopeCtx` — the locals walker resolves heads like the occurrence walker (B)

**Files:** `src/index/extractor.rs`, `src/handlers/references.rs`,
`src/handlers/definition.rs`, `src/handlers/completion.rs`,
`src/handlers/highlight.rs`, `tests/test_extractor.rs`

- [x] **Step 1: Failing tests** (`tests/test_extractor.rs`, using the
  `_tree` variants with an `ExtractConfig` whose `lint_as` is filled, as
  `extractor.rs:4000` does):
  - `:lint-as {my.lib/defcomponent clojure.core/defn}` with
    `(ns app (:require [my.lib :refer [defcomponent]]))
    (defcomponent c [x] x)`: `x` is a local in the body (bare referred head —
    the name-part rule cannot see it today).
  - Config outranks the name-part fallback: `:lint-as {my.lib/defn
    clojure.core/def}` with `(ns app (:require [my.lib :as lib]))
    (lib/defn cfg [a b])`: a cursor on `a` finds **no** local (the vector is
    an initializer expression), where today the name part `defn` binds it.
  - Bare `(are [x] (= x 1) 1)` in a file with no clojure.test require: `x`
    is not a local in the template. With `(:require [clojure.test :refer
    [are]])`, `:refer :all`, or `t/are` under `[clojure.test :as t]`: it is.
  - `(ns a (:require [clojure.core :as cc])) (cc/let [y 1] y)`: `y` is a
    local.
  - `are_template_of_argv` classifies the head the same way: a quoted
    `'x` in the template of a bare `(are [x] …)` without clojure.test is not
    a usage of anything (the argv binds nothing there); with clojure.test
    required it is still a usage of the argv `x`, as today.
  - An unsaved-edit shape: the ns form in the *source given* decides, no
    index involved (covered by construction — the tests pass only source).
  - `local_references_at_tree` and `locals_in_scope_at_tree` still agree
    with their non-tree twins under the default config (extend the existing
    equivalence test near `test_extractor.rs:2100`).
- [x] **Step 2:** run them — FAIL (compile errors count; land the signature
  first if needed to get a red test run).
- [x] **Step 3: Implement** per Design B: `ScopeCtx`, the ns-only pass, the
  dispatch order, the shared core-head check, new public signatures, caller
  updates. Fix existing tests whose bare `are` needs a clojure.test require.
- [x] **Step 4:** Update the doc comment on `locals_in_scope_at` and the
  comments in `walk_scope` that describe the removed limitations.
- [x] **Step 5:** `cargo test` — PASS (whole suite: handler callers changed).
- [x] **Step 6:** `git commit -m "locals: resolve heads through ns metadata and :lint-as"`

### Task 3: `:rename` keys are sites (C, extractor half)

**Files:** `src/index/extractor.rs`, `src/index/jar_cache.rs`, `tests/test_extractor.rs`

- [x] **Step 1: Failing test:** for
  `(ns b (:require [a :refer [foo] :rename {foo f}])) (f)` the occurrences of
  `a/foo` are exactly three: the `:refer` entry, the `:rename` key, the call
  token `f`; the map value `f` records nothing. Also a `:require-macros`
  libspec with `:rename`, and a `:rename` written before its `:refer`.
- [x] **Step 2:** run — FAIL.
- [x] **Step 3: Implement** in `collect_refer_occurrences`; bump
  `CACHE_FORMAT_VERSION` to 22 and update any test asserting the number.
- [x] **Step 4:** `cargo test` — PASS. Watch the native lints
  (`unused-namespace`, `duplicate-require`) and clean-ns tests in
  particular: a new occurrence inside the ns form must not count as a use of
  the namespace. `:refer` entries are already occurrences there, so the
  existing exclusion should cover it; if a lint test fails, fix the lint's
  exclusion, not the occurrence.
- [x] **Step 5:** `git commit -m "extractor: record :rename keys as occurrences of the var"`

### Task 4: Rename edits only tokens that spell the name (C, handler half)

**Files:** `src/handlers/references.rs`, `tests/test_e2e.rs`

- [x] **Step 1: Failing e2e tests** (`LspClient`, `setup_project()` pattern;
  add the two-file shape to the copied fixture at test time, as neighboring
  rename tests do):
  - Rename `foo` → `bar` from its definition in `a.clj`: edits in `b.clj`
    are exactly the `:refer [foo]` entry and the `:rename` key; no edit
    touches `f` in the map or the `(f)` call. Assert the full edit set for
    both files.
  - Same rename started from the `:rename` key in `b.clj`: same edit set.
  - `prepareRename` on the `(f)` call and `rename` from it both answer the
    refusal message from Design C (`-32602` as other refusals).
  - `references` on `foo` still lists the `(f)` call and now the key.
  - Regression: a record rename still rewrites `->Foo` / `map->Foo` /
    `Foo.` calls (existing tests should already cover it; confirm they run).
  - With `b.clj` **not open** (index path, text read from disk) the edit set
    is the same.
- [x] **Step 2:** `bb e2e` filtered to the new tests — FAIL.
- [x] **Step 3: Implement** the spelling filter in `rename` and the refusal
  in `rename_target` per Design C. Add a unit test beside
  `a_constructor_site_must_spell_the_constructor` for the filter.
- [x] **Step 4:** `bb e2e` — PASS.
- [x] **Step 5:** `git commit -m "rename: leave a :rename'd local name alone, edit the key"`

### Task 5: e2e for method-param locals, then the gates

**Files:** `tests/test_e2e.rs`, `docs/MEMORY.md`

- [x] **Step 1:** One e2e test: in an `extend-protocol` method body,
  `definition` on a param use lands on the argv, `references` lists both,
  and `rename` edits both and nothing else; completion in the body offers
  the param.
- [x] **Step 2:** `bb check` — green. `bb e2e` — green. `bb e2e-pulse` —
  green. `bb e2e-calva` — green.
- [x] **Step 3:** `bb compare clj-kondo`. Expected: `local/plain` 594 of 594
  agree, no new divergence in any other bucket, total diverge/null down by
  the nine. Record the new rows and totals in `docs/MEMORY.md` under a dated
  heading in the style of "Re-check of the two gap-resolved local entries".
  Anything unexpected is triaged before moving on — a fix, or a backlog
  line; never a silent `KNOWN` entry.
- [x] **Step 4:** `bb bench clj-kondo`. Compare the definition median with
  the MEMORY.md table; a visible regression means the ns-only pass needs
  tightening before continuing.
- [x] **Step 5:** `git commit -m "test: e2e for method-param locals; record compare re-run"`

### Task 6: Docs and roadmap close-out

**Files:** `docs/ROADMAP.md`, `docs/backlog/…`, `docs/archive/…`, `CLAUDE.md`
(and `AGENTS.md` if separate), `docs/FEATURES.md`

- [x] **Step 1:** `git mv docs/backlog/2026-09-30-method-params-in-type-bodies-are-not-locals.md docs/archive/`
  and set its status to closed with the date and this plan.
- [x] **Step 2:** ROADMAP: tick the Task 0 item, status `done`, link the
  archived issue and state the compare result; delete the three Backlog
  lines (2026-09-10 `:lint-as` in the locals walker, 2026-09-29 renaming a
  var referred under another name, 2026-09-30 method params). The
  2026-09-17 "defs nested in a wrapping macro" line stays.
- [x] **Step 3:** Invariants in `CLAUDE.md`:
  - "Defining macros resolve by fqn…": replace the sentence saying
    `walk_scope` has neither ns metadata nor `ExtractConfig` with the new
    rule (one resolver, three readers; ns metadata read from the live tree,
    once per request).
  - "`are` …": the locals walker now resolves the head by fqn too.
  - A new invariant for method params (the locals walker mirrors
    `walk_type_specs`; a param shadows a field).
  - "Constructor calls are sites…" / rename invariants: a `:rename` key is a
    site; a rename edits only tokens spelling the old name; a cursor on the
    renamed local name is refused by `rename_target`.
  - `CACHE_FORMAT_VERSION` 22 where 21 is quoted.
- [x] **Step 4:** `docs/FEATURES.md`: adjust the locals and rename sections
  if they describe the old limits. README only if it mentions them.
- [x] **Step 5:** `bb check` — green.
- [x] **Step 6:** `git commit -m "docs: close method-param locals, lint-as locals and refer-rename items"`

## Completion summary (2026-09-30)

Implemented as designed: the locals walker binds method params
(`walk_scope_type_specs`), classifies heads through `head_def_kind`,
`are_head_fqn` and `head_resolves_to_core` over a `ScopeCtx` whose ns
metadata is read from the live tree (`ns_meta_of_tree`), the `:rename` key
is an occurrence, and a var rename edits only tokens spelling the old name,
refusing from the renamed local name. `CACHE_FORMAT_VERSION` 22.

Gates: `bb check`, `bb e2e`, `bb e2e-pulse`, `bb e2e-calva` green;
`bb compare clj-kondo` `local/plain` 594 of 594, total 4920 agree / 216
diverge / 62 known / 51 null (4913 / 219 / 60 / 57 on master);
`bb bench clj-kondo` definition 19 / 18 ms (17 / 18 recorded).

Deviations:

> Deviation (Task 1, from its codex review): a vector-headed list after a
> parameter vector is a body expression, not another arity
> (`walk_scope_fn_tail`); it also applies to `defn`/`fn`. Landed in the
> Task 2 commit. A cursor on a method's own name sees no params.

> Deviation (Task 2): the `_tree` entry points take no `path` — the ns-only
> pass needs none (the plan allowed dropping it). No existing test relied on
> a bare `are` without clojure.test, so none changed.

> Deviation (Task 5): the clj-kondo corpus itself uses
> `:refer [assert-submaps2] :rename {assert-submaps2 assert-submaps}`, so
> the rename change moved two `var-def/defmacro` answers from agree to
> diverge, by design (kondo lists the renamed calls and not the key). The
> plan said "never a silent `KNOWN` entry": this one is dated, reasoned,
> matched only when every file with an extra site holds a
> `:rename {<name> …` entry, and recorded in `docs/MEMORY.md`.

> Deviation (process): per-task codex reviews ran in the background while
> the next task started, instead of blocking; findings were folded into the
> following commit. No session task list — the tool is not available in
> this harness; this document is the record.

What the plan could have specified better: it should have checked the
compare corpus for the `:rename` shape before promising "no new divergence
in any other bucket".
