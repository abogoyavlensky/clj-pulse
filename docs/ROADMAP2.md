# Roadmap 2 — closing the practical gap with clojure-lsp

[ROADMAP.md](ROADMAP.md) covered the read-and-navigate core and is mostly
done. This document is the next arc: a source-level gap analysis against
clojure-lsp (July 2026, clojure-lsp master vs clj-pulse 0.1.4) turned into
prioritized work. Still-open items from ROADMAP.md are folded into the
tiers below so this file can supersede it as the active plan.

Guiding idea: **correctness of existing features before new surface.** An
early-stage server loses users faster to one wrong clean-ns result (from an
unparsed `:refer :all`) than to a missing extract-function. Tiers are
ordered by what breaks daily use, not by feature-count parity — clojure-lsp
has ~60 refactor commands and a dozen extension methods we deliberately
will not chase (see "Not planned").

## Where we stand (July 2026)

Solid: definition (locals, keywords, Integrant keys, jar/git-dep/JDK
navigation), references, rename, hover, completion, signature help,
document/workspace symbols, add-require + clean-ns actions, 3 native lints,
indent-on-Enter, `jar:` content provider, ignored-form dimming. Plus
differentiators clojure-lsp lacks: instant startup / no JVM, Integrant
navigation, let-go/lgx support.

Main gaps vs clojure-lsp, in descending practical impact:

1. Diagnostics depth — no syntax errors, no unresolved-symbol, no arity
   checks; clojure-lsp ships all of clj-kondo plus its own linters.
2. Editing support — no formatting (beyond indent-on-Enter), selection
   ranges, document highlight, folding, semantic tokens, code lens.
3. Refactorings — 2 code actions vs clojure-lsp's suite.
4. Correctness holes in shipped features — ns-form parsing gaps
   (`:as-alias`, `:refer :all`, `:use`, prefix lists, `^:private`
   metadata, `declare`), keyword rename refused, project-only occurrence
   index.

## Tier 1 — trust and daily-driver blockers

The things users notice as *broken*, not as missing.

### 1.1 clj-kondo diagnostics bridge - DONE

Shipped: clj-pulse spawns a `clj-kondo` binary per lint pass on the didOpen,
didSave, and debounced didChange paths, and publishes its findings with
`source: "clj-kondo"`. A successful run owns the three codes the native lints
also emit, so nothing is reported twice; any failure degrades to the native set
unchanged. Configured by `:kondo {:enabled :path}` in `.clj-pulse/config.edn`
or the matching VS Code settings, both live-reloaded, with
`CLJ_PULSE_DISABLE_KONDO` as the test kill-switch. The extension shows the
active tier in its status-bar tooltip via a `clojurePulse/lintStatus`
notification.

Classpath cache warming shipped too: when a `.clj-kondo` directory exists,
clj-pulse runs `clj-kondo --lint <classpath> --dependencies --parallel` in the
background after a project's classpath resolves, so the cross-file linters work
before the user has opened enough files to populate the cache by hand.

Deliberately deferred: `--copy-configs`, which writes JAR-exported clj-kondo
configs into the repo. clj-pulse should not modify a user's working tree
without being asked.

The original plan, for reference:

- Shell out to a `clj-kondo` binary when present on `PATH` (or configured
  path), `clj-kondo --lint - --filename <path> --config ...`, feeding the
  live buffer on the existing 300ms-debounced didChange path
  (`src/diagnostics.rs`). Parse the JSON findings into LSP diagnostics.
- Merge with the native lints, don't replace them: native
  unresolved-namespace / unused-namespace / duplicate-require are instant
  and power the add-require lightbulb; dedupe overlapping findings by
  (range, code), native wins for the codes we own.
- `source: "clj-kondo"` on bridged diagnostics so users can tell them
  apart and configure them in `.clj-kondo/config.edn` as usual — we
  already read that file (`src/kondo.rs`), so levels/excludes come free.
- Graceful degradation: no binary found → native lints only, one log line,
  no error. Kill/respawn on timeout; never block the request path.
- Deliberately **not** embedding clj-kondo (GraalVM lib or reimplement):
  the subprocess costs one integration and buys hundreds of linters that
  track clj-kondo releases for free. A native re-implementation of even 10
  linters would cost more and lag forever.
- Follow-up in the same area: honor `:linters` levels from
  `.clj-kondo/config.edn` for clj-pulse's own native diagnostics (already
  noted as future work in ROADMAP.md).

### 1.2 ns-form parsing completeness (extractor correctness)

Silent correctness holes (`src/index/extractor.rs:493-535` parses only
`:as` and `:refer [vec]`): they corrupt unused-namespace lints, clean-ns,
and resolution in real codebases. Cheap fixes, outsized trust impact.
Sourced from docs/MISSING_PARTS.md plus the gap analysis.

- [ ] `:as-alias` — treated as an alias for keyword/symbol resolution; must
      not count as "unused" just because no runtime var is referenced.
- [ ] `:refer :all` — mark the namespace as wildcard-referred; resolution
      falls back to that ns's public vars; unused-namespace must not flag it.
- [ ] `:use` (and `:use ... :only [...]`) — legacy but present in older
      codebases; treat as `:refer :all` / `:refer [only-vec]`.
- [ ] `:rename` in refer clauses — map renamed locals to original vars.
- [ ] `:refer-clojure` `:exclude`/`:rename` — affects which bare names
      resolve to clojure.core.
- [ ] Prefix-list requires `(clojure set string)` — parse and resolve
      (carried from ROADMAP.md Phase 5; today explicitly unsupported,
      `extractor.rs:446-449`).
- [ ] `(def ^:private …)` metadata privacy — currently only `defn-` is
      detected as private.
- [ ] `declare` forward declarations — index as declarations so
      goto-definition can prefer the real def but references still resolve.

Bump `JarCacheEntry::format_version` (src/index/jar_cache.rs) with any of
these — extractor output changes and JAR mtimes never change.

### 1.3 Keyword rename + prepareRename

re-frame/Integrant codebases live on namespaced keywords; refusing rename
(`handlers/references.rs:129`) hurts exactly our target users. The
occurrence index already tracks keyword usages with full-token ranges.

- [ ] Rename `::kw`, `:ns/kw`, `::alias/kw` across the project, rewriting
      each occurrence in its own notation (auto-resolved stays
      auto-resolved, aliased stays aliased). Refuse cross-file rename only
      when an occurrence can't be rewritten safely in its notation.
- [ ] Include Integrant EDN config files in the edit (the occurrences are
      already indexed).
- [ ] `textDocument/prepareRename` — advertise
      `rename_provider: {prepareProvider: true}` so clients show correct
      UI: valid range for symbols/keywords, rejection (instead of a
      server error) for library/built-in symbols and other non-renameable
      positions.

### 1.4 Completion quality pass

What separates "functional" from "modern" completion. Carried over from
ROADMAP.md Phase 3 (auto-require, keywords, fuzzy — locals are done).

- [ ] Trigger characters `":"` and `"/"` in `CompletionOptions`
      (`src/server.rs:309`) — today completion only fires on identifier
      chars, so keyword and alias-qualified completion never trigger
      naturally.
- [ ] Keyword completion — from the keyword occurrence index; ranked by
      frequency, current-ns keywords first.
- [ ] Auto-require on accept — completing a symbol from a not-yet-required
      namespace inserts the `:require` via `additionalTextEdits` (reuse the
      add-require code action's edit builder, `handlers/code_action.rs`).
- [ ] Fuzzy matching — replace bare `starts_with` with the 4-tier
      exact/prefix/substring/subsequence matcher already used by
      workspace/symbol (`handlers/symbols.rs:117`); extract it into a
      shared module.
- [ ] `completionItem/resolve` — defer docstring/signature rendering to
      resolve so large candidate lists stay cheap; advertise
      `resolve_provider: true`.

## Tier 2 — editing-support parity

Editor chrome users see constantly. All structurally cheap because the
tree-sitter parse is already resident; each is mostly a mapping exercise.

- [ ] **Selection ranges** (`textDocument/selectionRange`) —
      expand/contract selection along the parse tree. Disproportionately
      valuable in a lisp; near-free with tree-sitter. Do this first in the
      tier.
- [ ] **Document highlight** (`textDocument/documentHighlight`) —
      occurrences of the symbol under cursor in the current buffer; the
      local-references machinery and occurrence index already compute
      exactly this (`handlers/references.rs`). Distinguish Read/Write
      where cheap (binding site vs usage).
- [ ] **Folding ranges** — top-level forms, `(comment …)` blocks, ns
      form, literal maps/vectors over N lines. Trivial from the tree.
- [ ] **Full + range formatting** — cljfmt-compatible whole-document and
      range formatting (`textDocument/formatting` / `rangeFormatting`).
      Tier B of the indent work (ROADMAP.md Phase 3): a cljfmt `:indents`
      rules table + `.cljfmt.edn` reading, applied by the existing
      structural engine (`handlers/indent.rs`). Note on priority: Calva
      formats client-side, so the maintainer setup won't feel the gap —
      but Neovim/Zed/Helix users format via LSP and read its absence as
      disqualifying. Compatibility bar: idempotent on cljfmt-formatted
      code (format twice = format once; test against real library
      sources).
- [ ] **Semantic tokens** (full + range) — resolution-based coloring:
      function vs macro vs var, defs, locals, keywords, with
      unused-binding modifiers. Already earmarked as the Tier-2 follow-up
      to ignored-form dimming (ROADMAP.md "Done" notes). Token legend ≈
      clojure-lsp's: namespace, type, function, macro, keyword, variable,
      method; modifiers: definition, defaultLibrary.

## Tier 3 — power features

The ~20% of clojure-lsp's refactor surface that gets ~80% of real use.

- [ ] **Implementation provider** (`textDocument/implementation`) —
      defprotocol/defmulti → deftype/defrecord/extend-*/reify/defmethod
      impls. We already navigate impl → declaration (ROADMAP.md Phase 5);
      this inverts that index. Lifts the "protocol implementations" item
      out of ROADMAP.md's out-of-scope list — the data now exists.
- [ ] **Starter refactor set** — as code actions + `executeCommand`, not
      the full suite. Chosen for muscle-memory frequency in Calva/CIDER:
  - [ ] Sort requires (carried from ROADMAP.md Phase 3; extends clean-ns).
  - [ ] Add missing import (Java classes; `:import` is already parsed).
  - [ ] Extract function.
  - [ ] Inline symbol (let binding or def).
  - [ ] Thread first/last (+ thread-all, unwind once, unwind all).
  - [ ] Move to let / expand let.
  - [ ] Cycle privacy (defn ↔ defn-, incl. `^:private` after 1.2 lands).
  - Requires advertising `executeCommandProvider` and structured
    `codeActionKinds`. Each action is a rewrite of a tree-sitter subtree —
    build one small "edit a form, preserve formatting" helper first and
    keep each refactor a pure function over it (each is its own small
    project; ship them one at a time).
- [ ] **Reference-count code lens** — carried from ROADMAP.md Phase 3;
      the occurrence index has the data. `codeLens` returns ranges,
      `codeLens/resolve` fills counts lazily. Off by default if noisy.
- [ ] **ns rename on file move** (`workspace/willRenameFiles`) — rewrite
      the `ns` form and all requires when a file is renamed/moved.
      Small feature, prevents a whole class of broken-namespace states.
      Advertise via `workspace.fileOperations.willRename` with clj/cljc/
      cljs globs.

## Tier 4 — ecosystem depth (opportunistic)

- [ ] **Library-wide occurrence index** — references today are
      project-only plus currently-open library files (ROADMAP.md notes
      this). Indexing occurrences for library files makes find-references
      truthful from inside deps and later unlocks unused-public-var-style
      lints. Costs memory/index time — gate behind a setting or index
      lazily per-jar on first library-side request; bump the jar cache
      format version.
- [ ] **shadow-cljs classpath and cljs-aware indexing** (carried from
      ROADMAP.md Phase 5) — `npx shadow-cljs classpath` or parse
      `shadow-cljs.edn` deps; reader-conditional-aware resolution per
      dialect.
- [ ] **Leiningen transitive deps** — reconsider the "never start a JVM"
      rule as an explicit opt-in: shell out to `lein classpath` once,
      cache like `.cpcache`. Keep the current no-JVM direct-deps path as
      the default (docs/MEMORY.md rationale stands).
- [ ] **Keyword indexing for re-frame subs/events** (carried from
      ROADMAP.md Phase 5) — `reg-sub`/`reg-event-*` registration keywords
      as definition sites, `subscribe`/`dispatch` as usages; the Integrant
      machinery is the template.
- [ ] **Settings surface** — grow `.clj-pulse/config.edn` deliberately
      (diagnostics levels, formatting toggles, index opt-ins) rather than
      cloning clojure-lsp's 70KB settings.md. Document every key in one
      place from day one.
- [ ] **Persistent project analysis cache** (carried from out-of-scope) —
      startup is already fast; only matters for very large monorepos.
      Revisit only if someone hits it.
- [ ] **CLI mode** — `clj-pulse clean-ns|format|lint` over files/globs for
      CI. Cheap once the features exist; it's how clojure-lsp got CI
      adoption. Exit non-zero on findings/diffs.
- [ ] **Clojuredocs for built-ins** (carried from ROADMAP.md Phase 5) —
      enrich hover for clojure.core with examples/see-also; bundled or
      cached download, never a blocking network call.
- [ ] **Custom macro definitions beyond `:lint-as`** (carried from
      ROADMAP.md Phase 5) — e.g. `defcomponent`-style macros that define
      more than one name; consider honoring clj-kondo hooks metadata as a
      config source, not by running hook code.
- [ ] **Local file cache for project files** (carried from ROADMAP.md
      Phase 5).

## Not planned (deliberate)

Feature-diff items against clojure-lsp we choose not to chase, with why:

- **Java decompilation (CFR) and stubs generation** — huge complexity,
  niche payoff; JDK `src.zip` navigation covers the common case. Library
  `.class`/instance-method interop stays out until there's demonstrated
  demand.
- **Paredit-over-LSP, drag/move-form commands** — editors do structural
  editing natively (Calva, Neovim plugins, Zed); clojure-lsp's versions
  see little use.
- **Call hierarchy, linked editing ranges** — real but low-frequency;
  revisit after Tier 3 ships.
- **Calva custom APIs** (`clojure/serverInfo`, cursorInfo, test tree,
  project tree) — Calva works without them; reconsider only if a concrete
  Calva integration needs one. We already answer
  `clojure/dependencyContents` for compatibility.
- **`typeDefinition`, moniker, inlay hints, document color** —
  clojure-lsp doesn't implement them either; not part of the canonical
  bar for a lisp.
- **`.lsp/config.edn` compatibility** — we keep our own
  `.clj-pulse/config.edn` plus read-only clj-kondo config compat; a
  third config dialect isn't worth the ambiguity.

## Cross-cutting rules

- Every tier item that changes server behavior lands with `bb e2e`
  coverage; client-visible protocol changes (new capabilities, new
  request types) also pass `bb e2e-nvim`, and Calva-facing ones
  `bb e2e-calva` (CLAUDE.md verification rules).
- Any extractor/`Symbol`/`NsMeta` layout change bumps
  `JarCacheEntry::format_version`.
- New capabilities are advertised in `ServerCapabilities`
  (`src/server.rs:299`) in the same change that implements the handler —
  never advertise ahead of implementation.
- Protect the differentiators while closing gaps: startup latency, single
  dependency-free binary, let-go/lgx and Integrant support are why
  clj-pulse exists; no tier item may regress them (e.g. clj-kondo stays a
  subprocess, never a bundled runtime).
