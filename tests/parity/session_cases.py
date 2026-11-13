"""Synthetic v1 session vectors; independent framing and semantic assertions."""
import copy
import hashlib
import json
from pathlib import Path
import subprocess

ROOT=Path(__file__).resolve().parents[2]
GOLDEN=ROOT/'tests/fixtures/sessions/golden.json'
def compact(value): return json.dumps(value,ensure_ascii=False,separators=(',',':'))
def digest(value): return hashlib.sha256(value if isinstance(value,bytes) else compact(value).encode()).hexdigest()
def check(value,code):
    if not value: raise ValueError(code)
def oracle(binary,request):
    result=subprocess.run([str(binary)],input=(compact(request)+'\n').encode(),capture_output=True,timeout=30,check=True)
    row=json.loads(result.stdout);check(row.get('accepted') is True,'fixture generation rejected');return row['value']
def configuration():
    descriptor={'name':'get_test_value','description':'Return a fixed test JSON value without side effects','revision':'1','schema':{'type':'object','properties':{},'required':[],'additionalProperties':False}}
    identity=dict(descriptor);identity['implementation_revision']=identity.pop('revision')
    identity={k:identity[k] for k in ('name','description','implementation_revision','schema')}
    return {'binding':{'provider':'openai-siwc','account':'synthetic-m4','requested_model':'gpt-fixture','returned_model':'gpt-fixture'},'epoch':{'id':'session_epoch','instructions':'Use the pure fixture tool','exact_state':{'mode':'offline','revision':1}},'tools':{'revision':digest([identity]),'descriptors':[descriptor]},'input':[]}
def request(config,history,text):
    value=copy.deepcopy(config);value['input']=copy.deepcopy(list(history))+[{'role':'user','content':text}];return value
def accepted(config,history=(),index=1,mode='basic',compact_first=False):
    text='Call get_test_value. Preserve ORCHID.';next_request=request(config,history,text)
    if mode=='plan':
        base=config['epoch'];state={'base_epoch':base['id'],'base_state':base['exact_state'],'base_instructions':base['instructions'],'mode':'plan','tools_revision':config['tools']['revision']}
        next_request['epoch']={'id':digest(state),'instructions':base['instructions']+'\nPlan mode: inspect and reason using read-only tools. Do not modify files or execute commands. Provide a concrete plan.','exact_state':state}
    return {'kind':'accepted','command':f'c{index}','turn':f't{index}','request':f'r{index}','request_number':index,'text':text,'operation_run':f'run{index}','next':next_request,'compact':compact_first,'mode':mode,'round_limit':3,'call_limit':8}
def transition(kind,index=1,**fields): return {'kind':kind,'turn':f't{index}',**fields}
def generate(binary):
    config=configuration();init={'kind':'init','version':1,'session':'fixture','configuration':config};start=accepted(config)
    first=oracle(binary,{'op':'offline','request':start['next']})
    calls=[i for i in first['items'] if i['type']=='function_call'];check(len(calls)==1,'one fixture call')
    call=calls[0];intent={'operation':'run1:op:1','call':call['call_id'],'name':call['name'],'revision':'1','arguments':json.loads(call['arguments'])}
    result={'operation':intent['operation'],'call':intent['call'],'output':'{"ok":true,"value":"OMH_FIXED_VALUE_42"}','execution':0,'effects':1,'dispatched':True}
    next_request=copy.deepcopy(start['next']);next_request['input']+=first['items']+[{'type':'function_call_output','call_id':call['call_id'],'output':result['output']}]
    final=oracle(binary,{'op':'offline','request':next_request})
    completed=[init,start,transition('response',response=first),transition('intent',invocation=intent),transition('outcome',result=result),transition('continued'),transition('response',response=final),transition('done')]
    traces={'completed':completed,'old-missing-mode':copy.deepcopy(completed),'paused':completed[:3]+[transition('pause',diagnostic='fixture_pause')],'cancelled':completed[:3]+[transition('cancel')],'outstanding-intent':completed[:4],'known-prefix':completed[:5]}
    traces['old-missing-mode'][1].pop('mode')
    traces['resumed']=traces['paused']+[transition('resume',command='resume1')]+completed[3:]
    plan=accepted(config,mode='plan');plan_response=oracle(binary,{'op':'answer','request':plan['next'],'text':'A read-only plan.'})
    traces['plan']=[init,plan,transition('response',response=plan_response),transition('done')]
    denied=copy.deepcopy(result);denied.update(execution=3,effects=0,dispatched=False)
    traces['not-started-resume']=completed[:4]+[transition('outcome',result=denied),transition('pause',diagnostic='auth_required'),transition('resume',command='resume1')]+completed[3:]
    unknown=copy.deepcopy(result);unknown.update(execution=1,effects=2)
    traces['effect-unknown']=completed[:4]+[transition('outcome',result=unknown),transition('pause',diagnostic='effect_unknown')]
    history=[];simple=[init]
    for index in range(1,4):
        event=accepted(config,history,index);answer=oracle(binary,{'op':'answer','request':event['next'],'text':f'ORCHID answer {index}'})
        simple += [event,transition('response',index,response=answer),transition('done',index)]
        history=event['next']['input']+answer['items']
    source=copy.deepcopy(config);source['input']=copy.deepcopy(history)
    summary_req=oracle(binary,{'op':'summary_request','value':source});summary_response=oracle(binary,{'op':'answer','request':summary_req,'text':'Goal ORCHID. Keep constraints.'})
    summary=oracle(binary,{'op':'summary','request':source,'response':summary_response})
    native={'object':'response.compaction','id':'compact_fixture','output':[{'type':'compaction','id':'encrypted_compact','encrypted_content':'SYNTHETIC_ONLY'},{'role':'user','content':'Goal ORCHID retained'}]}
    for name,native_value in [('summary',summary),('native-compact',native)]:
        events=simple+[accepted(config,history,4,compact_first=True),transition('compacted',4,compaction={'request':source,'native':native_value})]
        probe=oracle(binary,{'op':'state','session':'fixture','configuration':config,'events':events})
        check(all(step['accepted'] for step in probe['steps']),'compaction source trace')
        answer=oracle(binary,{'op':'answer','request':probe['state']['turn']['next'],'text':'ORCHID after compaction'})
        traces[name]=events+[transition('response',4,response=answer),transition('done',4)]
    two=copy.deepcopy(first);second_call=copy.deepcopy(call);second_call.update(id='call_item_second',call_id='call_second');two['items'].append(second_call)
    two=oracle(binary,{'op':'response','value':two})
    prefix=[init,start,transition('response',response=two),transition('intent',invocation=intent),transition('outcome',result=result)]
    intent2=copy.deepcopy(intent);intent2.update(operation='run1:op:2',call='call_second');result2=copy.deepcopy(result);result2.update(operation='run1:op:2',call='call_second')
    traces['batch-prefix']=prefix
    following=copy.deepcopy(start['next']);following['input']+=two['items']+[{'type':'function_call_output','call_id':r['call'],'output':r['output']} for r in (result,result2)]
    terminal=oracle(binary,{'op':'answer','request':following,'text':'Both known results'})
    traces['batch-completed']=prefix+[transition('intent',invocation=intent2),transition('outcome',result=result2),transition('continued'),transition('response',response=terminal),transition('done')]
    next_round=copy.deepcopy(first);next_round['request']=next_request;next_round['id']='response_round2'
    next_round=oracle(binary,{'op':'response','value':next_round})
    round_intent=copy.deepcopy(intent);round_intent['operation']='run1:op:2';round_result=copy.deepcopy(result);round_result['operation']='run1:op:2'
    last=copy.deepcopy(next_request);last['input']+=next_round['items']+[{'type':'function_call_output','call_id':call['call_id'],'output':result['output']}]
    last_answer=oracle(binary,{'op':'answer','request':last,'text':'Two rounds complete'})
    traces['multi-round']=completed[:6]+[transition('response',response=next_round),transition('intent',invocation=round_intent),transition('outcome',result=round_result),transition('continued'),transition('response',response=last_answer),transition('done')]
    value={'schema_version':1,'configuration':config,'traces':traces,'source':source,'summary_request':summary_req,'summary_response':summary_response,'summary':summary,'native':native}
    GOLDEN.parent.mkdir(parents=True,exist_ok=True);GOLDEN.write_text(json.dumps(value,ensure_ascii=False,indent=2)+'\n')
    import tempfile,os
    for name,events in list(traces.items())+[('minimal',[{'kind':'test','value':1}])]:
        with tempfile.TemporaryDirectory(prefix='ansa-m4-') as temporary:
            root=Path(temporary);marker=root/'fixture-owner';marker.write_bytes(b'ansa-m4-synthetic-v1\n');marker.chmod(0o600)
            result=oracle(binary,{'op':'journal','root':str(root),'events':events})
            check(all(step['accepted'] for step in result['appends']),'golden journal append')
            (GOLDEN.parent/(name+'.log')).write_bytes((root/'journal/part-00000000000000000000.log').read_bytes())
    return value
def golden():return json.loads(GOLDEN.read_bytes())
def cases():
    data=golden();config=data['configuration'];rows=[]
    def add(id,req,accepted=True,**expect):rows.append({'id':id,'request':req,'accepted':accepted,**expect})
    def state(events):return {'op':'state','session':'fixture','configuration':config,'plan_tools':config,'events':events,'queries':[{'before':0,'limit':64},{'before':0,'limit':1,'forward':True}]}
    for name,events in data['traces'].items():
        add('trace-'+name,state(events),all_steps=True)
        for cut in range(1,len(events)+1):add(f'prefix-{name}-{cut}',state(events[:cut]),all_steps=True)
    full=data['traces']['completed']
    for i,event in enumerate(full):
        for field in event:
            altered=copy.deepcopy(event);del altered[field]
            # Legacy mode/limits are optional; compact is required but type is loose.
            if field in ('mode','round_limit','call_limit'):continue
            add(f'missing-{i}-{field}',state(full[:i]+[altered]),last_rejected=True)
        if i>1:add(f'wrong-turn-{i}',state(full[:i]+[dict(event,turn='other')]),last_rejected=True)
    # Every event kind has stage/identity negative controls.
    for name,events in data['traces'].items():
        for i,event in enumerate(events):
            if event['kind']!='init':add(f'without-init-{name}-{i}',state([event]),last_rejected=True)
    for execution in range(5):
        for effects in range(4):
            for dispatched in (False,True):
                result=dict(full[4]['result'],execution=execution,effects=effects,dispatched=dispatched)
                valid=execution<=3 and effects<=2 and (dispatched or effects==0 and execution!=0)
                add(f'result-{execution}-{effects}-{int(dispatched)}',{'op':'result','value':result},valid)
    add('codec-request',{'op':'request','value':config})
    for i in (2,6):add('codec-response-'+str(i),{'op':'response','value':full[i]['response']})
    add('summary-request',{'op':'summary_request','value':data['source']})
    add('summary-native',{'op':'summary','request':data['source'],'response':data['summary_response']})
    for name in ('summary','native'):add('compact-'+name,{'op':'compact','request':data['source'],'native':data[name]})
    for field in ('output','summary_response'):
        native=copy.deepcopy(data['summary']);native.pop(field)
        add('summary-missing-'+field,{'op':'compact','request':data['source'],'native':native},False)
    native=copy.deepcopy(data['summary']);native['output']=[]
    add('summary-output-conflict',{'op':'compact','request':data['source'],'native':native},False)
    for field,value in [('object','wrong'),('id',''),('output',[]),('output',[{'role':'assistant','content':'invalid'}]),('output',[{'type':'compaction','id':'x','encrypted_content':''}])]:
        native=copy.deepcopy(data['native']);native[field]=value
        add('compact-invalid-'+field+'-'+str(len(rows)),{'op':'compact','request':data['source'],'native':native},False)
    # Resume never resolves outstanding/unknown effects or permits duplicate IDs.
    for name in ('outstanding-intent','effect-unknown'):
        add('resume-denied-'+name,state(data['traces'][name]+[transition('resume',command='resume1')]),last_rejected=True)
    resumed=data['traces']['paused']+[transition('resume',command='resume1')]
    add('resume-duplicate',state(resumed+[transition('resume',command='resume1')]),last_rejected=True)
    for field,value in [('operation','forged:op:1'),('call','other'),('name','other'),('revision','other'),('arguments',{'extra':True})]:
        invocation=dict(full[3]['invocation']);invocation[field]=value
        add('intent-conflict-'+field,state(full[:3]+[transition('intent',invocation=invocation)]),last_rejected=True)
    for field,value in [('operation','other'),('call','other'),('execution',0)]:
        result=dict(full[4]['result']);result[field]=value
        if field=='execution':result['effects']=2
        add('outcome-conflict-'+field,state(full[:4]+[transition('outcome',result=result)]),last_rejected=True)
    return rows
def output_pass(case,row):
    check(type(row.get('accepted')) is bool and row['accepted']==case['accepted'],'session top-level acceptance')
    if not row['accepted']:
        check(set(row)=={'accepted','diagnostic'} and isinstance(row['diagnostic'],str) and row['diagnostic'].isascii() and 0<len(row['diagnostic'])<=80,'session diagnostic schema');return
    value=row['value']
    if case.get('all_steps'):check(all(step['accepted'] is True for step in value['steps']),'valid trace rejected')
    if case.get('last_rejected'):
        steps=value['steps'];check(steps[-1]['accepted'] is False,'invalid event accepted')
        if len(steps)>1:check(steps[-1]['state']==steps[-2]['state'],'rejected event changed state')
    if case['id']=='trace-completed':
        check(value['state']['turn']['stage']=='completed' and value['state']['unknown'] is None,'completed state')
        check(value['state']['history_bytes']==len(compact(value['state']['history']).encode()),'history bytes')
        entries=value['pages'][0]['entries'];check([e['role'] for e in entries]==['user','assistant'] and 'OMH_FIXED_VALUE_42' in entries[-1]['text'],'durable transcript')
    if case['id']=='trace-outstanding-intent':check(value['state']['unknown']=='run1:op:1' and len(value['state']['history'])==0,'unknown operation identity')
    if case['id']=='trace-batch-prefix':check(value['state']['turn']['results'][0]['operation']=='run1:op:1' and value['state']['unknown'] is None,'known batch prefix')
    if case['id']=='trace-multi-round':check(value['state']['turn']['operation_offset']==2 and len(value['state']['turn']['previous_results'])==2,'round accumulation')
    if case['id']=='trace-plan':check(value['pages'][0]['entries'][0]['mode']=='plan' and value['state']['configuration']['epoch']==golden()['configuration']['epoch'],'plan epoch binding')
    if case['id']=='compact-native':check(value['epoch']['id']=='compact_1_'+digest(golden()['native'])[:16] and value['epoch']['exact_state']['compaction_digest']==digest(golden()['native']),'exact compact epoch')

def part_header(index,previous):return f'OMHJ1 {index:020} {previous}\n'.encode()
def frame(event,sequence,previous):
    body=compact({'schema_version':1,'sequence':sequence,'previous':previous,'event':event}).encode();checksum=digest(body)
    return f'{len(body):08x} {len(body)^0xffffffff:08x} {checksum}\n'.encode()+body,checksum
def encode(events):
    data=part_header(0,'0'*64);previous='0'*64
    for sequence,event in enumerate(events,1):encoded,previous=frame(event,sequence,previous);data+=encoded
    return data
def decode(parts):
    events=[];previous_part='0'*64;previous_record='0'*64;sequence=0
    for index,(name,data) in enumerate(sorted(parts.items())):
        check(name==f'part-{index:020}.log','part sequence')
        head=part_header(index,previous_part);check(head.startswith(data[:len(head)]),'part header')
        previous_part=digest(data)
        if len(data)<len(head):continue
        offset=len(head)
        while offset<len(data):
            if len(data)-offset<83:break
            prefix=data[offset:offset+83];check(prefix[8:9]==b' ' and prefix[17:18]==b' ' and prefix[-1:]==b'\n','frame header')
            check(all(c in b'0123456789abcdefABCDEF' for c in prefix[:8]+prefix[9:17]),'frame hex');length=int(prefix[:8],16);complement=int(prefix[9:17],16);check(0<length<=32*1024*1024 and length^complement==0xffffffff,'frame length')
            body=data[offset+83:offset+83+length]
            if len(body)<length:break
            checksum=digest(body);check(checksum.encode()==prefix[18:82],'frame checksum')
            import auth
            value=auth.json_bytes(body);check(type(value['schema_version']) is int and type(value['sequence']) is int and value['schema_version']==1 and value['sequence']==sequence+1 and value['previous']==previous_record,'record chain')
            events.append(value['event']);sequence+=1;previous_record=checksum;offset+=83+length
    return events

if __name__=='__main__':
    import sys
    generate(Path(sys.argv[1]).resolve())
    print('Generated synthetic session fixtures:',len(golden()['traces']),'traces;',len(cases()),'pure cases')
