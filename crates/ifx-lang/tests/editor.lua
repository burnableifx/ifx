-- Run from repository root with IFX_LANG_BIN pointing to the built binary:
-- nvim --headless -u NONE -l crates/ifx-lang/tests/editor.lua
local binary = assert(vim.env.IFX_LANG_BIN, 'IFX_LANG_BIN is required')
vim.cmd('edit examples/language/configure.ifx')
local buffer = vim.api.nvim_get_current_buf()
local client_id = assert(vim.lsp.start({ name = 'ifx-test', cmd = { binary, 'lsp' }, root_dir = vim.fn.getcwd() }))
assert(vim.wait(5000, function()
  local client = vim.lsp.get_client_by_id(client_id)
  return client and client.initialized
end), 'LSP did not initialize')
local client = assert(vim.lsp.get_client_by_id(client_id))
local uri = vim.uri_from_bufnr(buffer)
vim.api.nvim_buf_set_lines(buffer, 0, -1, false, { 'resource node = linode.' })
assert(vim.wait(5000, function() return #vim.diagnostic.get(buffer) > 0 end), 'incomplete source did not produce diagnostics')
local response = assert(client:request_sync('textDocument/completion', {
  textDocument = { uri = uri }, position = { line = 0, character = 23 },
}, 5000, buffer))
local found = false
for _, item in ipairs(response.result.items) do if item.label == 'instance' then found = true end end
assert(found, 'namespace completion missing')
vim.api.nvim_buf_set_lines(buffer, 0, -1, false, { 'let key = "node";', 'resource node = memory.value(key).value(1);' })
assert(vim.wait(5000, function() return #vim.diagnostic.get(buffer) == 0 end), 'valid edit did not clear diagnostics')
local definition = assert(client:request_sync('textDocument/definition', {
  textDocument = { uri = uri }, position = { line = 1, character = 29 },
}, 5000, buffer))
assert(definition.result.range.start.line == 0, 'definition did not resolve local binding')
vim.api.nvim_buf_set_lines(buffer, 0, -1, false, { 'use shared::' })
assert(vim.wait(5000, function() return #vim.diagnostic.get(buffer) > 0 end), 'partial use did not produce diagnostics')
local imports = assert(client:request_sync('textDocument/completion', {
  textDocument = { uri = uri }, position = { line = 0, character = 12 },
}, 5000, buffer))
local web = false
for _, item in ipairs(imports.result.items) do if item.label == 'shared::web' then web = true end end
assert(web, 'manifest-scoped use completion missing')
vim.api.nvim_buf_set_lines(buffer, 0, -1, false, {
  'use shared::web;', 'module app = web("editor");', 'output url: String = app.url;',
})
assert(vim.wait(5000, function() return #vim.diagnostic.get(buffer) == 0 end), 'saved dependency modules did not resolve without open buffers')
client:stop()
assert(vim.wait(5000, function() return client:is_stopped() end), 'LSP did not shut down')
print('IFX Neovim: initialization, incomplete completion, unsaved diagnostics, definition, package imports, shutdown passed')
vim.cmd('qa!')
