# Locals Under `binding`/`let` and Macro Heads in a `for` `:let` Implementation Plan

> **For agentic workers:** Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the two `bb compare` backlog entries the gaps change already resolved — locals inside `(binding […] (let […] …))` and `one-of` in a `for` `:let` — with regression tests, archive them, and record the remaining `local/plain` class as a backlog entry.

**Tech Stack:** Rust, tree-sitter-clojure; tests in `tests/test_extractor.rs`; docs under `docs/`.

---

## Design

### What the re-check established (2026-09-30, master `eb34a94`)

Both entries carried "re-check after the gaps fix". The re-check says the
gaps change (#43, plan `2026-09-18-0751-discards-and-comments-are-gaps.md`)
fixed both, and no server code changes in this plan.

- **Locals inside `(binding […] (let […] …))`.** The mechanism was the
  `;; for backward compatibility …` comment inside the inner `let` vector
  of `run!` (`core.clj:138`), which re-paired every binding after it, and
  the `cfg-dir` name split from its `(cond …)` value across two lines made
  the shifted pairing look like a `binding`-form problem. A scratch test
  with the exact corpus nesting (the `:keys`/`:or`/`:as` params, `let`,
  `binding`, inner `let` with the comment and the split pair) answers
  right on master: `cfg-dir` finds its three usages, the inner `config`
  declares on the inner `let` line with `destructured_key` false and lists
  its three usages, and `copy-configs` finds its one usage. `binding` is
  deliberately not `is_let_like` (its left-hand symbols are var usages),
  so `walk_scope` descends through it generically, which is correct.
- **`one-of` in a `for` `:let`.** Same mechanism: the `;; nested syntax
  quotes …` comment inside the `let` vector at `usages.clj:138` shifted the
  pairs so `(one-of t […])` landed in a name slot, and the `:let`
  rebinding in `namespace.clj:625` looked like the culprit. On master the
  `for` `:let [kw (:k n) kw (one-of kw […])]` shape and the `let` shape
  with a comment before the `(one-of …)` pair both record `one-of` as an
  occurrence of the referred var at every call, and
  `local_references_at` on the head returns `None`.
- **`bb compare clj-kondo` on master** (`.tmp/compare-cljkondo-before.log`,
  2026-09-30): `var-usage/project/macro` 192 of 192 agree,
  `local/destructured` 507 of 507, `local/plain` 585 agree, 3 diverge,
  6 null of 594; total 5249 probes, 4913 agree, 219 diverge, 60 known,
  57 null — the 2026-09-29 numbers exactly. None of the nine `local/plain`
  misses is at a site either entry names.

### What the nine `local/plain` misses are

All nine are method parameters inside `deftype`, `defrecord`,
`extend-protocol` and `extend-type` bodies:

- `parser/clj_kondo/impl/rewrite_clj/node/coerce.clj:60` `(coerce [v] …)`
  under `(extend-protocol NodeCoerceable Object …)`; definition from `:63:7`
  answers null. `:107` `(coerce [sq] …)`: references and rename null.
- `parser/clj_kondo/impl/rewrite_clj/node/protocols.clj:26` `(sexpr [this] this)`;
  `reader_macro.clj:65` `(toString [this] …)`: definition null.
- `parser/clj_kondo/impl/rewrite_clj/node/seq.clj:82`
  `(replace-children [this children'] …)`: references and rename null.
- `inlined/…/tools/reader/reader_types.clj:143` `(get-line-number [reader] …)`
  under a `deftype`: references and rename null.

`walk_scope` reaches the method list `(coerce [v] …)` by generic descent;
its head is neither a core form nor a def, so the argv never binds and
`locals_at_node` has no `v`. `walk_scope_def`'s `Defrecord | Deftype` arm
binds the fields and descends, but never treats a child list as a method
impl, and `extend-protocol`/`extend-type`/`reify` have no arm at all. The
occurrence walker has `walk_method_impl`, so the two disagree. This is a
distinct class and gets a backlog entry, not a fix here.

### Regression tests

Two tests in the `gaps` mod of `tests/test_extractor.rs`, beside
`test_comment_in_let_vector_does_not_shift_pairs`, since the mechanism was
a gap in a binding vector:

1. `test_binding_then_let_with_comment_and_split_pair` — the corpus
   nesting reduced to what matters (an outer `let`, `binding`, an inner
   `let` whose vector has a name on one line and its value on the next,
   then a `;;` comment, then a rebinding of an outer `:keys` name).
   Asserts, through `local_references_at`: the split-pair name's usages;
   the rebinding's declaration is the inner `let` line, `destructured_key`
   is false, and its usages are the lines after it alone; the outer
   `let`'s binding finds its usage inside the inner body.
2. `test_macro_head_in_for_let_and_after_a_comment_is_a_usage` — a
   referred `one-of` used as `(for [c cs :let [kw (:k c) kw (one-of kw […])] :when kw] …)`
   and in a `let` vector where a `;;` comment precedes the
   `unq? (one-of t […])` pair. Asserts the occurrences of `u/one-of` are
   the `:refer` entry plus every call line, and `local_references_at` on
   each call's head is `None`.

Both are pinned to the current behavior; there is no red step, since
nothing is being fixed. The plan still runs them before and after nothing
changes — they pass on master, which is the point.

### Close-out

- The two issue files move to `docs/archive/` (`git mv`) with the status
  line format of the 2026-09-28 close-out:
  `**Status:** fixed 2026-09-18 by the gaps change (plan …); regression test added 2026-09-30 (plan …); archived.`
- ROADMAP: one ticked Milestone 5 item before **Release**, linking both
  archived issues and this plan; the two Backlog sub-lines under the
  2026-09-17 `bb compare` entry are deleted; a new Backlog line (dated
  2026-09-30) links the new method-params issue file.
- `docs/MEMORY.md`: a short dated subsection after the 2026-09-29 one,
  no table — the run reproduced the 2026-09-29 totals; it names the two
  closed classes and the method-params class that is all of `local/plain`.
- New issue file `docs/backlog/2026-09-30-method-params-in-type-bodies-are-not-locals.md`
  in the existing entry shape (Found, Status, Symptom, Evidence, Where to
  look, Verify).
- README and `AGENTS.md` (`CLAUDE.md` symlinks to it) are reviewed in the
  closing change, as working rule 2 asks. No server behavior moved, so the
  expected result is no edit; the commit message records the outcome.

### Gates

`bb check` alone. There is no server change, so no `CACHE_FORMAT_VERSION`
bump and no e2e, bench, soak or compare re-run; the compare run on master
is the closing evidence and is already recorded.

## File Structure

- Modify `tests/test_extractor.rs` — two tests in the `gaps` mod.
- Move `docs/backlog/2026-09-17-locals-under-binding-and-let.md` and
  `docs/backlog/2026-09-17-one-of-in-a-for-let-resolves-to-its-own-line.md`
  to `docs/archive/`, with a new Status line each.
- Create `docs/backlog/2026-09-30-method-params-in-type-bodies-are-not-locals.md`.
- Modify `docs/ROADMAP.md` — Milestone 5 item, Backlog lines.
- Modify `docs/MEMORY.md` — dated compare note.

## Tasks

### Task 1: Start the ROADMAP item

**Files:**
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Add the item** under Milestone 5, directly before
  `- [ ] **Release**`, in the style of its neighbours:
  `- [ ] **Locals under `binding`/`let` and macro heads in a `for` `:let`.**`
  with two or three lines saying both were resolved by the gaps change and
  get regression tests, `Issues:` linking the two files (still under
  `backlog/` for now), `(Backlog, 2026-09-17)`, and a
  `Plan: [2026-09-30-0907-binding-let-locals-and-for-let-macro-heads.md](plans/2026-09-30-0907-binding-let-locals-and-for-let-macro-heads.md) — in progress`
  line (working rule 1 asks for the `in progress` status; Task 6 flips it
  to `done`).

- [ ] **Step 2: Commit**
  `git commit -m "docs: start the binding/let locals and for :let macro-head item"`

### Task 2: Regression test for locals under `binding`/`let`

**Files:**
- Modify: `tests/test_extractor.rs` (the `gaps` mod, after
  `test_comment_in_let_vector_does_not_shift_pairs`)

- [ ] **Step 1: Write the test** `test_binding_then_let_with_comment_and_split_pair`.
  Source, as a raw string so the columns are easy to read:

  ```clojure
  (ns x (:require [h :as hooks]))
  (defn run! [{:keys [config debug copy-configs] :as args}]
    (let [copy-configs (if copy-configs (run! args) copy-configs)]
      (binding [hooks/*debug* debug]
        (let [cfg-dir
              (cond config (config) :else (str "user.dir"))
              ;; a comment inside the binding vector
              config (assoc config :dir cfg-dir)
              classpath (:classpath config)
              config (dissoc config :classpath)]
          [copy-configs cfg-dir config classpath]))))
  ```

  Use the mod's `col` helper for every position. Assert:
  - `local_references_at` at `cfg-dir` on its own line (line 4): usages on
    lines 7 and 10 only.
  - `local_references_at` at the `config` on line 7 (the first inner
    rebinding, `col(src, 7, "config (assoc")`): `declaration.start.line`
    is 7, `destructured_key` is false, usages are lines 8 and 9 (the
    `(:classpath config)` and `(dissoc config …)` reads) — not line 1, not
    line 5 (that `config` reads the `:keys` binding, since the RHS of a
    pair does not see its own LHS), not line 10 (the last rebinding
    shadows it).
  - `local_references_at` at `copy-configs` on line 2 (the outer `let`
    LHS): usages on line 10 alone.
  - `analysis(src).unused_bindings` is empty.

- [ ] **Step 2: Run it**
  Run: `cargo test --test test_extractor gaps::test_binding_then_let -- --nocapture`
  Expected: PASS. If a line number assertion fails, check the corpus
  reproduction in the Design section before touching the walker: the
  expectation above was verified on master with the fuller corpus shape.

- [ ] **Step 3: Commit**
  `git commit -m "test: locals under binding/let with a comment and a split pair"`

### Task 3: Regression test for a macro head in a `for` `:let`

**Files:**
- Modify: `tests/test_extractor.rs` (the `gaps` mod, after Task 2's test)

- [ ] **Step 1: Write the test** `test_macro_head_in_for_let_and_after_a_comment_is_a_usage`.
  Source:

  ```clojure
  (ns x (:require [u :refer [one-of]]))
  (defn f [clauses t]
    (for [c clauses
          :let [kw-node (-> c :children first)
                kw (:k kw-node)
                kw (one-of kw [:require :use])]
          :when kw]
      [kw-node kw])
    (let [quote? (= :quote t)
          ;; nested syntax quotes are treated as normal quoted expressions
          sq? (= :syntax-quote t)
          unq? (one-of t [:unquote :unquote-splicing])]
      [quote? sq? unq?]))
  ```

  Assert on `analysis(src).occurrences` filtered to fqn `u/one-of` (the
  field is `name_range`): the start lines are exactly `[0, 5, 11]` (the
  `:refer` entry and the two calls). Then for lines 5 and 11,
  `local_references_at(src, Position::new(line, col(src, line, "one-of")), "one-of")`
  is `None`. Also assert `analysis(src).unused_bindings` is empty, so the
  `kw` rebinding and `sq?` are read as bound and used.

- [ ] **Step 2: Run it**
  Run: `cargo test --test test_extractor gaps::test_macro_head -- --nocapture`
  Expected: PASS.

- [ ] **Step 3: Commit**
  `git commit -m "test: a referred macro head in a for :let stays a var usage"`

### Task 4: Backlog entry for method params in type bodies

**Files:**
- Create: `docs/backlog/2026-09-30-method-params-in-type-bodies-are-not-locals.md`
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Write the issue file** in the shape of the 2026-09-17
  entries. Title: "Method params in `deftype`/`defrecord`/`extend-*` bodies
  are not locals". Found: 2026-09-30, `bb compare` re-run on the clj-kondo
  corpus while closing the two gap entries; bucket `local/plain`. Status:
  open. Symptom: definition, references and rename on a method
  parameter answer null. Evidence: the six sites from the Design section
  with what each answers (`coerce.clj:63:7` definition → null, expected
  `:60`; `:107:12` references → null, expected `{:107, :108}`;
  `protocols.clj:26:17` and `reader_macro.clj:66:18` definition → null;
  `seq.clj:82:27` references → null, expected `{:82, :83}`;
  `reader_types.clj:143:23` references → null). Where to look:
  `extractor::walk_scope` / `walk_scope_def` — the `Defrecord | Deftype`
  arm descends into a method list generically, and `extend-protocol`,
  `extend-type` and `reify` have no arm; the occurrence walker's
  `walk_method_impl` is the twin to mirror. Verify: a `test_extractor.rs`
  test with `(extend-protocol P Object (m [v] v))` and definition on the
  body `v`; `bb compare` `local/plain` on clj-kondo (3 diverge, 6 null on
  2026-09-30, all this class).

- [ ] **Step 2: ROADMAP Backlog line**, newest last, dated 2026-09-30,
  one sentence with the link:
  `[Method params in type bodies are not locals](backlog/2026-09-30-method-params-in-type-bodies-are-not-locals.md)`
  — the nine `local/plain` answers `bb compare` still flags on clj-kondo.

- [ ] **Step 3: Commit**
  `git commit -m "backlog: method params in deftype/extend-* bodies are not locals"`

### Task 5: Record the compare run

**Files:**
- Modify: `docs/MEMORY.md`

- [ ] **Step 1: Add a subsection** after "After `defmulti`, constructor
  and `:lint-as` declare sites (2026-09-29)":
  `### Re-check of the two gap-resolved local entries (2026-09-30)`.
  Three to five sentences, no table: same corpus, kondo and container on
  master `eb34a94`; every row and the total match the 2026-09-29 table
  (5249 / 4913 / 219 / 60 / 57); the `binding`/`let` and `one-of` entries
  were both the comment-in-a-binding-vector class the gaps change fixed,
  and regression tests now pin the shapes; `var-usage/project/macro` is
  192 of 192 and `local/destructured` 507 of 507; the nine `local/plain`
  misses are all method params in type bodies (link the new backlog file).
  Mention the compare log name is not kept — the numbers are.

- [ ] **Step 2: Commit**
  `git commit -m "docs: record the 2026-09-30 compare re-check"`

### Task 6: Archive and close out

**Files:**
- Move: `docs/backlog/2026-09-17-locals-under-binding-and-let.md` → `docs/archive/`
- Move: `docs/backlog/2026-09-17-one-of-in-a-for-let-resolves-to-its-own-line.md` → `docs/archive/`
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: `git mv` both files** into `docs/archive/`, then replace
  each `**Status:** open.` line with
  `**Status:** fixed 2026-09-18 by the gaps change (plan `docs/plans/2026-09-18-0751-discards-and-comments-are-gaps.md`); regression test added 2026-09-30 (plan `docs/plans/2026-09-30-0907-binding-let-locals-and-for-let-macro-heads.md`); archived.`
  keeping the "Sites were …" tail as the archived entries do. Add one
  sentence under each Symptom saying what the mechanism turned out to be
  (the `;;` comment inside the inner `let` vector at `core.clj:138`; the
  comment at `usages.clj:138`), so a reader does not re-suspect `binding`
  or the `:let` rebinding.

- [ ] **Step 2: ROADMAP** — tick the Milestone 5 item, rewrite its two
  issue links from `backlog/` to `archive/`, append ` — done` to its
  `Plan:` line, and delete the two sub-lines under the 2026-09-17 Backlog
  entry (leave the entry and its other two sub-lines).

- [ ] **Step 3: README and AGENTS.md** (`CLAUDE.md` is a symlink to
  `AGENTS.md`) — working rule 2 asks for both in the closing change. Read
  the README features paragraph and the `AGENTS.md` "Testing notes" and
  the locals/rename invariants against what this plan changed. No server
  behavior moved, so the expected outcome is that neither needs a word;
  if a sentence there turns out to describe the old, wrong behavior, fix
  it. Say in the commit message which of the two changed and why the
  other did not.

- [ ] **Step 4: `bb check`**
  Run: `bb check`
  Expected: fmt clean, clippy clean, all tests pass (including the two
  new ones and `compare_simple_project`).

- [ ] **Step 5: Commit**
  `git commit -m "docs: archive the binding/let locals and for :let one-of issues (README, CLAUDE.md unchanged)"`
