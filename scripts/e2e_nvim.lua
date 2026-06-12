-- Real-client e2e: drives clj-lsp through Neovim's built-in LSP client,
-- verifying capability negotiation and feature behavior against a real
-- editor client (not just a raw JSON-RPC harness).
--
-- Usage: nvim --headless -l scripts/e2e_nvim.lua [project-root] [server-binary]

local root = arg[1] or "tests/fixtures/simple_project"
local server = arg[2] or "target/debug/clj-lsp"
root = vim.fn.fnamemodify(root, ":p"):gsub("/$", "")
server = vim.fn.fnamemodify(server, ":p")

local failures = 0
local function check(cond, msg)
  if cond then
    print("ok    " .. msg)
  else
    failures = failures + 1
    print("FAIL  " .. msg)
  end
end

vim.cmd.edit(root .. "/src/utils.clj")
local buf = vim.api.nvim_get_current_buf()

local indexed = false
local client_id = vim.lsp.start({
  name = "clj-lsp",
  cmd = { server },
  root_dir = root,
  handlers = {
    ["window/logMessage"] = function(_, params)
      if params and params.message and params.message:find("Indexed") then
        indexed = true
      end
    end,
  },
})
check(client_id ~= nil, "server started and attached to buffer")
if not client_id then
  os.exit(1)
end

vim.wait(20000, function()
  return indexed
end, 50)
check(indexed, "project indexed (window/logMessage received)")

-- Locate `core/add` in the buffer
local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
local dl, dc
for i, l in ipairs(lines) do
  local s = l:find("core/add", 1, true)
  if s then
    dl, dc = i - 1, s + 4 -- 0-based line, cursor inside the token
  end
end
check(dl ~= nil, "found core/add in utils.clj")

local params = {
  textDocument = { uri = vim.uri_from_bufnr(buf) },
  position = { line = dl, character = dc },
}

local resp = vim.lsp.buf_request_sync(buf, "textDocument/definition", params, 10000) or {}
local def = resp[client_id] and resp[client_id].result
check(
  def ~= nil and def.uri ~= nil and def.uri:match("src/core%.clj$") ~= nil,
  "definition: core/add resolves to src/core.clj"
)

resp = vim.lsp.buf_request_sync(buf, "textDocument/hover", params, 10000) or {}
local hov = resp[client_id] and resp[client_id].result
check(
  hov ~= nil
    and hov.contents ~= nil
    and hov.contents.value:find("Adds two numbers", 1, true) ~= nil,
  "hover: docstring shown for core/add"
)

resp = vim.lsp.buf_request_sync(buf, "textDocument/completion", params, 10000) or {}
local comp = resp[client_id] and resp[client_id].result or {}
local found = false
for _, item in ipairs(comp.items or comp) do
  if item.label == "core/add" then
    found = true
  end
end
check(found, "completion: core/add offered")

if failures > 0 then
  print(failures .. " check(s) FAILED")
  os.exit(1)
end
print("ALL CHECKS PASSED (real Neovim LSP client)")
os.exit(0)
