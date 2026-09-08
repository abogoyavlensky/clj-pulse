# clj-pulse

Rust LSP server for Clojure (tower-lsp, tree-sitter). See ARCHITECTURE.md for data flow.

See project's various notes at docs/MEMORY.md. The active plan is
docs/ROADMAP.md; follow its working rules: when starting an item, link its
plan on the item's `Plan:` line, and when the plan is complete, tick the item
and update README and this file in the same change.

## Verification (run before claiming anything works)

- `bb check` — fmt *check* + clippy `-D warnings` + all tests. CI runs the
  same, so a green `bb check` means a green CI; it fails on unformatted code
  instead of rewriting it, and `bb fmt` is the fixer.
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

- `bb bench` — a large real project (metabase, shallow-cloned into
  `.tmp/bench/` on first run) indexed by the release binary under production
  settings, reporting index time, symbol counts, RSS, and per-edit and
  definition latency (`tests/test_bench.rs`). Not a pass/fail gate beyond a
  120 s hang ceiling; compare its table against the baseline in
  [docs/MEMORY.md](docs/MEMORY.md).

| Gate | Run when |
|---|---|
| `bb e2e` | every server behavior change |
| `bb e2e-pulse` | any client-visible change |
| `bb e2e-calva` | definition, `jar:`, or location-shape changes |
| `bb e2e-nvim` | new capabilities or protocol changes |
| `bb bench` | before a release, and after index or extractor changes |

## Testing notes

- The e2e harness (`LspClient` in `tests/common/mod.rs`, shared by
  `test_e2e.rs` and `test_bench.rs`) is the template for new
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
- `CLJ_PULSE_TEST_PANIC` (non-empty) makes the server register one extra
  method, `clojurePulse/__testPanic`, whose handler panics on purpose. It is
  the only way to test the panic guard end to end; `LspClient::start_with_str_env`
  sets it. Never set in normal runs, so the method does not exist in a release.
- `LspClient::start_production` sets *none* of the `CLJ_PULSE_DISABLE_*`
  variables, so stage 3 runs and clj-kondo is spawned when installed. It exists
  for `bb bench` alone — a regular test using it would behave differently per
  machine.
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
- `NsMeta.as_aliases` never appears in `requires`: an `:as-alias` namespace is
  not loaded, so the alias resolves keywords and qualified names while a usage
  spelling the full namespace stays an unresolved namespace. `core_excludes`
  holds `(:refer-clojure :exclude …)` names *and* the original half of every
  `:refer-clojure :rename` pair, since renaming a core name unmaps it.
- `declare` symbols are de-duplicated at the end of extraction: a `Declare`
  whose fqn another symbol in the same file defines is dropped, so definition
  lands on the real def while references and rename still reach the declare.
- A panicking request handler must not take the process down. tower-lsp polls
  handler futures inline, so `PanicGuard` (`src/panic_guard.rs`) wraps the
  service in `catch_unwind` and answers that one request with an internal
  error; `install_panic_hook` records payload and location in `server.log`, for
  background-task panics too. Because tower-lsp clears its pending-request map
  only when a handler *returns*, the guard also cancels each panicked id before
  it dispatches the next request — otherwise that id would answer
  `invalid request` for the rest of the session.
- Keyword occurrences carry both notations: a qualified keyword under the
  namespace it resolves to (`:my.ns/id`), an unqualified one under `:id`; a
  namespaced map (`#:user{:id 1}`) qualifies its keys with the map's prefix.
  Definition ignores the unqualified fqns — there is nothing to navigate to —
  while references and keyword completion use them. `Index::keyword_counts`
  aggregates them and is maintained at every mutation of `occurrences`
  (`replace_occurrences`, `remove_file`), never by scanning: keyword completion
  ranks by it on the keystroke path.
- A cursor inside a `:`/`::` token makes completion answer with keywords alone
  (`complete_keywords`). Each item carries a `text_edit` spanning the whole
  token, so Clojure Pulse, Calva and Neovim replace the same span whatever
  their word patterns say, and a `tier-scope-rank-label` `sort_text`:
  current-namespace keywords first, then the most-used.
- Auto-require items (a var of a namespace the file has not required) sort
  after every in-scope item — `9-tier-label`, not the pool scheme — because
  inserting a require is a bigger action than picking a name already in scope.
  They come from project namespaces and `code_action::CURATED_ALIASES` only,
  and never propose an alias the file has bound to another namespace.
- Every completion pool filters through `handlers::matching::match_score`, the
  same matcher `workspace/symbol` uses, and each item carries a `sort_text` of
  `tier-pool-name`: tier is the match (exact 0 to subsequence 3), pool is how
  local the name is (locals 0, current ns 1, refers 2, core and special forms 3,
  aliases and namespaces 4, Java 5). The response is always `isIncomplete`,
  because `tier_allowed` (no fuzzy tiers below two typed characters, no
  subsequence for namespaces) and the 50-item namespace cap withhold candidates
  a longer prefix needs. Items ship without `documentation`; `data` records
  which source to read and `completion::resolve` renders the doc on demand.
- `rename` and `prepareRename` share `references::rename_target`, so every
  rejection carries the same message from both. Only the checks that need the
  new name (validity, local capture) live in `rename`.

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
