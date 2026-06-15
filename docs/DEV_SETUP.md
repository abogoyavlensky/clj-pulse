# Development & verification setup

How clj-pulse is developed and verified across two very different
environments: the maintainer's editor, and the headless CI/agent box.

## Two environments

- **Maintainer (manual testing):** VS Code on **macOS** via **Calva**, with
  `"calva.clojureLspPath": "/Users/andrew/Projects/clj-pulse/target/debug/clj-pulse"`,
  rebuilding the debug binary (`cargo build`) on each change.
- **CI / automated agent:** an isolated **Linux** box with no editor and no
  view of the maintainer's setup. All verification here is headless.

> The project was renamed **clj-lsp → clj-pulse** on 2026-06-15: crate / lib /
> bin names, the `.clj-pulse/` data dir, the LSP `serverInfo` name, and the
> diagnostic source string.

## Tooling

- **Rust** + **babashka** (`bb` tasks drive all checks).
- **clojure CLI** for `bb e2e-real`, installed via `mise` (global pins
  `clojure@1.12.4` + java `temurin-25`).
- **Neovim 0.9.5** (headless) for `bb e2e-nvim`.
- **Xvfb + real VS Code + real Calva** for `bb e2e-calva` (first run downloads
  VS Code + Calva, ~150MB).

## Verifying changes headlessly

All of these run without an editor and are the source of truth for "does it
work" (see also the quick reference in [AGENTS.md](../AGENTS.md)):

- `bb check` — fmt + clippy `-D warnings` + all tests. CI runs the same.
- `bb e2e` — spawns the real binary, speaks framed JSON-RPC over stdio like an
  editor (`tests/test_e2e.rs`): definition (project + `jar:` URIs), completion,
  hover, didChange, `workspace/textDocumentContent`.
- `bb e2e-real` — same harness against a real Maven classpath: generates
  `.cpcache` via `clojure -Spath` and navigates into a downloaded JAR.
- `bb e2e-nvim` — drives the server through a real editor client (headless
  Neovim's built-in LSP client, `scripts/e2e_nvim.lua`).
- `bb e2e-calva` — the definitive reproduction of the maintainer's setup: real
  VS Code + real Calva (`calva.clojureLspPath` → our binary) under Xvfb
  (`scripts/calva-e2e/`).

## Why this matters

- **"Works in tests" ≠ "works in the editor."** Client-side wiring (Calva /
  VS Code) differs from unit-test conditions, so server behavior is verified
  headlessly but end-to-end.
- **Test realistic library code.** The metadata-on-ns-name bug
  (`(ns ^{:doc "…"} foo)`) only surfaced against a real JAR, not toy snippets.
- **Calva handles `jar:` URIs client-side.** Its own `TextDocumentContentProvider`
  reads JARs locally (JSZip); it never calls `workspace/textDocumentContent`.
  Returning clojure-lsp-style `jar:file:///…!/…` scalar `Location`s is all the
  server needs to do. Verified working via the Calva rig on 2026-06-12.

## Related fixtures

- `../tickets` (sibling of this repo) is a real Leiningen + ClojureScript
  project used to test `project.clj` support manually. Its `project.clj`
  exercises the hard cases: `^{:protect false}` / `^:replace` metadata and a
  `#"user"` regex literal (all rejected by `edn_format`), plus `:dependencies`
  split across the top level and `:profiles`. In the CI box only `cheshire` of
  its deps is downloaded under `~/.m2/repository`.
