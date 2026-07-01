# Local (`let`/`fn`) binding go-to-definition & completion Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make go-to-definition and completion work for locally-bound names —
a symbol bound in a `let` (or `fn`/`loop`/`for`/destructuring/…) that is used
later in the same form navigates to its binding site and is offered in
completion.

**Tech Stack:** Rust, tower-lsp, tree-sitter (`tree-sitter-clojure`).

---

## Design

### The problem

The extractor already tracks lexical scope (`scope: Vec<HashSet<String>>` in
`src/index/extractor.rs`) for every binding form — `let`, `loop`, `for`,
`doseq`, `when-let`/`if-let`/`when-some`/`if-some`, `with-open`, `dotimes`,
`fn`, `defn`/`defn-`/`defmacro`/`defmethod` params, `letfn`, `defrecord`/
`deftype` fields, and destructuring. But it uses that tracking **only to
suppress** locals from being recorded as var occurrences (`record_occurrence`
returns early when a name is in scope). It never records *where* a local is
bound.

Consequences today:

- **Go-to-def on a local usage** (e.g. `add` used in a later `let` binding or
  the body) finds no occurrence and no symbol, falls through to the bare-word
  resolver (`resolve_symbol`), and either lands on a same-named global var
  (wrong) or nothing.
- **Completion** never offers in-scope locals.

### The approach

Introduce one position-scoped primitive that both features consume:

```rust
pub struct LocalBinding {
    pub name: String,
    pub name_range: Range, // the binding-site symbol's range
}

/// Local bindings visible at `pos`, in outermost→innermost order (so the last
/// match shadows). A cursor sitting on a binding site itself yields that
/// binding (harmless self-jump, matches clojure-lsp).
pub fn locals_in_scope_at(source: &str, pos: Position) -> Vec<LocalBinding>
```

Implementation is a **position-directed spine walk**: parse the buffer, then
descend from the root only into the child subtree whose LSP range contains
`pos`, and at each binding form on that path collect the bindings lexically
visible at `pos`. It mirrors the exact scoping rules the occurrence walker
already encodes:

- **Sequential binding vectors** (`let`/`loop`/`for`/`doseq`/`when-let`/… — the
  `is_let_like` set plus `loop`): a `[lhs rhs]` pair's `lhs` is visible to
  *later* pairs' RHS and to the body, but **not to its own RHS**. This is
  exactly the reported bug. Comprehension modifiers: `:let [..]` recurses as a
  nested binding vector; `:when`/`:while` are plain body expressions.
- **`fn`/`defn`/`defn-`/`defmacro`/`defmethod`**: per-arity params (and the
  optional `fn` self-name) visible in that arity's body.
- **`letfn`**: the fn names are mutually-recursive locals visible in every fn
  body and the letfn body.
- **`defrecord`/`deftype`**: the field vector.
- **Destructuring** (shared with binding vectors and params): `:keys`/`:strs`/
  `:syms`, `:as`, `:or` defaults, vector positions, `& rest`; `&` and `_` are
  excluded.

`if-let`/`when-let` bindings are treated as visible in the whole body (not
then-branch-only), matching the existing occurrence walker — consistency with
current behavior over perfect semantics.

To obtain binding-site **ranges** (which the occurrence walker's
`collect_binding_names` discards — it collects only names into a `HashSet`),
add an internal sibling `collect_binding_targets(pattern) -> Vec<LocalBinding>`
that walks a binding pattern and yields each bound symbol with its `name_range`.

**Key decision — mirror, don't refactor.** `collect_binding_names` is left
untouched. It carries occurrence side-effects (`:or` defaults and map-key
keywords recorded via `ctx`/`scope`/`out`) and feeds references/rename, which
has a large test surface. The locals walk has a different traversal shape
(spine, not full-tree) and different output (name + range, no side-effects), so
the two binding-rule sets are mirrored with cross-reference comments rather than
unified through a shared abstraction threaded into the hot occurrence path. The
duplication is the ~40 lines of destructuring rules.

The primitive lives in `src/index/extractor.rs` because it needs the module's
private helpers (`named_children`, `is_let_like`, `arity_body`, `sym_text`,
`sym_name_node`, `node_to_lsp_range`). Node-vs-`pos` containment reuses an LSP
`range_contains(&Range, Position)` check (same rule as
`handlers/references.rs`) computed from `node_to_lsp_range`, so UTF-16 columns
match the rest of the code — no byte/UTF-16 mismatch.

### Integrations

**Go-to-definition (`src/handlers/definition.rs`).** Locals shadow all vars, so
check them **first**, before `resolve_fqn_at`/`resolve_symbol`. A new
`local_definition(documents, uri, pos) -> Option<Location>`:

- returns `None` if the cursor is on a keyword (`is_keyword_at`) or the word is
  qualified (contains `/`) — locals are never either;
- computes `locals_in_scope_at` over the live buffer text and picks the
  **innermost** binding whose name equals the word under the cursor
  (`.iter().rev().find(...)`);
- builds a `Location` with the request's own `uri` (same document) and the
  binding's `name_range`.

`None` → the existing resolution pipeline runs unchanged, so non-locals have no
regression. Because the occurrence walker already suppresses locals,
`resolve_fqn_at` returns `None` for a local usage anyway; the early check simply
preempts the accidental fall-through to a same-named global.

**Completion (`src/handlers/completion.rs`).** `complete_symbols` has no
document/position access, so locals are added in `handle` (where `documents`,
`uri`, `pos` exist), for **non-qualified prefixes only**. A new
`local_completions(documents, uri, pos, prefix) -> Vec<CompletionItem>`:

- computes `locals_in_scope_at`, iterates innermost-first, filters by
  `name.starts_with(prefix)`, de-dups by name (innermost shadows);
- each item: `label = name`, `kind = VARIABLE`, `detail = "local"`,
  `sort_text = Some("0-<name>")` so locals sort above vars/core (clients sort
  the rest by label; `"0-"` precedes typical identifiers).

Locals are prepended to `complete_symbols`' result; the empty-response check
moves after the merge so a buffer with only locals still completes.

### Non-goals

- Hover on locals (show the binding form).
- References / rename for locals (keeps the occurrence model unchanged; locals
  stay unrecorded as occurrences).
- Cross-file — N/A, locals are always same-file.
- `#(... %)` anonymous-fn `%`/`%1` params (skipped, as elsewhere).

### Testing strategy

- **Unit** (`src/index/extractor.rs` `#[cfg(test)]`): drive `locals_in_scope_at`
  with source + position, asserting visible names and ranges across the scope
  rules above (the load-bearing case: an early `let` binding is visible in a
  later RHS and the body, but not its own RHS; innermost shadowing; params;
  destructuring; `for` `:let`; `letfn`).
- **E2E** (`tests/test_e2e.rs` + a self-contained fixture): assert goto-def on a
  local usage lands on the binding site, and completion offers the local.
- **Verification**: `bb check` + `bb e2e` (server-behavior change); `bb e2e-nvim`
  since definition and completion are client-visible.

## File Structure

- **Modify** `src/index/extractor.rs` — add `pub struct LocalBinding`,
  `pub fn locals_in_scope_at`, the internal spine walk, and
  `collect_binding_targets`; add unit tests to the existing `tests` module.
- **Modify** `src/handlers/definition.rs` — add `local_definition` and call it
  first in `handle`.
- **Modify** `src/handlers/completion.rs` — add `local_completions` and merge it
  in `handle`.
- **Create** `tests/fixtures/simple_project/src/locals.clj` — a self-contained
  namespace (`simple.locals`) using only local bindings and `clojure.core`, so
  it adds **no** cross-file occurrences (the references test asserts exactly
  two references to `simple.core/add`; this fixture must not touch counted
  symbols).
- **Modify** `tests/test_e2e.rs` — add goto-def and completion tests over the
  new fixture.
- **Modify** `ARCHITECTURE.md` — note locals-first resolution under "Symbol
  Resolution".

---

## Task 1: `locals_in_scope_at` primitive

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `src/index/extractor.rs` (`#[cfg(test)] mod tests`)

- [x] **Step 1: Write failing unit tests**
  Add a test helper `locals_at(source, line, col) -> Vec<LocalBinding>` calling
  `locals_in_scope_at(source, Position::new(line, col))`. Cover:
  - **let sequential (the bug):** for
    `(ns x)\n(defn f []\n  (let [a 1\n        b (+ a 1)]\n    (+ a b)))`,
    at the body `(+ a b)` both `a` and `b` are visible; at `b`'s RHS `(+ a 1)`
    only `a` is visible (not `b`); the `name_range` reported for `a` points at
    the `a` in the binding vector.
  - **innermost shadowing:** a nested `let` re-binding `a` — the innermost `a`
    is the last element and its range is the inner binding site.
  - **fn/defn params:** a param is visible in the body.
  - **destructuring:** `[{:keys [x] :as m} [y & ys]]` binds `x`, `m`, `y`, `ys`
    (not `&`).
  - **`for` `:let`:** `(for [i xs :let [j (inc i)]] (+ i j))` exposes `i` and
    `j` in the body.
  - **letfn:** both fn names are visible in the letfn body.

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib locals_`
  Expected: FAIL (unresolved `locals_in_scope_at` / `LocalBinding`).

- [x] **Step 3: Implement the primitive**
  Add `pub struct LocalBinding { pub name: String, pub name_range: Range }` and
  `pub fn locals_in_scope_at`. Implement the position-directed spine walk and
  `collect_binding_targets` per the Design section, reusing the module's private
  helpers and an LSP `range_contains(&Range, Position)` containment check. Mirror
  the binding rules from `walk_let_form`/`walk_fn_form`/`walk_letfn_form`/
  `collect_binding_names`, adding a comment on `collect_binding_targets` and
  `collect_binding_names` that cross-references the other (keep-in-sync note).

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib locals_`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -am "feat: resolve local bindings in scope at a position"`

## Task 2: Go-to-definition for locals

**Files:**
- Modify: `src/handlers/definition.rs`
- Create: `tests/fixtures/simple_project/src/locals.clj`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Add the fixture**
  Create `tests/fixtures/simple_project/src/locals.clj` with namespace
  `simple.locals`, self-contained (no cross-file refs). Include a `let` where an
  early binding is used in a later binding and in the body, e.g.:
  ```clojure
  (ns simple.locals)

  (defn compute [n]
    (let [base   (inc n)
          scaled (* base 2)]
      (+ base scaled)))
  ```

- [ ] **Step 2: Write the failing e2e test**
  Add `test_e2e_goto_definition_local_in_let`: `setup_project`, `initialize`,
  `did_open` `src/locals.clj`. Goto-def on the `base` usage inside the later
  binding `(* base 2)` (use a position on that `base`, distinct from the binding
  site). Assert the response URI ends with `/src/locals.clj` and the returned
  range start line/character equals the binding-site `base` (line of
  `[base   (inc n)`). Add a second assertion: goto-def on `scaled` in the body
  `(+ base scaled)` lands on the `scaled` binding site.

- [ ] **Step 3: Run to verify it fails**
  Run: `cargo test --test test_e2e goto_definition_local`
  Expected: FAIL (no location, or wrong range).

- [ ] **Step 4: Implement `local_definition` and wire it in**
  Add `local_definition(documents, &uri, pos) -> Option<Location>` per the
  Design section (keyword/qualified guards, innermost match, same-`uri`
  Location). Call it at the top of `handle`, right after `current_ns` is
  computed and before `resolve_fqn_at`; return its `Scalar` location when `Some`.

- [ ] **Step 5: Run to verify it passes**
  Run: `cargo test --test test_e2e goto_definition_local`
  Expected: PASS.

- [ ] **Step 6: Commit**
  `git commit -am "feat: go-to-definition for let/fn-bound locals"`

## Task 3: Completion for locals

**Files:**
- Modify: `src/handlers/completion.rs`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing e2e test**
  Add `test_e2e_completion_local_in_let`, following
  `test_e2e_completion_bare_prefix_in_current_ns`. Open `src/locals.clj`,
  `did_change_insert` a partial reference to a local inside the `let` body (e.g.
  replace/extend the body to type `ba` where `base`/`scaled` are in scope), then
  request completion at that position. Assert the labels contain `base`, and
  that the `base` item has `detail == "local"`.

- [ ] **Step 2: Run to verify it fails**
  Run: `cargo test --test test_e2e completion_local`
  Expected: FAIL (`base` not offered).

- [ ] **Step 3: Implement `local_completions` and merge it**
  Add `local_completions(documents, &uri, pos, &prefix) -> Vec<CompletionItem>`
  per the Design section. In `handle`, when `prefix` has no `/`, prepend locals
  to `complete_symbols`' result; move the empty-check after the merge.

- [ ] **Step 4: Run to verify it passes**
  Run: `cargo test --test test_e2e completion_local`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -am "feat: offer in-scope locals in completion"`

## Task 4: Full verification & docs

**Files:**
- Modify: `ARCHITECTURE.md`

- [ ] **Step 1: Note locals-first resolution**
  Under "Symbol Resolution (for definition + hover)" in `ARCHITECTURE.md`, add a
  short line that a cursor on a locally-bound name resolves to its binding site
  in the same file (via `extractor::locals_in_scope_at`), before var/alias/core
  resolution.

- [ ] **Step 2: Full check**
  Run: `bb check`
  Expected: fmt clean, clippy `-D warnings` clean, all unit + integration tests
  pass (confirms no reference-count/other regression from the new fixture).

- [ ] **Step 3: End-to-end**
  Run: `bb e2e`
  Expected: PASS, including the two new tests.

- [ ] **Step 4: Real-editor client**
  Run: `bb e2e-nvim`
  Expected: PASS (definition + completion are client-visible).

- [ ] **Step 5: Commit**
  `git commit -am "docs: note locals-first symbol resolution"`
