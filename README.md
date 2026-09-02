# clj-pulse

A fast, lightweight Clojure language server.

With first-class [let-go](https://github.com/nooga/let-go) support: `.lg` projects, deps are indexed and navigable via [lgx](https://github.com/abogoyavlensky/lgx).

> [!NOTE]
> **Status:** clj-pulse is early-stage and a bit experimental, but it already
> covers much of the day-to-day Clojure workflow - go-to-definition, completion,
> hover, find references, and rename. It's under active development and
> real-world testing, so expect the occasional rough edge. Bug reports and
> feature requests via
> [issues](https://github.com/abogoyavlensky/clj-pulse/issues) are very welcome.

## Features

Language features:

- **Go to definition** - across project source, library JARs (via `jar:` URIs),
  and source-directory deps (git deps in `~/.gitlibs`, `:local/root`).
- **Autocomplete** - project symbols and `clojure.core` builtins.
- **Hover** - docstrings and signatures for the symbol under the cursor.
- **ClojureDocs** - the `clojurePulse/clojureDocs` request returns the
  [ClojureDocs](https://clojuredocs.org) entry (docstring, arglists, community
  examples, see-alsos) for the symbol at a position, resolved through the same
  alias-aware lookup as hover, or for a given `ns/name`. Served from a local
  export file the editor points at, never the network — see
  [ClojureDocs data](#clojuredocs-data).
- **Signature help** - argument hints while typing a call (after `(` and spaces).
- **Find references** - locate every usage of a symbol across the project.
- **Rename** - rename a project symbol and all of its references.
- **Keyword navigation** - go to definition and find references on namespaced
  keywords, including Integrant component keys: jump from `:my.app/db` in a
  `config.edn` system map (or an `#ig/ref`) to its `(defmethod ig/init-key ::db …)`.
- **Java interop (built-in/JDK)** - go to definition, Javadoc hover, completion,
  and signature help for JDK classes, static members, and constructors. (Instance methods
  (`(.foo obj)`), library classes, and decompilation aren't supported yet.)
- **Document symbols** - outline of the definitions in the current file.
- **Workspace symbols** - fuzzy symbol search across the whole project.
- **Code actions** - "Add require" quickfix for a qualified symbol whose
  namespace isn't required yet.
- **Diagnostics** - unresolved-namespace, unused-namespace, and
  duplicate-require warnings, updated live as you type; clj-kondo's full
  linter set as well when the binary is installed (see [Linting](#linting)).
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
- **Project types:** `deps.edn` (resolved from the `.cpcache` classpath),
  Leiningen `project.clj`, and let-go `.lg` projects, whose lgx dependencies at `lgx.edn`
  (git and `:local/root` deps under `~/.lgx/gitlibs`) are indexed and navigable.
- **Library indexing:** symbols from JAR dependencies and source-directory deps
  are indexed and navigable, with project symbols always taking precedence.
- **Live index:** incremental edits, re-index on save, and file watching keep the
  index fresh across git pulls and branch switches; files outside the project's
  `:paths` are indexed when opened.

> [!NOTE]
> **Dependency depth:** `deps.edn` and let-go projects index the full transitive
> dependency tree (from `.cpcache` and `lgx.edn`). Leiningen `project.clj`
> projects index only direct dependencies that declare an explicit version and
> already live in `~/.m2`; transitive deps and parent-inherited versions are not
> indexed yet. See [docs/MEMORY.md](docs/MEMORY.md).

## Linting

clj-pulse lints in two tiers.

The **native** tier always runs. It is built into the server, needs nothing
installed, and reports three things: `unresolved-namespace`,
`unused-namespace`, and `duplicate-require`. It is instant, index-free, and it
powers the "Add require" and "Clean namespace" quickfixes.

The **clj-kondo** tier runs when a `clj-kondo` binary is on your `PATH`. Then
clj-pulse spawns it once per lint pass, feeds it the unsaved buffer, and
publishes its findings alongside the native ones with `source: "clj-kondo"`.
That buys you clj-kondo's whole linter set (unresolved symbols, arities, syntax
errors, unused bindings, and the rest) and your existing
`.clj-kondo/config.edn`: linter levels, `:lint-as`, and excludes all apply
exactly as they do on the command line. The config is resolved from the file
being linted, so in a monorepo each subproject's own `.clj-kondo/config.edn`
wins over the workspace root's.

When a clj-kondo run succeeds it owns the three codes above, and the native
copies are dropped for that pass so no squiggle appears twice. When clj-kondo
is missing, disabled, slow, or broken, the native diagnostics are published
unchanged. Losing the binary never loses your diagnostics.

Install clj-kondo from [its own instructions](https://github.com/clj-kondo/clj-kondo/blob/master/doc/install.md),
then restart nothing: clj-pulse re-checks on every config change.

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
         :path "clj-kondo"}}
```

`:enabled` means "use clj-kondo when it is found", not "require it". Set it to
`false` to stay on native lints only; clj-pulse then never probes for the
binary or spawns it. `:path` is passed to the OS as-is, so a bare name is
resolved through `PATH` and an absolute path is used verbatim. Both keys apply
live, with no restart.

The VS Code extension exposes the same two settings as
`clojurePulse.kondo.enabled` and `clojurePulse.kondo.path`, and shows which
tier is active in its status-bar tooltip.

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

Install [Calva](https://calva.io/) extension, then add to `settings.json`:

```json
{
  "calva.clojureLspPath": "/path/to/clj-pulse"
}
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
first request, and without a configured path the request answers with an
error rather than an empty entry. Notes are never served — ClojureDocs states
a license for examples (CC0) but none for notes.

## Configuration

clj-pulse reads an optional `.clj-pulse/config.edn` at the workspace root and
falls back to `.clj-kondo/config.edn` where the keys overlap. It understands
three keys: `:projects` and `:lint-as`, below, and `:kondo`, documented under
[Linting](#settings).

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
bb check      # run all checks (fmt + lint + test)
bb outdated   # check outdated deps 
bb build      # build the dev binary
bb release    # build release binary
bb tag        # create and push new git tag based on version form Cargo.toml
```

> [!NOTE]
> To run `bb outdated` you need to have `cargo-outdated` installed. You can install it with `cargo install cargo-outdated`.

## License

MIT License. Copyright (c) 2026 Andrey Bogoyavlenskiy.
