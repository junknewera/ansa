local M = {}
local uv = vim.uv or vim.loop
local cap = 512 * 1024
local state = { client = string.format('nvim_%d_%.0f', uv.os_getpid(), uv.hrtime()), pending = {}, sequence = 0 }

local function notify(message)
  vim.notify(message, vim.log.levels.INFO, { title = 'ANSA' })
end

local function disconnect()
  if state.pipe then
    state.pipe:read_stop()
    if not state.pipe:is_closing() then state.pipe:close() end
  end
  state.pipe = nil
  state.attachment = nil
  state.buffered = ''
  state.pending = {}
  state.latest = nil
  state.snapshot = nil
  state.scheduled = false
  state.pending_history = nil
  state.share_pending = nil
  state.shared = nil
  state.history_retries = 0
  state.history_blocked = false
  state.history_retry_scheduled = false
  state.history_retry_token = (state.history_retry_token or 0) + 1
  state.epoch = (state.epoch or 0) + 1
end

local function send(fields)
  local attachment = state.attachment
  if not state.pipe or not attachment then return false, 'Нет подключения ANSA' end
  fields.version = 1
  fields.session = attachment.session
  fields.incarnation = attachment.incarnation
  state.sequence = state.sequence + 1
  fields.id = fields.id or ('r_' .. state.sequence)
  local line = vim.json.encode(fields) .. '\n'
  if #line > 128 * 1024 then return false, 'Запрос слишком большой' end
  if (state.pipe:get_write_queue_size() or 0) + #line > 256 * 1024 then
    return false, 'Подключение ANSA занято'
  end
  local pipe = state.pipe
  pipe:write(line, function(err)
    if err then vim.schedule(function()
      if state.pipe == pipe then disconnect(); notify('Соединение ANSA потеряно; проверьте состояние задачи') end
    end) end
  end)
  return true, fields.id
end

local function history()
  if state.pending_history or state.history_blocked or state.history_retry_scheduled then return end
  local ok, id = send({ kind = 'history', before = 0, limit = 64, forward = false })
  if ok then state.pending_history = id
  else state.history_blocked = true; notify(id) end
end

local function invalidate(context)
  local ok = send({kind='invalidate_context', editor_incarnation=context.editor_incarnation,
    buffer=context.buffer, sequence=context.sequence})
  if not ok then disconnect(); notify('Подключение ANSA закрыто: не удалось отозвать выделение') end
end

local function buffer(name)
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_name(buf, name)
  -- acwrite tracks changed text; nofile silently clears the modified flag.
  vim.bo[buf].buftype = 'acwrite'
  vim.bo[buf].bufhidden = 'hide'
  vim.bo[buf].swapfile = false
  vim.bo[buf].filetype = 'markdown'
  vim.bo[buf].modifiable = false
  return buf
end

local function replace(buf, lines)
  if not vim.api.nvim_buf_is_valid(buf) then return end
  if vim.bo[buf].modified then
    -- Keep edits in their original buffer and publish the new view separately.
    buf = buffer('ansa://history/' .. tostring(uv.hrtime()))
    state.history_buf = buf
    notify('Обновление доступно в новом буфере: предыдущий буфер изменён')
  end
  local views = {}
  for _, win in ipairs(vim.fn.win_findbuf(buf)) do
    views[win] = vim.api.nvim_win_call(win, vim.fn.winsaveview)
  end
  vim.bo[buf].modifiable = true
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modified = false
  vim.bo[buf].modifiable = false
  for win, view in pairs(views) do
    if vim.api.nvim_win_is_valid(win) then
      vim.api.nvim_win_call(win, function() vim.fn.winrestview(view) end)
    end
  end
end

local function render(snapshot)
  local attachment = state.attachment
  if not attachment or snapshot.session ~= attachment.session
    or snapshot.incarnation ~= attachment.incarnation then return end
  if type(snapshot.through) ~= 'number' or snapshot.through < (state.through or 0) then return end
  state.through = snapshot.through
  state.snapshot = snapshot
  if not state.history and state.history_buf and vim.api.nvim_buf_is_valid(state.history_buf) then
    history()
    return
  end
  if not state.history_buf or not vim.api.nvim_buf_is_valid(state.history_buf) then
    state.history_buf = buffer('ansa://history/' .. state.document_key .. '/' .. tostring(uv.hrtime()))
  end
  local lines = { '# ANSA', '' }
  local page = state.history or {}
  for _, entry in ipairs(page.entries or {}) do
    if entry.role == 'user' or entry.role == 'assistant' then
      table.insert(lines, entry.role == 'user' and '## Вы' or '## Агент')
      table.insert(lines, '')
      vim.list_extend(lines, vim.split(entry.text or '', '\n', { plain = true }))
      table.insert(lines, '')
    end
  end
  local current = snapshot.state or {}
  if type(current.transcript_total) == 'number'
    and (not page.total or current.transcript_total > page.total) then history() end
  local preview = current.preview
  if preview and preview ~= vim.NIL and preview.state == 'preview' and preview.text ~= '' then
    table.insert(lines, '## Предварительный ответ')
    table.insert(lines, '')
    vim.list_extend(lines, vim.split(preview.text, '\n', { plain = true }))
  end
  replace(state.history_buf, lines)
  vim.b[state.history_buf].ansa_phase = current.phase
end

local function receive(message)
  if message.type == 'welcome' then
    if message.version ~= 1 or message.role ~= state.requested_role then
      disconnect()
      notify('Несовместимый workspace protocol')
      return
    end
    state.attachment = message
    state.through = 0
    local key = message.session .. '/' .. message.incarnation
    if state.document_key ~= key then
      state.document_key = key
      state.history_buf = nil
      state.history = nil
    end
    send({ kind = 'snapshot' })
    history()
  elseif message.type == 'snapshot' then
    render(message)
  elseif message.type == 'history' then
    local attachment = state.attachment
    if attachment and message.session == attachment.session
      and message.incarnation == attachment.incarnation and message.id == state.pending_history then
      state.pending_history = nil
      state.history_retries = 0
      state.history_retry_scheduled = false
      state.history_retry_token = state.history_retry_token + 1
      state.history = message
      if state.snapshot then render(state.snapshot) end
    end
  elseif message.type == 'posted' and message.kind == 'history'
    and message.id == state.pending_history and message.status ~= 'Accepted' then
    state.pending_history = nil
    if message.status == 'QueueFull' and state.history_retries < 3 then
      state.history_retries = state.history_retries + 1
      state.history_retry_scheduled = true
      local epoch = state.epoch
      local token = state.history_retry_token
      vim.defer_fn(function()
        if state.epoch == epoch and state.history_retry_token == token and state.attachment then
          state.history_retry_scheduled = false
          history()
        end
      end,
        100 * state.history_retries)
    else
      state.history_blocked = true
      notify('История ANSA недоступна: ' .. tostring(message.status) .. '. :AnsaRefresh повторяет чтение')
    end
  elseif message.type == 'context_published' and state.share_pending and message.id == state.share_pending.id then
    local pending = state.share_pending
    state.share_pending = nil
    local context = pending.context
    if pending.invalidated or not vim.api.nvim_buf_is_valid(context.buffer)
      or vim.api.nvim_buf_get_changedtick(context.buffer) ~= context.changedtick
      or vim.api.nvim_buf_get_name(context.buffer) ~= pending.name then
      invalidate(context)
      notify('Выделение изменилось; повторите :AnsaShare')
    else
      state.shared = context
      notify('Выделение передано в workspace')
    end
  elseif message.type == 'gap' then
    -- Every update is a complete replacement snapshot with its own barrier.
    state.gap = message.through
  elseif message.type == 'error' then
    if state.share_pending and message.id == state.share_pending.id then
      state.share_pending = nil
      notify('ANSA: ' .. tostring(message.code))
      return
    end
    state.pending_history = nil
    state.history_blocked = true
    state.history_retry_scheduled = false
    state.history_retry_token = state.history_retry_token + 1
    notify('ANSA: ' .. tostring(message.code))
  end
end

function M.attach(path, options)
  disconnect()
  if not path or path == '' then notify('Укажите путь к workspace.sock'); return end
  state.buffered = ''
  state.requested_role = options and options.editor and 'editor' or 'viewer'
  local role = state.requested_role
  local pipe = uv.new_pipe(false)
  local epoch = state.epoch
  state.pipe = pipe
  pipe:connect(path, function(err)
    if err then vim.schedule(function()
      if state.pipe == pipe then disconnect(); notify('Не удалось подключиться к ANSA') end
    end); return end
    if state.pipe ~= pipe then return end
    pipe:write(vim.json.encode({ version = 1, role = role, client = state.client }) .. '\n')
    pipe:read_start(function(read_error, chunk)
      if read_error or not chunk then
        vim.schedule(function() if state.pipe == pipe then disconnect(); notify('ANSA отключена') end end)
        return
      end
      if state.pipe ~= pipe then return end
      state.buffered = state.buffered .. chunk
      while true do
        local last = state.buffered:find('\n', 1, true)
        if not last then break end
        if last > cap then
          pipe:read_stop()
          vim.schedule(function() if state.pipe == pipe then disconnect(); notify('Превышен размер workspace frame') end end)
          return
        end
        local line = state.buffered:sub(1, last - 1)
        state.buffered = state.buffered:sub(last + 1)
        local ok, message = pcall(vim.json.decode, line)
        if not ok or type(message) ~= 'table' then
          pipe:read_stop()
          vim.schedule(function() if state.pipe == pipe then disconnect(); notify('Некорректный workspace frame') end end)
          return
        end
        if message.type == 'snapshot' then state.latest = message
        elseif #state.pending < 4 then table.insert(state.pending, message)
        else
          pipe:read_stop()
          vim.schedule(function() if state.pipe == pipe then disconnect(); notify('Переполнена очередь workspace') end end)
          return
        end
      end
      if #state.buffered > cap then
        pipe:read_stop()
        vim.schedule(function() if state.pipe == pipe then disconnect(); notify('Превышен размер workspace frame') end end)
        return
      end
      if not state.scheduled then
        state.scheduled = true
        vim.schedule(function()
          if state.pipe ~= pipe or state.epoch ~= epoch then return end
          state.scheduled = false
          local messages = state.pending
          state.pending = {}
          for _, message in ipairs(messages) do receive(message) end
          if state.latest then local latest = state.latest; state.latest = nil; receive(latest) end
        end)
      end
    end)
  end)
end

function M.open()
  if not state.history_buf or not vim.api.nvim_buf_is_valid(state.history_buf) then
    notify('История ANSA ещё не загружена'); return
  end
  vim.api.nvim_set_current_buf(state.history_buf)
end

function M.status()
  return { connected = state.attachment ~= nil, snapshot = state.snapshot, buffer = state.history_buf, shared = state.shared }
end

function M.share(first, last)
  if state.share_pending then notify('Дождитесь подтверждения предыдущего выделения'); return end
  if not state.attachment or state.attachment.role ~= 'editor' then
    notify('Для передачи выделения нужны --workspace-editor и :AnsaAttach!'); return
  end
  local buf = vim.api.nvim_get_current_buf()
  local lines = vim.api.nvim_buf_get_lines(buf, first - 1, last, false)
  local text = table.concat(lines, '\n')
  if #text > 16 * 1024 then notify('Выделение превышает 16 KiB'); return end
  local path = vim.api.nvim_buf_get_name(buf)
  local name = path
  if path:match('^ansa://') then path = '' end
  if path ~= '' then path = uv.fs_realpath(path) or path end
  state.context_sequence = (state.context_sequence or 0) + 1
  local context = { editor_incarnation = state.client, buffer = buf,
    changedtick = vim.api.nvim_buf_get_changedtick(buf), sequence = state.context_sequence,
    path = path, filetype = vim.bo[buf].filetype, first_line = first, text = text }
  local ok, id = send({ kind = 'publish_context', context = context })
  if ok then state.share_pending = { id = id, context = context, name = name } else notify(id) end
end

function M.setup()
  vim.api.nvim_create_user_command('AnsaAttach', function(args) M.attach(args.args, {editor=args.bang}) end,
    { nargs = 1, bang = true, complete = 'file', desc = 'Подключиться к Carbon workspace; ! передаёт контекст редактора' })
  vim.api.nvim_create_user_command('AnsaShare', function(args) M.share(args.line1, args.line2) end,
    { range = true, desc = 'Передать выбранные строки агенту без сохранения файла' })
  vim.api.nvim_create_user_command('AnsaHistory', M.open, { desc = 'Открыть историю ANSA' })
  vim.api.nvim_create_user_command('AnsaRefresh', function()
    state.history_retries = 0
    state.history_blocked = false
    state.history_retry_scheduled = false
    state.history_retry_token = state.history_retry_token + 1
    send({ kind = 'snapshot' })
    history()
  end, { desc = 'Обновить историю ANSA' })
  vim.api.nvim_create_user_command('AnsaDetach', disconnect, { desc = 'Отключить viewer ANSA' })
  vim.api.nvim_create_autocmd('VimLeavePre', { callback = disconnect })
  vim.api.nvim_create_autocmd({'TextChanged', 'TextChangedI', 'BufWipeout', 'BufFilePost'}, {
    callback = function(args)
      if not state.attachment then return end
      local tick = vim.api.nvim_buf_is_valid(args.buf) and vim.api.nvim_buf_get_changedtick(args.buf) or 0
      local changed_name = args.event == 'BufWipeout' or args.event == 'BufFilePost'
      local shared = state.shared
      if shared and shared.buffer == args.buf and (changed_name or tick ~= shared.changedtick) then
        invalidate(shared)
        state.shared = nil
      end
      local pending = state.share_pending
      if pending and pending.context.buffer == args.buf
        and (changed_name or tick ~= pending.context.changedtick) then
        invalidate(pending.context)
        pending.invalidated = true
      end
    end,
  })
end

return M
