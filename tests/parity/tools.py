"""M6 local tools/process parity; replay the retained raw commands and records."""
import copy
import hashlib
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import time
import select
import threading
import ctypes
import risk_process
from concurrent.futures import ThreadPoolExecutor
import auth
import protocol
import run as common
import runtime
import sessions
import session_cases as v

MANIFEST=common.ROOT/'tests/parity/fixtures/tools.json'
MAX=64*1024*1024
NATIVE=('config','instruction-links','files','raw-names','future-auth','basic','cancel','timeout','detached','output','privileges','evidence','runtime-nonzero','runtime-cancel')
CPP={'local':('config','files','catalog','instruction-links'),'process':('basic','output','cancel','timeout','privileges','crash','detached','evidence','signals','guardian-crash'),'loop':('roundtrip','known-failure','limit','crash','between-rounds','process-recovery')}
def check(value,code):v.check(value,code)
def paths(cpp,rust):
    cpp=(common.ROOT/cpp).resolve();rust=(common.ROOT/rust).resolve()
    return {'cpp':cpp/'ansa_tools_oracle','rust':rust/'debug/examples/tools_adapter','native-local':cpp/'ansa_local_tests','native-process':cpp/'ansa_process_tests','native-loop':cpp/'ansa_loop_tests'}
def identities(values):
    expected={'cpp':'ansa_tools_oracle','rust':'tools_adapter','native-local':'ansa_local_tests','native-process':'ansa_process_tests','native-loop':'ansa_loop_tests'}
    check(set(values)==set(expected),'tools binary inventory')
    result={}
    for key,path in values.items():
        check(path.name==expected[key],'tools binary name')
        result[key]={'path':str(path),'sha256':hashlib.sha256(protocol.owned_bytes(path,256*1024*1024,executable=True)).hexdigest()}
    return result
class Context(runtime.Context):
    def call(self,adapter,request,exit=0,fault=None,started=None):
        request=copy.deepcopy(request);request['root']=str(self.root)
        return sessions.Context.call(self,adapter,request,exit,fault,started)
    def setup(self,files):
        if not self.live:return
        for name,value in files.items():
            path=self.root/name;check(not path.is_absolute() or path.is_relative_to(self.root),'fixture path')
            check('..' not in Path(name).parts and not any(p=='.env' or p.startswith('.env.') for p in Path(name).parts),'unsafe fixture input')
            path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(value)
    def artifact(self,suffix,payload=None):
        label=self.row['id']+'-'+suffix+'.raw';check(suffix not in self.artifact_used,'duplicate artifact');self.artifact_used.add(suffix)
        if self.live:
            path=self.bundle.directory/label;path.write_bytes(payload);self.row.setdefault('artifacts',{})[suffix]=protocol.raw(path)
        return protocol.retained_bytes(self.bundle.directory,self.row['artifacts'][suffix],label,256*1024)
    def record(self,operation,name,index):
        path=self.root/'jobs'/hashlib.sha256(operation.encode()).hexdigest()/name
        raw=self.artifact(f'{operation}-{index}-{name}',protocol.owned_bytes(path,256*1024) if self.live else None)
        return protocol.strict_loads(raw)
def accepted(value):return sessions.accepted(value)
def outcome(value):
    value=accepted(value);check(set(value)=={'execution','effects','output'},'tools outcome shape')
    return value,protocol.strict_loads(value['output'])
def same(a,b):check(protocol.structural(a)==protocol.structural(b),'C++/Rust values differ')
def configuration(c,overlay=None,invalid=False):
    files={}
    if overlay:files=overlay
    c.setup(files);values=[c.call(adapter,{'op':'config'}) for adapter in ('cpp','rust')]
    if invalid:
        check(all(v['accepted'] is False for v in values),'invalid config accepted');return
    a,b=[accepted(v) for v in values];same(a,b)
    settings=a['settings'];check(settings['access_mode']=='full_access' and settings['allow_sudo'] is False,'access policy')
    check(settings['cwd']==str(c.root/'home/workspace'),'config workspace')
    names=[d['name'] for d in a['tools']['descriptors']];check(names==['list_directory','read_file','search_files','run_command','get_test_value'],'compiled tool inventory')
    check([d['name'] for d in a['readonly']['descriptors']]==['list_directory','read_file','search_files','get_test_value'],'plan permits writer')
    if overlay:
        check(settings['model']=='project' and settings['returned_model']=='project','model precedence')
        same(settings['tool_rounds'],12);same(settings['tool_calls'],24)
    else:
        expected={'model':'gpt-6.1-sol','returned_model':'gpt-6.1-sol','compact_bytes':262144,'tool_rounds':32,'tool_calls':64,'process_timeout_ms':60000,'process_output_bytes':16384,'exclude':['.git','node_modules','.cache','build','build-ansa','result']}
        for key,value in expected.items():same(settings[key],value)
    check(a['epoch']['id']=='ansa_local_v1' and a['epoch']['exact_state']['settings']==settings,'epoch/settings binding')
    token=v.digest(a['epoch']['exact_state']['instructions']);check('instruction_token is '+token in a['epoch']['instructions'],'initial instruction token')
def utf8(raw):
    out='';i=0
    while i<len(raw):
        lead=raw[i];size=1 if lead<128 else 2 if 194<=lead<=223 else 3 if 224<=lead<=239 else 4 if 240<=lead<=244 else 0
        try:piece=raw[i:i+size].decode('utf-8') if size and i+size<=len(raw) else None
        except UnicodeDecodeError:piece=None
        out+=piece if piece is not None else '\ufffd';i+=size if piece is not None else 1
    return out
def read_case(c,offset,limit,payload):
    c.setup({'home/workspace/value.txt':payload})
    args={'path':'value.txt','offset':offset,'max_bytes':limit}
    values=[outcome(c.call(adapter,{'op':'local','name':'read_file','arguments':args,'initial_token':True})) for adapter in ('cpp','rust')]
    same(values[0],values[1]);result,body=values[0];same(result['execution'],0);same(result['effects'],1)
    chunk=payload[offset:offset+limit]
    if b'\0' in chunk:check(body=={'error':'binary_file'},'binary file admitted');return
    expected={'path':str(c.root/'home/workspace/value.txt'),'offset':offset,'next_offset':offset+len(chunk),'content':utf8(chunk),'truncated':len(payload[offset:])>limit};same(body,expected)
def nested(c):
    c.setup({'home/.ansa/AGENTS.md':b'GLOBAL','home/workspace/AGENTS.md':b'PROJECT','home/workspace/nested/AGENTS.md':b'NESTED','home/workspace/nested/value':b'needle'})
    args={'path':'nested/value','offset':0,'max_bytes':8192};first=[]
    for adapter in ('cpp','rust'):
        result,body=outcome(c.call(adapter,{'op':'local','name':'read_file','arguments':args,'initial_token':True}));same(result['effects'],1)
        check(body['status']=='instructions_required' and body['executed'] is False,'nested access before acknowledgement')
        check(body['instructions']==[{'path':str(c.root/'home/workspace/nested/AGENTS.md'),'text':'NESTED'}],'nested instructions provenance');first.append(body)
    same(*first);args['instruction_token']=first[0]['instruction_token']
    for adapter in ('cpp','rust'):
        _,body=outcome(c.call(adapter,{'op':'local','name':'read_file','arguments':args}));check(body['content']=='needle','nested token not accepted')
def search(c,mode):
    c.setup({'home/workspace/value.txt':b'first\nneedle line\n','home/workspace/nested/AGENTS.md':b'nested rule','home/workspace/nested/hidden.txt':b'needle','home/workspace/node_modules/hidden':b'needle'})
    values=[outcome(c.call(adapter,{'op':'local','name':'search_files','initial_token':True,'arguments':{'path':'.','query':'needle' if mode=='content' else 'value','mode':mode,'max_results':10}})) for adapter in ('cpp','rust')];same(values[0],values[1]);_,body=values[0]
    check(len(body['matches'])==1 and body['matches'][0]['path']==str(c.root/'home/workspace/value.txt'),'search crossed excluded/instruction scope')
    check(body['instruction_scopes']==[str(c.root/'home/workspace/nested')],'nested scope inventory')
def process_specs():
    return [('basic',['sh','-c','printf hello; printf diagnostic >&2; printf x >> marker; exit 7'],7,0,'hello','diagnostic'),
            ('zero',['sh','-c','printf ok'],0,0,'ok',''),('nonzero',['sh','-c','exit 19'],19,0,'',''),
            ('signal',['sh','-c','kill -TERM $$'],-1,15,'',''),('missing',['/ansa-m6-no-executable'],127,0,'',''),('sudo',['sudo','unreachable'],126,0,None,None)]
def process(c,spec):
    name,argv,exit_code,signal_code,stdout,stderr=spec;values=[]
    for writer,other in [('cpp','rust'),('rust','cpp')]:
        invocation={'operation':writer,'call':'call','name':'execute','revision':'1','arguments':{}}
        request={'op':'process','argv':argv,'invocation':invocation}
        value,body=outcome(c.call(writer,request));same(value['execution'],0);same(value['effects'],1)
        if name=='sudo':check(body=={'error':'sudo_disabled','exit_code':protocol.NumberToken('126',True)},'sudo dispatched')
        else:
            expected={'exit_code':exit_code,'signal':signal_code,'cancelled':False,'timed_out':False,'truncated':False,'stdout':stdout,'stderr':stderr,'stdout_bytes':len(stdout.encode()),'stderr_bytes':len(stderr.encode()),'spawn_errno':2 if name=='missing' else 0,'cleanup_complete':True};same(body,expected)
        records=[c.record(writer,n,0) for n in ('request.json','started.json','result.json')]
        expected={'version':1,'invocation':invocation,'cwd':str(c.root/'home/workspace'),'argv':argv,'timeout_ms':3000,'output_bytes':16384};same(records[0],expected);digest=v.digest(records[0]);check(records[1]=={'version':protocol.NumberToken('1',True),'request_hash':digest},'durable consume identity');same(records[2],{'version':1,'request_hash':digest,**value})
        check(accepted(c.call(other,{'op':'recover','invocation':invocation}))==value,'cross-client process recovery')
        check(accepted(c.call(other,request))==value,'consumed process executed again')
        for index,n in enumerate(('request.json','started.json','result.json')):same(c.record(writer,n,1),records[index])
        if name=='basic':
            marker=c.artifact('marker-'+writer,(c.root/'home/workspace/marker').read_bytes() if c.live else None);check(marker==b'x'*(1 if writer=='cpp' else 2),'process repeated after recovery')
        values.append(value)
    same(values[0],values[1])
def inherited(c,writer):
    other='rust' if writer=='cpp' else 'cpp'
    invocation={'operation':writer,'call':'call','name':'execute','revision':'1','arguments':{}}
    value,body=outcome(c.call(writer,{'op':'process','argv':['sh','-c','printf x >> marker; exit 7'],'inherit_signals':True,'invocation':invocation}))
    check(runtime.integer(value['effects']) in (1,2),'inherited signal outcome')
    if runtime.integer(value['effects'])==1:same(body['exit_code'],7)
    _,recovered=outcome(c.call(other,{'op':'recover','invocation':invocation}));same(recovered['exit_code'],7);check(recovered['cleanup_complete'] is True,'inherited signal cleanup')
    check(c.artifact('signal-marker',(c.root/'home/workspace/marker').read_bytes() if c.live else None)==b'x','inherited signal repeated effect')
def invalid_process(c,options):
    request={'op':'process','argv':['sh','-c','printf x > unexpected'],**options}
    for adapter in ('cpp','rust'):check(c.call(adapter,request)['accepted'] is False,'invalid process spec reached launch')
    marker=b'present' if c.live and (c.root/'home/workspace/unexpected').exists() else b'absent'
    check(c.artifact('unexpected',marker if c.live else None)==b'absent','invalid process produced an effect')
def missing_cwd(c,writer):
    other='rust' if writer=='cpp' else 'cpp'
    request={'op':'process','argv':['sh','-c','printf x > unexpected'],'cwd':str(c.root/'nonexistent')}
    value=accepted(c.call(writer,request));same(value['effects'],2)
    same(accepted(c.call(other,request)),value)
    check(c.artifact('unexpected',b'present' if c.live and (c.root/'home/workspace/unexpected').exists() else b'absent')==b'absent','unavailable cwd launched process')
def partial(c,writer,which):
    other='rust' if writer=='cpp' else 'cpp';invocation={'operation':'partial','call':'call','name':'execute','revision':'1','arguments':{}}
    request={'op':'process','argv':['sh','-c','printf x >> marker; exit 7'],'invocation':invocation}
    value=accepted(c.call(writer,request));same(value['effects'],1)
    before=c.record('partial',which,0)
    job=c.root/'jobs'/hashlib.sha256(b'partial').hexdigest()
    if c.live:(job/which).rename(job/(which+'.pending'))
    for adapter in (writer,other):
        recovered=c.call(adapter,{'op':'recover','invocation':invocation})
        if which=='result.json':check(recovered=={'accepted':True,'value':None},'partial result was published')
        else:check(recovered['accepted'] is False,'partial identity admitted')
        rerun=c.call(adapter,request)
        if which=='result.json':same(accepted(rerun)['effects'],2)
        else:check(rerun['accepted'] is False,'partial identity replayed')
    check(c.artifact('partial-marker',(c.root/'home/workspace/marker').read_bytes() if c.live else None)==b'x','partial publication repeated command')
    raw=c.artifact('pending-record',protocol.owned_bytes(job/(which+'.pending'),256*1024) if c.live else None);same(protocol.strict_loads(raw),before)
def validate_topology(pids,parents,actual,parent_pid):
    check(set(pids)=={'primary','descendant','guardian','runtime'} and len(set(pids.values()))==4 and all(type(p) is int and 1<p<=2**31-1 for p in pids.values()),'retained process topology')
    check(set(parents)==set(pids) and all(type(p) is int and 1<p<=2**31-1 for p in parents.values()),'retained parent topology')
    check(type(actual) is int and type(parent_pid) is int and 1<parent_pid<=2**31-1 and pids['runtime']==actual and parents['runtime']==parent_pid,'runtime differs from owned Popen')
    check(parents['guardian']==pids['runtime'] and parents['primary']==pids['guardian'] and parents['descendant']==pids['primary'],'fixture ancestry differs')
def pin_topology(pids,actual,root,binary):
    candidates={};parents={}
    try:
        check(len(set(pids.values()))==4 and all(type(pid) is int and 1<pid<=2**31-1 and pid!=os.getpid() for pid in pids.values()),'unsafe fixture pid')
        for name,pid in pids.items():
            candidates[name]=os.pidfd_open(pid)
            with Path(f'/proc/{pid}/cmdline').open('rb') as stream:command=stream.read(16385)
            expected=str(binary) if name=='runtime' else str(root)
            check(len(command)<=16384 and expected.encode() in command,'fixture process identity')
            status=Path(f'/proc/{pid}/status').read_text()
            parents[name]=int(next(line.split()[1] for line in status.splitlines() if line.startswith('PPid:')))
        validate_topology(pids,parents,actual,os.getpid())
        check(Path(f'/proc/{actual}/exe').resolve()==binary and os.getpgid(actual)==actual,'fixture runtime executable/group')
        return candidates,parents
    except BaseException:
        # No candidate becomes signal-owned until the complete ancestry passes.
        for fd in candidates.values():os.close(fd)
        raise
def exited(fd):
    poll=select.poll();poll.register(fd,select.POLLIN);return bool(poll.poll(0))
def observed_exits(fds):
    deadline=time.monotonic()+3
    while True:
        result={name:exited(fds[name]) for name in ('primary','descendant','guardian')}
        if all(result.values()) or time.monotonic()>=deadline:return result
        time.sleep(.005)
def crash(c,writer,guardian):
    other='rust' if writer=='cpp' else 'cpp';invocation={'operation':'crash','call':'call','name':'execute','revision':'1','arguments':{}}
    cwd=c.root/'home/workspace';fixture=common.ROOT/'tests/parity/process_fixture.py'
    request={'op':'process','argv':[sys.executable,str(fixture),'tree',str(cwd)],'timeout_ms':30000,'invocation':invocation}
    fds={};owned={};ready=threading.Event();future=None;pool=None;result=None;topology=None
    def started(pid):
        owned.update(pid=pid,fd=os.pidfd_open(pid));ready.set()
    if c.live:
        libc=ctypes.CDLL(None,use_errno=True);libc.prctl.argtypes=[ctypes.c_int]+[ctypes.c_ulong]*4;libc.prctl.restype=ctypes.c_int
        check(libc.prctl(36,1,0,0,0)==0,'test controller subreaper unavailable')
    try:
        if c.live:
            pool=ThreadPoolExecutor(max_workers=1);future=pool.submit(c.call,writer,request,0 if guardian else -signal.SIGKILL,None,started)
            check(ready.wait(3),'owned Popen not captured');deadline=time.monotonic()+8
            while True:
                try:pids={n:int((cwd/n).read_text()) for n in ('descendant','primary','guardian')}
                except (OSError,ValueError):pids={}
                if len(pids)==3 and all(pid>1 for pid in pids.values()):break
                check(time.monotonic()<deadline,'process readiness timeout');time.sleep(.005)
            pids['runtime']=owned['pid'];fds,parents=pin_topology(pids,owned['pid'],c.root,c.bundle.paths[writer]);topology={'pids':pids,'parents':parents}
        else:result=c.call(writer,request,exit=0 if guardian else -signal.SIGKILL)
        topology=auth.json_bytes(c.artifact('topology',json.dumps(topology,sort_keys=True).encode() if c.live else None))
        if c.live:
            signal.pidfd_send_signal(fds['guardian' if guardian else 'runtime'],signal.SIGKILL);result=future.result(timeout=12)
        command=c.bundle.commands[c.row['commands'][0]]
        validate_topology(topology['pids'],topology['parents'],command.get('pid'),command.get('parent_pid'))
        if guardian:
            same(accepted(result)['effects'],2)
            # Unknown cleanup is expected after guardian death. Harness cleanup
            # below is distinct from the outcome and cannot certify it as known.
            if c.live:
                for fd in fds.values():
                    try:signal.pidfd_send_signal(fd,signal.SIGKILL)
                    except ProcessLookupError:pass
            same(accepted(c.call(other,request))['effects'],2)
        else:
            recovered,body=outcome(c.call(other,{'op':'recover','invocation':invocation}));same(recovered['effects'],1)
            check(body['cancelled'] is True and body['cleanup_complete'] is True,'runtime death lost cleanup/outcome')
            observed=auth.json_bytes(c.artifact('exits-before-harness-cleanup',json.dumps(observed_exits(fds),sort_keys=True).encode() if c.live else None))
            check(observed=={'primary':True,'descendant':True,'guardian':True},'claimed known cleanup left a surviving process')
            check(accepted(c.call(other,request))==recovered,'runtime crash replayed command')
        check(c.artifact('launches',(cwd/'launches').read_bytes() if c.live else None)==b'x','crash replayed operation')
    finally:
        if c.live:
            # This direct identity comes from Popen even if topology validation
            # failed. A forged descendant can never enter this owned set.
            cleanup=list(fds.values())+([owned['fd']] if 'fd' in owned else [])
            for fd in cleanup:
                try:signal.pidfd_send_signal(fd,signal.SIGKILL)
                except ProcessLookupError:pass
                os.close(fd)
            if future is not None:
                try:future.result(timeout=10)
                except (ValueError,TimeoutError):pass
            if pool:pool.shutdown(wait=True)
            try:risk_process.cleanup()
            except AssertionError as error:raise ValueError(str(error)) from error
def jobs(mode):
    result=[('config-default',lambda c:configuration(c)),('config-overlay',lambda c:configuration(c,{'home/.ansa/config.json':b'{"schema_version":1,"model":"global","tool_rounds":12,"allow_sudo":false}','home/workspace/.ansa/config.json':b'{"schema_version":1,"model":"project","tool_calls":24}'}))]
    invalid=[b'{"schema_version":2}',b'{"schema_version":1,"unknown":true}',b'{"schema_version":1,"allow_sudo":false}',b'{"schema_version":1,"process_timeout_ms":0}',b'{"schema_version":1,"tool_rounds":65}',b'{"schema_version":1,"process_output_bytes":16385}',b'{"schema_version":1,"model":false}']
    result += [('config-invalid-'+str(i),lambda c,raw=raw:configuration(c,{'home/workspace/.ansa/config.json':raw},True)) for i,raw in enumerate(invalid)]
    for offset in (0,1,3,99):
        for limit in (1,8,8192):result.append((f'read-{offset}-{limit}',lambda c,o=offset,l=limit:read_case(c,o,l,'hello Ж\nneedle\n'.encode())))
    result += [('read-binary',lambda c:read_case(c,0,8192,b'hello\0binary')),('read-invalid-utf8',lambda c:read_case(c,0,8192,b'\xf0\x80\x80\xff')),
               ('nested-instructions',nested),('search-name',lambda c:search(c,'name')),('search-content',lambda c:search(c,'content'))]
    result += [('process-'+spec[0],lambda c,spec=spec:process(c,spec)) for spec in process_specs()]
    if mode=='fault':
        def native(c,suite):check(accepted(c.call('rust',{'op':'case','suite':suite}))=={'suite':suite,'assertions_completed':True},'native Rust tool assertions')
        result += [('rust-'+suite,lambda c,suite=suite:native(c,suite)) for suite in NATIVE]
        result += [(f'cpp-{group}-{suite}',lambda c,g=group,s=suite:c.native(g,s)) for group,suites in CPP.items() for suite in suites]
        result += [(f'stress-{i}',lambda c,i=i:native(c,('detached','runtime-cancel','runtime-nonzero','evidence')[i%4])) for i in range(12)]
        result += [(f'inherited-{w}',lambda c,w=w:inherited(c,w)) for w in ('cpp','rust')]
        result += [(f'partial-{name}-{w}',lambda c,n=name,w=w:partial(c,w,n)) for name in ('request.json','started.json','result.json') for w in ('cpp','rust')]
        result += [(f'crash-{g}-{w}',lambda c,g=g,w=w:crash(c,w,g)) for g in (False,True) for w in ('cpp','rust')]
        invalid=[{'argv':[]},{'argv':['']},{'argv':['sh']*65},{'argv':['sh','x\0y']},{'argv':['x'*16385]}, {'timeout_ms':0},{'timeout_ms':600001},{'output_bytes':0},{'output_bytes':16385},{'cwd':'/invalid\0suffix'}]
        result += [(f'process-invalid-{i}',lambda c,o=o:invalid_process(c,o)) for i,o in enumerate(invalid)]
        result += [('missing-cwd-'+w,lambda c,w=w:missing_cwd(c,w)) for w in ('cpp','rust')]
    return result
def inventory(mode):return [id for id,_ in jobs(mode)]
def manifest():
    value={'schema_version':1,'suite':'tools','compare_ids':inventory('compare'),'fault_ids':inventory('fault')}
    check(auth.json_bytes(protocol.owned_bytes(MANIFEST,MAX))==value,'tools manifest');return value
def execute(bundle,mode,rows,live):
    work=jobs(mode);check(live or [r['id'] for r in rows]==inventory(mode),'tools case inventory')
    for index,(id,job) in enumerate(work):
        row={'id':id,'status':'pass'} if live else rows[index];context=Context(bundle,row,live)
        try:job(context);context.close()
        finally:
            if live:context.temporary.cleanup()
        if live:rows.append(row)
        check(row['status']=='pass','tools skipped/failed')
    check(bundle.used==set(bundle.commands),'unreferenced tools commands')
    roots=[r['root'] for r in rows];check(len(roots)==len(set(roots)),'tools roots reused')
def verify(report,directory):
    try:
        manifest();check(type(report.get('schema_version')) is int and report['schema_version']==1 and report.get('suite')=='tools' and report.get('mode') in ('compare','fault'),'tools report identity')
        check(report.get('manifest_sha256')==common.sha(MANIFEST),'tools manifest identity')
        check(report.get('source',{}).get('files')==common.source_info()['files'] and report['source']['worktree_status']=='','tools source identity/dirty')
        host=report['platform'];check(host['system']=='Linux' and host['machine']=='aarch64' and host['os_release']['ID']=='nixos','native platform')
        binaries={k:Path(v['path']) for k,v in report['binaries'].items()};check(identities(binaries)==report['binaries'],'tools binary identities')
        execute(sessions.Bundle(directory,binaries,report['commands']),report['mode'],report['cases'],False)
        check(report.get('status')=='PASS' and report.get('errors')==[],'tools report status');return []
    except (OSError,ValueError,TypeError,KeyError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as error:return [str(error) if isinstance(error,ValueError) else 'invalid tools evidence: '+type(error).__name__]
def capture(args):
    report={'status':'FAIL','errors':[],'cases':[]};directory=None
    try:
        manifest();check(args.cpp_build and args.rust_target and args.output,'tools capture paths');target=Path(args.output).resolve();check(not any(p=='.env' or p.startswith('.env.') for p in target.parts),'forbidden evidence path');target.mkdir(parents=True,exist_ok=False);directory=target
        source=common.source_info();check(source['worktree_status']=='','clean tools checkpoint required');binaries=paths(Path(args.cpp_build),Path(args.rust_target));report.update(schema_version=1,suite='tools',mode=args.action,source=source,platform=common.platform_info(),manifest_sha256=common.sha(MANIFEST),binaries=identities(binaries),commands={})
        execute(sessions.Bundle(directory,binaries,report['commands']),args.action,report['cases'],True);check(source==common.source_info() and identities(binaries)==report['binaries'],'tools capture changed inputs');report['status']='PASS';report['errors']=verify(report,directory);report['status']='FAIL' if report['errors'] else 'PASS'
    except (OSError,ValueError,TypeError,KeyError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as error:report['errors'].append(str(error) if isinstance(error,ValueError) else 'tools capture failed: '+type(error).__name__)
    if directory is not None:common.write_json(directory/'tools-report.json',report)
    print(json.dumps({'suite':'tools','mode':args.action,'status':report['status'],'cases':len(report['cases']),'errors':report['errors']},indent=2));return int(report['status']!='PASS')
def check_evidence(evidence,fault_evidence=None):
    errors=[]
    for mode,directory in [('compare',Path(evidence).resolve()),('fault',Path(fault_evidence).resolve() if fault_evidence else Path(str(evidence)+'-fault'))]:
        try:
            report=auth.json_bytes(protocol.owned_bytes(directory/'tools-report.json',MAX));check(report.get('mode')==mode,'wrong tools evidence mode');errors += [mode+': '+e for e in verify(report,directory)]
        except (OSError,ValueError,TypeError,KeyError):errors.append(mode+': missing/unsafe tools report')
    print(json.dumps({'milestone':'M6','status':'FAIL' if errors else 'PASS','errors':errors},indent=2));return int(bool(errors))
