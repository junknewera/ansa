"""M1 risk corpus, differential oracle and evidence checks, stdlib only."""
from datetime import datetime, timezone
import hashlib
import json
import math
from pathlib import Path
import random
import subprocess
import sys
import tempfile

import run as common
import risk_process
import risk_transport

FIXTURES = common.ROOT / 'tests/parity/fixtures'
MANIFEST = FIXTURES / 'risk.json'
JSON_IDS = ('null','booleans','number-lexemes','huge-int','negative-huge-int','huge-state',
            'decoded-strings','control-escape-writer','escaped-slash','opaque-order','zero-exponent',
            'subnormal','round-up-subnormal','duplicate-key','decoded-duplicate','nul-duplicate',
            'lone-surrogate','float-overflow','float-underflow','round-down-underflow','trailing-data',
            'trailing-comma','leading-zero','negative-leading-zero','nan','comment','invalid-escape',
            'depth-63','depth-64','object-equality','array-order','numeric-equality','decoded-equality','invalid-comparison')
CPP_PROCESS = ('basic','cancel','timeout','crash','detached','evidence','signals','guardian-crash')
DENIED_WIRE = {'http-duplicate-mime','http-wrong-mime','http-missing-mime','http-bad-charset',
               'http-redirect','http-truncated','http-mime-trailer','http-eof-headers','http-large-headers',
               'http-header-count','http-control-header','http-body-limit','tls-invalid-ca','tls-hostname',
               'proxy-connect-denied','proxy-socks-denied','proxy-unsupported','h2-mime-trailer',
               'h2-reset','h2-duplicate-mime','h2-large-headers','h2-invalid-ca','http-head-trailer-count',
               'http-info-byte-budget','http-long-reason'}


def manifest():
    value = json.loads(MANIFEST.read_bytes())
    if value.get('schema_version') != 1 or value.get('suite') != 'risk':
        raise ValueError('unsupported risk manifest')
    if [row['id'] for row in value['json_cases']] != list(JSON_IDS):
        raise ValueError('required JSON cases missing/duplicate/reordered')
    if [row['id'] for row in value['hash_domains']] != ['hash-domains-'+str(i) for i in range(5)]:
        raise ValueError('required hash cases missing')
    if value['number_fuzz'] != {'seed':4501721,'count':512} or value['generated_limits'] != {'nodes_accept_leaves':131071,'nodes_reject_leaves':131072}:
        raise ValueError('required generated corpus differs')
    expected_files = {'transport/tls/'+name for name in ('ca.pem','ca.der','server.pem','server.der','server-key.pem','server-key.der')}
    if set(value['fixture_files']) != expected_files:
        raise ValueError('required synthetic TLS fixtures differ')
    for name, digest in value['fixture_files'].items():
        path = FIXTURES / name
        if path.is_symlink() or not path.resolve().is_relative_to(FIXTURES.resolve()) or common.sha(path) != digest:
            raise ValueError('synthetic fixture identity differs')
    deviations = value['reviewed_deviations']
    if set(deviations) != {'http-obsfold','http-info-count','http-chunk-budget'} or any(row['cpp'] is not True or row['rust'] is not False or not row['reason'] for row in deviations.values()):
        raise ValueError('unreviewed deviation policy')
    return value


def json_cases(data):
    rows = [dict(row) for row in data['json_cases']]
    generator = random.Random(data['number_fuzz']['seed'])
    for index in range(data['number_fuzz']['count']):
        integer = str(generator.randrange(0,10**18))
        fraction = f'{generator.randrange(0,10**6):06}'
        sign = '-' if generator.randrange(2) else ''
        token = f'{sign}{integer}.{fraction}e{generator.randrange(-500,501):+}'
        number = float(token)
        accepted = math.isfinite(number) and (number != 0 or all(c == '0' for c in integer+fraction))
        rows.append({'id':f'number-fuzz-{index:03}','payload':token,'accepted':accepted})
    for accepted,name in ((True,'nodes_accept_leaves'),(False,'nodes_reject_leaves')):
        rows.append({'id':name,'payload':'['+','.join(['null']*data['generated_limits'][name])+']','accepted':accepted})
    return rows


def paths(cpp_build, rust_target):
    return {'cpp':Path(cpp_build).resolve()/'ansa_risk_oracle',
            'cpp_process':Path(cpp_build).resolve()/'ansa_process_tests',
            'rust':Path(rust_target).resolve()/'debug/examples/risk_adapter',
            'rust_worker':Path(rust_target).resolve()/'debug/ansa-rs'}


def binary_info(binaries):
    return {name:{'path':str(path),'sha256':common.sha(path)} for name,path in binaries.items()}


def raw(path):
    return {'file':path.name,'sha256':common.sha(path)}


def batch(binary, requests, directory, label, cwd, commands):
    path = directory/(label+'.stdin')
    path.write_bytes(b''.join((json.dumps(row,ensure_ascii=True)+'\n').encode() for row in requests))
    result = common.command([binary],cwd,directory,label,timeout=120,input_file=path)
    result['stdin'] = raw(path)
    commands[label] = result
    if not common.succeeded(result):
        raise ValueError(label+' adapter failed or timed out')
    output = directory/result['stdout']['file']
    if output.stat().st_size > 64*1024*1024:
        raise ValueError('risk adapter output limit')
    rows = [json.loads(line) for line in output.read_bytes().splitlines()]
    if len(rows) != len(requests):
        raise ValueError(label+' response count differs')
    return rows


def assert_json(case, output):
    if output.get('accepted') is not case['accepted']:
        return False
    if not case['accepted']:
        return output == {'accepted':False}
    if not isinstance(output.get('dump'),str) or output.get('sha256') != hashlib.sha256(output['dump'].encode()).hexdigest():
        return False
    if 'equal' in case and output.get('equal') is not case['equal']:
        return False
    if case['id'] in ('number-lexemes','huge-int','negative-huge-int') and output['dump'] != case['payload']:
        return False
    return True


def domain_pass(a,b):
    if a!=b or set(a)!={'tool','instruction','plan','compact','evidence'}: return False
    for domain,value in a.items():
        preimage=value.get('bytes')
        if not isinstance(preimage,str): return False
        digest=hashlib.sha256(preimage.encode()).hexdigest()
        expected='compact_1_'+digest[:16] if domain=='compact' else digest
        if value.get('hash')!=expected: return False
    return True


def wire_pass(case,outcomes,data):
    if not all(isinstance(outcomes[name],dict) and outcomes[name].get('executors')==0 for name in ('cpp','rust')): return False
    deviation=data['reviewed_deviations'].get(case)
    if deviation:
        expected_prefix=14000 if case=='http-chunk-budget' else 0
        return (outcomes['cpp'].get('accepted') is True
                and outcomes['rust']=={'accepted':False,'executors':0,'uncommitted_body_bytes':expected_prefix})
    if case in ('http-info-byte-budget','http-long-reason'):
        # curl rejects while reading the head; Hyper's cumulative raw-wire
        # guard rejects at EOF. Neither prefix can become admitted history.
        return (outcomes['cpp']=={'accepted':False,'executors':0,'uncommitted_body_bytes':0}
                and outcomes['rust']=={'accepted':False,'executors':0,'uncommitted_body_bytes':len(risk_transport.BODY)})
    return outcomes['cpp']==outcomes['rust'] and outcomes['cpp'].get('accepted') is (case not in DENIED_WIRE)


def expected_cases(mode,data):
    rows = json_cases(data)
    if mode == 'compare':
        return [r['id'] for r in rows]+[r['id'] for r in data['hash_domains']]+list(risk_transport.CASES)
    return [r['id'] for r in rows if not r['accepted']]+list(risk_transport.CASES)+list(risk_process.CASES)+['cpp-process-'+name for name in CPP_PROCESS]


def expected_commands(mode):
    labels={'json-cpp','json-rust'}
    labels |= {'wire-'+case+'-'+name for case in risk_transport.CASES for name in ('cpp','rust')}
    if mode=='compare': labels |= {'domains-cpp','domains-rust'}
    else: labels |= {'process-rust'} | {'process-cpp-'+name for name in CPP_PROCESS}
    return labels


def execute(args,directory,data):
    binaries = paths(args.cpp_build,args.rust_target)
    report = {'schema_version':1,'suite':'risk','mode':args.action,'status':'FAIL',
              'created_at_utc':datetime.now(timezone.utc).isoformat(),'source':common.source_info(),
              'manifest_sha256':common.sha(MANIFEST),'platform':common.platform_info(),
              'binaries':binary_info(binaries),'commands':{},'cases':[],'errors':[]}
    commands,cases,errors=report['commands'],report['cases'],report['errors']
    with tempfile.TemporaryDirectory(prefix='ansa-risk-cwd-') as cwd:
        rows=json_cases(data)
        if args.action=='fault': rows=[row for row in rows if not row['accepted']]
        requests=[{'op':'json',**{key:row[key] for key in ('payload','other') if key in row}} for row in rows]
        outputs={name:batch(binaries[name],requests,directory,'json-'+name,cwd,commands) for name in ('cpp','rust')}
        for index,row in enumerate(rows):
            passed=all(assert_json(row,outputs[name][index]) for name in ('cpp','rust')) and outputs['cpp'][index]==outputs['rust'][index]
            cases.append({'id':row['id'],'status':'pass' if passed else 'failed'})
        if args.action=='compare':
            requests=[{'op':'domains','payload':row['payload']} for row in data['hash_domains']]
            outputs={name:batch(binaries[name],requests,directory,'domains-'+name,cwd,commands) for name in ('cpp','rust')}
            for index,row in enumerate(data['hash_domains']):
                a,b=outputs['cpp'][index],outputs['rust'][index]
                passed=domain_pass(a,b)
                cases.append({'id':row['id'],'status':'pass' if passed else 'failed'})
        for case in risk_transport.CASES:
            outcomes={}
            for name in ('cpp','rust'):
                label='wire-'+case+'-'+name
                try:
                    result=risk_transport.run_case(case,[binaries[name]],binaries['rust'],FIXTURES,cwd)
                    record={'argv':result['argv'],'exit_code':result['exit_code'],'timed_out':result.get('timed_out',False),'observations':result['observations']}
                    for stream in ('stdout','stderr'):
                        path=directory/(label+'.'+stream); path.write_bytes(result[stream]); record[stream]=raw(path)
                    path=directory/(label+'.stdin'); path.write_bytes((json.dumps(result['request'])+'\n').encode()); record['stdin']=raw(path)
                    commands[label]=record
                    risk_transport.validate_request(case,result['request'],FIXTURES)
                    outcomes[name]=risk_transport.validate_case(case,result)
                except (OSError,ValueError,AssertionError,subprocess.SubprocessError) as error:
                    errors.append(label+': '+str(error)); outcomes[name]=None
            passed=wire_pass(case,outcomes,data)
            deviation=data['reviewed_deviations'].get(case)
            cases.append({'id':case,'status':'pass' if passed else 'failed',
                          'deviation':deviation['reason'] if deviation else None})
        if args.action=='fault':
            result=common.command([sys.executable,common.ROOT/'tests/parity/risk_process.py',binaries['rust_worker']],cwd,directory,'process-rust',timeout=30)
            commands['process-rust']=result
            if not common.succeeded(result): raise ValueError('Rust process risk driver failed')
            process=common.output_json(result,directory)
            if process.get('schema_version')!=1 or [row['id'] for row in process.get('cases',[])]!=list(risk_process.CASES): raise ValueError('process risk inventory differs')
            cases.extend(process['cases'])
            for name in CPP_PROCESS:
                result=common.command([binaries['cpp_process'],name],cwd,directory,'process-cpp-'+name,timeout=30)
                commands['process-cpp-'+name]=result
                cases.append({'id':'cpp-process-'+name,'status':'pass' if common.succeeded(result) else 'failed'})
    if [row['id'] for row in cases]!=expected_cases(args.action,data): errors.append('required risk case inventory differs')
    errors += [row['id']+' failed' for row in cases if row['status']!='pass']
    if report['source']['files']!=common.source_info()['files'] or report['binaries']!=binary_info(binaries): errors.append('inputs changed during execution')
    report['status']='FAIL' if errors else 'PASS'
    return report


def capture(args):
    directory=None; created=False
    try:
        if not all((args.cpp_build,args.rust_target,args.output)): raise ValueError('risk suite requires --cpp-build --rust-target --output')
        directory=Path(args.output).resolve(); directory.mkdir(parents=True,exist_ok=False)
        created=True
        data=manifest()
        report=execute(args,directory,data)
    except (OSError,ValueError,KeyError,TypeError,AssertionError,subprocess.SubprocessError) as error:
        report={'schema_version':1,'suite':'risk','mode':args.action,'status':'FAIL','errors':[str(error)],'cases':[]}
    if created: common.write_json(directory/'risk-report.json',report)
    print(json.dumps({'suite':'risk','mode':args.action,'status':report['status'],'cases':len(report['cases']),'errors':report['errors']},indent=2))
    return int(report['status']!='PASS')


def check(evidence,fault_evidence=None):
    errors=[]; data=manifest()
    paths_={'compare':evidence,'fault':Path(fault_evidence).resolve() if fault_evidence else Path(str(evidence)+'-fault')}
    for mode,directory in paths_.items():
        report=json.loads((directory/'risk-report.json').read_bytes())
        if report.get('schema_version')!=1 or report.get('suite')!='risk' or report.get('mode')!=mode or report.get('status')!='PASS' or report.get('errors'): errors.append(mode+' did not pass')
        cases=report.get('cases',[])
        if [row['id'] for row in cases]!=expected_cases(mode,data) or any(row.get('status')!='pass' for row in cases): errors.append(mode+' required cases differ/failed/skipped')
        if report.get('manifest_sha256')!=common.sha(MANIFEST) or report.get('source',{}).get('files')!=common.source_info()['files']: errors.append(mode+' source/fixtures differ')
        if set(report.get('binaries',{}))!={'cpp','cpp_process','rust','rust_worker'}: errors.append(mode+' required binary identities missing')
        for item in report.get('binaries',{}).values():
            if common.sha(Path(item['path']))!=item['sha256']: errors.append(mode+' binary differs')
        if set(report.get('commands',{}))!=expected_commands(mode): errors.append(mode+' required commands missing/different')
        for name,result in report.get('commands',{}).items():
            if not common.succeeded(result): errors.append(name+' command failed')
            if 'stdout' not in result or 'stderr' not in result or ((name.startswith('json-') or name.startswith('domains-') or name.startswith('wire-')) and 'stdin' not in result): errors.append(name+' raw evidence missing')
            for stream in ('stdin','stdout','stderr'):
                if stream in result:
                    path=directory/result[stream]['file']
                    if path.name!=name+'.'+stream or path.is_symlink() or path.parent!=directory or common.sha(path)!=result[stream]['sha256']: errors.append(name+' raw identity/digest differs')
        try:
            replay(report,directory,data)
        except (ValueError,KeyError,TypeError,AssertionError) as error:
            errors.append(mode+' raw validation failed: '+str(error))
        host=report.get('platform',{})
        if host.get('system')!='Linux' or host.get('machine')!='aarch64' or host.get('os_release',{}).get('ID')!='nixos': errors.append('native NixOS/aarch64 evidence required')
    print(json.dumps({'milestone':'M1','status':'FAIL' if errors else 'PASS_FOR_RISK_CAPTURES','errors':errors},indent=2))
    return int(bool(errors))


def replay(report,directory,data):
    """Recompute admission from retained bytes, never trust PASS metadata alone."""
    commands=report['commands']
    def read(label,stream):
        path=directory/commands[label][stream]['file']
        if path.parent!=directory or path.is_symlink() or path.stat().st_size>64*1024*1024: raise ValueError('raw path/size')
        return path.read_bytes()
    def batch_outputs(label,expected):
        name=label.rsplit('-',1)[1]
        if commands[label]['argv']!=[report['binaries'][name]['path']]: raise ValueError('adapter identity')
        requests=[json.loads(line) for line in read(label,'stdin').splitlines()]
        if requests!=expected: raise ValueError(label+' input inventory')
        output=[json.loads(line) for line in read(label,'stdout').splitlines()]
        if len(output)!=len(expected): raise ValueError(label+' output inventory')
        return output
    rows=json_cases(data)
    if report['mode']=='fault': rows=[row for row in rows if not row['accepted']]
    requests=[{'op':'json',**{key:row[key] for key in ('payload','other') if key in row}} for row in rows]
    outputs={name:batch_outputs('json-'+name,requests) for name in ('cpp','rust')}
    for index,row in enumerate(rows):
        if not all(assert_json(row,outputs[name][index]) for name in ('cpp','rust')) or outputs['cpp'][index]!=outputs['rust'][index]: raise ValueError(row['id']+' admission')
    if report['mode']=='compare':
        requests=[{'op':'domains','payload':row['payload']} for row in data['hash_domains']]
        outputs={name:batch_outputs('domains-'+name,requests) for name in ('cpp','rust')}
        if not all(domain_pass(a,b) for a,b in zip(outputs['cpp'],outputs['rust'])): raise ValueError('hash preimages')
    for case in risk_transport.CASES:
        outcomes={}
        for name in ('cpp','rust'):
            label='wire-'+case+'-'+name
            request=json.loads(read(label,'stdin'))
            risk_transport.validate_request(case,request,FIXTURES)
            if commands[label]['argv']!=[report['binaries'][name]['path']]: raise ValueError('wire adapter identity')
            record={**commands[label],'stdout':read(label,'stdout'),'stderr':read(label,'stderr')}
            outcomes[name]=risk_transport.validate_case(case,record)
        if not wire_pass(case,outcomes,data): raise ValueError(case+' admission')
    if report['mode']=='fault':
        process=json.loads(read('process-rust','stdout'))
        if process.get('schema_version')!=1 or [row.get('id') for row in process.get('cases',[])]!=list(risk_process.CASES) or any(row.get('status')!='pass' for row in process['cases']): raise ValueError('process results')
