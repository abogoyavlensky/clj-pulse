# Goal: Clean-ns code action

> Status: ✅ Achieved 2026-06-23 · Created: 2026-06-23
> Brief for an autonomous `/goal` session — read it in full first. Launch with the condition at the bottom.

## Target state
The server offers a "Clean namespace" code action (kind `source.organizeImports`) for any Clojure
source file. When the file's `ns` form contains requires the file does not use, applying the action
returns a `WorkspaceEdit` that removes the unused entries — leaving every used require, the file's
own namespace, and surrounding code untouched. It mirrors the existing "Add require" quickfix: a
pure analysis plus a tree-sitter-driven edit, returned directly in the code-action response. No new
diagnostic, no `executeCommand`, no cross-file changes.

## Fixed decisions
- **Exposed as a code action, kind `source.organizeImports`, returning a `WorkspaceEdit` in the response** — matches the existing `src/handlers/code_action.rs` pattern; the editor applies the edit. No `executeCommand`/`workspace/applyEdit` infrastructure is added.
- **Title "Clean namespace"** — stable string the e2e test and editors match on.
- **Removal only, surviving order preserved, no sorting** — the cleaned `ns` keeps its remaining entries in their original order; no alphabetical reordering or reformatting of untouched lines.
- **What counts as "unused" (the removal set):** an `:as` alias with no `alias/…` usage in the file body; individual `:refer` names never used as bare symbols (prune just the unused names; drop the whole entry only if it then has neither an `:as` nor any surviving refer); exact duplicate require entries. The namespace being defined is never removed.
- **What is kept:** any require whose alias or refer is used; plain `[some.ns]` requires that carry no `:as` and no `:refer` (treated as possibly side-effecting — see Assumptions); `:require` entries inside reader conditionals when usage is ambiguous.
- **Offered only when it changes something** — if nothing is unused, return no clean-ns action (no no-op edit). Respect `context.only`: don't let the clean-ns source action and the existing add-require quickfix shadow each other.
- **Reuse the existing seams** — usage detection builds on `extractor::qualified_usages` (and bare-symbol usage for refers); the edit re-parses the `ns` form with tree-sitter like `require_edit` does (positions from tree-sitter/ropey, never manual byte math). Stay within the `Index` public API.
- **If `NsMeta` layout changes** (e.g. adding per-require ranges), bump `JarCacheEntry::format_version` per CLAUDE.md — JAR mtimes never change, so stale caches survive otherwise.

## Assumptions
- Plain `[some.ns]` requires with no `:as`/`:refer` whose `some.ns/…` never appears are **left in place** (side-effecting loads — multimethods, protocol/`defmethod` registration — would break if removed). If you decide clojure-lsp-style removal of these is wanted instead, flag it before doing so.
- `.cljc` reader-conditional requires are handled conservatively: when it's not clear a require is unused across all branches, keep it. If this proves too conservative for a real case, flag it.
- Macro-introduced or string/`eval`-based usages can't be seen by static analysis; a require used only that way may look unused. Acceptable for v1 — note it if it bites a realistic fixture.

## Non-goals
- No Java `:import` handling (no removal or sorting of imported classes).
- No alphabetical sorting or reformatting of surviving requires.
- No `executeCommand` `clean-ns` command and no server-initiated `workspace/applyEdit`.
- No new `unused-namespace`/`unused-require` diagnostic or squiggle.
- No cross-file edits — only the current file's `ns` form.
- No removal of the `(:require …)`-less `ns` form's other clauses (`:gen-class`, `:refer-clojure`, etc.).

## Acceptance criteria
- [ ] `bb check` passes (fmt + clippy `-D warnings` + all unit tests).
- [ ] `bb e2e` passes, including a new test in `tests/test_e2e.rs` that opens a file with an unused require, requests `textDocument/codeAction`, finds an action whose `kind` is `source.organizeImports` and title contains "Clean", and asserts its `WorkspaceEdit` removes the unused require while keeping a used one.
- [ ] Unit tests in `src/handlers/code_action.rs` cover: unused `:as` alias removed; used alias kept; unused `:refer` name pruned while a sibling used refer survives; exact duplicate require dropped; the file's own namespace never removed; a `.cljc` reader-conditional require is not corrupted; and no action is offered when nothing is unused.
- [ ] The action is returned only when cleaning would change the file (no no-op edits), and applying it produces valid, parseable Clojure.
- [ ] The existing "Add require" quickfix and `unresolved-namespace` behavior are unchanged (their tests still pass).

## Verification
Run these, show the passing output, and end with a final diff summary before claiming done.
- `bb check` — fmt, clippy `-D warnings`, and all unit tests (proves the analysis + edit logic and no regressions).
- `bb e2e` — spawns the real binary and speaks LSP over stdio; the new code-action test proves the user-visible behavior the way an editor triggers it.
- Show a concrete before/after: a sample `ns` with an unused require, the returned action's title/kind, and the resulting `ns` after the edit is applied.
- Recommended regression check (not a gate): `bb e2e-nvim` to confirm the added code action doesn't disturb the real-editor-client path.

Before claiming done, restate each acceptance criterion in the conversation with the evidence that satisfies it — the `/goal` evaluator judges only the transcript, not this file.

## Review policy
- After verification passes, run the **review-with-codex** skill on the diff.
- Fix real correctness / API / edge-case / coverage findings; keep the implementation minimal (no unrequested scope — e.g. don't slip in sorting or import handling).
- Ignore pure style nits unless they reveal a real problem. Up to 4 review/fix rounds.

## Escalation policy
- Decide local implementation details yourself (naming, internal structure, how usage detection is wired — anything reversible).
- Ask me only if: the public code-action shape needs to change from the Fixed decisions; removing plain side-effecting requires turns out to be wanted (an Assumption flips); the goal conflicts with existing behavior; or two viable approaches differ in user-facing semantics.
- Stop and ask when blocked (missing dependency, repeated verification failure, contradictory requirement) — don't guess past a blocker.

## Result
✅ Achieved 2026-06-23.

**What shipped** (all in `src/handlers/code_action.rs`, tests in `tests/test_e2e.rs`):
- A "Clean namespace" code action, kind `source.organizeImports`, returned with a `WorkspaceEdit` in the response — no `executeCommand`/`applyEdit`. The existing "Add require" quickfix was factored into `add_require_actions`; both now gate on `context.only` via a new `kind_allowed` helper.
- `clean_ns_edits(source)` does index-free analysis + a tree-sitter `ns`-form rebuild: removes libspecs with an unused `:as` alias and no fully-qualified use, prunes unused `:refer` names (dropping the `:refer`/whole spec when emptied), and drops exact-duplicate specs. Surviving specs keep their original order/formatting; an emptied `(:require …)` clause is removed while `(ns …)` is preserved. Returns `None` when nothing changes (no no-op action).
- Conservative safety guards: plain side-effecting `[some.ns]`/bare requires and reader-conditional specs are left untouched; libspecs with unmodeled options (`:rename`, …) are left untouched.

**Deviations / additions beyond the literal brief:**
- An emptied `(:require)` clause is removed entirely (cleaner than leaving `(:require)`); `(ns …)` itself is never touched.
- **review-with-codex** ran four rounds, each finding resolved with a regression test:
  - R1 (P1, must-fix): aliases used *only* in auto-resolved keywords (`::s/foo`) or namespaced maps (`#::s{…}`) were invisible to `qualified_usages` (symbols only) and would be wrongly removed, breaking `clojure.spec`-style files. Fixed with `collect_keyword_prefixes`, feeding keyword/ns-map namespace prefixes into `used_prefixes`. This only ever *prevents* removals, so it cannot cause a wrong removal.
  - R2 (P2): an `ns` form may carry multiple `(:require …)` clauses; only the first was cleaned. Refactored into `clean_one_clause` looping over all clauses, sharing `used_prefixes`/`used_bare`/`seen` (so duplicates collapse across clauses too).
  - R3 (P2): duplicate detection keyed on the *pre-pruning* spec text, so two specs made identical by refer-pruning left a duplicate needing a second pass. Now dedupes on the *final emitted* text — idempotent in one pass (asserted by a test running clean twice).
  - R4: no issues found.

**Evidence:**
- `bb check` — green (fmt + clippy `-D warnings` + all unit tests; 31 `code_action` unit tests incl. keyword-alias, `:rename`, multi-clause, cross-clause dedupe, and idempotency regressions).
- `bb e2e` — `68 passed; 0 failed; 1 ignored`, including the new `test_e2e_clean_ns_removes_unused_require` (drives `textDocument/codeAction` with `only: ["source.organizeImports"]`, applies the returned edit, asserts the unused `clojure.string` require is gone and `[simple.helpers :as helpers]` + body remain).

No Assumptions from the brief proved false: plain side-effecting requires and reader conditionals are kept as planned; the side-effect-removal Assumption was not flipped.

## `/goal` launcher
```
/goal Implement docs/goals/2026-06-23-clean-ns-code-action.md to its Target state. Done when: you have restated each Acceptance criterion from that file in this conversation with the evidence that satisfies it, `bb check` and `bb e2e` are shown passing here, the review-with-codex skill has run with must-fix items resolved, and the file's Status + Result are updated. If genuinely blocked, ask me — that counts as done. Or stop after 30 turns.
```
