# Feature parity with clojure-lsp

[Back to README](../README.md)

What clojure-lsp offers that clj-pulse does and does not, taken from the
`initialize` response of clojure-lsp 2026.07.06-14.34.19 (24 capabilities and
62 commands; a realistic VS Code client payload returns the same set as an
empty one). The matrix is kept by hand: when a feature ships or the pinned
clojure-lsp moves, update it.

- **Class.** *daily*: part of the everyday workflow the
  [ROADMAP](ROADMAP.md) targets. *occasional*: useful, outside the daily
  score. *excluded*: deliberately not offered, with the reason.
- **Status.** *shipped*; *planned* (scheduled in a ROADMAP milestone, named in
  the note); *gap* (not scheduled); *n/a* (excluded rows).

**Score: 16 of 42 daily items shipped** — 13 of 20 capabilities and 3 of 22
commands. Most of the gap is the refactoring commands. Speed is compared
separately, in [Performance](PERFORMANCE.md).

## Capabilities

| Feature | Class | Status | Notes |
|---|---|---|---|
| `textDocumentSync`, `completionProvider`, `hoverProvider`, `signatureHelpProvider`, `definitionProvider`, `referencesProvider`, `documentHighlightProvider`, `documentSymbolProvider`, `workspaceSymbolProvider`, `renameProvider`, `selectionRangeProvider`, `codeActionProvider` | daily | shipped | |
| `experimental.clojuredocs` | daily | shipped | As `clojurePulse/clojureDocs`. |
| `implementationProvider` | daily | planned | Milestone 4. |
| `codeLensProvider` | daily | planned | Milestone 4: reference counts, off by default. |
| `workspace.fileOperations.willRename` | daily | planned | Milestone 4, `workspace/willRenameFiles`: rewrite the `ns` form and requires when a file moves. |
| `workspace.fileOperations.didRename` | daily | gap | The ROADMAP schedules only `willRenameFiles`. |
| `documentFormattingProvider`, `documentRangeFormattingProvider` | daily | gap | The editor formats today (Clojure Pulse bundles cljfmt). A cljfmt-compatible formatter in the server is on the ROADMAP's best-effort list. |
| `executeCommandProvider` | daily | gap | clj-pulse offers no commands; see the tables below. |
| `declarationProvider` | occasional | gap | Not checked what clojure-lsp answers; definition covers the daily case. |
| `foldingRangeProvider` | occasional | gap | Dropped from the ROADMAP on 2026-09-08: every priority editor folds from its own grammar. |
| `semanticTokensProvider` | occasional | gap | Shipped and retired on 2026-07-01 ([plan](plans/2026-07-01-semantic-tokens-tier1.md)); the lowest of the best-effort items. |
| `callHierarchyProvider`, `linkedEditingRangeProvider` | excluded | n/a | ROADMAP, Not planned. |
| `experimental.testTree`, `experimental.projectTree`, `experimental.cursorInfo`, `experimental.serverInfo` | excluded | n/a | Calva custom APIs; ROADMAP, Not planned (clj-pulse answers `clojure/dependencyContents`). |

## Namespace commands

| Feature | Class | Status | Notes |
|---|---|---|---|
| `clean-ns` | daily | shipped | As the Clean namespace code action. Sorting requires is Milestone 4. |
| `add-missing-libspec`, `add-require-suggestion` | daily | shipped | As the Add require quickfix; not checked that it covers every suggestion variant. |
| `add-missing-import`, `add-import-to-namespace` | daily | gap | ROADMAP backlog: "Add missing import". |
| `as-to-refer`, `refer-to-as` | daily | gap | |
| `replace-refer-all-with-alias`, `replace-refer-all-with-refer` | daily | gap | |
| `suppress-diagnostic` | daily | gap | |
| `resolve-macro-as` | daily | gap | Would write `:lint-as` into the clj-kondo config, which clj-pulse only reads today. |

## Refactoring commands

All gaps. The ROADMAP's best-effort "Refactor set as `executeCommand`" holds
the plan, starting with a shared "edit a form, preserve formatting" helper.

| Feature | Class | Status | Notes |
|---|---|---|---|
| `extract-function`, `extract-to-def`, `inline-symbol`, `introduce-let`, `move-to-let`, `expand-let`, `create-function`, `cycle-privacy`, `destructure-keys`, `restructure-keys`, `create-test` | daily | gap | |
| `promote-fn`, `demote-fn`, `if->cond-refactor`, `cond->if-refactor`, `get-in-all`, `get-in-less`, `get-in-more`, `get-in-none`, `sort-map`, `sort-clauses`, `move-to-for-let`, `inline-function`, `cycle-keyword-auto-resolve` | occasional | gap | |

## Excluded commands

| Feature | Class | Status | Notes |
|---|---|---|---|
| `forward`, `backward`, `forward-select`, `backward-select`, `forward-slurp`, `backward-slurp`, `forward-barf`, `backward-barf`, `kill-sexp`, `raise-sexp`, `drag-forward`, `drag-backward`, `drag-param-forward`, `drag-param-backward`, `move-coll-entry-up`, `move-coll-entry-down`, `change-coll`, `cycle-coll` | excluded | n/a | Paredit-style structural editing; editors do it natively (ROADMAP, Not planned). |
| `move-form` | excluded | n/a | ROADMAP, Not planned: drag and move-form commands. |
| `thread-first`, `thread-first-all`, `thread-last`, `thread-last-all`, `unwind-thread`, `unwind-all` | excluded | n/a | Not needed. |
| `cursor-info`, `server-info` | excluded | n/a | Calva custom APIs; ROADMAP, Not planned. |

## Only in clj-pulse

Not clojure-lsp capabilities, so outside the matrix: `ns` insertion for new
files (`workspace/didCreateFiles`), on-type formatting (indent on Enter),
Integrant key navigation, [let-go](https://github.com/nooga/let-go) and lgx
support, and monorepo project discovery.
