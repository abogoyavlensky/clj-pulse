# clj-pulse

A fast, lightweight Clojure language server.

With first-class [let-go](https://github.com/nooga/let-go) support: `.lg` projects, deps are indexed and navigable via [lgx](https://github.com/abogoyavlensky/lgx).

> [!NOTE]
> **Status:** clj-pulse is early-stage and a bit experimental, but it already
> covers much of the day-to-day Clojure workflow - go-to-definition, completion,
> hover, find references, and rename. It's under active development and
> real-world testing, so expect the occasional rough edge - though a request
> that goes wrong now fails on its own instead of taking the server down with
> it. Bug reports and
> feature requests via
> [issues](https://github.com/abogoyavlensky/clj-pulse/issues) are very welcome.
> What comes next is in [docs/ROADMAP.md](docs/ROADMAP.md).

## Features

Language features:

- **Go to definition** - across project source, library JARs (via `jar:` URIs),
  and source-directory deps (git deps in `~/.gitlibs`, `:local/root`).
- **Autocomplete** - locals, project symbols, `:refer`red and alias-qualified
  vars (including `:refer :all` and `:use`), namespace and alias names,
  `clojure.core`, special forms, and JDK classes. Names match fuzzily (exact,
  prefix, substring, subsequence) and rank by match quality first, then by how
  local the name is. Docstrings load per item through `completionItem/resolve`,
  so a long list stays cheap.
- **Keyword completion** - typing `:` or `::` offers the keywords the project
  already uses, in the notation being typed: `::name` and `::alias/name` under
  `::`, `:name` and `:ns/name` under a single colon. The current namespace's
  keywords come first, then the most-used ones.
- **Auto-require on accept** - completion also offers vars from namespaces the
  file has not required yet, labelled `alias/name`; accepting one inserts the
  `:require` along with the name. The pool is every project namespace, aliased
  by its last segment, plus the conventional aliases (`str`, `set`, `io`,
  `edn`, `walk`, `pp`, `async`, `sh`). An alias the file has already bound to
  something else is never proposed.
- **Hover** - docstrings and signatures for the symbol under the cursor.
- **ClojureDocs** - the `clojurePulse/clojureDocs` request returns the
  [ClojureDocs](https://clojuredocs.org) entry (docstring, arglists, community
  examples, see-alsos) for the symbol at a position, resolved through the same
  alias-aware lookup as hover, or for a given `ns/name`. Served from a local
  export file the editor points at, never the network — see
  [ClojureDocs data](#clojuredocs-data).
- **Signature help** - argument hints while typing a call (after `(` and spaces).
- **Find references** - locate every usage of a symbol across the project.
- **Rename** - rename a project symbol and all of its references, or a local
  binding (params, `let`/`loop`/`for` bindings, destructured names) within
  its scope. The editor's rename box opens on the exact token that will change,
  and names that cannot be renamed - library and built-in symbols,
  `:keys`-destructured bindings - are refused up front with a reason.
- **Keyword rename** - rename a qualified keyword across the project. Each site
  keeps the notation it was written in, because only the name at the end of the
  token is replaced: `::db`, `::alias/db` and `:my.app/db` all become `::store`,
  `::alias/store` and `:my.app/store`. Integrant `config.edn` files are rewritten
  with the sources. Unqualified keywords, keywords of a library namespace, and
  keywords read through `{::keys [db]}` destructuring (where the name is also the
  binding) are refused rather than half-renamed.
- **Keyword navigation** - go to definition and find references on namespaced
  keywords, including Integrant component keys: jump from `:my.app/db` in a
  `config.edn` system map (or an `#ig/ref`) to its `(defmethod ig/init-key ::db …)`.
- **Java interop (built-in/JDK)** - go to definition, Javadoc hover, completion,
  and signature help for JDK classes, static members, and constructors. (Instance methods
  (`(.foo obj)`), library classes, and decompilation aren't supported yet.)
- **Highlight occurrences** - the editor underlines every occurrence of the
  symbol under the cursor in the current buffer, marking its definition as a
  write and each usage as a read. Locals resolve by scope, so a parameter that
  shadows a var highlights only itself.
- **Expand selection** - `textDocument/selectionRange` grows the selection one
  form at a time along the parse tree: the name half of `str/join`, then the
  whole symbol, then the call, then the enclosing `defn`.
- **Document symbols** - outline of the definitions in the current file.
- **Workspace symbols** - fuzzy symbol search across the whole project.
- **Code actions** - "Add require" quickfix for a qualified symbol whose
  namespace isn't required yet, and "Clean namespace" (`source.organizeImports`)
  that drops unused and duplicate requires.
- **Diagnostics** - unresolved-namespace, unused-namespace, duplicate-require,
  unused-binding, and unused-private-var warnings, updated live as you type;
  clj-kondo's full linter set as well when the binary is installed (see
  [Linting](#linting)).
- **Indent-on-Enter** - pressing Enter indents the new line to the structurally
  correct column (`textDocument/onTypeFormatting`): vectors, maps, and
  non-symbol-headed lists align to their first element; symbol-headed lists get
  a 2-space body indent. In VS Code enable `editor.formatOnType` for Clojure
  (the Clojure Pulse extension turns it on by default). Using Parinfer in
  Paren/Smart mode? Set `editor.formatOnType: false` for Clojure — Parinfer
  manages indentation there. Parinfer's Indent Mode is complementary
  (clj-pulse indents; Parinfer places brackets).
- **Ignored-form dimming** - the server reports the ranges of `#_` discard
  forms and `(comment …)` blocks over a `clojurePulse/ignoredForms` request; the
  editor extension dims them (brackets included, nested and multi-line) with a
  decoration a syntax grammar can't produce. No theme configuration needed.

Clojure & project support:

- **File types:** `.clj`, `.cljs`, `.cljc`, `.lg`.
- **ns forms:** `:as`, `:as-alias`, `:refer` (including `:refer :all` and
  `(:use ns)`), `:rename`, `:refer-clojure :exclude` / `:rename`, `:import`,
  reader conditionals, and legacy prefix lists `(clojure [set :as s] string)`.
  `declare` is indexed too, so a name that is only declared still navigates.
- **Project types:** `deps.edn` (resolved from the `.cpcache` classpath),
  Leiningen `project.clj`, and let-go `.lg` projects, whose lgx dependencies at `lgx.edn`
  (git and `:local/root` deps under `~/.lgx/gitlibs`) are indexed and navigable.
- **Library indexing:** symbols from JAR dependencies and source-directory deps
  are indexed and navigable, with project symbols always taking precedence.
- **Live index:** incremental edits, re-index on save, and file watching keep the
  index fresh across git pulls and branch switches; files outside the project's
  `:paths` are indexed when opened.

> [!NOTE]
> **Dependency depth:** every project type indexes the full transitive
> dependency tree — deps.edn from the resolved classpath, let-go from
> `lgx.edn`, and Leiningen from `lein classpath`, run in the background and
> enabled by default for the workspace root. Where that command is turned off
> or fails, a Leiningen project falls back to the direct dependencies that name
> an explicit version and already live in `~/.m2`. See
> [docs/MEMORY.md](docs/MEMORY.md).

## Performance

`bb bench` runs clj-pulse and [clojure-lsp](https://clojure-lsp.io) through the
same client, over stdio, with the same requests, on two corpora pinned by
commit. Every metric is behavioral. The startup rows time the wait until a
`textDocument/definition` lands where it should; memory and the latency
medians are sampled only after the server goes quiet and no child process of
it is still running. Both servers run at their defaults.

Warm runs — a second start with what the first left cached — on one Linux
container (5 cores, 11 GiB, 2026-09-10): clj-pulse 0.5.0, clojure-lsp
2026.07.06-14.34.19, metabase at `42a8e9f7`, clj-kondo at `13a32d1c`.

**metabase** (1 400+ files, 43 164 symbols):

| Metric | clj-pulse | clojure-lsp |
|---|---|---|
| Time to first definition | 3.1 s | 60 s |
| Time to first definition inside a dependency | 3.9 s | 60 s |
| Memory once settled | 353 MiB | 1 798 MiB |
| Definition (median of 20) | 34 ms | 7 ms |
| Keystroke → diagnostics, 452 KiB file | 381 ms | 1 371 ms |
| Keystroke → diagnostics, 251 KiB file | 911 ms | 925 ms |

**clj-kondo** (400 files, 2 252 symbols):

| Metric | clj-pulse | clojure-lsp |
|---|---|---|
| Time to first definition | 520 ms | 2.4 s |
| Time to first definition inside a dependency | 521 ms | 2.4 s |
| Memory once settled | 90 MiB | 273 MiB |
| Definition (median of 20) | 16 ms | 3 ms |
| Keystroke → diagnostics, 233 KiB file | 789 ms | 759 ms |

What the tables do not say:

- The two servers do different work at startup. clojure-lsp analyzes the whole
  classpath through clj-kondo before it answers; clj-pulse indexes the
  project's sources first and the classpath in the background, and reads JAR
  entries lazily. The first two rows are that difference. From cold, with no
  caches at all, the same metabase row reads 3.4 s against 293 s.
- clojure-lsp answers a definition faster once it is up, from a fuller
  analysis.
- The 452 KiB row is clj-pulse's native lint tier alone: that file is above
  `:kondo {:live-max-kb 256}`, so clj-kondo sits out the keystroke path. The
  251 KiB row is the same measurement with clj-kondo in it. clojure-lsp runs
  its embedded clj-kondo on every keystroke either way.
- One Linux container, one run each. macOS numbers are not in yet.

Cold tables, the full method, and the caveats in detail are in
[docs/MEMORY.md](docs/MEMORY.md). To reproduce: `bb bench`.

## Linting

clj-pulse lints in two tiers.

The **native** tier always runs. It is built into the server, needs nothing
installed, and reports five things: `unresolved-namespace`,
`unused-namespace`, `duplicate-require`, `unused-binding`, and
`unused-private-var`. It is instant, and it powers the "Add require" and
"Clean namespace" quickfixes.

The **clj-kondo** tier runs when a `clj-kondo` binary is on your `PATH`. Then
clj-pulse spawns it once per lint pass, feeds it the unsaved buffer, and
publishes its findings alongside the native ones with `source: "clj-kondo"`.
That buys you clj-kondo's whole linter set (unresolved symbols, arities, syntax
errors, unused bindings, and the rest) and your existing
`.clj-kondo/config.edn`: linter levels, `:lint-as`, and excludes all apply
exactly as they do on the command line. The config is resolved from the file
being linted, so in a monorepo each subproject's own `.clj-kondo/config.edn`
wins over the workspace root's.

When a clj-kondo run succeeds it owns the five codes above, and the native
copies are dropped for that pass so no squiggle appears twice. When clj-kondo
is missing, disabled, slow, or broken, the native diagnostics are published
unchanged. Losing the binary never loses your diagnostics.

Install clj-kondo from [its own instructions](https://github.com/clj-kondo/clj-kondo/blob/master/doc/install.md),
then restart nothing: clj-pulse re-checks on every config change.

clj-pulse looks for `clj-kondo` (and the `clojure` CLI it runs for classpath
resolution) on `PATH` first, then in the usual install directories: mise shims
(`~/.local/share/mise/shims`), Homebrew (`/opt/homebrew/bin`,
`/usr/local/bin`, Linuxbrew), `~/.cargo/bin`, `~/.local/bin` and `~/bin`. That
covers an editor started from the Dock or an app menu, whose `PATH` lacks what
your shell adds. A mise shim picks the version the project's mise config pins,
because clj-pulse runs it from the file's own directory. When clj-kondo is not
found, the log line (and the extension's lint status) says where it looked.

### Cross-file linters need a `.clj-kondo` directory

clj-kondo's cross-file linters (`invalid-arity`, `unresolved-var`) read a cache
of the signatures your project and its dependencies define. It writes that
cache into a `.clj-kondo` directory, and it never creates one itself. So run
this once per project:

```bash
mkdir .clj-kondo
```

With the directory present, clj-pulse scans your resolved classpath in the
background the first time it indexes the project, so library arities are known
without opening a single file. Editors that support work-done progress show
this as "Linting classpath (clj-kondo)". Without the directory, buffer linting
still works; only the cross-file linters stay quiet.

### Settings

```clojure
;; .clj-pulse/config.edn - defaults made explicit
{:kondo {:enabled true
         :path "clj-kondo"
         :live-max-kb 256}}
```

`:enabled` means "use clj-kondo when it is found", not "require it". Set it to
`false` to stay on native lints only; clj-pulse then never probes for the
binary or spawns it. `:path` names a program, not a command line: a bare name
is resolved through `PATH` and the install directories above, and an absolute
path is used verbatim. `mise exec -- clj-kondo` cannot work there; use the
path `mise which clj-kondo` prints, or the shim. All three keys apply live,
with no restart.

See [docs/SETTINGS.md](docs/SETTINGS.md) for these three keys beside every
other setting, with the initialization options and environment variables.

`:live-max-kb` keeps clj-kondo off the keystroke path for very large files.
While you type in a buffer larger than this many KiB, each pass publishes the
native tier alone; opening and saving the file still run clj-kondo, so its full
findings are never more than a save away. `0` removes the limit. clj-kondo
takes close to a second on a 450 KiB file, and that time is its own, so this
is the one lever over when it runs.

The VS Code extension exposes `clojurePulse.kondo.enabled` and
`clojurePulse.kondo.path`, and shows which tier is active in its status-bar
tooltip. `clojurePulse.kondo.liveMaxKb` is pending in the extension; until it
ships, set `:live-max-kb` in `.clj-pulse/config.edn`.

## Installation

### Homebrew (macOS, Linux)

```sh
brew install abogoyavlensky/tap/clj-pulse
```

### mise (macOS, Linux)

```sh
mise use -g github:abogoyavlensky/clj-pulse
```

### Manual download

Download the archive for your platform from
[releases](https://github.com/abogoyavlensky/clj-pulse/releases), unpack it,
and put the binary on your `PATH`. Checksums for all archives are in
`checksums.txt` attached to each release.

> [!NOTE]
> macOS quarantines binaries downloaded through a browser, so Gatekeeper
> refuses to run them ("cannot be opened because the developer cannot be
> verified"). Remove the attribute with
> `xattr -d com.apple.quarantine ./clj-pulse`. Installs via mise are not
> affected.

## Editor Setup

### VS Code

Install the [Clojure Pulse](https://github.com/abogoyavlensky/clojure-pulse-vscode)
extension. It finds `clj-pulse` on your `PATH`; to use another binary, set:

```json
{
  "clojurePulse.server.path": "/path/to/clj-pulse"
}
```

Alternatively, with [Calva](https://calva.io/) installed, point it at the
server instead of clojure-lsp:

```json
{
  "calva.clojureLspPath": "/path/to/clj-pulse"
}
```

### Neovim

Neovim 0.11+ with the built-in LSP client, no plugin needed:

```lua
vim.lsp.config("clj_pulse", {
  cmd = { "clj-pulse" },
  filetypes = { "clojure" },
  root_markers = { "deps.edn", "project.clj", "lgx.edn", ".git" },
})
vim.lsp.enable("clj_pulse")
-- let-go sources:
vim.filetype.add({ extension = { lg = "clojure" } })
```

#### Library navigation

Go-to-definition into a dependency answers with a `jar:` URI. Neovim's built-in
client opens an empty buffer for it and fires `BufReadCmd`; the handler below
fills that buffer from the server's `clojure/dependencyContents`. Save it as
`~/.config/nvim/lua/clj_pulse_jar.lua`:

```lua
-- Opens `jar:` locations in Neovim.
--
-- Go-to-definition into a library lands on a `jar:file:///…!/clojure/core.clj`
-- URI. Neovim's built-in LSP client creates an empty buffer for it and fires
-- `BufReadCmd`; clj-pulse serves the entry's text through
-- `clojure/dependencyContents`. Call `setup()` once, after `vim.lsp.enable`.
local M = {}

--- @param opts? { client_name?: string }  `client_name` must match the name
--- the LSP client is registered under (default `"clj_pulse"`).
function M.setup(opts)
  local client_name = (opts or {}).client_name or "clj_pulse"
  vim.api.nvim_create_autocmd("BufReadCmd", {
    -- `jar:file://…` is not a URL Neovim recognizes (the scheme is not
    -- followed by `://`), so it names the buffer relative to the working
    -- directory: the pattern has to allow that prefix, and the URI is read
    -- back out of the name.
    pattern = { "jar:*", "*/jar:*" },
    callback = function(args)
      local uri = args.file:match("jar:.*")
      local client = vim.lsp.get_clients({ name = client_name })[1]
      if not uri or not client then
        return
      end
      local res = client:request_sync("clojure/dependencyContents", { uri = uri }, 5000)
      if not res or res.err or type(res.result) ~= "string" then
        return
      end
      vim.bo[args.buf].modifiable = true
      vim.api.nvim_buf_set_lines(args.buf, 0, -1, false, vim.split(res.result, "\n"))
      vim.bo[args.buf].filetype = "clojure"
      vim.bo[args.buf].buftype = "nofile"
      vim.bo[args.buf].modifiable = false
      vim.bo[args.buf].modified = false
    end,
  })
end

return M
```

and call it after `vim.lsp.enable`, with `client_name` matching the name the
client is registered under (`clj_pulse` above, which is the default):

```lua
require("clj_pulse_jar").setup()
```

The same file ships as [`editors/nvim/jar.lua`](editors/nvim/jar.lua); vendor it
and load it in one line instead:

```lua
dofile("/path/to/clj-pulse/editors/nvim/jar.lua").setup()
```

### Zed

Install [Clojure](https://zed.dev/extensions/clojure#details) extension, then add to `~/.config/zed/settings.json`:

```json
{
  "lsp": {
    "clojure-lsp": {
      "binary": {
        "path": "/path/to/clj-pulse",
      },
    },
  },
}
```

> [!NOTE]
> Currently, Zed editor, `clj-pulse` works only with project's own files, no libs inspection yet.

### ClojureDocs data

`clojurePulse/clojureDocs` reads a local copy of the ClojureDocs export. The
editor passes its path at startup:

```json
{ "initializationOptions": { "clojuredocs": { "path": "/path/to/clojuredocs-export.json" } } }
```

Clojure Pulse bundles a stripped copy and sends this automatically. Any other
client can download the official export from
<https://clojuredocs.org/clojuredocs-export.json> and point at it: the server
reads the export's own shape, every field optional. The file is read on the
first request that resolves or names a var; without a configured path such a
request answers with an error rather than an empty entry (a position with no
symbol under it still answers `{"symbol": null, "entry": null}`). Notes are never served — ClojureDocs states
a license for examples (CC0) but none for notes.

## Configuration

clj-pulse reads an optional `.clj-pulse/config.edn` at the workspace root and
falls back to `.clj-kondo/config.edn` where the keys overlap. It understands
three keys: `:projects` and `:lint-as`, below, and `:kondo`, documented under
[Linting](#settings). Every key, its default, and the Clojure Pulse setting
that matches it are listed in [docs/SETTINGS.md](docs/SETTINGS.md).

`:projects` controls per-project classpath resolution. clj-pulse detects every
directory holding a `deps.edn`, `project.clj`, or `lgx.edn` (up to four levels
deep, honoring `.gitignore`) and automatically indexes the sources and cached
classpath (`.cpcache`) of all of them — a monorepo needs no configuration at
all. On top of that, each deps.edn or Leiningen project can run a shell
command that resolves its full classpath, so dependencies declared under
aliases (`:test`, `:dev`, …) are indexed and navigable too (lgx projects
resolve their dependencies internally and never run a command). The command
runs in the project's directory
and its last stdout line is taken as the classpath; with a warm `.cpcache`
the clojure CLI skips the JVM entirely, and on the first resolve — or after a
deps.edn change — it may download dependencies. By default the command is
enabled only for the workspace root:

```clojure
;; .clj-pulse/config.edn — defaults made explicit
{:projects [{:path "."             ; "." is the workspace root
             :classpath {:enabled true
                         :cmd "clojure -A:dev:test -Spath"}}
            {:path "apps/backend"  ; subprojects default to :enabled false
             :classpath {:enabled false
                         :cmd "clojure -A:dev:test -Spath"}}]}
```

Entries are overrides: every detected project exists whether or not it is
listed, and an entry changes only the keys it names. The default `:cmd` is
`clojure -A:dev:test -Spath` for deps.edn projects and `lein classpath` for
Leiningen ones; change it to select other aliases or a different tool. Set
`:enabled true` on a subproject to resolve its full classpath too, or
`:enabled false` on the root to opt out — a deps.edn project then indexes
only what `.cpcache` already holds (a Leiningen project falls back to the
direct dependency JARs named in `project.clj`; lgx resolution is unaffected).
Listing a path detection skipped (for example a
gitignored checkout with its own `deps.edn`) adds it as a project. Editing
the config applies live, no restart needed.

Editors can also force a full refresh with the custom `clojurePulse/rescan`
request: it re-runs project detection, re-reads the config, and re-resolves
every enabled project's classpath — the way to retry a failed resolution or
pick up a subproject created inside a gitignored directory, where no file
watcher ever fires. The request returns null immediately and the work runs in
the background, emitting `clojurePulse/librariesChanged` as it progresses —
clients should simply re-request on each notification (one is guaranteed at
the end even when nothing changed, so the panel never waits forever). While a
classpath command
runs, clj-pulse reports standard LSP work-done progress
("Resolving classpath: …") to clients that advertise the
`window.workDoneProgress` capability, so the editor shows why library
navigation isn't ready yet.

`:lint-as` (also read from `.clj-kondo/config.edn`) tells clj-pulse to treat a
custom macro like a built-in `def` form so the name it introduces becomes
navigable:

```clojure
;; .clj-pulse/config.edn  (or .clj-kondo/config.edn)
{:lint-as {my.app/defcomponent clojure.core/def}}
```

With that mapping, go-to-definition, hover, find-references, and the document
outline all resolve a name defined by `(defcomponent thing …)`. clj-pulse merges
the two files (with `.clj-pulse/config.edn` winning on conflicts) and watches
them, reloading `:lint-as` when either changes, with no restart needed. A
project that
already configures `:lint-as` for clj-kondo works with no extra setup. Only
mappings to `def`-family forms (`def`, `defn`, `defmethod`, …) take effect;
others (such as `clojure.core/for`) are ignored.

`.clj-pulse/` also holds generated data (`jar-cache/`, `server.log`), so commit
`config.edn` and gitignore the rest.

## Development

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
bb outdated   # check outdated deps 
bb build      # build the dev binary
bb release    # build release binary
bb tag        # create and push new git tag based on version form Cargo.toml
```

End-to-end checks (see [docs/DEV_SETUP.md](docs/DEV_SETUP.md)):

```sh
bb e2e        # real binary over stdio, framed JSON-RPC like an editor
bb e2e-real   # same, against a real Maven classpath (needs the clojure CLI)
bb e2e-nvim   # through headless Neovim's built-in LSP client
bb e2e-calva  # real VS Code + Calva under Xvfb
bb e2e-pulse  # real VS Code + the Clojure Pulse extension under Xvfb
```

`bb bench` checks out [metabase](https://github.com/metabase/metabase) and
[clj-kondo](https://github.com/clj-kondo/clj-kondo) at pinned commits under
`.tmp/bench/`, downloads the pinned clojure-lsp release beside them, and runs
four configurations per corpus — each server cold and warm — printing a table
and a `BENCH_JSON` line per row. `bb bench metabase` or `bb bench clj-kondo`
runs one. The recorded tables are in [docs/MEMORY.md](docs/MEMORY.md), and the
warm summary is under [Performance](#performance).

> [!NOTE]
> To run `bb outdated` you need to have `cargo-outdated` installed. You can install it with `cargo install cargo-outdated`.

## License

MIT License. Copyright (c) 2026 Andrey Bogoyavlenskiy.
