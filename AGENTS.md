# clj-pulse

Rust LSP server for Clojure (tower-lsp, tree-sitter). See ARCHITECTURE.md for data flow.

See project's various notes at docs/MEMORY.md. The active plan is
docs/ROADMAP.md; follow its working rules: when starting an item, link its
plan on the item's `Plan:` line, and when the plan is complete, tick the item
and update README and this file in the same change.

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
- `bb e2e-pulse` — the first-priority editor, headless: real VS Code + the real
  Clojure Pulse extension (`clojurePulse.server.path` → our binary) under Xvfb
  (`scripts/pulse-e2e/`). Packages `../clojure-pulse-vscode` when that checkout
  is present, else installs the latest release. Covers project definition,
  `jar:` content through the extension's own `clojure/dependencyContents`
  provider, hover, completion, and diagnostics.

| Gate | Run when |
|---|---|
| `bb e2e` | every server behavior change |
| `bb e2e-pulse` | any client-visible change |
| `bb e2e-calva` | definition, `jar:`, or location-shape changes |
| `bb e2e-nvim` | new capabilities or protocol changes |

## Testing notes

- The e2e harness (`LspClient` in `tests/test_e2e.rs`) is the template for new
  feature tests: copy the fixture with `setup_project()`, `initialize`, `did_open`,
  then assert on raw JSON responses. `wait_for_log("Indexed")` /
  `wait_for_log("library indexing complete")` synchronize with the two
  background indexing tasks; `wait_for_log("full classpath indexed")` with
  stage-3 CLI resolution.
- The harness sets `CLJ_PULSE_DISABLE_CLASSPATH_CLI=1` so regular e2e tests
  (whose fixtures contain a deps.edn) never spawn `clojure`; tests that
  exercise stage 3 use `LspClient::start_with_classpath_cli`. It sets
  `CLJ_PULSE_DISABLE_KONDO=1` for the same reason: the suite must behave
  identically on a machine with clj-kondo installed and one without.
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
- The workspace is multi-project: `projects::detect` finds every dir holding a
  `deps.edn` / `project.clj` / `lgx.edn` (gitignore-respecting, max depth 4),
  and `.clj-pulse/config.edn` `{:projects [{:path "apps/a" :classpath
  {:enabled … :cmd "…"}}]}` entries override per path — or add a project
  detection skipped (gitignored dirs). The old top-level `:classpath
  {:enabled … :aliases […]}` syntax is gone; there is no back-compat parsing.
- Classpath indexing is graduated *per project*: stage 1 scans every project's
  own `:paths` into one shared index; stage 2 reads each project's `.cpcache`
  instantly; stage 3 runs the project's verbatim `:cmd` in the project dir
  (`clojure -A:dev:test -Spath` for deps.edn, `lein classpath` for Leiningen,
  none for lgx) — enabled by default only for the workspace root. Stage-3 runs
  are serialized (`ClasspathCliLock`) and compare against that project's
  last-indexed entry set — never re-read `.cpcache` to detect change,
  `-Spath` just wrote it. Any stage-3 failure degrades to the stage-2 result.
- The library index is rebuilt per project, per kind (`rebuild_libs`), never
  as one flat scan — a flat `index_classpath_libs` over the union would skip
  in-workspace lgx `:local/root` dirs and lose let-go core. Disabling a
  project only stops stage 3; its stage-2 libraries stay indexed.
- Source scans stop gitignore ancestry at the project dir
  (`scanner::ScanRoot`): a configured project inside a gitignored dir still
  scans, while gitignores at or below the project dir keep applying.
- `CLJ_PULSE_DISABLE_CLASSPATH_CLI` (non-empty) forces `:enabled false` for
  every project (the e2e harness depends on this).
- Diagnostics come from two tiers: the native lints (`unresolved-namespace`,
  `unused-namespace`, `duplicate-require`, `unused-binding`,
  `unused-private-var`) and clj-kondo, spawned per lint pass when found. A
  successful kondo run owns all five codes and the native copies are dropped
  for that pass; any failure publishes the native set unchanged. One publish
  per pass, never two.
- Rename resolves locals structurally (`extractor::local_references_at`)
  *before* the fqn path, so a param shadowing a global only ever edits itself;
  a `:keys`/`:strs`/`:syms`-destructured binding is rejected, since its name is
  also the key being read.
- `CLJ_PULSE_DISABLE_KONDO` (non-empty) forces `:kondo {:enabled false}`, the
  twin of `CLJ_PULSE_DISABLE_CLASSPATH_CLI`. `LspClient::start` sets it, so no
  test depends on a host clj-kondo; kondo tests opt in with
  `start_with_kondo` / `start_with_kondo_env`, which put the committed fake
  (`tests/fixtures/fake-clj-kondo/clj-kondo`) first on the child's PATH.
- Classpath libraries come in two shapes: JARs (`SymbolSource::Jar`, navigated
  via `jar:` URIs) and source directories — git deps in `~/.gitlibs`,
  `:local/root` deps (`SymbolSource::Dir`, navigated via plain `file:` URIs).
- Files outside deps.edn `:paths` are indexed on `didOpen`.
- Only top-level `:paths` in deps.edn counts (not `:paths` inside `:aliases`).
- Defining macros resolve by fqn, never by bare name: the user's `:lint-as` map
  first, then the built-in table `DefKind::from_macro_fqn`
  (`clojure.test/deftest` and friends). `NsMeta.refer_all` records
  `:refer :all` / `(:use ns)` namespaces; head resolution, completion and
  `resolve_symbol` all consult it, so `deftest` works however `clojure.test`
  was required.

## Releasing

Releases are tag-driven: `bb tag` reads the version from `Cargo.toml`, tags it
`v<version>`, and pushes to `origin`, which triggers the release CI (build matrix
+ checksums + GitHub Release). The CI also regenerates the Homebrew formula and
pushes it to the tap (`brew install abogoyavlensky/tap/clj-pulse`). `Cargo.toml`
is the source of truth — bump it first. See [docs/RELEASE.md](docs/RELEASE.md)
for the full flow.

## User's setup

The maintainer tests manually in VS Code on macOS via the Clojure Pulse
extension (`../clojure-pulse-vscode`, `clojurePulse.server.path` →
`target/debug/clj-pulse`) and via Calva (`calva.clojureLspPath`). Editor
priority: Clojure Pulse, Calva, Neovim; Zed and ClojureScript are best effort.
Clojure Pulse registers its own `jar:` `TextDocumentContentProvider` that calls
the server's `clojure/dependencyContents`; Calva reads JARs itself and never
calls it, so both paths must keep working.

See [docs/DEV_SETUP.md](docs/DEV_SETUP.md) for the full development &
verification environment: the two environments (maintainer's Calva/macOS vs the
headless CI box), tooling versions, and what each `bb e2e*` task covers.
