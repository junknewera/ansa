local M = { configured = false, generation = 0, owned = {}, serial = 0, state = 'text' }
local uv = vim.uv
local png = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg=='
local ids, order, paste_tail = {}, {}, ''
local receive
local function release(buf)
  local owned=M.owned[buf]
  if not owned then return end
  M.owned[buf]=nil
  if owned.conversion then
    -- Pinned Snacks Proc:kill only marks a running process. Mark this
    -- conversion's queued processes too, so its global queue drains them
    -- through on_exit without starting a converter after the viewer closes.
    for _,step in ipairs(owned.conversion.steps or {}) do if step.proc then step.proc.aborted=true end end
    owned.conversion:abort()
  end
  if M.configured then
    local ok,image=pcall(require,'snacks.image')
    if ok then image.placement.clean(buf) end
  end
end
vim.api.nvim_create_autocmd('BufWipeout',{callback=function(event) release(event.buf) end})
vim.api.nvim_create_autocmd({'BufWinEnter','WinEnter','TabEnter'},{callback=function()
  local buf=vim.api.nvim_get_current_buf()
  local owned=M.owned[buf]
  if not owned or not owned.placement then return end
  vim.schedule(function()
    if M.owned[buf]==owned and vim.api.nvim_buf_is_valid(buf) and not vim.bo[buf].modified then owned.placement:show() end
  end)
end})

local function own_id()
  local bytes=assert(uv.random(4))
  local id=(bytes:byte(1)*16777216+bytes:byte(2)*65536+bytes:byte(3)*256+bytes:byte(4))%2147483646+1
  while ids[tostring(id)] do id=id%2147483646+1 end
  local key=tostring(id)
  ids[key]=true; order[#order+1]=key
  if #order>32 then ids[table.remove(order,1)]=nil end
  return id
end
local function install_paste_filter()
  if M.paste_filter then return vim.paste==M.paste_filter end
  local previous=vim.paste
  -- Nvim 0.12 forwards APC bytes inside bracketed paste to vim.paste. Remove
  -- only responses bearing one of this renderer's random query IDs, forwarding
  -- every other byte and the original paste phases to the existing handler.
  M.paste_filter=function(lines,phase)
    if phase==1 or phase==-1 then paste_tail='' end
    local clean,responses={},{}
    for index,line in ipairs(lines) do
      if index==1 then line=paste_tail..line; paste_tail='' end
      line=line:gsub('(\27_Gi=(%d+);([ -~]-)\27\\)',function(full,id,response)
        if ids[id] and #full<=512 then responses[#responses+1]=full; return '' end
        return full
      end)
      if index==#lines and phase~=3 and phase~=-1 then
        local from=math.max(1,#line-511)
        for pos=from,#line do
          if line:byte(pos)==27 then
            local tail=line:sub(pos)
            local candidate=false
            for id in pairs(ids) do
              local prefix='\27_Gi='..id..';'
              if prefix:sub(1,#tail)==tail or tail:sub(1,#prefix)==prefix then candidate=true; break end
            end
            if candidate then paste_tail=tail; line=line:sub(1,pos-1); break end
          end
        end
      end
      clean[#clean+1]=line
    end
    if (phase==3 or phase==-1) and paste_tail~='' then
      if #clean==0 then clean[1]=paste_tail else clean[#clean]=clean[#clean]..paste_tail end
      paste_tail=''
    end
    local ok,result=pcall(previous,clean,phase)
    if not ok or result==false then paste_tail='' end
    if #responses>0 then vim.schedule(function() for _,sequence in ipairs(responses) do receive(sequence) end end) end
    if not ok then error(result,0) end
    return result
  end
  vim.paste=M.paste_filter
  return true
end

local function tty()
  if not vim.api.nvim_ui_send or vim.env.NO_COLOR~=nil or vim.env.TERM=='dumb'
    or vim.env.TMUX or vim.env.SSH_TTY or vim.env.SSH_CONNECTION then return false end
  local uis=vim.api.nvim_list_uis()
  if #uis~=1 then return false end
  local ok,info=pcall(vim.api.nvim_get_chan_info,uis[1].chan)
  return ok and info.client and info.client.name=='nvim-tui' and uis[1].stdout_tty and uis[1].chan or nil
end
local function cancel_probe()
  local active=M.active
  if not active then return end
  M.active=nil
  if active.autocmd then pcall(vim.api.nvim_del_autocmd,active.autocmd) end
  active.timer:stop(); active.timer:close()
end
function M.probe(callback)
  cancel_probe()
  local ui=tty()
  if not ui then M.state='text'; vim.schedule(function() callback(false,'terminal') end); return end
  if not install_paste_filter() then M.state='text'; vim.schedule(function() callback(false,'paste-handler') end); return end
  local active={timer=uv.new_timer(),stage=1,ui=ui}
  M.active=active; M.state='probing'
  local function finish(ok,reason)
    if M.active~=active then return end
    cancel_probe(); M.state=ok and 'png' or 'text'; callback(ok,reason,active.ui)
  end
  local function query()
    if tty()~=active.ui then finish(false,'terminal-changed'); return end
    active.id=own_id()
    local data=active.stage==1 and 'AAAA' or png
    local format=active.stage==1 and '24,s=1,v=1' or '100'
    vim.api.nvim_ui_send('\27_Ga=q,t=d,f='..format..',q=0,i='..active.id..';'..data..'\27\\')
  end
  receive=function(sequence)
    if type(sequence)~='string' or #sequence>512 then return end
    sequence=sequence:gsub('\27\\$','')
    local id,response=sequence:match('^\27_Gi=(%d+);(.+)$')
    if tonumber(id)~=active.id or M.active~=active then return end
    if response~='OK' then finish(false,'query'); return end
    if active.stage==1 then active.stage=2; query() else finish(true,'png') end
  end
  active.autocmd=vim.api.nvim_create_autocmd('TermResponse',{callback=function(ev) receive(ev.data and ev.data.sequence) end})
  active.timer:start(1000,0,vim.schedule_wrap(function() finish(false,'timeout') end))
  query()
end
function M.clear()
  M.generation=M.generation+1
  cancel_probe()
  for buf in pairs(M.owned) do release(buf) end
  M.owned={}; M.state='text'
end
vim.api.nvim_create_autocmd({'UIEnter','UILeave'},{callback=function() M.clear() end})
function M.setup()
  local ok,snacks=pcall(require,'snacks')
  if not ok then return false end
  snacks.setup({image={enabled=true,formats={'png'},force=false,doc={enabled=false},math={enabled=false},convert={notify=false}}})
  M.configured=true
  return true
end
local function fallback(buf,d,path)
  vim.bo[buf].modifiable=true; vim.bo[buf].filetype='markdown'
  vim.api.nvim_buf_set_lines(buf,0,-1,false,{'# '..d.title,'','PNG · '..d.size_bytes..' bytes','',path})
  vim.bo[buf].modified=false; vim.bo[buf].modifiable=false
end
function M.open(d,path,options,callback)
  options=options or {}
  local generation=M.generation
  local buf=options.buffer
  if not buf or not vim.api.nvim_buf_is_valid(buf) or vim.bo[buf].modified then
    buf=vim.api.nvim_create_buf(false,true)
    vim.api.nvim_buf_set_name(buf,'ansa://image/'..d.session_id..'/'..d.artifact_id..'/'..d.revision..'/'..tostring(uv.hrtime()))
    vim.bo[buf].buftype='acwrite'; vim.bo[buf].bufhidden='hide'; vim.bo[buf].swapfile=false
  end
  vim.b[buf].ansa_artifact=vim.deepcopy(d)
  release(buf)
  fallback(buf,d,path)
  if options.focus then vim.cmd('hide buffer '..buf) end
  local owned={tick=vim.api.nvim_buf_get_changedtick(buf),revision=d.revision}
  M.owned[buf]=owned
  if callback then callback(buf) end
  if not M.configured then return buf end
  local function current(ui)
    return M.generation==generation and vim.api.nvim_buf_is_valid(buf) and M.owned[buf]==owned
      and not vim.bo[buf].modified and vim.api.nvim_buf_get_changedtick(buf)==owned.tick and tty()==ui
  end
  M.probe(function(supported,_,ui)
    if not supported or not current(ui) then return end
    local image=require('snacks.image')
    image.terminal.detect(function()
      if not current(ui) then return end
      if not image.supports_terminal() then return end
      -- Prepare metadata before creating a placement. The pinned renderer's
      -- loading timer can otherwise survive a closed placement and erase a
      -- newer revision's decorations while its converter finishes.
      owned.conversion=image.convert.convert({src=path,on_done=function(convert)
        vim.schedule(function()
          if not current(ui) or not convert:ready() then return end
          image.placement.clean(buf)
          vim.bo[buf].modifiable=true
          vim.api.nvim_buf_set_lines(buf,0,-1,false,{''})
          vim.bo[buf].modified=false;vim.bo[buf].filetype='image';vim.bo[buf].modifiable=false
          owned.placement=image.placement.new(buf,path,{conceal=true,auto_resize=true})
        end)
      end})
      owned.conversion:run()
    end)
  end)
  return buf
end
return M
