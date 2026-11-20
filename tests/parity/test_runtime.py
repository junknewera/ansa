"""Negative tests for M5 independent evidence assertions and capture guards."""
import contextlib
import copy
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import protocol
import runtime

def witness():
    spec=runtime.request(0,1)
    requests,_=runtime.pipeline(spec)
    identity={'session':'offline','turn':'turn0','incarnation':'a'*32,'request':'request_1'}
    value={'incarnation':'a'*32,'invocations':0,'requests':requests,'context':None,
           'transcript':[{'turn':'turn0','request':'request_1','role':role,'text':text,'state':'completed','mode':'basic'} for role,text in [('user','fixture'),('assistant','ANSWER')]],
           'events':[dict(identity,sequence=i+1,type=kind,**fields) for i,(kind,fields) in enumerate([('accepted',{'command':'turn0'}),('text',{'text':'ANSWER'}),('done',{})])],
           'snapshot':{'state':'completed','busy':False,'text':'ANSWER','diagnostic':'','completed':0,'history':2,'transcript':2,'compactions':0,'durable':True,'storage_failed':False,'mode':'basic','through':4,'request':'request_1','submit':'turn0','recovery':None,'preview':None,'resume_available':False}}
    return spec,protocol.strict_loads(json.dumps({'accepted':True,'value':value}))
class RuntimeEvidenceTests(unittest.TestCase):
    def mutate_rejected(self,mutation):
        spec,value=witness();mutation(value['value'])
        with self.assertRaises(ValueError):runtime.trace_value(value,spec)
    def test_witness(self):
        spec,value=witness();runtime.trace_value(value,spec)
    def test_all_rejected_is_not_parity(self):
        spec,_=witness()
        with self.assertRaises(ValueError):runtime.trace_value({'accepted':False},spec)
    def test_lost_completion(self):self.mutate_rejected(lambda v:v['events'].pop())
    def test_unexpected_control(self):self.mutate_rejected(lambda v:v['events'].append(dict(v['events'][-1],type='cancelled')))
    def test_stale_incarnation(self):self.mutate_rejected(lambda v:v['events'][0].update(incarnation='b'*32))
    def test_duplicate_cursor(self):self.mutate_rejected(lambda v:v['events'][1].update(sequence=v['events'][0]['sequence']))
    def test_future_terminal_cursor(self):self.mutate_rejected(lambda v:v['snapshot'].update(through=protocol.NumberToken('1',True)))
    def test_noncanonical_numeric_metadata(self):self.mutate_rejected(lambda v:v.update(invocations=True))
    def test_cursor_overflow(self):
        def overflow(value):
            for event in value['events']:event['sequence']=protocol.NumberToken(str(int(event['sequence'].lexeme)+2**80),True)
            value['snapshot']['through']=protocol.NumberToken(str(2**80+4),True)
        self.mutate_rejected(overflow)
    def test_noncanonical_cursor_lexeme(self):self.mutate_rejected(lambda v:v['events'][0].update(sequence=protocol.NumberToken('01',True)))
    def test_equal_wrong_provider_request(self):self.mutate_rejected(lambda v:v['requests'][0]['binding'].update(account='implicit-rebind'))
    def test_equal_wrong_history(self):self.mutate_rejected(lambda v:v['requests'][0]['input'].reverse() or v['requests'][0]['input'].append({'role':'user','content':'extra'}))
    def test_hidden_recovery(self):self.mutate_rejected(lambda v:v['snapshot'].update(recovery='unknown'))
    def test_existing_capture_preserved(self):
        with tempfile.TemporaryDirectory(prefix='ansa-m5-checker-') as temporary:
            path=Path(temporary)/'runtime-report.json';path.write_bytes(b'original')
            args=SimpleNamespace(cpp_build='build',rust_target='target',output=temporary,action='compare')
            with patch.object(runtime,'manifest',return_value={}),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runtime.capture(args),1)
            self.assertEqual(path.read_bytes(),b'original')
    def test_duplicate_json_key_rejected(self):
        with self.assertRaises(ValueError):protocol.strict_loads('{"accepted":true,"accepted":true}')
    def test_case_inventory_is_closed(self):
        with self.assertRaises(ValueError):runtime.execute(SimpleNamespace(), 'compare', [], False)
    def test_unrelated_durable_journal_rejected(self):
        spec=runtime.request(0,1,durable=True)
        from session_cases import GOLDEN
        parts={'part-00000000000000000000.log':(GOLDEN.parent/'completed.log').read_bytes()}
        with self.assertRaises(ValueError):runtime.journal_matches(parts,spec,{'incarnation':'a'*32})
if __name__=='__main__':unittest.main()
