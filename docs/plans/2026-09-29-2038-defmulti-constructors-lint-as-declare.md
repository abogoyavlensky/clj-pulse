# Defmulti, Constructor and `:lint-as` Declare Sites Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** References and rename reach a `defmulti`'s own line, a `deftype`/`defrecord`'s constructor calls, and the names a `:lint-as … clojure.core/declare` macro declares; the `:lint-as … defprotocol` half is proven and its corpus divergence re-attributed.

**Tech Stack:** Rust, tree-sitter-clojure, tower-lsp; `bb check` / `bb e2e` / `bb compare`.

**Backlog entries closed:** `docs/backlog/2026-09-17-defmulti-missing-from-its-own-references.md`,
`docs/backlog/2026-09-17-constructor-calls-are-not-references.md`,
`docs/backlog/2026-09-17-lint-as-defprotocol-and-declare-half-honored.md`.

---

## Design

### 1. A `defmulti` is missing from its own references

**Root cause (verified in code).** `extract_def` emits a `Defmethod` symbol for every
`(defmethod foo …)`, and its fqn is the multimethod's own (`ns/foo`). `Index::insert_file`
inserts it through `rank_insert` with the same rank as the file's `defmulti`, and among
equals the last writer wins, so the last `defmethod` in the file overwrites the
`defmulti`'s slot. `references` then lists `index.lookup_all(fqn)` — the defmethod head,
which is also an occurrence — and the `defmulti` line never appears. `inspect*`
(`declare` at :11, `defmulti` at :37) fails the same way. `bb compare` on the clj-kondo
corpus: `var-def/defmulti` 3 of 6 agree, each divergence missing exactly the `defmulti`
line.

**Fix: a `Defmethod` symbol never occupies an index slot.** `insert_file` and
`insert_lib_file` skip symbols of kind `Defmethod` — not inserted, not in
`FileRecord.fqns`, not in `ns_symbols`. Extraction keeps emitting them: `documentSymbol`
reads the live extraction for the outline (`SymbolKind::METHOD`), and `resolve_fqn_at` /
`documentHighlight` already skip the kind. Nothing in the index needs them; today a
defmethod for a multimethod referred from another namespace even plants a junk
`current.ns/foo` var, which the fix removes.

### 2. Constructor calls are not references of a `deftype`/`defrecord`

**What the walker already does.** Constructor tokens are recorded with predictable fqns
and no special casing: bare `(Foo. 1)` → `ns/Foo.` (`in_ns` fallback), `(->Foo 1)` →
`ns/->Foo`, referred `->Foo` and alias-qualified `c/map->Foo` → `my.ns/->Foo` /
`my.ns/map->Foo`. clj-kondo (probed with a scratch project) counts an in-namespace
`(Foo. 1)` as a usage of `Foo`, keys `->Foo` / `map->Foo` usages under those names with
their definitions at the record's name token, and does *not* count `(Foo. 1)` in another
namespace that `:import`s the class. The fix lives in the handlers; the extractor is
untouched.

**Fix: one helper spells the constructor shapes, every handler reads it.**

- `handlers::factory_target(name)` (mod.rs) grows the third shape: `"Foo."` →
  `("Foo", Shape::Dot)`; today it returns `(target, is_map_ctor)`. Replace the bool with
  an enum so the shape is named once:

  ```rust
  pub(crate) enum CtorShape { Positional, Map, Dot }   // `->T`, `map->T`, `T.`
  pub(crate) fn factory_target(name: &str) -> Option<(&str, CtorShape)>
  ```

  `resolve_factory` accepts `Dot` for both kinds and `Map` for records only, as now.
  `word_at` already includes `.` in identifiers, so hover on `Foo.` shows the record.
- `references::constructor_forms(index, fqn) -> Vec<CtorForm>` answers, for an fqn that
  any entry of `index.lookup_all(fqn)` (both dialect slots) holds as `Defrecord` or
  `Deftype`, the occurrence fqns of its constructors and how many UTF-16 columns of the
  token surround the type name. A form whose fqn the index holds as a symbol of its own
  (`(defn ->Foo …)` beside the record) is left out: an explicit var outranks the generated
  constructor, so its calls stay its own and a record rename never rewrites them.

  ```rust
  pub(crate) struct CtorForm { pub fqn: String, pub prefix: u32, pub suffix: u32 }
  // `ns/->T` (2, 0) and `ns/T.` (0, 1) for both kinds; `ns/map->T` (5, 0) for records only.
  ```

  `narrow(occ, form) -> Occurrence` returns the occurrence with `name_range.start.character
  += prefix` and `end.character -= suffix`, so a rename edit rewrites only the `T` part
  (`->Foo` → `->Bar`, `Foo.` → `Bar.`).
- `occurrences_for(index, documents, fqn)` becomes the one place that expands: for each
  file it keeps `occ.fqn == fqn` plus every `CtorForm` match, narrowed. Callers —
  `references`, `rename`, and the keyword path — need no change. `occurrence_range_at`
  (prepareRename) and `highlight.rs` filter live occurrences themselves; both use a shared
  `matching_occurrences(occs, fqn, forms)` so `prepareRename` on `->Foo` pre-selects the
  `Foo` span and highlight underlines it.
- `resolve_fqn_at` canonicalizes at the end: an occurrence fqn whose name part is a
  constructor shape, whose stem fqn the index holds as `Defrecord` / `Deftype`
  (`Map` records only), and which is *not itself* an indexed symbol, returns the stem fqn. So a cursor on `->Foo` asks definition,
  references, rename and highlight about the record. Definition on `->Foo` already lands on
  the record through the bare-word fallback; canonicalizing makes the position path answer
  first.

**Decision surfaced in the design:** references on a record *include* `->T` / `map->T`
calls although kondo keys them under separate vars. Rename must rewrite them or leave
broken code, and references and rename share sites. `bb compare`'s
`var-def/defrecord` / `var-def/deftype` rows therefore gain superset answers, allowlisted
with a `KNOWN` entry (`missing: 0`), like the protocol-implementation one.

Out of scope, matching kondo: `(:import [my.ns Foo])` + `(Foo. 1)` in another namespace
(recorded as `other.ns/Foo.`, never linked).

### 3. `:lint-as … clojure.core/declare` declares nothing

**Root cause.** `DefKind::from_def_symbol` has no `declare` arm, so `settings::merge`
logs "ignoring non-def :lint-as target" and drops `me.raynes.conch/programs
clojure.core/declare`. `(programs rm mkdir mv)` then defines nothing and rename is refused
(`var-def/programs` 3 of 6, the three rename probes).

**Fix.**

- `from_def_symbol("declare")` → `DefKind::Declare`. `process_top_level_list` routes a
  `Declare` kind — from the literal head, from `:lint-as`, or from a qualified
  `x/declare` head — to `extract_declare`, which already records one symbol per name in
  `children[1..]`.
- `walk_list`: `str_to_defkind("declare")` now hits, so `walk_def_form` gets a `Declare`
  arm. It records a declared name as an occurrence of `ns/name` **only when the name is
  not in `ctx.declared`** — the names whose `Declare` symbol survived extraction (the
  file does not define them). A name the file defines elsewhere keeps today's behavior:
  the declare line is a usage, so references and rename reach it
  (`test_e2e_declare_defers_to_the_real_definition`). A declare-only name is represented
  by its symbol alone, which removes an existing double count: today references list that
  line twice (symbol + occurrence) and rename emits two edits for one range.
  `OccurrenceCtx` gains `declared: HashSet<&str>`; the two other constructors of the
  context pass an empty set.
- For a lint-as'd head the walker still pushes the head occurrence
  (`me.raynes.conch/programs`) before `walk_def_form`, as for every mapped macro.
- `walk_scope_def`'s default arm binds nothing for `Declare`; no change.
- Extractor output changes (the declare-only occurrence disappears):
  `CACHE_FORMAT_VERSION` 20 → 21.

### 4. `:lint-as … clojure.core/defprotocol` — already honored

`process_top_level_list` → `extract_def(Defprotocol)` → `extract_protocol_methods`, and
`walk_list` → `head_def_kind` → `walk_def_form(Defprotocol)` skips the body: the mapped
macro indexes its methods exactly like `defprotocol`. `bb compare` (2026-09-29, this
session) shows every missed caller of the `defprotocol+` methods goes through
`clj-kondo.impl.rewrite-clj.node` — the `import-vars` re-export namespace (`node/tag`
in indent.clj:29, parser/core.clj:174, hooks_api.clj:91; bare `tag` in node.clj:122 next
to `(import-vars …)`), and the missed `Node` sites are `node/Node` in `defrecord` specs.
The 16 extra sites per method are protocol implementations, the existing `KNOWN` reason.
This plan adds a regression test that pins the behavior and moves the corpus evidence to
the import-vars backlog entry.

### Testing strategy

Unit tests in `tests/test_extractor.rs` (declare occurrence rule, lint-as declare,
lint-as defprotocol), an index test in `src/index/mod.rs` (defmethod never displaces the
defmulti), e2e tests in `tests/test_e2e.rs` on `simple_project` (defmulti references and
rename; constructor references, rename, prepareRename, highlight; cross-file
`c/map->Foo`), and a `programs`-style macro in `lint_as_project`. Gates: `bb check`,
`bb e2e`, `bb e2e-pulse` (rename edits are client-visible), `bb compare clj-kondo`
recorded in `docs/MEMORY.md`.

Expected `bb compare clj-kondo` movement (baseline 2026-09-29, `.tmp/compare-clj-kondo.log`):

| Bucket | Before | After |
|---|---|---|
| `var-def/defmulti` | 3 agree / 3 diverge | 6 / 0 |
| `var-def/declare` | 27 / 1 | 28 / 0 |
| `var-def/programs` | 3 / 3 | 6 / 0 |
| `var-def/deftype` | 0 / 20 | 20 agree, or known (superset) where `->T` calls exist |
| `var-def/defrecord` | 44 / 6 (2 null) | in-file `T.` sites agree; `->T` extras known; the 2 null (`utils.clj:56`, a `defrecord` inside a wrapping form) stay — backlog "defs nested in a wrapping macro" |
| `var-def/defprotocol+` | 0 / 26 | unchanged, re-attributed to import-vars |

## File Structure

- Modify `src/index/mod.rs` — `from_def_symbol` gains `declare`; `insert_file` /
  `insert_lib_file` skip `Defmethod`; unit test.
- Modify `src/index/extractor.rs` — `process_top_level_list` routes `Declare`;
  `OccurrenceCtx.declared`; `walk_def_form` `Declare` arm.
- Modify `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` 21.
- Modify `src/handlers/mod.rs` — `CtorShape`, `factory_target` with the `T.` shape,
  `resolve_factory`.
- Modify `src/handlers/references.rs` — `CtorForm`, `constructor_forms`,
  `matching_occurrences`, `occurrences_for` expansion, `occurrence_range_at`,
  `resolve_fqn_at` canonicalization.
- Modify `src/handlers/highlight.rs` — use `matching_occurrences`.
- Modify `tests/fixtures/simple_project/src/…` — a new `records.clj` (+ a consumer) and
  a defmulti with same-file defmethods; check `compare_simple_project` stays at zero.
- Modify `tests/fixtures/lint_as_project/` — `programs`-style macro mapped to `declare`,
  a `defprotocol`-mapped macro.
- Modify `tests/test_extractor.rs`, `tests/test_e2e.rs`, `tests/test_compare.rs` (`KNOWN`).
- Modify docs: `docs/FEATURES.md`, `docs/SETTINGS.md`, `docs/MEMORY.md`, `CLAUDE.md`,
  `docs/ROADMAP.md`, the three backlog files, the import-vars backlog file.

---

### Task 1: A `Defmethod` symbol never takes an index slot

**Files:**
- Modify: `src/index/mod.rs`
- Test: `src/index/mod.rs` (unit tests module), `tests/test_e2e.rs`

- [x] **Step 1: Write the failing index test**
  In the `tests` module of `src/index/mod.rs`, next to `same_dialect_collision_keeps_the_losers_record`: insert one project file whose symbols are a `Defmulti` `app/dispatch` at line 2 followed by a `Defmethod` with the same fqn at line 4 (build them with the module's existing symbol helper). Assert `index.lookup("app/dispatch")` is the `Defmulti`, `lookup_all` has exactly one entry, and `ns_symbols["app"]` holds the fqn once. Add a second case: a file whose only symbol is a `Defmethod` for `other/foo` — `lookup("other/foo")` is `None` and the file's `FileRecord.fqns` is empty.

- [x] **Step 2: Write the failing e2e test**
  Add `(defmulti area :shape)` plus two same-file `(defmethod area …)` forms and a call `(area {:shape :circle})` to `tests/fixtures/simple_project/src/core.clj` (or a new small file if core.clj is crowded; keep `compare_simple_project` in mind — the fixture is linted by kondo too). Test `test_e2e_defmulti_is_its_own_reference`: `references` with `includeDeclaration` from the call includes the `defmulti` line and lists each `defmethod` line once; `rename` from the `defmulti` line produces one edit on that line and one per defmethod head and call, no duplicate ranges.
  Run: `cargo test --test test_e2e test_e2e_defmulti` — Expected: FAIL (defmulti line missing).

- [x] **Step 3: Implement**
  In `insert_file` and `insert_lib_file`, skip `sym.kind == DefKind::Defmethod` before the fqn bookkeeping and `rank_insert` (one shared predicate with a doc comment: a defmethod head names the multimethod it extends; it is a symbol for the outline alone). Leave extraction and `symbols.rs` untouched.

- [x] **Step 4: Run the tests**
  Run: `cargo test --lib index && cargo test --test test_e2e test_e2e_defmulti` — Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "index: a defmethod symbol never displaces the defmulti's slot"`

> Deviation: the fix, `multi.clj` fixture and e2e test were written by a concurrent session in the same tree (stopped on the user's instruction); kept as-is after a red/green check. The index test also asserts a library `defmethod` plants nothing.

### Task 2: Constructor shapes in one place

**Files:**
- Modify: `src/handlers/mod.rs`

- [x] **Step 1: Extend the unit tests**
  In `factory_target_strips_prefixes_and_flags_map_ctor`, switch the expectations to the `CtorShape` enum and add `"Foo."` → `("Foo", Dot)`, `"."` → `None`, `"java.io.File."` → `("java.io.File", Dot)` (the index lookup, not the parser, decides it is not a record). Add to `map_constructor_is_record_only` that `"T."` resolves for a `Deftype`, and to `resolve_factory_ignores_non_record_targets` that `"foo."` with a `Defn` `foo` resolves to nothing.

- [x] **Step 2: Implement**
  Define `pub(crate) enum CtorShape { Positional, Map, Dot }`; `factory_target` returns `Option<(&str, CtorShape)>` — `map->` first, then `->`, then a trailing `.` (non-empty stem, and the stem must not itself end in `.`). `resolve_factory` allows `Map` for `Defrecord` only. Update the two call sites in `resolve_symbol`.

- [x] **Step 3: Run**
  Run: `cargo test --lib handlers` — Expected: PASS.

- [x] **Step 4: Commit**
  `git commit -m "handlers: name the three constructor shapes in factory_target"`

> Deviation: the kind rule is a shared `builds(kind, shape)` helper, so `resolve_factory` and Task 3's `constructor_forms` / canonicalization read one rule. `resolve_factory` has four call sites, not two; none needed changes.

### Task 3: Constructor occurrences are sites of their type

**Files:**
- Modify: `src/handlers/references.rs`, `src/handlers/highlight.rs`
- Modify: `tests/fixtures/simple_project/src/records.clj` (new), `tests/fixtures/simple_project/src/consumer.clj` (or a new consumer)
- Test: `tests/test_e2e.rs`, `tests/test_compare.rs`

- [x] **Step 1: Fixture**
  `records.clj` (`simple.records`): `(defrecord Point [x y])`, `(deftype Cell [v])`, and a fn using `(Point. 1 2)`, `(->Point 3 4)`, `(map->Point {:x 5})`, `(Cell. 1)`, `(->Cell 2)`. A consumer namespace requires it `:as rec` and `:refer [->Point]`, and calls `(rec/map->Point {})`, `(->Point 0 0)`, `(rec/->Cell 1)`.

- [x] **Step 2: Write the failing e2e tests**
  - `test_e2e_constructor_calls_are_references_of_the_record`: references from the `defrecord Point` name include the `Point.`, `->Point`, `map->Point` lines in records.clj and the three consumer sites (the `:refer [->Point]` token included — kondo counts it too); references from the `deftype Cell` name include `Cell.` and both `->Cell` calls.
  - `test_e2e_rename_record_rewrites_its_constructors`: rename `Point` → `Pt` from the `(->Point 3 4)` cursor (canonicalization) yields edits whose ranges cover exactly `Point` inside `->Point`, `map->Point`, `Point.` — assert `start.character` is 2 / 5 / 0 past the token start and the range length is 5 — plus the definition. `prepareRename` on `->Point` returns the `Point` sub-range from a cursor on the `P` *and* from a cursor on the `-` of the prefix.
  - `test_e2e_explicit_constructor_var_wins`: a fixture namespace with `(defrecord Box [v])` and `(defn ->Box [v] (Box. v))` plus a `(->Box 1)` call — references on `Box` list `Box.` but not the `->Box` call; definition from the call lands on the `defn`.
  - `test_e2e_highlight_covers_constructors`: `documentHighlight` on `Point` in records.clj lists the definition as WRITE and the three constructor sub-ranges as READ.
  - Definition from `(Cell. 1)` lands on the `deftype` line (guards the `Dot` shape through the position path).
  Run: `cargo test --test test_e2e constructor` — Expected: FAIL.

- [x] **Step 3: Implement in references.rs**
  - `CtorForm { fqn, prefix, suffix }` and `constructor_forms(index, fqn)`: any `lookup_all(fqn)` entry of kind `Defrecord` → three forms, `Deftype` → `->T` and `T.`, otherwise empty; drop a form whose fqn `lookup_all` finds as a symbol. Build the fqns from the symbol's `ns` and `name`.
  - `narrow(occ, &form)` and `pub(crate) fn matching_occurrences(occs: &[Occurrence], fqn: &str, forms: &[CtorForm]) -> Vec<Occurrence>` (exact matches first, then each form's matches narrowed).
  - `occurrences_for`: compute the forms once, use `matching_occurrences` for both the indexed and the live branches.
  - `occurrence_range_at`: test cursor containment against the *original* constructor
  token (a cursor on the `->` of `->Foo` is on the site), and return the narrowed range.
  `matching_occurrences` therefore also has to make the original range available — return
  pairs `(narrowed, token)` from a sibling helper, or narrow inside `occurrence_range_at`
  after matching. Rename and references use the narrowed range only.
  - `resolve_fqn_at`: after an occurrence match (and only there — a symbol match is already the type), `canonical_type_fqn(index, &fqn)`: split at the last `/`, `factory_target` on the name part, look up `ns/stem`, accept by the same kind rule as `resolve_factory`. Do the same on the alias-fallback result at the end.

- [x] **Step 4: highlight.rs**
  Replace the `occ.fqn == fqn` filter with `matching_occurrences(&occs, &fqn, &references::constructor_forms(index, &fqn))`.

- [x] **Step 5: KNOWN entry in tests/test_compare.rs**
  Add, dated 2026-09-29: bucket prefix `var-def/defrecord` and `var-def/deftype` (two entries or one with `starts_with` on both), matching `Verdict::Diverge { missing: 0, .. }`, reason "a record's `->T`/`map->T` calls are sites of the record in clj-pulse (rename must rewrite them); kondo keys them under the constructor var". Keep `compare_simple_project` at zero *new* divergences — the new fixture's `->Point` calls become `known`, not `diverge`.

- [x] **Step 6: Run**
  Run: `cargo test --test test_e2e constructor && cargo test --test test_e2e -- declare && cargo test --test test_compare compare_simple_project` — Expected: PASS.

- [x] **Step 7: Commit**
  `git commit -m "references: constructor calls are sites of their deftype/defrecord"`

> Deviation: the consumer is a new `records_consumer.clj`; the explicit-`->Box` case is written by its test, so `compare_simple_project` never sees a redefined var. `CtorForm` carries its `spelling`, and `matching_occurrences` takes the source text: a constructor site must spell the constructor (codex review — a `:refer [map->Foo] :rename {map->Foo f}` call is recorded under the constructor with a range covering `f`, and narrowing it produced an inverted rename edit). Indexed files are read from disk only when they call a constructor.

### Task 4: `:lint-as … clojure.core/declare`

**Files:**
- Modify: `src/index/mod.rs` (`from_def_symbol`), `src/index/extractor.rs`, `src/index/jar_cache.rs`
- Modify: `tests/fixtures/lint_as_project/.clj-kondo/config.edn`, `tests/fixtures/lint_as_project/src/app/core.clj`
- Test: `tests/test_extractor.rs`, `tests/test_e2e.rs`, `src/settings.rs` tests

- [x] **Step 1: Write the failing extractor tests**
  - `test_declare_only_name_is_a_symbol_not_an_occurrence`: `(ns app)\n(declare helper later)\n(defn later [] (helper))` — occurrences of `app/helper` are exactly one (the call), none on the declare line; occurrences of `app/later` include the declare line (the file defines it; today's behavior). Adjust `test_declare_indexes_each_name` if its `app/helper` assertion now needs the call site.
  - `test_lint_as_declare_declares_each_name`: with `ExtractConfig { lint_as: {"conch/programs" → Declare} }` and `(ns t (:require [conch :refer [programs]]))\n(programs rm mv)\n(defn go [] (rm "-rf"))`: symbols `t/rm` and `t/mv` of kind `Declare`; occurrences hold `conch/programs` at the head, `t/rm` at the call only, nothing for `t/mv`.
  - `test_lint_as_defprotocol_indexes_methods_and_callers` (pins item 4): `lint_as {"pot/defprotocol+" → Defprotocol}`, `(pot/defprotocol+ Node (tag [_]) (sexpr [_]))`, a `(defrecord R [] Node (tag [_] :r))` and `(defn f [n] (tag n))` in the same file: symbols `t/Node`, `t/tag`, `t/sexpr`; occurrences of `t/tag` are the impl head and the call, none inside the protocol body.
  Run: `cargo test --test test_extractor declare && cargo test --test test_extractor lint_as` — Expected: FAIL.

- [x] **Step 2: Implement**
  - `from_def_symbol`: `"declare" => DefKind::Declare`. Update `src/settings.rs`'s `merge` test with a `clojure.core/declare` target that is kept.
  - `process_top_level_list`: replace the `first_text == "declare"` branch by routing `Some(DefKind::Declare)` from the kind resolution to `extract_declare`; everything else through `extract_def` as now.
  - `OccurrenceCtx.declared: HashSet<&str>` filled in `extract_analysis_tree` from the retained `Declare` symbols; empty in `collect_edn_ns_map` and the test-only constructor near line 1751.
  - `walk_def_form`: a `Declare` arm before the `binds_vector` logic — for each `sym_lit` in `children[1..]` whose name is not in `ctx.declared`, `record_occurrence`; then return.
  - `CACHE_FORMAT_VERSION` → 21.

- [x] **Step 3: Fixture and e2e**
  `lint_as_project/.clj-kondo/config.edn` gains `app.macros/programs clojure.core/declare`; `core.clj` requires `programs` and has `(programs rm mv)` and `(defn clean [] (rm "-rf"))`. `test_e2e_lint_as_declare_names_are_renamable`: definition from the `rm` call lands on the `programs` line; `rename` `rm` → `remove` edits the declaration token and the call; references list the declaration once. Keep `test_e2e_lint_as_config_live_reload` green (it edits the same config file — read it first).

- [x] **Step 4: Run**
  Run: `cargo test --test test_extractor && cargo test --test test_e2e lint_as && cargo test --test test_e2e declare` — Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "extractor: honor :lint-as to clojure.core/declare and stop double-counting declare-only names"`

> Deviation: `walk_def_form`'s `Declare` arm no longer records the `declare` head itself, as for every other core def head (definition and hover on `declare` resolve through the bare word). `jar_cache` documents version 21. `test_declare_indexes_each_name` needed no change.

### Task 5: Gates

- [x] **Step 1:** `bb check` — Expected: green (fmt, clippy `-D warnings`, all tests).
- [x] **Step 2:** `bb e2e` — Expected: PASS.
- [x] **Step 3:** `bb e2e-pulse` — Expected: PASS (rename edit shapes are client-visible).
- [x] **Step 4:** `bb e2e-calva` — Expected: PASS (definition and location shapes: the CLAUDE.md gate table asks for it).
- [x] **Step 5:** `bb bench` and `bb soak` (default clj-kondo corpus) — the index and extractor changed. Bench is not a gate: compare against the tables in `docs/MEMORY.md` and note any drift there. Soak must pass.
- [x] **Step 6:** `bb compare clj-kondo > .tmp/compare-after.log` — compare the six rows against the table in the Design section; every `var-def/defmulti`, `var-def/programs` and `var-def/declare` divergence gone, `deftype`/`defrecord` in-file `T.` sites agreeing, `->T` extras `known`. Any surprise is a finding to fix or to backlog with a dated line, not to allowlist silently.

### Task 6: Docs, backlog and roadmap

**Files:**
- Modify: `docs/FEATURES.md`, `docs/SETTINGS.md`, `docs/MEMORY.md`, `CLAUDE.md`, `docs/ROADMAP.md`, `docs/backlog/*.md`

- [x] **Step 1: FEATURES.md** — under Rename / Find references: renaming a `deftype`/`defrecord` rewrites its `->T`, `map->T` and `T.` calls, and references list them; a `defmulti` is listed with its defmethods. In the ns/`declare` line: a `:lint-as … clojure.core/declare` macro declares its names.
- [x] **Step 2: SETTINGS.md** — the `:lint-as` bullet: the accepted targets are the `def` family plus `defprotocol`, `defmulti`, `defrecord`, `deftype` and `declare`; "a target that names no def-family form drops the macro" stays.
- [x] **Step 3: CLAUDE.md invariants** — three additions, in the style of the existing ones: (a) a `Defmethod` symbol is for the outline alone and never takes a slot (`insert_file` / `insert_lib_file`); (b) constructor calls are sites of the type through `references::constructor_forms` + `matching_occurrences`, ranges narrowed to the name, `resolve_fqn_at` canonicalizes `->T`/`map->T`/`T.` to the type, the cross-namespace `:import` + `T.` case is deliberately out; (c) `declare` walks as a def form: a declare-only name is its `Declare` symbol alone, a name the file defines has the declare line as an occurrence, and `:lint-as` to `clojure.core/declare` is honored. Bump note: `CACHE_FORMAT_VERSION` 21.
- [x] **Step 4: MEMORY.md** — a new dated compare table row set (or the delta rows) from `.tmp/compare-after.log`, following the existing table's shape.
- [x] **Step 5: README** — read it; the constructor-rename and defmulti behavior are feature detail (FEATURES.md), so expect no change, but confirm nothing in it now misstates rename.
- [x] **Step 6: Backlog and roadmap** — close the three backlog entries (status `done`, date, one line on the cause, per the /backlog skill's close convention); add the `defprotocol+` corpus sites (node.clj:38/122, indent.clj:27/29, parser/core.clj:174/193, hooks_api.clj:91, meta.clj:9, reader_macro.clj:11/42/69) to `2026-09-17-import-vars-re-exports-are-not-definitions.md` as evidence. In `docs/ROADMAP.md`, tick the three backlog lines with this plan linked.
  > Deviation: following the repo's precedent (#46, #47), the three entries moved to `docs/archive/` with a `fixed` status line, their Backlog lines were removed, and the milestone item added at start was ticked `done`.
- [x] **Step 7: Commit**
  `git commit -m "docs: record defmulti, constructor and lint-as declare sites"`
