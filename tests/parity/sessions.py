"""M4 capture/replay using the existing raw-command and source identity helpers."""
import copy
import json
import math
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import hashlib
import time
from concurrent.futures import ThreadPoolExecutor
import run as common
import protocol
import auth
import session_cases as v

MAX=64*1024*1024
MANIFEST=common.ROOT/'tests/parity/fixtures/sessions.json'
PART='part-00000000000000000000.log'
POINTS=('before_write','after_write','before_sync','after_sync')
def check(value,code): v.check(value,code)
def fixtures():
    names=['golden.json','minimal.log']+[name+'.log' for name in v.golden()['traces']]
    return {str((v.GOLDEN.parent/name).relative_to(common.ROOT)):common.sha(v.GOLDEN.parent/name) for name in names}
def binary_info(paths):return {name:{'path':str(path),'sha256':common.sha(path)} for name,path in paths.items()}
def parts(snapshot):
    check(isinstance(snapshot,list) and len(snapshot)<=4096,'snapshot inventory')
    result={}
    for row in snapshot:
        check(set(row)=={'name_hex','kind','bytes_hex'} and row['kind']=='file','unsafe journal snapshot')
        name=bytes.fromhex(row['name_hex']);check(b'/' not in name and name.startswith(b'part-') and name.endswith(b'.log'),'snapshot name')
        text=name.decode('utf-8');check(text not in result,'duplicate snapshot name');value=bytes.fromhex(row['bytes_hex']);check(len(value)<=MAX,'snapshot size');result[text]=value
    return result
def snapshot(root):
    directory=root/'journal'
    if not directory.exists():return []
    result=[]
    for entry in sorted(os.scandir(directory),key=lambda e:os.fsencode(e.name)):
        name=os.fsencode(entry.name)
        if not name.startswith(b'part-') or not name.endswith(b'.log'):continue
        meta=entry.stat(follow_symlinks=False);safe=stat.S_ISREG(meta.st_mode) and meta.st_nlink==1 and meta.st_uid==os.geteuid() and meta.st_size<=MAX
        value=protocol.owned_bytes(Path(entry.path),MAX).hex() if safe else ''
        result.append({'name_hex':name.hex(),'kind':'file' if safe else 'unsafe','bytes_hex':value})
    return result
class Context:
    def __init__(self,bundle,row,live):
        self.bundle=bundle;self.row=row;self.live=live;self.command_index=0;self.snapshot_index=0
        if live:
            self.temporary=tempfile.TemporaryDirectory(prefix='ansa-m4-');self.root=Path(self.temporary.name)
            marker=self.root/'fixture-owner';marker.write_bytes(b'ansa-m4-synthetic-v1\n');marker.chmod(0o600)
            row.update(root=str(self.root),snapshots=[])
        else:
            self.root=Path(row['root']);check(self.root.parent==Path(tempfile.gettempdir()) and self.root.name.startswith('ansa-m4-') and str(self.root)==str(self.root.resolve()),'fixture root identity')
    def close(self):
        if self.live:self.temporary.cleanup()
        else:check(self.command_index==len(self.row['commands']) and self.snapshot_index==len(self.row['snapshots']),'unreferenced case evidence')
    def call(self,adapter,request,exit=0):
        request=copy.deepcopy(request)
        if request.get('op') in ('journal','store','catalog','hold'):request['root']=str(self.root)
        label=self.row['id']+'-'+str(self.command_index);self.command_index+=1
        raw=(v.compact(request)+'\n').encode()
        if self.live:
            self.row.setdefault('commands',[]).append(label);path=self.bundle.directory/(label+'.stdin');path.write_bytes(raw)
            command=common.command([self.bundle.paths[adapter]],common.ROOT,self.bundle.directory,label,timeout=45,input_file=path)
            command['stdin']={'path':path.name,'sha256':common.sha(path)};self.bundle.commands[label]=command
        else:check(self.row['commands'][self.command_index-1]==label,'command order differs')
        check(label not in self.bundle.used,'duplicate command reference');self.bundle.used.add(label)
        command=self.bundle.commands[label]
        check(auth.command_ok(command,exit) and command['argv']==[str(self.bundle.paths[adapter])] and command['cwd']==str(common.ROOT),'command identity/exit/timeout')
        retained={stream:protocol.retained_bytes(self.bundle.directory,command.get(stream),label+'.'+stream,MAX) for stream in ('stdin','stdout','stderr')}
        check(retained['stdin']==raw,'command input differs')
        if exit:check(not retained['stdout'],'crashed writer returned admission');return None
        lines=retained['stdout'].splitlines()
        if request.get('op')=='hold':check(len(lines)==2 and json.loads(lines[0])=={'ready':True},'lock readiness');lines=lines[1:]
        check(len(lines)==1,'adapter output inventory');value=auth.json_bytes(lines[0])
        check(isinstance(value,dict) and type(value.get('accepted')) is bool,'adapter output schema');return value
    def shot(self):
        if self.live:self.row['snapshots'].append(snapshot(self.root))
        value=self.row['snapshots'][self.snapshot_index];self.snapshot_index+=1;return value
    def prepare(self,values):
        if not self.live:return
        directory=self.root/'journal';directory.mkdir(mode=0o700,exist_ok=True)
        for name,value in values.items():
            path=os.fsencode(directory)+b'/'+(name if isinstance(name,bytes) else name.encode())
            fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
            with os.fdopen(fd,'wb') as file:file.write(value)
def accepted(row):check(row.get('accepted') is True,'required adapter operation rejected');return row['value']
def state_request(events):return {'op':'state','session':'fixture','configuration':v.golden()['configuration'],'plan_tools':v.golden()['configuration'],'events':events,'queries':[]}
def store_request(events=()):return dict(state_request(list(events)),op='store')
def normalized(value):
    if isinstance(value,dict):return {k:normalized(x) for k,x in value.items() if k!='diagnostic'}
    if isinstance(value,list):return [normalized(x) for x in value]
    return value
def trace_interop(c,name,writer):
    other='rust' if writer=='cpp' else 'cpp';events=v.golden()['traces'][name]
    written=accepted(c.call(writer,{'op':'journal','events':events}));check(all(e['accepted'] is True for e in written['appends']),'fixture write')
    before=parts(c.shot());check(v.decode(before)==events,'written canonical events')
    read=accepted(c.call(other,store_request()));check(read['diagnostic']=='' and read['events']==events,'cross reader replay')
    check(parts(c.shot())==before,'read changed journal bytes')
    expected=accepted(c.call(writer,state_request(events)))['state'];check(read['state']==expected,'cross reader state differs')
    if name=='completed':
        config=copy.deepcopy(v.golden()['configuration']);config['epoch']=expected['context']['epoch']
        event=v.accepted(config,expected['history'],2)
        result=accepted(c.call(other,store_request([event])));check(result['appends']==[{'accepted':True}],'cross append')
        after=parts(c.shot());check(all(after[n].startswith(value) for n,value in before.items()),'append changed original bytes')
        reopened=accepted(c.call(writer,store_request()));check(reopened['state']==result['state'] and reopened['events']==events+[event],'reverse reopen')
def truncation(c,cut):
    raw=(v.GOLDEN.parent/'minimal.log').read_bytes()[:cut];c.prepare({PART:raw});check(parts(c.shot())=={PART:raw},'truncation setup')
    for writer in ('cpp','rust'):
        value=accepted(c.call(writer,{'op':'journal'}));check(value['diagnostic']=='' and value['events']==[],'torn tail read')
    result=accepted(c.call('rust',{'op':'journal','events':[{'kind':'test','value':2}]}));check(result['appends']==[{'accepted':True}],'torn append')
    after=parts(c.shot());check(after[PART].startswith(raw) and len(after)==(1 if cut==92 else 2) and (cut==92 or after[PART]==raw),'torn tail overwritten');check(v.decode(after)==[{'kind':'test','value':2}],'linked torn tail framing')
    value=accepted(c.call('cpp',{'op':'journal'}));check(value['events']==[{'kind':'test','value':2}] and value['diagnostic']=='','C++ linked recovery')
def crash_specs():
    found={}
    for events in v.golden()['traces'].values():
        for index,event in enumerate(events):found.setdefault(event['kind'],(events,index))
    check(set(found)=={'init','accepted','response','intent','outcome','continued','compacted','done','cancel','pause','resume'},'event kind inventory')
    return found
def crash(c,kind,point,writer,action='exit'):
    other='rust' if writer=='cpp' else 'cpp';events,index=crash_specs()[kind];prefix=events[:index]
    if prefix:accepted(c.call(writer,store_request(prefix[1:])))
    before=parts(c.shot());request=store_request([events[index]] if kind!='init' else []);request['fault']={'kind':kind,'point':point,'action':action}
    if action=='exit':c.call(writer,request,exit=71)
    else:
        result=accepted(c.call(writer,request));check(result['appends'][0].get('accepted') is False,'IO fault acknowledged');check(result['state']['turn']['stage']!='completed','undurable assistant published')
    after=parts(c.shot());check(all(after[n].startswith(value) for n,value in before.items()),'crash changed prefix')
    replay=accepted(c.call(other,store_request()));check(replay['diagnostic']=='','cross crash reopen')
    actual=prefix+(events[index:index+1] if point!='before_write' else [])
    if not actual:actual=events[:1]
    check(replay['events']==actual,'crash recovered different durable prefix')
    final=parts(c.shot());check(v.decode(final)==actual,'crash raw framing')
    if actual[-1]['kind']=='intent':check(replay['state']['unknown']==actual[-1]['invocation']['operation'],'lost unresolved intent')
BAD=('header','length','complement','checksum','sequence','previous','version','part-name','part-gap','invalid-utf8-name','event-kind','configuration')
def malformed(c,kind):
    events=v.golden()['traces']['completed'];raw=v.encode(events[:1]);name=PART
    if kind in ('sequence','previous','version','event-kind','configuration'):
        event=copy.deepcopy(events[0]);event['kind']='future_event' if kind=='event-kind' else event['kind']
        if kind=='configuration':event['session']='wrong_session'
        body={'schema_version':2 if kind=='version' else 1,'sequence':2 if kind=='sequence' else 1,'previous':'1'*64 if kind=='previous' else '0'*64,'event':event};body=v.compact(body).encode();raw=v.part_header(0,'0'*64)+f'{len(body):08x} {len(body)^0xffffffff:08x} {v.digest(body)}\n'.encode()+body
    elif kind=='header':raw=b'WRONG!'+raw[6:]
    elif kind=='length':raw=raw[:92]+b'00000000 ffffffff'+raw[109:]
    elif kind=='complement':raw=raw[:101]+b'00000000'+raw[109:]
    elif kind=='checksum':raw=raw[:110]+b'0'*64+raw[174:]
    elif kind=='part-name':name='part-invalid.log'
    elif kind=='part-gap':name='part-00000000000000000001.log'
    elif kind=='invalid-utf8-name':name=b'part-\xff.log'
    c.prepare({name:raw});before=c.shot()
    for adapter in ('cpp','rust'):
        result=accepted(c.call(adapter,store_request()));check(bool(result['diagnostic']),'malformed state writable');check(result['state']['initialized'] is False,'unsupported state silently rebound')
        rejected=accepted(c.call(adapter,{'op':'journal','events':[{'kind':'test','value':2}]}));check(rejected['appends'][0]['accepted'] is False,'read-only append accepted')
    check(c.shot()==before,'corruption recovery overwrote bytes')
GUARDS=('part-hardlink','part-symlink','part-fifo','lock-hardlink','lock-symlink','lock-fifo','marker-hardlink','marker-fifo')
def guard(c,kind):
    sentinel=b'owned M4 sentinel';directory=c.root/'journal';alias=c.root/'sentinel'
    if c.live:
        alias.write_bytes(sentinel);directory.mkdir(mode=0o700)
        target=directory/PART if kind.startswith('part-') else directory/'lock' if kind.startswith('lock-') else c.root/'fixture-owner'
        if target.exists():target.unlink()
        if kind.endswith('hardlink'):os.link(alias,target)
        elif kind.endswith('symlink'):target.symlink_to(alias)
        else:os.mkfifo(target,0o600)
    before=c.shot()
    for adapter in ('cpp','rust'):
        row=c.call(adapter,{'op':'journal','events':[{'kind':'test','value':2}]})
        if row['accepted']:check(row['value']['diagnostic'] and row['value']['appends'][0]['accepted'] is False,'unsafe part accepted')
    check(c.shot()==before,'guard changed part')
    if c.live:c.row['sentinel_sha256']=v.digest(alias.read_bytes())
    check(c.row['sentinel_sha256']==v.digest(sentinel),'guard changed owned sentinel')
def catalog(c,writer):
    other='rust' if writer=='cpp' else 'cpp';created=accepted(c.call(writer,{'op':'catalog'}));id=created['id'];check(id.startswith('s_') and len(id)<=64 and all(ch.isascii() and (ch.isalnum() or ch in '_-') for ch in id),'created session ID')
    opened=accepted(c.call(other,{'op':'catalog','id':id}));check(opened==created,'catalog cross reopen')
    if c.live:
        directory=c.root/'sessions'/id;metadata=(directory/'info.json').read_bytes();c.row['metadata_hex']=metadata.hex()
        (directory/PART).write_bytes((v.GOLDEN.parent/'minimal.log').read_bytes())
    metadata=json.loads(bytes.fromhex(c.row['metadata_hex']));check(metadata=={'schema_version':1,'id':id,'cwd':str(c.root/'workspace'),'profile':'offline'},'catalog metadata binding')
    check(accepted(c.call(other,{'op':'catalog','resume':True}))==created,'latest selection')
    rejected=c.call(other,{'op':'catalog','resume':True,'id':id});check(rejected['accepted'] is False,'selection conflict accepted')
def process_lock(c,writer):
    other='rust' if writer=='cpp' else 'cpp'
    if c.live:
        with ThreadPoolExecutor(max_workers=1) as pool:
            future=pool.submit(c.call,writer,{'op':'hold'})
            path=c.bundle.directory/(c.row['id']+'-0.stdout');deadline=time.monotonic()+5
            while not path.exists() or b'{"ready":true}' not in path.read_bytes():
                check(time.monotonic()<deadline and not future.done(),'lock holder missing');time.sleep(0.005)
            denied=c.call(other,{'op':'journal'});held=future.result()
    else:held=c.call(writer,{'op':'hold'});denied=c.call(other,{'op':'journal'})
    check(held['accepted'] is True and denied=={'accepted':False,'diagnostic':'session_busy'},'cross-process lock failed')
def inventory(mode):
    result=[case['id'] for case in v.cases()]
    if mode=='compare':return result+[f'interop-{name}-{writer}' for name in v.golden()['traces'] for writer in ('cpp','rust')]+['catalog-cpp','catalog-rust']
    return result+[f'torn-{cut}' for cut in range((v.GOLDEN.parent/'minimal.log').stat().st_size)]+['bad-'+name for name in BAD]+['guard-'+name for name in GUARDS]+[f'crash-{kind}-{point}-{writer}' for kind in crash_specs() for point in POINTS for writer in ('cpp','rust')]+[f'io-{kind}-{writer}' for kind in ('outcome','done') for writer in ('cpp','rust')]+['lock-cpp','lock-rust']
class Bundle:
    def __init__(self,directory,paths,commands):self.directory=directory;self.paths=paths;self.commands=commands;self.used=set()
def execute_cases(bundle,mode,rows,live):
    expected=inventory(mode);check(live or [row.get('id') for row in rows]==expected,'case inventory differs')
    cases=v.cases();requests=[case['request'] for case in cases];outputs={}
    # Reuse per-command raw infrastructure while batching all pure codec/reducer cases.
    for adapter in ('cpp','rust'):
        label='pure-'+adapter;raw=b''.join((v.compact(r)+'\n').encode() for r in requests)
        if live:
            path=bundle.directory/(label+'.stdin');path.write_bytes(raw);command=common.command([bundle.paths[adapter]],common.ROOT,bundle.directory,label,timeout=90,input_file=path);command['stdin']={'path':path.name,'sha256':common.sha(path)};bundle.commands[label]=command
        command=bundle.commands[label];check(auth.command_ok(command) and command['argv']==[str(bundle.paths[adapter])] and command['cwd']==str(common.ROOT),'pure command identity');bundle.used.add(label)
        retained={stream:protocol.retained_bytes(bundle.directory,command.get(stream),label+'.'+stream,MAX) for stream in ('stdin','stdout','stderr')};check(retained['stdin']==raw,'pure input differs');outputs[adapter]=[auth.json_bytes(line) for line in retained['stdout'].splitlines()];check(len(outputs[adapter])==len(cases),'pure output inventory')
    for i,case in enumerate(cases):
        for adapter in outputs:v.output_pass(case,outputs[adapter][i])
        if outputs['cpp'][i]['accepted']:check(normalized(outputs['cpp'][i]['value'])==normalized(outputs['rust'][i]['value']),'pure values differ: '+case['id'])
        if live:rows.append({'id':case['id'],'status':'pass'})
    jobs=[]
    if mode=='compare':
        jobs += [(f'interop-{name}-{writer}',lambda c,n=name,w=writer:trace_interop(c,n,w)) for name in v.golden()['traces'] for writer in ('cpp','rust')]
        jobs += [('catalog-'+writer,lambda c,w=writer:catalog(c,w)) for writer in ('cpp','rust')]
    else:
        jobs += [(f'torn-{cut}',lambda c,n=cut:truncation(c,n)) for cut in range((v.GOLDEN.parent/'minimal.log').stat().st_size)]
        jobs += [('bad-'+name,lambda c,n=name:malformed(c,n)) for name in BAD]
        jobs += [('guard-'+name,lambda c,n=name:guard(c,n)) for name in GUARDS]
        jobs += [(f'crash-{kind}-{point}-{writer}',lambda c,k=kind,p=point,w=writer:crash(c,k,p,w)) for kind in crash_specs() for point in POINTS for writer in ('cpp','rust')]
        jobs += [(f'io-{kind}-{writer}',lambda c,k=kind,w=writer:crash(c,k,'before_sync',w,'fail')) for kind in ('outcome','done') for writer in ('cpp','rust')]
        jobs += [('lock-'+writer,lambda c,w=writer:process_lock(c,w)) for writer in ('cpp','rust')]
    for offset,(id,job) in enumerate(jobs,len(cases)):
        row={'id':id,'status':'pass'} if live else rows[offset];context=Context(bundle,row,live)
        try:job(context);context.close()
        finally:
            if live:context.temporary.cleanup()
        if live:rows.append(row)
    check([r['id'] for r in rows]==expected and all(r.get('status')=='pass' for r in rows),'case skipped/failed')
    check(bundle.used==set(bundle.commands),'unreferenced commands')
    roots=[r['root'] for r in rows if 'root' in r];check(len(roots)==len(set(roots)),'fixture root reused')
def manifest():
    data=json.loads(protocol.owned_bytes(MANIFEST,MAX));check(data=={'schema_version':1,'suite':'sessions','fixtures':fixtures(),'compare_ids':inventory('compare'),'fault_ids':inventory('fault')},'session manifest differs');return data
def verify(report,directory):
    errors=[]
    try:
        manifest();check(type(report.get('schema_version')) is int and report['schema_version']==1 and report.get('suite')=='sessions' and report.get('mode') in ('compare','fault'),'report identity')
        check(report.get('fixtures')==fixtures() and report.get('manifest_sha256')==common.sha(MANIFEST),'fixture identities')
        check(report.get('source',{}).get('files')==common.source_info()['files'] and report['source'].get('worktree_status')=='','source snapshot differs/dirty')
        host=report['platform'];check(host['system']=='Linux' and host['machine']=='aarch64' and host['os_release']['ID']=='nixos','native platform')
        identities=report['binaries'];check(set(identities)=={'cpp','rust'},'binary inventory');paths={key:Path(value['path']) for key,value in identities.items()};check(identities==binary_info(paths),'binary identities')
        execute_cases(Bundle(directory,paths,report['commands']),report['mode'],report['cases'],False)
        check(report.get('status')=='PASS' and report.get('errors')==[],'report failed')
    except (OSError,ValueError,KeyError,TypeError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as error:errors.append(str(error) if isinstance(error,ValueError) else 'invalid session evidence: '+type(error).__name__)
    return errors
def capture(args):
    report={'status':'FAIL','errors':[],'cases':[]};directory=None
    try:
        manifest();check(args.cpp_build and args.rust_target and args.output,'session capture needs paths');target=Path(args.output).resolve();check(not any(p=='.env' or p.startswith('.env.') for p in target.parts),'forbidden evidence path');target.mkdir(parents=True,exist_ok=False);directory=target
        paths={'cpp':(common.ROOT/args.cpp_build).resolve()/'ansa_session_oracle','rust':(common.ROOT/args.rust_target).resolve()/'debug/examples/session_adapter'};source=common.source_info();check(not source['worktree_status'],'clean source required')
        report.update(schema_version=1,suite='sessions',mode=args.action,source=source,platform=common.platform_info(),fixtures=fixtures(),manifest_sha256=common.sha(MANIFEST),binaries=binary_info(paths),commands={})
        execute_cases(Bundle(directory,paths,report['commands']),args.action,report['cases'],True);check(source==common.source_info() and report['binaries']==binary_info(paths),'capture inputs changed');report['status']='PASS';report['errors']=verify(report,directory);report['status']='FAIL' if report['errors'] else 'PASS'
    except (OSError,ValueError,KeyError,TypeError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as error:report['errors'].append(str(error) if isinstance(error,ValueError) else 'session capture failed: '+type(error).__name__)
    if directory is not None:common.write_json(directory/'session-report.json',report)
    print(json.dumps({'suite':'sessions','mode':args.action,'status':report['status'],'cases':len(report['cases']),'errors':report['errors']},indent=2));return int(report['status']!='PASS')
def check_evidence(evidence,fault_evidence=None):
    errors=[]
    for mode,directory in [('compare',Path(evidence).resolve()),('fault',Path(fault_evidence).resolve() if fault_evidence else Path(str(evidence)+'-fault'))]:
        try:
            report=auth.json_bytes(protocol.owned_bytes(directory/'session-report.json',MAX));check(report.get('mode')==mode,'wrong evidence mode');errors += [mode+': '+e for e in verify(report,directory)]
        except (OSError,ValueError,KeyError,TypeError):errors.append(mode+': missing/unsafe session report')
    print(json.dumps({'milestone':'M4','status':'FAIL' if errors else 'PASS','errors':errors},indent=2));return int(bool(errors))
