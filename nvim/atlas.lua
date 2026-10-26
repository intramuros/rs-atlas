-- rustgraph3d live mode for Neovim 0.10+. Copy or symlink into ~/.config/nvim/plugin/.
--
--   :Atlas       map the project you are in, serve the viewer, open it in the browser
--   :AtlasStop   stop the server (it also stops when Neovim exits)
--   gd           in Rust buffers: go to definition and add the hop, labeled with the name
--                it landed on, to the viewer's trail
--
-- Every Rust buffer you enter is reported as well, so <C-o>/<C-i> move along the trail.
--
-- Options, set in init.lua:
--   vim.g.atlas_port = 7878
--   vim.g.atlas_args = { "--depth", "3", "--prune" }  -- extra rustgraph3d flags
--   vim.g.atlas_gd = "gd"  -- false: keep your gd and map <Plug>(atlas-definition) yourself

local atlas = { url = nil, job = nil, ready = false, jumping = false }

local function report(params)
  if not atlas.ready then return end
  local cmd = { "curl", "-s", "-m", "1", "-G", atlas.url .. "/visit" }
  for k, v in pairs(params) do
    vim.list_extend(cmd, { "--data-urlencode", k .. "=" .. v })
  end
  vim.system(cmd)
end

vim.api.nvim_create_user_command("Atlas", function()
  if atlas.job then
    if atlas.ready then vim.ui.open(atlas.url) else vim.notify("rustgraph3d: still building the graph") end
    return
  end
  local port = vim.g.atlas_port or 7878
  local url = ("http://127.0.0.1:%d"):format(port)
  local root = vim.fs.root(0, { "Cargo.lock", "Cargo.toml" }) or vim.fn.getcwd()
  local cmd = { "rustgraph3d", root, "--serve", tostring(port), "-o", vim.fn.tempname() .. ".html" }
  if vim.v.servername ~= "" then vim.list_extend(cmd, { "--nvim", vim.v.servername }) end
  vim.list_extend(cmd, vim.g.atlas_args or {})
  local log = ""
  local ok, job = pcall(vim.system, cmd, {
    -- The server prints its address once the graph is built: open the browser then.
    stderr = function(_, data)
      log = (log .. (data or "")):sub(-4000)
      if not atlas.ready and log:find("viewer:", 1, true) then
        atlas.ready = true
        vim.schedule(function() vim.ui.open(url) end)
      end
    end,
  }, function(res)
    atlas.job, atlas.ready = nil, false
    if res.code ~= 0 and res.signal == 0 then
      vim.schedule(function() vim.notify("rustgraph3d exited:\n" .. log, vim.log.levels.ERROR) end)
    end
  end)
  if not ok then
    return vim.notify("Could not start rustgraph3d (is it on PATH?): " .. tostring(job), vim.log.levels.ERROR)
  end
  atlas.url, atlas.job = url, job
  vim.notify("rustgraph3d: mapping " .. root .. " …")
end, {})

local function stop()
  if atlas.job then atlas.job:kill(15) end
end
vim.api.nvim_create_user_command("AtlasStop", stop, {})
vim.api.nvim_create_autocmd("VimLeavePre", { callback = stop })

-- Report every Rust buffer you land on (jumplist, :e, pickers ...). Wait for the cursor:
-- a jump enters the buffer before it moves.
vim.api.nvim_create_autocmd("BufEnter", {
  pattern = "*.rs",
  callback = function(ev)
    if not atlas.ready or atlas.jumping then return end
    vim.schedule(function()
      local file = vim.api.nvim_buf_get_name(ev.buf)
      if file ~= "" and vim.api.nvim_get_current_buf() == ev.buf then
        report({ file = file, line = vim.fn.line(".") })
      end
    end)
  end,
})

-- Go to definition the way the built-in jump does (jumplist, tag stack, folds), then report
-- the hop: where it started, and the name under the cursor where it landed.
local function definition()
  local win, buf = vim.api.nvim_get_current_win(), vim.api.nvim_get_current_buf()
  local from = { file = vim.api.nvim_buf_get_name(buf), line = vim.fn.line("."), word = vim.fn.expand("<cword>") }
  local pos = vim.fn.getpos(".")
  pos[1] = buf
  vim.lsp.buf.definition({
    on_list = function(t)
      local it = t.items[1]
      if not it then return end
      if #t.items > 1 then vim.fn.setqflist({}, " ", t) end
      vim.cmd("normal! m'")
      vim.fn.settagstack(win, { items = { { tagname = from.word, from = pos } } }, "t")
      local dest = it.bufnr or vim.fn.bufadd(it.filename)
      vim.bo[dest].buflisted = true
      atlas.jumping = true -- this jump reports itself; skip the BufEnter visit
      local ok, err = pcall(vim.api.nvim_win_set_buf, win, dest)
      atlas.jumping = false
      if not ok then return vim.notify(err, vim.log.levels.ERROR) end
      vim.api.nvim_win_set_cursor(win, { it.lnum, math.max(it.col - 1, 0) })
      vim.cmd("normal! zv")
      local name = vim.fn.expand("<cword>")
      report({
        file = vim.api.nvim_buf_get_name(dest),
        line = it.lnum,
        from = from.file,
        fline = from.line,
        sym = name ~= "" and name or from.word,
      })
    end,
  })
end

vim.keymap.set("n", "<Plug>(atlas-definition)", definition, { desc = "Go to definition, add the hop to the rustgraph3d trail" })

vim.api.nvim_create_autocmd("LspAttach", {
  callback = function(ev)
    local key = vim.g.atlas_gd == nil and "gd" or vim.g.atlas_gd
    if key and vim.bo[ev.buf].filetype == "rust" then
      vim.keymap.set("n", key, definition, { buffer = ev.buf, desc = "Go to definition (rustgraph3d trail)" })
    end
  end,
})
