"""Prepared source/diff documents in real Neovim, with no filesystem writes."""
import argparse
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
from parity.frontend_io import environment


def run(output):
    output.mkdir(parents=True,exist_ok=False)
    events,errors=[],[]
    with tempfile.TemporaryDirectory(prefix='ansa-documents-',dir='/tmp') as directory:
        root=Path(directory); endpoint=root/'workspace.sock'; original=root/'source.rs'
        original.write_text('ORIGINAL_DISK_UNCHANGED\n')
        def document(kind,id):
            return dict(document_id=id,session_id='session',producer_operation_id='operation_'+kind,kind=kind,path=str(original),filetype='rust',
                        first_line=2,before='first\nsecond\n' if kind=='source' else 'let counter = 1;\n',after='' if kind=='source' else 'let counter = 2;\n',
                        client='editor',editor_incarnation='editor',buffer=1,changedtick=2,sequence=1)
        ids=dict(source='d_'+'a'*64,diff='d_'+'b'*64)
        documents={id:document(kind,id) for kind,id in ids.items()}
        ids['unnamed']='d_'+'c'*64
        documents[ids['unnamed']]=dict(document('diff',ids['unnamed']),path=None,first_line=20)
        listener=socket.socket(socket.AF_UNIX);listener.bind(str(endpoint));listener.listen(1);listener.settimeout(8)
        def serve():
            try:
                stream,_=listener.accept()
                with stream,stream.makefile('rb') as pipe:
                    stream.settimeout(8);hello=json.loads(pipe.readline());assert hello['role']=='viewer'
                    def emit(value):stream.sendall(json.dumps(value).encode()+b'\n')
                    emit(dict(type='welcome',version=1,role='viewer',session='session',incarnation='inc',capabilities=['artifact_catalog','artifact_read','document_read']))
                    emit(dict(type='snapshot',version=1,session='session',incarnation='inc',through=1,state=dict(phase='completed',transcript_total=0,timeline_total=5)))
                    for line in pipe:
                        req=json.loads(line);kind,id=req['kind'],req['id'];events.append(kind)
                        if kind=='snapshot':emit(dict(type='posted',kind=kind,id=id,status='Accepted'))
                        elif kind=='history':
                            emit(dict(type='posted',kind=kind,id=id,status='Accepted'))
                            emit(dict(type='history',id=id,session='session',incarnation='inc',begin=0,end=0,total=0,entries=[]))
                        elif kind=='artifact_catalog':
                            emit(dict(type='posted',kind=kind,id=id,status='Accepted'))
                            metadata=[{key:doc[key] for key in ['document_id','session_id','kind','path','filetype','first_line']} for doc in documents.values()]
                            if (root/'evicted').exists() and req['before']==0:
                                emit(dict(type='artifact_catalog',id=id,session='session',incarnation='inc',begin=10,end=20,total=20,artifacts=[],documents=[]))
                            else:
                                emit(dict(type='artifact_catalog',id=id,session='session',incarnation='inc',begin=0,end=5,total=20 if (root/'evicted').exists() else 5,artifacts=[],documents=metadata))
                        elif kind=='document_read':
                            if (root/'evict').exists():
                                (root/'evict').unlink();(root/'evicted').touch();events.append('cache_eviction')
                                emit(dict(type='error',id=id,code='document_not_loaded'))
                            else:emit(dict(type='document',id=id,document=documents[req['document_id']]))
                        else:raise AssertionError(req)
            except BaseException as error:errors.append(repr(error))
            finally:listener.close()
        worker=threading.Thread(target=serve,daemon=True);worker.start()
        plugin=Path(__file__).resolve().parents[1]/'integrations/ansa.nvim'
        script=root/'test.lua'
        script.write_text("""vim.opt.runtimepath:prepend(%s)
local a=require('ansa');a.setup();a.attach(%s)
assert(vim.wait(2000,function() return vim.tbl_count(a.status().documents)==3 end,10),'document metadata')
local source=vim.api.nvim_create_buf(true,false);vim.api.nvim_set_current_buf(source)
vim.api.nvim_buf_set_lines(source,0,-1,false,{'MY_UNSAVED_SOURCE'})
vim.cmd('AnsaDocuments '..%s)
assert(vim.wait(2000,function() return #vim.api.nvim_tabpage_list_wins(0)==2 end,10),'proposed diff windows')
local windows=vim.api.nvim_tabpage_list_wins(0)
for _,win in ipairs(windows) do
  assert(vim.wo[win].diff and not vim.bo[vim.api.nvim_win_get_buf(win)].modifiable,'proposal is not a read-only diff')
  assert(vim.wo[win].winbar:find('source.rs:2-3',1,true),'diff lost its source path and original range')
end
assert(vim.fn.diff_hlID(1,1)~=0,'native diff highlighting missing')
local edited=vim.api.nvim_get_current_buf()
assert(vim.api.nvim_buf_get_lines(edited,0,1,false)[1]=='let counter = 2;')
vim.bo[edited].modifiable=true;vim.api.nvim_buf_set_lines(edited,0,1,false,{'MY_PROPOSAL_DRAFT'});vim.bo[edited].modified=true
vim.cmd('AnsaDocuments '..%s)
assert(vim.wait(2000,function() return #vim.api.nvim_list_tabpages()==3 end,10),'reopen diff')
assert(vim.api.nvim_get_current_buf()==edited and vim.bo[edited].modified and vim.api.nvim_buf_get_lines(edited,0,1,false)[1]=='MY_PROPOSAL_DRAFT','reopen overwrote draft')
local before=vim.api.nvim_win_get_buf(vim.api.nvim_tabpage_list_wins(0)[1])
vim.cmd('tabclose');vim.cmd('tabclose')
vim.cmd('bdelete '..before)
assert(vim.api.nvim_buf_is_valid(before) and not vim.api.nvim_buf_is_loaded(before),'bdelete fixture must retain unloaded handle')
vim.cmd('AnsaDocuments '..%s)
assert(vim.wait(2000,function() return #vim.api.nvim_list_tabpages()==2 end,10),'reopen partially unloaded diff')
assert(vim.api.nvim_get_current_buf()==edited and vim.bo[edited].modified,'loaded proposal draft lost during reconstruction')
local restored=vim.api.nvim_win_get_buf(vim.api.nvim_tabpage_list_wins(0)[1])
assert(vim.api.nvim_buf_get_lines(restored,0,1,false)[1]=='let counter = 1;','unloaded diff source missing')
vim.cmd('tabclose');vim.cmd('bunload! '..edited)
vim.cmd('AnsaDocuments '..%s)
assert(vim.wait(2000,function() return #vim.api.nvim_list_tabpages()==2 end,10),'reopen unloaded proposal')
assert(vim.api.nvim_buf_get_lines(0,0,1,false)[1]=='let counter = 2;','unloaded proposal snapshot missing')
vim.cmd('tabclose')
assert(vim.api.nvim_get_current_buf()==source and vim.api.nvim_buf_get_lines(source,0,1,false)[1]=='MY_UNSAVED_SOURCE','proposal changed original editor buffer')
vim.cmd('AnsaDocuments '..%s)
assert(vim.wait(2000,function() return vim.api.nvim_buf_get_name(0):find('/before',1,true)~=nil end,10),'source view')
assert(not vim.bo.modifiable and vim.api.nvim_win_get_cursor(0)[1]==2,'source view line/read-only')
assert(vim.api.nvim_buf_get_lines(0,0,1,false)[1]=='first','source view contents')
local unloaded=vim.api.nvim_get_current_buf()
vim.api.nvim_set_current_buf(source);vim.cmd('bunload '..unloaded)
vim.cmd('AnsaDocuments '..%s)
assert(vim.wait(2000,function() return vim.api.nvim_buf_get_name(0):find('/before',1,true)~=nil end,10),'source bunload reopen')
assert(vim.api.nvim_buf_get_lines(0,0,2,false)[2]=='second' and not vim.bo.modifiable,'source snapshot lost after bunload')
vim.api.nvim_set_current_buf(source)
vim.fn.writefile({'evict'},%s)
vim.cmd('AnsaDocuments '..%s)
assert(vim.wait(2000,function() return vim.api.nvim_buf_get_name(0):find('/before',1,true)~=nil end,10),'canonical regrant after cache eviction')
assert(vim.api.nvim_buf_get_lines(0,0,1,false)[1]=='first','regrant lost source document')
vim.cmd('AnsaDocuments '..%s)
assert(vim.wait(2000,function() return #vim.api.nvim_tabpage_list_wins(0)==2 end,10),'unnamed selection diff')
for _,win in ipairs(vim.api.nvim_tabpage_list_wins(0)) do
  assert(vim.wo[win].winbar:find('Выделение:20-21',1,true),'JSON null path broke selection context')
end
vim.cmd('qa!')
"""%(json.dumps(str(plugin)),json.dumps(str(endpoint)),json.dumps(ids['diff']),json.dumps(ids['diff']),json.dumps(ids['diff']),json.dumps(ids['diff']),json.dumps(ids['source']),json.dumps(ids['source']),json.dumps(str(root/'evict')),json.dumps(ids['source']),json.dumps(ids['unnamed'])))
        child=subprocess.run(['nvim','--clean','--headless','-l',str(script)],env=environment(root,root,False),capture_output=True,timeout=10)
        worker.join(timeout=2)
        (output/'stdout').write_bytes(child.stdout);(output/'stderr').write_bytes(child.stderr)
        (output/'observations.json').write_text(json.dumps(dict(events=events,errors=errors),indent=2))
        assert child.returncode==0,child.stderr.decode(errors='replace')
        assert not worker.is_alive() and not errors,errors
        assert original.read_text()=='ORIGINAL_DISK_UNCHANGED\n'
        assert all(kind in ['snapshot','history','artifact_catalog','document_read','cache_eviction'] for kind in events)
        eviction=events.index('cache_eviction')
        assert events[eviction+1:]==['artifact_catalog','artifact_catalog','document_read','document_read'],events
    result=dict(passed=9,cases=['readonly-native-diff-highlighting','proposal-draft-preserved-on-reopen','original-buffer-and-file-preserved','source-view-exact-line','bdelete-diff-source-retains-loaded-draft','bunload-proposal-rehydrates-snapshot','bunload-source-rehydrates-snapshot','evicted-document-bounded-canonical-regrant','JSON-null-path-unnamed-selection-diff-context'])
    (output/'report.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--output',required=True,type=Path);run(parser.parse_args().output)
