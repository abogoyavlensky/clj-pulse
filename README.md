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
- **Diagnostics** - unresolved-namespace warnings, updated live as you type.

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

## Installation

### Homebrew (macOS, Linux)

```sh
brew install abogoyavlensky/tap/clj-pulse
```

### mise (macOS, Linux)

```sh
mise use -g ubi:abogoyavlensky/clj-pulse
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

> [NOTE!]
> Currently, Zed editor, `clj-pulse` works only with project's own files, no libs inspection yet.

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
