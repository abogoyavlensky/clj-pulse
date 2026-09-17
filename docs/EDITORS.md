# Installation and editor setup

[Back to README](../README.md)

## Homebrew (macOS, Linux)

```sh
brew install abogoyavlensky/tap/clj-pulse
```

## mise (macOS, Linux)

```sh
mise use -g github:abogoyavlensky/clj-pulse
```

## Manual download

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
-- Go-to-definition into a library lands on a `jar:file:///...!/clojure/core.clj`
-- URI. Neovim's built-in LSP client creates an empty buffer for it and fires
-- `BufReadCmd`; clj-pulse serves the entry's text through
-- `clojure/dependencyContents`. Call `setup()` once, after `vim.lsp.enable`.
local M = {}

--- @param opts? { client_name?: string }  `client_name` must match the name
--- the LSP client is registered under (default `"clj_pulse"`).
function M.setup(opts)
  local client_name = (opts or {}).client_name or "clj_pulse"
  vim.api.nvim_create_autocmd("BufReadCmd", {
    -- `jar:file://...` is not a URL Neovim recognizes (the scheme is not
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

The same file ships as [`editors/nvim/jar.lua`](../editors/nvim/jar.lua); vendor it
and load it in one line instead:

```lua
dofile("/path/to/clj-pulse/editors/nvim/jar.lua").setup()
```

### Zed

Install the [Clojure](https://zed.dev/extensions/clojure#details) extension, then add to `~/.config/zed/settings.json`:

```json
{
  "lsp": {
    "clojure-lsp": {
      "binary": {
        "path": "/path/to/clj-pulse"
      }
    }
  }
}
```

> [!NOTE]
> Zed support is best effort. Project navigation works, but library JAR
> navigation is not supported yet.

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
symbol under it still answers `{"symbol": null, "entry": null}`). Notes are never served - ClojureDocs states
a license for examples (CC0) but none for notes.
