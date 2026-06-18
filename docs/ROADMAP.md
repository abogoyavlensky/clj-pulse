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

- [x] Add-missing-require code action — the most-used clojure-lsp
      refactoring; the namespace index needed to power it already exists.
- [ ] Clean ns.
- [ ] Sort requires.
- [ ] Completion: auto-require on accept (`additionalTextEdits`), locals
      (params, `let` bindings), keywords, fuzzy matching.
- [ ] Reference count code lens (free once Phase 2 lands).

## Phase 4 — diagnostics

Deliberately after references/rename: clj-kondo covers linting well in the
meantime and keeping the server dependency-free is worth more early on.

- [x] Native unresolved-namespace lint — warns on qualified usages whose
      prefix isn't required (debounced, powers the add-require lightbulb).
- [ ] Native fallback lints: unused require, unresolved (unqualified) symbol.
- [ ] clj-kondo bridge — shell out to a `clj-kondo` binary when present,
      translate JSON findings to LSP diagnostics.

## Phase 5 — broader project support (adoption)

- [x] let-go support with lgx (~/.lgx/gitlibs) deps resolver — indexes `.lg`
      project files and resolves lgx git/`:local/root` deps (transitive,
      first-wins) for navigation. let-go built-in `core` nav now supported when
      `:lg-version` is pinned (see below).
- [x] Clojure protocols support: navigation to protocol's method, navigation
      from map->DB to DB protocol — protocol method signatures are indexed as
      namespace-level vars (so definition/hover/completion/references reach
      them); `->X`/`map->X` resolve to the `defrecord`/`deftype` `X` via a
      resolve-time fallback. Method *implementations* in
      `defrecord`/`deftype`/`extend-type`/`extend-protocol`/`reify` now navigate
      to the protocol's declaration too. The reverse ("find implementations")
      is not in scope.
- [ ] resolve and navigate to libs that required with common syntax:
      ```[flock.staff.spec
          [common :as c]
          [helpers :as h]]```
- [x] Transitive Clojure deps navigation — definition, references, hover, and
      signature work from inside `jar:` (and `file:` dir-dep) library files, so
      navigation reaches transitive dependencies. References from a library
      symbol cover project usages plus currently-open library files (the
      occurrence index stays project-only).
- [ ] Keyword indexing — navigation/rename for namespaced keywords. + Navigation on Integrant keys from integratn system edn file to components
- [ ] Download docs for built-in functions from https://clojuredocs.org/
- [ ] Custom macros definitions (example `defcomponent` from flockman)
- [ ] Java interop (class navigation/completion, decompilation, stubs). (if possible)
- [x] let-go core navigation — when `:lg-version` is pinned in `lgx.edn` and
      lgx has fetched the source, indexes let-go's built-in `core`/stdlib from
      `$LGX_HOME/let-go/source/<version>/pkg/rt/core/*.lg` and navigates into the
      real `.lg` source. Each stdlib namespace is registered under both its
      let-go name and the `clojure.*` alias (`[clojure.string :as str]` →
      `string.lg`), and bare builtins (`map`, `when`) resolve to `core`. Built-ins
      with no `.lg` source — compiler special forms (`if`, `try`, `catch`, …) and
      native core fns (`count`, `subs`, `throw`, …, implemented in Go) — get
      hover descriptions and completion entries (special forms from a curated
      table; natives from a generated name list with docs borrowed from the
      clojure.core table), but don't navigate, since no `.lg` definition exists.
- [X] Install with my homebrew-tap repo.
- [x] Leiningen classpath (`project.clj` / NO `lein classpath`) — inspects
      `project.clj` only (no java): masks strings/comments then EDN-parses just
      the `:dependencies`/`:source-paths`/`:test-paths`/`:local-repo` vectors,
      so metadata (`^…`) and regex (`#"…"`) elsewhere don't break it. Maps
      direct deps (top-level + profiles) to existing `~/.m2`/`:local-repo` JARs
      and reuses the classpath JAR indexer. Used only when there is no
      `.cpcache`. Transitive deps deferred (see below).
- [ ] shadow-cljs classpath and cljs-aware indexing.
- [ ] Keyword indexing for re-frame subs
- [ ] Local cache for project's files

## Out of scope for now

- Formatting — Calva ships and defaults to its own formatter.
- The full clojure-lsp refactoring suite (extract function, inline symbol,
  thread/unthread, move-to-let, …) — each is its own project.
- Semantic tokens, call hierarchy, protocol implementations, Calva custom
  APIs (`clojure/serverInfo`, test tree, project tree), `.lsp/config.edn`
  settings system, persistent project analysis cache.
