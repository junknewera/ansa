"""Real Neovim reports: dirty revisions, reconnect, scope switch and errors."""
import argparse
import hashlib
import json
import select
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

from parity.frontend_io import environment


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    events, errors = [], []
    with tempfile.TemporaryDirectory(prefix="ansa-nvim-artifacts-", dir="/tmp") as directory:
        root = Path(directory)
        endpoint = root / "workspace.sock"
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(endpoint)); listener.listen(3); listener.settimeout(12)
        texts = {version: "# Report " + version + "\n\n" + "\n".join(f"line {n} Ж界" for n in range(1,101)) for version in ["a","b","c"]}
        def descriptor(session, version):
            return dict(artifact_id="a_monthly_report", revision=version * 64, session_id=session, producer_operation_id="operation_"+version,
                        mime="text/markdown", size_bytes=len(texts[version].encode()), sha256=hashlib.sha256(texts[version].encode()).hexdigest(),
                        title="report.md", storage_ref=version*64+"/report.md")
        def serve():
            try:
                advanced=False
                for connection in range(3):
                    stream,_ = listener.accept()
                    with stream, stream.makefile("rb",buffering=0) as pipe:
                        stream.settimeout(12)
                        hello=json.loads(pipe.readline()); assert hello["role"]=="viewer"
                        session="other" if connection==2 else "original"
                        incarnation="inc_a" if connection==0 else "inc_b"
                        catalogs=reads=0
                        def emit(frame): stream.sendall(json.dumps(frame,ensure_ascii=False).encode()+b"\n")
                        emit(dict(type="welcome",version=1,role="viewer",session=session,incarnation=incarnation,capabilities=["snapshot","history","artifact_catalog","artifact_read"]))
                        emit(dict(type="snapshot",version=1,session=session,incarnation=incarnation,through=1,state=dict(phase="completed",transcript_total=0,timeline_total=3 if connection==0 else 20,durable=True)))
                        changed=False
                        while True:
                            if connection==0 and not changed and (root/"dirty-ready").exists():
                                changed=True
                                emit(dict(type="snapshot",version=1,session=session,incarnation=incarnation,through=2,state=dict(phase="generating",transcript_total=0,timeline_total=10,durable=True)))
                            if not select.select([stream],[],[],.01)[0]: continue
                            line=pipe.readline()
                            if not line: break
                            req=json.loads(line); kind,id=req["kind"],req["id"]
                            events.append(dict(connection=connection,kind=kind,id=id,before=req.get('before')))
                            assert req["session"]==session and req["incarnation"]==incarnation
                            if kind=="snapshot": emit(dict(type="posted",id=id,kind=kind,status="Accepted"))
                            elif kind=="history":
                                emit(dict(type="posted",id=id,kind=kind,status="Accepted"))
                                emit(dict(type="history",id=id,session=session,incarnation=incarnation,begin=0,end=0,total=0,entries=[]))
                            elif kind=="artifact_catalog":
                                catalogs+=1
                                if connection==1 and catalogs==1: time.sleep(.3)
                                emit(dict(type="posted",id=id,kind=kind,status="Accepted"))
                                if req['before']>0:
                                    advanced=True
                                    emit(dict(type="snapshot",version=1,session=session,incarnation=incarnation,through=3,state=dict(phase="generating",transcript_total=0,timeline_total=20,durable=True)))
                                    time.sleep(.1)
                                    emit(dict(type="artifact_catalog",id=id,session=session,incarnation=incarnation,begin=0,end=3,total=20,artifacts=[descriptor(session,'a')]))
                                else:
                                    version="a" if connection==0 and catalogs==1 else "c" if advanced else "b"
                                    end=dict(a=3,b=10,c=20)[version]
                                    emit(dict(type="artifact_catalog",id=id,session=session,incarnation=incarnation,begin=0 if version=='a' else end-5,end=end,total=end,artifacts=[descriptor(session,version)]))
                            elif kind=="artifact_read":
                                reads+=1
                                if connection==0 and reads==1: time.sleep(.2)
                                assert connection!=2,"old report triggered an automatic read in another session"
                                if connection==1 and reads==2:
                                    emit(dict(type="error",id=id,code="artifact_corrupt")); continue
                                version=req["revision"][0]
                                emit(dict(type="artifact",id=id,availability="available",descriptor=descriptor(session,version),path=str(root/"report.md"),content=texts[version]))
                            else: raise AssertionError(req)
            except BaseException as error: errors.append(repr(error))
            finally: listener.close()
        worker=threading.Thread(target=serve,daemon=True); worker.start()
        plugin=Path(__file__).resolve().parents[1]/"integrations/ansa.nvim"
        script=root/"test.lua"
        script.write_text("""vim.opt.runtimepath:prepend(%s)
local notifications={}
vim.notify=function(message) notifications[#notifications+1]=message end
vim.ui.select=function(_,_,callback) callback(nil) end
local a=require('ansa'); a.setup()
local endpoint=%s
local id='a_monthly_report'
local function attach(session,revision)
  a.attach(endpoint)
  assert(vim.wait(2500,function()
    local state=a.status(); local d=state.artifacts[id..'/'..revision]
    return state.snapshot and state.snapshot.session==session and d and d.session_id==session
  end,10),'catalog attachment '..session)
end
local ra,rb,rc=string.rep('a',64),string.rep('b',64),string.rep('c',64)
attach('original',ra)
local source=vim.api.nvim_create_buf(true,false); vim.api.nvim_set_current_buf(source)
vim.api.nvim_buf_set_lines(source,0,-1,false,{'ORIGINAL_DRAFT'})
vim.cmd('AnsaArtifact '..id..' '..ra)
vim.api.nvim_buf_set_lines(source,0,-1,false,{'DRAFT_TYPED_WHILE_LOADING'})
assert(vim.wait(2000,function() return a.status().artifact_documents['original/'..id]~=nil end,10),'background report')
assert(vim.api.nvim_get_current_buf()==source and vim.api.nvim_buf_get_lines(source,0,1,false)[1]=='DRAFT_TYPED_WHILE_LOADING','delayed artifact stole active editor focus')
vim.cmd('AnsaArtifact '..id..' '..ra)
assert(vim.wait(2000,function() return vim.api.nvim_buf_get_name(0):match('^ansa://artifact/')~=nil end,10),'initial report')
local old=vim.api.nvim_get_current_buf()
vim.api.nvim_win_set_cursor(0,{60,0}); vim.fn.winrestview({topline=50})
vim.bo[old].modifiable=true
vim.api.nvim_buf_set_lines(old,0,1,false,{'MY_REPORT_DRAFT'})
vim.bo[old].modified=true
local cursor=vim.api.nvim_win_get_cursor(0); local view=vim.fn.winsaveview()
vim.fn.writefile({'ready'},%s)
assert(vim.wait(2000,function() local doc=a.status().artifact_documents['original/'..id]; return doc and doc.descriptor.revision==rb end,10),'new revision')
local fresh=a.status().artifact_documents['original/'..id].buffer
assert(fresh~=old and vim.api.nvim_get_current_buf()==old,'dirty revision replaced current document')
assert(vim.api.nvim_buf_get_lines(old,0,1,false)[1]=='MY_REPORT_DRAFT' and vim.bo[old].modified,'draft changed')
assert(vim.deep_equal(cursor,vim.api.nvim_win_get_cursor(0)) and view.topline==vim.fn.winsaveview().topline,'draft cursor/scroll changed')
vim.cmd('AnsaArtifact '..id..' '..rb)
assert(vim.wait(2000,function() return vim.api.nvim_get_current_buf()==fresh end,10),'explicit new version')
vim.api.nvim_win_set_cursor(0,{60,0}); vim.fn.winrestview({topline=50})
local freshview=vim.fn.winsaveview()
vim.cmd('AnsaArtifactsEarlier')
assert(vim.wait(2000,function() return a.status().artifact_documents['original/'..id].descriptor.revision==rc end,10),'older response swallowed latest timeline growth')
assert(vim.api.nvim_get_current_buf()==fresh,'latest revision moved focus during older catalog read')
local versions
vim.ui.select=function(items,options,callback)
  versions=items
  assert(options.format_item(items[1]):find('последняя',1,true),'latest version label missing')
  callback(nil)
end
vim.cmd('AnsaArtifacts')
assert(vim.wait(2000,function() return versions~=nil end,10),'version picker')
assert(#versions==3 and versions[1].revision==rc,'picker hid saved revisions or did not put latest first')
local listed={};for _,d in ipairs(versions) do listed[d.revision]=true end
assert(listed[ra] and listed[rb] and listed[rc],'older result unavailable without typing an ID')
vim.cmd('AnsaArtifact '..id..' '..ra)
assert(vim.wait(2000,function() return vim.api.nvim_buf_get_lines(0,0,1,false)[1]=='# Report a' end,10),'old version did not open')
local pinned=vim.api.nvim_get_current_buf()
assert(pinned~=fresh and vim.api.nvim_buf_get_name(pinned):find(ra,1,true),'old version reused the live report URI')
vim.cmd('AnsaArtifacts')
assert(vim.wait(2000,function() return not a.status().artifact_read_pending end,10))
vim.wait(250,function() return false end,10)
assert(vim.api.nvim_get_current_buf()==pinned and vim.api.nvim_buf_get_lines(pinned,0,1,false)[1]=='# Report a','catalog replaced the explicitly selected old version')
vim.api.nvim_set_current_buf(fresh)
vim.cmd('AnsaDetach'); a.attach(endpoint)
assert(vim.wait(2000,function() return a.status().snapshot ~= nil end,10),'reconnect snapshot')
assert(not a.status().artifacts[id..'/'..rc],'old incarnation cache survived before the delayed catalog')
vim.cmd('AnsaArtifact '..id..' '..rc)
assert(vim.wait(2000,function() return a.status().artifacts[id..'/'..rc] and not a.status().artifact_read_pending end,10),'reconnect did not wait for canonical catalog before reread')
assert(vim.api.nvim_get_current_buf()==fresh and vim.api.nvim_win_get_cursor(0)[1]==60 and vim.fn.winsaveview().topline==freshview.topline,'reread changed cursor/scroll')
local before=vim.api.nvim_buf_get_lines(fresh,0,-1,false)
vim.cmd('AnsaArtifact '..id..' '..rc)
assert(vim.wait(2000,function() for _,message in ipairs(notifications) do if message:find('artifact_corrupt',1,true) then return true end end end,10),'corrupt artifact diagnostic')
assert(vim.deep_equal(before,vim.api.nvim_buf_get_lines(fresh,0,-1,false)),'failed read changed report')
vim.cmd('AnsaDetach'); attach('other',rc)
vim.wait(100,function() return false end,10)
assert(vim.api.nvim_get_current_buf()==fresh and vim.deep_equal(before,vim.api.nvim_buf_get_lines(fresh,0,-1,false)),'new session replaced old report')
assert(vim.api.nvim_buf_get_lines(old,0,1,false)[1]=='MY_REPORT_DRAFT','scope switch lost draft')
-- Two new revisions arrive before the first read finishes. The plugin's own
-- write of B must not look like the user changed the buffer and cancel C.
local seq = 0
local live = require('ansa.artifacts').new({
  attachment=function() return {session='queue',incarnation='inc',capabilities={'artifact_catalog'}} end,
  notify=function() end,
  send=function() seq=seq+1; return true,'q_'..seq end,
})
local function descriptor(revision)
  return {artifact_id='queued',revision=revision,session_id='queue',mime='text/markdown',size_bytes=1,title='Report'}
end
local da,db,dc=descriptor(ra),descriptor(rb),descriptor(rc)
local function catalog(total,d)
  live:observe({session='queue',incarnation='inc',state={timeline_total=total}})
  live:handle({type='artifact_catalog',id=live.pending,session='queue',incarnation='inc',begin=0,['end']=total,total=total,artifacts={d}})
end
local function show(d,content)
  live:handle({type='artifact',id=live.read.id,descriptor=d,content=content})
end
live:attached()
catalog(1,da)
live:open(da.artifact_id,ra)
show(da,'A')
catalog(2,db)
catalog(3,dc)
show(db,'B')
assert(live.read and live.read.descriptor.revision==rc,'write of B skipped queued C')
show(dc,'C')
assert(vim.api.nvim_get_current_line()=='C')
vim.cmd('qa!')
"""%(json.dumps(str(plugin)),json.dumps(str(endpoint)),json.dumps(str(root/'dirty-ready'))))
        child=subprocess.run(["nvim","--headless","--clean","-l",str(script)],env=environment(root,root,True),capture_output=True,timeout=15)
        worker.join(timeout=2)
        (output/"stdout").write_bytes(child.stdout); (output/"stderr").write_bytes(child.stderr)
        (output/"observations.json").write_text(json.dumps(dict(events=events,errors=errors),indent=2))
        assert child.returncode==0,child.stderr.decode(errors="replace")
        assert not worker.is_alive() and not errors,errors
        assert all(row["kind"] in ["snapshot","history","artifact_catalog","artifact_read"] for row in events)
        assert sum(row["kind"]=="artifact_read" for row in events)==8,events
        assert any(row['kind']=='artifact_catalog' and row['before']==5 for row in events),'older page race was not exercised'
        assert sum(row['connection']==0 and row['kind']=='history' for row in events)==1,'tool outcome waited for chat history refresh'
    result=dict(passed=12,cases=["delayed-read-does-not-steal-typing-focus","timeline-only-revision-update-before-Done","older-page-does-not-ack-latest-barrier","dirty-report-kept-and-new-revision-separated","cursor-scroll-preserved","saved-versions-picker-with-latest-label","old-version-keeps-its-uri-and-content","reconnect-rereads-without-mutation","delayed-reconnect-catalog-before-read","corrupt-read-preserves-document","cross-session-report-not-overwritten","queued-revisions-follow-after-own-buffer-write"])
    (output/"report.json").write_text(json.dumps(result,indent=2)); print(json.dumps(result))


if __name__=="__main__":
    parser=argparse.ArgumentParser(); parser.add_argument("--output",required=True,type=Path)
    run(parser.parse_args().output)
