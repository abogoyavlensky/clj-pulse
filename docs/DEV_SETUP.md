# Development & verification setup

How clj-pulse is developed and verified across two very different
environments: the maintainer's editor, and the headless CI/agent box.

## Two environments

- **Maintainer (manual testing):** VS Code on **macOS** via the **Clojure
  Pulse** extension (`../clojure-pulse-vscode`,
  `"clojurePulse.server.path": ".../clj-pulse/target/debug/clj-pulse"`) and via
  **Calva** (`"calva.clojureLspPath"` pointed at the same binary), rebuilding
  the debug binary (`cargo build`) on each change.
- **CI / automated agent:** an isolated **Linux** box with no editor and no
  view of the maintainer's setup. All verification here is headless.

> The project was renamed **clj-lsp → clj-pulse** on 2026-06-15: crate / lib /
> bin names, the `.clj-pulse/` data dir, the LSP `serverInfo` name, and the
> diagnostic source string.

## Tooling

Every CLI tool is pinned in `.mise.toml`; `mise install` in the repo root
installs all of them (CI's `mise-action` reads the same file).

- **Rust** + **babashka** (`bb` tasks drive all checks).
- **clojure CLI** + **java** (temurin-25) for `bb e2e-real` and the
  `bb e2e-calva` fixture classpath.
- **clj-kondo** for `bb e2e-real-kondo`.
- **Neovim** (headless) for `bb e2e-nvim`.
- **Xvfb + real VS Code + real Calva** for `bb e2e-calva`: `xvfb` from the OS
  package manager, `npm install` in `scripts/calva-e2e`; the first run
  downloads VS Code + Calva (~150MB) into the gitignored `.vscode-test/`.

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
- `bb e2e-calva` — real VS Code + real Calva (`calva.clojureLspPath` → our
  binary) under Xvfb (`scripts/calva-e2e/`).
- Clojure Pulse has no gate here yet (roadmap Milestone 0). Its own suite in
  `../clojure-pulse-vscode` runs an end-to-end test against a server binary
  when `CLJ_PULSE_E2E_BIN` is set:
  `CLJ_PULSE_E2E_BIN=$PWD/target/debug/clj-pulse xvfb-run -a npx vscode-test -g "end to end"`.

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
  **Clojure Pulse does the opposite:** its `jar:` provider asks the server via
  `workspace/textDocumentContent`, so that request must keep working too.

## Related fixtures

- `../tickets` (sibling of this repo) is a real Leiningen + ClojureScript
  project used to test `project.clj` support manually. Its `project.clj`
  exercises the hard cases: `^{:protect false}` / `^:replace` metadata and a
  `#"user"` regex literal (all rejected by `edn_format`), plus `:dependencies`
  split across the top level and `:profiles`. In the CI box only `cheshire` of
  its deps is downloaded under `~/.m2/repository`.
