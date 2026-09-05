# Roadmap — toward the public release

The active plan. Earlier roadmaps live in [archive/](archive/) and are frozen.

## Direction

clj-pulse is a single, fast, dependency-free binary that covers the daily
Clojure workflow. The gap to close is *reliability of what is shipped* first,
then the editor features users notice as missing. Guiding decisions:

- **Editors, in priority order:** the Clojure Pulse VS Code extension
  ([clojure-pulse-vscode](https://github.com/abogoyavlensky/clojure-pulse-vscode)),
  Calva, Neovim. Zed is best effort.
- **Linting is clj-kondo's job.** The native lints stay a fallback that also
  powers the add-require and clean-ns actions. No new native linters; clj-kondo
  stays a subprocess, never a bundled runtime.
- **Formatting is client-side for now.** Clojure Pulse bundles cljfmt (compiled
  to JavaScript) for Format Document and indent-on-Enter. A native,
  cljfmt-compatible formatter in the server is wanted eventually for Neovim and
  Zed, but it is not a priority.
- **ClojureScript is best effort.** `.cljs` files are indexed and `js/` is
  known, but `:require-macros` and shadow-cljs classpaths are not; fix cheap
  things, don't build a cljs stack.
- **Protect the differentiators:** instant startup, no JVM on the hot path,
  Integrant and let-go/lgx support. No item may regress them.

## Working rules

1. **Starting an item:** write its plan under `docs/plans/` and put the link on
   the item's `Plan:` line, with status `in progress`.
2. **Finishing:** when the plan is complete and verified (`bb check`, `bb e2e`,
   plus the editor gates for client-visible changes), tick the item, set the
   status to `done`, and update README and AGENTS.md in the same change.
3. **Reordering** is fine; say why in the commit message. New items go in the
   milestone they belong to, not at the end.

## Where we stand (September 2026, v0.3.0)

Shipped: definition (project, JAR, git and `:local/root` deps, JDK sources,
locals, keywords, Integrant keys, protocol and multimethod declarations),
references, rename (vars and locals), hover, ClojureDocs request, completion
(prefix-matched), signature help, document and workspace symbols, add-require
and clean-ns code actions, five native lints plus the clj-kondo bridge,
indent-on-Enter, `jar:` content provider, ignored-form dimming, multi-project
workspaces with graduated classpath resolution, let-go/lgx support.

Not shipped: `prepareRename`, keyword rename, `:as-alias`/`:rename`/
`:refer-clojure`/prefix-list/`declare` in ns forms, completion trigger
characters and fuzzy matching, keyword completion, auto-require on accept,
`documentHighlight`, `selectionRange`, `foldingRange`, formatting, semantic
tokens, code lens, implementation provider, `executeCommand` refactors,
`willRenameFiles`, a Clojure Pulse e2e gate, panic safety, performance
baselines.

## Milestone 0 — release gates

Nothing below ships as "done" without these.

- [ ] **Clojure Pulse e2e gate** (`bb e2e-pulse`). Mirror
      `scripts/calva-e2e/`: real VS Code under Xvfb, the packaged Clojure Pulse
      `.vsix` (built from `../clojure-pulse-vscode` when present, else the
      latest GitHub release), `clojurePulse.server.path` pointed at
      `target/debug/clj-pulse`. Checks: project definition, `jar:` navigation
      through the extension's own content provider (it calls
      `clojure/dependencyContents`, unlike Calva), hover, completion, and
      diagnostics arrive. The extension repo already skips or
      runs its end-to-end suite on `CLJ_PULSE_E2E_BIN`; reuse that where it
      fits. Then add the gate to AGENTS.md's verification list.
      Plan: [2026-09-05-1502-pulse-e2e-gate.md](plans/2026-09-05-1502-pulse-e2e-gate.md) — in progress
- [ ] **Docs accuracy sweep on every release**. README feature list,
      AGENTS.md invariants, and this file agree with `ServerCapabilities`
      (`src/server.rs`). Add a checklist step to `docs/RELEASE.md`.
      Plan: [2026-09-05-1502-pulse-e2e-gate.md](plans/2026-09-05-1502-pulse-e2e-gate.md) — in progress

## Milestone 1 — correctness of shipped features

Small fixes that remove wrong answers. Each extractor change bumps
`JarCacheEntry::format_version`.

- [ ] **ns-form remainder**
  - [ ] `:as-alias` — record as an alias; keyword resolution works; never
        counts as an unused namespace.
  - [ ] `:refer-clojure :exclude` / `:rename` — verify first whether a project
        var shadowing a core name resolves to the project or to core; then
        honor the clause.
  - [ ] `:rename` in `:refer` clauses — map renamed names to the original vars.
  - [ ] `declare` — index as a declaration so definition prefers the real def
        and references still resolve.
  - [ ] Prefix-list requires `(clojure set string)` — legacy, lowest.
  Plan: [2026-09-05-1537-ns-form-remainder-and-prepare-rename.md](plans/2026-09-05-1537-ns-form-remainder-and-prepare-rename.md) — in progress
- [ ] **`prepareRename`**. Advertise `prepareProvider: true`; return the
      token range for renameable symbols and a clean rejection (not a server
      error) for library, built-in, keyword, and `:keys`-destructured names.
      Plan: [2026-09-05-1537-ns-form-remainder-and-prepare-rename.md](plans/2026-09-05-1537-ns-form-remainder-and-prepare-rename.md) — in progress
- [ ] **Reliability floor**
  - [ ] Panic hook that logs to `server.log`; verify how tower-lsp behaves
        when a handler panics and make a panicking request fail alone.
  - [ ] Performance baseline: a `bb bench` task that indexes a large
        open-source Clojure repo and reports startup, memory, and per-edit
        lint latency. Fix cliffs it finds.
  - [ ] Malformed-input pass: unbalanced buffers, huge single lines, non-UTF-8
        files, empty `deps.edn`. Every handler returns, none panics.
  Plan: [2026-09-05-1537-reliability-floor.md](plans/2026-09-05-1537-reliability-floor.md) — in progress

## Milestone 2 — completion quality

The feature users touch most; today it is prefix-only and fires only on
identifier characters.

- [ ] Trigger characters `:` and `/` in `CompletionOptions`.
- [ ] Fuzzy matching. Extract the exact/prefix/substring/subsequence
      matcher from `handlers/symbols.rs` into a shared module and use it in
      `handlers/completion.rs`.
- [ ] Keyword completion from the occurrence index, current-ns keywords first.
- [ ] Auto-require on accept via `additionalTextEdits`, reusing the
      add-require edit builder.
- [ ] `completionItem/resolve` for docstrings and signatures so long lists stay
      cheap.
  Plan: —

## Milestone 3 — editor chrome for Calva and Neovim

Cheap with the tree-sitter parse resident; their absence reads as
"unfinished" in Neovim.

- [ ] `textDocument/documentHighlight`. Reuse `local_references_at` and
      the occurrence index; Read vs Write where cheap.
- [ ] `textDocument/selectionRange`. Expand along the parse tree.
- [ ] `textDocument/foldingRange`. Top-level forms, `(comment …)`, the ns
      form, multi-line collections.
- [ ] **Keyword rename**. Rewrite each occurrence in its own notation
      (`::kw`, `:ns/kw`, `::alias/kw`); include Integrant EDN files; refuse
      only when an occurrence can't be rewritten safely.
  Plan: —

## Milestone 4 — small power features

Each is small because the index already holds the data.

- [ ] `textDocument/implementation`. defprotocol/defmulti →
      deftype/defrecord/extend-*/reify/defmethod; the inverse of the
      impl→declaration navigation that exists.
- [ ] Sort requires, as an extension of clean-ns.
- [ ] `workspace/willRenameFiles`. Rewrite the `ns` form and every require
      when a file moves.
- [ ] Reference-count code lens, off by default.
  Plan: —

## Milestone 5 — public release

- [ ] Windows build target restored in the release matrix (commented out for
      CI time), or documented as unsupported.
- [ ] Settings documented in one place: every `.clj-pulse/config.edn` key with
      its default and the matching Clojure Pulse setting.
- [ ] Neovim setup verified against `bb e2e-nvim` and documented in README
      (done in README; keep it true).
- [ ] Issue templates and a short contributing note.
- [ ] Version 1.0 tag once Milestones 0–3 are done.
  Plan: —

## Best effort — do when cheap or asked

- **Native cljfmt-compatible formatter** (`textDocument/formatting` and
  `rangeFormatting`). Wanted for Neovim and Zed; Clojure Pulse formats
  client-side today. Compatibility bar: byte-identical to cljfmt on
  cljfmt-formatted sources, idempotent, honors `.cljfmt.edn`. A `cljfmt`
  native-binary bridge (same pattern as clj-kondo) is the cheaper interim if
  demand appears first.
- **ClojureScript**. `:require-macros` / `:refer-macros` parsing,
  `goog.*` prefixes, shadow-cljs classpath (`shadow-cljs.edn` `:dependencies`
  need Maven resolution, the same problem as Leiningen transitive deps).
- **Zed** — Zed formats and highlights via LSP and tree-sitter; the formatter
  above is what it needs most.
- **Semantic tokens**. Calva and Zed highlight without them; lowest of the
  chrome items.
- **Refactor set as `executeCommand`**: extract function, inline
  symbol, thread/unthread, move-to-let, cycle privacy. Build one
  "edit a form, preserve formatting" helper first.
- **Library-wide occurrence index**. Truthful references from inside deps;
  gate behind a setting, lazy per JAR.
- **Leiningen transitive deps** — see [MEMORY.md](MEMORY.md); opt-in
  `lein classpath` at most.
- **re-frame keyword registrations** (`reg-sub`/`reg-event-*` as definitions);
  the Integrant machinery is the template.
- **CLI mode** (`clj-pulse clean-ns|lint`) for CI, once the features exist.
- **Persistent project analysis cache** — only if a monorepo shows the need.
- **Custom macros beyond `:lint-as`** (multi-name macros) — consider clj-kondo
  hook metadata as a config source, never by running hook code.

## Not planned

- Java decompilation, `.class` stubs, library instance-method interop.
- Paredit-over-LSP and drag/move-form commands; editors do this natively.
- Call hierarchy, linked editing ranges, `typeDefinition`, moniker, inlay
  hints, document color.
- Calva custom APIs (`clojure/serverInfo`, cursorInfo, test tree, project
  tree) beyond the `clojure/dependencyContents` compatibility we already answer.
- `.lsp/config.edn` compatibility; `.clj-pulse/config.edn` plus read-only
  clj-kondo config is enough.
- Embedding clj-kondo or reimplementing its linters natively.
