"""Regressions for M1 false-green reports and retained failure evidence."""
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import risk
import risk_transport


class RiskEvidenceTests(unittest.TestCase):
    def test_all_reject_cannot_pass_raw_replay(self):
        data=risk.manifest()
        requests=[{'op':'json',**{key:row[key] for key in ('payload','other') if key in row}}
                  for row in risk.json_cases(data)]
        with tempfile.TemporaryDirectory() as temporary:
            directory=Path(temporary); commands={}
            for name in ('cpp','rust'):
                label='json-'+name; commands[label]={}
                commands[label]['argv']=[name]
                for stream,value in [('stdin',b''.join((json.dumps(row)+'\n').encode() for row in requests)),
                                     ('stdout',b'{"accepted":false}\n'*len(requests))]:
                    path=directory/(label+'.'+stream); path.write_bytes(value)
                    commands[label][stream]=risk.raw(path)
            with self.assertRaisesRegex(ValueError,'null admission'):
                risk.replay({'commands':commands,'mode':'compare','binaries':{name:{'path':name} for name in ('cpp','rust')}},directory,data)
            (directory/'json-cpp.stdout').write_bytes(b'')
            with self.assertRaisesRegex(ValueError,'output inventory'):
                risk.replay({'commands':commands,'mode':'compare','binaries':{name:{'path':name} for name in ('cpp','rust')}},directory,data)

    def test_unexplained_wire_mismatch_fails(self):
        data=risk.manifest()
        self.assertFalse(risk.wire_pass('http-valid',{'cpp':{'accepted':True,'executors':0},
                                                     'rust':{'accepted':False,'executors':0}},data))
        self.assertFalse(risk.wire_pass('http-valid',{name:{'accepted':False,'executors':0} for name in ('cpp','rust')},data))
        self.assertFalse(risk.wire_pass('http-truncated',{name:{'accepted':True,'executors':0} for name in ('cpp','rust')},data))

    def test_proxy_mode_and_h2_prefix_are_required(self):
        record={'exit_code':0,'stderr':b'','stdout':b'{"accepted":true,"executors":0}',
                'observations':[[{'request':True}],[{'proxy':'connect'}],[]]}
        with self.assertRaises(AssertionError): risk_transport.validate_case('proxy-lower-http',record)
        record['stdout']=b'{"accepted":false,"executors":0,"uncommitted_body_bytes":0}'
        with self.assertRaises(AssertionError): risk_transport.validate_case('h2-reset',record)

    def test_timeout_retains_partial_bytes(self):
        error=subprocess.TimeoutExpired(['fake'],6,output=b'partial-out',stderr=b'partial-error')
        with patch.object(subprocess,'run',side_effect=error):
            result=risk_transport.execute(['fake'],{}, {},'.')
        self.assertTrue(result.timed_out)
        self.assertEqual((result.stdout,result.stderr),(b'partial-out',b'partial-error'))
        self.assertNotEqual(result.returncode,0)

    def test_wrong_fault_request_cannot_replace_required_case(self):
        request={'op':'http','url':'https://localhost:1234/','proxy_env':{}}
        risk_transport.validate_request('tls-invalid-ca',request,risk.FIXTURES)
        for key,value in [('body_limit',0),('h2',True),('ca_pem','other'),('proxy_env',{'https_proxy':'http://localhost:1'})]:
            with self.assertRaises(AssertionError): risk_transport.validate_request('tls-invalid-ca',{**request,key:value},risk.FIXTURES)

    def test_agreeing_adapters_cannot_drop_body_interim_or_trailers(self):
        for case in ('http-valid','http-interim','http-trailers','h2-trailer'):
            record={'exit_code':0,'stderr':b'','observations':[],
                    'stdout':b'{"accepted":true,"executors":0,"body_hex":"","interim":[],"trailers":[]}'}
            with self.assertRaises(AssertionError): risk_transport.validate_case(case,record)
        record['stdout']=json.dumps({'accepted':True,'executors':0,'body_hex':risk_transport.BODY.hex(),
                                    'interim':[],'trailers':[]}).encode()
        for case in ('http-interim','http-trailers','h2-trailer'):
            with self.assertRaises(AssertionError): risk_transport.validate_case(case,record)

    def test_existing_evidence_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory=Path(temporary); path=directory/'risk-report.json'; path.write_bytes(b'original')
            with patch('builtins.print'):
                status=risk.capture(SimpleNamespace(action='compare',cpp_build='x',rust_target='y',output=temporary))
            self.assertNotEqual(status,0)
            self.assertEqual(path.read_bytes(),b'original')

    def test_nonzero_adapter_output_saved_before_validation(self):
        row={'id':'null','payload':'null','accepted':True}
        output={'accepted':True,'dump':'null','sha256':risk.hashlib.sha256(b'null').hexdigest()}
        failed={'argv':['fake'],'exit_code':7,'stdout':b'partial-out','stderr':b'failure',
                'request':{'op':'http','url':'http://localhost:1/'},'observations':[]}
        with tempfile.TemporaryDirectory() as temporary, \
             patch.object(risk,'json_cases',return_value=[row]), \
             patch.object(risk,'batch',return_value=[output]), \
             patch.object(risk,'binary_info',return_value={}), \
             patch.object(risk_transport,'CASES',('http-valid',)), \
             patch.object(risk_transport,'run_case',return_value=failed):
            directory=Path(temporary)
            report=risk.execute(SimpleNamespace(action='compare',cpp_build='x',rust_target='y'),directory,
                                {'hash_domains':[],'reviewed_deviations':{}})
            self.assertEqual(report['status'],'FAIL')
            for name in ('cpp','rust'):
                label='wire-http-valid-'+name
                self.assertEqual(report['commands'][label]['exit_code'],7)
                self.assertEqual((directory/(label+'.stdout')).read_bytes(),b'partial-out')
                self.assertEqual((directory/(label+'.stderr')).read_bytes(),b'failure')


if __name__=='__main__': unittest.main()
