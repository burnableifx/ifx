-- Neovim 0.11+: load this file after putting ifx-lang on PATH.
vim.filetype.add({ extension = { ifx = 'ifx' } })
vim.lsp.config('ifx', {
  cmd = { 'ifx-lang', 'lsp' },
  filetypes = { 'ifx' },
  root_markers = { 'Ifx.toml', '.git' },
})
vim.lsp.enable('ifx')
