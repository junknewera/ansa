"""Independent replay of retained performance observations, work and distributions."""
import hashlib
import math
from pathlib import Path
import re
import statistics
import shutil
import subprocess
import sys
import tempfile
import tomllib
import os
import stat
import auth
import e2e
import protocol
import run as common
import session_cases as journal

COUNTS=(0,100,4096)
SAMPLES=20
WIRE_SHA256='58b180870f2cb319fb85c9882a0e1ca022b1fbf48cd5ccf5b4e28b463e9ef137'
BINDING={'provider':'openai-siwc','account':'offline-fixture','requested_model':'gpt-6.1-sol','returned_model':'gpt-6.1-sol'}
def need(value,message):
    if not value:raise ValueError('perf evidence: '+message)
def same(a,b,message):need(protocol.structural(a)==protocol.structural(b),message)
def integer(value,minimum=0):
    need(type(value) is int and minimum<=value<2**64,'integer/boolean/range');return value
def number(value):
    if isinstance(value,protocol.NumberToken):
        need(value.integer and re.fullmatch('0|[1-9][0-9]*',value.lexeme),'canonical integer');return integer(int(value.lexeme))
    return integer(value)
def distribution(values):
    need(values and all(type(v) in (int,float) and math.isfinite(v) and v>=0 for v in values),'finite positive measurements')
    ordered=sorted(values)
    return {'count':len(values),'min':ordered[0],'median':statistics.median(values),'p95_nearest_rank':ordered[math.ceil(len(values)*.95)-1],'max':ordered[-1]}
def raw(directory,item,expected=None,maximum=256*1024*1024):
    name=item.get('path',item.get('file'));path=Path(name)
    need(isinstance(name,str) and not path.is_absolute() and '..' not in path.parts and not any(p=='.env' or p.startswith('.env.') for p in path.parts),'artifact path')
    need(expected is None or name==expected,'artifact inventory/name')
    data=protocol.owned_bytes(directory/path,maximum,single_link=True)
    need(hashlib.sha256(data).hexdigest()==item['sha256'],'artifact digest');return data
def value(directory,item,expected=None):return auth.json_bytes(raw(directory,item,expected))
def binary_hash(path,name):
    path=Path(path);need(path.name==name,'binary name/inventory')
    return hashlib.sha256(protocol.owned_bytes(path,256*1024*1024,executable=True)).hexdigest()
def source_hash(path):
    # Immutable Nix files may share store hardlinks. Still reject aliases,
    # special files and forbidden components before reading any bytes.
    path=Path(path);need(str(path).startswith('/nix/store/') and not any(p=='.env' or p.startswith('.env.') for p in path.parts) and path==path.resolve(),'Nix source path')
    with os.fdopen(os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC),'rb') as stream:
        meta=os.fstat(stream.fileno());need(stat.S_ISREG(meta.st_mode) and meta.st_size<=64*1024*1024,'Nix source file type/size');return hashlib.sha256(stream.read(64*1024*1024+1)).hexdigest()
CPP_PACKAGE_SELECTOR='''packages:
  if packages ? ansa-cpp then packages.ansa-cpp.drvPath
  else if packages.default ? cmakeFlags
    && builtins.elem "-DBUILD_TESTING=ON" packages.default.cmakeFlags
  then packages.default.drvPath
  else throw "No C++ reference package: use a frozen pre-M10 revision"'''
def expected_derivation(language,head):
    need(language in ('cpp','rust') and re.fullmatch('[0-9a-f]{40}',head),'pinned package source revision')
    reference='git+file://'+str(common.ROOT)+'?rev='+head+'#packages.aarch64-linux'
    suffix=['--apply',CPP_PACKAGE_SELECTOR] if language=='cpp' else []
    if language=='rust':reference+='.ansa-rust.drvPath'
    return subprocess.check_output(['nix','eval','--no-update-lock-file','--raw',reference,*suffix],stderr=subprocess.PIPE,text=True,timeout=90).strip()
def installed_source(binary,language,files,head):
    package=str(Path(binary).parent.parent);need(package.startswith('/nix/store/'),'installed source package')
    derivation=subprocess.check_output(['nix-store','--query','--deriver',package],text=True,timeout=30).strip()
    need(derivation.startswith('/nix/store/') and derivation.endswith('.drv'),'known package derivation')
    need(derivation==expected_derivation(language,head),'installed '+language+' does not use the pinned Release derivation')
    result=auth.json_bytes(subprocess.check_output(['nix','derivation','show',derivation],timeout=30));entries=result.get('derivations',result)
    need(len(entries)==1 and next(iter(entries)) in (derivation,Path(derivation).name),'actual derivation')
    item=next(iter(entries.values()));output=item['outputs']['out']['path'];output=output if output.startswith('/') else '/nix/store/'+output
    need(output==package and item['system']=='aarch64-linux','derivation output/target');source=Path(item['env']['src']);inputs={}
    for name,digest in files.items():
        if language=='rust' and name.startswith('rust/'):relative=name[5:]
        elif language=='cpp' and (name.startswith(('src/','include/')) or name in ('CMakeLists.txt','flake.nix','flake.lock')):relative=name
        else:continue
        actual=source_hash(source/relative);need(actual==digest,'installed '+language+' source differs: '+name);inputs[relative]=actual
    need(inputs,'empty installed source proof')
    return {'derivation':derivation,'source':str(source),'output':output,'inputs':inputs}
def nix_info(data):
    if 'info' in data:
        need(data['storeDir']=='/nix/store','Nix store directory');entries=data['info']
        return {key if key.startswith('/') else '/nix/store/'+key:value for key,value in entries.items()}
    return data.get('paths',data)
def command(directory,row,binary,request=None):
    need(row['argv']==[binary] and type(row['exit_code']) is int and row['exit_code']==0,'actual adapter invocation')
    need(not raw(directory,row['stderr']),'unexpected diagnostic')
    if request is not None:same(value(directory,row['stdin']),request,'actual request')
    result=value(directory,row['stdout']);need(set(result)=={'accepted','value'} and result['accepted'] is True,'admitted actual output');return result['value']
def crossing(trace,data,text,offset=0):
    if isinstance(text,str):text=text.encode()
    index=data.find(text,offset);need(index>=0,'missing measured output')
    return next(r['at_ns'] for r in trace['reads'] if r['end']>=index+len(text))
def observation(directory,row,argv):
    data=raw(directory,row['raw']);observed=value(directory,row['observation']);trace=observed['trace']
    need(trace['argv']==argv,'PTY actual argv')
    integer(trace['popen_pid'],2);integer(trace['parent_pid'],2)
    need(trace['popen_pid']!=trace['parent_pid'] and trace['popen_identity'][0]==trace['parent_pid'],'actual Popen identity');integer(trace['popen_identity'][1],1)
    start=integer(trace['start_ns'],1);end=integer(trace['exit_ns'],start)
    need(type(trace['exit_code']) is int and trace['exit_code']==0 and trace['termios_restored'] is True and trace['flags_restored'] is True,'terminal restoration/exit')
    need(row['exit_code']==0 and row['termios_restored'] is True and row['flags_restored'] is True,'report terminal status')
    offset=0;clock=start
    for item in trace['reads']:
        need(integer(item['begin'])==offset and integer(item['end'],offset+1)<=len(data),'PTY read coverage')
        clock=integer(item['at_ns'],clock);need(clock<=end,'PTY read clock');offset=item['end']
    need(offset==len(data) and offset>0,'incomplete PTY read trace')
    clock=start
    boundaries={0:start,**{r['end']:r['at_ns'] for r in trace['reads']}}
    for sent in trace['sends']+trace['signals']:
        stamp=integer(sent['at_ns'],start);offset=integer(sent['offset']);need(offset in boundaries,'PTY input read boundary')
        after=next((r['at_ns'] for r in trace['reads'] if r['begin']==offset),end)
        need(boundaries[offset]<=stamp<=after,'PTY input clock outside read timeline')
    for sent in trace['sends']:
        clock=integer(sent['at_ns'],clock);bytes.fromhex(sent['bytes_hex'])
    need(trace['rss_bytes'] and all(type(n) is int and n>0 for n in trace['rss_bytes']) and row['rss_bytes']==max(trace['rss_bytes']),'native RSS observation')
    return data,trace,observed['process']
def sent(trace,data,text):
    encoded=text.encode() if isinstance(text,str) else text
    rows=[r for r in trace['sends'] if r['bytes_hex']==encoded.hex()];need(len(rows)==1,'exact measured input')
    return rows[0]
def latency(trace,data,text,write):return (crossing(trace,data,text,write['offset'])-write['at_ns'])/1e9
def seed(directory,entry,count,binary,kind):
    root=Path(entry['root']);need(root.parent==Path('/tmp') and root.name.startswith('ansa-m4-'),'owned synthetic root')
    request={'root':str(root),'mode':'plan','turns':count,'start':0,'quiet':True,'compact':True}
    output=command(directory,entry['seed'],binary,request)
    need(integer(output['generation_count'])==2*count and integer(output['summary_count'])==max(0,(count-2)//2),'seed exact callbacks')
    need(number(output['snapshot']['transcript_size'])==2*count,'seed transcript')
    if kind=='startup':parts={name:raw(directory,{'path':f'datasets/{entry["submits"]}/{name}','sha256':digest}) for name,digest in entry['parts'].items()}
    else:parts={name:raw(directory,item) for name,item in entry['parts'].items()}
    records=journal.decode(parts)
    need(records and records[0]['kind']=='init' and records[0]['session']=='e2e','seed canonical init');same(records[0]['configuration']['binding'],BINDING,'seed model/account')
    expected={'init':1,'accepted':count,'response':2*count,'intent':count,'outcome':count,'continued':count,'done':count,'compacted':max(0,(count-2)//2)}
    need(set(r['kind'] for r in records)<=set(expected) and all(sum(r['kind']==k for r in records)==v for k,v in expected.items()),'seed durable inventory')
    need(all(r.get('mode')=='plan' for r in records if r['kind']=='accepted'),'seed mode')
    e2e.canonical_tools(records,root)
    # Validate every production continuation/epoch/compact, without retaining an
    # O(count) copy of expected callback inputs that this quiet seed omits.
    e2e.replay(records,{'start':count,'turns':0})
    return parts,records
def startup(report,directory,binaries,adapter,counts=COUNTS,samples=SAMPLES):
    datasets=report['datasets'];need([d['submits'] for d in datasets]==list(counts),'startup datasets')
    by_count={d['submits']:d for d in datasets}
    for entry in datasets:seed(directory,entry,entry['submits'],adapter,'startup')
    expected=[(count,index,language) for count in counts for index in range(samples) for language in ('cpp','rust')]
    need([(r['submits'],r['sample'],r['language']) for r in report['runs']]==expected,'startup sample inventory/order')
    summaries={}
    for row in report['runs']:
        need(row['status']=='PASS','startup failed');root=Path(by_count[row['submits']]['root']);cwd=root/'home/workspace'
        argv=[binaries[row['language']],'--offline','--cwd',str(cwd),'--session','e2e','--auth-dir',str(root/'auth')]
        data,trace,process=observation(directory,row,argv);need(not process and not trace['signals'],'startup dispatched/cancelled')
        ready=crossing(trace,data,'готов');duration=(ready-trace['start_ns'])/1e9
        if row['submits']:
            last=f'fixture{row["submits"]-1}'.encode();need(data.find(last)<data.find('готов'.encode()) and data.find(last)>=0,'history before first input')
        probe=sent(trace,data,b'INPUT_PROBE');need(probe['at_ns']>=ready,'input before first history frame')
        for field in ('first_frame_seconds','history_ready_seconds'):need(row[field]==duration,'startup clock differs')
        need(row['input_ack_seconds']==latency(trace,data,'INPUT_PROBE',probe),'input clock differs')
    for language in ('cpp','rust'):
        for count in counts:
            rows=[r for r in report['runs'] if r['language']==language and r['submits']==count]
            summaries[f'{language}-{count}']={f:distribution([r[f] for r in rows]) for f in ('first_frame_seconds','history_ready_seconds','input_ack_seconds','rss_bytes')}
    same(report['summary'],summaries,'startup distributions');return summaries
def pipeline_records(prior,events,row):
    kind=row['kind'];prefix=max(0,row['submits']-1);root=Path(row['root']);work=root/'home/workspace'/row['scope']
    kinds=['accepted']+['response','intent','outcome','continued']*3+['response','done'] if kind=='turn' else ['accepted','response','intent','outcome','continued','response','intent','cancel','outcome']
    need([e['kind'] for e in events]==kinds,'pipeline durable inventory')
    admission=events[0];turn=admission['turn'];run=admission['operation_run'];target=admission['request']
    need(admission['mode']=='basic' and admission['text']==f'fixture{prefix}' and target==f'request_{prefix+1}' and number(admission['request_number'])==prefix+1,'pipeline admission/mode')
    need(re.fullmatch('[0-9a-f]{32}_'+re.escape(target),run) and re.fullmatch('c_[0-9a-f]{12}_[0-9]+',admission['command']) and turn=='t_'+admission['command'][2:],'pipeline command/operation identities')
    need(all(e['turn']==turn for e in events),'pipeline turn changed')
    tools=prior[0]['configuration']['tools']['descriptors'];token=journal.digest(prior[0]['configuration']['epoch']['exact_state']['instructions'])
    intents=[e['invocation'] for e in events if e['kind']=='intent'];outcomes=[e['result'] for e in events if e['kind']=='outcome'];responses=[e['response'] for e in events if e['kind']=='response']
    next_request=admission['next']
    # The existing independent reducer establishes exact replay/compacted epoch
    # into the acceptance; cancel adds no continuation or automatic generation.
    all_events=prior+events if kind=='turn' else prior+events[:7]
    e2e.replay(all_events,{'start':prefix,'turns':1})
    for index,(inv,result,response) in enumerate(zip(intents,outcomes,responses)):
        name='run_command' if index==1 else 'read_file';operation=f'{run}:op:{index+1}';call=f'call_fixture{prefix}_{index}'
        descriptor=next(d for d in tools if d['name']==name)
        if name=='read_file':args={'path':row['scope']+('/value.txt' if index==0 else '/marker'),'offset':0,'max_bytes':8192,'instruction_token':token}
        else:args={'argv':['sh','-c','printf x >> marker; printf RUN_OK; exit 7' if kind=='turn' else "printf x >> marker; printf '%s' $$ > primary; printf ready > ready; exec sleep 30"],'cwd':str(work),'timeout_ms':3000 if kind=='turn' else 30000,'instruction_token':token}
        same(inv,{'operation':operation,'call':call,'name':name,'revision':descriptor['revision'],'arguments':args},'pipeline exact executor')
        same(response['request'],next_request,'pipeline exact continuation')
        item={'type':'function_call','id':f'item_fixture{prefix}_{index}','call_id':call,'name':name,'namespace':'omh','arguments':None,'status':'completed'}
        need(len(response['items'])==1,'pipeline native batch');item['arguments']=response['items'][0]['arguments'];same(response['items'],[item],'pipeline native identity');same(protocol.strict_loads(item['arguments']),args,'native executor arguments')
        need(result['operation']==operation and result['call']==call and number(result['execution'])==0 and number(result['effects'])==1 and result['dispatched'] is True,'pipeline outcome provenance')
        body=auth.json_bytes(result['output'].encode())
        if name=='read_file':
            content='SOURCE_VALUE\n' if index==0 else 'x';expected={'path':str(work/('value.txt' if index==0 else 'marker')),'offset':0,'next_offset':len(content),'content':content,'truncated':False}
        else:
            expected={'exit_code':7 if kind=='turn' else -1,'signal':0 if kind=='turn' else body.get('signal'),'cancelled':kind=='cancel','timed_out':False,'truncated':False,'stdout':'RUN_OK' if kind=='turn' else '','stderr':'','stdout_bytes':6 if kind=='turn' else 0,'stderr_bytes':0,'spawn_errno':0,'cleanup_complete':True}
            if kind=='cancel':need(type(body['signal']) is int and body['signal'] in (9,15),'cancel process signal')
        same(body,expected,'pipeline exact known result')
        next_request=dict(response['request'],input=response['request']['input']+response['items']+[{'type':'function_call_output','call_id':call,'output':result['output']}])
    need(row['marker_hex']=='78','physical side effect once')
def pipeline(report,directory,adapters,counts=COUNTS,samples=SAMPLES):
    datasets=report['datasets'];need([d['submits'] for d in datasets]==list(counts),'pipeline datasets');seeds={}
    for entry in datasets:
        need(entry['prefix_submits']==max(0,entry['submits']-1),'pipeline cap/prefix');seeds[entry['submits']]=seed(directory,entry,entry['prefix_submits'],adapters['cpp'],'pipeline')
    expected=[(count,index,kind,language) for count in counts for index in range(samples) for kind in ('turn','cancel') for language in ('cpp','rust')]
    need([(r['submits'],r['sample'],r['kind'],r['language']) for r in report['runs']]==expected,'pipeline sample inventory/order')
    for row in report['runs']:
        need(row['status']=='PASS','pipeline failed');count=row['submits'];prefix=max(0,count-1);scope=f'metrics_{count}_{row["sample"]}_{row["kind"]}_{row["language"]}'
        need(row['scope']==scope and row['root']==next(d['root'] for d in datasets if d['submits']==count),'pipeline scope/root')
        data,trace,process=observation(directory,row,[adapters[row['language']],'--e2e-frontend',row['root'],row['kind'],scope])
        submit=sent(trace,data,f'fixture{prefix}\r')
        if prefix:
            mode=sent(trace,data,'/basic\r');need(crossing(trace,data,'basic  готов',mode['offset'])<=submit['at_ns'],'basic before measured Submit')
        base,prior=seeds[count];parts={}
        need(set(base)<=set(row['journal_deltas']),'lost seed part')
        for name,item in row['journal_deltas'].items():
            initial=base.get(name,b'');need(item['prefix_bytes']==len(initial),'prefix byte count')
            parts[name]=initial+raw(directory,item,scope+'-'+name+'.delta');need(item['after_bytes']==len(parts[name]) and item['after_sha256']==hashlib.sha256(parts[name]).hexdigest(),'append digest/size')
        events=journal.decode(parts)[len(prior):];pipeline_records(prior,events,row);need(row['event_kinds']==[e['kind'] for e in events],'event summary')
        footers=re.findall(rb'(?:\r?\n)M8_METRICS (\{[^\r\n]+\})',data);need(len(footers)==1,'native metric footer inventory')
        native=auth.json_bytes(footers[0]);same(row['native'],native,'native metric raw footer')
        need(len(re.findall(rb'(?:\r?\n)M8_METRICS ',data))==1 and integer(native['exit'])==0 and integer(native['summary_count'])==0,'native metric inventory')
        syncs=native['metrics']['fsync_ns'];need(len(syncs)==len(events) and all(type(n) is int and 0<n<600*10**9 for n in syncs),'fsync work/count')
        need(row['fsync_seconds']==distribution([n/1e9 for n in syncs])['p95_nearest_rank'],'fsync observation')
        if row['kind']=='turn':
            need(not process and not trace['signals'] and integer(native['generation_count'])==4,'turn callbacks/signals')
            done=native['metrics']['terminal_durable_ns'];need(len(done)==1 and integer(done[0],1)<600*10**9,'terminal clock inventory')
            need(row['terminal_durable_done_seconds']==done[0]/1e9 and row['submit_preview_seconds']==latency(trace,data,'E2E_PREVIEW',submit),'turn metric clocks')
        else:
            need(integer(native['generation_count'])==2 and native['metrics']['terminal_durable_ns']==[],'cancel callbacks')
            need(len(trace['signals'])==1 and trace['signals'][0]['number']==2,'actual Cancel signal');sig=trace['signals'][0]
            need(process['popen_pid']==trace['popen_pid'] and process['parent_pid']==trace['parent_pid'],'cleanup actual Popen')
            ancestry=process['ancestry'];need(2<=len(ancestry)<=6,'cleanup ancestry inventory');ids=set()
            need(int(bytes.fromhex(process['primary_bytes_hex']))==ancestry[0]['pid'],'actual primary process')
            for index,item in enumerate(ancestry):
                pid=integer(item['pid'],2);need(pid not in ids and pid not in (trace['popen_pid'],trace['parent_pid']),'cleanup PID alias');ids.add(pid);integer(item['starttime'],1)
                need(item['ppid']==(ancestry[index+1]['pid'] if index+1<len(ancestry) else trace['popen_pid']),'cleanup ancestry')
                stat=item['proc_stat'];fields=stat[stat.rfind(')')+2:].split();need(int(stat.split(' ',1)[0])==pid and int(fields[1])==item['ppid'] and int(fields[19])==item['starttime'],'retained proc identity')
                fdinfo=dict(line.split(':',1) for line in item['pidfd_info'].splitlines());need(int(fdinfo['Pid'].strip())==pid,'actual pinned pidfd')
            need(process['before_exited']==[False]*len(ancestry) and process['after_exited']==[True]*len(ancestry),'cleanup before harness intervention')
            need(process['cancel_ns']==sig['at_ns'] and sig['at_ns']>=submit['at_ns'] and sig['at_ns']<process['cleanup_ns']<=trace['exit_ns'],'cleanup clocks')
            ack=crossing(trace,data,'отменено',sig['offset']);need(process['ack_ns']==ack,'Cancel ack clock')
            need(row['cancel_ack_seconds']==(ack-sig['at_ns'])/1e9 and row['process_cleanup_seconds']==(process['cleanup_ns']-sig['at_ns'])/1e9,'cancel/cleanup metric clocks')
    summaries={}
    for language in ('cpp','rust'):
        for count in counts:
            for kind,fields in {'turn':('submit_preview_seconds','terminal_durable_done_seconds','fsync_seconds','rss_bytes'),'cancel':('cancel_ack_seconds','process_cleanup_seconds','fsync_seconds','rss_bytes')}.items():
                rows=[r for r in report['runs'] if r['language']==language and r['submits']==count and r['kind']==kind]
                summaries[f'{language}-{count}-{kind}']={f:distribution([r[f] for r in rows]) if f!='fsync_seconds' else distribution([n/1e9 for r in rows for n in r['native']['metrics']['fsync_ns']]) for f in fields}
    same(report['summary'],summaries,'pipeline distributions');return summaries
def sse(report,directory,adapters,samples=SAMPLES,iterations=50):
    library=Path(adapters['cpp']).parent/'libansa_allocation_counter.so'
    need(report['counter']=={'path':str(library),'sha256':binary_hash(library,'libansa_allocation_counter.so')},'actual allocation interposer')
    wire=raw(directory,report['wire'],'sse-wire.bin');need(hashlib.sha256(wire).hexdigest()==WIRE_SHA256,'frozen SSE workload');events=protocol.parse_events(wire.decode())
    need(len(events)==6 and [e['type'] for e in events]==['response.created','response.output_item.added','response.output_item.done','response.output_item.added','response.output_item.done','response.completed'],'SSE event inventory')
    opaque='x'*(256*1024)
    for item in (events[1]['item'],events[2]['item'],events[-1]['response']['output'][0]):need(item['encrypted_content']==opaque,'SSE encrypted reasoning work')
    need(events[-1]['response']['output'][1]['content']==[{'type':'output_text','text':'SSE_OK'}] and events[-1]['response']['model']==BINDING['returned_model'],'SSE terminal work')
    expected=[(index,language) for index in range(samples) for language in ('cpp','rust')]
    need([(r['sample'],r['language']) for r in report['runs']]==expected,'SSE sample inventory/order')
    def actual(row,language):
        request=value(directory,row['stdin']);need(set(request)=={'op','root','iterations'} and request['op']=='sse_bench' and integer(request['iterations'])==iterations,'SSE actual request')
        output=command(directory,row,adapters[language],request)
        same({k:v for k,v in output.items() if k!='elapsed_ns'},{'wire_sha256':hashlib.sha256(wire).hexdigest(),'bytes_per_response':len(wire),'iterations':iterations,'admissions':iterations,'tool_calls':0},'SSE full actual admissions')
        return integer(output['elapsed_ns'],1)/1e9/(len(wire)*iterations/1048576)
    for row in report['runs']:need(row['seconds_per_mib']==actual(row['command'],row['language']),'SSE time/work')
    need(set(report['allocations'])=={'cpp','rust'},'allocation measurements')
    for language,item in report['allocations'].items():
        instrumentation=item['command']['allocation_instrumentation'];fd=integer(instrumentation['output_fd'],3)
        same(instrumentation,{'library':str(library),'library_sha256':report['counter']['sha256'],'output_fd':fd,'inherited_fds':[fd]},'actual allocation instrumentation')
        actual(item['command'],language);counts=value(directory,item['raw'],language+'-allocations.json');same(counts,item['counts'],'allocation raw counts')
        need(set(counts)=={'allocations','requested_bytes'} and all(type(v) is int and v>iterations for v in counts.values()),'allocation work missing')
    summary={language:distribution([r['seconds_per_mib'] for r in report['runs'] if r['language']==language]) for language in ('cpp','rust')}
    same(report['summary'],summary,'SSE distributions');return summary
def resources(report,directory,budget):
    rows=report['runs'];need(rows,'missing frozen resource measurements')
    progress=protocol.owned_bytes(directory/'progress.jsonl',64*1024*1024,single_link=True)
    same(rows,[auth.json_bytes(line) for line in progress.splitlines()],'M0 raw progress journal')
    labels=[];cli={}
    for name in ('self-check','offline-demo'):
        labels += [f'{language}-{name}-warmup' for language in ('cpp','rust')]
        for category,count in [('warm',50),('cold',20),('rss-warm',20),('rss-cold',10)]:
            labels += [f'{language}-{name}-{category}-{i:02}' for i in range(count) for language in ('cpp','rust')]
            for language in ('cpp','rust'):
                prefix=f'{language}-{name}-{category}-';selected=[r for r in rows if r['label'].startswith(prefix)]
                cli[prefix[:-1]]=distribution([r['runtime_maxrss_bytes' if category.startswith('rss') else 'wall_seconds'] for r in selected])
    labels += [f'{language}-{action}' for language in ('cpp','rust') for action in ('strip','closure')]+['rust-dependencies-warmup']
    for index in range(3):
        labels += [f'cpp-configure-{index}',f'cpp-clean-build-{index}',f'rust-clean-prep-{index}',f'rust-clean-build-{index}']
        labels += [f'{language}-incremental-build-{index}-{edit}' for language in ('cpp','rust') for edit in range(3)]
    need([r['label'] for r in rows]==labels,'frozen M0 run inventory/order')
    for row in rows:
        need(type(row['exit_code']) is int and row['exit_code']==0 and row['timed_out'] is False,'failed/timed-out measurement')
        need(type(row['wall_seconds']) in (int,float) and 0<row['wall_seconds']<600,'measurement clock')
        for stream in ('stdout','stderr'):raw(directory,row[stream],f'logs/{row["label"]}.{stream}')
        required_rss=bool(re.fullmatch('(cpp|rust)-(?:self-check|offline-demo)-rss-(?:warm|cold)-[0-9]+',row['label']) or re.fullmatch('(?:cpp-configure|(?:cpp|rust)-(?:clean-build|incremental-build))-[0-9](?:-[0-9])?',row['label']) or row['label']=='rust-dependencies-warmup')
        need(('rss_raw' in row)==required_rss,'required GNU time RSS inventory')
        if 'rss_raw' in row:
            name=f'logs/{row["label"]}.rss-kib'
            need(row['runtime_maxrss_bytes']==int(raw(directory,row['rss_raw'],name).strip())*1024 and type(row['runtime_maxrss_bytes']) is int,'GNU time raw RSS')
            need(row['argv'][:6]==[budget['environment']['timer']['path'],'-f','%M','-o',str(directory/name),'--'],'GNU time method/output identity')
        match=re.fullmatch('(cpp|rust)-(self-check|offline-demo)-(warmup|warm-[0-9]+|cold-[0-9]+|rss-warm-[0-9]+|rss-cold-[0-9]+)',row['label'])
        if match:
            language,name,category=match.groups();argv=row['argv'][6:] if 'rss_raw' in row else row['argv'];need(len(argv)==2 and argv[1]=='--'+name,'actual safe CLI')
            if 'cold' in category:
                need(argv[0]==str(directory/'cold-executables'/row['label']),'controlled cold binary path')
                need(hashlib.sha256(protocol.owned_bytes(Path(argv[0]),256*1024*1024,executable=True,single_link=True)).hexdigest()==report['footprints'][language]['sha256'],'controlled cold binary bytes')
                resident=row['cache_condition'];need(type(resident['resident_pages_before_exec']) is int and resident['resident_pages_before_exec']==0 and integer(resident['file_pages'],1)>0,'controlled cache residency')
            else:need(argv[0]==report['footprints'][language]['path'],'actual warm binary')
            output=value(directory,row['stdout'])
            if name=='self-check':need(output['project']=='ansa' and output['target']=='aarch64-linux','actual self-check')
            else:same(output,{'status':'passed','mode':'offline','tool_invocations':1,'native_replay':True,'final_text':'OMH_TOOL_OK:OMH_FIXED_VALUE_42'},'actual offline demo')
    same(report['cli'],cli,'safe CLI distributions')
    source=directory/'build-source';rust=['cargo','build','--manifest-path',str(source/'rust/Cargo.toml'),'--locked','--release','--bin','ansa-rs','-j2']
    for name,digest in report['source']['files'].items():
        if name=='CMakeLists.txt' or name.startswith(('src/','include/','rust/')):need(hashlib.sha256(protocol.owned_bytes(source/name,64*1024*1024,single_link=True)).hexdigest()==digest,'measured build source identity')
    for row in rows:
        label=row['label'];argv=row['argv'][6:] if 'rss_raw' in row else row['argv']
        match=re.fullmatch('(cpp|rust)-(configure|clean-build|clean-prep|incremental-build)-([0-9])(?:-([0-9]))?',label)
        if label=='rust-dependencies-warmup':need(argv==rust,'dependency warmup build identity')
        if not match:continue
        language,kind,index,edit=match.groups();target=directory/f'cpp-build-{index}';need(row['cwd']==str(source),'measured build cwd')
        if language=='cpp':
            if kind=='configure':expected=['cmake','-S',str(source),'-B',str(target),'-G','Ninja','-DCMAKE_BUILD_TYPE=Release','-DBUILD_TESTING=OFF','-DCMAKE_CXX_COMPILER='+report['host_context']['toolchains']['g++']['path'],'-DCMAKE_CXX_COMPILER_LAUNCHER=','-DCMAKE_CXX_FLAGS=','-DCMAKE_CXX_FLAGS_RELEASE=-O3 -DNDEBUG']
            else:expected=['cmake','--build',str(target),'--target','ansa','-j2']
        elif kind=='clean-prep':expected=['cargo','clean','--manifest-path',str(source/'rust/Cargo.toml'),'--release','-p','ansa','-p','ansa-linux']
        else:expected=rust
        need(argv==expected,'measured build invocation/profile')
        if kind=='incremental-build':
            entry=source/('src/main.cpp' if language=='cpp' else 'rust/crates/ansa/src/bin/ansa.rs');expected=hashlib.sha256(entry.read_bytes()+f'\n// M8 isolated entry point edit {index}-{edit}\n'.encode()).hexdigest()
            need(row['changed_source_sha256']==expected,'actual entry point edit')
            log=raw(directory,row['stdout' if language=='cpp' else 'stderr'])
            need((b'src/main.cpp.o' in log and b'Linking CXX executable ansa' in log) if language=='cpp' else b'Compiling ansa ' in log,'actual incremental compile/relink')
    builds={}
    for language in ('cpp','rust'):
        for kind in ('clean-build','incremental-build'):
            selected=[r for r in rows if r['label'].startswith(language+'-'+kind+'-')]
            for row in selected:need(integer(row['sampled_group_rss_bytes'],1)>0 and row.get('host_before') and row.get('host_after'),'sampled descendant RSS/context')
            builds[language+'-'+kind]={field:distribution([r[field] for r in selected]) for field in ('wall_seconds','runtime_maxrss_bytes','sampled_group_rss_bytes')}
    same(report['builds'],builds,'build distributions')
    for language,footprint in report['footprints'].items():
        binary=Path(footprint['path']);need(str(binary).startswith('/nix/store/') and binary.name==('ansa' if language=='cpp' else 'ansa-rs'),'installed Nix identity')
        need(footprint['sha256']==binary_hash(binary,'ansa' if language=='cpp' else 'ansa-rs') and integer(footprint['installed_binary_bytes'],1)==binary.stat().st_size,'installed binary size/hash')
        stripped=directory/(language+'-stripped');need(footprint['strip_all_sha256']==hashlib.sha256(protocol.owned_bytes(stripped,256*1024*1024,executable=True,single_link=True)).hexdigest() and integer(footprint['strip_all_binary_bytes'],1)==stripped.stat().st_size,'stripped binary size/hash')
        same(footprint['source_proof'],installed_source(binary,language,report['source']['files'],report['source']['head']),'installed source provenance')
        strip=next(r for r in rows if r['label']==language+'-strip');need(strip['argv']==['strip','--strip-all',str(stripped)] and strip['cwd']==str(common.ROOT),'actual strip command')
        # Recompute the deterministic transform of this immutable installed file;
        # a tiny arbitrary file with a matching report digest is insufficient.
        strip_tool=Path(shutil.which('strip')).resolve(strict=True)
        need(report['host_context']['toolchains']['strip']=={'path':str(strip_tool),'sha256':binary_hash(strip_tool,strip_tool.name),'version':subprocess.check_output([strip_tool,'--version'],text=True).strip()},'pinned strip tool')
        with tempfile.TemporaryDirectory(prefix='ansa-m8-strip-check-') as temporary:
            copy=Path(temporary)/'binary';shutil.copyfile(binary,copy)
            subprocess.run([strip_tool,'--strip-all',copy],check=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=30)
            need(common.sha(copy)==footprint['strip_all_sha256'],'stripped bytes derived from actual installed binary')
        closure=next(r for r in rows if r['label']==language+'-closure');info=nix_info(value(directory,closure['stdout']))
        package=str(binary.parent.parent);need(closure['argv']==['nix','path-info','--json','--json-format','2','--closure-size',package] and closure['cwd']==str(common.ROOT),'actual closure invocation')
        need(set(info)=={package} and footprint['nix_closure_nar_bytes']==info[package]['closureSize'],'Nix closure raw package/bytes')
        actual=nix_info(auth.json_bytes(subprocess.check_output(closure['argv'],text=False,timeout=30)))
        need(actual[package]['closureSize']==footprint['nix_closure_nar_bytes'],'immutable Nix closure size')
    bounds=budget['budgets'];errors=[];rust=report['footprints']['rust']
    for field in ('installed_binary_bytes','strip_all_binary_bytes','nix_closure_nar_bytes'):
        if rust[field]>bounds[field]:errors.append(field+' exceeds frozen M0 ceiling')
    for name in ('self-check','offline-demo'):
        for cache in ('warm','cold'):
            if cli[f'rust-{name}-{cache}']['p95_nearest_rank']>bounds['cli_wall_p95_seconds'][name+'_'+cache]:errors.append(f'rust-{name}-{cache} p95 exceeds frozen M0 budget')
            if cli[f'rust-{name}-rss-{cache}']['max']>bounds['cli_runtime_maxrss_bytes'][name+'_'+cache]:errors.append(f'rust-{name}-rss-{cache} exceeds frozen M0 budget')
    for kind in ('clean-build','incremental-build'):
        measured=builds['rust-'+kind]
        if measured['wall_seconds']['max']>bounds[('clean' if kind=='clean-build' else 'incremental')+'_build_max_wall_seconds']:errors.append('rust-'+kind+' exceeds frozen M0 wall budget')
        for field,bound in [('runtime_maxrss_bytes','build_single_process_maxrss_bytes'),('sampled_group_rss_bytes','build_sampled_descendants_maxrss_bytes')]:
            if measured[field]['max']>bounds[bound]:errors.append('rust-'+kind+' exceeds frozen M0 '+field)
    return errors
def new_errors(report,policy):
    errors=[]
    for section in ('startup','pipeline'):
        summary=report[section]['summary']
        for key,measures in summary.items():
            if not key.startswith('cpp-'):continue
            for field,baseline in measures.items():
                if field=='rss_bytes':continue
                limit=max(baseline['p95_nearest_rank']*policy['new_latency']['max_ratio'],baseline['p95_nearest_rank']+policy['new_latency']['additive_floor_seconds'])
                if summary['rust-'+key[4:]][field]['p95_nearest_rank']>limit:errors.append(section+' '+key[4:]+' '+field+' exceeds agreed p95 budget')
    if report['sse']['summary']['rust']['p95_nearest_rank']>report['sse']['summary']['cpp']['p95_nearest_rank']*policy['new_time_per_work']['max_ratio']:errors.append('SSE time per MiB exceeds agreed p95 budget')
    return errors
def release_cache(data,compiler):
    cache={}
    for line in data.decode().splitlines():
        if not line or line.startswith(('#','//')):continue
        key,sep,value=line.partition('=')
        if sep:cache[key.split(':')[0]]=value
    need(cache['CMAKE_BUILD_TYPE']=='Release' and cache['OMH_SANITIZERS']=='OFF' and cache['CMAKE_HOME_DIRECTORY']==str(common.ROOT),'C++ Release/non-sanitized adapter profile')
    need(cache['CMAKE_CXX_COMPILER']==compiler and cache['CMAKE_CXX_FLAGS']=='' and cache['CMAKE_CXX_FLAGS_RELEASE']=='-O3 -DNDEBUG','C++ compiler/optimization flags')
    need(all(cache.get(key,'')=='' for key in ('CMAKE_CXX_COMPILER_LAUNCHER','CMAKE_CXX_LINKER_LAUNCHER','CMAKE_EXE_LINKER_FLAGS','CMAKE_EXE_LINKER_FLAGS_RELEASE','CMAKE_SHARED_LINKER_FLAGS','CMAKE_SHARED_LINKER_FLAGS_RELEASE')),'C++ launcher/linker overrides')
def adapter_profiles(report,directory,adapters):
    preparation=report['adapter_preparation'];cpp=Path(adapters['cpp']).parent;target=Path(adapters['rust']).parents[2]
    release_cache(raw(directory,preparation['cpp_cache'],'adapter-CMakeCache.txt'),report['host_context']['toolchains']['g++']['path'])
    proof=preparation['toolchain'];need(proof['argv']==[sys.executable,str(common.ROOT/'tests/parity/check_toolchain.py')] and type(proof['exit_code']) is int and proof['exit_code']==0,'actual Cargo compiler probe')
    toolchain=value(directory,proof['stdout']);need(not raw(directory,proof['stderr']) and toolchain['status']=='PASS','Cargo-selected compiler proof')
    declared=tomllib.loads((common.ROOT/'rust/rust-toolchain.toml').read_text())['toolchain']['channel']
    need(toolchain['declared']=={'channel':declared,'rust_version':declared,'edition':'2024'} and toolchain['rustc']['path']==report['host_context']['toolchains']['rustc']['path'] and toolchain['cargo_rustc']['output'].startswith('rustc '+declared+' '),'pinned actual Cargo rustc')
    expected={'cpp':['cmake','--build',str(cpp),'--target','ansa_e2e_oracle','ansa_allocation_counter','-j2'],
              'rust':['cargo','build','--manifest-path',str(common.ROOT/'rust/Cargo.toml'),'--target-dir',str(target),'--release','--locked','--example','e2e_adapter','--features','e2e-test-adapter','-j2','--message-format=json']}
    for language in ('cpp','rust'):
        item=preparation[language];need(item['argv']==expected[language] and type(item['exit_code']) is int and item['exit_code']==0,'actual Release adapter build')
        raw(directory,item['stderr'])
    messages=[auth.json_bytes(line) for line in raw(directory,preparation['rust']['stdout']).splitlines()]
    artifacts=[m for m in messages if m.get('reason')=='compiler-artifact' and m['target']['name']=='e2e_adapter'];need(len(artifacts)==1,'Cargo adapter artifact')
    item=artifacts[0];profile=item['profile'];need(item['target']['kind']==['example'] and item['executable']==adapters['rust'] and item['target']['src_path']==str(common.ROOT/'rust/crates/ansa/examples/e2e_adapter.rs'),'actual Cargo adapter source/path')
    need(profile['opt_level']=='3' and profile['debuginfo']==0 and profile['debug_assertions'] is False and profile['overflow_checks'] is False and profile['test'] is False,'Rust Release adapter profile')
    raw(directory,preparation['cpp']['stdout'])
def verify(report,directory):
    need(report['schema_version']==1 and report['suite']=='performance' and report['source']['worktree_status']=='','report/source identity')
    need(report['source']['files']==common.source_info()['files'],'source input hashes')
    need(report['platform']['machine']=='aarch64' and report['platform']['os_release']['ID']=='nixos','native platform')
    budget_path=common.ROOT/'tests/parity/perf_budget.json';policy_path=common.ROOT/'tests/parity/fixtures/m8_perf_policy.json'
    need(report['budget_sha256']==common.sha(budget_path) and report['policy_sha256']==common.sha(policy_path),'frozen policy identity')
    budget=auth.json_bytes(budget_path.read_bytes());policy=auth.json_bytes(policy_path.read_bytes())
    need(budget['status']=='FROZEN_M0' and policy['datasets']==list(COUNTS),'agreed policy/datasets')
    need(report['open_metrics']==[] and set(report['footprints'])==set(report['adapters'])=={'cpp','rust'},'required metric/binary inventory')
    binaries={k:v['path'] for k,v in report['footprints'].items()};adapters={k:v['path'] for k,v in report['adapters'].items()}
    for language,item in report['adapters'].items():need(item['sha256']==binary_hash(item['path'],'ansa_e2e_oracle' if language=='cpp' else 'e2e_adapter'),'adapter binary hash')
    adapter_profiles(report,directory,adapters)
    startup(report['startup'],directory,binaries,adapters['cpp']);pipeline(report['pipeline'],directory,adapters);sse(report['sse'],directory,adapters)
    return resources(report,directory,budget)+new_errors(report,policy)
def check_evidence(directory):
    directory=Path(directory).resolve()
    try:
        report=auth.json_bytes(protocol.owned_bytes(directory/'perf-report.json',64*1024*1024,single_link=True));errors=verify(report,directory)
        need(report['status']=='PASS' and report['errors']==errors==[],'reported PASS/limits')
    except (OSError,ValueError,KeyError,TypeError,IndexError,StopIteration) as error:
        print(str(error));return 1
    print('M8 performance raw replay and frozen budgets PASS');return 0
if __name__=='__main__':
    import argparse
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--evidence',required=True)
    raise SystemExit(check_evidence(parser.parse_args().evidence))
