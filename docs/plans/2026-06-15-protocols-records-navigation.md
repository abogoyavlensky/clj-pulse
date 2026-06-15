# Clojure Protocols & Records Navigation Implementation Plan

> **Status: COMPLETED (2026-06-15).** See the summary at the end.

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make go-to-definition work for Clojure protocol methods and record factory functions — `(fetch x)` jumps to the `fetch` signature in its `defprotocol`, and `(map->DB …)` / `(->DB …)` jump to the `DB` `defrecord` — by extending the existing symbol-extraction and resolution pipeline.

**Tech Stack:** Rust, tree-sitter-clojure (extractor), the existing `Index`/`Symbol`/`resolve_symbol` pipeline.

---

## Design

Both features reduce to work on the **existing** symbol/resolution pipeline — no
new subsystems.

### Protocol method navigation

Today `(defprotocol Storage (fetch [this id]) (store [this x]))` emits a single
`Defprotocol` symbol for `Storage`; the method names are not indexed, so
go-to-definition on a `(fetch …)` usage fails. In Clojure, protocol methods *are*
namespace-level vars (`my.ns/fetch`), so the fix is to **also extract each method
signature as a real symbol**. Once `ns/fetch` is in the index, the existing
`resolve_symbol` already handles bare (`fetch`), alias-qualified (`s/fetch`), and
`:refer`ed usages — so definition, hover, completion, and references all work for
free.

### Record factory navigation

`defrecord DB` auto-generates `->DB` and `map->DB`; `deftype DB` generates
`->DB`. These factory fns don't exist textually, so there is nothing to extract.
Instead, add a **fallback in `resolve_symbol`**: when a name doesn't resolve and
matches `->X` or `map->X`, strip the prefix and resolve `X` — but only when `X`
is a `Defrecord`/`Deftype`. Navigation lands on the record's name range; hover
describes it too (same seam). Gating on the record/type kind means a real fn
named `->foo` is never hijacked (and it is found by the normal lookup first
anyway).

### Key decisions

1. **Protocol methods become real `Defn` symbols**, not a new `DefKind`. They
   behave exactly like fn vars, and reusing `Defn` avoids touching the
   kind→`SymbolKind` map (`handlers/symbols.rs`), hover/completion formatting,
   and serialization.
2. **Factory navigation via a `resolve_symbol` fallback, not synthetic index
   entries.** Synthetic `->DB`/`map->DB` symbols would leak into
   `workspace/symbol`, references, and rename (renaming `map->DB` can't sensibly
   edit the record). The fallback is navigation+hover only and is gated on the
   target being a record/type.
3. **Bump `jar_cache::CACHE_FORMAT_VERSION` (5 → 6).** Extractor output changes
   (protocol methods are now extracted from library JARs too), so stale per-JAR
   caches must invalidate — JAR mtimes never change otherwise.
4. **Scope boundary:** index protocol method *declarations* (the navigation
   target). Method *implementations* inside `defrecord`/`deftype`/`reify`/
   `extend-type`/`extend-protocol` are **not** separately indexed — a `(fetch …)`
   call navigates to the protocol declaration, which is the ask. `defrecord`
   gets `->X` and `map->X`; `deftype` gets `->X` only.

### How protocol methods are shaped in the tree

`(defprotocol Storage "optional doc" :extend-via-metadata true
  (fetch [this id] "fetch doc")
  (store [this x] [this x y]))`

The `defprotocol` `list_lit`'s named children after the name are: an optional
`str_lit` (protocol doc), zero or more `kwd_lit` + value option pairs, and the
method signatures as `list_lit`s. Each method `list_lit`: first child is the
method name (`sym_lit`), followed by one or more `vec_lit` arities, then an
optional trailing `str_lit` (method doc). Extraction iterates the `list_lit`
children only (skipping options), and for each emits a `Defn` symbol:
`name`/`name_range` from the method name node, `range` = the method `list_lit`,
`params` = the arity vectors' text, `doc` = the trailing string.

## File Structure

- **Modify `src/index/extractor.rs`** — in `extract_def`, when
  `kind == DefKind::Defprotocol`, push one `Defn` symbol per method signature
  (in addition to the existing protocol symbol). Add a helper
  `extract_protocol_methods(children, source, file, ns_name, symbols)`.
- **Modify `src/index/jar_cache.rs`** — `CACHE_FORMAT_VERSION = 6`.
- **Modify `src/handlers/mod.rs`** — add a pure helper
  `factory_target_name(name) -> Option<&str>` (`"map->DB"`/`"->DB"` → `"DB"`)
  and a `resolve_factory(index, ns, name)` that returns the `Defrecord`/
  `Deftype` symbol; call `resolve_factory` as the last resort in both the
  qualified and bare branches of `resolve_symbol`.
- **Modify `tests/test_e2e.rs`** — e2e navigation tests for a protocol-method
  call and for `map->DB`/`->DB`.
- **Modify `docs/ROADMAP.md`** — check the protocols item.

Reuse existing extractor helpers (`named_children`, `sym_name_node`,
`node_text`, `node_to_lsp_range`, `strip_string_quotes`). No new types.

---

### Task 1: Extract protocol method symbols

**Files:**
- Modify: `src/index/extractor.rs`
- Modify: `src/index/jar_cache.rs`

- [x] **Step 1: Write failing unit tests**
  In `src/index/extractor.rs` `#[cfg(test)]`, using `extract(source, path)`:
  - `(ns my.ns)\n(defprotocol Storage (fetch [this id]) (store [this x] [this x y]))`
    yields symbols including `Storage` (kind `Defprotocol`) **and** `fetch`
    (fqn `my.ns/fetch`, kind `Defn`) and `store` (fqn `my.ns/store`), each with
    a `name_range` pointing at the method name.
  - A protocol with a doc string and an option
    (`(defprotocol P "doc" :extend-via-metadata true (foo [this]))`) still
    extracts `foo` and does **not** emit a symbol for the option keyword/value.
  - `store`'s `params` contains both arities (`[this x]` and `[this x y]`); a
    method's trailing string becomes its `doc`.

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib extractor`
  Expected: FAIL (method symbols absent).

- [x] **Step 3: Implement**
  In `extract_def`, after pushing the existing symbol, when
  `kind == DefKind::Defprotocol` call a new
  `extract_protocol_methods(&children[2..], source, file, ns_name, symbols)`
  that, for each `list_lit` child, builds a `Defn` `Symbol`: name + `name_range`
  from `sym_name_node` of the first child, `range` from the method `list_lit`,
  `params` from each `vec_lit`, `doc` from a trailing `str_lit`. Skip non-list
  children (doc string, `kwd_lit` options and their values).

- [x] **Step 4: Bump the JAR cache format version**
  In `src/index/jar_cache.rs` set `CACHE_FORMAT_VERSION = 6` (extractor output
  changed — protocol methods now extracted from JARs).

- [x] **Step 5: Run tests to verify they pass**
  Run: `cargo test --lib extractor jar_cache`
  Expected: PASS.

- [x] **Step 6: Commit**
  `git commit -m "Extract protocol method declarations as navigable symbols"`

### Task 2: Record/type factory navigation

**Files:**
- Modify: `src/handlers/mod.rs`

- [x] **Step 1: Write failing unit tests**
  In `src/handlers/mod.rs` `#[cfg(test)]`:
  - `factory_target_name("map->DB") == Some("DB")`,
    `factory_target_name("->DB") == Some("DB")`,
    `factory_target_name("plain") == None`, `factory_target_name("->") == None`.
  - A `resolve_symbol` test: build an `Index`, insert a `Defrecord` symbol `DB`
    in ns `my.ns`, then assert `resolve_symbol(&index, "map->DB", "my.ns")` and
    `"->DB"` both resolve to the `DB` symbol; a `Defn` named `foo` is **not**
    reachable via `->foo` (gated on record/type kind).

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib handlers`
  Expected: FAIL (`factory_target_name`/fallback absent).

- [x] **Step 3: Implement**
  Add `factory_target_name(name: &str) -> Option<&str>` (strip a leading
  `map->` then `->`; return `None` if the remainder is empty) and
  `resolve_factory(index, ns, name)` that looks up `factory_target_name(name)`
  in `ns` and returns the symbol only when its kind is `Defrecord` or `Deftype`.
  In `resolve_symbol`, call `resolve_factory` as the final fallback in both the
  qualified branch (using the resolved `full_ns`) and the bare branch (using
  `current_ns`), returning `ResolvedSymbol::Project`.

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib handlers`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Navigate record factory fns (->X / map->X) to the defrecord"`

### Task 3: End-to-end navigation

**Files:**
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing e2e tests**
  Modeled on existing definition e2e tests (`setup_project`, `did_open`,
  `goto_definition`, assert on the returned `range`):
  - A file defining `(defprotocol Storage (fetch [this id]))` and a usage
    `(fetch x)`: go-to-definition on the usage returns a location whose
    `range.start.line` is the `fetch` signature line in the `defprotocol`.
  - A file with `(defrecord DB [conn])` and usages `(map->DB {})` and `(->DB c)`:
    go-to-definition on each lands on the `DB` `defrecord` name line.

- [x] **Step 2: Run to verify it fails / passes**
  Run: `cargo test --test test_e2e protocol` and `... record` (or the chosen
  test names). They should pass once Tasks 1–2 are in; if a test reveals a gap,
  fix it.

- [x] **Step 3: Run the full check + e2e suite**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 4: Commit**
  `git commit -m "e2e: navigate to protocol methods and record factories"`

### Task 4: Mark the roadmap item done

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Check the box**
  Change the Phase 5 line `Clojure protocols support: navigation to protocol's
  method, navigation from map->DB to DB protocol` from `- [ ]` to `- [x]` with a
  short note (protocol methods indexed as ns vars; `->X`/`map->X` resolve to the
  record/type; impls in defrecord/extend-* not separately indexed).

- [x] **Step 2: Commit**
  `git commit -m "Mark Clojure protocols navigation complete in roadmap"`

---

## Notes & limitations

- **Declarations, not implementations.** A `(fetch …)` call navigates to the
  `defprotocol` signature, not to a specific `defrecord`/`extend-type` impl.
  "Find implementations" is a separate, larger feature (out of scope).
- **Factories are navigation+hover only.** `->X`/`map->X` are resolved at query
  time, not indexed, so they don't appear in `workspace/symbol` or completion,
  and rename does not treat them as the record.
- **`reify`/`extend-protocol`/`extend-type` method bodies** are not indexed as
  symbols; only the protocol's own method declarations are.

---

## Implementation summary (2026-06-15)

Implemented as designed on branch `protocols-records-navigation`. All `bb check`
(fmt + clippy `-D warnings` + 110 lib tests) and `bb e2e` (40 tests) pass.

- **`src/index/extractor.rs`** — `extract_def` now also calls
  `extract_protocol_methods`, emitting a `Defn` symbol per `defprotocol` method
  signature (name/range/arities/doc; options and the protocol doc string
  skipped).
- **`src/index/jar_cache.rs`** — `CACHE_FORMAT_VERSION` 5 → 6.
- **`src/handlers/mod.rs`** — `factory_target_name` + `resolve_factory`, wired
  as the last-resort fallback in both branches of `resolve_symbol`; `->X`/
  `map->X` resolve to a `Defrecord`/`Deftype` `X`.
- **Tests** — extractor unit tests (methods extracted, options skipped),
  `resolve_symbol`/`factory_target_name` unit tests, and two e2e tests
  (protocol-method call → signature; `map->DB`/`->DB` → defrecord).

### Codex review follow-ups (all fixed, final re-review clean)

Iterative second-opinion codex reviews caught four P2 correctness issues, each
fixed with a regression test:

- **Protocol declarations double-counted as occurrences.** Once methods are
  indexed, the occurrence pass walked the `defprotocol` body and recorded each
  method declaration as a usage (breaking references/rename). Fixed by skipping
  the protocol body in `walk_def_form`. Test:
  `test_occurrence_protocol_method_decl_not_recorded`.
- **`map->` accepted `deftype`.** `deftype` generates `->X` but no `map->X`;
  `resolve_factory` now allows `map->` only for `defrecord`. Test:
  `map_constructor_is_record_only`.
- **Referred constructors didn't resolve.** `(:require [recs :refer [->DB]])`
  failed because the ctor fqn isn't indexed; the refer branch now resolves the
  factory in the referred namespace. Test:
  `resolve_symbol_navigates_referred_factory`.
- **Core shadowed a local constructor.** A local record whose `->X` collides
  with a `clojure.core` name (e.g. `->Eduction`) must win; the factory fallback
  now runs before the core fallback. Test: `local_constructor_shadows_core`.
