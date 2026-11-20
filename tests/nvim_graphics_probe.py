"""Real Nvim TUI APC parsing with a controlled terminal responder, not a GPU gate."""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time

from parity.frontend_io import Pty


def run(output):
    output.mkdir(parents=True,exist_ok=False)
    observations=[]
    plugin=Path(__file__).resolve().parents[1]/'integrations/ansa.nvim'
    packet=re.compile(rb'\x1b_G([^;]*);([A-Za-z0-9+/=]*)\x1b\\')
    for case in ['success','reject','timeout','cancel','cancel-paste','no-color']:
        with tempfile.TemporaryDirectory(prefix='ansa-probe-',dir='/tmp') as directory:
            root=Path(directory); report=root/'report.json'; ready=root/'ready'
            script=root/'probe.lua'
            script.write_text("""vim.opt.runtimepath:prepend(%s)
local image=require('ansa.images')
local calls,result,paste_calls=0,nil,0
local previous=vim.paste
vim.paste=function(lines,phase) paste_calls=paste_calls+1;return previous(lines,phase) end
local function save()
  vim.fn.writefile({vim.json.encode({calls=calls,result=result,state=image.state,paste_calls=paste_calls,lines=vim.api.nvim_buf_get_lines(0,0,-1,false),uis=vim.api.nvim_list_uis()})},%s)
  vim.cmd('qa!')
end
vim.cmd('startinsert')
vim.defer_fn(function()
  vim.fn.writefile({'ready'},%s)
  image.probe(function(ok,reason)
    calls=calls+1; result={supported=ok,reason=reason}
    vim.defer_fn(save,150)
  end)
  if (%s):match('^cancel') then vim.defer_fn(function() image.clear(); vim.defer_fn(save,150) end,50) end
end,100)
"""%(json.dumps(str(plugin)),json.dumps(str(report)),json.dumps(str(ready)),json.dumps(case)))
            screen=Pty(shutil.which('nvim'),root,root,root,'',case=='no-color',argv=['--clean','-c','luafile '+str(script)])
            cursor=0; queries=[]; typed=False
            try:
                deadline=time.monotonic()+5
                while screen.child.poll() is None:
                    assert time.monotonic()<deadline,(case,queries)
                    screen.drain(.01)
                    if ready.exists() and not typed:
                        screen.send('HELLOЖ界'); typed=True
                    for match in packet.finditer(screen.raw,cursor):
                        cursor=match.end()
                        header=match[1].decode(); fields=dict(value.split('=',1) for value in header.split(','))
                        if fields.get('a')!='q':continue
                        assert fields.get('q')=='0'
                        id=int(fields['i']); queries.append(dict(id=id,format=fields['f']))
                        if case=='timeout':continue
                        if case.startswith('cancel'):
                            time.sleep(.08)
                            if case=='cancel-paste':screen.send('\x1b[200~LATEPASTE')
                            screen.send(f'\x1b_Gi={id};OK\x1b\\')
                            if case=='cancel-paste':screen.send('\x1b[201~')
                            continue
                        screen.send(f'\x1b_Gi={id+999};OK\x1b\\')
                        screen.send('\x1b]4;1;rgb:ffff/0000/0000\x1b\\')
                        response='EINVAL' if case=='reject' and fields['f']=='100' else 'OK'
                        if case=='success' and fields['f']=='24':
                            screen.send('\x1b[200~PASTED\ntext\x1b_Gi=0;LITERAL\x1b\\')
                        screen.send(b'\x1b_Gi=')
                        time.sleep(.005)
                        screen.send(f'{id};{response}\x1b\\')
                        if case=='success' and fields['f']=='24':screen.send('\x1b[201~')
                        if fields['f']=='100':screen.send(f'\x1b_Gi={id};OK\x1b\\')
                screen.finish()
                assert report.exists(),(case,screen.raw[-2000:])
                result=json.loads(report.read_text())
                if case=='success':
                    assert result['calls']==1 and result['result']['supported'] and result['state']=='png',result
                    assert ''.join(result['lines'])=='HELLOЖ界PASTEDtext\x1b_Gi=0;LITERAL\x1b\\' and result['paste_calls']>0,result
                elif case.startswith('cancel'):assert result['calls']==0 and result['state']=='text',result
                else:assert result['calls']==1 and not result['result']['supported'] and result['state']=='text',result
                if case!='success':assert ''.join(result['lines'])=='HELLOЖ界'+('LATEPASTE' if case=='cancel-paste' else ''),result
                assert len(queries)==(2 if case in ['success','reject'] else 0 if case=='no-color' else 1),(case,queries)
                observations.append(dict(case=case,queries=queries,report=result))
            finally:
                (output/(case+'.pty')).write_bytes(screen.raw)
                screen.close()
    result=dict(passed=len(observations),cases=observations,note='Controlled responder verifies Nvim input/capability lifecycle only; real Kitty/Ghostty rendering remains required')
    (output/'report.json').write_text(json.dumps(result,ensure_ascii=False,indent=2))
    print(json.dumps(dict(passed=len(observations),cases=[value['case'] for value in observations])))


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--output',required=True,type=Path)
    run(parser.parse_args().output)
