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
bb build      # release build
```

> [!NOTE]
> To run `bb outdated` you need to have `cargo-outdated` installed. You can install it with `cargo install cargo-outdated`.

## Editor Setup

### VS Code

Setup & test workflow

- cargo build                          # build the LSP binary
- cd editors/vscode && npm install     # install vscode-languageclient
- Then: Open editors/vscode/ in VS Code → F5 → opens Extension Development Host → open a Clojure project → jump-to-definition works.

### Zed

Add to `~/.config/zed/settings.json`:

```json
{
  "lsp": {
    "clj-lsp": {
      "binary": { "path": "/path/to/clj-lsp" }
    }
  },
  "languages": {
    "Clojure": { "language_servers": ["clj-lsp"] }
  }
}
```

### Neovim (nvim-lspconfig)

```lua
vim.lsp.start({
  name = "clj-lsp",
  cmd = { "/path/to/clj-lsp" },
  root_dir = vim.fs.dirname(vim.fs.find({ "deps.edn", "project.clj" }, { upward = true })[1]),
})
```

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for data flow and design decisions.
