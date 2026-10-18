# References and Rename Through `:refer :all` / `(:use …)` Implementation Plan

**Status: completed 2026-10-18**

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A bare call that reaches a var through `[ns :refer :all]` or `(:use ns)` is a site of that var for references, rename, document highlight and `prepareRename`, as it already is for definition and hover.

**Tech Stack:** Rust, tower-lsp, tree-sitter-clojure; the `bb compare` harness for verification.

---

## Design

### The bug

`bb compare clj-kondo` (2026-10-17): references on `read-past`, defined in
clj-kondo's inlined tools.reader `impl/commons.clj`, answer 1 site of 6. The
five callers (`reader.clj:512`, `:762`, `:815`, `edn.clj:170`, `:305`) bring
the namespace in with `[… impl.commons :refer :all]`. A rename would leave them
calling a name that no longer exists. 22 references and 22 rename probes on
that corpus diverge this way.

Definition already works: `handlers::resolve_symbol` tries the current namespace,
then each `NsMeta.refer_all` namespace (public vars and record constructors). The
occurrence walker cannot do the same at extraction time — it has no index — so
`record_occurrence` files a bare name that is neither referred, nor defined in the
file, nor core under the *current* namespace: `reader`'s call is recorded as
`…reader/read-past`, a var nobody defines. References on `…commons/read-past`
never look at it; references started on the call resolve to that phantom fqn and
list only its own spellings.

### The fix: resolve at query time, one rule for every reader

Index time cannot settle it either: the namespace a `:refer :all` points at may be
indexed later (libraries index concurrently) or change on any save. So the
resolution moves to query time, through one helper that both definition and the
site-based handlers call, so they cannot disagree:

```rust
/// The var a bare `name` reaches through `refer_all` in a file of namespace
/// `file_ns`: none when `file_ns` defines `name` itself (or builds it as a
/// record constructor), else the first `refer_all` namespace holding a public
/// var or constructor of that name, in order. The rule `resolve_symbol` applies.
pub(crate) fn refer_all_target(
    index: &Index,
    file_ns: &str,
    refer_all: &[String],
    name: &str,
    dialect: Dialect,
) -> Option<(String, Symbol)>   // (target namespace, its symbol)
```

It lives in `src/handlers/mod.rs` beside `resolve_symbol`, and `resolve_symbol`'s
`refer_all` loop is rewritten to call it (behavior unchanged; the current-namespace
check already happened just before that loop).

A second helper turns an occurrence's recorded fqn into the var it names:

```rust
/// `fqn` as recorded in a file of namespace `file_ns`, or the `refer_all`
/// var it names when it is a bare name filed under `file_ns` that the
/// namespace does not define.
pub(crate) fn referred_fqn(
    index: &Index, file_ns: &str, refer_all: &[String], fqn: &str, dialect: Dialect,
) -> Option<String>
```

`Some` only when `refer_all` is non-empty and `fqn` is `{file_ns}/{name}`;
otherwise the caller keeps `fqn`. It lives in `src/handlers/references.rs`.

The walker records an explicit `app/f` exactly like a bare `f` in `app` (same
fqn, the range of the name half), so a recorded fqn does not prove the token was
bare. The callers apply `referred_fqn` only to an occurrence whose token is bare
— the character before its name range is not `/` — read from the buffer text,
or from the file on disk for an indexed file (read only when the file holds a
candidate occurrence, as `occurrences_for` already does for constructor calls).
A shared helper does the rewrite for a whole file:

```rust
/// `occs` of one file with every bare refer-all occurrence's fqn replaced by
/// the var it names ([`referred_fqn`]); `None` when nothing changed.
pub(crate) fn with_referred_fqns(
    index: &Index, file_ns: &str, refer_all: &[String], occs: &[Occurrence],
    text: &str, dialect: Dialect,
) -> Option<Vec<Occurrence>>
```

Rewriting the whole list *before* matching matters for constructors: a bare
`->R` / `map->R` call in `app` is recorded as `app/->R`; rewritten to `lib/->R`,
it is then found by the existing `constructor_forms` / `matching_occurrences`
logic for `lib/R` and narrowed to `R` as any constructor call is.

Callers:

1. **`references::resolve_fqn_at`** (references, rename, `prepareRename`,
   highlight all start here): the occurrence under the cursor goes through
   `referred_fqn`, with the live buffer's `NsMeta` (the first element
   `extract_full_tree` already returns). A cursor on the call in `reader.clj`
   now resolves to `…commons/read-past`.
2. **`references::occurrences_for`**: for a target `N/name`, each file whose
   `refer_all` contains `N` and holds an occurrence filed under `file_ns` with
   that name or one of its constructor names is read, its occurrences go through
   `with_referred_fqns`, and the existing matching runs on the result. Indexed files read `ns` and `refer_all` through a new
   `Index::file_refer_all(&Path) -> Option<(String, Vec<String>)>` (from the
   file's `FileRecord`, `None` when `refer_all` is empty, so the common case
   costs one map lookup per file). Open buffers read them from the live tree
   (`extractor::ns_meta_of_tree`, made `pub`). Rename keeps
   `references::spells`, so only tokens spelling the old name are edited — a
   bare `read-past` does.
3. **`highlight`**: the buffer's occurrences go through `with_referred_fqns`
   before matching, so highlight on either side agrees with references.

Unchanged on purpose:
- The occurrence walker and the index: occurrences keep their recorded fqns, so
  no `CACHE_FORMAT_VERSION` bump and nothing to maintain on insert/remove.
- `unused-private-var`: a private var is never referred.
- A name the file's own namespace defines (in any of its files), a core name the
  walker already filed under core, and a private var of the referred namespace
  map to nothing.
- `:use … :only [f]` still refers the whole namespace (`collect_use_namespaces`
  over-offers); this plan follows that.
- Library targets work the same way when the library is indexed: references on
  `is` from a `(:use clojure.test)` file list the project's bare `is` calls;
  rename stays refused for library vars.

### Testing

- Unit tests (`src/handlers/references.rs` tests module, or `mod.rs` beside
  `resolve_symbol_falls_back_to_refer_all_namespaces`): `refer_all_target` and
  `referred_fqn` — maps a bare name to the referred var; not when the file's
  namespace defines it; not for a private var; first namespace wins; record
  constructor `->T` maps.
- E2E (`tests/test_e2e.rs`): a project with `lib.core` defining `f` and
  `app.core` doing `(:require [lib.core :refer :all])` plus another file doing
  `(:use lib.core)`, each calling `(f)`:
  references on `f`'s definition list both calls; references started on a call
  list the definition and both calls; rename from the definition edits all three
  tokens; `documentHighlight` on the call in `app.core` marks it; a third file
  defining its own `f` and also referring `lib.core` wholesale is not listed;
  an explicit `app.core/f`-style qualified token is not remapped; `lib.core`
  defines a record `R` and `app.core` calls `(->R 1)`: references and rename
  from `R` include that call, narrowed to `R`; after an unsaved edit drops the
  `:refer :all` from `app.core`'s buffer, references no longer list its call;
  references and `prepareRename` started *on* the qualified `app.core/f` token
  do not resolve to `lib.core/f`; references started on the bare `->R` call
  resolve to `lib.core/R` (namespace rewritten first, then
  `canonical_type_fqn`).
- Gates: `bb check`, `bb e2e`, `bb e2e-pulse` (client-visible references),
  `bb compare` (clj-kondo's `var-def/defn` references/rename rows should gain
  about 22 agreements each; penpot must not lose any), `bb soak` (references are
  in its probe set).

## File Structure

- Modify `src/handlers/mod.rs` — `refer_all_target`; `resolve_symbol` uses it.
- Modify `src/handlers/references.rs` — `referred_fqn`; `resolve_fqn_at` and
  `occurrences_for` apply it; unit tests.
- Modify `src/handlers/highlight.rs` — map buffer occurrences.
- Modify `src/index/mod.rs` — `Index::file_refer_all`.
- Modify `src/index/extractor.rs` — `ns_meta_of_tree` becomes `pub`.
- Modify `tests/test_e2e.rs` — the e2e test.
- Modify `AGENTS.md`, `docs/FEATURES.md`, `docs/ROADMAP.md`, `docs/MEMORY.md`.

## Tasks

### Task 0: Open the ROADMAP item

- [x] **Step 1:** Add a Milestone 5 item to `docs/ROADMAP.md`, beside the
  2026-10-17 compare items: "**References and rename through `:refer :all` /
  `(:use …)`**", one line of evidence (`read-past`, 1 of 6), and
  `Plan: [2026-10-18-0812-refer-all-references.md](plans/2026-10-18-0812-refer-all-references.md) — in progress`.
- [x] **Step 2: Commit** the plan and the item: `git commit -m "roadmap: plan references through :refer :all"`

### Task 1: One `refer_all` rule

**Files:**
- Modify: `src/handlers/mod.rs`

- [x] **Step 1: Write failing unit tests** for `refer_all_target` next to
  `resolve_symbol_falls_back_to_refer_all_namespaces` (reuse its index-building
  helpers): maps `f` to `lib`; `None` when `app` defines `f`; `None` for a
  `DefnPrivate` in `lib`; with `refer_all = ["a", "b"]` both defining `f`, `a`
  wins; `->R` maps when `lib` defines record `R`.
  Run: `cargo test --lib refer_all_target` — Expected: FAIL (function missing).
- [x] **Step 2: Implement** `refer_all_target` (signature in Design) by lifting
  the body of `resolve_symbol`'s `refer_all` loop, plus the "file_ns defines it"
  guard (`lookup_in_ns_for(index, file_ns, name, dialect)` or
  `resolve_factory(index, file_ns, name, dialect)` → `None`). Replace the loop in
  `resolve_symbol` with a call; that path has already tried the current
  namespace, so the guard changes nothing there.
- [x] **Step 3: Run** `cargo test --lib` — Expected: PASS, the existing
  `resolve_symbol` tests included.
- [x] **Step 4: Commit** `git commit -m "handlers: one refer_all resolution rule"`

### Task 2: References, rename and highlight read it

**Files:**
- Modify: `src/handlers/references.rs`, `src/handlers/highlight.rs`,
  `src/index/mod.rs`, `src/index/extractor.rs`

- [x] **Step 1: Write failing unit tests** for `referred_fqn`: `app/f` in an
  `app` file referring `lib` wholesale → `lib/f`; `app/g` that `app` defines →
  `None`; empty `refer_all` → `None`; `lib2/f` (not filed under `file_ns`) →
  `None`.
- [x] **Step 2: Implement** `referred_fqn` (signature in Design), splitting the
  fqn at the `/` after `file_ns` so a name containing `/` (`clojure.core//`) is
  handled.
- [x] **Step 3: `Index::file_refer_all`** — from `files`, `None` when the
  record's `refer_all` is empty. Make `extractor::ns_meta_of_tree` `pub`.
- [x] **Step 4: `resolve_fqn_at`** — map the occurrence found under the cursor
  through `referred_fqn` with the buffer's `NsMeta`, before
  `canonical_type_fqn`.
- [x] **Step 5: `with_referred_fqns`** (signature in Design): rewrites only
  occurrences whose token is bare (character before the name range is not `/`);
  unit test with an explicit `app/f` that must stay `app/f` and a bare `->R`
  that becomes `lib/->R`.
- [x] **Step 6: `occurrences_for`** — for each indexed file with
  `file_refer_all` containing the target's namespace and an occurrence filed
  under its own namespace with the target's name or a constructor name of it,
  read the file, run `with_referred_fqns`, and match on the result instead of
  the stored list; open buffers the same with their live `NsMeta` and text.
- [x] **Step 7: `highlight`** — run `with_referred_fqns` on the buffer's
  occurrences before `matching_occurrences`.
- [x] **Step 8: Run** `cargo test --lib` — Expected: PASS.
- [x] **Step 9: Commit** `git commit -m "references: bare calls through :refer :all are sites of the referred var"`

> Deviation: `occurrence_range_at` (rename's `:rename` check and `prepareRename`'s range) matched raw occurrences too, so `prepareRename` on a bare referred call answered "nothing to rename here"; it now runs `with_referred_fqns` like the others.
> Deviation (codex): `with_referred_fqns` takes the file's live definitions (`defined: &[Symbol]`, empty for a saved file), so an unsaved `(defn f …)` shadows a referred `f`; `resolve_fqn_at` calls it on the one occurrence instead of `referred_fqn` directly. It resolves each name once and splits the text into lines only when a candidate needs its token checked (codex measured the first version as quadratic).

### Task 3: E2E

**Files:**
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Write the test** described in Design → Testing, modeled on an
  existing references/rename e2e test (e.g. `test_e2e_references_find_usage_in_unopened_alias_test_dir`)
  that writes extra files into the temp project before `initialize`. Assert on
  `textDocument/references` from the definition and from a call, on
  `textDocument/rename` edits (three tokens, the self-defining file untouched),
  and on `textDocument/documentHighlight` at the call.
- [x] **Step 2: Run** `cargo test --test test_e2e refer_all` — Expected: PASS.
- [x] **Step 3: Commit** `git commit -m "e2e: references and rename through :refer :all"`

> Deviation: a question started on a call needs that buffer open (`resolve_fqn_at` reads the live document, as for every handler), so the test opens `app` first. It also checks `prepareRename` on the call and an unsaved shadowing definition.

### Task 4: Gates, records, docs

- [x] **Step 1:** `bb check` — Expected: green.
- [x] **Step 2:** `bb e2e` and `bb e2e-pulse` — Expected: green.
- [x] **Step 3:** `bb compare` — Expected: clj-kondo `var-def/defn` references
  and rename rows gain agreements (`read-past`, `throwing-reader`, …); no row of
  either corpus loses agreement beyond resampling. Record both tables in
  `docs/MEMORY.md` under a dated section.
- [x] **Step 4:** `bb soak` — Expected: pass.
- [x] **Step 5: Docs.** AGENTS.md: the `NsMeta.refer_all` sentence ("head
  resolution, completion and `resolve_symbol` all consult it") gains references,
  rename and highlight through `references::referred_fqn` /
  `handlers::refer_all_target`, resolved at query time because the walker files
  such a call under the current namespace. `docs/FEATURES.md`: references and
  rename list calls through `:refer :all` / `:use`. README: update it in the
  same change if its feature or support-boundary lines mention references or
  `:refer :all`; otherwise leave it. `docs/ROADMAP.md`: tick the item, status `done`.
- [x] **Step 6: Commit** `git commit -m "docs: references through :refer :all"`

> Deviation: the plan expected about 22 agreements per references/rename row; the gain is 10 in all (`var-def/defn` 16 → 6 diverge), because the 2026-10-17 `.cljc` change had already moved that row from 43 to 16. README is unchanged, since it does not mention `:refer :all`.

## Completion summary

Implemented `handlers::refer_all_target` (the one `:refer :all` rule, now
also behind `resolve_symbol`), `references::referred_fqn` and
`with_referred_fqns`, `Index::file_refer_all`, and a public
`extractor::ns_meta_of_tree`. `resolve_fqn_at`, `occurrences_for`,
`occurrence_range_at` and `highlight` read bare refer-all calls as sites of
the referred var. One e2e test covers references from both ends, rename,
prepareRename, highlight, constructor calls, a qualified token, an unsaved
refer removal and an unsaved shadowing definition.

Gates: `bb check`, `bb e2e`, `bb e2e-pulse`, `bb soak` green; `bb compare`
clj-kondo 4744 → 4754 agree, penpot unchanged.

Deviations:
- `occurrence_range_at` needed the rewrite too; without it, `prepareRename` on a bare call refused.
- Following codex: `with_referred_fqns` takes the buffer's live definitions, so an unsaved def shadows a referred name. It resolves each name once and splits lines lazily, where the first version was quadratic.
- The e2e test opens the calling buffer before asking from it.
- The compare gain was smaller than estimated (10, not ~44), because an earlier change had already fixed part of that row.

What the plan could have specified better: list every caller of the raw
occurrence matchers (`occurrence_range_at` was missed) and account for an
open buffer's unsaved definitions in the shadowing rule.
