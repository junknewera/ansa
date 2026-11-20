"""M3 evidence regressions: agreeing adapters and PASS metadata are insufficient."""
import copy
import hashlib
import json
import math
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import auth
import auth_cases
import auth_store_cases as stores
import protocol
import run


class AuthEvidenceTests(unittest.TestCase):
    def golden(self):
        return json.loads((run.ROOT/'tests/fixtures/auth/golden.json').read_bytes())

    def test_inventory_covers_native_crash_and_boundary_cases(self):
        data=auth.manifest()
        self.assertEqual(len(data['native_suites']),16)
        self.assertEqual(data['compare_ids'],auth.case_ids('compare'))
        self.assertEqual(data['fault_ids'],auth.case_ids('fault'))
        self.assertTrue(set(data['compare_ids']).issubset(data['fault_ids']))
        self.assertIn('cross-process-cpp-rust-refresh',data['fault_ids'])
        self.assertIn('adapter-unterminated',data['fault_ids'])
        self.assertEqual(set(data['reviewed_deviations']),set(auth_cases.DEVIATIONS))

    def test_golden_signature_and_signed_bytes_are_independently_validated(self):
        spec=auth_cases.jwt_specs()[0]
        auth_cases.validate_generated(spec,self.golden())
        for mutation in ('signature','public_key','claims'):
            fixture=self.golden()
            if mutation=='public_key': fixture['jwks']['keys'][0]['n']=auth_cases.b64(b'\x80'+b'\x01'*255)
            else:
                parts=fixture['jwt'].split('.')
                if mutation=='signature':
                    raw=bytearray(auth_cases.unb64(parts[2]));raw[0]^=1;parts[2]=auth_cases.b64(raw)
                else: parts[1]=auth_cases.b64(b'{}')
                fixture['jwt']='.'.join(parts)
            with self.assertRaises(ValueError): auth_cases.validate_generated(spec,fixture)

    def test_positive_values_and_rejection_diagnostics_are_checked(self):
        case=auth_cases.materialize(auth_cases.jwt_specs()[0],self.golden())
        output={key:value for key,value in case['expected'].items() if key!='accepted'}
        output['accepted']=True
        self.assertTrue(auth_cases.output_pass(case,output,'cpp'))
        self.assertFalse(auth_cases.output_pass(case,{**output,'subject':'other'},'cpp'))
        self.assertFalse(auth_cases.output_pass(case,{'accepted':False,'diagnostic':'auth_jwt_signature'},'rust'))
        negative={'request':{'op':'jwt'},'expected':{'accepted':{'cpp':False,'rust':False}}}
        self.assertTrue(auth_cases.output_pass(negative,{'accepted':False,'diagnostic':'auth_jwt_signature'},'rust'))
        self.assertFalse(auth_cases.output_pass(negative,{'accepted':False,'diagnostic':'fixture_access_123456'},'rust'))
        self.assertFalse(auth_cases.output_pass(negative,{'accepted':False,'diagnostic':'auth_jwt_signature','subject':'leaked'},'cpp'))

    def test_command_boolean_exit_or_nonfinite_time_cannot_pass(self):
        good={'exit_code':0,'timed_out':False,'elapsed_seconds':0.1}
        self.assertTrue(auth.command_ok(good))
        for change in ({'exit_code':False},{'timed_out':0},{'elapsed_seconds':float('nan')},
                       {'elapsed_seconds':float('inf')},{'elapsed_seconds':False},{'elapsed_seconds':-1}):
            self.assertFalse(auth.command_ok({**good,**change}))

    def bundle(self,root):
        spec=auth_cases.jwt_specs()[0];fixture=self.golden();case=auth_cases.materialize(spec,fixture)
        paths={}
        for name,basename in (('cpp','ansa_auth_oracle'),('rust','auth_adapter')):
            path=root/basename;path.write_bytes(b'#!/bin/sh\nexit 0\n');path.chmod(0o700);paths[name]=path
        commands={}
        successful={key:value for key,value in case['expected'].items() if key!='accepted'};successful['accepted']=True
        for label,adapter,request,output in [('auth-generate','cpp',spec['generate'],fixture),
                ('auth-compare-cpp','cpp',case['request'],successful),('auth-compare-rust','rust',case['request'],successful)]:
            value={'argv':[str(paths[adapter])],'cwd':str(run.ROOT),'exit_code':0,'timed_out':False,
                   'elapsed_seconds':0.1,'fixture_root':str(run.ROOT/'tests/fixtures/auth')}
            for channel,data in [('stdin',(auth_cases.compact(request)+'\n').encode()),
                                 ('stdout',(auth_cases.compact(output)+'\n').encode()),('stderr',b'')]:
                path=root/(label+'.'+channel);path.write_bytes(data);value[channel]=protocol.raw(path)
            commands[label]=value
        report={'schema_version':1,'suite':'auth','mode':'compare','status':'PASS','errors':[],
                'source':{'files':{}},'platform':{'system':'Linux','machine':'aarch64','os_release':{'ID':'nixos'}},
                'manifest_sha256':run.sha(auth.MANIFEST),'fixtures':{},'binaries':auth.binary_info(paths),
                'commands':commands,'cases':[{'id':case['id'],'status':'pass'}]}
        return report,spec

    def isolated_verify(self,root,report,spec):
        with patch.object(auth,'manifest',return_value={'fixtures':{}}), \
             patch.object(auth_cases,'jwt_specs',return_value=[spec]), \
             patch.object(auth_cases,'simple_cases',return_value=[]), \
             patch.object(stores,'case_ids',return_value=[]), \
             patch.object(run,'source_info',return_value={'files':{}}):
            return auth.verify(report,root)

    def test_minimal_valid_bundle_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);report,spec=self.bundle(root)
            self.assertEqual(self.isolated_verify(root,report,spec),[])

    def test_tampering_cannot_be_hidden_by_pass_metadata(self):
        mutations=['all-reject','agreeing-wrong','missing-case','missing-binary','foreign-platform','extra-argv',
                   'binary-digest','raw-digest','fixture-input','source','timeout','unreferenced-command','boolean-schema']
        for mutation in mutations:
            with self.subTest(mutation=mutation),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);report,spec=self.bundle(root)
                if mutation in ('all-reject','agreeing-wrong'):
                    for adapter in ('cpp','rust'):
                        label='auth-compare-'+adapter;path=root/(label+'.stdout');value=json.loads(path.read_bytes())
                        if mutation=='all-reject':value={'accepted':False,'diagnostic':'auth_jwt_signature'}
                        else:value['subject']='other'
                        path.write_text(auth_cases.compact(value)+'\n');report['commands'][label]['stdout']=protocol.raw(path)
                elif mutation=='missing-case':report['cases']=[]
                elif mutation=='missing-binary':report['binaries'].pop('rust')
                elif mutation=='foreign-platform':report['platform']['system']='Darwin'
                elif mutation=='extra-argv':report['commands']['auth-compare-rust']['argv'].append('--extra')
                elif mutation=='binary-digest':report['binaries']['rust']['sha256']='0'*64
                elif mutation=='raw-digest':report['commands']['auth-compare-cpp']['stdout']['sha256']='0'*64
                elif mutation=='fixture-input':report['commands']['auth-compare-rust']['fixture_root']='/tmp/wrong-fixtures'
                elif mutation=='source':report['source']['files']={'wrong.cpp':'0'*64}
                elif mutation=='timeout':report['commands']['auth-compare-rust']['timed_out']=True
                elif mutation=='unreferenced-command':report['commands']['extra']=copy.deepcopy(report['commands']['auth-compare-rust'])
                else:report['schema_version']=True
                self.assertTrue(self.isolated_verify(root,report,spec))

    def test_existing_capture_directory_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);marker=root/'auth-report.json';marker.write_bytes(b'keep')
            args=SimpleNamespace(action='compare',cpp_build='missing',rust_target='missing',output=root)
            with patch('builtins.print'):self.assertNotEqual(auth.capture(args),0)
            self.assertEqual(marker.read_bytes(),b'keep')

    def test_cli_has_auth_and_m3_dispatch(self):
        import subprocess
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for args in (['compare','--suite','auth','--cpp-build',str(root/'none'),'--rust-target',str(root/'none'),'--output',str(root/'capture')],
                         ['check','--milestone','M3','--evidence',str(root/'none')]):
                result=subprocess.run([sys.executable,str(run.ROOT/'tests/parity/run.py'),*args],capture_output=True,text=True)
                self.assertNotEqual(result.returncode,0)
                self.assertNotIn('invalid choice',result.stderr)


import sys
if __name__=='__main__':unittest.main()
