"""Pinned package smoke: real parsers/rendered extmarks and safe image config."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
from parity.frontend_io import environment


def run(binary,output):
    output.mkdir(parents=True,exist_ok=False)
    with tempfile.TemporaryDirectory(prefix='ansa-renderer-',dir='/tmp') as directory:
        root=Path(directory); script=root/'test.lua'; report=root/'report.json'
        script.write_text("""local image=require('snacks.image')
assert(image.config.doc.enabled==false and image.config.math.enabled==false and image.config.force==false,'unsafe image configuration')
assert(image.config.convert.notify==false,'cancelled metadata conversion must not send global notifications')
assert(vim.fn.exepath('magick'):match('^/nix/store/'),'pinned converter missing from the editor wrapper')
assert(vim.deep_equal(image.config.formats,{'png'}),'non-PNG formats enabled')
vim.treesitter.language.add('markdown');vim.treesitter.language.add('markdown_inline')
vim.api.nvim_buf_set_lines(0,0,-1,false,{'# Monthly totals','','| Month | Total |','| --- | ---: |','| 2026-01 | 0.3 |','','```rust','let total = 0.3;','```','',string.rep('long line ',30)})
vim.bo.filetype='markdown'
local trees=vim.treesitter.get_parser(0,'markdown'):parse()
assert(trees[1]:root():type()=='document','Markdown parser did not parse the report')
vim.bo.modifiable=false
require('render-markdown').enable()
local count=0
assert(vim.wait(2000,function()
  count=0
  for name,namespace in pairs(vim.api.nvim_get_namespaces()) do
    if name:lower():find('render',1,true) then count=count+#vim.api.nvim_buf_get_extmarks(0,namespace,0,-1,{}) end
  end
  return count>0
end,20),'renderer produced no decorations')
local info={passed=true,decorations=count,nvim=vim.version().major..'.'..vim.version().minor..'.'..vim.version().patch,
  snacks=debug.getinfo(image.supports_file,'S').source:sub(2),markdown=debug.getinfo(require('render-markdown').setup,'S').source:sub(2)}
vim.fn.writefile({vim.json.encode(info)},%s)
vim.cmd('qa!')
"""%json.dumps(str(report)))
        child=subprocess.run([str(binary),'--headless','-c','luafile '+str(script)],env=environment(root,root,False),capture_output=True,timeout=8)
        (output/'stdout').write_bytes(child.stdout);(output/'stderr').write_bytes(child.stderr)
        assert child.returncode==0,child.stderr.decode(errors='replace')
        result=json.loads(report.read_text())
        source=Path(result['snacks'])
        # This hash is the actual installed pinned source, not a plugin version
        # string retained by a package override's derivation name.
        result['snacks_source_sha256']=hashlib.sha256(source.read_bytes()).hexdigest()
        result['markdown_source_sha256']=hashlib.sha256(Path(result['markdown']).read_bytes()).hexdigest()
        assert result['snacks_source_sha256']=='65a77640d3c42718af92a7b694659dc8004c50f662114112d23181f97b12fd4b'
        assert result['markdown_source_sha256']=='82824ad5a3ff2874391802ec1e1e5e1b5a9bdc7a09afe66ad2344a139139b22e'
    result['cases']=['markdown-and-inline-parsers','readonly-report-headings-tables-fences-decorated','doc-images-and-math-disabled','PNG-only-no-force']
    (output/'report.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--nvim',required=True,type=Path);parser.add_argument('--output',required=True,type=Path)
    args=parser.parse_args();run(args.nvim.resolve(),args.output)
