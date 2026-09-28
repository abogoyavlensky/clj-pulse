# Quoted Keywords and Qualified `:keys` Locals Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close four `bb compare` backlog entries: keywords inside quoted data become occurrences, a qualified `{:keys [c/x]}` entry resolves as the local it binds, and the two entries the gaps fix already resolved get regression tests and are archived.

**Tech Stack:** Rust, tree-sitter-clojure, tower-lsp; tests in `tests/test_extractor.rs`, `tests/test_e2e.rs`, `tests/test_compare.rs`.

---

## Design

### What the compare re-run established (2026-09-28, current master)

`bb compare clj-kondo` gives the same numbers as the 2026-09-18 table in
`docs/MEMORY.md`: `keyword/keys` 5 agree of 5, `local/destructured` 507 of
507, `keyword/qualified` 142 agree, 93 diverge, 41 null of 276.

- **`{:ns/keys [a]}` renames the local** — already fixed by the gaps change
  (#43). The mechanism was a `;;` comment earlier in the outer `let` vector
  re-pairing the bindings, so the entry's declaration resolved to a plain
  usage on the next line. `is_destructured_key` has matched the directive's
  name part (`keys`/`strs`/`syms`, whatever the namespace) since #24. Only a
  regression test for the literal-namespace shape is missing: the e2e suite
  covers `{::db/keys [db]}` and `{:keys [simple.core/x]}`, not
  `{:simple.core/keys [x]}`.
- **Keywords in binding values** — already fixed by the same change. The
  probe at `analyzer.clj:4289` (`has-first-arg? (:clj-kondo.impl/fn-has-first-arg m)`)
  answers; what it still misses lies in `findings.edn`. Regression tests
  for the broken shape (a comment inside the binding vector before the
  pair) and close.
- **Keywords in quoted data** — the bulk of what remains in
  `keyword/qualified`: 118 of the 122 missing sites are `'{:deps …}`,
  `'[:nilable/set …]`, `''([…])`, `(assert-submaps '[…])`. The other 4 are
  in non-Integrant `.edn` files under source paths, a class of its own. The
  run also shows one more skipped data position, the ns form's attr-map
  (`(ns x {:clj-kondo/config '{…}})`, `pprint.clj:3`), and one extra site
  on our side, a keyword inside `(comment …)`, which the oracle skips
  (`:skip-comments true`).
- **A qualified `{:keys [c/x]}` entry resolves as the keyword** — still
  open, guarded by the `KNOWN` entry in `tests/test_compare.rs`.

### 1. Quoted data records keywords

`walk_occurrences` skips every `quoting_lit` and `walk_list` skips
`(quote …)`, so a keyword under a quote is not an occurrence: references
miss it, a keyword rename leaves quoted config reading the old key, and a
cursor on it answers null.

A new `walk_quoted_data(node, ctx, out)` in `src/index/extractor.rs`
replaces both skips. It descends `named_children` (so discards and comments
stay gaps) and:

- records every `kwd_lit` through `record_keyword_occurrence` — the reader
  resolves `::k` and `::alias/k` before the quote sees them, so the existing
  `keyword_occurrence_fqn` rule applies unchanged;
- qualifies the keys of an `ns_map_lit` with `ns_map_prefix`, as
  `walk_ns_map` does, and walks the values as quoted data. Factor the key
  and value pairing out of `walk_ns_map` if that keeps the two from drifting;
  a splicing reader conditional inside the map falls back to the plain
  descent as it does there;
- records nothing for a `sym_lit`: quoted symbols stay non-usages, as the var
  rule (`test_qualified_usages_collects_and_skips_quotes`) and the alias
  rename (`alias_sites_tree`, "quoted symbols included") already assume.
  `'foo/bar` remains excluded from `qualified_usages`.

The same walker runs over the ns form's non-list children (the attr-map and
a docstring, which has no keywords) beside `collect_refer_occurrences`, so
`(ns x {:clj-kondo/config '{…}})` records `:clj-kondo/config`.

Nested quotes (`''([…])`) are quoted data all the way down; a `(quote …)`
list inside quoted data is data too. `syn_quoting_lit` stays on the ordinary
walk. `mark_quoted_symbols_used`, `collect_quoted_name_occurrences` and the
locals walker (`walk_scope`) are untouched: they never depended on the
occurrence walker's quote skip.

Consequences that need no code: keyword references, `documentHighlight`
(`TEXT`), definition (`resolve_fqn_at` matches occurrences by range) and
keyword rename (`keyword_target` builds sites from occurrences; a quoted
site is a `kwd_lit` token, so the all-or-nothing check passes) reach quoted
sites. `Index::keyword_counts` grows by the quoted keywords, which is the
intended ranking. `alias_sites_tree` tells a `{:keys [h/x]}` binding entry
from data by a keyword occurrence starting at a qualified symbol; quoted
data records no symbol-positioned keyword, so `'{:keys [h/x]}` is still
rewritten by an alias rename as before.

Extractor output changes, so `CACHE_FORMAT_VERSION` in
`src/index/jar_cache.rs` bumps.

### 2. A qualified `:keys` entry is the local it binds

`references::local_refs_at` and `definition::local_definition` both refuse a
word containing `/` ("locals are never qualified"), so from the binding
token of `{:keys [c/x]}` the fqn path answers the one keyword occurrence
`:c/x` while a usage of `x` answers the local — `documentHighlight` follows
references, so the two cursors highlight different things. The extractor
already binds `x` there (`collect_binding_targets` uses `sym_name_node`, so
the binding's `name_range` is the name part) and `is_destructured_key`
already recognises the entry.

A shared helper in `src/handlers/references.rs`:

```rust
/// The local a cursor names: an unqualified word as is, a qualified one by
/// its name part only when the cursor is inside the name part of a `:keys`
/// destructuring entry (`{:keys [c/x]}` binds `x`). `None` on a keyword.
pub(crate) fn local_name_at(documents: &DocumentStore, uri: &Url, pos: Position) -> Option<String>
```

replaces the two guards. For a qualified word it asks a new extractor
function, `destructured_entry_name_at_tree(tree, source, pos) -> Option<String>`,
which finds the `sym_lit` whose *name* node contains `pos` (`node_path_at`
or a `descendant_for_point_range` from `position_to_point`), requires a
`namespace` child, and returns the name part when `is_destructured_key`
accepts the name node's range. The helper returns the name *and* the
name node's range. `local_references_at_tree` then needs one more entry
condition: when no unqualified occurrence contains `pos`, a qualified
`:keys` entry whose name range contains `pos` also admits the cursor (its
usages are still the unqualified `x` tokens `collect_name_occurrences`
gathers, and `locals_at_node` at an LHS position already yields the entry's
binding as the declaration).

`is_destructured_key` reads shape alone (a vector after a `:keys` keyword
inside a map), so `{:keys [c/x]}` written as *data* in the body of
`(let [x 1] …)` looks the same, and the outer `x` would be in scope there.
A data-position `c/x` is the var `c/x`, never the local, so both callers
require the binding they resolve to be the entry itself: references
accept the cursor only when the declaration `locals_at_node` yields equals
the entry's name range, and `local_definition` only when the binding it
finds has that range. An unqualified `{:keys [x]}` as data is unaffected
(there `x` is evaluated, and resolving it to the outer local is right).

Results: references from `c/|x` answer the entry's name range plus the
usages; `documentHighlight` (which calls `local_refs_at`) agrees, `WRITE`
on the entry; rename and `prepareRename` refuse with the destructuring
message from the local path — the same text `{::c/keys [blend]}` gets —
instead of the keyword path's `{:keys [x]}` message; definition on the
entry lands on itself, as it does for an unqualified entry today.

A cursor on the namespace half (`c` of `c/x`) keeps resolving to the
keyword `:c/x` through `resolve_fqn_at`, mirroring the alias-versus-var
split of `h/greet`: one token, two things, split by half.

The `KNOWN` entry for `local/destructured` tokens containing `/` is removed;
`compare_simple_project` (fixture `alias_sites.clj:10`) becomes the test.

### 3. Regression tests for the two already-fixed entries

- e2e: rename and `prepareRename` on the entry of
  `(defn f [{:simple.core/keys [x]}] x)` are refused with the destructuring
  message; references from the entry answer the entry plus the usage; the
  keyword `:simple.core/x` lists the entry as a site.
- extractor: `(let [a 1\n ;; note\n b (:my.ns/k m)] b)` and
  `(if-let [x (some-> m ::k)] x)` record `:my.ns/k` and the current-ns `::k`
  at the value's range.

### 4. Compare bookkeeping

- New Backlog line in `docs/ROADMAP.md`: keywords in non-Integrant `.edn`
  files under source paths are not occurrences (kondo lints
  `test-regression/**/findings.edn` and `config.types.edn`; the index
  keeps only what `is_integrant_edn` accepts).
- No `KNOWN` entry for the `(comment …)` site: a matcher on
  `missing: 0` alone would hide every superset answer in `keyword/`, and
  the verdict carries nothing that says "inside a comment form". The one
  divergence (`types.clj:1095`, probes on `:nilable/set`) stays visible in
  the report and is explained in `docs/MEMORY.md`; the gate is advisory.
- `bb compare clj-kondo` re-run; the table in `docs/MEMORY.md` gains a
  dated section (or new columns) for `keyword/qualified` and any bucket that
  moved; the four issue files move to `docs/archive/`; the ROADMAP gets one
  item under Milestone 5 (before **Release**) linking the four archived
  issues with this plan; `docs/FEATURES.md` and `CLAUDE.md` invariants
  are updated.

### Testing

Extractor tests for the walker, e2e tests for the client-visible answers,
`compare_simple_project` for the `KNOWN` removal. Gates from the
CLAUDE.md table: `bb check`, `bb e2e`, `bb compare` (extractor and
references change), `bb bench clj-kondo` (extractor change; the walker
runs on every file), `bb e2e-pulse` (client-visible change), `bb e2e-calva`
(definition change).

## File Structure

- Modify `src/index/extractor.rs` — `walk_quoted_data`, its use in
  `walk_occurrences`, `walk_list` (`quote` and `ns` arms);
  `destructured_entry_name_at_tree`; entry condition in
  `local_references_at_tree`; unit tests beside
  `local_refs_flags_keys_destructured`.
- Modify `src/index/jar_cache.rs` — `CACHE_FORMAT_VERSION` 19 → 20.
- Modify `src/handlers/references.rs` — `local_name_at`, used by
  `local_refs_at`; doc comments on `resolve_fqn_at` ("locals are never
  qualified") and `rename_target`.
- Modify `src/handlers/definition.rs` — `local_definition` uses
  `local_name_at`.
- Modify `tests/test_extractor.rs` — quoted-data and binding-value tests.
- Modify `tests/test_e2e.rs` — quoted keyword references and rename;
  `{:keys [c/x]}` references, highlight, rename; `{:simple.core/keys [x]}`
  refusal.
- Modify `tests/test_compare.rs` — remove the `local/destructured` entry.
- Modify `docs/ROADMAP.md`, `docs/MEMORY.md`, `docs/FEATURES.md`,
  `CLAUDE.md`; move three files from `docs/backlog/` to `docs/archive/` (one file covers both keyword entries).

## Tasks

### Task 1: Start the ROADMAP item

**Files:**
- Modify: `docs/ROADMAP.md`

- [x] **Step 1: Add the item** under Milestone 5, before **Release**, as
  `- [x] **Keyword occurrences in quoted data; qualified `:keys` entries as locals.**`
  with two sentences on the four issues (the two the gaps fix already
  resolved named as such), the four `backlog/` links, and
  `Plan: [2026-09-28-2134-quoted-keywords-and-qualified-keys-locals.md](plans/…) — in progress`.
- [x] **Step 2: Commit**
  `git commit -m "roadmap: start quoted keywords and qualified :keys locals"`

### Task 2: Extractor walks quoted data for keywords

**Files:**
- Modify: `src/index/extractor.rs`
- Modify: `src/index/jar_cache.rs`
- Test: `tests/test_extractor.rs`

- [x] **Step 1: Write the failing tests** next to
  `test_occurrence_qualified_alias_name_only_range`, using `extract_full`
  and `occurrences_of`:
  - `'{:mvn/version "1"}` in a `(def deps …)` records `:mvn/version` at the
    keyword's range;
  - `(quote [:a/b :c])` records `:a/b` and `:c`;
  - `''([{:keys [:foo/bar]}])` records `:foo/bar`;
  - `'#::{:id 1}` under `(ns my.app)` records `:my.app/id`, and
    `'#:user{:id 1}` records `:user/id`;
  - `'::alias/k` with `[other.lib :as alias]` required records
    `:other.lib/k`;
  - `'foo/bar` and `'(f x)` record no symbol occurrence (`other.lib/f`
    absent, and `qualified_usages` still excludes `foo/bar`);
  - `(ns my.app {:clj-kondo/config '{:linters {:x/y 1}}})` records
    `:clj-kondo/config` and `:x/y`.
- [x] **Step 2: Run them to see them fail**
  Run: `cargo test --test test_extractor quoted`
  Expected: FAIL on the missing occurrences.
- [x] **Step 3: Implement `walk_quoted_data`** as designed; wire it into
  `walk_occurrences` (`quoting_lit`), `walk_list` (`Some("quote")` walks
  `children[1..]`; `Some("ns")` also walks every non-`list_lit` child after
  the name). Update the comments that say quoted data is skipped
  (`walk_occurrences`, the module doc near line 36, `record_keyword_occurrence`).
- [x] **Step 4: Bump `CACHE_FORMAT_VERSION`** to 20.
- [x] **Step 5: Run the extractor tests**
  Run: `cargo test --test test_extractor`
  Expected: PASS, including `test_qualified_usages_collects_and_skips_quotes`
  and `test_are_quoted_template_argument_counts_as_used`.
- [x] **Step 6: Commit**
  `git commit -m "extractor: record keyword occurrences inside quoted data"`

### Task 3: Quoted keywords reach references and rename end to end

**Files:**
- Test: `tests/test_e2e.rs`

- [x] **Step 1: Write the e2e test** `test_e2e_keyword_sites_in_quoted_data`:
  write a probe file into the copied fixture (as
  `test_e2e_rename_refuses_qualified_keys_destructuring` does) holding
  `(def cfg '{:simple.keywords/local 1})`; open `src/keywords.clj`;
  references on `::local` include the probe's quoted site; rename `::local` to `flag` returns an edit for
  the probe file whose text becomes `:simple.keywords/flag`; definition
  from inside the quoted keyword does not error.
- [x] **Step 2: Run it**
  Run: `cargo test --test test_e2e keyword_sites_in_quoted_data`
  Expected: PASS.
- [x] **Step 3: Commit**
  `git commit -m "e2e: keyword references and rename reach quoted data"`

> Deviation: the probe uses `::c/thing` (`:simple.core/thing`) instead of `::local` — the fixture's `kw_destructure.clj` reads `::local` through `{::kw/keys [local]}`, so its rename is refused by design.

### Task 4: A qualified `:keys` entry resolves as the local

**Files:**
- Modify: `src/index/extractor.rs`
- Modify: `src/handlers/references.rs`
- Modify: `src/handlers/definition.rs`
- Test: `tests/test_e2e.rs`, `tests/test_compare.rs`

- [x] **Step 1: Write the failing e2e test** `test_e2e_qualified_keys_entry_is_a_local`
  on `src/alias_sites.clj` line `(defn g [{:keys [c/x]}] x)`:
  - references at the `x` of `c/x` answer the entry's name range and the
    usage — the same set as references at the usage;
  - `documentHighlight` there answers the two ranges, `WRITE` on the entry;
  - `prepareRename` and `rename` at the entry are refused with the
    `:keys/:strs/:syms destructured binding` message
    (`prepare_rename_error`);
  - definition at the `x` of `c/x` answers the entry's own name range;
  - references at the `c` of `c/x` still answer the keyword `:simple.core/x`
    sites (the existing behaviour, pinned);
  - in a probe file, `(defn h [x] {:keys [c/x]})`: references at the data
    entry's `x` do not answer the param `x` (they fall through to the
    keyword `:c/x`), and definition there does not land on the param.
- [x] **Step 2: Remove the `local/destructured` entry** from `KNOWN` in
  `tests/test_compare.rs`, so `compare_simple_project` fails with the
  divergence.
- [x] **Step 3: Run both to see them fail**
  Run: `cargo test --test test_e2e qualified_keys_entry_is_a_local` and
  `cargo test --test test_compare compare_simple_project`
  Expected: FAIL (the second skips when the host has no clj-kondo; this
  host has one).
- [x] **Step 4: Implement** `destructured_entry_name_at_tree` in the
  extractor with a unit test beside `local_refs_flags_keys_destructured`
  (cursor on the name part → `Some(("x", range))`; on the namespace part,
  on a qualified symbol in a call, or on a `{:keys [x]}` unqualified entry
  → `None`); extend `local_references_at_tree`'s entry condition with the
  declaration-equals-entry check; add
  `local_name_at` to `references.rs` and use it in `local_refs_at` and
  `definition::local_definition`. Update the "locals are never qualified"
  comments in `resolve_fqn_at`, `local_refs_at`, `local_definition` and
  `rename_target`.
- [x] **Step 5: Run the tests**
  Run: `cargo test --test test_e2e qualified_keys` and
  `cargo test --test test_compare compare_simple_project` and
  `cargo test --lib extractor`
  Expected: PASS.
- [x] **Step 6: Commit**
  `git commit -m "references: a qualified :keys entry resolves as the local it binds"`

> Deviation: `local_name_at` returns `Option<(String, Option<Range>)>` — the entry's name range rides along so `local_definition` can make the declaration-equals-entry check without a second tree query.
> Deviation: the namespace half of `{:keys [c/x]}` resolves to the keyword `:c/x`, not `:simple.core/x` — destructuring reads the entry's namespace verbatim, so the test pins references there to the entry token alone.

### Task 5: Regression tests for the two entries the gaps fix closed

**Files:**
- Test: `tests/test_e2e.rs`, `tests/test_extractor.rs`

- [ ] **Step 1: e2e** `test_e2e_rename_refuses_literal_namespace_keys_entry`:
  probe file `(ns simple.ns-keys)\n(defn f [{:simple.core/keys [x]}]\n  x)`;
  `prepare_rename_error` at the entry's `x` contains
  `destructured binding 'x'`; rename there errors; references at the entry
  answer entry plus usage; references on `:simple.core/x` in `keywords.clj`
  include the entry line.
- [ ] **Step 2: extractor** `test_keyword_occurrences_in_binding_values_after_a_comment`:
  `(ns my.app)\n(defn f [m]\n  (let [a 1\n        ;; note\n        b (:my.ns/k m)\n        c (if-let [x (some-> m ::k)] x)]\n    [a b c]))`
  records `:my.ns/k` and `:my.app/k` at the value ranges.
- [ ] **Step 3: Run**
  Run: `cargo test --test test_e2e literal_namespace_keys` and
  `cargo test --test test_extractor binding_values`
  Expected: PASS without code changes (they pin the gaps fix).
- [ ] **Step 4: Commit**
  `git commit -m "tests: pin :ns/keys rename refusal and binding-value keywords"`

### Task 6: Full gates and the compare record

**Files:**
- Modify: `docs/MEMORY.md`

- [ ] **Step 1: Run the gates**
  Run: `bb check`, then `bb e2e`, then `bb compare clj-kondo`
  Expected: `bb check` and `bb e2e` green; compare exits 0 with
  `keyword/qualified` diverge and null both well below 93 and 41, the
  leftover divergences being `.edn` sites and the one `(comment …)` site;
  `local/destructured` and `keyword/keys` unchanged at 0 diverge. Paste
  the `COMPARE_JSON` rows.
- [ ] **Step 2: Record the run** in `docs/MEMORY.md` under the compare
  section: a dated paragraph with the rows that moved and what remains
  (`.edn` files, the `(comment …)` site the oracle's `:skip-comments`
  drops).
- [ ] **Step 3: Run `bb bench clj-kondo`** and compare the timeline and
  keystroke rows against the recorded tables; a visible regression is a
  finding to report, not to absorb.
- [ ] **Step 4: Run `bb e2e-pulse` and `bb e2e-calva`** (client-visible
  references/highlight change; definition change).
  Expected: both green.
- [ ] **Step 5: Commit**
  `git commit -m "docs: record the compare run after quoted keyword sites"`

### Task 7: Docs and close-out

**Files:**
- Modify: `docs/FEATURES.md`, `CLAUDE.md`, `docs/ROADMAP.md`
- Move: the three `docs/backlog/2026-09-17-*.md` files (keywords-in-binding-values-and-quoted-data, ns-keys-with-explicit-namespace-renames-the-local, qualified-keys-entry-resolves-as-the-keyword) to `docs/archive/`

- [ ] **Step 1: FEATURES.md** — Find references / keyword navigation:
  keyword sites include quoted data (`'{…}`, `(quote …)`) and the ns
  attr-map; Rename: a `{:keys [c/x]}` entry is the local `x` (refused like
  any destructured binding), the namespace half is the keyword.
- [ ] **Step 2: CLAUDE.md invariants** — the "Keyword occurrences carry
  both notations" bullet gains the quoted-data rule (keywords recorded,
  symbols not, `walk_quoted_data`); the rename/highlight bullets note that
  `local_refs_at` accepts a qualified `:keys` entry by its name part
  (`local_name_at`) as the one exception to "locals are never qualified".
- [ ] **Step 3: ROADMAP** — tick the item, `Plan: … — done`, links
  rewritten to `archive/`; add the Backlog line for keywords in
  non-Integrant `.edn` files under source paths (dated 2026-09-28); delete
  the four Backlog lines. Move the three issue files with `git mv`, adding
  a `**Status:** done (2026-09-28, …)` line to each.
- [ ] **Step 4: README** — check the features paragraph; it names
  references and rename without detail, so it most likely needs nothing.
  The working rules ask for README and CLAUDE.md in the closing change,
  so say so in the commit message if it stayed untouched.
- [ ] **Step 5: `bb check`** once more for the docs-only change (fmt of
  nothing, but it is the rule).
- [ ] **Step 6: Commit**
  `git commit -m "docs: quoted keyword sites and qualified :keys locals; archive four compare issues"`
