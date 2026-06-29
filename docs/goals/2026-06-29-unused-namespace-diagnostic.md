# Goal: Unused-namespace diagnostic

> Status: ✅ Achieved 2026-06-29 · Created: 2026-06-29
> Brief for an autonomous `/goal` session — read it in full first. Launch with the condition at the bottom.

## Target state
The server publishes a warning diagnostic for every namespace a Clojure source file `:require`s in
its `ns` form but never uses. Open a file whose `ns` requires `[clojure.string :as str]` with no
`str/…` (or fully-qualified `clojure.string/…`) usage, and `textDocument/publishDiagnostics`
carries an `unused-namespace` warning over that require; a require whose alias, fully-qualified
name, or any `:refer`'d name *is* used produces nothing. It is the squiggle counterpart to the
existing "Clean namespace" code action — the same files it flags are the ones that action removes
as unused — and rides the existing pure, index-free `diagnostics::compute` pipeline. No change to
`unresolved-namespace`, no new quickfix, no cross-file analysis.

## Fixed decisions
- **Emitted from `src/diagnostics.rs::compute`, alongside `unresolved-namespace`** — `compute` stays the single, pure, index-free entry point that `server.rs` already calls; the new diagnostics are concatenated into its returned `Vec<Diagnostic>`. The analysis needs only the file's own `ns` form plus body usage, so it stays index-free (no false positives without a classpath).
- **Diagnostic shape: `code` = `"unused-namespace"`, `severity` = `WARNING`, `source` = `"clj-pulse"`, `tags` = `[DiagnosticTag::UNNECESSARY]`** — `unused-namespace` is the clj-kondo / clojure-lsp linter name and pairs with the existing `unresolved-namespace`; `UNNECESSARY` makes editors fade the require like clojure-lsp does, which is the idiomatic "unused" UX (distinct from an error squiggle). Message in the existing style, naming the namespace (e.g. `"Unused namespace: clojure.string"` or clojure-lsp's `"namespace clojure.string is required but never used"`).
- **"Unused" means exactly what "Clean namespace" removes for being unused** — reuse the usage analysis already in `src/handlers/code_action.rs` (the `Plan::Remove`-for-usage path in `plan_libspec`, built on `extractor::qualified_usages`, `collect_keyword_prefixes`, and bare-symbol usage for `:refer`). A require is unused iff its `:as` alias is unused **and** it has no `clojure.string/…`-style fully-qualified use **and** none of its `:refer` names are used. Sharing one analysis is what guarantees the diagnostic and the clean-ns fix never disagree — the same invariant the codebase already keeps between `unresolved-namespace` and `candidates`/`resolves_prefix`. Factor the shared decision out rather than copying it.
- **Conservative bias — keeping a require is always safe, flagging a used one is not.** When usage is ambiguous, do not flag. Concretely, never flag: plain side-effecting requires (`[some.ns]` / bare `some.ns` with no `:as`/`:refer` — they may load `defmethod`/Integrant/protocol registrations, which this project cares about); requires inside reader conditionals; libspecs with options we don't model (`:rename`, etc.); a require used only through an auto-resolved keyword (`::alias/x`) or namespaced map (`#::alias{…}`); and the file's own namespace.
- **Ranges come from tree-sitter/ropey via `extractor::point_to_position`, never manual byte math** — per the project invariant. EDN config files (`deps.edn`/`project.clj`/`lgx.edn`) are skipped, same as `unresolved-namespace` already does via `is_clojure_source`.
- **If `NsMeta`'s layout changes** (e.g. you add per-require ranges to it), bump `JarCacheEntry::format_version` per CLAUDE.md — JAR mtimes never change, so stale caches survive otherwise. Prefer computing ranges from the live tree (as clean-ns does) so no layout change is needed.

## Assumptions
- **Diagnostic code is `unused-namespace`** (not `unused-require`). If you'd rather match the ROADMAP's "unused require" wording for the `code`, flag it — it's the user-visible identifier.
- **Squiggle covers the required namespace symbol** (e.g. `clojure.string` inside `[clojure.string :as str]`). Covering the whole libspec instead is acceptable if simpler — flag the choice if it affects a test assertion.
- **Macro- / `eval`- / string-introduced usages** can't be seen by static analysis, so a require used only that way may look unused (same limitation clean-ns already accepts). Acceptable for v1 — note it if a realistic fixture trips on it.

## Non-goals
- No diagnostic for individual unused `:refer`'d vars within an otherwise-used require (clj-kondo's `unused-referred-var`) — this goal is whole-namespace only, matching the user's "unused required namespaces" framing.
- No `duplicate-require` diagnostic — "Clean namespace" still dedupes; that is a separate concern from "unused".
- No new bound quickfix / `executeCommand` to remove the require — the existing "Clean namespace" source action is the fix. (A diagnostic-bound "Remove unused require" quickfix is a reasonable future follow-up, out of scope here.)
- No flagging of unused `:import`ed Java classes.
- No change to the `unresolved-namespace` diagnostic, the add-require quickfix, or the clean-ns action's behavior.

## Acceptance criteria
- [ ] `bb check` passes (fmt + clippy `-D warnings` + all unit tests).
- [ ] `bb e2e` passes, including a new test in `tests/test_e2e.rs` that opens a file whose `ns` requires an unused namespace and a used one (the `[clojure.string :as str]` + `[simple.helpers :as helpers]` shape already used by `test_e2e_clean_ns_removes_unused_require` / the `simple_project` fixture), waits for `publishDiagnostics`, and asserts: an `unused-namespace` diagnostic with `severity` 2 (WARNING) whose message names the unused namespace, and **no** `unused-namespace` diagnostic for the used one.
- [ ] Unit tests in `src/diagnostics.rs` cover: unused `:as` alias flagged; used alias not flagged; namespace used fully-qualified (`[clojure.set]` + `clojure.set/union`) not flagged; plain side-effecting `[some.ns]` / bare require not flagged; `:refer` with all names unused flagged, with a used name not flagged; alias used only via `::alias/kw` (or `#::alias{}`) not flagged; reader-conditional require not flagged; the file's own namespace never flagged.
- [ ] Consistency: the set of requires flagged `unused-namespace` is a subset of what the "Clean namespace" action removes, and matches it exactly for the unused-reason cases (duplicates excluded). Demonstrate on one shared example.
- [ ] Existing diagnostics and code-action tests still pass unchanged (`unresolved-namespace`, add-require, clean-ns, and the no-diagnostics-on-EDN tests).

## Verification
Run these, show the passing output, and end with a final diff summary before claiming done.
- `bb check` — fmt, clippy `-D warnings`, and all unit tests (proves the analysis and no regressions).
- `bb e2e` — spawns the real binary and speaks LSP over stdio; the new test proves the user-visible squiggle the way an editor receives it. Per CLAUDE.md, a server-behavior change is not done until `bb e2e` passes.
- Show a concrete before/after: a sample `ns` with one unused and one used require, and the `unused-namespace` diagnostics `compute` returns for it (count, code, severity, message, range).
- Recommended regression check (not a gate): `bb e2e-nvim` to confirm the real-editor-client path still publishes diagnostics cleanly.

Before claiming done, restate each acceptance criterion in the conversation with the evidence that satisfies it — the `/goal` evaluator judges only the transcript, not this file.

## Review policy
- After verification passes, run the **review-with-codex** skill on the diff.
- Fix real correctness / API / edge-case / coverage findings, especially any false-positive flag (a used require flagged unused) — keep the implementation minimal (no unrequested scope: no refer-var lint, no duplicate lint, no new quickfix).
- Ignore pure style nits unless they reveal a real problem. Up to 4 review/fix rounds.

## Escalation policy
- Decide local implementation details yourself (how the shared usage analysis is factored, internal naming, exact range, message wording — anything reversible).
- Ask me only if: the diagnostic `code`/shape should differ from the Fixed decisions; flagging plain side-effecting requires turns out to be wanted (a conservative-bias call flips to a clojure-lsp-style behavior change); the goal conflicts with existing behavior; or two viable approaches differ in user-facing semantics.
- Stop and ask when blocked (repeated verification failure, contradictory requirement) — don't guess past a blocker.

## Result

✅ Achieved 2026-06-29.

**What shipped**
- `diagnostics::compute` now emits an `unused-namespace` warning (severity WARNING, source `clj-pulse`, message `Unused namespace: <ns>`, tagged `DiagnosticTag::UNNECESSARY` so editors fade it) for every required namespace the file never uses, alongside the existing `unresolved-namespace`. It stays pure and index-free, and the `did_open` / debounced `did_change` paths in `server.rs` publish it unchanged.
- The "is this require unused?" decision was factored out of `code_action.rs::plan_libspec` into a shared `Libspec` parser + `libspec_unused` predicate, and exposed as `pub fn unused_requires(source) -> Vec<UnusedRequire>` (namespace + range of the namespace symbol). The diagnostic and the "Clean namespace" action now read libspecs through the same code, so the squiggle flags exactly what the fix removes.
- Conservative by construction: plain side-effecting requires (`[some.ns]` / bare `some.ns`), reader-conditional specs, libspecs with options we don't model (`:rename`, …), alias-only-keyword usage (`::alias/x`, `#::alias{…}`), and the file's own namespace are never flagged.

**Evidence**
- `bb check` — passing (fmt + clippy `-D warnings` + all unit tests). New unit tests in `src/diagnostics.rs` cover unused `:as` / `:refer`, used alias / fully-qualified / refer, keyword-only alias use, reader-conditional, unmodeled option, no-require-clause, self-require, and a multi-spec "flag only the unused" case.
- `bb e2e` — `72 passed; 0 failed; 2 ignored`, including the new `test_e2e_unused_namespace_diagnostic` (opens `scratch.clj` requiring an unused `clojure.string` + used `simple.helpers`; asserts one `unused-namespace` WARNING tagged UNNECESSARY naming `clojure.string`, and none for `simple.helpers`).
- Consistency demonstrated by `unused_requires_agree_with_clean_ns_removal` and `self_require_is_neither_flagged_nor_removed` in `code_action.rs`: the flagged set equals what Clean-namespace removes.
- Second-opinion review via `review-with-codex` (scope: uncommitted). One P2 finding — the file's own namespace could be falsely flagged — was resolved by adding a self-namespace guard to the shared analysis (threaded the declaring ns name into both `plan_libspec` and `unused_requires` via a new `ns_name_of` helper), which also makes the Clean-namespace action honor its own "namespace being defined is never removed" invariant.

**Deviation from the contract**
- The contract scoped the self-namespace guard to the diagnostic; resolving the codex finding in the shared layer additionally hardened the pre-existing "Clean namespace" action so it, too, never removes a self-require. This is a small, test-covered correction that keeps the squiggle and the fix consistent — no behavior change for any non-self-require input. No assumptions proved false.

## `/goal` launcher
```
/goal Implement docs/goals/2026-06-29-unused-namespace-diagnostic.md to its Target state. Done when: you have restated each Acceptance criterion from that file in this conversation with the evidence that satisfies it, `bb check` and `bb e2e` are shown passing here (including a new e2e test asserting an `unused-namespace` WARNING for an unused require and none for a used one), the review-with-codex skill has run with must-fix items resolved, and the file's Status + Result are updated. If genuinely blocked on a public-API/semantics fork from the Escalation policy, ask me — that counts as done. Or stop after 12 turns.
```
