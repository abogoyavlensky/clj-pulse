# Rename Require Alias Implementation Plan

**Status: complete** (2026-09-16, branch `feat/rename-require-alias`).

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename a namespace alias bound by `:as` / `:as-alias` in the `ns` form, rewriting the binding and every site in the same file that spells the alias (`h/greet`, `::h/kw`, `#::h{…}`), from a cursor on any of them; `prepareRename` refuses exactly what `rename` would (ROADMAP Milestone 4).

**Tech Stack:** Rust, tower-lsp 0.20, tree-sitter-clojure. Tests: `tests/test_extractor.rs`, e2e in `tests/test_e2e.rs` on `tests/fixtures/simple_project`, `bb e2e-pulse` (rename is client-visible).

---

## Design

### Today

`references::rename_target` knows three targets: a local, a var by fqn, a qualified keyword. A cursor on the alias half of `h/greet` resolves to the *var* (`resolve_fqn_at`'s alias fallback) and `prepare_rename` reports the name's range through `on_qualifier_of`. There is no way to rename the alias itself: the ns-form parser (`parse_libspec_items`) records `alias → namespace` in `NsMeta.aliases` without ranges, and occurrences record the resolved fqn, not the alias a site used.

### The target

An alias is file-local, so the rename is one `WorkspaceEdit` for one document, built from the live tree (`DocumentStore::snapshot`), never from the index. Sites are found by a plain tree walk that is independent of the occurrence walker, since the question is textual ("which tokens spell this alias") rather than semantic.

**Where a rename starts.** The cursor sits on:

- the alias symbol after `:as` or `:as-alias` inside a libspec of the ns form (`(:require …)`, `(:use …)`, prefix lists and `#?`/`#?@` branches included, the same shapes `process_require_spec` accepts);
- the `sym_ns` part of a qualified symbol (`h|/greet`);
- the `kwd_ns` part of an auto-resolved keyword (`::h|/kw`, `::h/keys`);
- the prefix of an auto-resolved namespaced map (`#::h|{…}`).

The candidate name must be a key of the file's live `NsMeta.aliases` (first tuple element of `extract_full_tree`). Otherwise the cursor is not on an alias and `rename_target` continues to the existing paths unchanged: `simple.core/add` (a full namespace as qualifier) still renames the var, so `on_qualifier_of` stays.

**Cursor on the alias half of `h/greet` renames the alias, not `greet`.** This changes today's behavior on purpose; the var is still renamed from its name half. `test_e2e_prepare_rename_on_alias_half_reports_the_name` flips to assert the alias range.

**Sites that change**, always the namespace part alone, never the name:

| Site | Range edited |
|---|---|
| `[simple.helpers :as h]`, `[x :as-alias h]` (every branch of a reader conditional) | the `h` symbol |
| `h/greet`, `'h/greet`, `` `h/greet ``, `^h/Type` — any `sym_lit` with namespace `h` | its `sym_ns` |
| `::h/kw`, `::h/keys` — a `kwd_lit` with marker `::` and namespace `h` | its `kwd_ns` |
| `#::h{…}` — an `ns_map_lit` whose prefix is a `kwd_lit` with marker `::` and name `h` | the prefix's `kwd_name` |

Quoted symbols are included: `(resolve 'h/greet)` resolves the alias at run time and a user renaming `h` wants it followed.

**Sites left alone:**

- `:h/kw` with a single colon: a literal namespace, never alias-resolved by the reader.
- A destructuring entry (`{:keys [h/x]}` in a binding position): `clojure.core/destructure` takes the entry's namespace verbatim, so it reads `:h/x`, not the aliased namespace. The occurrence walker (`record_destructuring_keys`) already knows which maps are binding patterns and records each such entry as a *keyword* occurrence whose `name_range` is the whole entry symbol. The alias walk reuses that knowledge rather than guessing from the key name: a qualified symbol whose start equals the start of a colon-prefixed occurrence is a destructuring entry and is skipped. (A `::h/x` keyword occurrence starts at its `::` marker, two columns before its `kwd_ns`, so it never matches.) Plain data such as `(def handlers {:keys [h/greet]})` is not a binding pattern, records `h/greet` as a var occurrence, and *is* rewritten. The directive itself (`::h/keys`) is a keyword site and is rewritten too. `:syms [h/x]` in a binding position records no occurrence and is rewritten; it reads the literal symbol key `h/x`, a shape rare enough to accept.
- The alias as a bare symbol anywhere but after `:as`/`:as-alias` (a local named `h`): unrelated.
- Other files: an alias is bound per file. Two files using the same alias are two renames.

**Refusals.** Everything that does not need the new name lives in `rename_target`, so `prepare_rename` refuses the same things: library-file origin (already checked first) and "not on an alias" (falls through; if nothing else resolves, the existing "nothing to rename here"). `rename` alone checks the new name: `is_valid_symbol_name` (no `/`, no leading digit), and it must not already be a key of `NsMeta.aliases` — `cannot rename alias 'h' to 'c': 'c' is already an alias in this file` — because merging two aliases rewrites `c/x` to mean a different namespace.

### Components

`src/index/extractor.rs` gains two public functions, both over a cached tree:

```rust
pub struct AliasSites {
    /// The `:as` / `:as-alias` symbols in the ns form binding this alias.
    pub declarations: Vec<Range>,
    /// Every namespace part that spells the alias, in document order.
    pub usages: Vec<Range>,
}

/// The alias the cursor is on, if any: an `:as`/`:as-alias` binding in the ns
/// form, or the namespace part of a qualified symbol, auto-resolved keyword
/// or namespaced-map prefix. Purely positional — the caller checks it against
/// `NsMeta.aliases`.
pub fn alias_at_tree(tree: &Tree, source: &str, pos: Position) -> Option<String>;

/// Every site in the file that spells `alias` (see the site table).
/// `occurrences` is the file's occurrence list from `extract_full_tree`; a
/// qualified symbol starting where a keyword occurrence starts is a
/// destructuring entry and is left out.
pub fn alias_sites_tree(tree: &Tree, source: &str, alias: &str, occurrences: &[Occurrence]) -> AliasSites;
```

`alias_at_tree` is only a candidate finder: it names the alias the cursor *spells*, and `rename_target` then requires `pos` to be inside one of the collected declarations or usages. A cursor on the `h` of a `{:keys [h/x]}` entry therefore names `h` but is not a site, so the alias path falls through; `resolve_fqn_at` then finds the `:h/x` keyword occurrence and `keyword_target` refuses it with the existing destructuring message, from `rename` and `prepareRename` alike.

`alias_at_tree` uses `node_path_at` for the innermost literal and then checks the cursor against the `namespace` child (symbols, keywords) or the prefix's `name` child (`ns_map_lit`), with `range_contains` semantics (end inclusive, as `references::range_contains`). For the ns-form case it finds the top-level `(ns …)` list, walks its `:require`/`:use` clauses recursively through vectors, lists and reader conditionals, and matches a `sym_lit` immediately preceded by a `kwd_lit` `:as`/`:as-alias`.

`alias_sites_tree` walks the whole tree once. The ns-form declaration walk and the usage walk share the reader-conditional recursion; the destructuring skip compares each qualified symbol's start against the keyword occurrences passed in. A `sym_lit` directly after `:as` in the ns form is a declaration, not a usage, so the usage walk skips the ns form's libspec option positions (or the sites are de-duplicated by range afterwards — pick whichever reads simpler in the code; the tests assert on the two lists).

`src/handlers/references.rs`:

- `RenameTarget::Alias { alias: String, sites: extractor::AliasSites }`.
- In `rename_target`, after the locals branch and before `resolve_fqn_at`: `alias_at_tree` on the snapshot; if the name is in the live `NsMeta.aliases` (from `extract_full_tree`), collect sites with that call's occurrences. Return `Alias` only when `declarations` is non-empty **and** `pos` is inside a declaration or usage; otherwise fall through to the fqn path. Empty `declarations` with the alias in `NsMeta` means the walk missed a libspec shape the parser accepts; falling through is the safe answer, and the extractor test pins every accepted shape.
- `prepare_rename`: the declaration or usage range containing `pos`.
- `rename`: the alias-collision check, then one `TextEdit` per declaration and usage, all under the origin URI.

### Testing

- Extractor (`tests/test_extractor.rs`): `alias_sites_tree` on a snippet whose ns form binds `h` four ways — `[a :as h]`, `[b :as-alias h]`, a prefix list `(c [d :as h])`, and a splicing `#?@(:clj [[e :as h]] :cljs [[f :as h]])` — followed by `h/f`, `'h/f`, `` `h/f ``, `(str "naïve" h/f)` (a non-ASCII string before the site, so the expected column is in UTF-16 units), `::h/k`, `(defn f [{::h/keys [x]}] x)`, `#::h{:k 1}`, `(def data {:keys [h/f]})` (data, rewritten), `(defn g [{:keys [h/x]}] x)` (binding, skipped), `:h/k` (skipped), and `(let [h 1] h)` (skipped); assert the declaration count is 5, the usage list is exactly the expected ranges in order, and the three skips are absent. `alias_at_tree` at each site kind returns the alias; on `greet` in `h/greet`, on `:h/k`, and on a bare local `h` returns `None`.
- e2e (`tests/test_e2e.rs`, `simple_project`):
  - rename `h` → `help` from the `:as h` in `ns_options.clj`: two edits in that file only (`:as h`, `h/greet`), each covering exactly `h`; text after applying reads `:as help` and `help/greet`.
  - rename `cfg` → `conf` from the `::cfg/port` usage: edits at `:as-alias cfg` and the `cfg` of `::cfg/port`.
  - the new fixture `src/alias_sites.clj`: rename `c` from `#::c{…}` and count the edits, asserting the `{:keys [c/x]}` entry and the `:c/x` literal keep their columns out of the edit list.
  - `prepareRename` on the `h` of `h/greet` returns the alias range (rewrite `test_e2e_prepare_rename_on_alias_half_reports_the_name`).
  - rename `h` → `cfg` is refused with the collision message; rename to `bad/name` is refused as an invalid symbol name.
  - the existing `test_e2e_rename_across_files` (name half of `core/add`) stays green.
  - after an unsaved `didChange` that inserts a line above the ns form, the edits land on the shifted lines.
- `bb e2e-pulse`: extend the existing rename coverage in `scripts/pulse-e2e/` with an alias rename if a rename test exists there; otherwise leave the suite as the client-visible gate and run it.

## File Structure

Modify:

- `src/index/extractor.rs`: `AliasSites`, `alias_at_tree`, `alias_sites_tree`, the shared ns-form libspec walk.
- `src/handlers/references.rs`: `RenameTarget::Alias`, the branch in `rename_target` (candidate, live aliases, site membership), `prepare_rename`, `rename`.
- `tests/test_extractor.rs`: alias site tests.
- `tests/test_e2e.rs`: alias rename tests; the flipped alias-half prepareRename test.
- `tests/fixtures/simple_project/src/alias_sites.clj` (new): `(ns simple.alias-sites (:require [simple.core :as c]))` plus the site kinds from the table, one per line, so columns are easy to assert.
- `README.md` (Rename bullet), `AGENTS.md` (invariant), `ARCHITECTURE.md` (references.rs paragraph), `docs/ROADMAP.md` (Milestone 4 item with `Plan:` line; a Backlog line for references/documentHighlight on an alias).

## Tasks

### Task 1: `alias_sites_tree`

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [x] **Step 1: Write the failing test**
  `test_alias_sites_covers_every_notation_and_skips_literals` on the snippet from the Testing section, parsed with the same helper the other extractor tests use and passing the occurrences of `extract_full_tree`. Assert `declarations.len() == 5`, `usages` equals the expected `(line, start, end)` triples in document order (the `naïve` line's column counted in UTF-16 units), the data-map `{:keys [h/f]}` entry is present, and no range starts at the columns of the binding `{:keys [h/x]}` entry, the `:h/k` literal, or the local `h`.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor alias_sites`
  Expected: FAIL to compile (function missing).

- [x] **Step 3: Implement**
  Add `AliasSites` and `alias_sites_tree`. Declarations: locate the first top-level `list_lit` whose head is `ns`; for each `:require`/`:use` clause recurse through `vec_lit`, `list_lit` (prefix lists) and reader conditionals the way `process_require_spec` does, and push the range of any `sym_lit` that follows a `:as`/`:as-alias` keyword and spells `alias`. Usages: recursive walk over every node; on `sym_lit` with a `namespace` child equal to `alias`, push the namespace child's range unless its start equals the start of an occurrence whose fqn begins with `:` (a destructuring entry); on `kwd_lit` with marker `::` and `namespace` equal to `alias`, push the namespace child's range; on `ns_map_lit` whose prefix is a `kwd_lit` with marker `::` and no namespace and `name` equal to `alias`, push the name child's range. Exclude declaration ranges from usages.

- [x] **Step 4: Run to verify pass**
  Run: `cargo test --test test_extractor alias_sites`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Collect the sites a require alias is spelled at"`

> Deviation: usages are never de-duplicated against declarations — a declaration is a bare symbol and the usage walk only collects namespace parts, so the two lists cannot overlap (the test asserts it).

### Task 2: `alias_at_tree`

**Files:**
- Modify: `src/index/extractor.rs`
- Test: `tests/test_extractor.rs`

- [x] **Step 1: Write the failing test**
  `test_alias_at_finds_the_alias_under_the_cursor`: for the same snippet, a cursor on the `h` after `:as`, on the `h` of `h/f`, on the `h` of `::h/k`, on the `h` of `#::h{`, and at the end column of each (end inclusive) returns `Some("h")`; on the `f` of `h/f`, on the `h` of `:h/k`, and on the bare local `h` returns `None`. A cursor on the `h` of the binding `{:keys [h/x]}` returns `Some("h")` too — it is a candidate; Task 4's membership check is what rejects it.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor alias_at`
  Expected: FAIL to compile.

- [x] **Step 3: Implement**
  `alias_at_tree`: innermost node from `node_path_at`; match on `sym_lit`/`kwd_lit` (namespace child contains `pos`; keywords need the `::` marker), `ns_map_lit` (prefix's name child contains `pos`), else check whether the node is a declaration site by reusing the declaration walk from Task 1 (a `sym_lit` whose range contains `pos` and which the walk lists). Share the `contains` helper with `range_contains` or add a small one in the extractor.

- [x] **Step 4: Run to verify pass**
  Run: `cargo test --test test_extractor alias_at`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Find the require alias under the cursor"`

> Deviation: the declaration walk returns nodes (`alias_declarations`) and both callers filter — by text for the sites, by range for the cursor — instead of reusing a text-filtered walk; a cursor right after `[a :as h]`'s `h` sits on the `]`, so declarations are matched by range before `node_path_at` is consulted.

### Task 3: Fixture and e2e tests for the rename

**Files:**
- Create: `tests/fixtures/simple_project/src/alias_sites.clj`
- Modify: `tests/test_e2e.rs`

- [x] **Step 1: Add the fixture**
  `(ns simple.alias-sites (:require [simple.core :as c]))` followed by one site per line: `(c/add 1 2)`, `(resolve 'c/add)`, `(def k ::c/thing)`, `(defn f [{::c/keys [x]}] x)`, `(def m #::c{:thing 1})`, `(def data {:keys [c/add]})`, `(defn g [{:keys [c/x]}] x)`, `(def lit :c/thing)`, `(let [c 1] c)`. Check that no existing e2e assertion counts files or symbols in `simple_project` in a way the new file breaks (`grep -n "simple_project\|workspace_symbol" tests/test_e2e.rs`), and that the new file's `::c/thing` occurrences do not change the expected counts of keyword tests on `simple.core/thing` (`test_e2e_rename_keyword_*`, references on `::c/thing` in `keywords.clj`); if they do, rename the keyword in the fixture to `::c/site`.

- [x] **Step 2: Write the failing e2e tests**
  In `tests/test_e2e.rs`, next to the keyword rename tests:
  - `test_e2e_rename_alias_from_its_binding` (`ns_options.clj`, `h` → `help`, two edits, apply them with the same helper the unsaved-edits tests use and assert the text).
  - `test_e2e_rename_as_alias_from_a_keyword_usage` (`cfg` → `conf`).
  - `test_e2e_rename_alias_skips_literal_keyword_and_destructuring_entry` (`alias_sites.clj`, from `#::c{`; assert 7 edits: the binding, `c/add`, `'c/add`, `::c/thing`, `::c/keys`, `#::c{`, the data-map `c/add`; assert none on the binding `{:keys [c/x]}` line, the `:c/thing` line or the `let` line).
  - `test_e2e_rename_from_destructuring_entry_refuses_like_keyword_rename` (`alias_sites.clj`, cursor on the `c` of the binding `{:keys [c/x]}`: both `rename` and `prepareRename` are refused, and the message is the keyword path's destructuring message, not an alias rename of the other sites).
  - `test_e2e_rename_alias_refuses_existing_alias` (`h` → `cfg` refused, message contains "already an alias").
  - `test_e2e_rename_alias_uses_unsaved_edits` (didChange inserting a comment line at the top; edits land one line lower).
  - Rewrite `test_e2e_prepare_rename_on_alias_half_reports_the_name` as `test_e2e_prepare_rename_on_alias_half_reports_the_alias`: the range is the `h` before the `/`.

- [x] **Step 3: Run to verify failure**
  Run: `cargo test --test test_e2e rename_alias -- --test-threads=4` and `cargo test --test test_e2e prepare_rename_on_alias_half`
  Expected: FAIL (rename answers the var or refuses).

- [x] **Step 4: Commit the fixture and tests**
  `git commit -m "Add e2e coverage for renaming a require alias"`

> Deviation: the fixture spells `c/blend` (an undefined var) and `::c/site`, not `c/add` / `::c/thing` — `test_e2e_rename_across_files` asserts exactly three files edited for `simple.core/add` and the references tests count its usages, so the fixture must not add sites of an existing var.

### Task 4: `RenameTarget::Alias`

**Files:**
- Modify: `src/handlers/references.rs`

- [x] **Step 1: Implement the target**
  Add the variant; in `rename_target`, after the locals branch: snapshot the document, `alias_at_tree`, look the name up in the live `NsMeta.aliases` (from `extract_full_tree`, so an alias just typed counts), then `alias_sites_tree`. Empty `declarations` refuses. In `prepare_rename`, answer the declaration or usage range containing `pos`. In `rename`, before building edits, refuse when `new_name` is already a key of the live aliases; then one `TextEdit` per range under the origin URI. Keep the doc comments in the style of the keyword branch: say why quoted symbols are included and why `:keys` entries are not.

- [x] **Step 2: Run the e2e tests**
  Run: `cargo test --test test_e2e rename -- --test-threads=4`
  Expected: PASS, including `test_e2e_rename_across_files` and the keyword rename tests.

- [x] **Step 3: Full check**
  Run: `bb check`
  Expected: green (fmt, clippy `-D warnings`, all tests).

- [x] **Step 4: Commit**
  `git commit -m "Rename a require alias from its binding or any usage"`

> Deviation: the fixture's `{::c/keys [x]}` became `{::c/keys [blend]}` — it read `:simple.core/x`, and `test_e2e_rename_refuses_qualified_keys_destructuring` asserts the refusal names `qualified_keys.clj` as the only such site.

### Task 5: Editor gate and docs

**Files:**
- Modify: `README.md`, `AGENTS.md`, `ARCHITECTURE.md`, `docs/ROADMAP.md`, `scripts/pulse-e2e/` (only if it has a rename step)

- [x] **Step 1: Run the editor gate**
  Run: `bb e2e-pulse`
  Expected: green. If the Pulse suite has a rename step, add an alias rename to it first; otherwise the existing rename coverage is the gate.

- [x] **Step 2: Update the docs**
  README: extend the Rename bullet with a sentence on aliases (from the binding or any `h/x`, `::h/x`, `#::h{}` site; `:h/x` and `{:keys [h/x]}` untouched). AGENTS.md Invariants: one paragraph after the keyword-rename one — alias rename is file-local, textual over the live tree, resolved before the fqn path, and a cursor on the alias half renames the alias. ARCHITECTURE.md: a line in the references.rs paragraph. ROADMAP: tick the Milestone 4 item, status `done`; add the Backlog line for references/documentHighlight on an alias half. Set this plan's status to complete.

- [x] **Step 3: Final check**
  Run: `bb check && bb e2e`
  Expected: green.

- [x] **Step 4: Commit**
  `git commit -m "Document require-alias rename"`

---

## Completion summary

Implemented as designed: `extractor::AliasSites` / `alias_sites_tree` /
`alias_at_tree` (Tasks 1–2), the `alias_sites.clj` fixture and seven e2e
tests (Task 3), `RenameTarget::Alias` resolved before the fqn path with
`prepareRename` sharing every refusal (Task 4), an alias-rename step in the
Pulse e2e and the README / AGENTS / ARCHITECTURE / ROADMAP updates (Task 5).
`bb check`, `bb e2e` and `bb e2e-pulse` are green; the Pulse run drives a real
VS Code through the rename and applies the edit.

Codex review rounds added two things the plan did not have:

- declarations are read from *every* ns form, so a `.cljc` with one
  `(ns …)` per reader-conditional branch renames both bindings (Task 1 fixup);
- the new name is also refused when it already qualifies a name in the file
  (`[clojure.set]` required and `clojure.set/union` called — alias lookup
  outranks the full namespace, so an alias `clojure.set` would capture it),
  not only when it is already an alias (Task 4 fixup).

Deviations, gathered:

- Task 1: usages are not de-duplicated against declarations — a declaration is
  a bare symbol and the usage walk only collects namespace parts.
- Task 2: the declaration walk returns nodes and callers filter by text or by
  range; a cursor right after `[a :as h]`'s `h` sits on the `]`, so declarations
  are matched by range before `node_path_at`.
- Task 3/4: the fixture spells `c/blend`, `::c/site` and `{::c/keys [blend]}`
  instead of `c/add`, `::c/thing` and `[x]` — sites of `simple.core/add` and
  `:simple.core/x` are counted by existing tests.
- TaskCreate/TaskUpdate were unavailable in this harness; the plan document was
  the sole tracker.

What the plan could have specified better: the fixture's identifiers — it asked
to check for count collisions but named `c/add`, which collides, and did not
anticipate the `{::c/keys [x]}` one; and the full-namespace capture case in
the new-name check.
