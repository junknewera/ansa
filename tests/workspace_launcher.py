"""Launcher ownership/unknown-ACK regressions over a controlled Kitty adapter."""
import argparse
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import socket
import tempfile


def run(output):
    output.mkdir(parents=True,exist_ok=False)
    source=Path(__file__).resolve().parents[1]/'integrations/ansa.nvim/launcher.py'
    spec=importlib.util.spec_from_file_location('ansa_launcher',source);module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    cases=[]
    with tempfile.TemporaryDirectory(prefix='ansa-launcher-',dir='/tmp') as directory:
        root=Path(directory);endpoint=root/'kitty.sock';sock=socket.socket(socket.AF_UNIX);sock.bind(str(endpoint));endpoint.chmod(0o600)
        workspace=root/'workspace';workspace.mkdir(mode=0o700);bridge=workspace/'workspace.sock'
        live={};commands=[];lose_ack=False;duplicate=False
        def remote(address,*args):
            commands.append(args)
            if args[0]=='ls':
                match=args[2]
                if match=='id:1':windows=[dict(id=1,user_vars={})]
                else:
                    tag=match.split('=',1)[1].split(' or ')[0]
                    windows=[dict(id=id,user_vars=dict(ansa_workspace=value)) for id,value in live.items() if value==tag]
                    if duplicate and windows:windows.append(dict(windows[0],id=999))
                    windows.append(dict(id=1,user_vars={}))
                return json.dumps([dict(tabs=[dict(id=2,layout='splits',windows=windows)])]).encode()
            assert args[0]=='launch',args
            assert '--cwd='+os.getcwd() in args and args[-1]=='file with spaces.md',args
            tag=next(value.split('=',2)[2] for value in args if value.startswith('--var=ansa_workspace='))
            id=len(commands)+10;live[id]=tag
            if lose_ack:raise ValueError('fixture lost launch ACK')
            return str(id).encode()
        module.remote=remote
        def launch(reopen=False):return module.launch_kitty(bridge,'/pinned/nvim',['file with spaces.md'],False,'unix:'+str(endpoint),1,reopen)
        lose_ack=True
        try:launch();raise AssertionError('lost acknowledgement must remain unknown')
        except ValueError as error:assert 'fixture lost' in str(error)
        state=next((workspace/'kitty-viewers').glob('*.json'))
        assert json.loads(state.read_text())['status']=='pending'
        before=sum(value[0]=='launch' for value in commands)
        found=launch();assert found['status']=='existing'
        assert sum(value[0]=='launch' for value in commands)==before
        cases.append('lost-ACK-reconciles-tag-without-relaunch')
        saved=json.loads(state.read_text());saved['status']='pending';state.write_text(json.dumps(saved));live.clear()
        for reopen in [False,True]:
            try:launch(reopen);raise AssertionError('unknown result must not be retried')
            except ValueError as error:assert 'неизвестен' in str(error)
        assert sum(value[0]=='launch' for value in commands)==before
        cases.append('unknown-and-not-found-never-launches-again')
        saved['status']='opened';state.write_text(json.dumps(saved));assert launch()['status']=='closed'
        lose_ack=False;assert launch(True)['status']=='opened';assert len(live)==1
        cases.append('known-closed-requires-explicit-reopen')
        duplicate=True
        try:launch();raise AssertionError('duplicate ownership must fail')
        except ValueError as error:assert 'несколько' in str(error)
        duplicate=False;cases.append('ambiguous-tag-is-not-guessed')
        lock_path=next((workspace/'kitty-viewers').glob('*.lock'))
        with lock_path.open('r+') as locked:
            fcntl.flock(locked,fcntl.LOCK_EX|fcntl.LOCK_NB)
            try:launch();raise AssertionError('concurrent launch must not proceed')
            except ValueError as error:assert 'уже выполняется' in str(error)
        cases.append('concurrent-opener-does-not-race')
        outside=root/'untouched';outside.write_text('KEEP');state.unlink();state.symlink_to(outside)
        try:launch();raise AssertionError('state symlink accepted')
        except OSError:pass
        assert outside.read_text()=='KEEP';cases.append('state-symlink-rejected')
        sock.close()
    result=dict(passed=len(cases),cases=cases)
    (output/'report.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--output',required=True,type=Path);run(parser.parse_args().output)
