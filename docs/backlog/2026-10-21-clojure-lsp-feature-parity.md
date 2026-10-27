# Feature parity with clojure-lsp is not measured

- **Found:** 2026-10-21, in a discussion of how to measure parity with
  clojure-lsp.
- **Status:** open.

## Problem

Three gates compare clj-pulse with something else, and none of them compares
features. `bb bench` measures performance against clojure-lsp. `bb compare`
checks the answers of definition, references and rename against clj-kondo's
analysis. Nothing records which clojure-lsp features clj-pulse lacks, which
gaps are deliberate, or when a new clojure-lsp release adds something. The
ROADMAP's "Not shipped" line is the only list, and it is written by hand.

Breadth is the gap. For shared features, depth is already covered: clojure-lsp
answers definition, references and rename from the same clj-kondo analysis
that `bb compare` uses as its oracle.

## Proposed fix

Measure breadth against the **daily workflow**, not against everything
clojure-lsp does. Two pieces:

1. **`docs/PARITY.md`, a matrix.** One row per clojure-lsp capability,
   `executeCommand` command and experimental method. Each row has a class,
   *daily*, *occasional* or *excluded (reason)*, plus a status: *shipped*,
   *planned (ROADMAP milestone)* or *gap*. The score is the number of shipped
   daily rows over all daily rows.
2. **`bb parity`, a drift check.** Start the pinned clojure-lsp the way
   `bb bench` already does (`tests/test_bench.rs:85`, binary under
   `.tmp/bench/tools/`), send `initialize`, and fail when a capability or
   command it advertises has no row in `PARITY.md`. A clojure-lsp bump then
   adds rows instead of slipping by unnoticed. Send a realistic client
   capabilities payload: the findings below came from an empty one, and
   clojure-lsp may advertise more when the client declares support.

### Findings: clojure-lsp 2026.07.06-14.34.19

These come from its `initialize` response, run in a scratch `deps.edn`
project with empty client capabilities. It advertises 24 server capabilities
and 62 commands.

**Excluded, paredit and structural editing (18).** Not wanted at all; editors
do this natively (see Not planned in the ROADMAP): `forward`, `backward`,
`forward-select`, `backward-select`, `forward-slurp`, `backward-slurp`,
`forward-barf`, `backward-barf`, `kill-sexp`, `raise-sexp`, `drag-forward`,
`drag-backward`, `drag-param-forward`, `drag-param-backward`,
`move-coll-entry-up`, `move-coll-entry-down`, `change-coll`, `cycle-coll`.

**Excluded, not needed (6):** `thread-first`, `thread-first-all`,
`thread-last`, `thread-last-all`, `unwind-thread`, `unwind-all`.

**Excluded, already Not planned in the ROADMAP:** `callHierarchyProvider`,
`linkedEditingRangeProvider`, the `cursor-info` and `server-info` commands,
and the experimental `testTree`, `projectTree`, `cursorInfo` and `serverInfo`
(Calva custom APIs).

**Daily: capabilities**

| Row | Status |
|---|---|
| definition, references, rename, hover, completion, signature help, document/workspace symbols, highlight, selection range, code action, text sync | shipped |
| experimental `clojuredocs` | shipped as `clojurePulse/clojureDocs` |
| `implementationProvider` | gap, Milestone 4 |
| `codeLensProvider` (reference count) | gap, Milestone 4, off by default |
| `workspace.fileOperations` `willRename`/`didRename` | gap, Milestone 4 (`willRenameFiles`) |
| `declarationProvider` | gap; check what clojure-lsp answers (we expect the alias or refer in the ns form) before deciding it is daily |

**Daily: ns hygiene commands (11)**

| Command | Status |
|---|---|
| `clean-ns` | shipped (sorting requires is Milestone 4) |
| `add-missing-libspec`, `add-require-suggestion` | shipped as the Add require quickfix; confirm it covers the suggestion variant |
| `add-missing-import`, `add-import-to-namespace` | gap, already in the ROADMAP Backlog ("Add missing import") |
| `as-to-refer`, `refer-to-as` | gap |
| `replace-refer-all-with-alias`, `replace-refer-all-with-refer` | gap |
| `suppress-diagnostic` | gap |
| `resolve-macro-as` | gap; would write `:lint-as` into the clj-kondo config, which clj-pulse only reads today |

**Daily: refactor commands (11), all gaps:** `extract-function`,
`extract-to-def`, `inline-symbol`, `introduce-let`, `move-to-let`,
`expand-let`, `create-function`, `cycle-privacy`, `destructure-keys`,
`restructure-keys`, `create-test`.

**Occasional (14), outside the daily score:** `promote-fn`, `demote-fn`,
`if->cond-refactor`, `cond->if-refactor`, `get-in-all`, `get-in-less`,
`get-in-more`, `get-in-none`, `sort-map`, `sort-clauses`, `move-form`,
`move-to-for-let`, `inline-function`, `cycle-keyword-auto-resolve`.
`foldingRangeProvider` belongs here too: the ROADMAP Backlog dropped it on
2026-09-08 because every priority editor folds without it. So does
`semanticTokensProvider`: Tier 1 shipped and was retired on 2026-07-01
([plan](../plans/2026-07-01-semantic-tokens-tier1.md)), and the ROADMAP lists
it under Best effort as the lowest of the chrome items.

So about 3 of the 22 daily commands are covered, and most of the gap is the
refactor row. clj-pulse has no `executeCommandProvider` at all. The ROADMAP's
Best effort section already holds "Refactor set as `executeCommand`" with a
shared "edit a form, preserve formatting" helper to build first. Scoring the
matrix makes that row's size visible, which is the input for deciding whether
to promote it into a milestone.

### Open questions

- **Whole-document formatting** (`documentFormattingProvider`,
  `documentRangeFormattingProvider`). Decided 2026-10-27: a *daily* row,
  status *gap* (not yet implemented in the server). A native
  cljfmt-compatible formatter sits under "Best effort" in the ROADMAP, and
  Clojure Pulse formats client-side for now.
- **Where the matrix lives:** its own `docs/PARITY.md` (proposed), or a
  section of `docs/FEATURES.md`.

## Origin

Discussion on 2026-10-21 (no plan yet). Three measures were weighed: a
feature matrix with a generated drift check (chosen), clojure-lsp as a second
oracle in `bb compare` for completion and code actions (deferred), and
hand-scripted workflow scenarios (rejected as curated and prone to drift).
The denominator is the daily workflow from the ROADMAP's Direction section.
The user excluded paredit-style features and the `thread-*`/`unwind-*`
commands.
