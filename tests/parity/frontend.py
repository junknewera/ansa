"""M7 CLI/wire/Carbon capture with independent raw-evidence replay."""
import copy
import hashlib
import json
import math
from pathlib import Path
import re
import subprocess
import sys
import auth
import protocol
import run as common
import runtime
import sessions
import session_cases as v
import frontend_wire

MAX=64*1024*1024
MANIFEST=common.ROOT/'tests/parity/fixtures/frontend.json'
DRIVER=common.ROOT/'tests/parity/frontend_io.py'
CPP={'cli':('backpressure','pending-cancel','busy','input-size','terminal'),'frontend':('streaming','recovery','adversarial','modes','overflow')}
CLI=[('help',['--help'],0),('short-help',['-h','ignored'],0),('demo',['--offline-demo'],0),('self-check',['--self-check'],0),('config',['--print-config','--cwd','{CWD}'],0),('one-shot',['--offline','--cwd','{CWD}','question'],0),('empty',['--offline','--cwd','{CWD}',''],1),('plan-only',['--offline','--cwd','{CWD}','/plan'],1),('basic-only',['--offline','--cwd','{CWD}','/basic'],1),('continue-missing',['--offline','--cwd','{CWD}','--continue'],1),('duplicate',['--offline','--offline'],1),('unknown',['--unknown','value'],1),('missing',['--model'],1),('two-prompts',['first','second'],1),('slash',['/cancel'],1),('auth-status',['--auth-status'],0),('auth-required',['question'],1),('refresh-required',['--refresh-auth'],1),('auth-usage',['--auth-status','extra'],2),('demo-usage',['--offline-demo','extra'],2),('jsonl-usage',['--offline-jsonl','extra'],2),('live-required',['--live-jsonl'],1),('invalid-argv',['--offline',{'hex':'ff'}],1),('invalid-session',['--offline','--session','invalid!'],1)]
ARGS=[([],True),(['--offline','task'],True),(['--continue'],True),(['--print-config'],True),(['--session','session','--cwd','relative','--model','m','--returned-model','r','task'],True),(['--model','--offline'],True),(['--auth-dir','path','task'],True),(['--offline','--offline'],False),(['a','b'],False),(['--unknown','x'],False),(['--model'],False),(['/cancel'],False),(['/resume'],False),(['/status'],False),(['/exit'],False),(['/quit'],False)]
def check(value,message):v.check(value,message)
def same(a,b):check(protocol.structural(a)==protocol.structural(b),'frontend C++/Rust values differ')
def paths(cpp,rust):
    cpp=(common.ROOT/cpp).resolve();rust=(common.ROOT/rust).resolve()
    # Existing C++ executable/native suites are unchanged M6 production inputs.
    reference=cpp if (cpp/'ansa').is_file() else common.ROOT/('build-reference-sanitize' if cpp.name.endswith('-sanitize') else 'build-reference')
    return {'cpp':cpp/'ansa_frontend_oracle','rust':rust/'debug/examples/frontend_adapter','cppcli':reference/'ansa','rustcli':rust/'debug/ansa-rs','native-cli':reference/'ansa_cli_tests','native-frontend':reference/'ansa_frontend_tests'}
def identities(values):
    expected={'cpp':'ansa_frontend_oracle','rust':'frontend_adapter','cppcli':'ansa','rustcli':'ansa-rs','native-cli':'ansa_cli_tests','native-frontend':'ansa_frontend_tests'}
    check(set(values)==set(expected),'frontend binary inventory')
    check(all(path.name==expected[key] for key,path in values.items()),'frontend binary name')
    return {key:{'path':str(path),'sha256':hashlib.sha256(protocol.owned_bytes(path,256*1024*1024,executable=True,single_link=True)).hexdigest()} for key,path in values.items()}
def decoded(text):return protocol.strict_loads(text)
class Context(runtime.Context):
    def call(self,adapter,request,exit=0,fault=None):
        request=copy.deepcopy(request);request['root']=str(self.root)
        return sessions.Context.call(self,adapter,request,exit,fault)
    def driver(self,adapter,request):
        request=dict(request,root=str(self.root),binary=str(self.bundle.paths[adapter]),fixture_binary=str(self.bundle.paths['rust']),binaries={key:str(self.bundle.paths[key]) for key in ('cppcli','rustcli')})
        label=self.row['id']+'-'+str(self.command_index);self.command_index+=1;raw=(json.dumps(request,separators=(',',':'))+'\n').encode();argv=[sys.executable,str(DRIVER)]
        if self.live:
            self.row.setdefault('commands',[]).append(label);path=self.bundle.directory/(label+'.stdin');path.write_bytes(raw)
            command=common.command(argv,common.ROOT,self.bundle.directory,label,timeout=45,input_file=path);command['stdin']=protocol.raw(path);self.bundle.commands[label]=command
        else:check(self.row['commands'][self.command_index-1]==label,'driver command order')
        check(label not in self.bundle.used,'duplicate driver');self.bundle.used.add(label);command=self.bundle.commands[label]
        check(auth.command_ok(command) and command['argv']==argv and command['cwd']==str(common.ROOT) and 'process_env' not in command,'driver invocation/exit')
        retained={stream:protocol.retained_bytes(self.bundle.directory,command.get(stream),label+'.'+stream,MAX) for stream in ('stdin','stdout','stderr')}
        check(retained['stdin']==raw and not retained['stderr'],'driver raw input/diagnostics')
        report=auth.json_bytes(retained['stdout']);check(type(report.get('schema_version')) is int and report['schema_version']==1 and report['kind']==request['kind'] and report['case']==request.get('case',''),'driver report identity')
        check(report['binary_sha256']==common.sha(self.bundle.paths[adapter]),'driver binary differs');duration(report['elapsed_total'],45)
        return report
def duration(value,limit):check(type(value) in (int,float) and math.isfinite(value) and 0<=value<limit,'invalid frontend timing')
def integer(value,limit=2**64-1):check(type(value) is int and 0<=value<=limit,'invalid frontend integer');return value
def raw_hex(value):check(isinstance(value,str) and len(value)<=2*MAX and re.fullmatch(r'(?:[0-9a-f]{2})*',value) is not None,'invalid retained hex');return bytes.fromhex(value)
def journal_events(mapping):
    check(isinstance(mapping,dict),'journal inventory');groups={}
    for name,hex in mapping.items():
        path=Path(name);check(not path.is_absolute() and '..' not in path.parts and re.fullmatch(r'part-[0-9]{20}\.log',path.name),'journal relative path');groups.setdefault(str(path.parent),{})[path.name]=raw_hex(hex)
    return {key:v.decode(parts) for key,parts in groups.items()}
def pair(c,request,accepted=True):
    values=[c.call(adapter,request) for adapter in ('cpp','rust')]
    check(all(value['accepted'] is accepted for value in values),'frontend required admission differs');same(*values)
    return values[0].get('value')
def wire_command(kind,**kw):return dict(schema_version=1,session='offline',turn='turn',incarnation='incarnation',command_id='command',type=kind,**kw)
def commands():
    rows=[(wire_command('submit',text='question'),True),(wire_command('submit',text='question',mode='plan'),True),(wire_command('snapshot',after_sequence=2**64-1),True),(wire_command('cancel',target='request'),True),(wire_command('resume',target='request'),True),(wire_command('transcript',before=0,limit=64),True),(wire_command('transcript',before=5,limit=1,forward=True),True)]
    base=wire_command('snapshot',after_sequence=0)
    for key,value in [('schema_version',2),('schema_version',True),('session',''),('turn','a'*65),('command_id','bad!'),('after_sequence',True),('after_sequence',-1),('after_sequence',1.0),('after_sequence',2**64),('type','unknown')]:rows.append((dict(base,**{key:value}),False))
    rows.extend([(dict(base,extra=True),False),(wire_command('submit',text=''),False),(wire_command('submit',text='x',mode='bad'),False),(wire_command('cancel',target=''),False),(wire_command('resume',target='bad!'),False),(wire_command('transcript',before=0,limit=0),False),(wire_command('transcript',before=0,limit=65),False),(wire_command('transcript',before=0,limit=1,forward=1),False)])
    return rows
def args_case(c,args,accepted):pair(c,{'op':'args','args':args},accepted)
def command_case(c,request,accepted):pair(c,{'op':'decode','payload':json.dumps(request,separators=(',',':'))},accepted)
def rendering(c,kind,color):
    text={'unicode':'é Ж 界 😀 👩\u200d💻\nnext line','controls':'safe\x1b[31mINJECT\x1b]0;title\a\u009bdata','markdown':'# Heading\n**alpha bravo charlie delta echo foxtrot** `code`\n```cpp\nint main() {}\n```','history':'\n'.join('history '+str(i) for i in range(80))}[kind]
    entries=[{'role':'user','text':'**literal user** `code`'},{'role':'assistant','text':text}]
    step={'entries':entries,'input_hex':'draft é😀'.encode().hex(),'cursor':len('draft é😀'.encode()),'status':'plan  готов','expanded':True,'scroll':0,'context':'fixture workspace  fixture-model','cols':80,'rows':24}
    steps=[step,{**step,'cols':20,'rows':10,'scroll':1},{**step,'cols':100,'rows':32,'entries':[{'role':'user','text':'short'}]}]
    value=pair(c,{'op':'render','color':color,'steps':steps})
    check(len(value)==3,'frame count');frames=[v['frame'] for v in value]
    check(all(isinstance(frame,str) and '\x1b[2K' in frame and '> ' in frame for frame in frames),'empty/invalid frame');check('\x1b7\x1b[1B' in frames[-1],'shrink omitted stale-row clear')
    if kind=='controls':check('\x1b[31m' not in frames[0] and '\x1b]0;' not in frames[0],'terminal injection')
    if not color:check('\x1b[38;' not in frames[0] and '\x1b[0m' not in frames[0],'NO_COLOR violated')
def event_case(c):
    events=pair(c,{'op':'events'});check(len(events)==13,'wire variant coverage')
    for event in events:check(event['schema_version'].lexeme=='1' and event['stream_sequence'].lexeme=='7','wire cursor lexeme')
    check(events[0]['tools'][1]['output']=='result' and events[-2]['effects']=='effect_unknown','wire data loss')
def cli_case(c,adapter,name,args,expected):
    report=c.driver(adapter,{'kind':'cli','commands':[args]});check(len(report['runs'])==1,'CLI run count');result=report['runs'][0];check(type(result['exit']) is int and result['exit']==expected,'CLI exit differs')
    check(result['binary_sha256']==common.sha(c.bundle.paths[adapter]),'CLI binary identity');duration(result['elapsed'],5)
    out=raw_hex(result['stdout_hex']);err=raw_hex(result['stderr_hex']);journal_events(result['journals'])
    if name in ('help','short-help'):check(out==b'Usage: ansa [--continue | --session ID] [--cwd PATH] [--model SLUG] [PROMPT]\n       ansa --offline [PROMPT]\n       ansa --login | --auth-status | --models | --logout\n       ansa --print-config\nCommands: /status /cancel /resume /exit. Ctrl+C cancels; Ctrl+D finishes accepted work.\n' and not err,'help bytes')
    elif name=='demo':check(auth.json_bytes(out)=={'status':'passed','mode':'offline','tool_invocations':1,'native_replay':True,'final_text':'OMH_TOOL_OK:OMH_FIXED_VALUE_42'} and not err,'real offline demo')
    elif name=='self-check':value=auth.json_bytes(out);check(value['project']=='ansa' and value['target']=='aarch64-linux' and all(isinstance(value[k],str) and value[k] for k in ('curl','tls','yyjson','openssl')) and not err,'truthful dependency self-check')
    elif name=='one-shot':check(out==b'OMH_TOOL_OK:OMH_FIXED_VALUE_42\n' and result['session'] and len(journal_events(result['journals']))==1,'one-shot runtime/catalog')
    elif name=='config':value=auth.json_bytes(out);check(value['cwd']==str(c.root/'home/workspace') and value['model']=='gpt-6.1-sol' and value['access_mode']=='full_access' and value['allow_sudo'] is False and not err,'effective settings')
    elif name=='auth-status':check(auth.json_bytes(out)=={'signed_in':False,'refresh_pending':False,'plan_enabled':False,'account':'','expires_in':0,'refresh_wait':0} and not err,'owned empty auth status')
    elif name.endswith('usage'):check(not out and err.startswith(b'Usage: ansa '),'CLI usage contract')
    elif name in ('empty','plan-only','basic-only'):check(not out and result['session'],'non-submitting prompt')
    else:
        check(not out,'failed CLI emitted success');lines=err.splitlines()
        if name=='invalid-argv' and adapter=='cppcli':check(len(lines)==2 and lines[0].startswith(b'ansa session '),'reference malformed-argv startup');err=lines[1]
        value=auth.json_bytes(err);check(set(value)=={'schema_version','code','stage','diagnostic'} and integer(value['schema_version'])==1,'CLI error shape');integer(value['code'],10);integer(value['stage'],7)
        codes={'auth-required':('auth_required',0,7),'refresh-required':('auth_required',4,7),'duplicate':('duplicate_cli_option',0,7),'unknown':('unknown_cli_option',7,7),'missing':('cli_option_value',0,7),'two-prompts':('cli_prompt_arguments',0,7),'slash':('slash_command_requires_stdin',0,7),'live-required':('live_session_required',0,7),'invalid-argv':('json_string_invalid',0,0),'continue-missing':('no_previous_session',0,7),'invalid-session':('invalid_session_id',0,7)}
        check((value['diagnostic'],value['code'],value['stage'])==codes[name],'CLI diagnostic tuple differs')
def interop(c,writer):
    other='rustcli' if writer=='cppcli' else 'cppcli';commands=[{'adapter':writer,'args':['--offline','first']},{'adapter':other,'args':['--offline','--session','{SESSION}','second']},{'adapter':writer,'args':['--offline','--continue']}]
    result=c.driver(writer,{'kind':'cli','commands':commands});check(len(result['runs'])==3,'interop run count');previous={};session=None
    for index,run in enumerate(result['runs']):
        check(type(run['exit']) is int and run['exit']==0,'cross-client CLI failed');check(raw_hex(run['stdout_hex'])==(b'OMH_TOOL_OK:OMH_FIXED_VALUE_42\n' if index<2 else b''),'cross-client text')
        check(run['binary_sha256']==common.sha(c.bundle.paths[commands[index]['adapter']]),'cross-client executable');check(run['session'] and (session is None or session==run['session']),'cross-client session identity');session=run['session']
        current={key:raw_hex(value) for key,value in run['journals'].items()};check(all(key in current and current[key].startswith(value) for key,value in previous.items()),'cross-client modified saved prefix');events=journal_events(run['journals']);check(len(events)==1,'cross-client catalog fork');previous=current
    check(sum(e['kind']=='done' for e in next(iter(events.values())))==2,'cross-client lost/duplicated turn')
def jsonl_case(c,adapter,case,mode='basic'):
    result=c.driver(adapter,{'kind':'jsonl','case':case,'mode':mode});exit=integer(result['exit']) if result['exit']>=0 else 128-result['exit'];check(exit==(130 if case=='signal' else 0),'JSONL exit');events=result['events'];out=raw_hex(result['stdout_hex']);err=raw_hex(result['stderr_hex']);check([auth.json_bytes(line) for line in out.splitlines()]==events,'JSONL raw events differ')
    check(events and events[0]['type']=='snapshot' and events[0]['state']=='idle','initial snapshot')
    sequence=0;initial=events[0];check(initial['session']=='offline' and re.fullmatch('[0-9a-f]{32}',initial['incarnation']),'initial wire identity')
    for event in events:
        check(type(event['schema_version']) is int and event['schema_version']==1,'event version');cursor=integer(event['stream_sequence']);check(cursor>sequence,'event cursor order');sequence=cursor
        check(all(isinstance(event[k],str) and re.fullmatch('[A-Za-z0-9_-]{1,64}',event[k]) for k in ('session','turn','incarnation')),'event identity')
        check(event['session']==initial['session'] and event['incarnation']==initial['incarnation'],'wire identity changed')
        if event['type']=='snapshot':check(integer(event['through_sequence'])==cursor,'snapshot cursor')
    kinds=[e['type'] for e in events];journal=journal_events(result['journals']);check(len(journal)==1,'missing canonical JSONL journal');records=next(iter(journal.values()));check(records and records[0]['kind']=='init' and sum(e['kind']=='init' for e in records)==1,'missing canonical init')
    if case=='errors':
        errors=[auth.json_bytes(line) for line in err.splitlines()];expected=[('bad','unknown_session',7,7),('bad-target','cancel_target',7,7),(None,'submit_mode',0,7),(None,'command_contract',7,7),(None,'json_expected_unsigned_integer',0,0),(None,'transcript_direction',0,7)]
        check([(e['command_id'],e['diagnostic'],e['code'],e['stage']) for e in errors]==expected,'JSONL error correlation/tuple/order');check(len(events)==1 and not any(e['kind']=='accepted' for e in records),'invalid command executed')
    else:
        accepted=[e for e in events if e['type']=='command_accepted'];check(len(accepted)==1 and accepted[0]['command_id']=='submit' and accepted[0]['request_id']=='request_1' and accepted[0]['turn']=='turn','lost/misbound admission')
        canonical=[e for e in records if e['kind']=='accepted'];check(len(canonical)==1 and canonical[0]['command']=='submit' and canonical[0]['request']=='request_1' and canonical[0]['turn']=='turn' and canonical[0]['text']=='question' and canonical[0]['mode']==mode,'wire/journal admission differs')
        for event in events[events.index(accepted[0]):]:check(event['request_id']=='request_1' and event['turn']=='turn','wire request/turn changed')
    if case in ('cancel','signal'):
        check(not err and not any(k in kinds for k in ('tool_started','tool_finished','done','text_delta')) and not any(e['kind'] in ('intent','outcome','done') for e in records),'cancel dispatched/published')
        if case=='cancel':
            cancelled=[e for e in events if e['type']=='cancelled'];check(len(cancelled)==1 and cancelled[0]['command_id']=='cancel','lost/misbound cancellation');check(any(e['type']=='snapshot' and e['state']=='cancelled' and e['durable'] is True for e in events) and sum(e['kind']=='cancel' and e['turn']=='turn' for e in records)==1,'cancel not durable')
    elif case!='errors':
        check(not err and all(kinds.count(kind)==1 for kind in ('command_accepted','tool_started','tool_finished','text_delta','done')),'JSONL roundtrip count');page=next(e for e in events if e['type']=='transcript');check([e['role'] for e in page['entries']]==['user','assistant'] and all(e['mode']==mode for e in page['entries']),'JSONL transcript/mode');check(page['entries'][-1]['text']=='OMH_TOOL_OK:OMH_FIXED_VALUE_42','JSONL durable answer')
        operation=initial['incarnation']+'_request_1:op:1';answer='OMH_TOOL_OK:OMH_FIXED_VALUE_42';output='{"ok":true,"value":"OMH_FIXED_VALUE_42"}'
        check([kinds.index(kind) for kind in ('command_accepted','tool_started','tool_finished','text_delta','done')]==sorted(kinds.index(kind) for kind in ('command_accepted','tool_started','tool_finished','text_delta','done')),'wire execution order')
        for event in events:
            if event['type'] in ('tool_started','tool_finished'):check(event['operation_id']==operation and event['call_id']=='fixture_call','wire tool identity')
            if event['type']=='tool_started':check(event['name']=='get_test_value','wire tool name')
            if event['type']=='tool_finished':check(event['execution']=='succeeded' and event['effects']=='known_result' and event['output']==output,'wire tool result lost')
            if event['type']=='text_delta':check(event['text']==answer,'wire final text lost')
            if event['type']=='snapshot':
                for tool in event['tools']:
                    check(tool['operation_id']==operation and tool['call_id']=='fixture_call','snapshot tool identity')
                    if 'execution' in tool:check(tool['execution']=='succeeded' and tool['effects']=='known_result' and tool['output']==output,'snapshot tool result lost')
        check(page['entries'][0]['text']=='question' and all(e['request_id']=='request_1' and e['turn']=='turn' and e['state']=='completed' for e in page['entries']),'transcript identity/data')
        check(sum(e['kind']=='done' for e in records)==1 and sum(e['kind']=='outcome' for e in records)==1,'missing canonical completion')
        if case=='duplicate':check(kinds.count('command_replayed')==1,'JSONL repeat was executed')
def pty_case(c,case,plain=False):
    result=c.driver('rust',{'kind':'terminal','case':case,'plain':plain});check(type(result['exit']) is int and result['exit'] in (0,1),'PTY exit');check(result['termios_restored'] is True and result['flags_restored'] is True,'terminal/OFD restoration');raw=raw_hex(result['stdout_hex']);check(b'\x1b[?1049' not in raw,'alternate screen introduced');journal=journal_events(result['journals'])
    check(result['observations'],'missing PTY observations')
    for observation in result['observations']:
        needle=raw_hex(observation['text_hex']);begin=integer(observation['since']);end=integer(observation['through']);check(needle and 0<=begin<end<=len(raw) and needle in raw[begin:end],'PTY observation raw binding')
    if plain:check(b'\x1b[38;' not in raw and b'\x1b[0m' not in raw,'NO_COLOR')
    if case=='terminal':first=raw_hex(result['marks']['first']);check(raw.startswith(first) and first and 0<=first.find(b'> EARLIER_CONTEXT')<first.rfind(b'> '),'history-before-input');check(b'\x1b[31m' not in raw and b'assistant manager' in raw and b'next' in raw,'PTY sanitization/busy draft')
    if case=='reject':mark=raw_hex(result['marks']['rejected']);check(mark and raw.startswith(mark) and b'secondAFTER' in mark,'rejected admission lost input')
    if case=='job-control':check(result['marks'].get('stopped') is True,'job control')
    if case=='preview-invalid':
        check(len(journal)==1,'missing preview journal');records=next(iter(journal.values()));check(sum(e['kind']=='accepted' for e in records)==1 and sum(e['kind']=='pause' for e in records)==1 and not any(e['kind'] in ('intent','outcome','done') for e in records),'missing rejected-preview trace');check(all('draft' not in json.dumps(protocol.structural(e)) for e in records),'preview entered journal')
    if case=='repeat-cancel':
        check(sum(e['kind']=='cancel' for events in journal.values() for e in events)==2,'repeat cancel records')
        for mode in ('plan','basic'):check(any(raw_hex(o['text_hex'])==(mode+'  отменено').encode() for o in result['observations']),'cancel did not allow a subsequent mode command')
def fault_case(c,case):
    result=c.driver('rustcli',{'kind':'fault','case':case});expected=130 if case=='blocked-cancel' else 1 if case in ('blocked-shutdown','epipe','read-error') else 0;check(type(result['exit']) is int and result['exit']==expected,'fault exit');raw_hex(result['stdout_hex']);raw_hex(result['stderr_hex'])
    if case in ('pending','pending-signal'):
        check(result['generated']=='','early Cancel dispatched generation');groups=journal_events(result['journals']);check(len(groups)==1,'missing early-cancel journal');records=next(iter(groups.values()));check(sum(e['kind']=='accepted' for e in records)==1 and sum(e['kind']=='cancel' for e in records)==1 and not any(e['kind'] in ('intent','outcome','done') for e in records),'missing early-cancel durable trace')
    if case=='pending-signal':check(raw_hex(result['stderr_hex']).count(b'ansa: ')==2,'pending control-loop handshake')
    if case=='read-error':check(result['flags_restored'] is True,'error path OFD restoration');journal_events(result['journals'])
    if case.startswith('blocked') or case=='epipe':duration(result['elapsed'],3);journal_events(result['journals'])
def bridge_case(c,case,tls):
    request=v.configuration();request['binding']['requested_model']='gpt-6.1-sol';request['binding']['returned_model']='gpt-6.1-sol'
    report=c.driver('rust',{'kind':'bridge','case':case,'tls':tls,'request':request});check(type(report['exit']) is int and report['exit']==0 and raw_hex(report['stderr_hex'])==b'','bridge process')
    value=auth.json_bytes(raw_hex(report['stdout_hex']));check(value['accepted'] is True,'bridge adapter refusal');value=value['value'];positive=case in ('valid-control','interim-control','h2-valid','h2-trailer')
    check(value['accepted'] is positive and value['batch_accepted'] is positive and type(value['executors']) is int and value['executors']==int(positive),'bridge terminal/EOF/batch gate')
    body=raw_hex(report['body_hex']);check(body==protocol.fixture('round1.sse').encode(),'bridge synthetic response identity');integer(value['received'],16*1024*1024)
    request_raw=auth.json_bytes(raw_hex(report['request_hex']));check(request_raw['root']==str(c.root) and request_raw['url']==report['url'] and request_raw['request']==request,'bridge route/request binding')
    if case.startswith('h2-'):
        check(len(report['observations'])==1 and type(report['observations'][0]['server_exit']) is int,'H2 server observation');check(raw_hex(report['observations'][0]['stderr_hex'])==b'','H2 peer diagnostics');peer=auth.json_bytes(raw_hex(report['observations'][0]['stdout_hex']))
    else:
        check(len(report['observations'])==1,'bridge peer request count');peer=report['observations'][0];wire=frontend_wire.wire(case,body);check(raw_hex(peer['response_wire_hex'])==wire and peer['response_sha256']==hashlib.sha256(wire).hexdigest(),'bridge peer wire identity');integer(peer['sent'],len(wire))
    from urllib.parse import urlsplit
    route=urlsplit(report['url']);expected={'model':request['binding']['requested_model'],'store':False,'stream':True,'instructions':request['epoch']['instructions'],'input':request['input'],'tools':[{'type':'namespace','name':'omh','description':'Compiled local tools','tools':[{'type':'function','name':d['name'],'description':d['description'],'strict':True,'parameters':d['schema']} for d in request['tools']['descriptors']]}],'include':['reasoning.encrypted_content']}
    actual=decoded(raw_hex(peer['request_body_hex']));same(actual,decoded(json.dumps(expected)));check(peer['authorization_present'] is True and peer['authorization_matches'] is True and peer['method']=='POST' and peer['content_types']==['application/json'] and peer['target']==route.path,'bridge HTTP request headers/path')
    check(peer.get('authority')==route.netloc if case.startswith('h2-') else peer['hosts']==[route.netloc],'bridge HTTP authority')
    received=raw_hex(value['received_hex']);check(len(received)==value['received'],'bridge received count/raw differs')
    if case in ('truncated-after-terminal','mime-trailer-after-terminal','h2-reset','h2-mime-trailer','head-trailer-count','chunk-budget'):
        full=(b':'+b'x'*16000+b'\n\n' if case=='chunk-budget' else b'')+body;check(received.startswith(full),'failure before apparent terminal cannot cover EOF gate')
def ca_fifo(c):
    if c.live:
        import os
        os.mkfifo(c.root/'ca.pem',0o600)
    value=c.call('rust',{'op':'ca'});check(value['accepted'] is False and value['error']['diagnostic']=='tls_ca_invalid','FIFO CA reader did not fail promptly')
def large_event(c):
    report=c.driver('rust',{'kind':'jsonl','case':'complete','fixture_mode':'large-jsonl'});check(type(report['exit']) is int and report['exit']==0 and raw_hex(report['stderr_hex'])==b'','large valid event writer failed');events=report['events'];check([auth.json_bytes(line) for line in raw_hex(report['stdout_hex']).splitlines()]==events,'large event raw binding')
    views=[e for e in events if e['type']=='snapshot' and e['state']=='completed'];check(views and views[-1]['completed_tools']==12,'large snapshot not delivered');tools=views[-1]['tools'];check(len(tools)==12 and all(tool['output']=='x'*(100*1024) for tool in tools),'valid tool result truncated/lost');check(len(json.dumps(views[-1]))>1024*1024,'large output case too small');journal_events(report['journals'])
def first_frame(c,adapter):
    report=c.driver(adapter,{'kind':'first-frame'});check(type(report['exit']) is int and report['exit']==1,'guarded dispatch failure exit');check(type(report['connections_before_frame']) is int and report['connections_before_frame']==0 and type(report['connections_after_submit']) is int and report['connections_after_submit']==1,'first-frame network ordering');check(report['proxy_request_line'].startswith('CONNECT auth.openai.com:443 '),'loopback-only proxy trap');raw=raw_hex(report['stdout_hex']);first=raw_hex(report['first_hex']);check(raw.startswith(first) and b'basic' in first and b'> ' in first,'actual first frame observation');check(report['termios_restored'] is True and report['flags_restored'] is True,'first-frame restoration');events=journal_events(report['journals']);check(len(events)==1 and sum(e['kind']=='accepted' for values in events.values() for e in values)==1 and not any(e['kind'] in ('intent','outcome','done') for values in events.values() for e in values),'failed auth dispatched tools or published assistant')
def jobs(mode):
    result=[('args-'+str(i),lambda c,a=a,ok=ok:args_case(c,a,ok)) for i,(a,ok) in enumerate(ARGS)]
    result += [('decode-'+str(i),lambda c,row=row,ok=ok:command_case(c,row,ok)) for i,(row,ok) in enumerate(commands())]
    result += [('wire-events',event_case)]+[(f'render-{kind}-{color}',lambda c,k=kind,col=color:rendering(c,k,col)) for kind in ('unicode','controls','markdown','history') for color in (False,True)]
    result += [(f'cli-{adapter}-{name}',lambda c,a=adapter,n=name,args=args,e=exit:cli_case(c,a,n,args,e)) for adapter in ('cppcli','rustcli') for name,args,exit in CLI]
    result += [('interop-'+w,lambda c,w=w:interop(c,w)) for w in ('cppcli','rustcli')]
    result += [(f'jsonl-{a}-{case}-{m}',lambda c,a=a,case=case,m=m:jsonl_case(c,a,case,m)) for a in ('cppcli','rustcli') for case in ('complete','duplicate','cancel','errors','signal') for m in ('basic','plan')]
    if mode=='fault':
        result += [(f'cpp-{g}-{s}',lambda c,g=g,s=s:c.native(g,s)) for g,suites in CPP.items() for s in suites]
        result += [(f'pty-{s}-{p}',lambda c,s=s,p=p:pty_case(c,s,p)) for s in ('terminal','repeat-cancel','reject','preview-invalid','overflow','job-control') for p in (False,True)]
        result += [('io-'+s,lambda c,s=s:fault_case(c,s)) for s in ('pending','pending-signal','read-error','stderr','blocked-cancel','blocked-shutdown','epipe')]
        result += [(f'bridge-{s}-{tls}',lambda c,s=s,tls=tls:bridge_case(c,s,tls)) for s in frontend_wire.CASES for tls in (False,True)]
        result += [('bridge-'+s,lambda c,s=s:bridge_case(c,s,True)) for s in ('h2-valid','h2-trailer','h2-mime-trailer','h2-reset','h2-duplicate-mime','h2-large-headers')]+[('ca-fifo',ca_fifo),('large-valid-event',large_event)]
        result += [('local-first-frame-'+a,lambda c,a=a:first_frame(c,a)) for a in ('cppcli','rustcli')]
    return result
def inventory(mode):return [id for id,_ in jobs(mode)]
def manifest():
    value={'schema_version':1,'suite':'frontend','compare_ids':inventory('compare'),'fault_ids':inventory('fault')};check(auth.json_bytes(protocol.owned_bytes(MANIFEST,MAX))==value,'frontend manifest');return value
def execute(bundle,mode,rows,live):
    check(live or [row['id'] for row in rows]==inventory(mode),'frontend case inventory')
    for index,(id,job) in enumerate(jobs(mode)):
        row={'id':id,'status':'pass'} if live else rows[index];context=Context(bundle,row,live)
        try:job(context);context.close()
        finally:
            if live:context.temporary.cleanup()
        if live:rows.append(row)
        check(row['status']=='pass','frontend failed/skipped')
    check(bundle.used==set(bundle.commands),'frontend unreferenced commands');roots=[row['root'] for row in rows];check(len(roots)==len(set(roots)),'frontend roots reused')
def verify(report,directory):
    try:
        manifest();check(type(report.get('schema_version')) is int and report['schema_version']==1 and report['suite']=='frontend' and report['mode'] in ('compare','fault'),'frontend identity');check(report['manifest_sha256']==common.sha(MANIFEST),'frontend manifest identity');check(report['source']['files']==common.source_info()['files'] and report['source']['worktree_status']=='','frontend source identity/dirty');host=report['platform'];check(host['system']=='Linux' and host['machine']=='aarch64' and host['os_release']['ID']=='nixos','native platform')
        binaries={k:Path(v['path']) for k,v in report['binaries'].items()};check(identities(binaries)==report['binaries'],'frontend binary identities');execute(sessions.Bundle(directory,binaries,report['commands']),report['mode'],report['cases'],False);check(report['status']=='PASS' and report['errors']==[],'frontend report status');return []
    except (OSError,ValueError,TypeError,KeyError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as error:return [str(error) if isinstance(error,ValueError) else 'invalid frontend evidence: '+type(error).__name__]
def capture(args):
    report={'status':'FAIL','errors':[],'cases':[]};directory=None
    try:
        manifest();check(args.cpp_build and args.rust_target and args.output,'frontend capture paths');target=Path(args.output).resolve();check(not any(part=='.env' or part.startswith('.env.') for part in target.parts),'forbidden evidence path');target.mkdir(parents=True,exist_ok=False);directory=target;source=common.source_info();check(source['worktree_status']=='','clean frontend checkpoint required');binaries=paths(Path(args.cpp_build),Path(args.rust_target));report.update(schema_version=1,suite='frontend',mode=args.action,source=source,platform=common.platform_info(),binaries=identities(binaries),manifest_sha256=common.sha(MANIFEST));bundle=sessions.Bundle(directory,binaries,{});execute(bundle,args.action,report['cases'],True);report['commands']=bundle.commands;check(common.source_info()==source,'source changed during frontend capture');report.update(status='PASS',errors=[])
    except (OSError,ValueError,TypeError,KeyError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as error:
        if 'bundle' in locals():report['commands']=bundle.commands
        report['errors']=[str(error) if isinstance(error,ValueError) else 'frontend capture failed: '+type(error).__name__]
    if directory is not None:common.write_json(directory/'frontend-report.json',report)
    print(json.dumps({'suite':'frontend','mode':args.action,'status':report['status'],'cases':len(report['cases']),'errors':report['errors']},indent=2));return 0 if report['status']=='PASS' else 1
def check_evidence(compare,fault=None):
    errors=[]
    for mode,directory in [('compare',compare),('fault',Path(fault) if fault else Path(str(compare)+'-fault'))]:
        try:report=auth.json_bytes(protocol.owned_bytes(directory/'frontend-report.json',MAX));check(report['mode']==mode,'frontend report mode');errors.extend(mode+': '+e for e in verify(report,directory))
        except (OSError,ValueError,KeyError):errors.append(mode+': invalid/missing frontend report')
    print(json.dumps({'milestone':'M7','status':'PASS' if not errors else 'FAIL','errors':errors},indent=2));return 1 if errors else 0
