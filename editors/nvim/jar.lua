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
