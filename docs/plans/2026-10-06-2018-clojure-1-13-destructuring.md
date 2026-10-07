# Clojure 1.13 destructuring Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read Clojure 1.13's new map-destructuring syntax correctly — checked
keys, literal keys after `&`, key→val `:or`, the `:select`/`:all`/`:excess`/
`:missing`/`:defaults` directives and the `selector` macro — so locals, the
unused-binding lint, keyword references and every rename stay right on code
that uses it.

**Tech Stack:** Rust, tree-sitter-clojure, tower-lsp; tests in
`tests/test_extractor.rs`, the unit tests in `src/index/extractor.rs`,
`tests/test_e2e.rs`, `bb compare`.

---

## Design

### The syntax (1.13.0-alpha1 … alpha8)

Read from the release notes and `destmap*` / `selector` in the
`clojure-1.13.0-alpha8` tag of `clojure/clojure`.

| # | Form | Meaning |
|---|---|---|
| 1 | `{:keys! [a]}`, `:syms!`, `:strs!`, any namespace (`::keys!`, `:my.ns/keys!`) | Required keys: `req!` throws when the key is absent. Binds like `:keys`. |
| 2 | `{:keys [a & :b ::c]}`, `{:syms [a & 'b]}`, `{:strs [a & "b"]}` | Before `&`: binding symbols. After `&`: literal keys (taken verbatim — the directive's namespace does not apply), never bound; checked under a `!` directive, otherwise documentation. A symbol after `&` is an error. |
| 3 | `:or {:a 1}` | `:or` accepts key→val as well as name→val. |
| 4 | `:defaults d`, `:select s`, `:all a`, `:excess e`, `:missing m` | Each binds one symbol to a map. Nested map patterns take part implicitly (gensyms). |
| 5 | `(selector {…})` (`clojure.core/selector`) | Takes a destructuring map, returns a fn. **Binds nothing**; directive names are ignored. `:or` values are expressions evaluated where the `selector` form is. |
| 6 | `req!`, `some-vals`, `transform-keys`, `tap->`, `merge-deep`, `merge-deep-with`, `selector` | New core vars — **out of scope** (a 1.13 project indexes the Clojure JAR, so they resolve from the library index). |

### What already works and what is wrong today

Both walkers treat "keyword key, symbol value" in a map pattern as a binding
(`collect_binding_names`, `collect_binding_targets`), so row 4 already binds,
lints and renames. Row 1 binds too, but four places match the directive by
the literal names `keys|strs|syms` and miss the `!` forms:

- `is_destructured_key` (`src/index/extractor.rs`, ~line 4006): a `:keys!`
  local is *renameable*, which silently changes the key read. **Bug.**
- `collect_qualified` (~line 97): `{:keys! [foo/bar]}` reports `foo` as an
  unresolved namespace (native lint; add-require offers a require).
- `record_destructuring_keys` (~line 2806): `{::keys! [a]}` is no occurrence
  of `:my.ns/a`, so keyword references and rename skip it, and
  `alias_sites_tree` (which tells a `{:keys [h/x]}` entry by the keyword
  occurrence starting there) would rewrite `h` in `{:keys! [h/x]}`.
- `destructured_entry_name_at_tree` goes through `is_destructured_key`, so it
  follows.

Row 2: the vector after a directive goes through the generic `vec_lit` arm,
so `'b` after `&` binds a phantom `b` (both walkers; the native lint reports
it unused), a keyword after `&` is no occurrence (a keyword rename leaves
`::b` reading the old key), and under a namespaced directive the `&` symbol
itself is recorded as the keyword `:my.ns/&` by `record_destructuring_keys`.

Row 3: `:or` keys are skipped entirely; `::a` there is no occurrence.

Row 5: `(selector {:keys [a] :select s})` is walked as a call, so `a` is a
usage of whatever `a` is in scope. With a local `a` around it, references
list it and **a rename of that local rewrites the key the selector reads**.

### Approach

**1. One directive classifier.** In `src/index/extractor.rs`:

```rust
/// What a map pattern's key directive reads: `:keys`/`:keys!` keywords,
/// `:syms`/`:syms!` symbols, `:strs`/`:strs!` strings. Decided by the name
/// part, so `::keys!` and `:my.ns/syms` count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyRead { Keywords, Symbols, Strings }

fn key_directive(kw: Node, source: &str) -> Option<KeyRead>;

/// A directive vector's entries split at the first `&` symbol: the binding
/// symbols before it, the literal keys after it. The `&` itself is in
/// neither. Gaps are already filtered by `named_children`.
fn split_key_entries(vec: Node, source: &str) -> (Vec<Node>, Vec<Node>);
```

`key_directive` returns `None` for a non-`kwd_lit`. Every place that asked
"is this `:keys`/`:syms`/`:strs`" now asks `key_directive`:
`collect_qualified` (skip the vector when `key_directive(..).is_some()` and
the value is a `vec_lit` — this also stops `{:foo/keys [bar/x]}` reporting
`bar`, correct since the directive's namespace wins), `record_destructuring_keys`
(`== Some(KeyRead::Keywords)`), `is_destructured_key` (`is_some()`).

**2. Directive vectors bind only before `&`.** In the map arm of both
`collect_binding_names` (occurrence walker) and `collect_binding_targets`
(locals walker), a key directive whose value is a `vec_lit` binds each
entry of `split_key_entries(..).0` through the existing per-entry path (so
a schema marker or nested form behaves as today). The keys after `&` bind
nothing; in `collect_binding_names`, each `kwd_lit` among them is recorded
with `record_keyword_occurrence` under *every* directive — after `&` a key is
taken verbatim whatever the directive reads (`(if preamp? (tr bb) bb)` in
`destmap*`), so `{:syms [a & ::flag]}` names the key `:my.ns/flag` (`:b` →
`:b`, `::c` → `:my.ns/c`). `record_destructuring_keys`
iterates the before-`&` entries alone. Any other keyword key keeps today's
generic path, which is what makes row 4 bind.

**3. `:or` keyword keys are occurrences.** In `collect_binding_names`'s
`:or` arm, a `kwd_lit` key is passed to `record_keyword_occurrence`; values
are walked as today. Symbol keys stay non-sites. The locals walker needs
nothing (it skips `:or`; `pos_in_or_default` only covers values).

**4. A `selector` arm in both walkers.** Shared predicate:

```rust
/// Whether `children` is `(selector {…})` naming `clojure.core/selector`,
/// the second child being a `map_lit`. The head resolves there when it is
/// `selector` qualified to core (`head_resolves_to_core`); or bare, not a
/// local (`is_local`), and either `:refer`red to `clojure.core/selector` or
/// `cljs.core/selector` (`(:refer-clojure :rename {selector sel})` stores
/// `sel` that way in `NsMeta.refers`), or spelled `selector`, not
/// `:refer`red elsewhere and not in `core_excludes`.
fn is_core_selector(children: &[Node], ns_meta: &NsMeta, source: &str,
                    is_local: impl Fn(&str) -> bool) -> bool;
```

The occurrence walker passes a non-mutating `Scope::is_bound(name)` (new,
searches frames like `mark_used` without marking); the locals walker passes
`|n| out.iter().any(|b| b.name == n)` (the locals visible on the spine so
far). Checked in `walk_list` / `walk_scope` right after the `are` check,
before the core-form dispatch.

- *Occurrence walker:* push an `Occurrence` for the head under
  `clojure.core/selector` (the static core list lacks the name, so
  `record_occurrence` would put a bare head in the current namespace), run
  `collect_binding_names` on the map into a scratch `Vec` that is then
  dropped — it records keyword sites and walks `:or` values in the
  enclosing scope, and the names bind nothing and are never linted — and walk
  any further children normally.
- *Locals walker:* when `pos` is inside the map and *not*
  `pos_in_or_default`, `collect_binding_targets(map)` into `out` and
  return: the pattern's names shadow everything outside, their scope being
  the pattern alone. So an outer local's references skip the selector's
  `a` (it resolves to the pattern's own entry), a cursor on it self-resolves,
  and a rename started there is refused for a `:keys` entry
  (`is_destructured_key`) and harmlessly local otherwise. Elsewhere,
  descend generically (an `:or` value sees the enclosing scope).

### Decisions

- **No dialect gate.** The syntax is unambiguous; older Clojure throws on it
  rather than meaning something else. ClojureScript has no `selector`, and a
  `.cljs` file is unlikely to call one with a literal map.
- **A file defining its own `selector`** without excluding the core one is
  read as core when called with a literal map. Clojure 1.13 warns on that
  shadowing; rare enough to accept. Noted in AGENTS.md.
- **No version detection, no completion/hover for directives, no lints for
  `:or`/`:defaults` errors** — clj-kondo reports those (#2874, #2924, #2925
  closed). New core vars are out of scope (row 6).
- **No change to the rename refusal message** — ":keys/:strs/:syms
  destructured binding" still describes a `:keys!` entry.
- `CACHE_FORMAT_VERSION` 22 → 23 (occurrence output changes). The
  `feat/ns-for-new-files` branch does not touch it today; if it lands first
  with a bump, take the next number.
- `bb compare` cannot judge the new keyword sites (clj-kondo #2942 is open)
  and the pinned corpora predate 1.13; it is run to prove no regression.

## File Structure

| File | Change |
|---|---|
| `src/index/extractor.rs` | `KeyRead`, `key_directive`, `split_key_entries`, `is_core_selector`, `Scope::is_bound`; edits to `collect_qualified`, `collect_binding_names`, `record_destructuring_keys`, `is_destructured_key`, `collect_binding_targets`, `walk_list`, `walk_scope`; unit tests (unused lint, `local_references_at`) |
| `src/index/jar_cache.rs` | `CACHE_FORMAT_VERSION` 23 |
| `tests/test_extractor.rs` | occurrence and qualified-usage tests |
| `tests/test_e2e.rs` | one e2e test over rename/references |
| `docs/FEATURES.md`, `AGENTS.md`, `docs/ROADMAP.md` | docs and roadmap |

## Tasks

### Task 0: Branch and ROADMAP entry

Work happens on `feat/clojure-1.13-destructuring` (worktree
`../clj-pulse-1.13`, branched from master `50c019d`).

- [x] **Step 1:** In `docs/ROADMAP.md`, append to the end of Milestone 1
  (after the `are` item, before `## Milestone 2`):
  ```
  - [ ] **Clojure 1.13 destructuring.** Support Clojure 1.13 destructuring
        features, including required keys, :select, :all, :excess, :missing,
        and :defaults (plus literal keys after `&`, key→val `:or`, and the
        `selector` macro).
    Plan: [2026-10-06-2018-clojure-1-13-destructuring.md](plans/2026-10-06-2018-clojure-1-13-destructuring.md) — in progress
  ```
- [x] **Step 2:** `git add docs/ROADMAP.md docs/plans/2026-10-06-2018-clojure-1-13-destructuring.md && git commit -m "plan: Clojure 1.13 destructuring"`

### Task 1: Characterize the name directives (row 4)

These pass today; they pin the behavior the later tasks must keep.

**Files:** `src/index/extractor.rs` (unit tests module, next to
`unused_destructured_names_reported` and `local_refs_*`)

- [x] **Step 1:** Unused lint: `unused_names("(defn f [{:keys [a] :select s :all al :excess e :missing m :or {a 1} :defaults d}] a)")`
  reports exactly `s`, `al`, `e`, `m`, `d`; with every name used in the body,
  nothing.
- [x] **Step 2:** `local_references_at` from a body usage of `s` in
  `(defn f [{:keys [a] :select s}] (g s))` finds the `s` after `:select` as
  the declaration, `destructured_key` false (renameable).
- [x] **Step 3:** `locals_in_scope_at` in the body of
  `(let [{:excess e :missing m} x] |)` contains `e` and `m`.
- [x] **Step 4:** `cargo test --lib extractor` — PASS.
- [x] **Step 5:** `git commit -m "test: pin 1.13 name directives as bindings"`

> Deviation: no separate codex review for this test-only task; it is reviewed with Task 2's commit.

### Task 2: Checked-key directives and the `&` tail (rows 1–2)

**Files:** `src/index/extractor.rs`, `tests/test_extractor.rs`

- [x] **Step 1: Failing tests.**
  - `tests/test_extractor.rs`, next to
    `test_namespaced_keys_entries_are_keyword_occurrences`: in
    `(ns my.ns (:require [other.lib :as o]))` with
    `(defn f [{::keys! [a]}] a)`, `(defn g [{::o/keys! [b]}] b)`,
    `(defn h [{:keys! [other.lib/c]}] c)`, `(defn i [{::keys [x & ::p :q]}] x)`,
    `(defn j [{:syms [y & ::r]}] y)`: occurrences `:my.ns/a`, `:other.lib/b`,
    `:other.lib/c`, `:my.ns/p` (range = the `::p` token), `:q`, `:my.ns/r`
    — one each — and no occurrence whose fqn ends in `/&`.
  - Same file, next to `test_qualified_usages_collects_and_skips_quotes`:
    `qualified_usages` of `(defn f [{:keys! [foo/bar]} {:foo/keys [baz/x]}] [bar x])`
    is empty.
  - Unit tests: `unused_names("(defn f [{:syms [a & 'b] :strs [c & \"d\"] :keys [e & :f]}] [a c e])")`
    is empty, and `locals_in_scope_at` in that body has no `b`, `d` or `f`;
    `local_references_at` on a body usage of `a` in
    `(defn f [{:keys! [a]}] (inc a))` has `destructured_key: true`, and the
    same for `::keys!`, `:syms!`, `:strs!` (extend
    `local_refs_flags_strs_and_syms` / `local_refs_flags_namespaced_keys_directive`).
  - Unit test: `alias_sites_tree` for alias `o` in
    `(ns x (:require [other.lib :as o])) (defn f [{:keys! [o/x]}] (o/g x))`
    lists the `:as` symbol and `o/g`, not the entry.
- [x] **Step 2:** `cargo test --lib extractor && cargo test --test test_extractor` — the new tests FAIL.
- [x] **Step 3: Implement** `KeyRead`, `key_directive`, `split_key_entries`
  (signatures in the Design) and use them in `collect_qualified`,
  `record_destructuring_keys`, `is_destructured_key`,
  `collect_binding_names` and `collect_binding_targets` as the Design's
  points 1–2 say. Update the doc comments that list `:keys/:strs/:syms`.
- [x] **Step 4:** Both test commands — PASS, including every existing test.
- [x] **Step 5:** `git commit -m "extractor: checked key directives and literal keys after &"`

> Deviation: the `alias_sites_tree` test lives in `tests/test_extractor.rs`'s `alias_sites` module, where its siblings are. The vector arms' per-item loops became `collect_binding_names_seq` / `collect_binding_targets_seq`, so the entries before `&` keep the `:-` schema-marker rule.

### Task 3: `:or` keyword keys are sites (row 3)

**Files:** `src/index/extractor.rs`, `tests/test_extractor.rs`

- [x] **Step 1: Failing test** in `tests/test_extractor.rs`: in
  `(ns my.ns)\n(defn f [{::keys [a] :keys [b] :or {::a 1 b 2}}] [a b])` the
  `::a` key is an occurrence of `:my.ns/a` with the token's range (two in
  all: the `::keys` entry and the `:or` key), and `test_occurrence_destructuring_or_defaults_are_usages`
  still passes.
- [x] **Step 2:** `cargo test --test test_extractor` — FAIL.
- [x] **Step 3:** In `collect_binding_names`'s `:or` arm, record a `kwd_lit`
  key with `record_keyword_occurrence`.
- [x] **Step 4:** `cargo test --test test_extractor` — PASS.
- [x] **Step 5:** `git commit -m "extractor: keyword keys of :or are keyword sites"`

> Deviation: this five-line change is codex-reviewed together with Task 4.

### Task 4: `selector` binds nothing (row 5)

**Files:** `src/index/extractor.rs`, `tests/test_extractor.rs`

- [x] **Step 1: Failing tests.**
  - `tests/test_extractor.rs`: in
    `(ns my.ns)\n(def dflt 1)\n(defn f [a] (selector {::keys! [a] ::keys [z] :or {::z dflt} :select s}) a)`
    (every fixture here is valid 1.13: a selector names at least one of
    `:select`/`:all`/`:excess`/`:missing`, and every `:or` key is a key of
    the pattern): occurrences include `clojure.core/selector` (on the head),
    `:my.ns/a`, `:my.ns/z` (twice: the entry and the `:or` key) and
    `my.ns/dflt`; no occurrence named `my.ns/s` or `my.ns/selector`.
    `(:refer-clojure :rename {selector sel})` with `(sel {:keys [a] :select s})`
    is a selector too (head occurrence `clojure.core/selector`, no `a`
    usage). With `(:refer-clojure :exclude [selector])` in the ns form, or
    `(let [selector identity] (selector {:a x :select s}))`, the form is a
    plain call (the head records as a var/local usage, and `x` in the map is
    a usage).
  - Unit tests: `unused_names("(defn f [a] (selector {:keys [b] :select s}) a)")`
    is empty; `local_references_at` on the body `a` in
    `(defn f [a] (selector {:keys [a] :select s}) a)` has exactly one usage (the last
    `a`); from the selector's `a` it returns a declaration equal to that
    token's range and `destructured_key: true`; a cursor in an `:or` value,
    `(defn f [a] (selector {:keys [b] :or {b a} :select s}))` on the last `a`,
    resolves to the param.
- [x] **Step 2:** `cargo test --lib extractor && cargo test --test test_extractor` — FAIL.
- [x] **Step 3: Implement** `Scope::is_bound`, `is_core_selector` and the two
  arms (Design point 4). Comment in each arm that it mirrors the other.
- [x] **Step 4:** Both commands — PASS.
- [x] **Step 5:** `git commit -m "extractor: selector reads a pattern that binds nothing"`

> Deviation: the head occurrence is `core_ns(dialect)/selector` (`clojure.core/selector` in Clojure files), matching how `record_occurrence` places a bare core name. The plain-call fixture counts one `my.ns/x` occurrence, since `(def x 1)` is a definition rather than an occurrence.

### Task 5: Cache bump, e2e, gates

**Files:** `src/index/jar_cache.rs`, `tests/test_e2e.rs`

- [ ] **Step 1:** `CACHE_FORMAT_VERSION` 22 → 23.
- [ ] **Step 2: e2e test** `test_e2e_clojure_1_13_destructuring`, modelled
  on `test_e2e_rename_refuses_qualified_keys_destructuring`: write
  `src/destructuring_113.clj` into `setup_project()` with
  ```clojure
  (ns simple.destructuring-113)

  (defn f [{:keys! [k] :select s}] [k s])

  (defn g [{::keys [x & ::flag]}] x)

  (defn h [a] [(selector {:keys [a] :select s}) a])
  ```
  then, after `wait_for_log("Indexed")` and `did_open`:
  - rename on the body `k` → error whose message contains `destructured`;
  - references on `::flag` (include declaration) → one location, the
    `::flag` token;
  - rename of the param `a` of `h` to `b` → edits the param and the last
    `a` only, never the `a` inside `selector`.
- [ ] **Step 3:** `bb check` — green. `bb e2e` — green.
- [ ] **Step 4:** `bb e2e-pulse` (rename and references answers change) and
  `bb e2e-calva` (definition inside a `selector` pattern changes) — green.
- [ ] **Step 5:** `bb compare clj-kondo` — no new divergence against the
  table in `docs/MEMORY.md`; note the totals for the summary. `bb bench
  clj-kondo` (extractor change) — no regression against the MEMORY.md
  tables; it is not a gate, so note the numbers.
- [ ] **Step 6:** `git commit -m "test: e2e for 1.13 destructuring; cache format 23"`

### Task 6: Docs and roadmap close-out

**Files:** `docs/FEATURES.md`, `AGENTS.md` (`CLAUDE.md` is a symlink to it),
`docs/ROADMAP.md`, this plan

- [ ] **Step 1:** `docs/FEATURES.md`, Rename and Keyword rename bullets: the
  Clojure 1.13 forms — `:keys!`/`:syms!`/`:strs!` entries are refused like
  `:keys`; literal keys after `&` and keyword keys of `:or` are keyword
  sites; `:select`/`:all`/`:excess`/`:missing`/`:defaults` names are
  ordinary locals; a `selector` pattern binds nothing.
- [ ] **Step 2:** `AGENTS.md` invariants: a new bullet — key directives are
  classified by name part through `key_directive` (`!` forms included),
  entries bind only before `&`, the literal keys after it and `:or` keyword
  keys are keyword occurrences, `selector` (`is_core_selector`, a
  self-defined `selector` read as core when called with a literal map) binds
  nothing in the occurrence walker and shadows within its pattern in the
  locals walker; `CACHE_FORMAT_VERSION` 23 where 22 is quoted. README: the
  working rule asks for it in the same change — check it; it has no
  destructuring or Clojure-version claim today, so expect no edit and say so
  in the completion summary.
- [ ] **Step 3:** ROADMAP: tick the item, status `done`, add the compare
  result. Mark this plan's steps done and append a completion summary.
- [ ] **Step 4:** `bb check` — green.
- [ ] **Step 5:** `git commit -m "docs: close Clojure 1.13 destructuring"`
