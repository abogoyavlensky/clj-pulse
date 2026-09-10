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
  (headless Neovim's built-in LSP client, `scripts/e2e_nvim.lua`). It resolves
  the fixture's classpath first and drives `editors/nvim/jar.lua` — the `jar:`
  handler the README tells Neovim users to install — so a definition into the
  clojure JAR opens with its source.
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

- `bb bench [metabase|clj-kondo]` — the release binary and clojure-lsp, each
  driven by the same client through the same requests, on two corpora pinned by
  commit (`bb bench` runs both; they are checked out under `.tmp/bench/` on
  first use, and the pinned clojure-lsp release is downloaded and checksum-
  verified beside them). Four configurations per corpus, in a fixed order:
  clj-pulse cold, clj-pulse warm, clojure-lsp cold, clojure-lsp warm. Every
  metric is behavioral, so it means the same thing for both servers — time
  until a `textDocument/definition` on a project symbol *lands where it should*,
  the same into a JAR, RSS once the server is settled (no traffic for 2 s and no
  child process still working), and medians of 20 definitions and 20 keystrokes
  to `publishDiagnostics`. Each row also prints as one `BENCH_JSON` line, so a
  later run can be diffed. Not a pass/fail gate; compare against the tables in
  [docs/MEMORY.md](docs/MEMORY.md).

- `bb soak [metabase|clj-kondo]` — one long-lived server driven through rounds
  of realistic churn on the same pinned corpora: buffer edits, saves, on-disk
  modifications, creations, deletions, renames, and every fifth round a
  100-file batch delivered in a *single* `didChangeWatchedFiles`, the shape a
  branch switch has. Every action carries a witness — a uniquely named var
  whose `workspace/symbol` answer must change because of it — and a batch that
  has not landed within `CLJ_PULSE_SOAK_CONVERGE_TIMEOUT` fails the run. At
  each checkpoint the buffers are closed, the corpus is restored to its pinned
  commit, and a *freshly started* server is held to the same settle rule and
  asked the same probe set: definition, references, `documentSymbol` and
  `workspace/symbol` must match (as sets where order is the index's business),
  completion and hover only have to answer. RSS is sampled there, quiesced and
  with nothing open, which is the only state two checkpoints share. The run
  fails on a divergence, a witness that never landed, any JSON-RPC error
  answer, a `panicked at` in the accumulated `server.log`, or RSS growth past
  `CLJ_PULSE_SOAK_RSS_GROWTH` (1.5x) — a gate, unlike `bb bench`. Each
  checkpoint prints a `SOAK_JSON` line, and the seed is printed on every run:
  `bb soak clj-kondo <seed>` replays a failure exactly. Defaults to clj-kondo
  and 20 rounds (about half a minute); `bb soak metabase` is the long one.

| Gate | Run when |
|---|---|
| `bb e2e` | every server behavior change |
| `bb e2e-pulse` | any client-visible change |
| `bb e2e-calva` | definition, `jar:`, or location-shape changes |
| `bb e2e-nvim` | new capabilities or protocol changes |
| `bb bench` | before a release, and after index or extractor changes |
| `bb soak` | before a release, and after index, watcher, or document-store changes |

## Testing notes

- The e2e harness (`LspClient` in `tests/common/mod.rs`, shared by
  `test_e2e.rs`, `test_bench.rs` and `test_soak.rs`) is the template for new
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
- `tests/common/sampling.rs` and `tests/common/sites.rs` hold what the bench
  and the soak must agree about: how a settled server is recognized (the stage
  lines, the quiet window, the no-child-process rule), how RSS is read, and
  which cursor positions in a real corpus are worth asking a question about.
  Neither gate keeps a private copy.
- `CLJ_PULSE_TEST_PANIC` (non-empty) makes the server register one extra
  method, `clojurePulse/__testPanic`, whose handler panics on purpose. It is
  the only way to test the panic guard end to end; `LspClient::start_with_str_env`
  sets it. Never set in normal runs, so the method does not exist in a release.
- `LspClient::start_production` sets *none* of the `CLJ_PULSE_DISABLE_*`
  variables, so stage 3 runs and clj-kondo is spawned when installed. It exists
  for `bb bench` and `bb soak` alone — a regular test using it would behave
  differently per machine.
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
- The document store keeps one tree-sitter tree per open document. `open`
  parses once; `apply_changes` turns every incremental change into a
  `Tree::edit` and reparses incrementally before it returns, so the tree in
  the store always matches the rope (eager, never lazy). Handlers and the lint
  pass read `DocumentStore::snapshot` (text and tree from one lock) and call
  the extractor's `_tree` variants; never `text()` plus a fresh parse. `text()`
  is for text-only work such as `word_at` and indent-on-Enter.
- `:kondo {:live-max-kb}` applies to `LintTrigger::Change` alone: a keystroke
  on a larger buffer publishes the native set, while open, save, and an engine
  change always run clj-kondo. A save bumps the document's lint epoch so a
  change pass still waiting out its debounce stands down; the version alone
  cannot tell a save from the edit just before it.
- Rename resolves locals structurally (`extractor::local_references_at`)
  *before* the fqn path, so a param shadowing a global only ever edits itself;
  a `:keys`/`:strs`/`:syms`-destructured binding is rejected, since its name is
  also the key being read.
- Child processes (`clj-kondo`, the classpath shell) get PATH plus the
  well-known install directories in `tools::well_known_dirs` (mise shims,
  Homebrew, `~/.cargo/bin`, …), appended after the user's entries, so a
  Dock-launched editor's bare PATH still finds them. `CLJ_PULSE_TOOL_DIRS`
  (PATH-style) replaces that list; the discovery e2e tests set it, so they
  never depend on what the host has installed. A bare `:kondo {:path}` is
  resolved to a full path at probe time and that file is what lints; the
  probe's failure reason travels as `detail` on `clojurePulse/lintStatus`.
- `CLJ_PULSE_DISABLE_KONDO` (non-empty) forces `:kondo {:enabled false}`, the
  twin of `CLJ_PULSE_DISABLE_CLASSPATH_CLI`. `LspClient::start` sets it, so no
  test depends on a host clj-kondo; kondo tests opt in with
  `start_with_kondo` / `start_with_kondo_env`, which put the committed fake
  (`tests/fixtures/fake-clj-kondo/clj-kondo`) first on the child's PATH.
- Classpath libraries come in two shapes: JARs (`SymbolSource::Jar`, navigated
  via `jar:` URIs) and source directories — git deps in `~/.gitlibs`,
  `:local/root` deps (`SymbolSource::Dir`, navigated via plain `file:` URIs).
- Files outside deps.edn `:paths` are indexed on `didOpen`.
- Integrant EDN configs are found project-wide, not under `:paths`: the scan
  walks each project dir to `scanner::EDN_SCAN_MAX_DEPTH` (gitignore respected)
  and keeps what `is_integrant_edn` accepts. `:paths` is a classpath decision
  and the config's location is not one — `resources/config.edn` is routinely
  absent from it, and Leiningen `:resource-paths` never becomes a source root.
  A config that is gitignored or deeper than that is indexed on `didOpen`, and
  the `**/*.edn` watcher keeps every indexed one fresh. A config the scan
  cannot see makes references skip it and a keyword rename silently leave it
  pointing at the old key.
- Only top-level `:paths` in deps.edn counts (not `:paths` inside `:aliases`).
- Defining macros resolve by fqn, never by bare name: the user's `:lint-as` map
  first, then the built-in table `DefKind::from_macro_fqn`
  (`clojure.test/deftest` and friends), then — for a *qualified* head alone —
  the def form its name part names (`extractor::head_def_kind`): `mu/defn`,
  `s/defn`, `p/defn-` and `mu/defmethod` define what `defn`, `defn-` and
  `defmethod` define, whatever library the qualifier points at. `:lint-as` is
  consulted first, so it always outranks the fallback, and the fallback applies
  only when the form's second child is a symbol, which leaves `(s/def ::user …)`
  naming a keyword. Definition extraction (`process_top_level_list`) and the
  occurrence walker (`walk_list`) share the resolver, or the index and the
  occurrences disagree about what binds; `walk_scope` has neither ns metadata
  nor `ExtractConfig`, so it applies the name-part rule alone — a head
  `:lint-as` maps to a *non*-fn kind still binds its vector there (ROADMAP
  backlog, 2026-09-10). A `:-` marker annotates rather than binds, wherever it
  appears: `[x :- s/Int]` binds `x` and reads `s/Int`, and a return schema
  (`(mu/defn f :- [:vector :int] [xs] …)`) is an expression, so the parameter
  vector is the one after it. `NsMeta.refer_all` records
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
  namespaced map (`#:user{:id 1}`) qualifies its keys with the map's prefix, in
  EDN configs as well as in Clojure sources. A namespaced `:keys` destructuring
  entry (`{::keys [id]}`, `{:keys [user/id]}`) is an occurrence of the key it
  reads; `:syms` and `:strs` read symbols and strings, so they are not.
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
- Only vars reach the symbol completion pools (`completion::is_var_symbol`): an
  Integrant key is indexed as a symbol whose fqn is the keyword it defines
  (`:app.system/database`), so the current-namespace, alias-qualified,
  `:refer :all` and auto-require pools all skip it — offering `database` or
  `sys/database` would name nothing. Keys reach the user through
  `complete_keywords`.
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
  new name (validity, local capture, the colon a keyword's new name must not
  carry) live in `rename`.
- A keyword rename edits the name its token *ends* with, never the token:
  `::db`, `::alias/db` and `:my.app/db` all end in `db`, so one rule rewrites
  every notation and the notation takes care of itself. Columns are UTF-16
  units, and a token that is not a keyword ending in that name refuses the whole
  rename — all-or-nothing, since rewriting the rest would leave that site
  reading the old key. The known such shape is a `{::keys [db]}` /
  `{:keys [app/db]}` entry: a symbol that reads the key *and* binds a local of
  that name, so it gets the destructuring message. Sites come from occurrences,
  the `IntegrantKey`-style definition, and the live definitions of every open
  project buffer (one just typed has no indexed symbol); library files and
  unqualified or library-namespaced keywords are refused or filtered out in
  `rename_target`, so `prepareRename` refuses exactly what `rename` would.
- `documentHighlight` resolves in the same order references does:
  `references::local_refs_at` first and authoritatively, then
  `resolve_fqn_at`. It never leaves the buffer — occurrences and definitions
  come from one `extract_full_tree` of the open document, so an unsaved edit
  highlights at its current ranges. A definition in the file is `WRITE`, a
  usage `READ`; a keyword fqn (leading `:`) is `TEXT` throughout, since a
  keyword has no read/write distinction.
- `selectionRange` answers one chain per requested position, built from
  `extractor::node_path_at` over the cached tree. A qualified `sym_lit` or
  `kwd_lit` adds its `name` child as the innermost step only when the cursor
  is inside that child, so `al|ias/name` has no namespace-only step; equal
  consecutive ranges collapse, and a position with no containing named node
  gets a single zero-width range rather than being dropped.

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

Every user-facing setting — `.clj-pulse/config.edn`, `initializationOptions`,
environment variables — is tabulated with its default in
[docs/SETTINGS.md](docs/SETTINGS.md), which is read from the parsers and has to
change with them.

See [docs/DEV_SETUP.md](docs/DEV_SETUP.md) for the full development &
verification environment: the two environments (maintainer's Calva/macOS vs the
headless CI box), tooling versions, and what each `bb e2e*` task covers.
