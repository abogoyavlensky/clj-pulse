# clj-lsp

A minimal and fast Clojure LSP server.

## V1 Scope

- Jump to definition (project source)
- Autocomplete (project symbols + clojure.core builtins)
- Hover / documentation

## Installation

### mise (macOS, Linux)

```sh
mise use -g ubi:abogoyavlensky/clj-lsp
```

To find the binary path for editor configuration:

```sh
mise which clj-lsp
```

### Manual download

Download the archive for your platform from
[releases](https://github.com/abogoyavlensky/clj-lsp/releases), unpack it,
and put the binary on your `PATH`. Checksums for all archives are in
`checksums.txt` attached to each release.

> [!NOTE]
> macOS quarantines binaries downloaded through a browser, so Gatekeeper
> refuses to run them ("cannot be opened because the developer cannot be
> verified"). Remove the attribute with
> `xattr -d com.apple.quarantine ./clj-lsp`. Installs via mise are not
> affected.

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
