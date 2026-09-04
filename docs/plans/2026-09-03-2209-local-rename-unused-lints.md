# Local Rename and Native Unused Lints Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `textDocument/rename` work on locals (params, `let`/`loop`/`for` bindings, destructured names) and add two native diagnostics, `unused-binding` and `unused-private-var`, that show when clj-kondo is disabled or absent.

**Tech Stack:** Rust, tower-lsp, tree-sitter-clojure. Tests: extractor unit tests (`src/index/extractor.rs` `mod tests`, `tests/test_extractor.rs`), diagnostics unit tests (`src/diagnostics.rs`), e2e harness (`tests/test_e2e.rs`).

---

## Design

### Why rename fails on locals today

`handlers::references::references` resolves a local structurally through `extractor::local_references_at` before falling back to the fqn path. `handlers::references::rename` never does; it goes straight to `resolve_fqn_at`, which deliberately returns `None` for bare words, so every local rename ends in "nothing to rename here". The e2e test `test_e2e_rename_rejects_local_shadowing_global` pins that as current behaviour.

### Local rename

- `rename` tries the local path first, mirroring `references`: if the cursor is on a local, the `WorkspaceEdit` is the declaration range plus every usage range, all in the one document. The fqn path is never reached, so a `[add]` param can never edit the global `add`.
- Order of checks in `rename`: validate the new name, reject non-project origins (unchanged), then the local path, then the existing fqn path.
- A binding declared inside a `:keys`/`:strs`/`:syms` vector is rejected with an error. Renaming `a` in `{:keys [a]}` would silently change the key being looked up. The error text tells the user to rewrite it as `{new-name :a}` first. `LocalRefs` gains a `destructured_key: bool` so the handler can tell.
- An `:or {a 1}` key already resolves to the same declaration through the scope walker and is renamed along with it. That is correct: the `:or` key must match the binding name.

### `catch` and `as->` become binding forms

Neither walker treats `(catch Exception e …)` or `(as-> x v …)` as binding. Today `e` and `v` are recorded as var usages of the current namespace, which pollutes references and, once the unused lint exists, would hide the very common unused `catch` binding. Both the occurrence walker (`walk_list`) and the scope walker (`walk_scope`) learn them:

- `(catch Class name body…)`: the head and `Class` are usages, `name` binds for `body…`.
- `(as-> expr name body…)`: the head and `expr` are usages, `name` binds for `body…`.

### `unused-binding`

The occurrence walker already keeps a scope stack of bound names and already decides, for every bare symbol, whether it hits a local. The stack changes from `Vec<HashSet<String>>` to a `Scope` type whose frames hold slots with the binding's name range and a `used` flag. `record_occurrence` marks the innermost matching slot used instead of just checking membership. When a frame pops, its lintable, unused, non-`_`-prefixed slots go to `scope.unused`. No third copy of the scope rules.

Slots are searched from the innermost end, so `(let [x 1 x (inc x)] x)` reports nothing: the RHS marks the first `x`, the body marks the second.

Not lintable (bound, but never reported):
- `fn` and `letfn` self-names.
- `defrecord`/`deftype` fields.
- Params of protocol or type method impls (`walk_method_impl`): the arity is fixed by the signature.
- Anything starting with `_`, plus `&` and `_` themselves (already skipped).

`:or` keys no longer create a binding slot. The real binding lives in `:keys`/`:as`/a map key elsewhere in the same pattern; a slot for the `:or` key would shadow it and produce a false positive.

Lintable: `let`-family bindings (including `for`/`doseq` `:let`), `defn`/`defmacro`/`defmethod`/`fn`/`letfn` params including destructured names and `:as`, `catch` and `as->` names, `with-open`, `dotimes`, `loop`. Params of `defn` are flagged, as clj-kondo does; the `_` prefix is the opt-out.

Public entry point: `extractor::extract_analysis_with(source, path, cfg) -> Result<Analysis>` where

```rust
pub struct Analysis {
    pub ns_meta: NsMeta,
    pub symbols: Vec<Symbol>,
    pub occurrences: Vec<Occurrence>,
    pub unused_bindings: Vec<LocalBinding>,
}
```

`extract_full_with` becomes a thin wrapper returning the first three, so no caller changes.

### `unused-private-var`

Private means `defn-` or `^:private` / `^{:private true}` on the name of a `def`, `defonce`, `defn`, `defmacro`, or `defmulti`. `deftest-` is not linted: test runners call private tests. The extractor records `Symbol.private: bool`; since `Symbol` is serialized in the JAR cache, `CACHE_FORMAT_VERSION` bumps to 12.

A private var is unused when the file's own live occurrences contain no usage of its fqn outside its own form range, so recursion does not count as use. A `#'foo` var-quote counts as use. `(declare foo)` counts as use, a known small gap. Cross-file `#'ns/foo` reaches from tests are not seen, which matches clj-kondo.

### Diagnostics wiring

- `diagnostics::compute(source, path, cfg: &ExtractConfig)`. It stays index-free; the config is needed so `:lint-as` defining macros bind their params like `defn`.
- `lint_and_publish_doc` gains an `index: &Index` parameter and reads `index.extract_config()`. Every caller (`relint_open_documents`, `lint_and_publish`, the `did_change` spawn, `spawn_kondo_reload`) threads it through.
- Both new codes join `KONDO_OWNED_CODES`. A successful clj-kondo run owns them; when kondo is disabled or absent the native set is published unchanged. One publish per pass, as before.

| Code | Severity | Tag | Range | Message |
|---|---|---|---|---|
| `unused-binding` | WARNING | UNNECESSARY | binding name | `Unused binding: x` |
| `unused-private-var` | WARNING | UNNECESSARY | def name | `Unused private var: x` |

Both carry `source: "clj-pulse"`.

### Testing strategy

- Extractor unit tests (`src/index/extractor.rs` `mod tests`) for scope behaviour: `:keys` detection, `catch`/`as->` scoping, and every unused-binding rule above.
- `tests/test_extractor.rs` for the private flag and for `catch`/`as->` no longer producing var occurrences.
- `src/diagnostics.rs` unit tests for both lints and the ownership merge.
- e2e: rename a `let` local, rename a param that shadows a global, `:keys` rejection, and one diagnostic test per lint. All run with kondo disabled (the harness default), plus one test proving a successful fake-kondo run drops the native `unused-binding`.

## File Structure

- Modify: `src/index/extractor.rs`. `LocalRefs.destructured_key`; `catch`/`as->` in both walkers; `Scope`/`Frame`/`LocalSlot` types replacing `Vec<HashSet<String>>`; `Analysis` + `extract_analysis_with`; private-meta detection in `extract_def`.
- Modify: `src/index/mod.rs`. `Symbol.private`.
- Modify: `src/index/jar_cache.rs`. Version 12.
- Modify: `src/handlers/references.rs`. Local path in `rename`.
- Modify: `src/diagnostics.rs`. Two new lints, `cfg` parameter, ownership list.
- Modify: `src/server.rs`. Thread `&Index` into the lint pass.
- Modify: `tests/test_extractor.rs`, `tests/test_e2e.rs`, `tests/fixtures/snippets/private_vars.clj` (new), `tests/fixtures/simple_project/src/locals.clj` (unchanged; used by e2e).
- Modify: `CLAUDE.md`, `ARCHITECTURE.md`, `docs/ROADMAP.md`.

Other `Symbol { … }` constructors that must gain `private: false`: grep `Symbol {` across `src/` and `tests/` (extractor has several, `jar_cache.rs`, `handlers/mod.rs` tests, `index/jdk.rs` if it builds symbols).

---

### Task 1: `LocalRefs.destructured_key`

**Files:**
- Modify: `src/index/extractor.rs` (`LocalRefs`, `local_references_at`, tests module)

- [x] **Step 1: Write the failing tests**
  In the extractor `mod tests`, next to `local_refs_let_declaration_and_usages`:
  - `local_refs_flags_keys_destructured`: `(defn f [{:keys [a]}] (inc a))`, cursor on the `a` in `(inc a)`. Expect `destructured_key == true` and one usage.
  - `local_refs_flags_strs_and_syms`: same with `:strs` and `:syms`.
  - `local_refs_plain_map_key_is_not_destructured_key`: `(let [{a :a} m] a)` → `false`.
  - `local_refs_or_key_is_a_usage`: `(let [{a :a :or {a 1}} m] a)`, cursor on the body `a`. Expect `destructured_key == false` and two usages: the `:or` key and the body.
  - `local_refs_vector_binding_is_not_destructured_key`: `(let [[a b] v] (+ a b))` → `false`.

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib local_refs_`
  Expected: compile error (`destructured_key` missing).

- [x] **Step 3: Implement**
  Add `pub destructured_key: bool` to `LocalRefs`. In `local_references_at`, after the declaration range is known, find the `sym_lit` whose name range equals it (a small recursive search over the tree, skipping `quoting_lit` like `collect_name_occurrences`) and check: parent is a `vec_lit`, grandparent is a `map_lit`, and the vector's `prev_named_sibling()` is a `kwd_lit` with text `:keys`, `:strs`, or `:syms`. Namespaced keys (`{:keys [foo/bar]}`) sit in the same vector, so the same check applies.

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib local_refs_`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -am "Flag :keys-destructured locals in LocalRefs"`

> Deviation: the `:keys`/`:strs`/`:syms` check matches the directive keyword's
> *name* part, so namespaced forms (`{:user/keys [a]}`, `{::keys [a]}`) are
> flagged too — they bind from the key the same way. Found by the codex review.

### Task 2: Local rename

**Files:**
- Modify: `src/handlers/references.rs`
- Test: `tests/test_e2e.rs`

- [x] **Step 1: Write the failing e2e tests**
  - Rewrite `test_e2e_rename_rejects_local_shadowing_global` as `test_e2e_rename_local_never_touches_shadowed_global`. Same setup (insert `(defn f2 [add] add)` at the end of `core.clj`, cursor on the param at character 11). Now assert `client.rename(...)` succeeds: `changes` has exactly one key ending in `/src/core.clj`, with exactly two edits, both on `last_line`, with start characters 10 and 15 and `newText` `"plus"`. Keep the references assertions.
  - `test_e2e_rename_local_in_let`: open `src/locals.clj`, cursor via `position_of(&locals, "base")`, rename to `"b0"`. Expect one file, three edits, every `newText` `"b0"`, and the edits cover lines 3, 4, 5 of the fixture (the binding, the `scaled` RHS, the body).
  - `test_e2e_rename_rejects_keys_destructured_local`: append `(defn f3 [{:keys [k]}] (inc k))` to `core.clj`, cursor on the `k` inside `(inc k)`, `request_expect_error("textDocument/rename", …)`; assert the message contains `destructured`.

- [x] **Step 2: Run tests to verify they fail**
  Run: `cargo test --test test_e2e test_e2e_rename_local && cargo test --test test_e2e test_e2e_rename_rejects_keys`
  Expected: FAIL. The first two get an error response ("nothing to rename here"); the third gets the wrong message.

- [x] **Step 3: Implement**
  In `rename`, after the project-origin guard, add the local path. Reuse the guards from `local_references` (keyword check, no `/` in the word) via a shared helper `local_refs_at(documents, uri, pos) -> Option<LocalRefs>` that `local_references` also calls. When it returns `Some(refs)`:
  - if `refs.destructured_key`, `bail!("cannot rename a :keys/:strs/:syms destructured binding '{}': rewrite it as {{new-name :{}}} first", word, word)`.
  - otherwise build `changes` with one entry for `uri` holding a `TextEdit` for `refs.declaration` and one per usage, all with `new_text = new_name`, and return the `WorkspaceEdit`.
  Update the doc comments on `rename` and `resolve_fqn_at` (point 2 of its list no longer needs to mention rename protection since locals are handled before it).

- [x] **Step 4: Run tests to verify they pass**
  Run: `cargo test --test test_e2e rename`
  Expected: PASS for every rename test, including the unchanged cross-file and library-rejection ones.

- [x] **Step 5: Commit**
  `git commit -am "Rename local bindings"`

> Deviation: the rejection message interpolates the *requested* new name into
> the suggested rewrite (`rewrite it as {kk :k} first`) instead of the literal
> placeholder `new-name`.

### Task 3: `catch` and `as->` bind in both walkers

**Files:**
- Modify: `src/index/extractor.rs` (`walk_list`, `walk_scope`, tests module)
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write the failing tests**
  - `tests/test_extractor.rs`: `test_catch_and_as_arrow_bind_locals`. Source `(ns x)\n(defn f [] (try (g) (catch Exception e (log e))))\n(defn h [y] (as-> y v (inc v)))`. Use `extract_full` and assert no occurrence has fqn `x/e` or `x/v`, while `x/g`, `x/log`, and `clojure.core/inc` are present, and `catch`/`as->` heads are recorded as `clojure.core/…` or `x/…` (whichever `record_occurrence` yields today for `catch`; assert only that no `x/e`/`x/v` exists).
  - Extractor `mod tests`, using the existing `local_names` helper: `catch_binding_visible_in_its_body` (position inside `(log e)` lists `e`; position inside `(g)` does not) and `as_arrow_name_visible_in_body`.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --test test_extractor test_catch_and_as_arrow && cargo test --lib catch_binding && cargo test --lib as_arrow`
  Expected: FAIL (`x/e` present; `e` not in scope).

- [ ] **Step 3: Implement**
  `walk_list`: add arms `Some("catch")` and `Some("as->")`. `catch`: record the head, walk `children[1]`, push a frame binding `children[2]` (when a `sym_lit`), walk `children[3..]`, pop. `as->`: record the head, walk `children[1]`, bind `children[2]`, walk `children[3..]`, pop. `walk_scope`: the same shapes, steering descent: a cursor inside `children[1]` sees no new binding; inside `children[3..]` it sees the name, using `collect_binding_targets` on the name node.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --test test_extractor && cargo test --lib`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -am "Treat catch and as-> as binding forms"`

### Task 4: Scope frames with usage tracking and `unused_bindings`

**Files:**
- Modify: `src/index/extractor.rs`

- [ ] **Step 1: Write the failing tests**
  In the extractor `mod tests`, add a helper `unused(src) -> Vec<(String, u32)>` returning `(name, start line)` from `extract_analysis_with(src, Path::new("t.clj"), &ExtractConfig::default()).unwrap().unused_bindings`. Cases, one test each or grouped sensibly:
  - `(let [a 1 b 2] a)` → `[("b", 0)]`.
  - `(defn f [x y] x)` → `y`.
  - `(defn f [_y] 1)` → none; `(defn f [_] 1)` → none.
  - `(let [x 1 x (inc x)] x)` → none.
  - `(defn f [{:keys [a b] :as m}] a)` → `b`, `m`.
  - `(let [{a :a :or {a 1}} m] 1)` → exactly one `a`, at the `{a :a}` position (the `:or` key never reports).
  - `(fn me [x] x)` → none (self-name exempt); `(fn me [x] 1)` → `x` only.
  - `(letfn [(g [p] 1)] (g 1))` → `p`; `(letfn [(g [p] p)] 1)` → none (letfn name exempt).
  - `(defrecord R [a b] P (m [this q] a))` → none (fields and method params exempt).
  - `(try (f) (catch Exception e nil))` → `e`.
  - `(loop [i 0] (when (< i 3) (recur (inc i))))` → none.
  - `(for [x xs :let [y (inc x)]] x)` → `y`.
  - `(defmethod m :k [_ arg] 1)` → `arg`.
  - `(defmacro w [x] \`(let [v# ~x] v#))` → none (syntax-quote gensyms are matched by name).
  - `(as-> 1 v)` → `v`; `(as-> 1 v (inc v))` → none.
  - Multi-arity `(defn f ([a] a) ([a b] a))` → one `b`.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib unused_`
  Expected: compile error (`extract_analysis_with` missing).

- [ ] **Step 3: Implement the scope type**
  Replace `Vec<HashSet<String>>` in the occurrence walker with:

  ```rust
  struct LocalSlot { name: String, name_range: Range, used: bool, lintable: bool }
  struct Scope { frames: Vec<Vec<LocalSlot>>, unused: Vec<LocalBinding> }
  ```

  Methods: `push()`, `pop()` (drains the frame; slots that are `lintable && !used && !name.starts_with('_')` become `LocalBinding`s in `unused`), `bind(name, range, lintable)`, `mark_used(&mut self, name) -> bool` (search frames from innermost, slots from last to first; mark and return true on the first hit), `contains(name)` where still needed. `record_occurrence` takes `&mut Scope` and calls `mark_used` where it used to check membership.
  `collect_binding_names` collects `Vec<LocalBinding>` (name + name range from `sym_name_node`) instead of a `HashSet`, keeps walking `:or` defaults and `{pattern :key}` keywords as usages, and stops inserting `:or` keys. Callers bind the collected list with `lintable = true`, except: `walk_def_form` record/type fields (`false`), `walk_method_impl` params (`false`), `walk_fn_form` and `walk_letfn_form` self-names (`false`).
  `walk_letfn_form` and `walk_fn_tail` params stay lintable.

- [ ] **Step 4: Add `Analysis` and `extract_analysis_with`**
  Move the body of `extract_full_with` into `extract_analysis_with`, returning `Analysis { ns_meta, symbols, occurrences, unused_bindings: scope.unused }` (every top-level walk ends with all frames popped; after the loop, assert-free: `scope.frames` is empty). `extract_full_with` calls it and returns the 3-tuple. `file_occurrences_with` is unchanged.

- [ ] **Step 5: Run tests to verify they pass**
  Run: `cargo test --lib && cargo test --test test_extractor`
  Expected: PASS, including every pre-existing occurrence test (the refactor must not change occurrences).

- [ ] **Step 6: Commit**
  `git commit -am "Track local usage in scope frames and expose unused bindings"`

### Task 5: `unused-binding` diagnostic

**Files:**
- Modify: `src/diagnostics.rs`, `src/server.rs`
- Test: `src/diagnostics.rs` tests, `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing tests**
  - `src/diagnostics.rs`: change the `diags` helper to `compute(source, Path::new("test.clj"), &ExtractConfig::default())`. Add `flags_unused_let_binding` (`(ns a)\n(defn f [x]\n  (let [y 1] x))\n` → one `unused-binding`, WARNING, source clj-pulse, tags `[UNNECESSARY]`, message contains `y`, range on line 2 spanning 1 char), `no_flag_for_used_binding`, `no_flag_for_underscore_binding`, and `lint_as_defn_params_are_linted` (with a `:lint-as {"my/defthing" => Defn}` config, `(ns x (:require [my :refer [defthing]]))\n(defthing foo [p] 1)` flags `p`).
  - Update `successful_kondo_run_cedes_the_codes_it_owns` to include `unused-binding` and `unused-private-var` in the native list and assert both are dropped.
  - e2e `test_e2e_unused_binding_diagnostic`: write `src/scratch.clj` with `(ns simple.scratch)\n\n(defn run [x y]\n  (let [z 1]\n    x))\n`, `did_open`, `wait_for_diagnostics("/src/scratch.clj")`, assert exactly two `unused-binding` diagnostics naming `y` and `z`, severity 2, tags `[1]`.
  - e2e `test_e2e_kondo_run_drops_native_unused_binding`: copy the setup of the existing fake-kondo test (`start_with_kondo` on the `kondo_project` fixture, same wait), open a file containing an unused binding and no fake-kondo marker, and assert the published diagnostics contain no `unused-binding` (the fake returns zero findings with exit 0, which cedes ownership).

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib diagnostics`
  Expected: compile error (`compute` arity).

- [ ] **Step 3: Implement**
  - `compute(source, path, cfg)`: call `extractor::extract_analysis_with(source, path, cfg)` once (it replaces the existing `extractor::extract` call; use `analysis.ns_meta` for the unresolved-namespace filter). Map each `unused_bindings` entry to a diagnostic per the table in the design.
  - Add both codes to `KONDO_OWNED_CODES` (now 5) and extend its doc comment.
  - `src/server.rs`: `lint_and_publish_doc(client, documents, index: &Index, kondo_state, uri, version)`; compute `let cfg = index.extract_config();` and pass `&cfg`. Thread `&self.index` / a cloned `Arc<Index>` through `relint_open_documents`, `lint_and_publish`, the `did_change` spawn, and `spawn_kondo_reload`.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib diagnostics && cargo test --test test_e2e unused_binding`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -am "Add native unused-binding diagnostic"`

### Task 6: `Symbol.private`

**Files:**
- Modify: `src/index/mod.rs`, `src/index/extractor.rs`, `src/index/jar_cache.rs`, every `Symbol {` constructor
- Create: `tests/fixtures/snippets/private_vars.clj`
- Test: `tests/test_extractor.rs`

- [ ] **Step 1: Write the failing test**
  Fixture `private_vars.clj`:
  ```clojure
  (ns my.priv)
  (def ^:private secret 1)
  (defn ^{:private true :doc "d"} helper [] 1)
  (defn- old-style [] 2)
  (defonce ^:private state (atom nil))
  (defn public-fn [] 3)
  (def ^{:private false} not-private 4)
  ```
  `test_extracts_private_flag`: `secret`, `helper`, `old-style`, `state` have `private == true`; `public-fn`, `not-private` have `false`.

- [ ] **Step 2: Run test to verify it fails**
  Run: `cargo test --test test_extractor test_extracts_private_flag`
  Expected: compile error (no `private` field).

- [ ] **Step 3: Implement**
  - `Symbol` gains `#[serde(default)] pub private: bool`. Fix every constructor (`grep -rn "Symbol {" src tests`) with `private: false`, except `extract_def`.
  - In `extract_def`: `private = kind == DefKind::DefnPrivate || has_private_meta(name_node, source)`. `has_private_meta` walks `name_node`'s children of kind `meta_lit`/`old_meta_lit`; for each, take its named child: a `kwd_lit` with text `:private` → true; a `map_lit` whose pairs contain key text `:private` with value text `true` → true.
  - `CACHE_FORMAT_VERSION = 12` with the comment line `/// 12: \`Symbol.private\` (layout change).`

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test`
  Expected: PASS (all suites still compile).

- [ ] **Step 5: Commit**
  `git add tests/fixtures/snippets/private_vars.clj && git commit -am "Record private vars on Symbol"`

### Task 7: `unused-private-var` diagnostic

**Files:**
- Modify: `src/diagnostics.rs`
- Test: `src/diagnostics.rs` tests, `tests/test_e2e.rs`

- [ ] **Step 1: Write the failing tests**
  Unit tests filtering on the `unused-private-var` code:
  - `flags_unused_defn_private` (`(ns a)\n(defn- helper [] 1)\n` → one diagnostic, WARNING, tags UNNECESSARY, message contains `helper`, range on the name).
  - `flags_unused_private_meta_def` (`(def ^:private x 1)`).
  - `no_flag_when_private_var_is_called` (`(defn- h [] 1)\n(defn g [] (h))`).
  - `no_flag_when_private_var_is_var_quoted` (`(defn g [] #'h)`).
  - `recursion_only_is_still_unused` (`(defn- h [n] (h n))` → flagged).
  - `no_flag_for_public_var` and `no_flag_for_private_deftest` (`(ns a (:require [clojure.test :refer [deftest-]]))\n(deftest- t 1)` → none; check the extractor already maps `deftest-` to `Deftest`).
  - e2e `test_e2e_unused_private_var_diagnostic`: scratch file `(ns simple.scratch)\n\n(defn- helper [] 1)\n\n(defn run [] 2)\n` → exactly one `unused-private-var`, severity 2, tags `[1]`, message contains `helper`.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test --lib diagnostics`
  Expected: FAIL (no such code emitted).

- [ ] **Step 3: Implement**
  In `compute`, after the unused-binding block: for each symbol in `analysis.symbols` with `private` and kind in `{Def, Defonce, Defn, DefnPrivate, Defmacro, Defmulti}`, it is used when any occurrence in `analysis.occurrences` has `fqn == sym.fqn` and a `name_range` not inside `sym.range` (a small `range_within(inner, outer)` helper). Otherwise emit the diagnostic per the design table.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo test --lib diagnostics && cargo test --test test_e2e unused_private`
  Expected: PASS.

- [ ] **Step 5: Commit**
  `git commit -am "Add native unused-private-var diagnostic"`

### Task 8: Docs and full verification

**Files:**
- Modify: `CLAUDE.md`, `ARCHITECTURE.md`, `docs/ROADMAP.md`

- [ ] **Step 1: Update docs**
  - `CLAUDE.md` Invariants, diagnostics bullet: native lints are now `unresolved-namespace`, `unused-namespace`, `duplicate-require`, `unused-binding`, `unused-private-var`; a successful kondo run owns all five. Add one line: rename resolves locals structurally before the fqn path and rejects `:keys`-destructured bindings.
  - `ARCHITECTURE.md` Symbol Resolution paragraph: mention rename alongside find-references for locals, and that the occurrence walker's scope frames track usage to feed `unused-binding`.
  - `docs/ROADMAP.md` Phase 4: mark the native fallback bullet as partially done (`unused-binding`, `unused-private-var`); Phase 2 rename bullet: note locals are covered.

- [ ] **Step 2: Full check**
  Run: `bb check`
  Expected: fmt clean, clippy clean with `-D warnings`, all tests pass.

- [ ] **Step 3: End-to-end**
  Run: `bb e2e`
  Expected: PASS. Then `bb e2e-nvim` (rename is client-visible).
  Expected: PASS.

- [ ] **Step 4: Commit**
  `git commit -am "Document local rename and native unused lints"`
