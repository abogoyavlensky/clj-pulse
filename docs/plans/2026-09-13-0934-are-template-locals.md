# `are` Template Locals Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `(are [x y] expr & values)` from `clojure.test` / `cljs.test` bind `x` and `y` as locals in `expr`, so definition, hover, completion, references, rename, documentHighlight and the native `unused-binding` lint treat them as locals instead of vars of the current namespace.

**Tech Stack:** Rust (tower-lsp, tree-sitter-clojure), extractor unit tests (`tests/test_extractor.rs`), diagnostics unit tests (`src/diagnostics.rs`), e2e harness (`tests/test_e2e.rs`).

**Environment note:** `mise install` in the repo root provides every tool (rust, babashka, java, clojure). Gates: `bb check`, `bb e2e`, `bb e2e-pulse`, `bb e2e-calva`, plus a `bb bench clj-kondo` comparison at the end. Baseline on 2026-09-13: `bb check` and `bb e2e` green on `master` at `751f3ba`.

---

## Design

### Problem

`(are [x y] (= x y) 1 1 2 2)` is the second most common assertion form in
`clojure.test` after `is`. The extractor treats it as an ordinary call, so
the template's argument symbols `x` and `y` fall through `record_occurrence`
to the current namespace: `my.core-test/x`. Consequences, all wrong answers:

- Definition and hover on `x` inside the template find nothing.
- `x` is missing from local completion inside the template.
- References and rename of a project var named `x` pick up the test noise.
- documentHighlight on `x` highlights nothing.
- The native `unused-binding` lint cannot see an unused template argument.

`are` expands into a `do` of `is` forms with the argv substituted, so its
scope rule is distinctive: the argv binds locals visible in the *template
expression only*. The trailing values are ordinary expressions evaluated in
the enclosing scope.

### Approach

Teach both walkers the form. Nothing else changes: handlers already read
locals through `locals_in_scope_at_tree` and `local_references_at_tree`, and
the lint pass already reports unused slots from `Scope::pop`.

**Occurrence walker (`walk_list`).** Resolve the head by fqn, the way
`deftest` is resolved. `macro_def_kind` already builds the candidate list
(alias-qualified, `:refer`red, every `:refer :all` / `:use` namespace, or the
literal `clojure.test/are`); extract that list into a shared helper and check
it against a two-entry table, `clojure.test/are` and `cljs.test/are`. On a
hit, record the head as an occurrence of the matched fqn (like `deftest`
heads, not via `record_occurrence`, which would resolve a refer-all bare head
to the current namespace) and walk the form as:

1. `children[1]` must be a `vec_lit`; otherwise fall back to the generic walk.
2. Collect its binding names, push a frame, bind them **lintable**.
3. Walk `children[2]` (the template) inside that frame.
4. Pop the frame, then walk `children[3..]` (the values) in the enclosing scope.

A bare `are` in a file that never pulls in clojure.test stays a plain call.

**Locals walker (`walk_scope`).** It has no ns metadata, so it matches `are`
by the head's name part alone, bare or qualified (`are`, `t/are`,
`clojure.test/are`), when the second child is a `vec_lit`. Same rule the
walker already applies to `mu/defn`. Scope extent:

- Cursor inside the argv: bind the argv and return (self-resolve, like fn
  params).
- Cursor inside the template: bind the argv, then descend into the template.
- Cursor inside a value: descend into it with nothing bound from the argv.

### Key decisions

- **Fqn in the occurrence walker, name part in the locals walker.** Matches
  the existing `deftest` / `mu/defn` precedents. The two disagree only for a
  bare `are` in a file without clojure.test, where the locals walker binds and
  the occurrence walker does not. That is the gap the 2026-09-10 backlog item
  "`:lint-as` in the locals walker" already tracks (threading ns metadata
  through `locals_in_scope_at`); this plan adds `are` to that line rather than
  doing the refactor.
- **Argv bindings are lintable.** An unused template argument means a whole
  column of values is ignored. `let` bindings and `defn` params are lintable;
  `are` argv joins them.
- **Strict scope extent.** Values are outside the template scope, so a value
  spelled with an argv name is a var usage. `local_references_at` from a
  template usage lists template usages only.
- **No new `DefKind`.** `are` defines nothing. It is a table, a head test,
  and two small walkers beside `walk_binding_tail`.
- **`CACHE_FORMAT_VERSION` 16 → 17.** Extractor output changes for any
  library file containing `are`.

### Testing

- Extractor unit tests: every require style binds; no clojure.test means no
  binding; values are outside the scope; head recorded once under the matched
  fqn; `locals_in_scope_at` at the three cursor positions;
  `local_references_at` returns the argv declaration and template usages only.
- Diagnostics unit test: an unused argv symbol is one `unused-binding`.
- e2e: definition on an argv local inside a template, in a new `are` block in
  the fixture's `locals.clj`, lands on the argv.
- Gates, per the CLAUDE.md table: `bb check` and `bb e2e` (server behavior),
  `bb e2e-pulse` (client-visible), `bb e2e-calva` (definition changes), and
  `bb bench clj-kondo` once at the end (extractor change; compare, not a gate).

## File Structure

- Modify: `src/index/extractor.rs` — `head_fqn_candidates` (extracted from
  `macro_def_kind`), `are_head_fqn`, `walk_are_form`, `walk_scope_are`, and
  the two dispatch sites (`walk_list`, `walk_scope`).
- Modify: `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` bump.
- Modify: `tests/test_extractor.rs` — occurrence and scope tests.
- Modify: `src/diagnostics.rs` — one unused-binding test in the `tests` module.
- Modify: `tests/fixtures/simple_project/src/locals.clj` — an `are` block.
- Modify: `tests/test_e2e.rs` — one definition test.
- Modify: `docs/ROADMAP.md` — Milestone 1 item with its Plan line; backlog note.
- Modify: `CLAUDE.md` — one invariant paragraph.
- Modify: `README.md` — the Rename bullet enumerates local binding forms;
  add `are` template arguments there.

---

### Task 0: ROADMAP item

**Files:**
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Add the item**
  Under "Milestone 1 — correctness of shipped features", after the
  "Keep clj-kondo off the keystroke path" item, add:
  `- [ ] **\`are\` template arguments are locals.** \`(are [x y] expr & values)\`
  currently records \`x\` and \`y\` as vars of the current namespace, so
  definition, hover, completion, references, rename, documentHighlight and
  the native \`unused-binding\` lint all answer wrong inside the template.
  Bind the argv in the template expression only, resolved by fqn
  (\`clojure.test/are\`, \`cljs.test/are\`) in the occurrence walker and by
  name part in the locals walker.`
  followed by
  `Plan: [2026-09-13-0934-are-template-locals.md](plans/2026-09-13-0934-are-template-locals.md)`.

- [ ] **Step 2: Commit**
  `git commit -am "Add ROADMAP item for are template locals"`

### Task 1: Occurrence walker binds `are` argv in the template

**Files:**
- Modify: `src/index/extractor.rs`
- Modify: `src/index/jar_cache.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write the failing tests**
  Next to `test_catch_and_as_arrow_bind_locals`, add, using `extract_full`
  and `occurrences_of`:

  `test_are_binds_template_locals_in_every_require_style` — a table of
  (ns form, `are` head, expected head fqn) cases, each a separate `extract_full`
  call over `(ns x <requires>)\n(<head> [a b] (= a b) 1 1)`:
  `[clojure.test :refer [are]]` / `are`; `[clojure.test :refer :all]` / `are`;
  `(:use clojure.test)` / `are`; `[clojure.test :as t]` / `t/are`;
  no require / `clojure.test/are`; and `[cljs.test :as t]` / `t/are` in a
  `.cljs` path expecting `cljs.test/are`. For each: no `x/a` or `x/b`
  occurrence, exactly one head occurrence under the expected fqn, and
  `clojure.core/=` once.

  `test_are_values_are_outside_the_template_scope` — source
  `(ns x (:require [clojure.test :refer [are]]))\n(are [a] (pos? a) a (g a))`.
  Assert: `x/a` has exactly two occurrences (the bare value and the one
  inside `(g a)`), both on line 1 and both after the template's closing
  paren; `x/g` once.

  `test_are_without_clojure_test_is_a_plain_call` — `(ns x)\n(are [a] (pos? a) 1)`.
  Assert `x/a` has two occurrences (argv and template) and `x/are` one.

  `test_are_with_non_vector_second_child_is_generic` —
  `(ns x (:require [clojure.test :refer [are]]))\n(are foo bar)`. Assert
  `x/foo` and `x/bar` one each and `clojure.test/are` exactly once (the head
  must not be recorded twice). Also assert `extract_full` returns `Ok` and no
  `x/…` occurrences for the incomplete shapes an editor sends mid-typing:
  `(are)`, `(are [x])`, `(are [x] )`.

- [ ] **Step 2: Run the tests to verify they fail**
  Run: `cargo test --test test_extractor are_`
  Expected: the first two FAIL (`x/a` recorded), the last two PASS already.

- [ ] **Step 3: Extract the candidate list**
  In `src/index/extractor.rs`, pull the candidate-building half of
  `macro_def_kind` into
  `fn head_fqn_candidates(head: Node, ns_meta: &NsMeta, source: &str) -> Vec<String>`
  and make `macro_def_kind` call it. Behavior unchanged.

- [ ] **Step 4: Add the table and head test**
  Add `const ARE_FQNS: &[&str] = &["clojure.test/are", "cljs.test/are"];`
  and
  `fn are_head_fqn(head: Node, ns_meta: &NsMeta, source: &str) -> Option<String>`
  returning the first candidate found in `ARE_FQNS`. Doc comment: why fqn
  (mirrors `deftest`), and that a bare `are` without clojure.test is a call.

- [ ] **Step 5: Add `walk_are_form` and dispatch**
  `fn walk_are_form(children: &[Node], ctx: &OccurrenceCtx, scope: &mut Scope, out: &mut Vec<Occurrence>)`
  next to `walk_binding_tail`, implementing the four steps in the design.
  Bind with `scope.bind_all(bound, true)`. Collect binding names with
  `collect_binding_names` so destructuring in an argv (rare, legal) works.
  The dispatch site guarantees `children[1]` is a `vec_lit`; `children[2]`
  and `children[3..]` may be absent.

  In `walk_list`, right after the `head_def_kind` block and before the
  `head_is_core_form` computation, add: if `head.kind() == "sym_lit"`,
  `children[1]` is a `vec_lit`, and `are_head_fqn` hits, push an
  `Occurrence` for the head with the matched fqn and `sym_name_node(*head)`'s
  range (the same shape the `deftest` branch uses), call `walk_are_form`, and
  return. Check the vector *before* recording the head: a non-vector second
  child falls through to the generic walk, which records the head itself, so
  recording it here too would double-count it. With that check in the
  dispatch, `walk_are_form` can assume the vector; a missing template
  (`(are [x])`) binds and pops with nothing walked.

- [ ] **Step 6: Bump the cache format**
  `src/index/jar_cache.rs`: `CACHE_FORMAT_VERSION` 16 → 17.

- [ ] **Step 7: Run the tests to verify they pass**
  Run: `cargo test --test test_extractor`
  Expected: PASS, including the existing `deftest` tests.

- [ ] **Step 8: Commit**
  `git commit -am "Bind are template arguments as locals in the occurrence walker"`

### Task 2: Locals walker resolves `are` argv

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write the failing tests**
  Use `locals_in_scope_at` and `local_references_at` (already imported in the
  tree-variant test module; add a top-level import if needed). Source:

  ```clojure
  (ns x (:require [clojure.test :refer [deftest are]]))
  (deftest t
    (are [a b] (= a b)
      1 1
      a 2))
  ```

  `test_are_locals_in_scope` — at a position inside `(= a b)` on `a`, the
  names returned include `a` and `b`; at a position on the `a` in the values
  (line 4), they include neither; at a position on `a` inside the argv, they
  include `a` (self-resolve).

  `test_are_local_references_stop_at_the_template` — `local_references_at`
  from the template `a` returns `Some` with `declaration` at the argv `a`
  and exactly one usage (the template one); the value-line `a` is absent.
  From the value-line `a`, it returns `None`.

  `test_are_qualified_head_binds_in_scope_walker` — same body with `t/are`
  and `(:require [clojure.test :as t])`; template position includes `a`.

  Also extend `tree_variant_locals_match`'s pattern: assert
  `locals_in_scope_at` equals `locals_in_scope_at_tree` for the template
  position of the `are` source.

- [ ] **Step 2: Run the tests to verify they fail**
  Run: `cargo test --test test_extractor are_`
  Expected: the three new scope tests FAIL (`a` not in scope / `None`).

- [ ] **Step 3: Add `walk_scope_are` and dispatch**
  `fn walk_scope_are(children: &[Node], source: &str, pos: Position, out: &mut Vec<LocalBinding>)`
  next to `walk_scope_binding_tail`, implementing the three cursor cases
  with `collect_binding_targets`, `lsp_range_contains` and `walk_scope`.

  In `walk_scope`, before the `core_form` computation, add: if the head is a
  `sym_lit` whose `sym_name_node` text is `are` and `children[1]` is a
  `vec_lit`, call `walk_scope_are` and return. Comment: name-part rule
  because the walker has no ns metadata, matching `qualified_head_def_kind`;
  see the ROADMAP backlog line.

- [ ] **Step 4: Run the tests to verify they pass**
  Run: `cargo test --test test_extractor`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -am "Resolve are template arguments in the locals walker"`

### Task 3: Unused argv is an `unused-binding`

**Files:**
- Modify: `src/diagnostics.rs` (tests module only, unless the walker needs a fix)

- [ ] **Step 1: Write the failing test**
  Next to `flags_unused_let_binding`, add `flags_unused_are_argument`:
  source `(ns a (:require [clojure.test :refer [are]]))\n(are [x y] (= x 1) 1 2)\n`,
  `of_code(…, "unused-binding")` has exactly one diagnostic, message
  contains `y`, range on line 1 with width 1. Add
  `no_flag_for_used_are_arguments` with `(= x y)`.

- [ ] **Step 2: Run the tests**
  Run: `cargo test --lib are_argument`
  (one substring filter; cargo accepts a single positional test name)
  Expected: PASS already if Task 1 bound the argv lintable; if it FAILS,
  the bind is not lintable or the frame is popped before the template is
  walked. Fix in `walk_are_form`.

- [ ] **Step 3: Commit**
  `git commit -am "Test that an unused are argument is reported"`

### Task 4: e2e definition on an `are` local

**Files:**
- Modify: `tests/fixtures/simple_project/src/locals.clj`
- Modify: `tests/test_e2e.rs`

- [ ] **Step 1: Extend the fixture**
  Change the ns form to `(ns simple.locals (:require [clojure.test :refer [deftest are]]))`
  and append:

  ```clojure
  (deftest compute-table
    (are [input expected] (= expected (compute input))
      1 4
      2 6))
  ```

  Then run `cargo test --test test_e2e locals_` and `cargo test --test
  test_e2e completion_local` to confirm the existing `locals.clj` tests
  still pass (they anchor on `base`/`scaled`, which are unchanged).

- [ ] **Step 2: Write the failing e2e test**
  After `test_e2e_goto_definition_local_in_let`, add
  `test_e2e_goto_definition_are_template_local`: open `src/locals.clj`,
  `position_of(&locals, "expected (compute")` for the cursor on the template
  `expected`, `start_of(&text, "expected]")` for the argv site. Assert the
  definition is non-null, same file, and the range start equals the argv
  site. Add a second probe: definition on `input` inside `(compute input)`
  lands on the argv `input`.

- [ ] **Step 3: Run it**
  Run: `cargo test --test test_e2e are_template`
  Expected: PASS. Tasks 1 and 2 are already in, so this test documents the
  behavior end to end rather than driving it; the regression it guards is the
  one the extractor unit tests demonstrated red-first.

- [ ] **Step 4: Run the e2e gate**
  Run: `bb e2e`
  Expected: all PASS.

- [ ] **Step 5: Commit**
  `git commit -am "Add e2e definition test for are template locals"`

### Task 5: Docs, gates, tick

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/ROADMAP.md`
- Modify: `README.md` (verify no change needed)

- [ ] **Step 1: CLAUDE.md invariant**
  After the "Defining macros resolve by fqn" paragraph in "Invariants", add
  one paragraph: `are` (`clojure.test/are`, `cljs.test/are`, table
  `ARE_FQNS`) binds its argv in the template expression only; values are
  enclosing-scope usages; the occurrence walker resolves the head by fqn
  through `head_fqn_candidates`, the locals walker by name part alone; argv
  bindings are lintable.

- [ ] **Step 2: ROADMAP**
  Tick the Milestone 1 item and append ` — done` to its Plan line. In the
  Backlog, extend the 2026-09-10 "`:lint-as` in the locals walker" line with
  one sentence: `are` is matched by name part there too, so a bare `are` in a
  file without clojure.test binds in the locals walker but not in the
  occurrence walker.

- [ ] **Step 3: README**
  In the **Rename** bullet, the parenthetical `(params, \`let\`/\`loop\`/\`for\`
  bindings, destructured names)` becomes `(params, \`let\`/\`loop\`/\`for\`
  bindings, \`clojure.test/are\` template arguments, destructured names)`.
  Nothing else in the feature list enumerates binding forms.

- [ ] **Step 4: Full gates**
  Run: `bb check` — fmt clean, clippy clean, all tests PASS.
  Run: `bb e2e-pulse` — PASS (client-visible change).
  Run: `bb e2e-calva` — PASS (definition behavior changed).
  Run: `bb bench clj-kondo` — compare against the tables in `docs/MEMORY.md`;
  the extractor grew one head check per list, so no row should move outside
  its usual noise. Not a gate; note any surprise in MEMORY.md.

- [ ] **Step 5: Commit**
  `git commit -am "Document are template locals and tick the ROADMAP item"`
