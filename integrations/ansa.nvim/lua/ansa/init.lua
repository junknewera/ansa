local M = {}
local uv = vim.uv or vim.loop
local cap = 512 * 1024
local state = { client = string.format('nvim_%d_%.0f', uv.os_getpid(), uv.hrtime()), pending = {}, sequence = 0, watched = {} }
local messages
local artifacts

local function notify(message)
  vim.notify(message, vim.log.levels.INFO, { title = 'ANSA' })
end

local function disconnect()
  if messages then messages:disconnected() end
  if artifacts then artifacts:disconnected() end
  if state.pipe then
    state.pipe:read_stop()
    if not state.pipe:is_closing() then state.pipe:close() end
  end
  state.pipe = nil
  state.attachment = nil
  state.buffered = ''
  state.pending = {}
  state.pending_bytes = 0
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

local function revoke_buffer(buf)
  local shared = state.shared
  if shared and shared.buffer == buf then
    state.shared = nil
    invalidate(shared)
  end
  local pending = state.share_pending
  if pending and pending.context.buffer == buf and not pending.invalidated then
    pending.invalidated = true
    invalidate(pending.context)
  end
end

local function watch_buffer(buf)
  if state.watched[buf] then return true end
  local function revoke(_, changed)
    state.watched[changed] = nil
    revoke_buffer(changed)
    return true
  end
  local attached = vim.api.nvim_buf_attach(buf, false, {
    on_lines = revoke,
    on_changedtick = revoke,
    on_detach = revoke,
    on_reload = function(_, reloaded) revoke_buffer(reloaded) end,
  })
  if attached then state.watched[buf] = true end
  return attached
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
  vim.b[buf].ansa_document_title = 'История ANSA'
  return buf
end

local function replace(buf, lines, positions)
  if not vim.api.nvim_buf_is_valid(buf) then return end
  if vim.bo[buf].modified then
    -- Keep edits in their original buffer and publish the new view separately.
    buf = buffer('ansa://history/' .. tostring(uv.hrtime()))
    state.history_buf = buf
    notify('Обновление доступно в новом буфере: предыдущий буфер изменён')
  end
  local before = state.history_positions
  state.history_positions = positions
  local starts = {}
  for _, part in ipairs(positions) do starts[part.index] = part end
  local function relocate(row)
    for _, old in ipairs(before or {}) do
      if row >= old.start and row <= old.finish then
        local next = starts[old.index]
        if next then return math.min(next.start + row - old.start, next.finish) end
        -- The bounded page may have dropped the entry being read.
        for _, part in ipairs(positions) do
          if part.index > old.index then return part.start end
        end
        return #lines
      end
    end
    return math.min(row, #lines)
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
      local top = relocate(view.topline)
      if top ~= view.topline then view.topfill = 0 end
      view.topline = top
      view.lnum = relocate(view.lnum)
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
  artifacts:observe(snapshot)
  if not state.history and state.history_buf and vim.api.nvim_buf_is_valid(state.history_buf) then
    history()
    return
  end
  if not state.history_buf or not vim.api.nvim_buf_is_valid(state.history_buf) then
    state.history_buf = buffer('ansa://history/' .. state.document_key .. '/' .. tostring(uv.hrtime()))
  end
  local lines = { '# ANSA', '' }
  local positions = {}
  local page = state.history or {}
  for _, entry in ipairs(page.entries or {}) do
    if entry.role == 'user' or entry.role == 'assistant' then
      local start = #lines + 1
      table.insert(lines, entry.role == 'user' and '## Вы' or '## Агент')
      table.insert(lines, '')
      vim.list_extend(lines, vim.split(entry.text or '', '\n', { plain = true }))
      table.insert(lines, '')
      if type(entry.index) == 'number' then
        table.insert(positions, {index=entry.index, start=start, finish=#lines})
      end
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
  replace(state.history_buf, lines, positions)
  vim.b[state.history_buf].ansa_phase = current.phase
end

local function receive(message)
  if artifacts and artifacts:handle(message) then return end
  if messages and messages:handle(message) then return end
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
      state.history_positions = nil
    end
    send({ kind = 'snapshot' })
    history()
    messages:attached()
    artifacts:attached()
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
      local open=state.open_history
      state.open_history=nil
      if open and state.history_buf and vim.api.nvim_get_current_buf()==open.buffer
        and vim.api.nvim_buf_get_changedtick(open.buffer)==open.tick then M.open() end
      artifacts:refresh(false)
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
    -- Apply the already received projection before routing a send as Submit
    -- or Steer. ACKs and snapshots can share one socket read.
    if state.latest then local latest = state.latest; state.latest = nil; render(latest) end
    local pending = state.share_pending
    if not pending or message.id ~= pending.id then return end
    state.share_pending = nil
    local context = pending.context
    if pending.invalidated or not vim.api.nvim_buf_is_loaded(context.buffer)
      or vim.api.nvim_buf_get_changedtick(context.buffer) ~= context.changedtick
      or vim.api.nvim_buf_get_name(context.buffer) ~= pending.name then
      if not pending.invalidated then invalidate(context) end
      notify('Выделение изменилось; повторите :AnsaShare')
    else
      state.shared = context
      if pending.after then pending.after(context, message.mode) else notify('Выделение доступно агенту') end
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
  state.open_history=nil
  if state.auto_open_history and vim.fn.argc()==0 and vim.api.nvim_buf_get_name(0)==''
    and vim.deep_equal(vim.api.nvim_buf_get_lines(0,0,-1,false),{''}) then
    state.open_history={buffer=vim.api.nvim_get_current_buf(),tick=vim.api.nvim_buf_get_changedtick(0)}
  end
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
        elseif #state.pending < 8 and state.pending_bytes + #line <= 2 * 1024 * 1024 then
          table.insert(state.pending, message)
          state.pending_bytes = state.pending_bytes + #line
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
          state.pending_bytes = 0
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
  return { connected = state.attachment ~= nil, snapshot = state.snapshot, buffer = state.history_buf, shared = state.shared,
    pending_message = messages.pending, last_message = messages.last, message_draft = messages.draft,
    artifacts = artifacts.known, artifact_documents = artifacts.docs, artifact_read_pending = artifacts.read ~= nil,
    documents = artifacts.documents, prepared_documents = artifacts.prepared }
end

function M.share(first, last, after)
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
  if not watch_buffer(buf) then notify('Не удалось отследить изменения выделения'); return end
  local ok, id = send({ kind = 'publish_context', context = context })
  if ok then state.share_pending = { id = id, context = context, name = name, after = after } else notify(id) end
end

function M.send(first,last,prompt) messages:submit(first,last,prompt or '') end

function M.statusline()
  local win = vim.g.statusline_winid or vim.api.nvim_get_current_win()
  local buf = vim.api.nvim_win_get_buf(win)
  local artifact = vim.b[buf].ansa_artifact
  local name = vim.b[buf].ansa_document_title
  local hint = '%=%l:%c'
  local width = vim.api.nvim_win_get_width(win)
  if artifact then
    name = artifact.title .. (width >= 44 and ' · ' .. artifact.revision:sub(1, 8) or '')
    if artifact.mime == 'image/png' then
      local images = require('ansa.images')
      local action = images.zoom_action(buf, win)
      if action == 'zoom' or action == 'return' then
        hint = '%=Enter ' .. (action == 'return' and 'назад' or 'увеличить')
        if width >= 65 then hint = hint .. ' · o открыть' end
      elseif action == 'open' then
        hint = '%=Enter открыть PNG'
      end
    end
  end
  if not name then return '%<%f %h%m%r%=%-14.(%l,%c%V%) %P' end
  return '%<' .. vim.fn.strtrans(name):gsub('%%', '%%%%') .. '%{&modified ? " [+]" : ""} ' .. hint
end

function M.setup(options)
  options=options or {}
  state.auto_open_history=options.open_history
  if options.statusline then vim.o.statusline="%!v:lua.require'ansa'.statusline()" end
  if options.render and vim.env.NO_COLOR==nil then
    require('ansa.images').setup()
    local ok,markdown=pcall(require,'render-markdown')
    if ok then markdown.setup({latex={enabled=false}}) end
  end
  vim.api.nvim_create_user_command('AnsaArtifacts',function() artifacts:refresh(true) end,{desc='Выбрать сохранённый отчёт, CSV или PNG'})
  vim.api.nvim_create_user_command('AnsaDocuments',function(args)
    if args.args~='' then artifacts:open_document(args.args) else artifacts:document_picker(false) end
  end,{nargs='?',desc='Открыть подготовленный файл или proposed diff'})
  vim.api.nvim_create_user_command('AnsaDocumentsEarlier',function() artifacts:document_picker(true) end,{desc='Предыдущая страница подготовленных документов'})
  vim.api.nvim_create_user_command('AnsaArtifactsEarlier',function()
    if not artifacts.begin or artifacts.begin==0 then notify('Более ранних страниц артефактов нет'); return end
    artifacts:refresh(true,artifacts.begin)
  end,{desc='Загрузить предыдущую страницу артефактов'})
  vim.api.nvim_create_user_command('AnsaArtifact',function(args)
    if #args.fargs~=2 then notify(':AnsaArtifact ID REVISION'); return end
    artifacts:open(args.fargs[1],args.fargs[2])
  end,{nargs='+',desc='Открыть загруженный artifact ID и revision'})
  vim.api.nvim_create_user_command('AnsaAttach', function(args) M.attach(args.args, {editor=args.bang}) end,
    { nargs = 1, bang = true, complete = 'file', desc = 'Подключиться к Carbon workspace; ! передаёт контекст редактора' })
  vim.api.nvim_create_user_command('AnsaShare', function(args) M.share(args.line1, args.line2) end,
    { range = true, desc = 'Передать выбранные строки агенту без сохранения файла' })
  vim.api.nvim_create_user_command('AnsaSend',function(args) M.send(args.line1,args.line2,args.args) end,
    {range=true,nargs='*',desc='Отправить задачу с выделением или уточнение текущей задачи'})
  vim.api.nvim_create_user_command('AnsaPending',function() messages:inspect(true) end,{desc='Проверить исход отправки по ID'})
  vim.api.nvim_create_user_command('AnsaCancel',function() messages:cancel() end,{desc='Остановить свою подтверждённую задачу'})
  vim.api.nvim_create_user_command('AnsaForgetPending',function(args) messages:forget(args.bang) end,
    {bang=true,desc='Забыть локальную неподтверждённую отправку после проверки истории'})
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
  vim.api.nvim_create_autocmd({'BufUnload', 'BufDelete', 'BufWipeout', 'BufFilePost'}, {
    callback = function(args) revoke_buffer(args.buf) end,
  })
end

messages=require('ansa.messages').new({send=send,notify=notify,client=function() return state.client end,
  attachment=function() return state.attachment end,snapshot=function() return state.snapshot end,
  share=function(...) return M.share(...) end})
artifacts=require('ansa.artifacts').new({send=send,notify=notify,attachment=function() return state.attachment end})

return M
