"""M8 full offline composition and closed M1-M7 differential corpus."""
import copy
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import auth
import protocol
import run as common
import runtime
import sessions
import session_cases as v

MAX=64*1024*1024
MANIFEST=common.ROOT/'tests/parity/fixtures/e2e.json'
COMPONENTS=[('M1','risk'),('M2','protocol'),('M3','auth'),('M4','sessions'),('M5','runtime'),('M6','tools'),('M7','frontend')]
COUNTS={'M1':(602,286),'M2':(7303,195),'M3':(220,273),'M4':(407,829),'M5':(66,114),'M6':(32,102),'M7':(120,187)}
def need(value,code):v.check(value,code)
def number(value):
    if isinstance(value,protocol.NumberToken):return runtime.integer(value)
    need(type(value) is int and 0<=value<=2**64-1,'E2E invalid integer');return value
def spec(mode='basic',turns=1,**kw):return dict(mode=mode,turns=turns,start=0,**kw)
def jobs(mode):
    if mode=='compare':
        result=[(f'pipeline-{m}-{t}',('pair',spec(m,t))) for m in ('basic','plan') for t in (1,3)]
        result += [(f'compact-{m}',('pair',spec(m,4,compact=True))) for m in ('basic','plan')]
        result += [('bounded-resume',('pair',spec(limited=True)))]
        result += [(f'switch-{m}-{w}',('switch',spec(m),w)) for m in ('basic','plan') for w in ('cpp','rust')]
        result += [(f'restart-resume-{w}',('resume',spec(limited=True),w)) for w in ('cpp','rust')]
        return result
    result=[(f'crash-{kind}-{nth}-{writer}',('crash',kind,nth,writer)) for kind in ('accepted','response','intent','outcome','continued','done') for nth in ((1,2) if kind in ('response','intent','outcome','continued') else (1,)) for writer in ('cpp','rust')]
    result += [(f'race-composition-{i}',('pair',spec('basic' if i%2 else 'plan',limited=bool(i%3==0)))) for i in range(20)]
    return result
def inventory(mode):return [name for name,_ in jobs(mode)]
def manifest():
    expected={'schema_version':1,'suite':'e2e','compare_ids':inventory('compare'),'fault_ids':inventory('fault'),'components':COMPONENTS}
    need(auth.json_bytes(protocol.owned_bytes(MANIFEST,MAX))==dict(expected,components=[list(p) for p in COMPONENTS]),'E2E manifest differs')
def identities(paths):
    need(set(paths)=={'cpp','rust'} and paths['cpp'].name=='ansa_e2e_oracle' and paths['rust'].name=='e2e_adapter','E2E binary inventory')
    return {key:{'path':str(path),'sha256':hashlib.sha256(protocol.owned_bytes(path,256*1024*1024,executable=True,single_link=True)).hexdigest()} for key,path in paths.items()}
class Context(sessions.Context):
    def call(self,adapter,request,exit=0):
        value=super().call(adapter,dict(request,root=str(self.root)),exit)
        label=self.row['id']+'-'+str(self.command_index-1);command=self.bundle.commands[label]
        need(not protocol.retained_bytes(self.bundle.directory,command['stderr'],label+'.stderr',MAX),'unexpected E2E diagnostic')
        return value
    def marker(self):
        if self.live:
            path=self.root/'home/workspace/marker';data=path.read_bytes() if path.is_file() else b''
        else:data=None
        return self.artifact('marker-'+str(self.snapshot_index),data)
def normalized(value,inc):
    if isinstance(value,dict):return {k:normalized(v,inc) for k,v in value.items() if k not in ('stream_sequence','through_sequence')}
    if isinstance(value,list):return [normalized(v,inc) for v in value]
    if isinstance(value,str):return value.replace(inc,'INCARNATION')
    return value
def canonical_tools(events,root):
    admissions={};counts={};intents={};outcomes={}
    for event in events:
        kind=event['kind']
        if kind=='accepted':
            turn=event['turn'];need(turn not in admissions,'E2E duplicate durable turn');admissions[turn]=event;counts[turn]=0
            index=len(admissions)-1
            need(turn==f'turn{index}' and event['command']==f'command{index}' and event['text']==f'fixture{index}' and event['request']==f'request_{index+1}','E2E durable admission identities')
            need(re.fullmatch('[0-9a-f]{32}_'+re.escape(event['request']),event['operation_run']) is not None,'E2E operation run provenance')
        elif kind=='intent':
            turn=event['turn'];admission=admissions[turn];offset=counts[turn];counts[turn]+=1;inv=event['invocation'];plan=admission.get('mode','basic')=='plan'
            names=['read_file'] if plan else ['read_file','run_command','read_file'];need(offset<len(names),'E2E unexpected executor')
            need(inv['name']==names[offset] and inv['call']==f"call_{admission['text']}_{offset}" and inv['operation']==f"{admission['operation_run']}:op:{offset+1}",'E2E exact invocation identity')
            descriptor=next(d for d in events[0]['configuration']['tools']['descriptors'] if d['name']==inv['name']);need(inv['revision']==descriptor['revision'],'E2E executor revision')
            token=v.digest(events[0]['configuration']['epoch']['exact_state']['instructions'])
            if offset==1:arguments={'argv':['sh','-c','printf x >> marker; printf RUN_OK; exit 7'],'cwd':str(root/'home/workspace'),'timeout_ms':3000,'instruction_token':token}
            else:arguments={'path':'value.txt' if offset==0 else 'marker','offset':0,'max_bytes':8192,'instruction_token':token}
            need(protocol.structural(inv['arguments'])==protocol.structural(arguments),'E2E fixture executor arguments')
            need(inv['operation'] not in intents,'E2E repeated operation');intents[inv['operation']]=(inv,offset,len(admissions))
        elif kind=='outcome':
            result=event['result'];operation=result['operation'];need(operation in intents and operation not in outcomes,'E2E unmatched outcome');inv,offset,index=intents[operation]
            need(result['call']==inv['call'] and number(result['execution'])==0 and number(result['effects'])==1 and result['dispatched'] is True,'E2E known outcome provenance')
            if offset==1:expected={'exit_code':7,'signal':0,'cancelled':False,'timed_out':False,'truncated':False,'stdout':'RUN_OK','stderr':'','stdout_bytes':6,'stderr_bytes':0,'spawn_errno':0,'cleanup_complete':True}
            else:
                content='SOURCE_VALUE\n' if offset==0 else 'x'*index;name='value.txt' if offset==0 else 'marker';expected={'path':str(root/'home/workspace'/name),'offset':0,'next_offset':len(content.encode()),'content':content,'truncated':False}
            need(protocol.structural(protocol.strict_loads(result['output']))==protocol.structural(expected),'E2E exact canonical tool result')
            outcomes[operation]=result
    return outcomes
def replay(events,request):
    """Independent fixture reducer: derive exact ordered callbacks from durable transitions."""
    configuration=events[0]['configuration'];epoch=configuration['epoch'];history=[];generation=0;active=None;response=None;invocation=None;results=[];next_request=None
    expected=[];operations=[];done=[];accepted=[]
    start=request['start'];stop=start+request['turns'];capturing=not request.get('resume')
    current_turn=None
    def same(a,b,message):need(protocol.structural(a)==protocol.structural(b),message)
    def make(admission):
        selected=copy.deepcopy(configuration['tools']);context=copy.deepcopy(epoch)
        if admission.get('mode','basic')=='plan':
            selected['descriptors']=[d for d in selected['descriptors'] if d['name']!='run_command']
            selected['revision']=v.digest([{'name':d['name'],'description':d['description'],'implementation_revision':d['revision'],'schema':d['schema']} for d in selected['descriptors']])
            provenance={'base_epoch':context['id'],'base_state':context['exact_state'],'base_instructions':context['instructions'],'mode':'plan','tools_revision':selected['revision']}
            context={'id':v.digest(provenance),'instructions':context['instructions']+'\nPlan mode: inspect and reason using read-only tools. Do not modify files or execute commands. Provide a concrete plan.','exact_state':provenance}
        return {'binding':configuration['binding'],'epoch':context,'tools':selected,'input':copy.deepcopy(history)+[{'role':'user','content':admission['text']}]}
    for event in events[1:]:
        kind=event['kind']
        if kind=='accepted':
            need(active is None,'E2E overlapping accepted turn');active=event;current_turn=event['turn'];response=None;invocation=None;results=[]
            next_request=make(event);same(event['next'],next_request,'E2E accepted request/history/epoch')
            index=int(event['text'].removeprefix('fixture'));scope=start<=index<stop
            if scope and capturing:accepted.append((event['command'],current_turn,event['request']))
            continue
        need(active is not None and event['turn']==current_turn,'E2E canonical transition turn')
        index=int(active['text'].removeprefix('fixture'));scope=start<=index<stop
        if kind=='compacted':
            value=event['compaction'];same(value['request'],{'binding':configuration['binding'],'epoch':epoch,'tools':configuration['tools'],'input':history},'E2E compaction request')
            native=value['native'];checksum=v.digest(native);generation+=1;base=configuration['epoch']
            epoch={'id':f'compact_{generation}_{checksum[:16]}','instructions':base['instructions'],'exact_state':{'base':base['exact_state'],'base_id':base['id'],'compaction_generation':generation,'compaction_digest':checksum}}
            history=native['output'];next_request=make(active)
        elif kind=='response':
            same(event['response']['request'],next_request,'E2E exact native continuation/results')
            response=event['response'];results=[];invocation=None
            last_user=max(i for i,item in enumerate(next_request['input']) if item.get('role')=='user')
            step=len([item for item in next_request['input'][last_user+1:] if item.get('type')=='function_call_output']);plan=active.get('mode','basic')=='plan';label=active['text']
            need(set(response)=={'request','id','model','items'} and response['id']==f'response_{label}_{step}' and response['model']==configuration['binding']['returned_model'],'E2E response returned model/identity')
            if step==(1 if plan else 3):
                item={'type':'message','id':f'message_{label}','role':'assistant','status':'completed','content':[{'type':'output_text','text':'PLAN_OK' if plan else 'E2E_OK:exit7'}]}
                same(response['items'],[item],'E2E terminal native message')
            else:
                need(len(response['items'])==1,'E2E native call inventory');item=response['items'][0]
                expected_item={'type':'function_call','id':f'item_{label}_{step}','call_id':f'call_{label}_{step}','namespace':'omh','name':'run_command' if step==1 else 'read_file','arguments':item.get('arguments'),'status':'completed'}
                same(item,expected_item,'E2E native call identity')
            if scope and capturing:expected.append(copy.deepcopy(next_request))
        elif kind=='intent':
            need(response is not None and invocation is None,'E2E intent order');invocation=event['invocation']
            calls=[i for i in response['items'] if i.get('type')=='function_call'];need(len(calls)==1,'E2E native batch inventory');call=calls[0]
            need(invocation['call']==call['call_id'] and invocation['name']==call['name'],'E2E intent/call binding');same(invocation['arguments'],protocol.strict_loads(call['arguments']),'E2E intent arguments')
        elif kind=='outcome':
            need(invocation is not None and event['result']['operation']==invocation['operation'] and event['result']['call']==invocation['call'],'E2E outcome intent identity')
            results.append(event['result']);invocation=None
            if scope and capturing:operations.append(event['result']['operation'])
        elif kind=='continued':
            need(response is not None and len(results)==1 and invocation is None,'E2E incomplete continuation')
            next_request=copy.deepcopy(response['request']);next_request['input']+=response['items']+[{'type':'function_call_output','call_id':r['call'],'output':r['output']} for r in results]
        elif kind=='pause':
            need(event['diagnostic']=='tool_round_limit','E2E unexpected pause')
            if scope and capturing:expected.append(copy.deepcopy(next_request))
        elif kind=='resume':
            if request.get('resume') and event['command']==f'restart{start}':capturing=True;accepted.append((event['command'],current_turn,active['request']))
        elif kind=='done':
            need(response is not None and all(i.get('type')=='message' for i in response['items']) and invocation is None,'E2E premature Done')
            history=response['request']['input']+response['items']
            if scope and capturing:done.append(current_turn)
            active=None
        else:need(False,'E2E unexpected canonical event')
    return expected,operations,done,accepted
def validate(value,request,events,root,*,complete=True):
    need(isinstance(value,dict) and set(value)=={'events','requests','snapshot','generation_count','summary_count','resume_count','plan'},'E2E output fields')
    mode=request['mode'];turns=request['turns'];start=request['start'];plan=mode=='plan';calls=1 if plan else 3
    need(value['plan'] is plan,'E2E mode')
    snapshot=value['snapshot'];inc=snapshot['incarnation'];need(re.fullmatch('[0-9a-f]{32}',inc) is not None,'E2E incarnation')
    need(number(snapshot['schema_version'])==1,'E2E wire schema')
    need(snapshot['session']=='e2e' and snapshot['worker_busy'] is False and snapshot['storage_failed'] is False and snapshot['durable'] is True,'E2E final durability')
    need(events and events[0]['kind']=='init' and events[0]['session']=='e2e','E2E canonical initialization')
    canonical_tools(events,root)
    binding={'provider':'openai-siwc','account':'offline-fixture','requested_model':'gpt-6.1-sol','returned_model':'gpt-6.1-sol'}
    need(protocol.structural(events[0]['configuration']['binding'])==protocol.structural(binding),'E2E binding')
    requests=value['requests'];need(number(value['generation_count'])==len(requests),'E2E generation evidence')
    expected_requests,expected_operations,expected_done,expected_accepted=replay(events,request)
    need(protocol.structural(requests)==protocol.structural(expected_requests),'E2E exact ordered production callbacks')
    responses=[e for e in events if e['kind']=='response']
    for actual in requests:
        need(protocol.structural(actual['binding'])==protocol.structural(binding),'E2E request provenance')
    known={e['result']['operation']:e['result'] for e in events if e['kind']=='outcome'}
    def known_tool(tool):
        result=known.get(tool['operation_id']);need(result is not None and tool['call_id']==result['call'] and tool['output']==result['output'] and tool['effects']=='known_result' and tool['execution']=='succeeded','E2E wire/canonical result binding')
    for tool in snapshot['tools']:known_tool(tool)
    need(len(value['events'])>0,'empty E2E wire events');last_sequence=0
    for event in value['events']:
        sequence=number(event['stream_sequence']);need(sequence>last_sequence and event['incarnation']==inc and event['session']=='e2e','E2E event cursor/incarnation');last_sequence=sequence
        if event['type']=='tool_finished':known_tool(event)
    wire=value['events']
    need([e['operation_id'] for e in wire if e['type']=='tool_started']==expected_operations,'E2E missing/reordered ToolStarted')
    need([e['operation_id'] for e in wire if e['type']=='tool_finished']==expected_operations,'E2E missing/reordered ToolFinished')
    need([e['turn'] for e in wire if e['type']=='done']==expected_done,'E2E missing/reordered wire Done')
    for command,turn,target in expected_accepted:need(any(e['type']=='command_accepted' and e['command_id']==command and e['turn']==turn and e['request_id']==target for e in wire),'E2E missing admission event')
    final_text='PLAN_OK' if plan else 'E2E_OK:exit7'
    need([(e['turn'],e['text']) for e in wire if e['type']=='text_delta']==[(turn,final_text) for turn in expected_done],'E2E wire final text')
    for event in wire:
        if event['type']=='text_preview':need(event['text'] in ('','E2E_PREVIEW',final_text),'E2E wire preview text')
        if event['type']=='snapshot':need(event['text'] in ('',final_text),'E2E snapshot text')
    if complete:need(snapshot['preview']['text']==final_text and snapshot['preview']['state']=='committed','E2E final preview binding')
    if not complete:
        need(snapshot['state']=='paused' and snapshot['resume_available'] is True and number(value['generation_count'])==2,'E2E bounded pause')
        need([e['kind'] for e in events]==['init','accepted','response','intent','outcome','continued','pause'],'E2E pending durable inventory')
        body=protocol.strict_loads(events[4]['result']['output']);need(body['path']==str(root/'home/workspace/value.txt') and body['content']=='SOURCE_VALUE\n' and body['truncated'] is False,'E2E pending source read')
        return None
    need(snapshot['state']=='completed' and snapshot['text']==('PLAN_OK' if plan else 'E2E_OK:exit7') and snapshot['diagnostic']=='' and snapshot['resume_available'] is False,'E2E completion')
    need(number(snapshot['completed_tools'])==calls and number(snapshot['transcript_size'])==2*(start+turns),'E2E transcript/tools')
    if not request.get('resume'):need(len(requests)==turns*(calls+1+(calls-1 if request.get('limited') else 0)),'E2E exact request count')
    need(number(value['summary_count'])==(1 if request.get('compact') and turns==4 else 0),'E2E exact summary count')
    done=[e for e in events if e['kind']=='done'];accepted=[e for e in events if e['kind']=='accepted'];need(len(done)==len(accepted)==start+turns,'E2E durable turn inventory')
    expected_names=['read_file'] if plan else ['read_file','run_command','read_file']
    for index,admission in enumerate(accepted):
        need(admission['text']==f'fixture{index}' and admission.get('mode','basic')==mode,'E2E accepted text/mode')
        turn=admission['turn'];intents=[e for e in events if e['kind']=='intent' and e['turn']==turn];outcomes=[e for e in events if e['kind']=='outcome' and e['turn']==turn]
        need([e['invocation']['name'] for e in intents]==expected_names and len(outcomes)==calls,'E2E validated batch order')
        for offset,(intent,outcome) in enumerate(zip(intents,outcomes)):
            invocation=intent['invocation'];result=outcome['result'];operation=f"{admission['operation_run']}:op:{offset+1}"
            need(invocation['operation']==result['operation']==operation and invocation['call']==result['call']==f'call_fixture{index}_{offset}' and result['dispatched'] is True and number(result['effects'])==1,'E2E exact operation identity/effects')
            body=protocol.strict_loads(result['output'])
            if offset==1:
                need(number(body['exit_code'])==7 and body['stdout']=='RUN_OK' and body['stderr']=='' and body['cleanup_complete'] is True and body['cancelled'] is False and body['timed_out'] is False,'E2E known command failure/cleanup')
            else:
                name='value.txt' if offset==0 else 'marker';content='SOURCE_VALUE\n' if offset==0 else 'x'*(index+1)
                need(body['path']==str(root/'home/workspace'/name) and body['content']==content and body['truncated'] is False,'E2E full file contents')
        need(len([e for e in responses if e['turn']==turn])==calls+1,'E2E per-turn native response inventory')
    selected={key:snapshot[key] for key in ('state','text','completed_tools','history_items','compactions','storage_failed','durable','resume_available','mode','transcript_size','tools')}
    return protocol.structural(normalized({'snapshot':selected,'requests':requests,'generation_count':value['generation_count'],'summary_count':value['summary_count'],'resume_count':value['resume_count']},inc))
def run_case(c,job):
    kind=job[0]
    if kind=='pair':
        request=job[1];values=[]
        for writer in ('cpp','rust'):
            value=sessions.accepted(c.call(writer,request));events=v.decode(sessions.parts(c.shot()));values.append(validate(value,request,events,c.root));need(c.marker()==(b'' if request['mode']=='plan' else b'x'*request['turns']),'E2E actual effect count')
            if c.live and writer=='cpp':
                for name in ('journal','home','auth'):
                    path=c.root/name
                    if path.exists():shutil.rmtree(path)
        need(values[0]==values[1],'E2E C++/Rust composition differs')
    elif kind in ('switch','resume'):
        request=job[1];writer=job[2];other='rust' if writer=='cpp' else 'cpp';first=dict(request,**({'pause':True} if kind=='resume' else {}))
        value=sessions.accepted(c.call(writer,first));before=sessions.parts(c.shot());validate(value,first,v.decode(before),c.root,complete=kind!='resume')
        if kind=='resume':need(c.marker()==b'','E2E paused budget dispatched process')
        next_request=dict(request,**({'resume':True} if kind=='resume' else {'start':1}))
        value=sessions.accepted(c.call(other,next_request));after=sessions.parts(c.shot());need(all(after[name].startswith(data) for name,data in before.items()),'E2E binary switch overwrote state');validate(value,next_request,v.decode(after),c.root)
        need(c.marker()==(b'' if request['mode']=='plan' else b'x'*(1 if kind=='resume' else 2)),'E2E recovery repeated process effect')
    else:
        _,target,nth,writer=job;request=spec(fault={'kind':target,'occurrence':nth});c.call(writer,request,exit=71);before=sessions.parts(c.shot());events=v.decode(before)
        expected=['init','accepted','response','intent','outcome','continued','response','intent','outcome','continued','response','intent','outcome','continued','response','done'];positions=[i for i,x in enumerate(expected) if x==target];need([e['kind'] for e in events]==expected[:positions[nth-1]+1],'E2E crash canonical boundary')
        known=canonical_tools(events,c.root);replay(events,spec())
        other='rust' if writer=='cpp' else 'cpp';value=sessions.accepted(c.call(other,spec(turns=0)));after=sessions.parts(c.shot());need(before==after,'E2E read/reopen changed canonical bytes');need(number(value['generation_count'])==number(value['summary_count'])==0 and value['snapshot']['worker_busy'] is False,'E2E crash auto-dispatch')
        for tool in value['snapshot']['tools']:
            if 'execution' in tool:
                result=known.get(tool['operation_id']);need(result is not None and tool['call_id']==result['call'] and tool['output']==result['output'] and tool['effects']=='known_result','E2E reopened known result')
        need(c.marker()==(b'x' if positions[nth-1]>=8 else b''),'E2E crash effect count')
def execute(bundle,mode,rows,live):
    need(live or [r['id'] for r in rows]==inventory(mode),'E2E closed inventory')
    for index,(name,job) in enumerate(jobs(mode)):
        row={'id':name,'status':'running'} if live else rows[index];context=Context(bundle,row,live)
        if live:rows.append(row)
        try:
            run_case(context,job);context.close()
            if live:row['status']='pass'
        except BaseException:
            if live:row['status']='fail'
            raise
        finally:
            if live:context.temporary.cleanup()
        need(row['status']=='pass','failed/skipped E2E case')
    need(bundle.used==set(bundle.commands),'E2E unused commands')
def verify(report,directory):
    try:
        manifest();need(report['schema_version']==1 and report['suite']=='e2e' and report['mode'] in ('compare','fault'),'E2E report identity')
        need(report['manifest_sha256']==common.sha(MANIFEST) and report['source']['files']==common.source_info()['files'] and report['source']['worktree_status']=='','E2E source identity')
        platform=report['platform'];need(platform['machine']=='aarch64' and platform['os_release']['ID']=='nixos','E2E native platform')
        paths={key:Path(value['path']) for key,value in report['binaries'].items()};need(report['binaries']==identities(paths),'E2E binaries')
        need(set(report['components'])=={milestone for milestone,_ in COMPONENTS},'E2E component inventory')
        for milestone,suite in COMPONENTS:
            command=report['components'][milestone];child=directory/'components'/(milestone+('-fault' if report['mode']=='fault' else ''))
            argv=[sys.executable,str(common.ROOT/'tests/parity/run.py'),report['mode'],'--suite',suite,'--cpp-build',str(paths['cpp'].parent),'--rust-target',str(paths['rust'].parents[2]),'--output',str(child)]
            need(auth.command_ok(command) and command['argv']==argv and command['cwd']==str(common.ROOT) and 'process_env' not in command,'E2E component invocation')
            out=protocol.retained_bytes(directory,command['stdout'],'component-'+milestone+'.stdout',MAX);err=protocol.retained_bytes(directory,command['stderr'],'component-'+milestone+'.stderr',MAX)
            summary=auth.json_bytes(out);need(not err and summary['suite']==suite and summary['mode']==report['mode'] and summary['status']=='PASS' and summary['errors']==[] and summary['cases']==COUNTS[milestone][int(report['mode']=='fault')],'E2E component result')
        execute(sessions.Bundle(directory,paths,report['commands']),report['mode'],report['cases'],False);need(report['status']=='PASS' and not report['errors'],'E2E report status');return []
    except (OSError,ValueError,KeyError,TypeError,IndexError,AttributeError,subprocess.SubprocessError) as error:return [str(error)]
def capture(args):
    directory=Path(args.output).resolve();directory.mkdir(parents=True,exist_ok=False);report={'status':'FAIL','errors':[],'cases':[]};bundle=None
    try:
        manifest();source=common.source_info();need(source['worktree_status']=='','clean E2E checkpoint required')
        cpp=Path(args.cpp_build).resolve();rust=Path(args.rust_target).resolve();paths={'cpp':cpp/'ansa_e2e_oracle','rust':rust/'debug/examples/e2e_adapter'}
        report.update(schema_version=1,suite='e2e',mode=args.action,source=source,platform=common.platform_info(),manifest_sha256=common.sha(MANIFEST),binaries=identities(paths))
        bundle=sessions.Bundle(directory,paths,{});execute(bundle,args.action,report['cases'],True);report['commands']=bundle.commands
        for milestone,suite in COMPONENTS:
            child=directory/'components'/(milestone+('-fault' if args.action=='fault' else ''))
            result=common.command([sys.executable,common.ROOT/'tests/parity/run.py',args.action,'--suite',suite,'--cpp-build',cpp,'--rust-target',rust,'--output',child],common.ROOT,directory,'component-'+milestone,timeout=1800)
            report.setdefault('components',{})[milestone]=result;need(auth.command_ok(result),'E2E component '+milestone+' failed')
        need(source==common.source_info(),'E2E inputs changed');report['status']='PASS'
    except (OSError,ValueError,KeyError,TypeError,subprocess.SubprocessError) as error:report['errors'].append(str(error))
    if bundle is not None:report['commands']=bundle.commands
    common.write_json(directory/'e2e-report.json',report);print(json.dumps({'suite':'e2e','mode':args.action,'status':report['status'],'cases':len(report['cases']),'errors':report['errors']},indent=2));return int(report['status']!='PASS')
def check_evidence(compare,fault=None):
    compare=Path(compare).resolve();fault=Path(fault).resolve() if fault else Path(str(compare)+'-fault');errors=[]
    for mode,directory in [('compare',compare),('fault',fault)]:
        try:
            report=auth.json_bytes(protocol.owned_bytes(directory/'e2e-report.json',MAX));need(report['mode']==mode,'E2E mode');errors += [mode+': '+error for error in verify(report,directory)]
        except (OSError,ValueError,KeyError):errors.append(mode+': missing E2E report')
    for milestone,_ in COMPONENTS:
        result=subprocess.run([sys.executable,common.ROOT/'tests/parity/run.py','check','--milestone',milestone,'--evidence',compare/'components'/milestone,'--fault-evidence',fault/'components'/(milestone+'-fault')],cwd=common.ROOT,timeout=180)
        if result.returncode:errors.append('E2E component checker '+milestone)
    print(json.dumps({'milestone':'M8','status':'FAIL' if errors else 'PASS_FOR_OFFLINE_CORPUS','errors':errors,'separate_gates':['performance budgets','nightly quality','native full matrix','independent review']},indent=2));return int(bool(errors))
