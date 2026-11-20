local M = {}

function M.new(api)
  local self = { known = {}, preferred = {}, docs = {}, saved = {}, documents = {}, document_order = {}, prepared = {}, token = 0, retries = 0 }
  local function attachment()
    local value = api.attachment()
    if value and vim.tbl_contains(value.capabilities or {}, 'artifact_catalog') then return value end
  end
  local function key(d) return d.artifact_id .. '/' .. d.revision end
  local function document_key(d) return d.session_id .. '/' .. d.artifact_id end
  local function saved_key(d) return d.session_id .. '/' .. key(d) end
  local function origin()
    local buf=vim.api.nvim_get_current_buf()
    local view=vim.fn.winsaveview()
    return {window=vim.api.nvim_get_current_win(),buffer=buf,tick=vim.api.nvim_buf_get_changedtick(buf),
      cursor=vim.api.nvim_win_get_cursor(0),topline=view.topline,leftcol=view.leftcol}
  end
  local function unchanged(value)
    return value and vim.api.nvim_get_current_win()==value.window and vim.api.nvim_get_current_buf()==value.buffer
      and vim.api.nvim_buf_is_valid(value.buffer) and vim.api.nvim_buf_get_changedtick(value.buffer)==value.tick
      and vim.deep_equal(value.cursor,vim.api.nvim_win_get_cursor(0))
      and value.topline==vim.fn.winsaveview().topline and value.leftcol==vim.fn.winsaveview().leftcol
  end
  local function valid(d, session)
    return type(d) == 'table' and d.session_id == session and type(d.artifact_id) == 'string'
      and d.artifact_id:match('^[%w_-]+$') and type(d.revision) == 'string'
      and #d.revision == 64 and d.revision:match('^[0-9a-f]+$')
      and vim.tbl_contains({'text/markdown','text/csv','image/png'}, d.mime)
      and type(d.size_bytes) == 'number' and d.size_bytes > 0 and d.size_bytes <= 16 * 1024 * 1024
  end
  function self:disconnected()
    self.token = self.token + 1
    self.pending = nil
    self.read = nil
    self.retry_scheduled = false
    self.next_auto = nil
    self.pick_after = false
    self.deferred_open = nil
    self.deferred_document = nil
    self.pick_documents_after = false
    for _,doc in pairs(self.docs) do if doc.descriptor.mime=='image/png' then doc.needs_read=true end end
    local ok, images = pcall(require, 'ansa.images')
    if ok then images.clear() end
  end
  function self:attached()
    local value = attachment()
    if value then
      self.session = value.session
      self.known = {}
      self.preferred = {}
      self.documents = {}
      self.document_order = {}
      self.begin = nil
      self.catalog_total = 0
      self.wanted_total = 0
    end
    self.blocked = false
    self.retries = 0
  end
  function self:observe(snapshot)
    local value=attachment()
    local total=snapshot.state and snapshot.state.timeline_total
    if not value or snapshot.session~=value.session or snapshot.incarnation~=value.incarnation or type(total)~='number' then return end
    self.wanted_total=math.max(self.wanted_total or 0,total)
    if self.wanted_total>(self.catalog_total or 0) then self:refresh(false) end
  end
  function self:refresh(explicit, before)
    if explicit then self.pick_after = true; self.blocked = false; self.retries = 0 end
    if self.pending or self.retry_scheduled or self.blocked then return end
    if not attachment() then if explicit then api.notify('Артефакты недоступны в этом workspace') end; return end
    local ok, id = api.send({kind='artifact_catalog',before=before or 0,limit=64,forward=false})
    if ok then
      self.pending = id; self.before = before or 0
      local token=self.token
      vim.defer_fn(function()
        if self.token==token and self.pending==id then self.pending=nil; self.blocked=true; api.notify('Список артефактов не ответил: :AnsaArtifacts повторяет чтение') end
      end,5000)
    else self.blocked = true; api.notify(id) end
  end
  function self:choose()
    local items = {}
    for _, descriptor in pairs(self.known) do items[#items+1] = descriptor end
    table.sort(items, function(a,b)
      if a.artifact_id == b.artifact_id then
        if a.revision==b.revision then return false end
        local latest=self.preferred[a.artifact_id]
        if latest then
          if a.revision==latest.descriptor.revision then return true end
          if b.revision==latest.descriptor.revision then return false end
        end
        return a.revision < b.revision
      end
      return a.artifact_id < b.artifact_id
    end)
    if #items == 0 then api.notify('Сохранённых артефактов на этой странице нет'); return end
    vim.ui.select(items, {prompt='Артефакты ANSA', format_item=function(d)
      local latest=self.preferred[d.artifact_id]
      return string.format('%s · %.1f KiB · %s%s',d.title,d.size_bytes/1024,d.revision:sub(1,8),
        latest and latest.descriptor.revision==d.revision and ' · последняя' or '')
    end}, function(d) if d then self:fetch(d,true) end end)
  end
  function self:fetch(d, explicit, regranted, from, pinned)
    if not attachment() then api.notify('Сначала подключитесь к workspace'); return end
    if self.read then
      if explicit then
        api.notify('Дождитесь чтения предыдущего артефакта')
      else
        self.next_auto = {descriptor=d, origin=from or origin(), pinned=pinned}
      end
      return
    end
    local latest = self.preferred[d.artifact_id]
    if pinned == nil then pinned = explicit and (not latest or latest.descriptor.revision ~= d.revision) end
    local ok, id = api.send({kind='artifact_read',artifact_id=d.artifact_id,revision=d.revision})
    if ok then
      self.read = {id=id,descriptor=vim.deepcopy(d),explicit=explicit,regranted=regranted,origin=from or origin(),pinned=pinned}
      local token=self.token
      vim.defer_fn(function()
        if self.token==token and self.read and self.read.id==id then self.read=nil; self.next_auto=nil; api.notify('Артефакт не ответил: повторите открытие') end
      end,5000)
    else api.notify(id) end
  end
  function self:open(id, revision)
    local d = self.known[id .. '/' .. revision]
    if not d then
      if not attachment() then api.notify('Сначала подключитесь к workspace'); return end
      self.deferred_open={id=id,revision=revision,pages=0,origin=origin()}
      self:refresh(false)
      return
    end
    self:fetch(d,true)
  end
  function self:choose_document()
    local items={}
    for _,doc in pairs(self.documents) do items[#items+1]=doc end
    table.sort(items,function(a,b) return a.document_id<b.document_id end)
    if #items==0 then api.notify('Подготовленных документов на этой странице нет'); return end
    vim.ui.select(items,{prompt='Документы ANSA',format_item=function(doc)
      local name=type(doc.path)=='string' and vim.fn.fnamemodify(doc.path,':t') or 'Выделение'
      return (doc.kind=='diff' and 'Предложение · ' or 'Файл · ')..name..':'..doc.first_line
    end},function(doc) if doc then self:open_document(doc.document_id) end end)
  end
  function self:document_picker(earlier)
    self.pick_documents_after=true
    self:refresh(false,earlier and self.begin or 0)
  end
  function self:open_document(id,from,regranted)
    if not attachment() then api.notify('Сначала подключитесь к workspace'); return end
    if not self.documents[id] then
      self.deferred_document={id=id,pages=0,origin=from or origin(),regranted=regranted}; self:refresh(false); return
    end
    if self.read then api.notify('Дождитесь чтения предыдущего документа'); return end
    local ok,request=api.send({kind='document_read',document_id=id})
    if ok then
      self.read={kind='document',id=request,document_id=id,origin=from or origin(),regranted=regranted}
      local token=self.token
      vim.defer_fn(function()
        if self.token==token and self.read and self.read.id==request then self.read=nil;api.notify('Документ не ответил: повторите открытие') end
      end,5000)
    else api.notify(request) end
  end
  local function show_document(doc,focus)
    local name=doc.session_id..'/'..doc.document_id
    local prepared=self.prepared[name]
    local function make(text,suffix)
      local buf=vim.api.nvim_create_buf(false,true)
      vim.api.nvim_buf_set_name(buf,'ansa://document/'..name..'/'..suffix)
      vim.bo[buf].buftype='acwrite';vim.bo[buf].bufhidden='hide';vim.bo[buf].swapfile=false
      vim.api.nvim_buf_set_lines(buf,0,-1,false,vim.split(text,'\n',{plain=true}))
      vim.bo[buf].modified=false;vim.bo[buf].modifiable=false
      vim.bo[buf].filetype=doc.kind=='diff' and 'text' or doc.filetype
      vim.b[buf].ansa_document_title = type(doc.path)=='string' and vim.fn.fnamemodify(doc.path,':t') or 'Выделение'
      return buf
    end
    local function loaded(buf)
      if not buf or not vim.api.nvim_buf_is_valid(buf) then return false end
      if vim.api.nvim_buf_is_loaded(buf) then return true end
      vim.api.nvim_buf_delete(buf,{force=true})
      return false
    end
    prepared=prepared or {kind=doc.kind,document=doc}
    if not loaded(prepared.before) then prepared.before=make(doc.before,'before') end
    if doc.kind=='diff' and not loaded(prepared.after) then prepared.after=make(doc.after,'after') end
    self.prepared[name]=prepared
    if not focus then api.notify('Документ готов: :AnsaDocuments открывает его'); return end
    if doc.kind=='diff' then
      local path=type(doc.path)=='string' and vim.fn.fnamemodify(doc.path,':~:.') or 'Выделение'
      local _,newlines=doc.before:gsub('\n','')
      local context=path..':'..doc.first_line..(newlines>0 and '-'..(doc.first_line+newlines) or '')
      local function winbar(label)
        -- winbar interprets percent escapes; a filename remains literal text.
        return (label..' - '..context):gsub('%%','%%%%')
      end
      vim.cmd('tabnew');vim.api.nvim_set_current_buf(prepared.before);vim.cmd('diffthis');vim.wo.number=true;vim.wo.winbar=winbar('Исходный текст')
      vim.cmd('rightbelow vsplit');vim.api.nvim_set_current_buf(prepared.after);vim.cmd('diffthis');vim.wo.number=true;vim.wo.winbar=winbar('Предложение')
      vim.cmd('diffupdate')
    else
      vim.cmd('hide buffer '..prepared.before)
      vim.api.nvim_win_set_cursor(0,{math.min(doc.first_line,vim.api.nvim_buf_line_count(prepared.before)),0})
    end
  end
  local function text_buffer(d, content, focus, old)
    local buf = old and old.buffer
    if not buf or not vim.api.nvim_buf_is_valid(buf) or vim.bo[buf].modified then
      if buf and vim.api.nvim_buf_is_valid(buf) and vim.bo[buf].modified then
        api.notify('Новая версия доступна отдельно: предыдущий документ изменён')
      end
      buf = vim.api.nvim_create_buf(false,true)
      vim.bo[buf].buftype='acwrite'; vim.bo[buf].bufhidden='hide'; vim.bo[buf].swapfile=false
    end
    if not old or old.buffer ~= buf or old.descriptor.revision ~= d.revision then
      vim.api.nvim_buf_set_name(buf,'ansa://artifact/' .. saved_key(d) .. '/' .. buf)
    end
    local views = {}
    for _,win in ipairs(vim.fn.win_findbuf(buf)) do views[win]=vim.api.nvim_win_call(win,vim.fn.winsaveview) end
    vim.bo[buf].modifiable=true
    vim.api.nvim_buf_set_lines(buf,0,-1,false,vim.split(content,'\n',{plain=true}))
    vim.bo[buf].modified=false; vim.bo[buf].modifiable=false
    vim.bo[buf].filetype=d.mime=='text/markdown' and 'markdown' or 'csv'
    vim.b[buf].ansa_artifact=vim.deepcopy(d)
    for win,view in pairs(views) do
      if vim.api.nvim_win_is_valid(win) then vim.api.nvim_win_call(win,function() vim.fn.winrestview(view) end) end
    end
    if focus then vim.cmd('hide buffer ' .. buf) end
    return buf
  end
  function self:handle(frame)
    if frame.type=='posted' and frame.kind=='artifact_catalog' and frame.id==self.pending then
      if frame.status=='Accepted' then return true end
      self.pending=nil
      if frame.status=='QueueFull' and self.retries<3 then
        self.retries=self.retries+1; self.retry_scheduled=true
        local token,before=self.token,self.before
        vim.defer_fn(function()
          if self.token~=token then return end
          self.retry_scheduled=false; self:refresh(false,before)
        end,100*self.retries)
      else self.blocked=true; api.notify('Список артефактов недоступен: '..tostring(frame.status)) end
      return true
    elseif frame.type=='artifact_catalog' then
      if frame.id~=self.pending then return true end
      self.pending=nil
      local value=attachment()
      if not value or frame.session~=value.session or frame.incarnation~=value.incarnation then return true end
      self.retries=0; self.begin=frame.begin
      if self.before==0 then self.catalog_total=math.max(self.catalog_total or 0,frame.total or 0) end
      local count=vim.tbl_count(self.known)
      for _,d in ipairs(frame.artifacts or {}) do
        if valid(d,value.session) then
          local k=key(d)
          if self.known[k] or count<384 then
            if not self.known[k] then count=count+1 end
            self.known[k]=vim.deepcopy(d)
            local prior=self.preferred[d.artifact_id]
            if not prior or frame['end']>=prior.through then self.preferred[d.artifact_id]={descriptor=d,through=frame['end']} end
          end
        end
      end
      for _,doc in ipairs(frame.documents or {}) do
        if doc.session_id==value.session and type(doc.document_id)=='string' and doc.document_id:match('^d_[0-9a-f]+$')
          and vim.tbl_contains({'source','diff'},doc.kind) then
          if not self.documents[doc.document_id] then
            if #self.document_order==64 then self.documents[table.remove(self.document_order,1)]=nil end
            self.document_order[#self.document_order+1]=doc.document_id
          end
          self.documents[doc.document_id]=doc
        end
      end
      if self.deferred_document then
        local wanted=self.deferred_document
        if self.documents[wanted.id] then
          self.deferred_document=nil;self:open_document(wanted.id,wanted.origin,wanted.regranted)
        elseif frame.begin>0 and wanted.pages<16 and (self.before==0 or frame.begin<self.before) then
          wanted.pages=wanted.pages+1;self:refresh(false,frame.begin)
        else self.deferred_document=nil;api.notify('Документ не найден на загруженных страницах') end
      end
      if self.deferred_open then
        local wanted=self.deferred_open
        local d=self.known[wanted.id..'/'..wanted.revision]
        if d then
          self.deferred_open=nil; self:fetch(d,wanted.explicit~=false,wanted.regranted,wanted.origin,wanted.pinned)
        elseif frame.begin>0 and wanted.pages<16 and (self.before==0 or frame.begin<self.before) then
          wanted.pages=wanted.pages+1; self:refresh(false,frame.begin)
        else
          self.deferred_open=nil; api.notify('Версия не найдена на загруженных страницах: :AnsaArtifactsEarlier продолжает просмотр')
        end
      end
      if self.pick_after then self.pick_after=false; self:choose() end
      if self.pick_documents_after then self.pick_documents_after=false;self:choose_document() end
      local current=vim.api.nvim_get_current_buf()
      for _,doc in pairs(self.docs) do
        local latest=self.preferred[doc.descriptor.artifact_id]
        if doc.descriptor.session_id==value.session and doc.buffer==current and latest and (doc.needs_read or latest.descriptor.revision~=doc.descriptor.revision) then self:fetch(latest.descriptor,false); break end
      end
      for _,doc in pairs(self.saved) do
        if doc.descriptor.session_id==value.session and doc.buffer==current and doc.descriptor.mime=='image/png'
          and not require('ansa.images').owned[current] and self.known[key(doc.descriptor)] then
          self:fetch(doc.descriptor,false,nil,nil,true); break
        end
      end
      if self.wanted_total>(self.catalog_total or 0) then self:refresh(false) end
      return true
    elseif frame.type=='document' and self.read and self.read.kind=='document' and frame.id==self.read.id then
      local pending=self.read;self.read=nil
      local value=attachment();local doc=frame.document
      if not value or type(doc)~='table' or doc.document_id~=pending.document_id or doc.session_id~=value.session then return true end
      show_document(doc,unchanged(pending.origin))
      return true
    elseif frame.type=='artifact' and self.read and frame.id==self.read.id then
      local pending=self.read; self.read=nil
      local queued = self.next_auto
      self.next_auto = nil
      local follow = queued and unchanged(queued.origin)
      local value=attachment()
      if not value or not vim.deep_equal(frame.descriptor,pending.descriptor) or frame.descriptor.session_id~=value.session then return true end
      local d = frame.descriptor
      local collection = pending.pinned and self.saved or self.docs
      local name = pending.pinned and saved_key(d) or document_key(d)
      local old = collection[name]
      local focus = pending.explicit and unchanged(pending.origin)
      if d.mime=='image/png' then
        local ok,images=pcall(require,'ansa.images')
        if ok then
          images.open(d,frame.path,{buffer=old and old.buffer,focus=focus},function(buf)
            collection[name]={buffer=buf,descriptor=vim.deepcopy(d)}
          end)
        else api.notify('PNG сохранён: '..tostring(frame.path)) end
      elseif type(frame.content)=='string' then
        local buf = text_buffer(d,frame.content,focus,old)
        collection[name]={buffer=buf,descriptor=vim.deepcopy(d)}
        if pending.explicit and not focus then api.notify('Отчёт готов: :AnsaArtifacts открывает его') end
      end
      if follow and vim.api.nvim_get_current_buf() == queued.origin.buffer then
        self:fetch(queued.descriptor,false,nil,nil,queued.pinned)
      end
      return true
    elseif frame.type=='error' then
      if self.pending and frame.id==self.pending then
        self.pending=nil; self.blocked=true; api.notify('Не удалось прочитать список: '..tostring(frame.code)); return true
      elseif self.read and frame.id==self.read.id then
        local pending=self.read
        self.read=nil; self.next_auto=nil
        if pending.kind=='document' and frame.code=='document_not_loaded' and not pending.regranted then
          self.documents[pending.document_id]=nil
          for i,id in ipairs(self.document_order) do
            if id==pending.document_id then table.remove(self.document_order,i);break end
          end
          self.deferred_document={id=pending.document_id,pages=0,regranted=true,origin=pending.origin}
          self:refresh(false)
        elseif pending.kind~='document' and frame.code=='artifact_not_committed' and not pending.regranted then
          self.known[key(pending.descriptor)]=nil
          self.deferred_open={id=pending.descriptor.artifact_id,revision=pending.descriptor.revision,pages=0,
            regranted=true,origin=pending.origin,pinned=pending.pinned,explicit=pending.explicit}
          self:refresh(false)
        else api.notify('Артефакт недоступен: '..tostring(frame.code)) end
        return true
      end
    end
    return false
  end
  return self
end
return M
