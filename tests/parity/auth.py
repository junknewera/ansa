"""M3 native auth capture and independent replay of retained raw evidence."""
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys

import auth_cases as vectors
import auth_store_cases as stores
import protocol
import run as common

MANIFEST=common.ROOT/'tests/parity/fixtures/auth.json'
MAX=64*1024*1024


def json_bytes(data):
    return json.loads(data,object_pairs_hook=protocol._pairs,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError('nonfinite metadata')))


def manifest():
    data=json_bytes(protocol.owned_bytes(MANIFEST,4*1024*1024))
    stores.check(data.get('schema_version')==1 and type(data.get('schema_version')) is int and data.get('suite')=='auth','auth manifest identity')
    stores.check(data.get('native_suites')==vectors.NATIVE and data.get('reviewed_deviations')==vectors.DEVIATIONS,'auth suite/deviation inventory')
    expected={'tests/fixtures/auth/fixture-private.der','tests/fixtures/auth/fixture-public.der','tests/fixtures/auth/golden.json'}
    stores.check(set(data.get('fixtures',{}))==expected,'auth fixture inventory')
    for path,digest in data['fixtures'].items():
        stores.check(hashlib.sha256(protocol.owned_bytes(common.ROOT/path,256*1024)).hexdigest()==digest,'auth fixture differs')
    native=(common.ROOT/'tests/auth_tests.cpp').read_text()
    rust=(common.ROOT/'rust/crates/ansa/tests/auth_compat.rs').read_text()
    for suite in vectors.NATIVE:
        stores.check('"'+suite+'"' in native and '"'+suite+'"' in rust,'native auth source anchor missing')
    for mode in ('compare','fault'):
        stores.check(data.get(mode+'_ids')==case_ids(mode),'auth case inventory differs')
    return data


def case_ids(mode):
    return [s['id'] for s in vectors.jwt_specs()]+[s['id'] for s in vectors.simple_cases()]+stores.case_ids(mode)


def paths(cpp,rust):
    return {'cpp':(common.ROOT/Path(cpp)).resolve()/'ansa_auth_oracle',
            'rust':(common.ROOT/Path(rust)).resolve()/'debug/examples/auth_adapter'}


def binary_info(binaries):
    expected={'cpp':'ansa_auth_oracle','rust':'auth_adapter'}
    stores.check(set(binaries)==set(expected),'binary inventory')
    result={}
    for adapter,path in binaries.items():
        path=Path(path)
        stores.check(path.name==expected[adapter],'auth binary name')
        result[adapter]={'path':str(path),'sha256':hashlib.sha256(protocol.owned_bytes(path,256*1024*1024,executable=True)).hexdigest()}
    return result


def command_ok(command,exit_code=0):
    if not isinstance(command,dict) or type(command.get('exit_code')) is not int or command['exit_code']!=exit_code or command.get('timed_out') is not False:
        return False
    elapsed=command.get('elapsed_seconds')
    return type(elapsed) in (int,float) and math.isfinite(elapsed) and 0<=elapsed<=3600


class Evidence:
    def __init__(self,directory,binaries,commands=None):
        self.directory=Path(directory); self.binaries=binaries
        self.commands={} if commands is None else commands; self.used=set()

    def call(self,label,adapter,request,cwd=None,raw_input=None):
        stores.check(label not in self.commands,'duplicate auth command')
        source=self.directory/(label+'.stdin')
        source.write_bytes(raw_input if raw_input is not None else (vectors.compact(request)+'\n').encode())
        command=common.command([self.binaries[adapter]],cwd or common.ROOT,self.directory,label,timeout=60,input_file=source,fixture_root=common.ROOT/'tests/fixtures/auth')
        command['stdin']=protocol.raw(source)
        self.commands[label]=command
        return label

    def read_call(self,command,label,adapter,request,exit_code=0,cwd=None,raw_input=None):
        stores.check(command==label and label not in self.used,'auth command reference')
        self.used.add(label)
        value=self.commands[label]
        stores.check(command_ok(value,exit_code) and value.get('argv')==[str(self.binaries[adapter])],'auth command failed or wrong argv')
        stores.check(value.get('fixture_root')==str(common.ROOT/'tests/fixtures/auth'),'auth fixture input differs')
        stores.check(value.get('cwd')==str(cwd or common.ROOT),'auth command cwd differs')
        retained={stream:protocol.retained_bytes(self.directory,value.get(stream),label+'.'+stream,MAX)
                  for stream in ('stdin','stdout','stderr')}
        expected=raw_input if raw_input is not None else (vectors.compact(request)+'\n').encode()
        stores.check(retained['stdin']==expected,'auth command input differs')
        if exit_code:
            stores.check(not retained['stdout'],'crashed command returned admission')
            return None
        lines=retained['stdout'].splitlines()
        stores.check(len(lines)==1,'auth command output inventory')
        return protocol.strict_loads(lines[0])


def batch(r,label,adapter,requests):
    raw=b''.join((vectors.compact(request)+'\n').encode() for request in requests)
    return r.call(label,adapter,None,raw_input=raw)


def read_batch(r,label,adapter,requests):
    stores.check(label not in r.used,'duplicate auth batch')
    r.used.add(label)
    value=r.commands[label]
    stores.check(command_ok(value) and value.get('argv')==[str(r.binaries[adapter])] and value.get('cwd')==str(common.ROOT),'auth batch command')
    stores.check(value.get('fixture_root')==str(common.ROOT/'tests/fixtures/auth'),'auth batch fixture input differs')
    retained={stream:protocol.retained_bytes(r.directory,value.get(stream),label+'.'+stream,MAX)
              for stream in ('stdin','stdout','stderr')}
    expected=b''.join((vectors.compact(request)+'\n').encode() for request in requests)
    stores.check(retained['stdin']==expected,'auth batch requests differ')
    lines=retained['stdout'].splitlines()
    stores.check(len(lines)==len(requests),'auth batch row inventory')
    return [protocol.strict_loads(line) for line in lines]


def parse_generated(r):
    specs=vectors.jwt_specs(); requests=[s['generate'] for s in specs]
    values=read_batch(r,'auth-generate','cpp',requests)
    # Convert only the fixture container; claims remain exact signed bytes.
    fixtures=[json_bytes(protocol.exact_dumps(value)) for value in values]
    return [vectors.materialize(spec,fixture) for spec,fixture in zip(specs,fixtures)]+vectors.simple_cases()


def verify(report,directory):
    errors=[]
    try:
        data=manifest()
        stores.check(report.get('schema_version')==1 and type(report.get('schema_version')) is int and report.get('suite')=='auth'
                     and report.get('mode') in ('compare','fault'),'auth report identity')
        mode=report['mode']
        stores.check(report.get('manifest_sha256')==common.sha(MANIFEST) and report.get('fixtures')==data['fixtures'],'auth fixture identity')
        stores.check(report.get('source',{}).get('files')==common.source_info()['files'],'auth source snapshot differs')
        host=report.get('platform',{})
        stores.check(host.get('system')=='Linux' and host.get('machine')=='aarch64' and host.get('os_release',{}).get('ID')=='nixos','auth native platform')
        identities=report.get('binaries',{})
        stores.check(isinstance(identities,dict) and set(identities)=={'cpp','rust'},'auth binary identities')
        binaries={name:Path(item['path']) for name,item in identities.items()}
        stores.check(identities==binary_info(binaries),'auth adapter binary changed')
        r=Evidence(directory,binaries,report['commands'])
        cases=parse_generated(r)
        expected_ids=[case['id'] for case in cases]+stores.case_ids(mode)
        rows=report.get('cases',[])
        stores.check([row.get('id') for row in rows]==expected_ids and len(expected_ids)==len(set(expected_ids)),'auth cases missing/duplicate/reordered')
        stores.check(all(row.get('status')=='pass' for row in rows),'auth report contains failed/skipped case')
        outputs={adapter:read_batch(r,'auth-'+mode+'-'+adapter,adapter,[case['request'] for case in cases]) for adapter in ('cpp','rust')}
        for index,case in enumerate(cases):
            for adapter in ('cpp','rust'):
                stores.check(vectors.output_pass(case,outputs[adapter][index],adapter),'auth independent output oracle failed: '+case['id']+'-'+adapter)
            if case['id'] not in vectors.DEVIATIONS:
                # Rejection diagnostics can identify different failed guards;
                # their bounded static schema is checked above. Successful
                # values, arrays, numbers and hashes are compared exactly.
                if outputs['cpp'][index].get('accepted') is not False:
                    stores.check(protocol.structural(outputs['cpp'][index])==protocol.structural(outputs['rust'][index]),'auth values differ: '+case['id'])
        store_rows=rows[len(cases):]
        for row in store_rows:
            stores.validate(r,row)
        stores.check(r.used==set(r.commands),'auth command inventory missing/unreferenced')
        stores.check(report.get('status')=='PASS' and report.get('errors')==[],'auth report claims no valid pass')
    except (OSError,ValueError,KeyError,TypeError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as error:
        errors.append(str(error) if isinstance(error,ValueError) else 'invalid or unavailable auth evidence: '+type(error).__name__)
    return errors


def execute(args,directory):
    data=manifest(); binaries=paths(args.cpp_build,args.rust_target)
    before=common.source_info(); identities=binary_info(binaries)
    report={'schema_version':1,'suite':'auth','mode':args.action,'status':'FAIL','created_at_utc':datetime.now(timezone.utc).isoformat(),
            'source':before,'platform':common.platform_info(),'manifest_sha256':common.sha(MANIFEST),'fixtures':data['fixtures'],
            'binaries':identities,'commands':{},'cases':[],'errors':[]}
    r=Evidence(directory,binaries,report['commands'])
    try:
        specs=vectors.jwt_specs(); batch(r,'auth-generate','cpp',[s['generate'] for s in specs])
        cases=parse_generated(r); r.used.clear()
        outputs={}
        for adapter in ('cpp','rust'):
            label='auth-'+args.action+'-'+adapter
            batch(r,label,adapter,[case['request'] for case in cases])
            outputs[adapter]=read_batch(r,label,adapter,[case['request'] for case in cases])
        for index,case in enumerate(cases):
            passed=all(vectors.output_pass(case,outputs[adapter][index],adapter) for adapter in ('cpp','rust'))
            report['cases'].append({'id':case['id'],'status':'pass' if passed else 'failed'})
        report['cases'].extend(stores.capture(r,args.action))
        for row in report['cases'][len(cases):]:
            try: stores.validate(Evidence(directory,binaries,report['commands']),row); row['status']='pass'
            except (OSError,ValueError,KeyError,TypeError,IndexError): row['status']='failed'
        stores.check(before==common.source_info() and identities==binary_info(binaries),'auth inputs changed during capture')
        report['status']='PASS'
        report['errors']=verify(report,directory)
        report['status']='FAIL' if report['errors'] else 'PASS'
    except (OSError,ValueError,KeyError,TypeError,IndexError,AttributeError,OverflowError,subprocess.SubprocessError) as error:
        report['errors'].append(str(error) if isinstance(error,ValueError) else 'auth capture failed: '+type(error).__name__)
    return report


def capture(args):
    directory=None
    try:
        stores.check(args.cpp_build and args.rust_target and args.output,'auth capture needs --cpp-build --rust-target --output')
        target=Path(args.output).resolve()
        stores.check(not any(part=='.env' or part.startswith('.env.') for part in target.parts),'forbidden auth evidence path')
        target.mkdir(parents=True,exist_ok=False); directory=target
        report=execute(args,directory)
    except (OSError,ValueError,KeyError,TypeError) as error:
        report={'schema_version':1,'suite':'auth','mode':args.action,'status':'FAIL','errors':['auth capture setup failed: '+type(error).__name__],'cases':[],'commands':{}}
    if directory is not None: common.write_json(directory/'auth-report.json',report)
    print(json.dumps({'suite':'auth','mode':args.action,'status':report['status'],'cases':len(report['cases']),'errors':report['errors']},indent=2))
    return int(report['status']!='PASS')


def check(evidence,fault_evidence=None):
    errors=[]
    for mode,directory in [('compare',Path(evidence).resolve()),('fault',Path(fault_evidence).resolve() if fault_evidence else Path(str(evidence)+'-fault'))]:
        try:
            report=json_bytes(protocol.owned_bytes(directory/'auth-report.json',32*1024*1024))
            stores.check(report.get('mode')==mode,'wrong auth evidence mode')
            errors.extend(mode+': '+error for error in verify(report,directory))
        except (OSError,ValueError,KeyError,TypeError,AttributeError): errors.append(mode+': auth report missing/unsafe/invalid')
    print(json.dumps({'milestone':'M3','status':'FAIL' if errors else 'PASS','errors':errors},indent=2))
    return int(bool(errors))
