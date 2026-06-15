# Protocol Method Implementation → Declaration Navigation Implementation Plan

> **Status: COMPLETED (2026-06-15).** See the summary at the end.

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Go-to-definition on a protocol *method implementation* — e.g. `start`
inside `(defrecord DB [...] component/Lifecycle (start [component] ...))` —
jumps to the method's *declaration* in the protocol
(`com.stuartsierra.component/start`). Covers `defrecord`/`deftype` inline specs,
`extend-type`, `extend-protocol`, and `reify`, and fixes the related bug where
method-impl heads and params are recorded as phantom current-namespace
occurrences.

**Tech Stack:** Rust, tree-sitter-clojure (the occurrence walker in
`extractor.rs`), the existing `Index`/`Occurrence` infrastructure.

---

## Design

Two halves, both required.

### Why both halves are needed

Go-to-definition does **not** consult the occurrence index — it re-resolves the
word under the cursor via `resolve_symbol`, which only sees the bare word
(`start`) plus the current namespace. For a method impl whose protocol lives in
another namespace (`com.stuartsierra.component`), `resolve_symbol("start",
"tickets.components.db")` returns `None`. So recording the right occurrence is
necessary but not sufficient; the definition handler must also use it.

### Half 1 — extractor records impl heads against the protocol's namespace

The occurrence walker learns the protocol-implementation forms. In
`(defrecord DB [fields] component/Lifecycle (start [component] ...) (stop ...))`
the `component/Lifecycle` spec names the protocol; the following method-impl
lists belong to it. For each method impl `(name [params] body…)`:

- **Bind the params** as locals (so `component` is no longer recorded as a
  usage — fixing the phantom param occurrences).
- **Record the head** (`name`) as an occurrence resolved to the protocol's
  namespace: `com.stuartsierra.component/start`. Protocol methods are vars in
  the protocol's own namespace, so the method namespace is the resolved
  namespace of the preceding protocol symbol.
- **Skip the head** when the protocol namespace is undeterminable (`Object`,
  Java interfaces, unresolved bare symbols) — never create a phantom occurrence.
- **Walk the body** with the params in scope.

Protocol-symbol namespace resolution (`protocol_ns`):
- qualified `a/B` → alias `a` resolved via `ns_meta.aliases`, else literal `a`.
- bare `B` → `ns_meta.refers["B"]`'s namespace, else (if `B` is a current-ns
  def) the current namespace, else `None`.

Form shapes handled (all share one `walk_type_specs` helper):
- `(defrecord Name [fields] & specs)` / `(deftype Name [fields] & specs)` —
  fields bound; `specs` are interleaved *protocols* and method impls.
- `(extend-type Type & specs)` — `Type` is a type occurrence; `specs` interleave
  *protocols* and method impls.
- `(extend-protocol Proto & specs)` — `Proto` is fixed for all methods; `specs`
  interleave *types* (occurrences) and method impls.
- `(reify & specs)` — `specs` interleave *protocols* and method impls.

### Half 2 — definition handler: occurrence-at-cursor fallback

When `resolve_symbol` returns `None`, the handler looks up the recorded
occurrence covering the cursor position and navigates to
`index.lookup(its_fqn)`. This runs only as a fallback, so existing navigation is
untouched. The declaration target already exists in the index: protocol method
declarations are indexed (recent protocols work), and
`com.stuartsierra/component` is on the classpath via Leiningen support.

### Key decisions

1. **Occurrence-at-cursor as a *fallback* in the definition handler**, not
   context-threading into `resolve_symbol`. Reuses the maintained occurrence
   index; runs only when `resolve_symbol` fails → zero regression surface. A
   phantom/unknown fqn simply finds no symbol and navigates nowhere.
2. **Method namespace = resolved namespace of the preceding protocol symbol**
   (qualified alias / `:refer` / current-ns def). Correct because protocol
   methods are vars in the protocol's namespace.
3. **Skip undeterminable protocol namespaces** (`Object`/interfaces) so no new
   phantom occurrences are created; params are bound regardless. This is why the
   phantom-occurrence cleanup is part of this change rather than a separate hack.
4. **Cover `defrecord`/`deftype`/`extend-type`/`extend-protocol`/`reify`** with
   one shared spec-walking helper; the only differences are the prefix and
   whether interleaved symbols are protocols or types.
5. **Definition only** (not hover/references) in this change. The same fallback
   extends to hover trivially later; references/rename already benefit from the
   phantom fix.

## File Structure

- **Modify `src/index/extractor.rs`** — add `walk_type_specs`, `walk_method_impl`,
  `protocol_ns`, and `walk_occurrences` arms for `extend-type`/`extend-protocol`/
  `reify`; route `defrecord`/`deftype` bodies through `walk_type_specs`.
- **Modify `src/index/mod.rs`** — `occurrence_at(path, pos) -> Option<String>`.
- **Modify `src/handlers/definition.rs`** — occurrence fallback in the `None` arm.
- **Modify `tests/test_extractor.rs`** — occurrence unit tests for the new forms.
- **Modify `tests/test_e2e.rs`** — cross-namespace impl→decl navigation e2e.
- **Modify `docs/ROADMAP.md`** — note impl→decl navigation.

Reuse `record_occurrence`, `collect_binding_names`, `named_children`,
`node_to_lsp_range`, `sym_name_node`. No new public types.

---

### Task 1: Extractor — walk protocol-implementation bodies

**Files:**
- Modify: `src/index/extractor.rs`
- Modify: `tests/test_extractor.rs`

- [x] **Step 1: Write failing unit tests** (in `tests/test_extractor.rs`, using
  `extract_full` + the existing `occurrences_of` helper):
  - defrecord cross-ns impl: `(ns a (:require [proto.ns :as p]))\n(defrecord R [x]\n  p/Worker\n  (run-task [this job] x))` records an occurrence `proto.ns/run-task` at the impl head, and records **no** occurrence for the param `this`/`job` and **none** for the field `x` inside the body.
  - `Object` method head skipped: `(defrecord R [x] Object (toString [this] "s"))` records no occurrence whose name is `toString`.
  - `extend-protocol` shape: `(ns a (:require [proto.ns :as p]))\n(extend-protocol p/Worker String (run-task [this job] job))` records `proto.ns/run-task` for the method and records `String` as a (type) occurrence.
  - `extend-type` shape: `(extend-type String p/Worker (run-task [this job] job))` records `proto.ns/run-task`.

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --test test_extractor`
  Expected: FAIL.

- [x] **Step 3: Implement**
  - `protocol_ns(sym, ctx) -> Option<String>` as described in the design.
  - `walk_method_impl(list, proto_ns: Option<&str>, ctx, scope, out)`: from
    `named_children`, take the head name node and the first `vec_lit` (params);
    if `proto_ns` is `Some`, push `Occurrence { fqn: "<ns>/<name>", name_range }`;
    bind params into a new scope frame via `collect_binding_names`; walk the
    remaining body children; pop the frame.
  - `walk_type_specs(specs, mode, ctx, scope, out)` where `mode` is interleaved
    protocols vs. a fixed protocol ns: a `sym_lit` either updates the current
    protocol ns (interleaved) or is recorded as a type occurrence (fixed); a
    `list_lit` is a method impl → `walk_method_impl` with the current ns.
  - In `walk_def_form`, for `Defrecord`/`Deftype`: bind the `[fields]` vector,
    then `walk_type_specs(children[3..], interleaved)`.
  - In `walk_occurrences`, add arms for `"extend-type"` (record head; type =
    `children[1]` occurrence; `walk_type_specs(children[2..], interleaved)`),
    `"extend-protocol"` (record head; `proto_ns(children[1])` fixed;
    `walk_type_specs(children[2..], fixed)`), and `"reify"` (record head;
    `walk_type_specs(children[1..], interleaved)`).

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --test test_extractor`
  Expected: PASS (including the existing tests — no regressions).

- [x] **Step 5: Commit**
  `git commit -m "Resolve protocol method impls to their declaring namespace"`

### Task 2: `Index::occurrence_at`

**Files:**
- Modify: `src/index/mod.rs`

- [x] **Step 1: Write failing unit test**
  In `src/index/mod.rs` `#[cfg(test)]`: insert a file with an `Occurrence`
  whose `name_range` covers a known span; `occurrence_at(path, pos_inside)`
  returns its fqn; a position outside returns `None`.

- [x] **Step 2: Run to verify it fails**
  Run: `cargo test --lib index::tests` (or the chosen test name)
  Expected: FAIL.

- [x] **Step 3: Implement**
  `occurrence_at(&self, path: &Path, pos: Position) -> Option<String>`: look up
  the file's occurrences and return the `fqn` of the first whose `name_range`
  contains `pos` (single-line: `pos.line == start.line && start.character <=
  pos.character <= end.character`).

- [x] **Step 4: Run to verify it passes**
  Run: `cargo test --lib`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Add Index::occurrence_at for position-based usage lookup"`

### Task 3: Definition handler — occurrence fallback

**Files:**
- Modify: `src/handlers/definition.rs`

- [x] **Step 1: Implement**
  In `handle`, in the `None` arm (before `namespace_location`): call
  `index.occurrence_at(&path, pos)`; if it yields an fqn that `index.lookup`
  resolves to a symbol, return `location_for(&sym.file, sym.name_range,
  &sym.source)`. Otherwise fall through to `namespace_location` as today.

- [x] **Step 2: Verify build + existing tests**
  Run: `bb check`
  Expected: PASS.

- [x] **Step 3: Commit**
  `git commit -m "Definition: fall back to the resolved occurrence at the cursor"`

### Task 4: End-to-end impl → declaration navigation

**Files:**
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing e2e test**
  A cross-namespace setup (exercises the fallback, since same-ns method names
  already resolve directly):
  - `src/proto.clj`: `(ns app.proto)\n(defprotocol Worker\n  (run-task [this job]))`
  - `src/impl.clj`: `(ns app.impl (:require [app.proto :as p]))\n(defrecord Runner [id]\n  p/Worker\n  (run-task [this job] job))`
  - `LspClient::start`, `initialize`, `wait_for_log("Indexed")`, `did_open` the
    impl file, `goto_definition` on the `run-task` impl head → location whose
    URI ends with `/src/proto.clj` and whose `range.start.line` is the
    `(run-task [this job])` declaration line.

- [x] **Step 2: Run + full suite**
  Run: `cargo test --test test_e2e protocol_impl` then `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 3: Commit**
  `git commit -m "e2e: navigate from a protocol method impl to its declaration"`

### Task 5: Roadmap note

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Extend the protocols line**
  Add to the (already checked) Phase 5 protocols item a note that method
  implementations in `defrecord`/`deftype`/`extend-type`/`extend-protocol`/
  `reify` now navigate to the protocol's declaration.

- [x] **Step 2: Commit**
  `git commit -m "Roadmap: note protocol impl→declaration navigation"`

---

## Notes & limitations

- **The protocol's declaring namespace must be indexed** for navigation to land
  (true for `com.stuartsierra/component` here; a project protocol is always
  indexed). Otherwise `lookup` finds nothing and definition is a no-op.
- **`Object`/interface method impls** intentionally do not navigate (no protocol
  declaration to target); their params are still bound.
- **Definition only** — hover/references are unchanged beyond the incidental
  phantom-occurrence cleanup.

---

## Implementation summary (2026-06-15)

Implemented on branch `protocol-impl-navigation` (stacked on
`protocols-records-navigation`, which it depends on for protocol-method
indexing). All `bb check` and `bb e2e` (42 tests) pass.

- **`src/index/extractor.rs`** — `walk_type_specs`/`walk_method_impl`/
  `protocol_ns` and `walk_list` arms for `extend-type`/`extend-protocol`/
  `reify`; `defrecord`/`deftype` bodies route through `walk_type_specs`. Impl
  heads resolve to the protocol's namespace; params are bound; `Object`/
  interface heads are skipped. Fixes the phantom head/param occurrences.
- **`src/handlers/definition.rs`** — position-based resolution via the existing
  live resolver `references::resolve_fqn_at` (see follow-ups).
- **Tests** — extractor occurrence unit tests for all five forms, an
  `occurrence_at` unit test, and e2e tests (cross-ns impl→decl, plus a
  colliding-name regression).

### Codex review follow-ups (all fixed)

Iterative codex reviews drove the resolution design to its final form:

- **Position-based resolution preferred over bare-word resolution.** The first
  cut made the occurrence a *fallback* only in the `None` arm, so a protocol
  method impl whose name also resolves as a core/current-ns var navigated to
  that var. Now the handler resolves the cursor position first; only known
  symbols short-circuit, else it falls through to `resolve_symbol` (aliases,
  namespaces, static core list). Regression test:
  `test_e2e_protocol_impl_wins_over_colliding_def`.
- **Live-buffer resolution.** Rather than a new `Index::occurrence_at` over the
  stale index, the handler reuses `references::resolve_fqn_at`, which
  re-extracts the open document — so navigation is correct under unsaved edits
  and consistent with references/rename. `Index::occurrence_at` was removed as
  redundant.
- **Multi-arity method impls.** `(m ([x] …) ([x y] …))` previously bound no
  params (it expected a single `[params]` vector), leaking `x`/`y` and their
  body uses as global occurrences. `walk_method_impl` now binds per arity like
  `defn`. Regression test:
  `test_occurrence_multi_arity_method_impl_binds_params`.
