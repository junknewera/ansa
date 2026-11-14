"""Owned native M8 latency/SSE probes with retained observation clocks."""
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import subprocess
import tempfile
import time
import frontend_io
import protocol
import run as common
import session_cases as journal
import perf_measure

COUNTS=(0,100,4096)
SAMPLES=20
FIELDS={'turn':('submit_preview_seconds','terminal_durable_done_seconds','fsync_seconds','rss_bytes'),
        'cancel':('cancel_ack_seconds','process_cleanup_seconds','fsync_seconds','rss_bytes')}
def need(value,message):
    if not value:raise ValueError(message)
def stats(values):return perf_measure.stats([{'value':value} for value in values],'value')
def artifact(path):return {'path':path.name,'sha256':common.sha(path)}
class ObservedPty(frontend_io.Pty):
    def __init__(self,*args,**kwargs):
        self.trace={'start_ns':time.perf_counter_ns(),'reads':[],'sends':[],'signals':[],'rss_bytes':[]}
        super().__init__(*args,**kwargs)
        self.trace.update(popen_pid=self.child.pid,parent_pid=os.getpid(),popen_identity=list(perf_measure.identity(self.child.pid)),argv=self.child.args)
    def drain(self,timeout=.01):
        start=len(self.raw);super().drain(timeout)
        if len(self.raw)>start:self.trace['reads'].append({'begin':start,'end':len(self.raw),'at_ns':time.perf_counter_ns()})
    def send(self,data):
        if isinstance(data,str):data=data.encode()
        self.trace['sends'].append({'bytes_hex':data.hex(),'offset':len(self.raw),'at_ns':time.perf_counter_ns()})
        super().send(data)
    def signal(self,number):
        self.trace['signals'].append({'number':number,'offset':len(self.raw),'at_ns':time.perf_counter_ns()});self.child.send_signal(number)
    def memory(self):
        need(list(perf_measure.identity(self.child.pid))==self.trace['popen_identity'],'PTY identity changed')
        lines=Path(f'/proc/{self.child.pid}/status').read_text().splitlines()
        value=int(next(line.split()[1] for line in lines if line.startswith('VmHWM:')))*1024
        need(value>0,'missing native RSS');self.trace['rss_bytes'].append(value)
    def finish(self,*args,**kwargs):
        result=super().finish(*args,**kwargs);self.trace.update(exit_ns=time.perf_counter_ns(),exit_code=result['exit'],termios_restored=result['termios_restored'],flags_restored=result['flags_restored'])
        return result
def seen(trace,raw,text,offset=0):
    if isinstance(text,str):text=text.encode()
    index=raw.find(text,offset);need(index>=0,'missing measured PTY text')
    end=index+len(text)
    return next(row['at_ns'] for row in trace['reads'] if row['end']>=end)
def send_latency(screen,text):
    sent=screen.trace['sends'][-1];return (seen(screen.trace,screen.raw,text,sent['offset'])-sent['at_ns'])/1e9
def retain_observation(screen,row,directory,label):
    path=directory/(label+'.observation.json')
    common.write_json(path,{'trace':screen.trace,'process':{key:row[key] for key in ('ancestry','primary_bytes_hex','before_exited','after_exited','cleanup_ns','cancel_ns','ack_ns','popen_pid','parent_pid') if key in row}})
    row['observation']=artifact(path)
def owned_root():
    temporary=tempfile.TemporaryDirectory(prefix='ansa-m4-',dir='/tmp');root=Path(temporary.name)
    marker=root/'fixture-owner';marker.write_bytes(b'ansa-m4-synthetic-v1\n');marker.chmod(0o600)
    return temporary,root
def call(binary,row,directory,label,env=None,pass_fds=()):
    payload=(json.dumps(row,separators=(',',':'))+'\n').encode()
    start=time.perf_counter_ns()
    result=subprocess.run([str(binary)],input=payload,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=600,env=env,pass_fds=pass_fds)
    out=directory/(label+'.stdout');err=directory/(label+'.stderr');inp=directory/(label+'.stdin')
    out.write_bytes(result.stdout);err.write_bytes(result.stderr);inp.write_bytes(payload)
    command={'argv':[str(binary)],'stdin':artifact(inp),'stdout':artifact(out),'stderr':artifact(err),'exit_code':result.returncode,'elapsed_ns':time.perf_counter_ns()-start}
    if pass_fds:
        command['allocation_instrumentation']={'library':env['LD_PRELOAD'],'library_sha256':common.sha(Path(env['LD_PRELOAD'])),'output_fd':int(env['ANSA_ALLOCATION_FD']),'inherited_fds':list(pass_fds)}
    need(result.returncode==0 and not result.stderr,'metric adapter failed: '+label)
    response=json.loads(result.stdout);need(response['accepted'] is True,'metric adapter rejected')
    return response['value'],command
def seed(binary,root,count,directory,label):
    value,command=call(binary,{'root':str(root),'mode':'plan','turns':count,'start':0,'quiet':True,'compact':True},directory,label)
    need(value['generation_count']==2*count and value['summary_count']==max(0,(count-2)//2) and value['snapshot']['transcript_size']==2*count,'metric seed counts')
    parts={p.name:protocol.owned_bytes(p,64*1024*1024,single_link=True) for p in sorted((root/'journal').glob('part-*.log'))}
    need(parts,'metric seed empty')
    return parts,command
def pin_ancestors(primary,child):
    """Pin only the actual Popen descendant chain; these FDs are never signal targets."""
    result=[];fds=[];pid=primary
    try:
        while pid!=child.pid:
            need(type(pid) is int and pid>1 and pid!=os.getpid() and len(fds)<6,'metric process ancestry')
            fd=os.pidfd_open(pid);fds.append(fd)
            raw=Path(f'/proc/{pid}/stat').read_text();suffix=raw[raw.rfind(')')+2:].split();parent=int(suffix[1]);start=int(suffix[19])
            need(pid not in [r['pid'] for r in result],'metric process cycle')
            result.append({'pid':pid,'ppid':parent,'starttime':start,'proc_stat':raw,'pidfd_info':Path(f'/proc/self/fdinfo/{fd}').read_text()});pid=parent
        need(len(fds)>=2 and child.poll() is None,'metric process not owned')
        return fds,result
    except BaseException:
        for fd in fds:os.close(fd)
        raise
def exited(fd):
    poll=select.poll();poll.register(fd,select.POLLIN);return bool(poll.poll(0))
def footer(raw):
    matches=re.findall(rb'(?:\r?\n)M8_METRICS (\{[^\r\n]+\})',raw)
    need(len(matches)==1,'missing/duplicate native metric footer');value=json.loads(matches[0])
    need(set(value)=={'exit','metrics','generation_count','summary_count'} and value['exit']==0,'metric footer fields')
    need(set(value['metrics'])=={'fsync_ns','terminal_durable_ns'},'metric clock fields')
    return value
def decode(parts):
    # Decode/check every hash and chain link using the independent v1 reader.
    return journal.decode(parts)
def pipeline(adapters,directory,counts=COUNTS,samples=SAMPLES):
    runs=[];datasets=[]
    for count in counts:
        temporary,root=owned_root()
        try:
            # At the hard cap measure the 4096th Submit from its 4095-turn prefix.
            prefix=max(0,count-1);parts,command=seed(adapters['cpp'],root,prefix,directory,f'pipeline-seed-{count}')
            home=root/'home';cwd=home/'workspace';retained=directory/f'pipeline-seed-{count}';retained.mkdir()
            for name,data in parts.items():(retained/name).write_bytes(data)
            datasets.append({'submits':count,'prefix_submits':prefix,'root':str(root),'seed':command,'parts':{name:{'path':str((retained/name).relative_to(directory)),'sha256':hashlib.sha256(data).hexdigest()} for name,data in parts.items()}})
            prior=decode(parts)
            for index in range(samples):
                for kind in ('turn','cancel'):
                    for language,binary in adapters.items():
                        scope=f'metrics_{count}_{index}_{kind}_{language}';store=root/scope;work=cwd/scope
                        store.mkdir(mode=0o700);work.mkdir(mode=0o700);(work/'value.txt').write_bytes(b'SOURCE_VALUE\n');(work/'value.txt').chmod(0o600)
                        for name,data in parts.items():(store/name).write_bytes(data);(store/name).chmod(0o600)
                        row={'language':language,'submits':count,'sample':index,'kind':kind,'scope':scope,'root':str(root),'status':'FAIL'};runs.append(row);screen=None;fds=[]
                        try:
                            screen=ObservedPty(str(binary),root,home,cwd,'',True,argv=['--e2e-frontend',str(root),kind,scope])
                            screen.wait('готов',timeout=90)
                            # Reopened seeded journals retain plan mode. Change
                            # it explicitly before timing the same basic work.
                            if prefix:
                                at=len(screen.raw);screen.send('/basic\r');screen.wait('basic  готов',since=at,timeout=90)
                            else:need(b'basic  '+ 'готов'.encode() in screen.raw,'fresh metric mode')
                            at=len(screen.raw);begin=time.perf_counter_ns();screen.send(f'fixture{prefix}\r')
                            row['submit_ns']=begin
                            if kind=='turn':
                                screen.wait('E2E_PREVIEW',since=at,timeout=90);row['submit_preview_seconds']=send_latency(screen,'E2E_PREVIEW')
                                screen.wait('E2E_OK:exit7',since=at,timeout=90);screen.wait('готов',since=at,timeout=90)
                            else:
                                deadline=time.monotonic()+90
                                while not (work/'ready').is_file():
                                    need(time.monotonic()<deadline and screen.child.poll() is None,'metric process readiness');screen.drain(.001)
                                row['primary_bytes_hex']=(work/'primary').read_bytes().hex();primary=int(bytes.fromhex(row['primary_bytes_hex']));fds,ancestry=pin_ancestors(primary,screen.child)
                                row.update(popen_pid=screen.child.pid,parent_pid=os.getpid(),ancestry=ancestry,before_exited=[exited(fd) for fd in fds]);need(not any(row['before_exited']),'process exited before cancel')
                                at=len(screen.raw);screen.signal(signal.SIGINT);begin=screen.trace['signals'][-1]['at_ns']
                                ack=None;cleanup=None;deadline=time.monotonic()+5
                                while ack is None or cleanup is None:
                                    screen.drain(.001);now=time.perf_counter_ns()
                                    if ack is None and 'отменено'.encode() in screen.raw[at:]:ack=seen(screen.trace,screen.raw,'отменено',at)
                                    if cleanup is None and all(exited(fd) for fd in fds):cleanup=now
                                    need(time.monotonic()<deadline and screen.child.poll() is None,'metric cancel/cleanup deadline')
                                row.update(cancel_ns=begin,ack_ns=ack,cleanup_ns=cleanup,cancel_ack_seconds=(ack-begin)/1e9,process_cleanup_seconds=(cleanup-begin)/1e9,after_exited=[exited(fd) for fd in fds])
                            screen.memory();row['rss_bytes']=max(screen.trace['rss_bytes']);screen.send(b'\x04');result=screen.finish();need(result['exit']==0 and result['termios_restored'] and result['flags_restored'],'metric PTY restoration')
                            measured=footer(screen.raw);row['native']=measured;row['observations']=result['observations'];row.update(exit_code=0,termios_restored=True,flags_restored=True)
                            after={p.name:protocol.owned_bytes(p,64*1024*1024,single_link=True) for p in sorted(store.glob('part-*.log'))};deltas={}
                            for name,data in after.items():
                                base=parts.get(name,b'');need(data.startswith(base),'metric rewrote canonical journal');delta=directory/(scope+'-'+name+'.delta');delta.write_bytes(data[len(base):]);deltas[name]=dict(artifact(delta),prefix_bytes=len(base),after_bytes=len(data),after_sha256=hashlib.sha256(data).hexdigest())
                            need(set(parts)<=set(after),'metric lost journal part');row['journal_deltas']=deltas
                            new=decode(after)[len(prior):];row['event_kinds']=[e['kind'] for e in new]
                            need(sum(e['kind']=='accepted' for e in new)==1,'metric submit not durable')
                            outcomes=[e['result'] for e in new if e['kind']=='outcome'];process=[r for r in outcomes if r['name']=='run_command'] if outcomes and 'name' in outcomes[0] else [r for r in outcomes if 'exit_code' in json.loads(r['output'])]
                            need(len(process)==1,'metric missing process outcome');known=json.loads(process[0]['output']);need(known['cleanup_complete'] is True,'metric unknown cleanup')
                            need((work/'marker').read_bytes()==b'x','metric repeated/lost side effect')
                            if kind=='turn':
                                need(measured['generation_count']==4 and sum(e['kind']=='done' for e in new)==1 and known['exit_code']==7 and len(outcomes)==3,'metric known failure lost')
                                need(len(measured['metrics']['terminal_durable_ns'])==1,'metric missing terminal clock');row['terminal_durable_done_seconds']=measured['metrics']['terminal_durable_ns'][0]/1e9
                            else:need(measured['generation_count']==2 and not any(e['kind']=='done' for e in new) and sum(e['kind']=='cancel' for e in new)==1 and known['cancelled'] is True,'metric cancel dispatch')
                            fsync=measured['metrics']['fsync_ns'];need(len(fsync)==len(new) and all(type(ns) is int and ns>0 for ns in fsync),'metric fsync inventory');row['fsync_seconds']=stats([ns/1e9 for ns in fsync])['p95_nearest_rank']
                            row['marker_hex']='78';row['status']='PASS'
                        finally:
                            for fd in fds:os.close(fd)
                            if screen is not None:
                                raw=directory/(scope+'.pty');raw.write_bytes(screen.raw);row['raw']=artifact(raw);screen.close()
                                retain_observation(screen,row,directory,scope)
                            common.write_json(directory/'pipeline-progress.json',{'datasets':datasets,'runs':runs})
                        shutil.rmtree(store);shutil.rmtree(work)
            print('native pipeline dataset',count,'complete',flush=True)
        finally:temporary.cleanup()
    summary={}
    for language in adapters:
        for count in counts:
            for kind,fields in FIELDS.items():
                selected=[r for r in runs if r['language']==language and r['submits']==count and r['kind']==kind]
                summary[f'{language}-{count}-{kind}']={field:stats([r[field] for r in selected]) if field!='fsync_seconds' else stats([n/1e9 for r in selected for n in r['native']['metrics']['fsync_ns']]) for field in fields}
    return {'datasets':datasets,'runs':runs,'summary':summary,'method':'20 interleaved samples/client/dataset/kind; new owned scopes append once to a shared immutable prefix. 100/4096 measure the 100th/4096th Submit. Final terminal delivery paced800ms equally so the actual500ms transcript refresh exposes preview. Native monotonic clocks start AFTER terminal admission; fsync is before_sync..after_sync. Parent PTY and pinned descendant pidfds measure visible cancel and cleanup before harness intervention.'}
def wire():
    response={'id':'bench_response','model':'gpt-6.1-sol','status':'in_progress','output':[]}
    reasoning={'id':'bench_reasoning','type':'reasoning','status':'in_progress','summary':[],'encrypted_content':'x'*(256*1024)}
    message={'id':'bench_message','type':'message','status':'in_progress','role':'assistant','content':[]}
    done_reasoning=dict(reasoning,status='completed');done_message=dict(message,status='completed',content=[{'type':'output_text','text':'SSE_OK'}])
    events=[{'type':'response.created','response':response},
            {'type':'response.output_item.added','output_index':0,'item':reasoning},
            {'type':'response.output_item.done','output_index':0,'item':done_reasoning},
            {'type':'response.output_item.added','output_index':1,'item':message},
            {'type':'response.output_item.done','output_index':1,'item':done_message},
            {'type':'response.completed','response':dict(response,status='completed',output=[done_reasoning,done_message])}]
    return b''.join(b'data: '+json.dumps(dict(event,sequence_number=index),separators=(',',':')).encode()+b'\n\n' for index,event in enumerate(events))
def sse(adapters,counter,directory,samples=SAMPLES,iterations=50):
    temporary,root=owned_root();runs=[]
    try:
        data=wire();target=root/'wire.sse';target.write_bytes(data);target.chmod(0o600);retained=directory/'sse-wire.bin';retained.write_bytes(data)
        for index in range(samples):
            for language,binary in adapters.items():
                value,command=call(binary,{'op':'sse_bench','root':str(root),'iterations':iterations},directory,f'{language}-sse-{index:02}')
                validate_sse(value,data,iterations);runs.append({'language':language,'sample':index,'command':command,'seconds_per_mib':value['elapsed_ns']/1e9/(len(data)*iterations/1048576)})
        allocations={}
        for language,binary in adapters.items():
            target=directory/(language+'-allocations.json');fd=os.open(target,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
            try:
                env=dict(os.environ,LD_PRELOAD=str(counter),ANSA_ALLOCATION_FD=str(fd))
                value,command=call(binary,{'op':'sse_bench','root':str(root),'iterations':iterations},directory,language+'-sse-allocations',env=env,pass_fds=(fd,))
            finally:os.close(fd)
            validate_sse(value,data,iterations);counts=json.loads(target.read_bytes());need(set(counts)=={'allocations','requested_bytes'} and all(type(v) is int and v>iterations for v in counts.values()),'allocation counter missing')
            allocations[language]={'counts':counts,'raw':artifact(target),'command':command}
        return {'runs':runs,'summary':{language:stats([r['seconds_per_mib'] for r in runs if r['language']==language]) for language in adapters},'wire':artifact(retained),'allocations':allocations,'counter':{'path':str(counter),'sha256':common.sha(counter)},'method':'50 responses/sample,4096-byte chunks,256KiB encrypted reasoning,20 interleaved samples; native monotonic admission loop. Separate glibc allocation interposer runs excluded from timing; counts include process startup.'}
    finally:temporary.cleanup()
def validate_sse(value,data,iterations):
    need(set(value)=={'wire_sha256','bytes_per_response','iterations','admissions','tool_calls','elapsed_ns'} and value['wire_sha256']==hashlib.sha256(data).hexdigest() and value['bytes_per_response']==len(data) and value['iterations']==iterations and value['admissions']==iterations and value['tool_calls']==0 and type(value['elapsed_ns']) is int and value['elapsed_ns']>0,'SSE actual work differs')
