# Development & verification setup

How clj-pulse is developed and verified across two very different
environments: the maintainer's editor, and the headless CI/agent box.

## Getting started

Install [mise](https://mise.jdx.dev/) for managing tool versions, then:

```sh
mise install
```

This installs the correct versions of Rust and Babashka.

```sh
bb fmt        # fix code formatting
bb fmt-check  # check formatting without fixing
bb lint       # run clippy linter
bb test       # run tests
bb check      # run all checks (fmt-check + lint + test), exactly as CI does
bb bench      # compare clj-pulse with clojure-lsp on two real projects
bb soak       # churn one long-lived server and check it against a fresh one
bb compare    # judge definition/references/rename against clj-kondo's analysis
bb outdated   # check outdated deps
bb build      # build the dev binary
bb release    # build release binary
bb tag        # create and push new git tag based on version from Cargo.toml
```

For editor verification, see [Verifying changes headlessly](#verifying-changes-headlessly).

`bb bench` checks out [metabase](https://github.com/metabase/metabase) and
[clj-kondo](https://github.com/clj-kondo/clj-kondo) at pinned commits under
`.tmp/bench/`, downloads the pinned clojure-lsp release beside them, and runs
four configurations per corpus - each server cold and warm - printing a table
and a `BENCH_JSON` line per row. `bb bench metabase` or `bb bench clj-kondo`
runs one. See the [benchmark records](MEMORY.md#benchmark-against-clojure-lsp)
for the full tables and [Performance](PERFORMANCE.md) for the warm summary.

`bb soak` drives one server through 20 rounds of churn on the same corpora -
edits in open buffers, saves, files changed, created, deleted and renamed on
disk, and every fifth round a 100-file batch delivered as a single
`didChangeWatchedFiles`, the shape a branch switch has. Every action carries a
witness the index has to reflect, and at each checkpoint the corpus is put back
at its pinned commit and a freshly started server is asked the same questions:
if the two disagree about a definition, a reference, a document symbol or a
workspace symbol, the run fails and prints both answers. Memory is sampled at
each checkpoint in the same quiesced, nothing-open state. Unlike `bb bench` it
is a pass/fail gate. `bb soak metabase` is the long one, and the seed printed on
every run replays a failure exactly: `bb soak clj-kondo <seed>`.

`bb compare` asks one production server a definition, references or rename
question at every position clj-kondo's analysis of the same corpus knows the
answer to, and reports every disagreement by language construct - an oracle
that is not our own extractor. It is advisory (the report is the product;
`CLJ_PULSE_COMPARE_STRICT=1` makes any new divergence fail it), and the first
run's table with what it found is in [MEMORY.md](MEMORY.md).

> [!NOTE]
> To run `bb outdated` you need to have `cargo-outdated` installed. You can install it with `cargo install cargo-outdated`.

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
- **Xvfb + real VS Code + the Clojure Pulse extension** for `bb e2e-pulse`:
  `npm install` in `scripts/pulse-e2e`. VS Code is shared with the Calva gate
  (`scripts/.vscode-cache/`); the extension is packaged from
  `../clojure-pulse-vscode` when that checkout is present, else downloaded from
  the latest release.

## Verifying changes headlessly

All of these run without an editor and are the source of truth for "does it
work" (see also the quick reference in [AGENTS.md](../AGENTS.md)):

- `bb check` — fmt *check* + clippy `-D warnings` + all tests. CI runs the
  same, so a green `bb check` means a green CI; it fails on unformatted code
  rather than rewriting it. `bb fmt` is the fixer.
- `bb e2e` — spawns the real binary, speaks framed JSON-RPC over stdio like an
  editor (`tests/test_e2e.rs`): definition (project + `jar:` URIs), completion,
  hover, didChange, `workspace/textDocumentContent`.
- `bb e2e-real` — same harness against a real Maven classpath: generates
  `.cpcache` via `clojure -Spath` and navigates into a downloaded JAR.
- `bb e2e-nvim` — drives the server through a real editor client (headless
  Neovim's built-in LSP client, `scripts/e2e_nvim.lua`).
- `bb e2e-calva` — real VS Code + real Calva (`calva.clojureLspPath` → our
  binary) under Xvfb (`scripts/calva-e2e/`).
- `bb e2e-pulse` — real VS Code + the real Clojure Pulse extension
  (`clojurePulse.server.path` → our binary) under Xvfb (`scripts/pulse-e2e/`):
  project definition, `jar:` content through the extension's own
  `clojure/dependencyContents` provider, hover, completion, and diagnostics.
  The extension's own suite covers the other direction — it runs against a
  server binary when `CLJ_PULSE_E2E_BIN` is set:
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
  `clojure/dependencyContents`, so that request must keep working too. Verified
  by `bb e2e-pulse`.

## Related fixtures

- `../tickets` (sibling of this repo) is a real Leiningen + ClojureScript
  project used to test `project.clj` support manually. Its `project.clj`
  exercises the hard cases: `^{:protect false}` / `^:replace` metadata and a
  `#"user"` regex literal (all rejected by `edn_format`), plus `:dependencies`
  split across the top level and `:profiles`. In the CI box only `cheshire` of
  its deps is downloaded under `~/.m2/repository`.
