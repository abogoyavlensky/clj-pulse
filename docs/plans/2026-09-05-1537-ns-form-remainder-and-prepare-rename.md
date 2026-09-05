# ns-Form Remainder and prepareRename Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the ns-form parsing holes (`:as-alias`, `:refer-clojure`, `:rename`, prefix lists, `declare`) so navigation and lints stop giving wrong answers, and add `textDocument/prepareRename` so editors show a proper rename box or a clear rejection (ROADMAP Milestone 1, first two items).

**Tech Stack:** Rust, tower-lsp 0.20, tree-sitter-clojure. Tests: extractor unit tests (`tests/test_extractor.rs`, `src/index/extractor.rs` `mod tests`), diagnostics unit tests (`src/diagnostics.rs`), e2e harness (`tests/test_e2e.rs`).

---

## Design

### What is wrong today

`parse_require_vector` (`src/index/extractor.rs`, around line 620) understands `:as`, `:refer [names]`, and `:refer :all`. Everything else in a libspec is skipped:

- `:as-alias` is invisible, so `::alias/kw` never resolves and completion never offers the alias. The clean-ns parser (`handlers/code_action.rs`, `parse_libspec`) already treats it as an unknown option and never flags the require, so the only defect is missing navigation.
- `(:refer-clojure :rename {map cmap})` leaves `cmap` unresolvable. `:exclude` changes nothing for definition, because bare names resolve refers first, then the current namespace, then core (`handlers/mod.rs:52`); it does change what core completion should offer and what the occurrence walker should attribute to `clojure.core`.
- `[a.b :refer [x] :rename {x y}]` binds `x` instead of `y`.
- Prefix lists `(clojure [set :as s] string)` record nothing (`process_require_spec`, the `read_cond_lit` comment).
- `(declare foo bar)` is not a def form (`DefKind::from_def_symbol` has no entry), so a file that declares and never defines a name, or defines it through a macro, has nothing to navigate to.

### NsMeta changes

Two new fields, both `#[serde(default)]`:

- `as_aliases: Vec<String>`: the namespaces bound only through `:as-alias`. Their aliases also go into `aliases`, so keyword resolution (`keyword_fqn`) and the occurrence walker's qualified-symbol path work unchanged. They are not pushed into `requires`; an `:as-alias` namespace is not loaded, so a qualified var usage of it stays flagged by unresolved-namespace when the prefix is the full namespace name. (A usage through the alias resolves through `aliases`, matching what clj-kondo accepts.)
- `core_excludes: Vec<String>`: names from `(:refer-clojure :exclude [...])`.

`:refer-clojure :rename` needs no field: each renamed name goes into `refers` as `clojure.core/<original>`.

`JarCacheEntry` format: bump `CACHE_FORMAT_VERSION` (`src/index/jar_cache.rs:20`, currently 12) once, in the first task that changes `NsMeta`.

### Extractor changes

- `parse_require_vector` gains `:as-alias`, and `:rename` (a `map_lit` of `sym_lit` pairs: after the refer vector is parsed, each `from to` pair replaces the `from` refer entry with `to`, keeping the fqn).
- `extract_ns` gains a `":refer-clojure"` arm: `:exclude [..]` fills `core_excludes`; `:rename {..}` fills `refers`; `:only` is ignored.
- `process_require_spec` gains prefix-list expansion: a `list_lit` whose first child is a `sym_lit` and whose remaining children are `sym_lit` or `vec_lit` is a prefix list. Each child becomes a libspec with the prefix joined by a dot: a bare `string` becomes `clojure.string`; `[set :as s]` becomes `[clojure.set :as s]`. Implement by building the joined namespace name and calling the existing vector/symbol handling with it, not by rewriting source text. Reader conditionals inside prefix lists are out of scope.
- The occurrence walker's core fallback (`record_occurrence`, around line 1858) skips names in `core_excludes`, so an excluded name falls through to the current namespace.
- `declare`: a new `DefKind::Declare`. The top-level dispatch (`extract_top_level_form`, where `str_to_defkind` is consulted) handles `declare` before it: one `Symbol` per `sym_lit` child, `params` empty, `doc` none, `private` from `^:private` on the name via `has_private_meta`, `range` the whole form, `name_range` the name. After the file is extracted, drop every `Declare` symbol whose fqn another symbol in the same file also defines, so the real def wins in the index and the outline shows one entry. `def_names` (extractor line 301) is built from `symbols`, so declared names already resolve as current-namespace occurrences; the test in Task 4 pins that references and rename reach declare sites.

### Completion and lints

- Core completion (`handlers/completion.rs`, the `core_symbols` loop around line 190) skips `core_excludes`.
- Alias completion already reads `aliases`, so `:as-alias` names are offered without change.
- `unused_requires` / `parse_libspec` in `code_action.rs`: `:as-alias` and `:rename` stay *unmodeled* options, which `has_unknown_opt` already makes "never flagged, never pruned". That is deliberate, not an omission: the roadmap says an `:as-alias` require never counts as unused (the namespace is not loaded, so keeping it costs nothing), and clean-ns prunes individual `:refer` names (`clean_ns_edits`, around line 775), which cannot be done safely under `:rename` without rewriting both the `:refer` vector and the `:rename` map together. Tests pin both behaviors. Prefix lists stay skipped.
- `is_lintable_private_kind` does not include `Declare`, so a private declare is never reported unused.
- `handlers/symbols.rs` maps `Declare` to `SymbolKind::VARIABLE`.

### prepareRename

`handlers::references::rename` (line 142) runs a fixed sequence: valid new name, editable origin, local path (with capture and `:keys` rejection), then the fqn path with keyword and library rejections. `prepareRename` must agree with it exactly, so the sequence is extracted:

```rust
pub enum RenameTarget {
    Local { word: String, refs: LocalRefs },
    Global { fqn: String, sym: Symbol },
}

/// Everything `rename` checks before building edits, minus the new-name checks.
pub fn rename_target(index: &Index, documents: &DocumentStore, uri: &Url, pos: Position)
    -> Result<RenameTarget>;
```

`rename` calls it, then validates the new name and the capture rule (both need `new_name`), then builds edits as today. `prepare_rename` calls it and returns `PrepareRenameResponse::Range(range)`, where the range is the cursor token's range: for a local, the declaration or usage range containing the cursor; for a global, the occurrence `name_range` at the cursor (the same range the rename edit would touch, so the editor's rename box selects exactly what changes). Errors keep the existing messages and surface as `invalid_params`, as `rename` does, so the editor shows "cannot rename library or built-in symbol …" in place.

Capability: `rename_provider: Some(OneOf::Right(RenameOptions { prepare_provider: Some(true), work_done_progress_options: Default::default() }))`.

### Testing

- Extractor unit tests for each libspec shape, in `tests/test_extractor.rs` next to `test_ns_refer_all_and_use_recorded`, using new snippet files under `tests/fixtures/snippets/`.
- Diagnostics unit tests in `src/diagnostics.rs` `mod tests` for the `:as-alias` used/unused cases.
- e2e tests in `tests/test_e2e.rs` for `::alias/kw` navigation through `:as-alias`, `cmap` navigation through `:refer-clojure :rename`, definition through a prefix list, declare-only definition, and prepareRename on a local, a global, a library symbol, a keyword, and a `:keys` binding.
- `bb e2e-nvim` after the capability change (client-visible).

## File Structure

Modify:

- `src/index/mod.rs`: `NsMeta.as_aliases`, `NsMeta.core_excludes`, `DefKind::Declare`.
- `src/index/jar_cache.rs`: `CACHE_FORMAT_VERSION` 12 → 13.
- `src/index/extractor.rs`: libspec options, `:refer-clojure`, prefix lists, `declare`, core-exclude fallback, declare de-duplication.
- `src/handlers/completion.rs`: core exclusions.
- `src/handlers/code_action.rs`: `:as-alias` and `:rename` in `parse_libspec` / `libspec_unused`.
- `src/handlers/symbols.rs`: `Declare` mapping.
- `src/handlers/references.rs`: `RenameTarget`, `rename_target`, `prepare_rename`.
- `src/server.rs`: `prepare_rename` method, `RenameOptions` capability.
- `tests/test_extractor.rs`, `src/diagnostics.rs` tests, `tests/test_e2e.rs`.
- `tests/fixtures/snippets/ns_options.clj` (new), `tests/fixtures/simple_project/src/ns_options.clj` (new, for e2e).
- `README.md`, `AGENTS.md`, `docs/ROADMAP.md`: docs.

## Tasks

### Task 1: `:as-alias`

**Files:**
- Modify: `src/index/mod.rs`, `src/index/jar_cache.rs`, `src/index/extractor.rs`, `src/handlers/code_action.rs`, `src/diagnostics.rs`
- Test: `tests/test_extractor.rs`, `tests/fixtures/snippets/ns_options.clj`, `src/diagnostics.rs` tests

- [ ] **Step 1: Write the failing extractor test**
  Create `tests/fixtures/snippets/ns_options.clj` with an ns form using `[my.app.config :as-alias cfg]` and a body that reads `::cfg/port`. Add `test_ns_as_alias_recorded`: `aliases["cfg"] == "my.app.config"`, `as_aliases` contains it, `requires` does not, and the occurrences contain the keyword fqn `:my.app.config/port`.

- [ ] **Step 2: Run it to verify it fails**
  Run: `cargo test --test test_extractor test_ns_as_alias`
  Expected: FAIL (no `as_aliases` field, compile error).

- [ ] **Step 3: Implement**
  Add the field with `#[serde(default)]`, bump `CACHE_FORMAT_VERSION` to 13, parse `:as-alias` in `parse_require_vector` (insert into `aliases`, push to `as_aliases`, do not push to `requires`; note the `ns_meta.requires.push` at the top of the function must move after option parsing or be undone for this case).

- [ ] **Step 4: Pin the lint behavior**
  In `src/diagnostics.rs` `mod tests`, next to `no_flag_when_alias_used_only_in_keyword`: `no_flag_for_as_alias_used_in_keyword` and `no_flag_for_unused_as_alias`. Both expect no `unused-namespace` diagnostic. In `code_action.rs` tests, next to `clean_prunes_unused_refer_keeping_sibling`: clean-ns leaves an unused `:as-alias` libspec in place. These should pass already (unmodeled option); they exist so a later "model `:as-alias`" change has to face them.

- [ ] **Step 5: Confirm the unresolved-namespace side**
  A diagnostics test that `cfg/thing` (a var usage through an `:as-alias` alias) is not flagged as unresolved, since `resolves_prefix` reads `aliases`. Add it next to `no_flag_when_aliased`.

- [ ] **Step 6: Run the tests**
  Run: `cargo test --test test_extractor && cargo test --lib diagnostics`
  Expected: PASS.

- [ ] **Step 7: Commit**
  `git commit -m "Record :as-alias requires for keyword resolution and lints"`

### Task 2: `:refer-clojure` and `:rename`

**Files:**
- Modify: `src/index/mod.rs`, `src/index/extractor.rs`, `src/handlers/completion.rs`
- Test: `tests/test_extractor.rs`, `tests/test_completion.rs`

- [ ] **Step 1: Write the failing extractor tests**
  Extend `ns_options.clj` with `(:refer-clojure :exclude [update] :rename {map cmap})` and `[clojure.string :refer [join] :rename {join str-join}]`, a `(defn update [] …)`, and a body calling `(cmap inc [1])`, `(str-join "," [])`, and `(update)`. Tests: `refers["cmap"] == "clojure.core/map"`, `refers["str-join"] == "clojure.string/join"` with no `join` entry, `core_excludes` contains `update`, and the occurrence for the `(update)` call has fqn `<ns>/update`, not `clojure.core/update`.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor test_ns_refer_clojure`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  `core_excludes` field; `":refer-clojure"` arm in `extract_ns`; `:rename` handling in `parse_require_vector` applied after the refer vector; core fallback in `record_occurrence` consults `core_excludes`.

- [ ] **Step 4: Completion test**
  In `tests/test_completion.rs`, add a test that a file excluding `update` from core gets no `clojure.core` `update` item for prefix `upd` (the project's own `update` still appears). Implement by filtering the core loop in `handlers/completion.rs` on `core_excludes`.

- [ ] **Step 4b: Pin clean-ns under `:rename`**
  A `code_action.rs` test: `[clojure.string :refer [join split] :rename {join j}]` with only `j` used is neither flagged nor pruned by clean-ns (unmodeled option). The extractor still resolves `j` to `clojure.string/join`, which the Task 2 extractor test covers.

- [ ] **Step 5: Run the tests**
  Run: `cargo test --test test_extractor && cargo test --test test_completion`
  Expected: PASS.

- [ ] **Step 6: Commit**
  `git commit -m "Honor :refer-clojure :exclude/:rename and :refer :rename"`

### Task 3: Prefix-list requires

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write the failing test**
  Snippet with `(:require (clojure [set :as s] string))`. Assert `aliases["s"] == "clojure.set"` and `requires` contains `clojure.string` and `clojure.set`.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor test_ns_prefix_list`
  Expected: FAIL (nothing recorded).

- [ ] **Step 3: Implement**
  In `process_require_spec`, add a `"list_lit"` arm for prefix lists as described in the design. Update the doc comments that say prefix lists are unsupported (`process_require_spec`, `collect_use_namespaces`, and `diagnostics.rs` test `prefix_list_require_does_not_suppress` — read that test: it asserts the *lint* still flags `set/union` under a prefix list because the lint parser skips prefix lists; decide whether `resolves_prefix` should now see the expanded requires. It should: `NsMeta.requires` is what `resolves_prefix` reads, so the diagnostic disappears. Update that test to assert no flag and rename it.)

- [ ] **Step 4: Run the tests**
  Run: `cargo test --test test_extractor && cargo test --lib diagnostics`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -m "Expand prefix-list requires"`

### Task 4: `declare`

**Files:**
- Modify: `src/index/mod.rs`, `src/index/extractor.rs`, `src/handlers/symbols.rs`
- Test: `tests/test_extractor.rs`, `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing extractor tests**
  Snippet with `(declare helper ^:private hidden later)`, then `(defn later [] (helper))`. Tests: symbols contain `helper` (kind `Declare`, `private` false), `hidden` (`private` true), and exactly one `later` whose kind is `Defn`; the occurrence for `(helper)` has fqn `<ns>/helper`.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor test_declare`
  Expected: FAIL.

- [ ] **Step 3: Implement**
  `DefKind::Declare`; `extract_declare` called from the top-level dispatch; de-duplication pass at the end of `extract_analysis_with` (or wherever `symbols` is final) that removes `Declare` symbols shadowed by a same-fqn non-declare symbol; `symbols.rs` mapping. Fix every exhaustive `match` on `DefKind` the compiler reports.

- [ ] **Step 4: e2e test**
  Add `declared.clj` content to the new `tests/fixtures/simple_project/src/ns_options.clj` (or a dedicated file): `(declare only-declared)` with no def, plus `(declare defined-later)` and its `defn`. Tests: definition on a usage of `only-declared` lands on the declare line; definition on `defined-later` lands on the `defn`; references on `defined-later` include the declare site; rename of `defined-later` edits the declare site too.

- [ ] **Step 5: Run the tests**
  Run: `cargo test --test test_extractor && bb e2e`
  Expected: PASS.

- [ ] **Step 6: Commit**
  `git commit -m "Index declare forms as declarations"`

### Task 5: e2e for the ns-form work

**Files:**
- Modify: `tests/test_e2e.rs`, `tests/fixtures/simple_project/src/ns_options.clj`

- [ ] **Step 1: Add e2e tests**
  In the fixture file: an `:as-alias` require with a `::cfg/port` keyword whose definition exists as an Integrant key or keyword occurrence elsewhere in the fixture (check `integrant_project` for the pattern; a keyword's "definition" is its first occurrence or its `ig/init-key` defmethod). Tests: definition on `::cfg/port` resolves; completion of `cf` offers the `cfg` alias; definition on `cmap` lands on the curated core entry (hover shows `map`'s docstring); definition on a prefix-list alias usage works.

- [ ] **Step 2: Run**
  Run: `bb e2e`
  Expected: PASS.

- [ ] **Step 3: Commit**
  `git commit -m "Cover ns-form options end to end"`

### Task 6: prepareRename

**Files:**
- Modify: `src/handlers/references.rs`, `src/server.rs`
- Test: `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing e2e tests**
  Add a `prepare_rename(path, line, character)` helper to `LspClient` sending `textDocument/prepareRename`. Tests: a local in `locals.clj` returns a range equal to the token; a project global returns the token range; a library symbol, a keyword, and a `:keys` binding return errors whose messages match the existing rename rejections; `initialize` advertises `renameProvider.prepareProvider == true`.

- [ ] **Step 2: Run to verify failure**
  Run: `cargo test --test test_e2e prepare_rename`
  Expected: FAIL (method not found).

- [ ] **Step 3: Refactor `rename` around `rename_target`**
  Introduce `RenameTarget` and `rename_target` as in the design; `rename` keeps its behavior (all existing rename e2e tests stay green) and only reorders the new-name validation to after target resolution if needed. Keep the capture check in `rename`; it needs `new_name`.

- [ ] **Step 4: Implement `prepare_rename`**
  Handler returns `PrepareRenameResponse::Range`; `server.rs` wires `prepare_rename` with `invalid_params` error mapping like `rename`, and the capability becomes `RenameOptions { prepare_provider: Some(true), .. }`.

- [ ] **Step 5: Run the tests**
  Run: `bb check && bb e2e && bb e2e-nvim`
  Expected: PASS; the nvim run proves capability negotiation still works with the changed `renameProvider` shape.

- [ ] **Step 6: Commit**
  `git commit -m "Add textDocument/prepareRename"`

### Task 7: Docs and roadmap

**Files:**
- Modify: `README.md`, `AGENTS.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Update docs**
  README: the Rename bullet mentions the rename box validation (prepareRename); the "Clojure & project support" list notes `:as-alias`, `:refer-clojure`, `:rename`, prefix lists, and `declare`. AGENTS.md invariants: add a line on `as_aliases` (never in `requires`) and on declare de-duplication. ROADMAP: tick the two items and set their `Plan:` lines to `done`. Use /writing-clearly.

- [ ] **Step 2: Final verification and commit**
  Run: `bb check && bb e2e`
  Expected: PASS.
  `git commit -m "Document ns-form options and prepareRename"`
