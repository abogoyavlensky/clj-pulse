# clj-lsp

A minimal, fast Clojure LSP server.

## V1 Scope

- Jump to definition (project source)
- Autocomplete (project symbols + clojure.core builtins)
- Hover / documentation

## Prerequisites

Install [mise](https://mise.jdx.dev/) for managing tool versions, then:

```sh
mise install
```

This installs the correct versions of Rust and Babashka.

## Development

```sh
bb fmt        # fix code formatting
bb fmt-check  # check formatting without fixing
bb lint       # run clippy linter
bb test       # run tests
bb check      # run all checks (fmt + lint + test)
bb outdated   # check outdated deps 
bb build      # build the dev binary
bb release    # build release binary
```

> [!NOTE]
> To run `bb outdated` you need to have `cargo-outdated` installed. You can install it with `cargo install cargo-outdated`.

## Editor Setup

### VS Code

Install [Calva](https://calva.io/) extension, then add to `settings.json`:

```json
{
  "calva.clojureLspPath": "/path/to/clj-lsp"
}
```

### Zed

Install [Clojure](https://zed.dev/extensions/clojure#details) extension, then add to `~/.config/zed/settings.json`:

```json
{
  "lsp": {
    "clojure-lsp": {
      "binary": {
        "path": "/path/to/clj-lsp",
      },
    },
  },
}
```


## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for data flow and design decisions.
