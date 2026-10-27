# Match or Beat clojure-lsp on Every Bench Speed Row — Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Make clj-pulse at least on par with clojure-lsp on the two `bb bench` speed rows it loses today, definition latency and keystroke → diagnostics, by caching each open document's analysis per revision and cutting the change debounce from 300 ms to 50 ms.

**Tech Stack:** Rust, tower-lsp, tree-sitter, dashmap, tokio; `bb bench` / `bb e2e` / `bb soak` / `bb compare` harnesses.

---

## Design

### Where we stand

The published Mac tables ([PERFORMANCE.md](../PERFORMANCE.md)) win every
startup row by 10–100× and lose two:

| Row | clj-pulse | clojure-lsp |
|---|---|---|
| Definition, metabase | 11 ms | 6 / 8 ms |
| Definition, clj-kondo | 7 ms | 1 ms |
| Keystroke → diagnostics, metabase 251 KiB | 570 / 585 ms | 418 / 379 ms |
| Keystroke → diagnostics, clj-kondo 233 KiB | 524 ms | 298 ms |

**Keystroke row: the gap is debounce policy, not clj-kondo.** Our change
debounce is `DIAGNOSTIC_DEBOUNCE_MS = 300` (`src/server.rs:2071`), and the
bench times it. clojure-lsp's pinned release (2026.07.06-14.34.19,
`cli/src/clojure_lsp/server.clj:42-43`) debounces changes by 50 ms and
diagnostics by 25 ms. That leaves clojure-lsp about 75 ms of debounce plus
about 223 ms of work, against our 300 ms plus about 224 ms. The lint work is
the same. MEMORY.md and PERFORMANCE.md blame per-keystroke process start for
the difference, and that explanation is wrong. Expected after the change:
about 274 ms against 298 ms.

**Definition row: per-request re-extraction.** Every position request runs
`extractor::extract_full_tree` over the whole open buffer
(`references::resolve_fqn_at`, `src/handlers/references.rs:875`), and that is
nearly the whole request. `extract_analysis_tree` (`src/index/extractor.rs:362`)
is a pure function of (tree, text, path, `ExtractConfig`) and reads no index
state, so its result can be cached. This is the 2026-09-10 backlog item
"Cache the extraction per document version". This plan promotes it.

### Decisions

1. **The analysis cache lives in the document store, next to the tree.** The
   store already keeps one tree per open document in step with its rope
   (eager, never lazy). The cached `Analysis` follows the same discipline:
   - Each `Doc` gets a `rev: u64`, taken from one process-wide `AtomicU64` on
     `open` and on every `apply_changes`, whether the sync is incremental or
     full. Because revisions are global, a close and reopen can never reuse a
     revision a slow request is still holding.
   - `apply_changes` takes the new revision and clears the cache slot
     *before* it mutates the rope. A batch can fail part-way (a later change
     with an out-of-range position returns `Err` after earlier ones were
     applied), and the text it leaves must never carry the old revision.
   - `Snapshot` carries the `rev` it was taken at. `Snapshot::parse`, used
     outside the store, gets `rev: 0`, a value the store never issues.
   - The cache slot is `Option<CachedAnalysis { rev, cfg: Arc<ExtractConfig>,
     analysis: Arc<Analysis> }>`.
   - **Hit:** the slot's `rev == snapshot.rev` and `Arc::ptr_eq(cfg)`.
   - **Miss:** compute from the snapshot's own tree and text *outside* the
     DashMap lock, since a 452 KiB walk is about 20 ms and would block the
     shard and `did_change`. Then store the result only if the document's
     current `rev` still equals `snapshot.rev`. Two concurrent misses may both
     compute; the second store is identical and harmless, so there is no
     dedup.

   Staleness is ruled out by construction. Every input is in the key: tree
   and text through `rev`, config through the `Arc` pointer, and the path is
   fixed per URI. The cache never answers for a revision other than the
   caller's snapshot.

   ```rust
   // src/document.rs
   pub struct Snapshot { pub text: String, pub tree: Tree, pub rev: u64 }
   impl DocumentStore {
       /// The analysis of exactly `snapshot`, from the cache when this
       /// document's slot holds `snapshot.rev` under `cfg`, computed (and
       /// stored, if the document has not moved on) otherwise.
       pub fn analysis(&self, uri: &Url, snapshot: &Snapshot, path: &Path,
                       cfg: &Arc<ExtractConfig>) -> Result<Arc<Analysis>>;
   }
   ```

2. **`ExtractConfig` is shared as `Arc`.** `Index.extract_config` becomes
   `RwLock<Arc<ExtractConfig>>`, `extract_config()` returns
   `Arc<ExtractConfig>`, and `set_extract_config` wraps the new value in a
   fresh `Arc`. A config reload therefore invalidates every cached analysis
   through `ptr_eq`. Because the cache holds the `Arc`, the pointer cannot be
   freed and reused (no ABA). Call sites written `&index.extract_config()`
   still compile through deref coercion.

3. **Every open-buffer extraction reads the cache.** This covers
   `resolve_fqn_at`, `occurrence_range_at`, `alias_target`, `alias_capture`,
   `keyword_target`'s open-buffer loop and the remaining `extract_full_tree`
   calls on snapshots in `references.rs`, plus `highlight.rs`, `symbols.rs`,
   `code_action.rs:87` and the lint pass. Callers borrow from
   `Arc<Analysis>`. Where `with_referred_fqns` may rewrite occurrences, the
   result is a `Cow<[Occurrence]>`, borrowed when unchanged. The two
   `src/server.rs` paths that *index* an open buffer (`did_open` at about line
   2559, the config-change re-index at about line 2851) keep calling
   `extract_full_tree`. They need owned values for `insert_file` and run once
   per event, so they gain nothing. `extract_full_tree` itself stays for them
   and for tests.

4. **The lint pass shares the cache and stands down when stale.**
   - `diagnostics::compute_tree` is split. A new
     `compute_from(tree, source, path, analysis: &Analysis, stale: &dyn Fn() -> bool) -> Option<Vec<Diagnostic>>`
     does the work. `compute_tree` keeps its signature for tests and calls
     `compute_from` with a freshly extracted analysis and `&|| false`.
   - `compute_from` checks `stale()` between its stages: after the
     qualified-usage scan, after `unused_requires_tree`, and after
     `duplicate_requires_tree`. It returns `None` when the pass is stale.
   - In `lint_and_publish_doc`, the blocking closure gets the analysis through
     `documents.analysis(..)` for the pass's own snapshot. Its `stale` closure
     is `current_version != Some(version) || lint_epoch != epoch`.
   - `lint_and_publish_doc` and `spawn_lint_pass` already hold
     `Arc<DocumentStore>`, so the closure can capture a clone. Switch
     `lint_and_publish_doc` to take `&Arc<DocumentStore>` if it does not
     already.
   - A `None` from the native tier ends the pass without publishing. The
     post-join staleness check would have discarded it anyway.

   Why it matters: aborting the task drops a `spawn_blocking` handle but
   cannot stop the work. With a 50 ms debounce, superseded native passes
   would otherwise run to completion on every keystroke.

5. **The change debounce drops to 50 ms, clojure-lsp's own value.** The
   constant moves into the library crate as `pub const CHANGE_DEBOUNCE_MS: u64 = 50;`
   in `src/diagnostics.rs`, so the bench can read it. `src/server.rs` uses it
   and drops its private constant. The accepted cost was discussed: squiggles
   show up at shorter pauses, and while you type, clj-kondo processes start
   and get killed. Bursts within 50 ms still collapse: auto-pair, Enter plus
   indent, multi-cursor edits, held-key repeat.

6. **The bench shows the debounce next to the keystroke rows.** clj-pulse's
   value comes from `clj_pulse::diagnostics::CHANGE_DEBOUNCE_MS`, so it can't
   drift from the binary. clojure-lsp's is a `CLOJURE_LSP_DEBOUNCE_MS = 75`
   constant in `tests/test_bench.rs`, with a comment citing the upstream file
   and tag, beside `clojure-lsp-version` in `bb.edn`. Each server's keystroke
   section prints a `  debounce` row, and `BENCH_JSON` gains `debounce_ms`.
   The published tables write the keystroke cell as `N ms (D ms debounce)`
   for both servers.

7. **No `CACHE_FORMAT_VERSION` / `JarCacheEntry::format_version` bump.**
   Extractor output is unchanged.

### Expected effect and acceptance

- **Definition median:** at or below clojure-lsp's warm median on both
  corpora, on the Linux baseline box.
- **Keystroke → diagnostics:** at or below clojure-lsp's on both corpora.
- **RSS settled:** no more than about 10 MiB above the baseline. One cached
  `Analysis` per open document; the bench opens a handful.
- **If a target is still unmet** after Task 8, the work still ships: the
  cache and the debounce are improvements on their own. The ROADMAP item
  stays `in progress`, MEMORY.md records the remaining gap and its measured
  cause, and Task 11 ticks the item only when the Mac rows meet the targets.
- **If definition still loses after the cache,** Task 8 profiles the
  remainder. Likely culprits: three `rope.to_string()` copies per request
  (`symbol_position`, `local_definition`, `resolve_fqn_at`), and the locals
  walk in `local_definition`. Act only on what the profile shows.

### Testing

- **Document store unit tests:**
  - same revision and config → `Arc::ptr_eq` hit;
  - incremental edit, full-sync edit or config swap → miss, with a result
    equal (`PartialEq`, derived on `Analysis`) to a fresh
    `extract_analysis_tree` of the new text;
  - a snapshot taken before an edit computes the old text's analysis and does
    not store it;
  - a batch that fails part-way leaves no cache hit: the next snapshot has a
    new `rev`, and its analysis equals a fresh extraction of the text the
    batch left;
  - close and reopen: a snapshot from before the close neither hits nor
    populates the reopened document's slot;
  - `Snapshot::parse` (rev 0) computes and never stores.
- **`compute_from`:** returns `None` when `stale` is already true, and equals
  `compute_tree` when it is never true.
- **Gates** (the table in AGENTS.md): `bb check`, `bb e2e`, `bb e2e-pulse`
  (diagnostics timing is client-visible), `bb e2e-calva` (the definition
  path changed), `bb soak` (document-store change),
  `bb compare` (the references and rename paths changed), `bb bench` before
  and after.

## File Structure

- `src/document.rs`: `Doc.rev`, the global revision counter,
  `Snapshot.rev`, the cache slot, `DocumentStore::analysis`.
- `src/index/mod.rs`: `extract_config` as `RwLock<Arc<ExtractConfig>>`.
- `src/index/extractor.rs`: `#[derive(Debug, PartialEq)]` on `Analysis`.
- `src/diagnostics.rs`: `CHANGE_DEBOUNCE_MS`, `compute_from`, with
  `compute_tree` as a wrapper.
- `src/server.rs`: the debounce constant removed and the lint pass moved to
  the cache plus staleness check.
- `src/handlers/references.rs`, `highlight.rs`, `symbols.rs`,
  `code_action.rs`: snapshot extractions moved to `DocumentStore::analysis`.
- `tests/test_bench.rs`: the debounce row and `debounce_ms`.
- `src/document.rs` `#[cfg(test)] mod tests` (line ~424): the cache tests.
- `src/diagnostics.rs` `#[cfg(test)] mod tests` (line ~217): the
  `compute_from` tests.
- Docs: `docs/MEMORY.md`, `docs/PERFORMANCE.md`, `README.md`,
  `docs/ROADMAP.md`, `AGENTS.md`.

## Tasks

### Task 0: Register the item

**Files:** `docs/ROADMAP.md`

- [x] **Step 1:** Add to Milestone 1, after "Keep clj-kondo off the keystroke
  path", the item **Match or beat clojure-lsp on every bench speed row**, with
  `Plan: [2026-10-27-1800-match-clojure-lsp-speed.md](plans/2026-10-27-1800-match-clojure-lsp-speed.md) — in progress`.
  Delete the 2026-09-10 backlog entry "Cache the extraction per document
  version", which this item promotes.
- [x] **Step 2: Commit.** `git commit -am "roadmap: schedule matching clojure-lsp on every bench speed row"`

### Task 1: Baseline bench

**Files:** none (log under `.tmp/`)

> Baseline (Linux box, master `babc206`, 1 run): metabase warm def 31 ms, 452 KiB edit 388 ms, 251 KiB edit 895 ms, RSS 462 MiB; clj-kondo warm def 31 ms, edit 818 ms, RSS 96 MiB. clojure-lsp warm: def 12 / 6 ms, edit 1386 / 916 ms (metabase / clj-kondo corpus). On this box clj-pulse's keystroke rows already beat clojure-lsp; only definition lost.

- [x] **Step 1: Record the baseline on master.**
  Run: `CLJ_PULSE_BENCH_RUNS=1 bb bench 2>&1 | tee .tmp/bench-baseline.log`
  Expected: both corpora complete, with `BENCH_JSON` lines for clj-pulse and
  clojure-lsp. Note the definition medians, keystroke medians and RSS settled
  for both servers. These are the numbers to beat on this box.

### Task 2: Share `ExtractConfig` as `Arc`

**Files:**
- Modify: `src/index/mod.rs` (struct field, `extract_config`,
  `set_extract_config`, constructor)
- Modify: any call site the compiler rejects. Most `&index.extract_config()`
  calls coerce unchanged.

- [x] **Step 1: Implement.** Change the field to `RwLock<Arc<ExtractConfig>>`.
  `extract_config()` returns `Arc<ExtractConfig>` (`.clone()` of the Arc).
  `set_extract_config(cfg)` stores `Arc::new(cfg)`.
- [x] **Step 2: Verify.** Run: `bb check`. Expected: PASS.
- [x] **Step 3: Commit.** `git commit -am "index: share ExtractConfig as Arc"`

### Task 3: Per-revision analysis cache in the document store

> Deviation: tests and implementation were written together, so the "confirm they fail" step was not run red-first. `ExtractConfig` is imported from `crate::index`, not `extractor`.

**Files:**
- Modify: `src/document.rs`, `src/index/extractor.rs` (derive on `Analysis`)
- Test: `src/document.rs` `mod tests`

- [x] **Step 1: Write the failing tests** (see Testing in the Design): hit
  with `ptr_eq`; miss after an incremental edit, a full-sync edit and a
  config swap, each equal to a fresh `extract_analysis_tree`; a pre-edit
  snapshot is computed but not stored, so the next current snapshot is not
  `ptr_eq` to it; a batch whose second change has an out-of-range position
  leaves no hit; a snapshot from before `close` + `open` neither hits nor
  populates the new slot; `Snapshot::parse` never stores.
- [x] **Step 2: Run them to confirm they fail.**
  Run: `cargo test --lib document::tests::analysis`. Expected: compile failure or FAIL.
- [x] **Step 3: Implement.**
  - Add `static NEXT_REV: AtomicU64` starting at 1 (0 belongs to
    `Snapshot::parse`).
  - Add `Doc.rev`, set from the counter in `open` and at the *start* of
    `apply_changes`, before the first mutation. Add
    `Doc.analysis: Option<CachedAnalysis>`, cleared in both places at the
    same point.
  - `snapshot()` copies `rev`.
  - `analysis()` per Decision 1. Look up under `self.docs.get(uri)` and
    release the guard before computing. Store under `get_mut` only when
    `doc.rev == snapshot.rev`.
  - Derive `Debug, PartialEq` on `Analysis`.
- [x] **Step 4: Run the tests.** Run: `cargo test --lib document::tests`. Expected: PASS.
- [x] **Step 5: Run `bb check`.** Expected: PASS.
- [x] **Step 6: Commit.** `git commit -am "document: cache each open document's analysis per revision"`

### Task 4: Handlers read the cached analysis

> Deviation: `occurrences_for` keeps each open buffer's occurrences as a small `LiveOccs` enum (cached `Arc<Analysis>` or a rewritten copy) so a buffer is not cloned; its Clojure path now reads the cache too (EDN still uses `file_occurrences_tree`). `symbols.rs` and `live_definition_range` pass `uri::to_index_path(uri)` so every caller gives the cache the same path for a URI.

**Files:**
- Modify: `src/handlers/references.rs`, `src/handlers/highlight.rs`,
  `src/handlers/symbols.rs`, `src/handlers/code_action.rs`

- [x] **Step 1: Find the calls.**
  Run: `grep -n "extract_full_tree" src/handlers/*.rs`.
  Every call whose input is a `documents.snapshot(..)` moves to
  `documents.analysis(uri, &snapshot, &path, &index.extract_config())`.
  Leave any call on disk text alone.
- [x] **Step 2: Implement.**
  - Borrow `analysis.ns_meta`, `.symbols` and `.occurrences` instead of
    destructuring owned tuples.
  - Where `with_referred_fqns(..).unwrap_or(occs)` needs ownership, use
    `Cow<[Occurrence]>`, borrowed by default.
  - Where a caller consumed `syms.into_iter()` (`keyword_target`), iterate by
    reference and clone the `name_range` (it is `Copy`).
- [x] **Step 3: Verify.** Run: `bb check && bb e2e`. Expected: PASS.
- [x] **Step 4: Commit.** `git commit -am "handlers: answer from the cached analysis"`

### Task 5: Lint pass on the cache, with a staleness check

> Deviation: the blocking closure shares one `Arc<Snapshot>` with the kondo tier (no second copy of the text). `compute_from` and the closure both skip non-Clojure sources, so an EDN file is never extracted.

**Files:**
- Modify: `src/diagnostics.rs`, `src/server.rs` (`lint_and_publish_doc`)
- Test: `src/diagnostics.rs` `mod tests`

- [x] **Step 1: Write the failing tests.** `compute_from` with
  `&|| true` → `None`. With `&|| false` → `Some`, equal to `compute_tree` on
  a fixture that triggers all five native codes.
- [x] **Step 2: Run them to confirm they fail.** Expected: compile failure.
- [x] **Step 3: Implement `compute_from`** per Decision 4. `compute_tree`
  extracts the analysis and calls `compute_from(.., &|| false)`, unwrapping
  the result (it cannot be `None`).
- [x] **Step 4: Wire the lint pass.** The `spawn_blocking` closure captures a
  clone of the `Arc<DocumentStore>`, the `uri`, the snapshot, `version`,
  `epoch` and the config `Arc`. It calls `documents.analysis(..)` and then
  `compute_from(.., &stale)`. A `None` result returns early with no publish.
  A pass whose analysis errors keeps today's behavior: an empty native set.
- [x] **Step 5: Verify.** Run: `bb check && bb e2e`. Expected: PASS.
- [x] **Step 6: Commit.** `git commit -am "lint: reuse the cached analysis and stop superseded native passes"`

### Task 6: 50 ms change debounce

**Files:**
- Modify: `src/diagnostics.rs` (add `pub const CHANGE_DEBOUNCE_MS: u64 = 50;`
  with a doc comment naming clojure-lsp's 50 + 25 ms), `src/server.rs`
  (remove `DIAGNOSTIC_DEBOUNCE_MS`, use the new constant)
- Modify: `tests/test_e2e.rs:5692`. Keep the fixed 900 ms wait, but reword
  the comment to "well past the debounce". A shorter wait would only weaken
  the stand-down check.

- [x] **Step 1: Implement.**
- [x] **Step 2: Hunt for timing assumptions.**
  Run: `grep -n "debounce\|from_millis" tests/test_e2e.rs tests/common/mod.rs`.
  Check every test that sends two changes and expects one publish.
- [x] **Step 3: Verify, twice.** Run: `bb check && bb e2e`, then `bb e2e`
  again. Expected: PASS both times. A test that fails only sometimes is a
  timing assumption, so fix it rather than retry.
- [x] **Step 4: Commit.** `git commit -am "diagnostics: debounce changes by 50 ms, as clojure-lsp does"`

### Task 7: Bench shows each server's debounce

**Files:**
- Modify: `tests/test_bench.rs`

- [x] **Step 1: Implement** per Decision 6. Add a `  debounce` row under each
  server's keystroke rows, and `"debounce_ms"` in the `BENCH_JSON` object
  next to `edit_ms`. Add `CLOJURE_LSP_DEBOUNCE_MS = 75` with a comment:
  upstream `cli/src/clojure_lsp/server.clj`, `change-debounce-ms 50` +
  `diagnostics-debounce-ms 25`, at the tag in `bb.edn` `clojure-lsp-version`.
  The comment should also say to re-check this when that pin moves.
- [x] **Step 2: Verify.** Run: `bb check`. Expected: PASS. The bench itself is
  `#[ignore]`d and runs in Task 8.
- [x] **Step 3: Commit.** `git commit -am "bench: show each server's change debounce"`

### Task 8: Measure, and profile what is left

> Result (Linux box, 3 warm runs, medians): metabase def <1 ms vs clojure-lsp 18, 452 KiB edit 138 ms vs 1332, 251 KiB edit 700 vs 995, RSS 462 vs 462 MiB; clj-kondo corpus def <1 ms vs 6, edit 547 vs 759, RSS 95 vs 96 MiB. Every target met, so Step 2 (profiling) was not needed. The bench's `as_millis` resolution reports sub-millisecond definitions as 0.

**Files:** none unless Step 2 applies

- [x] **Step 1: Run the bench.**
  Run: `CLJ_PULSE_BENCH_RUNS=3 bb bench 2>&1 | tee .tmp/bench-after.log`.
  Expected, on both corpora against the clojure-lsp warm column of the same
  run: definition median ≤; keystroke median ≤; RSS settled within about
  10 MiB of Task 1. Every startup row should sit within Task 1's spread.
- [x] **Step 2: Profile only if definition still loses.**
  - Time the parts of one `definition::handle` on the bench's edit target:
    `symbol_position`, `local_definition`, `resolve_fqn_at`. Use temporary
    `Instant` logging, not committed.
  - If the `rope.to_string()` copies dominate, cache the materialized text
    per `rev` as `Arc<str>` in `Snapshot`, in the same slot discipline as
    Decision 1. If the locals walk dominates, record that in MEMORY.md and
    stop: narrowing it touches the walker the compare gate defends, and
    belongs in its own plan.
  - Re-run Step 1 after any change, and commit it as its own change.

### Task 9: Gates

> Passed: `bb soak`, `bb soak penpot`, `bb compare` (clj-kondo 4754/36/231/11 and penpot 6409/198/380/366, identical to the MEMORY.md tables), `bb e2e-pulse`, `bb e2e-calva`. `bb e2e` ran twice after the debounce change. `bb e2e-nvim` was not run (no protocol change).

- [x] **Step 1:** Run `bb soak`, then `bb soak penpot`. Expected: PASS, with
  no divergence and RSS growth under 1.5×.
- [x] **Step 2:** Run `bb compare`. Expected: zero *new* divergences against
  the MEMORY.md tables (the server's answers are unchanged by design).
- [x] **Step 3:** Run `bb e2e-pulse` and `bb e2e-calva`. Expected: PASS. If the
  box can't run one, say so in the hand-off rather than skipping silently.

### Task 10: Docs

> Deviation: README is untouched, because its table still shows the Mac rows and its "clojure-lsp answers a definition faster" sentence goes with them in Task 11. PERFORMANCE.md labels the Mac tables as measured before the change. The ROADMAP item records the Linux result and stays `in progress`.

**Files:**
- Modify: `docs/MEMORY.md`, `docs/PERFORMANCE.md`, `README.md`,
  `docs/ROADMAP.md`, `AGENTS.md`

- [x] **Step 1: `docs/MEMORY.md`.**
  - Add a dated section with the Task 8 tables (Linux box, before and after),
    in the existing table shape.
  - Fix the "process start is the difference" sentences in the macOS
    section, and the 452 KiB paragraph, to state the debounce split.
  - Update the "That gap is per-request re-analysis" bullet to say it is now
    cached, and what is left.
- [x] **Step 2: `docs/PERFORMANCE.md`.**
  - Rewrite the keystroke bullet: both servers debounce (50 ms; 50 + 25 ms),
    and the lint work is comparable.
  - Rewrite the definition bullet: the answer now comes from a per-revision
    analysis, like clojure-lsp's stored one.
  - Leave the Mac tables for Task 11, but label them with the clj-pulse
    version they measured (0.5.4).
- [x] **Step 3: `docs/ROADMAP.md`.** If Task 8 left a target unmet, add the
  measured gap to the item's text, and keep it `in progress`.
- [x] **Step 4: `AGENTS.md`.**
  - Extend the document-store invariant: each open document also caches its
    `Analysis` per revision (a global `rev`) and config `Arc`. Handlers and
    the lint pass read `DocumentStore::analysis` for a snapshot, never
    `extract_full_tree` on one; indexing an open buffer is the exception.
  - Note in the `:kondo {:live-max-kb}` invariant that a change pass waits
    `CHANGE_DEBOUNCE_MS` (50 ms), and that a superseded native pass stops at
    its next staleness check.
- [x] **Step 5: Commit.** `git commit -am "docs: record the analysis cache, the 50 ms debounce and their bench"`



## Completion summary (Tasks 0–10)

**Implemented.** `ExtractConfig` is shared as an `Arc`; each open document caches its `Analysis` per global revision and config pointer; the handlers and the lint pass read it; superseded native lint passes stop between stages; the change debounce is 50 ms (`diagnostics::CHANGE_DEBOUNCE_MS`); the bench prints each server's debounce and `debounce_ms`. On the Linux baseline box, definition is under 1 ms (was 28–31 ms, clojure-lsp 6–18 ms) and keystroke → diagnostics is 138 / 547 ms (was 388 / 818, clojure-lsp 1332 / 759), with RSS within noise. All gates passed.

**Issues.** None blocking. Two background `bb bench` runs overlapped once early on (killed and restarted before any number was taken).

**Deviations.** Gathered above, per task: no red-first run in Task 3; `LiveOccs` and the path normalisation in Task 4; the shared `Arc<Snapshot>` and non-Clojure guards in Task 5; profiling skipped in Task 8; README deferred to Task 11 in Task 10. The session task list tool was not available in this environment, so this document was the only tracker.

**What the plan could have specified better.** The Task 1 baseline should have said the keystroke rows already win on the Linux box (the gap is a Mac-only observation), and Task 10 should have said which README edits wait for Task 11.

### Task 11: Published tables (maintainer, on the Mac)

> Done 2026-10-27 from the maintainer's Mac run: every definition and keystroke row at or below clojure-lsp's, ROADMAP item ticked. A first Mac run read clj-pulse's metabase memory erratically (88–545 MiB); a second run on a quiet machine read 524–555 MiB, and the tables were re-recorded from it.

Execution stops here. The published tables are the maintainer's Mac alone,
and only the maintainer can run them.

- [x] **Step 1:** On the Mac, run `CLJ_PULSE_BENCH_RUNS=3 bb bench`.
- [x] **Step 2:** Update the README Performance table, both PERFORMANCE.md
  tables and the macOS section of MEMORY.md. Write keystroke cells as
  `N ms (D ms debounce)`. Drop README's "clojure-lsp answers a definition
  faster once settled" sentence if the new row no longer shows it.
- [x] **Step 3:** If every definition and keystroke row is at or below
  clojure-lsp's, tick the ROADMAP item and set its status to `done`. Commit
  that together with the README change, as the working rules require.
  Otherwise, leave the item `in progress`, write the remaining gap into it,
  and commit the tables alone.
