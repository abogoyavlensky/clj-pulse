# clj-pulse

Rust LSP server for Clojure (tower-lsp, tree-sitter). See ARCHITECTURE.md for data flow.

## Verification (run before claiming anything works)

- `bb check` — fmt + clippy `-D warnings` + all tests. CI runs the same.
- `bb e2e` — end-to-end: spawns the real binary, speaks framed JSON-RPC over
  stdio like an editor (`tests/test_e2e.rs`). Covers definition (project +
  jar: URIs), Integrant keyword navigation (`config.edn` key → `ig/init-key`
  defmethod), completion, hover, didChange edits, `workspace/textDocumentContent`.
- `bb e2e-real` — same harness against a real Maven classpath: generates
  `.cpcache` via `clojure -Spath` and navigates into a downloaded JAR.
  Needs the clojure CLI; ignored in plain `cargo test`.
- `bb e2e-nvim` — drives the server through a real editor client
  (headless Neovim's built-in LSP client, `scripts/e2e_nvim.lua`).
- `bb e2e-calva` — the user's exact setup, headless: real VS Code + real Calva
  (`calva.clojureLspPath` → our binary) under Xvfb (`scripts/calva-e2e/`).
  Covers project + jar: navigation through Calva's own definition pipeline and
  jar content provider. First run downloads VS Code + Calva (~150MB).

Server behavior changes are not done until `bb e2e` passes; client-visible
protocol changes should also pass `bb e2e-nvim`.

## Testing notes

- The e2e harness (`LspClient` in `tests/test_e2e.rs`) is the template for new
  feature tests: copy the fixture with `setup_project()`, `initialize`, `did_open`,
  then assert on raw JSON responses. `wait_for_log("Indexed")` /
  `wait_for_log("library indexing complete")` synchronize with the two
  background indexing tasks.
- Test realistic Clojure, not just toy snippets: real libraries use ns/def
  metadata (`(ns ^{:doc "…"} foo)`), reader conditionals, multi-arity fns.
  The extractor must handle them (see `test_extractor.rs`).
- `JarCacheEntry::format_version` (src/index/jar_cache.rs) must be bumped
  whenever extractor output or `Symbol`/`NsMeta` layout changes — JAR mtimes
  never change, so stale caches survive otherwise.

## Invariants

- Project symbols always win over library symbols with the same fqn; project
  and library indexing run concurrently, so library insertion uses
  `Index::insert_lib_file` (never plain `insert_file`).
- Classpath libraries come in two shapes: JARs (`SymbolSource::Jar`, navigated
  via `jar:` URIs) and source directories — git deps in `~/.gitlibs`,
  `:local/root` deps (`SymbolSource::Dir`, navigated via plain `file:` URIs).
- Files outside deps.edn `:paths` are indexed on `didOpen`.
- Only top-level `:paths` in deps.edn counts (not `:paths` inside `:aliases`).

## Releasing

Releases are tag-driven: `bb tag` reads the version from `Cargo.toml`, tags it
`v<version>`, and pushes to `origin`, which triggers the release CI (build matrix
+ checksums + GitHub Release). The CI also regenerates the Homebrew formula and
pushes it to the tap (`brew install abogoyavlensky/tap/clj-pulse`). `Cargo.toml`
is the source of truth — bump it first. See [docs/RELEASE.md](docs/RELEASE.md)
for the full flow.

## User's setup

The maintainer tests manually in VS Code on macOS via Calva
(`calva.clojureLspPath` → `target/debug/clj-pulse`). Plain vscode-languageclient
9.x has no `workspace/textDocumentContent` support, so `jar:` URI navigation
needs client-side wiring in the editor extension (not yet done).

See [docs/DEV_SETUP.md](docs/DEV_SETUP.md) for the full development &
verification environment: the two environments (maintainer's Calva/macOS vs the
headless CI box), tooling versions, and what each `bb e2e*` task covers.
