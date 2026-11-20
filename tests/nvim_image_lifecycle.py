"""Real buffers with controlled asynchronous renderer and paste callbacks."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
from parity.frontend_io import environment


def run(output):
    output.mkdir(parents=True,exist_ok=False)
    with tempfile.TemporaryDirectory(prefix='ansa-image-life-',dir='/tmp') as directory:
        root=Path(directory); script=root/'test.lua'
        plugin=Path(__file__).resolve().parents[1]/'integrations/ansa.nvim'
        script.write_text("""vim.opt.runtimepath:prepend(%s)
vim.api.nvim_list_uis=function() return {{chan=1,stdout_tty=true}} end
vim.api.nvim_get_chan_info=function() return {client={name='nvim-tui'}} end
local m=require('ansa.images')
local probe=m.probe
local delayed,placed,conversions={},{},{}
local defer_conversion=false
package.preload['snacks.image']=function() return {
  terminal={detect=function(callback) delayed[#delayed+1]=callback end},
  supports_terminal=function() return true end,
  convert={convert=function(options)
    local value={options=options,ready=function() return true end,abort=function(self) self.aborted=true end}
    value.run=function(self) if defer_conversion then conversions[#conversions+1]=self else options.on_done(self) end end
    return value
  end},
  placement={clean=function() end,new=function(buf,path) placed[#placed+1]={buffer=buf,path=path} end}
} end
m.configured=true
m.probe=function(callback) callback(true,'png',1) end
local function d(revision) return {session_id='session',artifact_id='report_chart',revision=revision,title='chart.png',size_bytes=100,mime='image/png'} end
local buf=m.open(d('a'),'/tmp/a.png')
m.open(d('b'),'/tmp/b.png',{buffer=buf})
delayed[1](); assert(#placed==0,'old revision callback created a stale placement')
delayed[2](); assert(vim.wait(200,function() return #placed==1 end,10) and placed[1].path=='/tmp/b.png','new revision was not placed')
m.open(d('c'),'/tmp/c.png',{buffer=buf})
vim.bo[buf].modifiable=true; vim.api.nvim_buf_set_lines(buf,0,1,false,{'USER_IMAGE_DRAFT'}); vim.bo[buf].modified=true
delayed[3](); assert(#placed==1 and vim.api.nvim_buf_get_lines(buf,0,1,false)[1]=='USER_IMAGE_DRAFT','late renderer overwrote dirty buffer')
local other=m.open(d('d'),'/tmp/d.png')
m.clear(); delayed[4](); assert(#placed==1,'cleared attachment was rendered again')
defer_conversion=true
local pending=m.open(d('e'),'/tmp/e.png')
delayed[5]();assert(#conversions==1,'metadata conversion did not start')
vim.bo[pending].modifiable=true;vim.api.nvim_buf_set_lines(pending,0,1,false,{'CONVERSION_DRAFT'});vim.bo[pending].modified=true
conversions[1].options.on_done(conversions[1]);vim.wait(20,function() return false end)
assert(#placed==1 and vim.api.nvim_buf_get_lines(pending,0,1,false)[1]=='CONVERSION_DRAFT','late metadata completion overwrote draft')
local cancelled=m.open(d('f'),'/tmp/f.png');delayed[6]();m.clear()
assert(conversions[2].aborted,'clear did not abort metadata process')
conversions[2].options.on_done(conversions[2]);vim.wait(20,function() return false end)
assert(#placed==1,'closed metadata conversion resurrected placement')
m.probe=probe; m.configured=false
local packets,forwarded={},{}
vim.api.nvim_ui_send=function(data) packets[#packets+1]=data end
local behavior='normal'
vim.paste=function(lines,phase)
  forwarded[#forwarded+1]={lines=vim.deepcopy(lines),phase=phase}
  if behavior=='error' then error('OWNED_PASTE_FAILURE') end
  return behavior~='false'
end
m.probe(function() end)
local id=m.active.id
behavior='false'; assert(vim.paste({'\\27_Gi='..id..';'},1)==false)
behavior='normal'; vim.paste({'AFTER_CANCEL'},-1)
assert(vim.deep_equal(forwarded[#forwarded].lines,{'AFTER_CANCEL'}),'cancelled paste prefix entered a new stream')
behavior='error'; assert(not pcall(vim.paste,{'\\27_Gi='..id..';'},1))
behavior='normal'; vim.paste({'AFTER_ERROR'},-1)
assert(vim.deep_equal(forwarded[#forwarded].lines,{'AFTER_ERROR'}),'failed paste prefix entered a new stream')
vim.paste({'\\27_Gi='..id..';'},1); vim.paste({'FRESH_STREAM'},1)
assert(vim.deep_equal(forwarded[#forwarded].lines,{'FRESH_STREAM'}),'separate paste streams were joined')
local unrelated='\\27_Gi=0;LITERAL\\27\\\\'
vim.paste({unrelated},-1)
assert(vim.deep_equal(forwarded[#forwarded].lines,{unrelated}),'unrelated user APC was removed')
vim.paste({'\\27_Gi='..id..';'},1); vim.paste({'OK\\27\\\\'},3)
assert(vim.wait(200,function() return #packets==2 end,10),'fragmented paste APC did not advance to PNG query')
m.clear(); packets={}
local result
m.probe(function(ok,reason) result={ok=ok,reason=reason} end)
id=m.active.id
vim.api.nvim_list_uis=function() return {{chan=1,stdout_tty=true},{chan=2,stdout_tty=true}} end
vim.api.nvim_exec_autocmds('TermResponse',{data={sequence='\\27_Gi='..id..';OK'}})
assert(result and not result.ok and result.reason=='terminal-changed' and #packets==1,'PNG query broadcast after UI audience changed')
m.clear(); vim.cmd('qa!')
"""%json.dumps(str(plugin)))
        child=subprocess.run(['nvim','--clean','--headless','-l',str(script)],env=environment(root,root,False),capture_output=True,timeout=5)
        (output/'stdout').write_bytes(child.stdout); (output/'stderr').write_bytes(child.stderr)
        assert child.returncode==0,child.stderr.decode(errors='replace')
    result=dict(passed=10,cases=['stale-revision-callback-rejected','dirty-buffer-callback-rejected','cleared-scope-callback-rejected',
                              'dirty-buffer-metadata-completion-rejected','closed-metadata-process-aborted-and-completion-ignored',
                              'prior-paste-cancel-tail-cleared','prior-paste-error-tail-cleared','paste-stream-boundaries',
                              'unrelated-APC-preserved-and-split-own-APC-consumed','multiple-UI-query-refused'])
    (output/'report.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--output',required=True,type=Path);run(parser.parse_args().output)
