# Roadmap

Goal: a fast, dependency-free Clojure LSP server that covers the daily
clojure-lsp workflow in Calva. Read-and-navigate features are done; the
roadmap is about the "understand usages and change code" half.

## Done

- Go-to-definition: project files, JAR libraries (`jar:` URIs +
  `workspace/textDocumentContent`), git/`:local/root` deps, require
  aliases and namespaces, clojure.core builtins.
- Hover with docstrings and signatures (project, libraries, curated core).
- Completion: current ns, `:refer`s, alias-qualified, alias names,
  namespace names, clojure.core.
- Signature help with multi-arity and rest/destructuring/type-hint support.
- Indexing: tree-sitter extraction, parallel scan, per-JAR disk cache,
  classpath discovery from `.cpcache` (deps.edn), project symbols always
  win over library symbols.
- Headless e2e infrastructure: stdio harness, real Maven classpath,
  headless Neovim, real VS Code + Calva under Xvfb.

## Phase 1 — quick wins

- [x] `textDocument/documentSymbol` — outline view; the index already has
      names, kinds, and ranges per file.
- [x] `workspace/symbol` — fuzzy search over the symbol index (Cmd+T).
- [x] UTF-16 position handling — `word_at` treats LSP positions as char
      offsets; non-ASCII lines resolve the wrong word.

## Phase 2 — occurrence index, references, rename

The core investment. The index stores only definitions today; an
occurrence index records every resolved symbol usage per file and unlocks
most of what follows.

- [x] Occurrence index (usages resolved through aliases/refers, updated
      on save/open like definitions).
- [x] `textDocument/references`.
- [x] `textDocument/rename` — cross-file `WorkspaceEdit` built on
      references.
- [x] `workspace/didChangeWatchedFiles` — keep the index correct on git
      pulls and branch switches, not just editor saves.

## Phase 3 — editing assistance

- [ ] Add-missing-require code action — the most-used clojure-lsp
      refactoring; the namespace index needed to power it already exists.
- [ ] Clean ns / sort requires.
- [ ] Completion: auto-require on accept (`additionalTextEdits`), locals
      (params, `let` bindings), keywords, fuzzy matching.
- [ ] Reference count code lens (free once Phase 2 lands).

## Phase 4 — diagnostics

Deliberately after references/rename: clj-kondo covers linting well in the
meantime and keeping the server dependency-free is worth more early on.

- [ ] clj-kondo bridge — shell out to a `clj-kondo` binary when present,
      translate JSON findings to LSP diagnostics.
- [ ] Native fallback lints from our own index (unresolved
      symbol/namespace, unused require) for setups without clj-kondo.

## Phase 5 — broader project support (adoption)

- [ ] let-go support with lgx (~/.lgx/gitlibs) deps resolver
- [ ] Keyword indexing — navigation/rename for namespaced keywords
      (re-frame subs, Integrant keys).
- [ ] Download docs for built-in functions from https://clojuredocs.org/
- [ ] Leiningen classpath (`project.clj` / NO `lein classpath`) — not used by
      the maintainer, but required for wider adoption. Do not run java at all - inspect project.clj.
- [ ] shadow-cljs classpath and cljs-aware indexing.

## Out of scope for now

- Formatting — Calva ships and defaults to its own formatter.
- The full clojure-lsp refactoring suite (extract function, inline symbol,
  thread/unthread, move-to-let, …) — each is its own project.
- Java interop (class navigation/completion, decompilation, stubs).
- Semantic tokens, call hierarchy, protocol implementations, Calva custom
  APIs (`clojure/serverInfo`, test tree, project tree), `.lsp/config.edn`
  settings system, persistent project analysis cache.
