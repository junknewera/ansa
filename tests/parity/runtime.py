"""M5 owned runtime capture and offline replay of retained native evidence."""
import copy
import hashlib
import json
from pathlib import Path
import re
import subprocess
import auth
import protocol
import run as common
import sessions
import session_cases as v

MAX=64*1024*1024
MANIFEST=common.ROOT/'tests/parity/fixtures/runtime.json'
NATIVE=('roundtrip','cancel','priority','admission','resync','invalid-response','tool-cancel','shutdown',
        'durable','auth-cancel','auth-drop','preview-flood','durable-bypass','mixed-registry',
        'restart-limit','storage-failure','summary','summary-cancel','checkpoints','restart-resume','plan-admission','unknown-restart','unresolved-restart','multi-round')
CPP={'runtime':('cancel','priority','admission','resync','invalid-response','tool-cancel','roundtrip','shutdown'),
     'summary':('codec','runtime','cancel'), 'frontend':('streaming','recovery','adversarial','modes','overflow'),
     'session':('blocked-disk','late-outcome','intent-cancel','batch-recovery','dispatch-guard')}
def check(value,code): v.check(value,code)
def integer(value):
    check(isinstance(value,protocol.NumberToken) and value.integer and re.fullmatch(r'0|[1-9][0-9]*',value.lexeme),'noncanonical runtime integer')
    result=int(value.lexeme);check(result<=2**64-1,'runtime integer exceeds u64');return result
def inventory(mode):
    if mode=='compare':return [f'trace-{c}-{t}-{m}-{d}' for c in range(4) for t in range(1,5) for m in ('basic','plan') for d in (False,True)]+['interop-cpp','interop-rust']
    return ['rust-'+s for s in NATIVE]+[f'cpp-{g}-{s}' for g,suites in CPP.items() for s in suites]+[f'crash-{k}-{p}-{w}' for k in ('accepted','response','intent','outcome','continued','done') for p in sessions.POINTS for w in ('cpp','rust')]+[f'stress-{i}' for i in range(20)]
def manifest():
    expected={'schema_version':1,'suite':'runtime','compare_ids':inventory('compare'),'fault_ids':inventory('fault')}
    check(auth.json_bytes(protocol.owned_bytes(MANIFEST,MAX))==expected,'runtime manifest differs');return expected
def binary_info(paths):
    expected={'cpp':'ansa_runtime_oracle','rust':'runtime_adapter','cppsession':'ansa_session_oracle','rustsession':'session_adapter',
              **{'native-'+g:'omh_'+g+'_tests' if g in ('runtime','summary','session') else 'ansa_frontend_tests' for g in CPP}}
    check(set(paths)==set(expected),'runtime binary inventory')
    result={}
    for key,path in paths.items():
        check(path.name==expected[key],'runtime binary name')
        result[key]={'path':str(path),'sha256':hashlib.sha256(protocol.owned_bytes(path,256*1024*1024,executable=True)).hexdigest()}
    return result
class Context(sessions.Context):
    def call(self,adapter,request,exit=0,fault=None):
        request=copy.deepcopy(request)
        if request.get('op')=='trace' and request.get('durable'):request['root']=str(self.root)
        return super().call(adapter,request,exit,fault)
    def native(self,group,suite):
        label=self.row['id']+'-'+str(self.command_index);self.command_index+=1
        argv=[str(self.bundle.paths['native-'+group]),suite]
        if self.live:
            self.row.setdefault('commands',[]).append(label)
            self.bundle.commands[label]=common.command(argv,common.ROOT,self.bundle.directory,label,timeout=60)
        else:check(self.row['commands'][self.command_index-1]==label,'native order')
        check(label not in self.bundle.used,'duplicate command');self.bundle.used.add(label)
        command=self.bundle.commands[label]
        check(auth.command_ok(command) and command['argv']==argv and command['cwd']==str(common.ROOT),'native identity/timeout/exit')
        check('stdin' not in command and 'process_env' not in command,'unexpected native inputs')
        for stream in ('stdout','stderr'):
            data=protocol.retained_bytes(self.bundle.directory,command.get(stream),label+'.'+stream,MAX)
            check(not data,'native baseline diagnostics')
def request(calls=1,turns=1,mode='basic',durable=False,start=0):
    return {'op':'trace','calls':calls,'turns':turns,'mode':mode,'durable':durable,'start':start}
def configuration(spec):
    config=v.configuration();config['binding']={'provider':'openai-siwc','account':'offline-fixture','requested_model':'gpt-6.1-sol','returned_model':'gpt-6.1-sol'}
    config['epoch']={'id':'fixture_epoch','instructions':'Use the pure fixture tool','exact_state':{'mode':'offline','revision':1}}
    descriptor=config['tools']['descriptors'][0]
    identity={'name':descriptor['name'],'description':descriptor['description'],'implementation_revision':descriptor['revision'],'schema':descriptor['schema']}
    config['tools']['revision']=v.digest([{'descriptor':identity,'requires_journal':True}] if spec['durable'] else [identity])
    return config
def pipeline(spec):
    config=configuration(spec)
    calls=spec['calls'];history=[];requests=[]
    for turn in range(spec['start']+spec['turns']):
        current=v.accepted(config,history,mode=spec['mode'])['next'];current['input'][-1]['content']='fixture'
        if turn>=spec['start']:requests.append(copy.deepcopy(current))
        if calls:
            current['input'] += [{'type':'function_call','id':f'item{i}','call_id':f'call{i}','namespace':'omh','name':'get_test_value','arguments':'{}','status':'completed'} for i in range(calls)]
            current['input'] += [{'type':'function_call_output','call_id':f'call{i}','output':'{"ok":true,"value":"OMH_FIXED_VALUE_42"}'} for i in range(calls)]
            if turn>=spec['start']:requests.append(copy.deepcopy(current))
        history=current['input']+[{'type':'message','id':'message','role':'assistant','status':'completed','content':[{'type':'output_text','text':'OMH_TOOL_OK:OMH_FIXED_VALUE_42' if calls else 'ANSWER'}]}]
    return requests,history
def journal_events(spec,inc):
    config=configuration(spec);requests,_=pipeline(spec);calls=spec['calls'];stride=2 if calls else 1
    events=[{'kind':'init','version':1,'session':'offline','configuration':config}]
    def response(request,items):return {'request':request,'id':'response','model':request['binding']['returned_model'],'items':items}
    for offset in range(spec['turns']):
        index=spec['start']+offset;turn=f'turn{index}';request_id=f'request_{index+1}';run=f'{inc}_{request_id}';first=requests[offset*stride]
        accepted=v.accepted(config,first['input'][:-1],index=index+1,mode=spec['mode'])
        accepted.update(command=turn,turn=turn,request=request_id,text='fixture',operation_run=run,next=first,round_limit=1,call_limit=16)
        events.append(accepted)
        if calls:
            continued=requests[offset*stride+1];items=continued['input'][len(first['input']):len(first['input'])+calls]
            events.append({'kind':'response','turn':turn,'response':response(first,items)})
            for call in range(calls):
                operation=f'{run}:op:{call+1}';call_id=f'call{call}'
                events += [{'kind':'intent','turn':turn,'invocation':{'operation':operation,'call':call_id,'name':'get_test_value','revision':'1','arguments':{}}},
                           {'kind':'outcome','turn':turn,'result':{'operation':operation,'call':call_id,'output':'{"ok":true,"value":"OMH_FIXED_VALUE_42"}','execution':0,'effects':1,'dispatched':True}}]
            events.append({'kind':'continued','turn':turn})
            final=continued
        else:final=first
        message={'type':'message','id':'message','role':'assistant','status':'completed','content':[{'type':'output_text','text':'OMH_TOOL_OK:OMH_FIXED_VALUE_42' if calls else 'ANSWER'}]}
        events += [{'kind':'response','turn':turn,'response':response(final,[message])},{'kind':'done','turn':turn}]
    return events
def journal_matches(parts,spec,value):
    decoded=v.decode(parts)
    check(protocol.structural(decoded)==protocol.structural(journal_events(spec,value['incarnation'])),'runtime journal/trace/config binding')
    return decoded
def trace_value(output,spec):
    value=sessions.accepted(output)
    check(set(value)=={'incarnation','invocations','requests','events','transcript','context','snapshot'},'runtime value fields')
    inc=value['incarnation'];check(isinstance(inc,str) and re.fullmatch('[0-9a-f]{32}',inc),'incarnation entropy shape')
    calls,turns,start=spec['calls'],spec['turns'],spec['start'];step=2*calls+2
    check(integer(value['invocations'])==calls*turns,'executor count')
    text='OMH_TOOL_OK:OMH_FIXED_VALUE_42' if calls else 'ANSWER'
    expected={'state':'completed','busy':False,'text':text,'diagnostic':'','completed':calls,'history':(start+turns)*step,'transcript':2*(start+turns),'compactions':0,'durable':True,'storage_failed':False,'mode':spec['mode'],
              'request':f'request_{start+turns}','submit':f'turn{start+turns-1}','recovery':None,'preview':None,'resume_available':False}
    snapshot=copy.deepcopy(value['snapshot']);through=integer(snapshot.pop('through'))
    check(protocol.structural(snapshot)==protocol.structural(expected),'terminal snapshot safety assertion')
    check(len(value['requests'])==turns*(2 if calls else 1),'production request count')
    check(len(value['transcript'])==2*(start+turns),'transcript size')
    for index,entry in enumerate(value['transcript']):
        turn=index//2;expected={'turn':f'turn{turn}','request':f'request_{turn+1}','role':'user' if index%2==0 else 'assistant','text':'fixture' if index%2==0 else text,'state':'completed','mode':spec['mode']}
        check(entry==expected,'transcript content/identity')
    if spec['durable']:check(len(value['context'])==(start+turns)*step,'durable context size')
    else:check(value['context'] is None,'volatile context inventory')
    previous=0;stable=[];cursor=0
    for turn in range(start,start+turns):
        types=['accepted']+['started','finished']*calls+['text','done']
        for kind in types:
            check(cursor<len(value['events']),'missing event');event=value['events'][cursor];cursor+=1
            seq=integer(event['sequence']);check(seq>previous,'event order');previous=seq
            check(event['type']==kind and event['session']=='offline' and event['turn']==f'turn{turn}' and event['request']==f'request_{turn+1}' and event['incarnation']==inc,'event identity/order')
            item=copy.deepcopy(event);del item['sequence'];del item['incarnation']
            if kind=='accepted':check(item['command']==f'turn{turn}','accepted identity')
            if kind in ('started','finished'):
                call=int(item['call'].removeprefix('call'));check(0<=call<calls,'tool call bounds')
                expected_op=f'{inc}_request_{turn+1}:op:{call+1}'
                check(item['operation']==expected_op,'runtime assigned operation identity')
                item['operation']=f'request_{turn+1}:op:{call+1}'
                if kind=='finished':check(item['execution']=='succeeded' and item['effects']=='known_result' and item['output']=='{"ok":true,"value":"OMH_FIXED_VALUE_42"}','known tool outcome')
                else:check(item['name']=='get_test_value','tool name')
            if kind=='text':check(item['text']==text,'durable text')
            stable.append(item)
    check(cursor==len(value['events']),'extra/late events')
    check(through>previous,'terminal snapshot cursor')
    requests,history=pipeline(spec)
    check(protocol.structural(value['requests'])==protocol.structural(requests),'production requests differ from independent pipeline')
    if spec['durable']:check(protocol.structural(value['context'])==protocol.structural(history),'durable context values')
    normalized=copy.deepcopy(value);del normalized['incarnation'];normalized['events']=stable;normalized['snapshot']=snapshot
    return protocol.structural(normalized)
def compare(c,spec):
    values=[]
    # Separate roots keep the two fresh captures independent.
    first=sessions.accepted(c.call('cpp',spec))
    values.append(trace_value({'accepted':True,'value':first},spec))
    if spec['durable']:
        before=sessions.parts(c.shot());journal_matches(before,spec,first)
        # Recreate a fresh journal under the same guarded root for the Rust writer.
        if c.live:
            for name in before:(c.root/'journal'/name).unlink()
    second=sessions.accepted(c.call('rust',spec));values.append(trace_value({'accepted':True,'value':second},spec))
    check(values[0]==values[1],'C++/Rust runtime values differ')
    if spec['durable']:journal_matches(sessions.parts(c.shot()),spec,second)
def interop(c,writer):
    other='rust' if writer=='cpp' else 'cpp'
    spec=request(durable=True);first=sessions.accepted(c.call(writer,spec));trace_value({'accepted':True,'value':first},spec)
    before=sessions.parts(c.shot());events=journal_matches(before,spec,first)
    spec=request(durable=True,start=1);second=sessions.accepted(c.call(other,spec));trace_value({'accepted':True,'value':second},spec)
    after=sessions.parts(c.shot());check(all(after[n].startswith(data) for n,data in before.items()),'interop overwrote canonical prefix')
    decoded=v.decode(after);expected=events+journal_events(spec,second['incarnation'])[1:]
    check(protocol.structural(decoded)==protocol.structural(expected),'cross-runtime append/reopen binding')
def crash(c,kind,point,writer):
    spec=request(durable=True);spec['fault']={'kind':kind,'point':point,'action':'exit'}
    c.call(writer,spec,exit=71)
    before=sessions.parts(c.shot());events=v.decode(before)
    check(events and events[0]['kind']=='init','runtime crash initialization')
    expected=['init','accepted','response','intent','outcome','continued','response','done'];target=expected.index(kind)
    length=target if point=='before_write' else target+1
    check([e['kind'] for e in events]==expected[:length],'runtime crash durable prefix')
    inc='a'*32
    if length>1:
        run=events[1]['operation_run'];check(isinstance(run,str) and re.fullmatch('[0-9a-f]{32}_request_1',run),'crash operation identity')
        inc=run[:32]
    check(protocol.structural(events)==protocol.structural(journal_events(spec,inc)[:length]),'runtime crash journal binding')
    config=events[0]['configuration']
    results=[]
    for adapter in ('cppsession','rustsession'):
        result=sessions.accepted(c.call(adapter,{'op':'store','session':'offline','configuration':config,'plan_tools':config,'events':[]}))
        check(result['diagnostic']=='' and result['events']==events,'cross-client crash replay')
        results.append(protocol.structural(result['state']))
        check(sessions.parts(c.shot())==before,'crash reopen changed bytes')
    check(results[0]==results[1],'cross-client crash state differs')
def execute(bundle,mode,rows,live):
    jobs=[]
    if mode=='compare':
        jobs=[(f'trace-{calls}-{turns}-{m}-{d}',lambda c,s=request(calls,turns,m,d):compare(c,s)) for calls in range(4) for turns in range(1,5) for m in ('basic','plan') for d in (False,True)]
        jobs += [('interop-'+w,lambda c,w=w:interop(c,w)) for w in ('cpp','rust')]
    else:
        def native(c,suite):
            value=sessions.accepted(c.call('rust',{'op':'case','suite':suite}));check(value=={'suite':suite,'assertions_completed':True},'native Rust assertions')
        jobs=[('rust-'+s,lambda c,s=s:native(c,s)) for s in NATIVE]
        jobs += [(f'cpp-{g}-{s}',lambda c,g=g,s=s:c.native(g,s)) for g,suites in CPP.items() for s in suites]
        jobs += [(f'crash-{k}-{p}-{w}',lambda c,k=k,p=p,w=w:crash(c,k,p,w)) for k in ('accepted','response','intent','outcome','continued','done') for p in sessions.POINTS for w in ('cpp','rust')]
        jobs += [(f'stress-{i}',lambda c,i=i:native(c,('priority','tool-cancel','preview-flood','checkpoints')[i%4])) for i in range(20)]
    check([id for id,_ in jobs]==inventory(mode),'runtime job inventory')
    check(live or [r.get('id') for r in rows]==inventory(mode),'runtime case inventory')
    for index,(id,job) in enumerate(jobs):
        row={'id':id,'status':'pass'} if live else rows[index];c=Context(bundle,row,live)
        try:job(c);c.close()
        finally:
            if live:c.temporary.cleanup()
        if live:rows.append(row)
        check(row['status']=='pass','failed/skipped runtime case')
    check(bundle.used==set(bundle.commands),'unreferenced runtime commands')
    roots=[r['root'] for r in rows];check(len(roots)==len(set(roots)),'reused runtime root')
def verify(report,directory):
    try:
        manifest();check(report.get('schema_version')==1 and type(report['schema_version']) is int and report.get('suite')=='runtime' and report.get('mode') in ('compare','fault'),'runtime report identity')
        check(report.get('manifest_sha256')==common.sha(MANIFEST),'runtime manifest hash')
        check(report.get('source',{}).get('files')==common.source_info()['files'] and report['source']['worktree_status']=='','runtime source identity/dirty')
        host=report['platform'];check(host['system']=='Linux' and host['machine']=='aarch64' and host['os_release']['ID']=='nixos','native NixOS/aarch64')
        paths={k:Path(v['path']) for k,v in report['binaries'].items()};check(report['binaries']==binary_info(paths),'runtime binary hash')
        execute(sessions.Bundle(directory,paths,report['commands']),report['mode'],report['cases'],False)
        check(report.get('status')=='PASS' and report.get('errors')==[],'runtime report status');return []
    except (OSError,ValueError,KeyError,TypeError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as e:return [str(e) if isinstance(e,ValueError) else 'invalid runtime evidence: '+type(e).__name__]
def capture(args):
    report={'status':'FAIL','errors':[],'cases':[]};directory=None
    try:
        manifest();check(args.cpp_build and args.rust_target and args.output,'runtime capture paths')
        target=Path(args.output).resolve();check(not any(p=='.env' or p.startswith('.env.') for p in target.parts),'forbidden evidence path');target.mkdir(parents=True,exist_ok=False);directory=target
        cpp=(common.ROOT/args.cpp_build).resolve();rust=(common.ROOT/args.rust_target).resolve()/'debug/examples'
        paths={'cpp':cpp/'ansa_runtime_oracle','rust':rust/'runtime_adapter','cppsession':cpp/'ansa_session_oracle','rustsession':rust/'session_adapter',**{'native-'+g:cpp/('omh_'+g+'_tests' if g in ('runtime','summary','session') else 'ansa_frontend_tests') for g in CPP}}
        source=common.source_info();check(source['worktree_status']=='','clean runtime source required')
        report.update(schema_version=1,suite='runtime',mode=args.action,source=source,platform=common.platform_info(),manifest_sha256=common.sha(MANIFEST),binaries=binary_info(paths),commands={})
        execute(sessions.Bundle(directory,paths,report['commands']),args.action,report['cases'],True)
        check(source==common.source_info() and report['binaries']==binary_info(paths),'runtime capture changed inputs')
        report['status']='PASS';report['errors']=verify(report,directory);report['status']='FAIL' if report['errors'] else 'PASS'
    except (OSError,ValueError,KeyError,TypeError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as e:report['errors'].append(str(e) if isinstance(e,ValueError) else 'runtime capture failed: '+type(e).__name__)
    if directory is not None:common.write_json(directory/'runtime-report.json',report)
    print(json.dumps({'suite':'runtime','mode':args.action,'status':report['status'],'cases':len(report['cases']),'errors':report['errors']},indent=2));return int(report['status']!='PASS')
def check_evidence(evidence,fault_evidence=None):
    errors=[]
    for mode,directory in [('compare',Path(evidence).resolve()),('fault',Path(fault_evidence).resolve() if fault_evidence else Path(str(evidence)+'-fault'))]:
        try:
            report=auth.json_bytes(protocol.owned_bytes(directory/'runtime-report.json',MAX));check(report.get('mode')==mode,'wrong runtime evidence mode');errors += [mode+': '+e for e in verify(report,directory)]
        except (OSError,ValueError,KeyError,TypeError):errors.append(mode+': missing/unsafe runtime report')
    print(json.dumps({'milestone':'M5','status':'FAIL' if errors else 'PASS','errors':errors},indent=2));return int(bool(errors))
