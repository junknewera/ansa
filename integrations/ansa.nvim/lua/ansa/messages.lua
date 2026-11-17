local M = {}

function M.new(api)
  local self = { sequence = 0, token = 0, attempts = 0, draft = '' }

  local function current()
    local attachment = api.attachment()
    return attachment and attachment.role == 'editor' and attachment or nil
  end
  local function schedule()
    if self.scheduled or self.lookup or not self.pending or self.attempts >= 5 then return end
    self.scheduled = true
    local token = self.token
    vim.defer_fn(function()
      if token ~= self.token then return end
      self.scheduled = false
      self:inspect(false)
    end, 200)
  end

  function self:disconnected()
    self.token = self.token + 1
    self.lookup = nil
    self.scheduled = false
    self.cancel_request = nil
    if self.pending then self.pending.phase = 'unknown' end
  end

  function self:attached()
    self.attempts = 0
    local attachment = current()
    if self.pending and attachment and self.pending.session == attachment.session then self:inspect(false) end
  end

  function self:inspect(explicit)
    local attachment = current()
    local message = self.pending or self.last
    if not attachment then if explicit then api.notify('Проверка отправки доступна через :AnsaAttach!') end; return end
    if not message then if explicit then api.notify('Отправленных задач нет') end; return end
    if attachment.session ~= message.session then
      if explicit then api.notify('Подключитесь к исходному разговору для проверки отправки') end
      return
    end
    if self.lookup then return end
    if explicit then self.attempts = 0 end
    if not explicit and self.attempts >= 5 then return end
    local ok, id = api.send({ kind = 'message_lookup', message = message.id })
    if ok then
      self.attempts = self.attempts + 1
      self.lookup = { id = id, message = message }
    elseif explicit then api.notify(id) end
  end

  local function matching(message, receipt)
    local expected_turn = message.target == vim.NIL and ('n_' .. vim.fn.sha256(api.client()):sub(1,16) .. '_' .. message.id) or message.turn
    return receipt.text_sha256 == vim.fn.sha256(message.text)
      and receipt.turn == expected_turn
      and receipt.kind == (message.target == vim.NIL and 'submit' or 'steer')
      and (message.target == vim.NIL or receipt.request == message.target)
      and (message.target ~= vim.NIL or receipt.mode == message.expected_mode)
  end

  function self:handle(frame)
    if frame.type == 'message_status' then
      if self.cancel_request and frame.id == self.cancel_request then
        self.cancel_request = nil
        api.notify(frame.status == 'Accepted' and 'Остановка запрошена' or 'Задача уже изменила состояние')
        return true
      end
      if not self.pending or frame.id ~= self.pending.id then return true end
      if frame.status == 'Accepted' or frame.status == 'Duplicate' or frame.status == 'Unknown' or frame.status == 'CommandConflict' then
        self.pending.phase = frame.status == 'Unknown' and 'unknown' or 'posted'
        self.pending.conflict = frame.status == 'CommandConflict'
        self:inspect(false)
      else
        self.pending = nil
        api.notify('Отправка отклонена: ' .. tostring(frame.status))
      end
      return true
    elseif frame.type == 'message_receipt' then
      local query = self.lookup
      if not query or frame.id ~= query.id then return true end
      self.lookup = nil
      local attachment = current()
      if not attachment or frame.session ~= attachment.session or frame.incarnation ~= attachment.incarnation
        or frame.message ~= query.message.id then return true end
      local receipt = frame.receipt
      if receipt and receipt ~= vim.NIL then
        if not matching(query.message, receipt) or query.message.conflict then
          if self.pending then self.pending.phase = 'conflict' end
          api.notify('ID относится к другому сообщению. Проверьте историю отправки')
          return true
        end
        query.message.receipt = receipt
        if frame.ready and not frame.storage_failed and receipt.admission_durable then
          self.last = query.message
          if receipt.kind == 'submit' then self.own_submit = query.message end
          if self.pending == query.message then
            self.pending = nil
            self.token = self.token + 1
            self.scheduled = false
            api.notify(receipt.kind == 'steer' and 'Уточнение отправлено' or 'Задача отправлена')
          end
          return true
        end
      end
      -- Absence in a snapshot does not prove that a previously transmitted
      -- request cannot still be admitted. Only status reads are repeated.
      schedule()
      if self.attempts >= 5 and self.pending then
        self.pending.phase = 'unknown'
        api.notify('Отправка не подтверждена. :AnsaPending проверяет состояние')
      end
      return true
    elseif frame.type == 'error' then
      if self.lookup and frame.id == self.lookup.id then self.lookup = nil; schedule(); return true end
      if self.cancel_request and frame.id == self.cancel_request then
        self.cancel_request = nil; api.notify('Не удалось запросить остановку: ' .. tostring(frame.code)); return true
      end
      if self.pending and frame.id == self.pending.id then
        self.pending = nil
        api.notify('Отправка отклонена: ' .. tostring(frame.code))
        return true
      end
    end
    return false
  end

  function self:submit(first, last, prompt)
    if self.pending then api.notify('Сначала проверьте предыдущую отправку через :AnsaPending'); return end
    local attachment = current()
    if not attachment then api.notify('Отправка доступна через --workspace-editor и :AnsaAttach!'); return end
    if prompt ~= '' then self.draft = prompt end
    local text_prompt = prompt ~= '' and prompt or self.draft
    api.share(first, last, function(context, mode)
      local snapshot = api.snapshot()
      local active = snapshot and snapshot.state or nil
      local attached = current()
      if not active or not attached or attached.session ~= attachment.session
        or attached.incarnation ~= attachment.incarnation or active.phase == 'loading' then
        api.notify('Разговор ещё не готов'); return
      end
      if active.phase == 'paused' then api.notify('Продолжите задачу в Carbon перед отправкой уточнения'); return end
      local body = text_prompt .. '\n\n' .. (context.path ~= '' and context.path or 'Выделение редактора')
        .. ':' .. context.first_line .. '\n' .. context.text
      if #body > 16 * 1024 then api.notify('Сообщение с выделением превышает 16 KiB'); return end
      self.sequence = self.sequence + 1
      local id = 'm_' .. self.sequence
      local busy = active.busy or not (active.phase == 'idle' or active.phase == 'completed' or active.phase == 'cancelled')
      if busy and (not active.request or active.request == vim.NIL) then api.notify('Текущая задача ещё не получила ID'); return end
      local fields = { kind = 'submit_message', id = id, text = body,
        turn = busy and active.turn or id, target = busy and active.request or vim.NIL,
        expected_mode = mode or active.input_mode or active.mode }
      self.pending = vim.deepcopy(fields)
      self.pending.session = attached.session
      self.pending.origin_incarnation = attached.incarnation
      self.pending.phase = 'sending'
      self.attempts = 0
      local ok, error = api.send(fields)
      if not ok then self.pending = nil; api.notify(error) end
    end)
  end

  function self:cancel()
    local attachment = current()
    local message = self.own_submit
    local receipt = message and message.receipt
    if not attachment or not message or message.session ~= attachment.session
      or not receipt.request or receipt.request == vim.NIL then
      api.notify('Сначала подтвердите свою задачу через :AnsaPending'); return
    end
    local snapshot = api.snapshot()
    local active = snapshot and snapshot.state
    if not active or active.turn ~= receipt.turn or active.request ~= receipt.request then
      api.notify('В этом разговоре сейчас другая задача'); return
    end
    if self.cancel_request then return end
    local ok, id = api.send({kind='cancel_message',message=message.id,target=receipt.request,turn=receipt.turn})
    if ok then self.cancel_request = id else api.notify(id) end
  end

  function self:forget(bang)
    if not bang then api.notify('Проверьте историю перед :AnsaForgetPending!'); return end
    self.pending = nil
    self.lookup = nil
    self.token = self.token + 1
    self.scheduled = false
  end

  return self
end

return M
