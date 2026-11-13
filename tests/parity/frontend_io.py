"""Isolated owned CLI/PTY controller. stdout is a retained raw observation."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import stat
import subprocess
import sys
import termios
import time

MAX=32*1024*1024
def need(value,message):
    if not value:raise AssertionError(message)
def fixture(root):
    root=Path(root);meta=root.lstat()
    need(root.parent==Path('/tmp') and root.name.startswith('ansa-m4-') and stat.S_ISDIR(meta.st_mode) and meta.st_uid==os.geteuid() and meta.st_mode&0o077==0,'fixture root')
    directory=os.open(root,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
    try:
        fd=os.open('fixture-owner',os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC,dir_fd=directory)
        with os.fdopen(fd,'rb') as owner:
            meta=os.fstat(owner.fileno());need(stat.S_ISREG(meta.st_mode) and meta.st_nlink==1 and meta.st_uid==os.geteuid() and meta.st_mode&0o077==0 and meta.st_size==21,'fixture marker metadata');need(owner.read(22)==b'ansa-m4-synthetic-v1\n','fixture marker')
    finally:os.close(directory)
    home=root/'home';cwd=home/'workspace';cwd.mkdir(parents=True,exist_ok=True)
    return root,home,cwd
def environment(home,root,plain=False):
    env=dict(os.environ,HOME=str(home),XDG_STATE_HOME=str(root/'xdg'),TERM='xterm',LC_ALL='C')
    env.pop('LOCALE_ARCHIVE',None)
    if plain:env['NO_COLOR']='1'
    else:env.pop('NO_COLOR',None)
    return env
def journals(root):
    result={}
    locations=[root/'journal',root/'jsonl']
    catalog=root/'home/.ansa/sessions'
    if catalog.is_dir():locations.extend(p for p in catalog.iterdir() if p.is_dir() and not p.is_symlink())
    for directory in locations:
        if not directory.is_dir():continue
        for path in directory.glob('part-*.log'):
            meta=path.lstat();need(stat.S_ISREG(meta.st_mode) and meta.st_nlink==1 and meta.st_size<=MAX,'journal artifact')
            result[str(path.relative_to(root))]=path.read_bytes().hex()
    return result
def wait_file(path,minimum=1,timeout=3):
    until=time.monotonic()+timeout
    while not path.is_file() or path.stat().st_size<minimum:
        need(time.monotonic()<until,'fixture file readiness');time.sleep(.005)
def stop(child):
    if child.poll() is None:
        try:os.killpg(child.pid,signal.SIGKILL)
        except ProcessLookupError:pass
        child.wait(timeout=3)
def cli(binary,row,root,home,cwd):
    runs=[];session=None
    for specification in row['commands']:
        args=specification['args'] if isinstance(specification,dict) else specification
        selected=row['binaries'][specification['adapter']] if isinstance(specification,dict) else binary
        replace={'{ROOT}':str(root),'{HOME}':str(home),'{CWD}':str(cwd),'{SESSION}':session or ''}
        argv=[]
        for arg in args:
            if isinstance(arg,dict):argv.append(bytes.fromhex(arg['hex']))
            else:
                for key,value in replace.items():arg=arg.replace(key,value)
                argv.append(arg)
        child=subprocess.Popen([selected,*argv],cwd=cwd,env=environment(home,root),stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
        begin=time.monotonic()
        try:out,err=child.communicate(timeout=5)
        finally:stop(child)
        need(len(out)+len(err)<=MAX,'CLI output bound')
        found=re.search(rb'ansa session ([a-zA-Z0-9_-]+) ',err)
        if found:session=found[1].decode()
        runs.append({'exit':child.returncode,'stdout_hex':out.hex(),'stderr_hex':err.hex(),'elapsed':time.monotonic()-begin,'session':session,'journals':journals(root),'binary_sha256':hashlib.sha256(Path(selected).read_bytes()).hexdigest()})
    return {'runs':runs}

class Lines:
    def __init__(self,child):
        self.child=child;self.out=b'';self.err=b'';self.buffer=b'';self.events=[];self.prefix=None;self.resynced=set()
        for pipe in (child.stdout,child.stderr):os.set_blocking(pipe.fileno(),False)
    def drain(self,timeout=.01):
        pipes=[p for p in (self.child.stdout,self.child.stderr) if p and not p.closed]
        for pipe in select.select(pipes,[],[],timeout)[0]:
            try:data=os.read(pipe.fileno(),65536)
            except BlockingIOError:continue
            if not data:continue
            if pipe is self.child.stdout:
                self.out+=data;self.buffer+=data
                while b'\n' in self.buffer:
                    line,self.buffer=self.buffer.split(b'\n',1)
                    if line:
                        event=json.loads(line);self.events.append(event)
                        if event['type']=='resync_required' and self.prefix and event['stream_sequence'] not in self.resynced:
                            self.resynced.add(event['stream_sequence']);self.send(dict(self.prefix,turn=event['turn'],command_id='resync_'+str(len(self.resynced)),type='snapshot',after_sequence=0))
            else:self.err+=data
        need(len(self.out)+len(self.err)<=MAX,'line output bound')
    def until(self,predicate,timeout=4):
        until=time.monotonic()+timeout
        while not predicate(self.events):
            need(time.monotonic()<until and self.child.poll() is None,'event readiness')
            self.drain()
        return self.events[-1]
    def send(self,message):self.child.stdin.write(json.dumps(message,separators=(',',':')).encode()+b'\n');self.child.stdin.flush()
    def finish(self):
        if not self.child.stdin.closed:self.child.stdin.close()
        until=time.monotonic()+5
        while self.child.poll() is None:need(time.monotonic()<until,'line child shutdown');self.drain()
        for _ in range(3):self.drain(0)
        return {'exit':self.child.returncode,'stdout_hex':self.out.hex(),'stderr_hex':self.err.hex(),'events':self.events}
def jsonl(binary,row,root,home,cwd):
    argv=[binary,'--frontend-fixture',str(root),row['fixture_mode']] if row.get('fixture_mode') else [binary,'--offline-jsonl','--session',str(root/'jsonl')]
    child=subprocess.Popen(argv,cwd=cwd,env=environment(home,root),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    lines=Lines(child)
    try:
        lines.until(lambda es:bool(es))
        first=lines.events[0];prefix={k:first[k] for k in ['schema_version','session','incarnation']};lines.prefix=prefix
        make=lambda kind,id,**kw:dict(prefix,turn='turn',command_id=id,type=kind,**kw)
        mode=row.get('case','complete')
        if mode=='errors':
            bad=[dict(make('snapshot','bad',after_sequence=0),session='other'),make('cancel','bad-target',target=''),make('submit','bad-mode',text='question',mode='bad'),make('snapshot','',after_sequence=0),dict(make('snapshot','bad-bool',after_sequence=True)),make('transcript','bad-direction',before=0,limit=1,forward=1)]
            for value in bad:lines.send(value)
        else:
            lines.send(make('submit','submit',text='question',mode=row.get('mode','basic')))
            lines.until(lambda es:any(e['type']=='command_accepted' for e in es))
            accepted=next(e for e in lines.events if e['type']=='command_accepted')
            if mode=='cancel':lines.send(make('cancel','cancel',target=accepted['request_id']));lines.until(lambda es:any(e['type']=='cancelled' for e in es))
            elif mode=='signal':child.send_signal(signal.SIGINT)
            else:
                lines.until(lambda es:any(e['type']=='done' or e['type']=='snapshot' and e['state']=='completed' and e['durable'] is True for e in es),timeout=10 if row.get('fixture_mode') else 4)
                if mode=='duplicate':lines.send(make('submit','submit',text='question',mode=row.get('mode','basic')));lines.until(lambda es:any(e['type']=='command_replayed' for e in es))
                lines.send(make('transcript','page',before=0,limit=16,forward=False));lines.until(lambda es:any(e['type']=='transcript' for e in es))
                lines.send(make('snapshot','snapshot',after_sequence=0))
        result=lines.finish();result['journals']=journals(root);return result
    finally:stop(child)

class Pty:
    def __init__(self,binary,root,home,cwd,mode,plain=False,size=(80,24),argv=None,extra_env=None):
        self.master,self.slave=pty.openpty();self.original=termios.tcgetattr(self.slave);self.flags=fcntl.fcntl(self.slave,fcntl.F_GETFL);self.raw=b'';self.observations=[]
        self.resize(*size,notify=False);os.set_blocking(self.master,False)
        # This driver has no other threads. The child acquires this newly owned
        # PTY as its controlling terminal after Popen's setsid, before exec.
        env=environment(home,root,plain)
        if extra_env:env.update(extra_env)
        self.child=subprocess.Popen([binary,*(argv if argv is not None else ['--frontend-fixture',str(root),mode])],stdin=self.slave,stdout=self.slave,stderr=self.slave,cwd=cwd,env=env,start_new_session=True,preexec_fn=lambda:fcntl.ioctl(0,termios.TIOCSCTTY,0))
    def drain(self,timeout=.01):
        if select.select([self.master],[],[],timeout)[0]:
            while True:
                try:data=os.read(self.master,65536)
                except (BlockingIOError,OSError):break
                if not data:break
                self.raw+=data;need(len(self.raw)<=MAX,'PTY output bound')
    def wait(self,text,since=0,timeout=4):
        text=text.encode() if isinstance(text,str) else text;until=time.monotonic()+timeout
        while text not in self.raw[since:]:
            need(time.monotonic()<until and self.child.poll() is None,'PTY expected '+repr(text));self.drain()
        self.observations.append({'text_hex':text.hex(),'since':since,'through':len(self.raw)})
    def send(self,data):
        data=data.encode() if isinstance(data,str) else data;until=time.monotonic()+5
        while data:
            try:count=os.write(self.master,data);data=data[count:]
            except BlockingIOError:pass
            need(time.monotonic()<until,'PTY input deadline');self.drain(.001)
    def resize(self,cols,rows,notify=True):
        import struct
        fcntl.ioctl(self.slave,termios.TIOCSWINSZ,struct.pack('HHHH',rows,cols,0,0))
        if notify:self.child.send_signal(signal.SIGWINCH)
    def finish(self,signal_number=None):
        if signal_number:self.child.send_signal(signal_number)
        until=time.monotonic()+4
        while self.child.poll() is None:need(time.monotonic()<until,'PTY shutdown deadline');self.drain()
        self.drain(0);restored=termios.tcgetattr(self.slave);flags=fcntl.fcntl(self.slave,fcntl.F_GETFL)
        return {'exit':self.child.returncode,'stdout_hex':self.raw.hex(),'observations':self.observations,'termios_restored':restored==self.original,'flags_restored':flags==self.flags}
    def close(self):stop(self.child);os.close(self.master);os.close(self.slave)
def terminal(binary,row,root,home,cwd):
    case=row['case'];mode='terminal' if case=='terminal' else 'limit' if case=='reject' else 'preview-invalid' if case=='preview-invalid' else 'cancel'
    screen=Pty(binary,root,home,cwd,mode,row.get('plain',False));marks={}
    try:
        screen.wait('basic')
        if case=='terminal':
            need(not (root/'generated').exists(),'generation before first history frame')
            need(screen.raw.find(b'> EARLIER_CONTEXT')<screen.raw.rfind(b'> '),'history before first input')
            marks['first']=screen.raw.hex();screen.send(b'\t');screen.wait('plan');screen.send(b'\x1b[Z');screen.wait('basic',since=len(bytes.fromhex(marks['first'])))
            screen.send(b'\x06\x1b[5~\x1b[6~');screen.send('привет');screen.wait('привет');screen.resize(20,10);screen.drain(.03);screen.resize(100,32);screen.wait('привет');screen.send(b'\r');screen.wait('draft');screen.wait('assistant manager');screen.send('next\r');screen.wait('busy');screen.wait('next');screen.child.send_signal(signal.SIGINT);screen.wait('отменено');screen.send(b'\x7f'*4+b'\x04')
        elif case=='repeat-cancel':
            for index in range(2):
                at=len(screen.raw);screen.send(f'question{index}\r');until=time.monotonic()+3
                # Cancel acknowledgement may precede joined cleanup. A busy
                # rejection must retain the draft for a subsequent Enter.
                while not (root/'generated').is_file() or (root/'generated').stat().st_size<index+1:
                    need(time.monotonic()<until and screen.child.poll() is None,'next turn after cleanup');screen.drain();screen.send(b'\r')
                cancel_at=len(screen.raw);screen.child.send_signal(signal.SIGINT);until=time.monotonic()+3;target='plan' if index==0 else 'basic';expected=(target+'  отменено').encode()
                while expected not in screen.raw[cancel_at:]:
                    need(time.monotonic()<until and screen.child.poll() is None,'cancel kept frontend alive');screen.send('/'+target+'\r');screen.drain(.01)
                screen.wait(expected,since=cancel_at);need(screen.child.poll() is None,'second cancel exited CLI')
            screen.send(b'\x04')
        elif case=='reject':
            screen.send('first\r');screen.wait('done');screen.wait('готов');at=len(screen.raw);screen.send('second\rAFTER');screen.wait('submit_id_limit',since=at);screen.wait('secondAFTER',since=at);marks['rejected']=screen.raw.hex();screen.send(b'\x7f'*11+b'/exit\r')
        elif case=='preview-invalid':
            screen.send('question\r');screen.wait('draft');screen.wait('незавершённый ответ');screen.wait('fixture_invalid_response');screen.send(b'\x04')
        elif case=='overflow':
            screen.send(b'x'*(32*1024+100)+b'\x7f'*(16*1024)+b'\r');screen.wait('submit_size');screen.send(b'\xc0\xaf');screen.wait('??');screen.send(b'\x7f'*2);screen.send('é😀');screen.wait('é😀');screen.send(b'\x7f'*3+b'\x04')
        elif case=='job-control':
            screen.child.send_signal(signal.SIGTSTP);until=time.monotonic()+2;stopped=False
            while time.monotonic()<until:
                pid,status=os.waitpid(screen.child.pid,os.WNOHANG|os.WUNTRACED)
                if pid and os.WIFSTOPPED(status):stopped=True;break
                screen.drain()
            need(stopped,'job-control did not stop');need(termios.tcgetattr(screen.slave)==screen.original,'termios while suspended');marks['stopped']=True;screen.child.send_signal(signal.SIGCONT);screen.drain(.03);screen.send(b'\x04')
        else:raise ValueError('unknown terminal case')
        result=screen.finish();result.update(marks=marks,journals=journals(root));return result
    finally:screen.close()
def fault(binary,row,root,home,cwd):
    case=row['case'];fixture_binary=row['fixture_binary']
    if case in ('pending','pending-signal'):
        child=subprocess.Popen([fixture_binary,'--frontend-fixture',str(root),'pending'],cwd=cwd,env=environment(home,root),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
        try:
            child.stdin.write(b'task\n'+(b'/cancel\n' if case=='pending' else b''));child.stdin.flush();wait_file(root/'accepted-blocked');need(not (root/'generated').exists(),'dispatch before accepted fsync');prefix=b''
            if case=='pending-signal':
                child.send_signal(signal.SIGINT)
                for count in (1,2):
                    child.stdin.write(b'/status\n');child.stdin.flush();until=time.monotonic()+2
                    while prefix.count(b'ansa: ')<count:
                        need(time.monotonic()<until and child.poll() is None,'pending signal/control handshake')
                        if select.select([child.stderr],[],[],.01)[0]:prefix+=os.read(child.stderr.fileno(),4096)
            (root/'release').write_bytes(b'x');child.stdin.close();child.wait(timeout=4)
            return {'exit':child.returncode,'generated':(root/'generated').read_bytes().hex() if (root/'generated').exists() else '', 'stdout_hex':child.stdout.read().hex(),'stderr_hex':(prefix+child.stderr.read()).hex(),'journals':journals(root)}
        finally:stop(child)
    if case=='read-error':
        fd=os.open('/dev/null',os.O_WRONLY);original=fcntl.fcntl(fd,fcntl.F_GETFL)
        child=subprocess.Popen([fixture_binary,'--frontend-fixture',str(root),'cancel'],cwd=cwd,env=environment(home,root),stdin=fd,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
        try:
            out,err=child.communicate(timeout=3);return {'exit':child.returncode,'stdout_hex':out.hex(),'stderr_hex':err.hex(),'flags_restored':fcntl.fcntl(fd,fcntl.F_GETFL)==original,'journals':journals(root)}
        finally:stop(child);os.close(fd)
    if case=='stderr':
        child=subprocess.Popen([binary,'--offline'],cwd=cwd,env=environment(home,root),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
        try:
            fcntl.fcntl(child.stderr,fcntl.F_SETPIPE_SZ,4096);child.stdin.write(b'/status\n'*10000+b'/exit\n');child.stdin.close();child.wait(timeout=4)
            return {'exit':child.returncode,'stdout_hex':child.stdout.read().hex(),'stderr_hex':child.stderr.read().hex()}
        finally:stop(child)
    child=subprocess.Popen([fixture_binary,'--frontend-fixture',str(root),'backpressure'],cwd=cwd,env=environment(home,root),stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    begin=time.monotonic()
    try:
        fcntl.fcntl(child.stdout,fcntl.F_SETPIPE_SZ,4096)
        if case=='epipe':child.stdout.close()
        elif case=='blocked-cancel':
            until=time.monotonic()+3
            while True:
                import array
                count=array.array('i',[0]);fcntl.ioctl(child.stdout,termios.FIONREAD,count,True)
                if count[0]>0:break
                need(time.monotonic()<until and child.poll() is None,'blocked writer readiness');time.sleep(.005)
            child.send_signal(signal.SIGINT)
        elif case!='blocked-shutdown':raise ValueError('unknown pipe fault')
        child.wait(timeout=3)
        return {'exit':child.returncode,'elapsed':time.monotonic()-begin,'stdout_hex':child.stdout.read().hex() if not child.stdout.closed else '', 'stderr_hex':child.stderr.read().hex(),'journals':journals(root)}
    finally:stop(child)
def bridge(binary,row,root,home,cwd):
    import frontend_wire
    import protocol
    fixtures=Path(__file__).resolve().parent/'fixtures';body=(Path(__file__).resolve().parents[1]/'fixtures/round1.sse').read_bytes();ca=(fixtures/'transport/tls/ca.der').read_bytes();case=row['case']
    peer=None;server=None;observations=[]
    try:
        if case.startswith('h2-'):
            server=subprocess.Popen([binary,'--serve-h2',str(Path(__file__).resolve().parents[1]/'fixtures'),str(fixtures),case],stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
            need(select.select([server.stdout],[],[],3)[0],'H2 peer readiness');port=int(server.stdout.readline());url='https://localhost:'+str(port)+'/'
        else:
            peer=frontend_wire.Peer(case,body,fixtures,row.get('tls',False));url=peer.url()
        request={'root':str(root),'op':'bridge','url':url,'ca_hex':ca.hex(),'request':row['request']}
        raw=(protocol.exact_dumps(request)+'\n').encode();child=subprocess.Popen([binary],cwd=cwd,env=environment(home,root),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
        try:out,err=child.communicate(raw,timeout=5)
        finally:stop(child)
        if peer:peer.close();observations=peer.records
        if server:server_out,server_err=server.communicate(timeout=3);observations=[{'server_exit':server.returncode,'stdout_hex':server_out.hex(),'stderr_hex':server_err.hex()}]
        return {'exit':child.returncode,'stdout_hex':out.hex(),'stderr_hex':err.hex(),'url':url,'request_hex':raw.hex(),'observations':observations,'body_hex':body.hex()}
    finally:
        if peer and peer.thread.is_alive():peer.close()
        if server:stop(server)
def first_frame(binary,row,root,home,cwd):
    import socket
    private=home/'.ansa';private.mkdir(mode=0o700);store=private/'auth';store.mkdir(mode=0o700)
    identity={'version':1,'host_id':'synthetic-host','client_id':'synthetic-client','subject':'synthetic-subject'}
    credentials=dict(identity,scope='openid profile email offline_access resource.invoke chatgpt.tokens.use.direct',session='synthetic-session',expires_at=0,earliest_refresh_at=0,refresh_pending=False,access_token='synthetic-expired-access',refresh_token='synthetic-refresh',nonce='synthetic-nonce')
    for name,value in [('registration.json',identity),('credentials.json',credentials)]:
        path=store/name;path.write_text(json.dumps(value));path.chmod(0o600)
    listener=socket.socket();listener.bind(('127.0.0.1',0));listener.listen(2);listener.setblocking(False);proxy='http://127.0.0.1:'+str(listener.getsockname()[1]);env={key:proxy for key in ['http_proxy','https_proxy','HTTPS_PROXY','all_proxy','ALL_PROXY']};env.update(no_proxy='',NO_PROXY='')
    screen=None;connection=None
    try:
        screen=Pty(binary,root,home,cwd,'',argv=['--cwd',str(cwd)],extra_env=env);screen.wait('basic')
        need(not select.select([listener],[],[],0)[0],'network before actual first frame');first=screen.raw.hex();screen.send('question\r')
        need(select.select([listener],[],[],3)[0],'expired profile did not exercise lazy dispatch');connection,_=listener.accept();connection.settimeout(1);head=b''
        while b'\r\n\r\n' not in head:head+=connection.recv(4096);need(len(head)<16384,'proxy request bound')
        need(head.startswith(b'CONNECT auth.openai.com:443 '),'guarded origin');connection.sendall(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');connection.close();connection=None
        screen.wait('пауза');connections=1
        while select.select([listener],[],[],0)[0]:extra,_=listener.accept();extra.close();connections+=1
        screen.send(b'\x04');result=screen.finish();result.update(first_hex=first,proxy_request_line=head.split(b'\r\n',1)[0].decode(),connections_before_frame=0,connections_after_submit=connections,journals=journals(root));return result
    finally:
        if connection:connection.close()
        listener.close()
        if screen:screen.close()
def main():
    row=json.loads(sys.stdin.buffer.readline(128*1024+1));root,home,cwd=fixture(row['root']);binary=str(Path(row['binary']).resolve());kind=row['kind']
    begin=time.monotonic()
    if kind=='cli':value=cli(binary,row,root,home,cwd)
    elif kind=='jsonl':value=jsonl(binary,row,root,home,cwd)
    elif kind=='terminal':value=terminal(binary,row,root,home,cwd)
    elif kind=='fault':value=fault(binary,row,root,home,cwd)
    elif kind=='bridge':value=bridge(binary,row,root,home,cwd)
    elif kind=='first-frame':value=first_frame(binary,row,root,home,cwd)
    else:raise ValueError('unknown frontend driver')
    value.update(schema_version=1,kind=kind,case=row.get('case',''),elapsed_total=time.monotonic()-begin,binary_sha256=hashlib.sha256(Path(binary).read_bytes()).hexdigest())
    print(json.dumps(value,sort_keys=True))
if __name__=='__main__':main()
