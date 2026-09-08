# Keyword Completion and Auto-Require Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete keywords from what the project already uses, in the notation being typed, and let completion insert the missing `:require` when a var from an unrequired namespace is accepted (ROADMAP Milestone 2, part 2 of 2). Depends on the completion-core plan (`2026-09-06-0756-completion-fuzzy-resolve.md`) for the matcher, ranking, and `CompletionOptions`.

**Tech Stack:** Rust, tower-lsp 0.20, tree-sitter-clojure. Tests: extractor tests (`tests/test_extractor.rs`), index tests (`tests/test_index.rs`), document tests (`src/document.rs`), completion tests (`tests/test_completion.rs`), e2e (`tests/test_e2e.rs`), `bb e2e-nvim` and `bb e2e-pulse`.

---

## Design

### Keyword data

Keyword literals are recorded today only when qualified (`keyword_fqn` in `src/index/extractor.rs` returns `None` for `:name`), so the index knows `:my.ns/id` but not `:id`, and most keywords in real code are unqualified. The extractor starts recording unqualified keywords too, as occurrences with fqn `:name` (colon, no slash), from `record_keyword_occurrence`. Only Clojure sources; `extract_edn` is unchanged.

Consequences to keep deliberate:

- Definition on `:name` already returns `None` for colon fqns with no symbol (`handlers/definition.rs`, the `fqn.starts_with(':')` guard), so nothing changes there.
- References on `:name` start returning every usage in the project. That is a feature, and the same as references on a qualified keyword today. `rename` still refuses keywords through `rename_target`.
- `Occurrence` docs and the ARCHITECTURE "Keyword & Integrant Indexing" section say unqualified keywords are not recorded; both get updated.
- Occurrences are project-only and never cached in the JAR cache, so no `CACHE_FORMAT_VERSION` bump.

The index keeps an aggregate `keyword_counts: DashMap<String, u32>` (fqn → number of occurrences across project files), maintained at the mutation points so completion never scans `occurrences`:

- `insert_file` and `insert_edn_file`: both *replace* the path's occurrence vector, so first subtract the counts of the vector being replaced (the value `occurrences.insert` returns), then add the counts of the new one. Re-indexing a file on save must leave counts unchanged when its keywords did not change.
- `remove_file`: subtract the counts of the removed vector (the vector is returned by the `occurrences.remove`), deleting zero entries.
- `merge_project_from`: for each incoming occurrence entry, subtract the old vector for that path if present, then add the new one. Stale files go through `remove_file` already.

`pub fn keyword_counts(&self) -> Vec<(String, u32)>` is the read API, snapshotting the map.

### Keyword context

`DocumentStore::keyword_at(uri, pos) -> Option<KeywordContext>` in `src/document.rs`:

```rust
pub struct KeywordContext {
    pub auto_resolved: bool,   // `::` marker
    pub text: String,          // what follows the marker up to the cursor; may contain '/'
    pub start: Position,       // position of the first ':'
    pub end: Position,         // end of the whole keyword token (past the cursor when mid-token)
}
```

It walks back from the cursor over identifier characters (the `word_at` rule) and then requires a `:` immediately before, with an optional second `:` before that, and walks forward from the cursor to the token's end. `text` is only what precedes the cursor and is what candidates are matched against; `start..end` is what the accepted item replaces, so accepting `:name` at `:na|me` yields `:name`, not `:nameme`. The cursor right after a bare `:` or `::` yields an empty `text`.

### Keyword candidates

When `keyword_at` returns a context, `handle` returns keyword items only, from a new `complete_keywords(index, ctx, current_ns, ns_meta)`:

- `auto_resolved` and no `/` in `text`: fqns of the form `:{current_ns}/name` rendered as `::name`, plus, when `text` is empty or matches an alias, every alias (including `:as-alias` ones) rendered as `::alias/` so the user can continue.
- `auto_resolved` with `alias/name`: resolve the alias through `ns_meta.aliases`; candidates are `:{ns}/name` rendered as `::alias/name`.
- Single colon: every fqn rendered as-is (`:ns/name` or `:name`).

Matching uses `matching::match_score` on the rendered label with the same tier guardrails as the core plan. Ranking, one rule for every case: `sort_text = format!("{tier}-{scope}-{rank:08}-{label}")` where `scope` is `0` for a keyword in the current namespace and `1` otherwise, and `rank = u32::MAX - count`. So within a match tier, current-namespace keywords come first and frequency decides after that. An empty `text` (every candidate is tier 3) returns the first 100 items in that order, which means current-namespace keywords first, then the most frequent of the rest.

Every item carries `kind: CompletionItemKind::KEYWORD`, `detail: "keyword, N uses"`, `filter_text: label`, and a `text_edit` replacing `ctx.start..ctx.end` with the label, so VS Code, Calva, and Neovim replace the same span regardless of their word patterns (the Clojure Pulse `wordPattern` includes `:`; Calva's differs).

### Auto-require

Symbols from namespaces the file has not required become completable, and accepting one inserts the require. Two entry points, both in `complete_symbols`:

- Bare prefix (Pool G, digit 6): for each candidate namespace, each public var whose name matches the prefix (two or more characters) is offered as `alias/name`.
- Qualified prefix whose alias is not a known alias of the file: the same, restricted to namespaces the alias would resolve to.

Candidate namespaces and their aliases come from a shared ranking extracted from `code_action::candidates`:

```rust
/// Namespaces `prefix` could refer to once required, with the alias to use.
pub fn namespaces_for_alias(index: &Index, ns_meta: &NsMeta, prefix: &str) -> Vec<Candidate>;
```

For the bare-prefix pool there is no alias to start from, so the pool iterates a bounded set: every project namespace (`index.is_project_path(meta.file)`), aliased by its last segment, plus the `CURATED_ALIASES` table. Library namespaces outside the curated table are not offered; that keeps the pool small and the aliases predictable. The pool skips namespaces `ns_meta.resolves_prefix` already covers and the file's own namespace, and it skips any candidate whose proposed alias is already bound in `ns_meta.aliases` to a different namespace (with `[other.lib :as str]` in the file, `str/join` from `clojure.string` is never offered, since inserting `[clojure.string :as str]` would conflict). It is capped at 30 items.

Ordering is absolute, not per tier: auto-require items use `sort_text = format!("9-{tier}-{label}")`, so every in-scope item of any match tier sorts before every auto-require item. Inserting a require is a bigger action than picking a name already in scope, and an exact auto-require match must not outrank an in-scope prefix match. The qualified-prefix entry point (`str/jo` with an unknown alias) has no in-scope competitors, so the same rule costs nothing there.

Each item: label `alias/name`, `detail: "requires [ns :as alias]"`, `additional_text_edits: vec![require_edit(text, &candidate.spec())]` built against the live buffer (`documents.text`), so `complete_symbols` gains an optional `source: Option<&str>` parameter (or a small `AutoRequireCtx`) that the bare `complete_symbols` unit tests pass as `None`. When `require_edit` returns `None` (no ns form), the item is still offered without the edit.

### Trigger character `:`

`CompletionOptions.trigger_characters` becomes `["/", ":"]`. The `:` trigger arrives with an empty `text` and returns the frequency-ranked top 100.

### Testing

- Extractor: `:name` occurrences recorded; `keyword_fqn` tests updated.
- Index (`tests/test_index.rs`): counts after insert, after remove, after a merge that changes a file's keywords, and after `insert_edn_file`.
- Document: `keyword_at` for `:`, `::`, `:ns/`, `::alias/na`, cursor mid-token, and a non-keyword word returning `None`.
- Completion unit tests for each keyword case and for both auto-require entry points, including "already required means no auto-require item".
- e2e in a new fixture file `tests/fixtures/simple_project/src/keywords.clj` with a mix of `:id`, `::local`, `:simple.core/x`, and `::c/thing` through an alias. Tests: `::` completion offers `::local` with the right `textEdit` range; `:` completion offers `:id` first when it is the most frequent; `str/jo` in a file without `clojure.string` offers `str/join` with an `additionalTextEdits` entry inserting `[clojure.string :as str]`; a file that already requires it gets no such edit. The clojure JAR is on the fixture's cached classpath (`test_e2e_completion_from_jar_library` relies on it).
- `bb e2e-nvim` and `bb e2e-pulse` for the trigger-character change and the edits.

## File Structure

Modify:

- `src/index/extractor.rs`: unqualified keyword occurrences.
- `src/index/mod.rs`: `keyword_counts` field, maintenance in the four mutation points, `keyword_counts()`.
- `src/document.rs`: `KeywordContext`, `keyword_at`.
- `src/handlers/completion.rs`: `complete_keywords`, Pool G, auto-require qualified path, `handle` dispatch.
- `src/handlers/code_action.rs`: `namespaces_for_alias` extracted from `candidates`; `CURATED_ALIASES` made `pub(crate)`.
- `src/server.rs`: `trigger_characters`, pass the live text into completion.
- `ARCHITECTURE.md`, `README.md`, `AGENTS.md`, `docs/ROADMAP.md`.
- Tests: `tests/test_extractor.rs`, `tests/test_index.rs`, `src/document.rs` tests, `tests/test_completion.rs`, `tests/test_e2e.rs`, `tests/fixtures/simple_project/src/keywords.clj` (new).

## Tasks

### Task 1: Record unqualified keywords

**Files:**
- Modify: `src/index/extractor.rs`, `ARCHITECTURE.md`
- Test: `tests/test_extractor.rs`

- [x] **Step 1: Write the failing test**
  `test_unqualified_keywords_recorded`: a snippet with `{:id 1 :name "x" ::local 2}` yields occurrences with fqns `:id`, `:name`, and `:<ns>/local`, each spanning the whole token.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_extractor unqualified_keywords`
  Expected: FAIL.

- [x] **Step 3: Implement**
  `keyword_fqn` returns `Some(format!(":{name}"))` for the unqualified, non-auto-resolved case. Update its doc comment, the `Occurrence` doc in `src/index/mod.rs`, and ARCHITECTURE's keyword section. Run the full suite: references tests that counted keyword occurrences may need their expectations adjusted deliberately.

- [x] **Step 4: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Record unqualified keyword occurrences"`

> Deviation: the unqualified case lives in a new `keyword_occurrence_fqn`
> wrapper rather than in `keyword_fqn` itself. `keyword_fqn` is shared with
> `extract_edn` and with the `ig/init-key` definition path, both of which must
> keep rejecting unqualified keywords; only occurrence recording takes the new
> fqn.

> Deviation: the codex review caught that `#:user{:id 1}` reads as `:user/id`,
> so recording a bare `:id` would answer find-references for every unrelated
> `:id`. Fixed in the same task (commit `40396f9`): `walk_ns_map` resolves a
> namespaced map's prefix (`#:user`, `#::`, `#::alias`), qualifies its
> unqualified keys, honors the `:_/x` escape, and stops recording the prefix
> itself as a keyword usage.

### Task 2: Keyword counts in the index

**Files:**
- Modify: `src/index/mod.rs`
- Test: `tests/test_index.rs`

- [x] **Step 1: Write the failing tests**
  `test_keyword_counts_track_insert_remove_merge`: build an `Index`, `insert_file` with occurrences for `:id` twice and `:x/y` once, assert counts; `insert_file` the same path again with the same occurrences, assert the counts did not double; `remove_file`, assert empty; insert again, then `merge_project_from` a new index where the file has `:id` once, assert 1; `insert_edn_file` with `:x/y` twice for one path, assert it counts once.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --test test_index keyword_counts`
  Expected: FAIL (no such method).

- [x] **Step 3: Implement**
  Field, two private helpers `add_keyword_counts(&[Occurrence])` and `sub_keyword_counts(&[Occurrence])`, calls at the four mutation points, and the public read API.

- [x] **Step 4: Run the tests**
  Run: `bb check`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Maintain keyword usage counts in the index"`

> Deviation: `insert_file`, `insert_edn_file` and `merge_project_from` all go
> through one private `replace_occurrences`, the single door onto
> `occurrences.insert`, so no future call site can bypass the counters.

> Deviation: the codex review flagged the decrement/remove pair as racy against
> a concurrent re-index; `sub_keyword_counts` now decrements and drops the zero
> entry under one DashMap entry lock (commit `995d56b`). The same review found
> that a splicing reader conditional inside a namespaced map (`#:user{#?@(…)
> :b :value}`) makes key/value pairing impossible — such a map now falls back to
> the ordinary walk rather than inventing `:user/value`.

### Task 3: Keyword context in the document store

**Files:**
- Modify: `src/document.rs`

- [x] **Step 1: Write the failing tests**
  In `src/document.rs` `mod tests`, next to `test_word_at_utf16_after_emoji`: the six cases from the design, a mid-token case (`:na|me` gives `text == "na"` and `end` past `me`), and a UTF-16 case with an emoji earlier on the line so `start` and `end` are correct UTF-16 columns.

- [x] **Step 2: Run to verify failure**
  Run: `cargo test --lib document::tests::keyword_at`
  Expected: FAIL.

- [x] **Step 3: Implement**
  `KeywordContext` and `keyword_at`, sharing the backward walk with `is_keyword_at` (refactor `is_keyword_at` to call it).

- [x] **Step 4: Run the tests**
  Run: `bb check`
  Expected: PASS.

- [x] **Step 5: Commit**
  `git commit -m "Detect the keyword token under the cursor"`

### Task 4: Keyword completion

**Files:**
- Modify: `src/handlers/completion.rs`, `src/server.rs`
- Test: `tests/test_completion.rs`, `tests/test_e2e.rs`, `tests/fixtures/simple_project/src/keywords.clj`

- [x] **Step 1: Add the fixture file**
  `keywords.clj` in `simple.keywords` namespace, requiring `[simple.core :as c]`, using `:id` three times, `:name` once, `::local` twice, `:simple.core/x` once, and `::c/thing` once. Check that no existing test counts files or symbols in the fixture in a way this breaks (`workspace/symbol` tests search by name).

- [x] **Step 2: Write the failing tests**
  Unit (`complete_keywords` with a hand-built index and `KeywordContext`): `::` empty offers `::local` and `::c/`; `::c/th` offers `::c/thing`; `:` empty offers `:id` first; `:na` offers `:name`. e2e: `test_e2e_completion_keywords_auto_resolved` and `test_e2e_completion_keywords_by_frequency`, asserting labels, `textEdit.range` spanning from the first colon to the token end, and `kind == 14` (Keyword); plus `test_e2e_completion_keyword_mid_token_replaces_whole_token`, which applies the returned `textEdit` to the buffer text in the test and asserts the result contains `:name` once, not `:nameme`. `test_e2e_completion_capabilities` (from the core plan) now expects `triggerCharacters` to contain `":"`.

- [x] **Step 3: Run to verify failure**
  Run: `cargo test --test test_completion keywords && cargo test --test test_e2e completion_keywords`
  Expected: FAIL.

- [x] **Step 4: Implement**
  `complete_keywords`, the dispatch in `handle`, and the `:` trigger in `server.rs`.

- [x] **Step 5: Run the tests**
  Run: `bb check && bb e2e`
  Expected: PASS.

- [x] **Step 6: Commit**
  `git commit -m "Complete keywords from project usage"`

> Deviation: candidates are matched against the whole typed token, marker
> included (`::lo` against `::local`), so the notation prefix-matches instead of
> merely being contained; the two-character fuzzy guardrail still counts only
> what follows the marker. The alias continuation items (`::c/`) carry
> `alias for <ns>` as their detail rather than a use count they do not have.

### Task 5: Auto-require on accept

**Files:**
- Modify: `src/handlers/code_action.rs`, `src/handlers/completion.rs`, `src/server.rs`
- Test: `tests/test_completion.rs`, `tests/test_e2e.rs`

- [x] **Step 1: Extract `namespaces_for_alias`**
  Refactor `candidates` so the ranking (curated, fully qualified, last segment) lives in `namespaces_for_alias` and `candidates` only adds the "defines `name`" filter. Run `bb check`: the add-require tests stay green. Commit: `git commit -m "Share add-require namespace ranking"`.

- [x] **Step 2: Write the failing tests**
  Unit: `test_auto_require_qualified_unknown_alias` (`str/jo` in a namespace without `clojure.string` offers `str/join` with an `additional_text_edits` entry whose text contains `[clojure.string :as str]`, given a source with an ns form); `test_auto_require_bare_prefix_project_ns` (a prefix matching a var in another project namespace offers `alias/name` with the edit); `test_no_auto_require_when_already_required`; `test_no_auto_require_on_alias_collision` (`[other.lib :as str]` present, `str/join` absent); `test_auto_require_sorts_after_in_scope` (an exact auto-require match has a `sort_text` greater than an in-scope prefix match); `test_auto_require_pool_capped`. e2e: `test_e2e_completion_auto_require_inserts_require` on a file in the fixture that lacks `clojure.string`, asserting the `additionalTextEdits` range sits inside the ns form; and the negative case in a file that has it.

- [x] **Step 3: Run to verify failure**
  Run: `cargo test --test test_completion auto_require && cargo test --test test_e2e auto_require`
  Expected: FAIL.

- [x] **Step 4: Implement**
  Pool G, the unknown-alias branch, the live-text parameter, and the `server.rs` call site. Keep the caps and the two-character minimum.

- [x] **Step 5: Run every gate**
  Run: `bb check && bb e2e && bb e2e-nvim && bb e2e-pulse`
  Expected: PASS. In the Pulse run, add a check that accepting a completion item with `additionalTextEdits` applies both edits (VS Code applies them through `vscode.executeCompletionItemProvider` results only on accept; if that is not observable through the API, assert on the item's `additionalTextEdits` field instead and say so in the test).

- [x] **Step 6: Commit**
  `git commit -m "Insert the missing require when completing an unrequired var"`

> Deviation: `complete_symbols` takes the live buffer as a fourth `source:
> Option<&str>` parameter, as the plan allowed; every existing call site passes
> `None`.

> Deviation: the codex review found `require_edit` re-parsing the whole buffer
> once per candidate — up to 30 parses per keystroke on a large file. The
> insertion point is now computed once (`code_action::RequireAnchor`), which the
> add-require action shares. The same review found auto-require items dropping
> the `data` that `completionItem/resolve` needs, so they now carry the symbol
> fqn and its real kind (commit `2373ec3`).

> Deviation: `bb e2e-pulse` grew two checks (keyword completion, and the
> auto-require item's `additionalTextEdits`) against two new fixture files;
> VS Code applies those edits only on accept, which `executeCompletionItemProvider`
> cannot drive, so the assertion is on the item, as the plan allowed.

### Task 6: Docs and roadmap

**Files:**
- Modify: `README.md`, `AGENTS.md`, `docs/ROADMAP.md`

- [x] **Step 1: Update docs**
  README Autocomplete bullet: keywords from project usage in the notation being typed, and auto-require on accept with its scope (project namespaces and the curated aliases). AGENTS.md invariants: unqualified keywords are recorded as `:name` occurrences (navigation ignores them, references include them); `keyword_counts` is maintained at every occurrence mutation point, never scanned. ROADMAP Milestone 2: tick the remaining items and set `Plan:` to `done`. Use /writing-clearly.

- [x] **Step 2: Verify and commit**
  Run: `bb check`
  Expected: PASS.
  `git commit -m "Document keyword completion and auto-require"`

---

## Completed — 2026-09-08

All six tasks are implemented, verified, and on branch
`completion-keywords-auto-require`.

**What shipped.** The extractor records unqualified keywords as `:name`
occurrences (and qualifies the keys of a namespaced map with its prefix); the
index keeps a `keyword_counts` aggregate at every mutation of `occurrences`;
`DocumentStore::keyword_at` reports the keyword token under the cursor;
completion answers a `:`/`::` token with keywords alone, in the notation being
typed, current-namespace first and then by usage; and a var from a namespace
the file has not required is offered as `alias/name` with the `:require` as an
`additionalTextEdits` entry. `CompletionOptions.trigger_characters` is now
`["/", ":"]`. README, AGENTS.md and the ROADMAP say so.

**Verification.** `bb check`, `bb e2e` (136 tests), `bb e2e-nvim`,
`bb e2e-pulse` (two new checks) and `bb e2e-calva` all pass. `bb bench` was run
against metabase and, on the same box, against a `master` control: index time,
per-edit diagnostics and definition latency are unchanged; RSS after project
index rises 287 → 358 MiB, which is the new occurrence data itself. Recorded in
[MEMORY.md](../MEMORY.md).

**Issues encountered.** Every task was reviewed by codex; four rounds found
real defects, all fixed in the same task (see the deviation notes above):
namespaced-map keys were being recorded under the wrong fqn, the keyword-count
decrement raced a concurrent re-index, `require_edit` re-parsed the whole buffer
once per candidate, auto-require items dropped their lazy-documentation `data`,
and Integrant keys were offered as if they were vars. A final branch-wide review
also surfaced the *pre-existing* version of that last bug — the ordinary
completion pools offer `(defmethod ig/init-key ::database …)` as a bare
`database` — which is out of this plan's scope and is now a dated Backlog entry.

**What the plan could have specified better.** It assumed `keyword_fqn` could
be changed in place, but that function is shared with `extract_edn` and the
Integrant definition path, both of which must keep rejecting unqualified
keywords; the unqualified case needed its own wrapper. And it treated "record
unqualified keywords" as a local change to one function, without asking which
*other* readers of a keyword literal exist — namespaced map literals and
splicing reader conditionals both change what a bare `:id` means, and both had
to be handled before the feature was correct.
