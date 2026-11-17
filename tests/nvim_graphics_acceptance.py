"""Real Kitty/Ghostty PNG lifecycle in an owned headless Weston display.

No connection to the user's desktop. Screenshots are compositor pixels, while
RPC commands control only the fixture editor's private socket.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
from PIL import Image


def wait(check, message, timeout=12):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.04)
    raise AssertionError(message)


def stop(child):
    if child and child.poll() is None:
        os.killpg(child.pid,signal.SIGTERM)
        try:child.wait(timeout=4)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid,signal.SIGKILL);child.wait(timeout=4)


def run(nvim,chart,output):
    output.mkdir(parents=True,exist_ok=False)
    observations=[]
    with tempfile.TemporaryDirectory(prefix='ansa-graphics-',dir='/tmp') as directory:
        root=Path(directory);runtime=root/'runtime';runtime.mkdir(mode=0o700)
        home=root/'home';home.mkdir()
        env=dict(os.environ,HOME=str(home),XDG_RUNTIME_DIR=str(runtime),XDG_CONFIG_HOME=str(root/'config'),
                 XDG_CACHE_HOME=str(root/'cache'),XDG_STATE_HOME=str(root/'state'),WAYLAND_DISPLAY='wayland-ansa',
                 LIBGL_ALWAYS_SOFTWARE='1',GDK_BACKEND='wayland',GSK_RENDERER='gl')
        for key in ['DISPLAY','TMUX','SSH_CONNECTION','SSH_TTY','SSH_CLIENT','NO_COLOR','ANSA_WORKSPACE_SOCKET','ANSA_WORKSPACE_EDITOR']:
            env.pop(key,None)
        bus_config=root/'bus.conf'
        bus_config.write_text('<busconfig><type>session</type><listen>unix:tmpdir='+str(runtime)+'</listen>'
            '<policy context="default"><allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/></policy></busconfig>')
        bus_log=open(output/'dbus.log','wb')
        bus=subprocess.Popen(['dbus-daemon','--nofork','--nopidfile','--print-address=1','--config-file='+str(bus_config)],
            stdout=subprocess.PIPE,stderr=bus_log,start_new_session=True)
        env['DBUS_SESSION_BUS_ADDRESS']=bus.stdout.readline().decode().strip()
        assert env['DBUS_SESSION_BUS_ADDRESS'].startswith('unix:'),'owned D-Bus startup'
        weston_log=open(output/'weston.log','wb')
        weston=subprocess.Popen(['weston','--backend=headless','--renderer=pixman','--width=1280','--height=800',
            '--fake-seat','--socket=wayland-ansa','--idle-time=0','--shell=kiosk-shell.so','--no-config','--debug'],
            env=env,stdout=weston_log,stderr=subprocess.STDOUT,start_new_session=True)
        try:
            wait(lambda:(runtime/'wayland-ansa').exists() or weston.poll() is not None,'Weston socket')
            assert weston.poll() is None,'Weston startup failed'
            for terminal in ['kitty','ghostty']:
                case=root/terminal;case.mkdir(mode=0o700);rpc=case/'nvim.sock';remote_socket=case/'kitty.sock'
                script=case/'init.lua'
                descriptor=dict(session_id='graphics_fixture',artifact_id='chart',revision=hashlib.sha256(chart.read_bytes()).hexdigest(),
                    mime='image/png',size_bytes=chart.stat().st_size,title='Monthly totals')
                script.write_text("""vim.o.laststatus=0;vim.o.showtabline=0;vim.o.number=false;vim.o.relativenumber=false
vim.o.signcolumn='no';vim.o.cmdheight=1
_G.fixture={descriptor=vim.json.decode(%s),path=%s}
fixture.text=vim.api.nvim_get_current_buf()
vim.api.nvim_buf_set_lines(fixture.text,0,-1,false,{'TEXT_ONLY_FIXTURE'})
vim.bo.modified=false
vim.defer_fn(function()
  fixture.image=require('ansa.images').open(fixture.descriptor,fixture.path,{focus=true})
end,300)
"""%(json.dumps(json.dumps(descriptor)),json.dumps(str(chart))))
                common=[str(nvim),'--listen',str(rpc),'-c','luafile '+str(script)]
                if terminal=='kitty':
                    command=['kitty','--config','NONE','--override','linux_display_server=wayland','--override','enable_audio_bell=no',
                        '--override','background=#181818','--override','foreground=#eeeeee','--override','enabled_layouts=splits',
                        '--override','allow_remote_control=socket-only','--listen-on','unix:'+str(remote_socket),*common]
                else:
                    command=['ghostty','--config-default-files=false','--gtk-single-instance=false','--window-decoration=false',
                        '--background=181818','--foreground=eeeeee','--font-size=12','-e',*common]
                log=open(output/(terminal+'.log'),'wb')
                # The editor wrapper must provide its own converter, including
                # a child launched by the terminal's native remote-control API.
                command[0]=shutil.which(command[0])
                child=subprocess.Popen(command,env=dict(env,PATH='/nonexistent'),stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
                def lua(source):
                    if ';' in source:source='(function() '+source+' end)()'
                    result=subprocess.run([str(nvim),'--clean','--server',str(rpc),'--remote-expr',
                        'luaeval('+json.dumps(source)+')'],env=env,capture_output=True,timeout=3)
                    assert result.returncode==0,(terminal,result.stderr.decode(errors='replace'))
                    return result.stdout.decode().strip()
                def screenshot(name, visible):
                    time.sleep(.25)
                    before=set(case.glob('*.png'))
                    result=subprocess.run(['weston-screenshooter'],cwd=case,env=env,capture_output=True,timeout=5)
                    assert result.returncode==0,(terminal,result.stderr.decode(errors='replace'))
                    paths=set(case.glob('*.png'))-before
                    assert len(paths)==1,(terminal,paths)
                    path=paths.pop();target=output/(terminal+'-'+name+'.png');shutil.move(path,target)
                    with Image.open(target) as image:
                        colors=image.convert('RGB').get_flattened_data()
                        count=sum(1 for r,g,b in colors if 50<=r<=110 and 135<=g<=175 and 90<=b<=135 and g>r+35 and g>b+20)
                        white=sum(1 for r,g,b in colors if min(r,g,b)>240)
                    # A 38-column split scales this thin line down to <300
                    # saturated pixels. Require both its specific green and
                    # the white plotting surface, and no chart pixels away.
                    assert (count>70 and white>5000) if visible else count==0,(terminal,name,count,white)
                    observations.append(dict(terminal=terminal,case=name,chart_pixels=count,screenshot=target.name))
                try:
                    wait(lambda:rpc.exists() or child.poll() is not None,terminal+' RPC socket')
                    assert child.poll() is None,terminal+' startup failed'
                    wait(lambda:lua("local ok,image=pcall(require,'ansa.images');return ok and image.state or 'loading'")=='png',terminal+' PNG negotiation')
                    wait(lambda:lua("vim.b[fixture.image].snacks_image ~= nil or vim.bo[fixture.image].filetype == 'image'")=='true',terminal+' image placement')
                    time.sleep(1)
                    screenshot('visible',True)
                    lua("vim.api.nvim_set_current_buf(fixture.text);return true")
                    screenshot('buffer-away',False)
                    lua("vim.api.nvim_set_current_buf(fixture.image);return true")
                    screenshot('buffer-return',True)
                    lua("vim.cmd('vsplit');vim.cmd('vertical resize 38');return true")
                    screenshot('resized-pane',True)
                    lua("vim.cmd('normal! 3'..string.char(5));return true")
                    screenshot('scrolled',True)
                    lua("vim.cmd('only');fixture.descriptor.revision=string.rep('b',64);require('ansa.images').open(fixture.descriptor,fixture.path,{buffer=fixture.image,focus=true});return true")
                    wait(lambda:lua("require('ansa.images').state")=='png',terminal+' revision negotiation')
                    screenshot('replaced-revision',True)
                    lua("require('ansa.images').clear();vim.api.nvim_set_current_buf(fixture.text);return true")
                    screenshot('session-cleanup',False)
                    lua("fixture.image=require('ansa.images').open(fixture.descriptor,fixture.path,{focus=true});return true")
                    wait(lambda:lua("require('ansa.images').state")=='png',terminal+' reopen negotiation')
                    screenshot('reopened',True)
                    lua("vim.api.nvim_buf_delete(fixture.image,{force=true});vim.api.nvim_set_current_buf(fixture.text);return true")
                    screenshot('buffer-close',False)
                    if terminal=='kitty':
                        source=Path(__file__).resolve().parents[1]/'integrations/ansa.nvim/launcher.py'
                        spec=importlib.util.spec_from_file_location('ansa_native_launcher',source)
                        launcher=importlib.util.module_from_spec(spec);spec.loader.exec_module(launcher)
                        address='unix:'+str(remote_socket)
                        parent=launcher.windows(address,'all')[0][1]['id']
                        relative=case/'relative note.md';relative.write_text('CALLER_RELATIVE_FILE\n')
                        child_rpc=case/'split.sock'
                        old_cwd=Path.cwd()
                        try:
                            os.chdir(case)
                            args=['--clean','--listen',str(child_rpc),'relative note.md']
                            opened=launcher.launch_kitty(case/'workspace.sock',str(nvim),args,False,address,parent)
                            existing=launcher.launch_kitty(case/'workspace.sock',str(nvim),args,False,address,parent)
                            assert opened['status']=='opened' and existing['status']=='existing' and existing['window_id']==opened['window_id']
                            wait(lambda:child_rpc.exists(),'Kitty adjacent editor socket')
                            def child_expr(expression):
                                result=subprocess.run([str(nvim),'--clean','--server',str(child_rpc),'--remote-expr',expression],env=env,capture_output=True,timeout=3)
                                assert result.returncode==0,result.stderr.decode(errors='replace')
                                return result.stdout.decode().strip()
                            wait(lambda:child_expr('getline(1)')=='CALLER_RELATIVE_FILE','Kitty relative caller file')
                            assert child_expr('getcwd()')==str(case)
                            child_expr("luaeval(\"vim.schedule(function() vim.cmd('qa!') end)\")")
                            wait(lambda:not child_rpc.exists(),'Kitty adjacent editor close')
                            closed=launcher.launch_kitty(case/'workspace.sock',str(nvim),args,False,address,parent)
                            assert closed['status']=='closed',closed
                            observations.append(dict(terminal=terminal,case='native-launch-idempotent-caller-cwd-and-known-close',opened=opened,existing=existing,closed=closed))
                        finally:os.chdir(old_cwd)
                    lua("vim.schedule(function() vim.cmd('qa!') end);return true")
                    child.wait(timeout=5)
                finally:
                    if child.poll() is None and rpc.exists():
                        try:lua("vim.schedule(function() vim.cmd('qa!') end);return true")
                        except (AssertionError,subprocess.TimeoutExpired):pass
                    stop(child);log.close()
        finally:stop(weston);weston_log.close();stop(bus);bus_log.close()
    result=dict(passed=len(observations),cases=observations,environment='owned Weston headless pixman + software GL',
                versions={tool:subprocess.run([tool,'--version'],capture_output=True,text=True).stdout.splitlines()[0] for tool in ['kitty','ghostty','weston']})
    (output/'report.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--nvim',required=True,type=Path);parser.add_argument('--chart',required=True,type=Path);parser.add_argument('--output',required=True,type=Path)
    args=parser.parse_args();run(args.nvim.absolute(),args.chart.resolve(),args.output)
