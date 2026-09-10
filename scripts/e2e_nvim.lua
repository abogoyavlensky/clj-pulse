-- Real-client e2e: drives clj-pulse through Neovim's built-in LSP client,
-- verifying capability negotiation and feature behavior against a real
-- editor client (not just a raw JSON-RPC harness).
--
-- Usage: nvim --headless -l scripts/e2e_nvim.lua [project-root] [server-binary]

local root = arg[1] or "tests/fixtures/simple_project"
local server = arg[2] or "target/debug/clj-pulse"
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

-- The `jar:` handler users are told to install (README, "Library navigation"),
-- driven here exactly as they would drive it.
dofile(vim.fn.fnamemodify("editors/nvim/jar.lua", ":p")).setup({ client_name = "clj-pulse" })

local indexed = false
local libs_indexed = false
local client_id = vim.lsp.start({
  name = "clj-pulse",
  cmd = { server },
  root_dir = root,
  handlers = {
    ["window/logMessage"] = function(_, params)
      if params and params.message and params.message:find("Indexed") then
        indexed = true
      end
      if params and params.message and params.message:find("library indexing complete") then
        libs_indexed = true
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

resp = vim.lsp.buf_request_sync(buf, "textDocument/documentHighlight", params, 10000) or {}
local hl = resp[client_id] and resp[client_id].result
check(
  hl ~= nil and #hl > 0 and hl[1].range ~= nil,
  "documentHighlight: core/add highlighted in the buffer"
)

resp = vim.lsp.buf_request_sync(buf, "textDocument/selectionRange", {
  textDocument = params.textDocument,
  positions = { params.position },
}, 10000) or {}
local sel = resp[client_id] and resp[client_id].result
local innermost = sel and sel[1]
local outermost = innermost
while outermost and outermost.parent do
  outermost = outermost.parent
end
check(
  innermost ~= nil
    and outermost ~= innermost
    and (outermost.range["end"].line - outermost.range.start.line)
      > (innermost.range["end"].line - innermost.range.start.line),
  "selectionRange: the chain expands past the line the cursor is on"
)

-- `jar:` navigation: `str` in utils.clj is clojure.core's, which lives in the
-- clojure JAR. Opening that location goes through the snippet above.
vim.wait(60000, function()
  return libs_indexed
end, 100)
check(libs_indexed, "library indexing complete (window/logMessage received)")

local sl, sc
for i, l in ipairs(lines) do
  local s = l:find("(str ", 1, true)
  if s then
    sl, sc = i - 1, s
  end
end
check(sl ~= nil, "found the `str` call in utils.clj")

resp = vim.lsp.buf_request_sync(buf, "textDocument/definition", {
  textDocument = { uri = vim.uri_from_bufnr(buf) },
  position = { line = sl, character = sc },
}, 10000) or {}
local jar_def = resp[client_id] and resp[client_id].result
if jar_def and jar_def[1] then
  jar_def = jar_def[1]
end
check(
  jar_def ~= nil and jar_def.uri ~= nil and jar_def.uri:match("^jar:") ~= nil,
  "definition: `str` resolves to a jar: URI"
)

if jar_def and jar_def.uri then
  vim.lsp.util.show_document(jar_def, "utf-16", { focus = true })
  local jar_buf = vim.api.nvim_get_current_buf()
  local jar_lines = vim.api.nvim_buf_get_lines(jar_buf, 0, -1, false)
  local body = table.concat(jar_lines, "\n")
  check(body:find("(defn str", 1, true) ~= nil, "jar: buffer holds clojure.core's source")
  check(vim.bo[jar_buf].filetype == "clojure", "jar: buffer is a Clojure buffer")
  check(not vim.bo[jar_buf].modifiable, "jar: buffer is read-only")
end

if failures > 0 then
  print(failures .. " check(s) FAILED")
  os.exit(1)
end
print("ALL CHECKS PASSED (real Neovim LSP client)")
os.exit(0)
