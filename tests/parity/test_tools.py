"""M6 negative evidence checks and an isolated real-process ownership regression."""
import contextlib
import ctypes
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import tools
import protocol
import risk_process

def topology():return {'pids':{'runtime':10,'guardian':11,'primary':12,'descendant':13},'parents':{'runtime':5,'guardian':10,'primary':11,'descendant':12}}
def unknown():return protocol.strict_loads('{"accepted":true,"value":{"execution":1,"effects":2,"output":"{\\"error\\":\\"process_result_unknown\\"}"}}')
class FakeCrash:
    live=False
    root=Path('/tmp/ansa-m4-unit')
    def __init__(self,initial=None,survivor=False):
        self.initial=initial;self.survivor=survivor;self.calls=0
        self.row={'commands':['command0']};self.bundle=SimpleNamespace(commands={'command0':{'pid':10,'parent_pid':5}})
    def call(self,adapter,request,**kwargs):
        self.calls+=1
        if self.calls==1:return self.initial
        if request['op']=='recover':return {'accepted':True,'value':{'execution':protocol.NumberToken('0',True),'effects':protocol.NumberToken('1',True),'output':'{"cancelled":true,"cleanup_complete":true}'}}
        return unknown()
    def artifact(self,name,payload=None):
        if name=='topology':return json.dumps(topology()).encode()
        if name=='exits-before-harness-cleanup':return json.dumps({'primary':True,'guardian':True,'descendant':not self.survivor}).encode()
        if name=='launches':return b'x'
        raise AssertionError(name)
class ToolsEvidenceTests(unittest.TestCase):
    def test_valid_topology(self):
        t=topology();tools.validate_topology(t['pids'],t['parents'],10,5)
    def test_unrelated_parent_rejected(self):
        t=topology();t['parents']['descendant']=5
        with self.assertRaises(ValueError):tools.validate_topology(t['pids'],t['parents'],10,5)
    def test_runtime_must_match_popen(self):
        t=topology()
        with self.assertRaises(ValueError):tools.validate_topology(t['pids'],t['parents'],99,5)
    def test_pid_overflow_rejected(self):
        t=topology();t['pids']['descendant']=2**80
        with self.assertRaises(ValueError):tools.validate_topology(t['pids'],t['parents'],10,5)
    def test_bool_parent_metadata_rejected(self):
        t=topology()
        with self.assertRaises(ValueError):tools.validate_topology(t['pids'],t['parents'],10,True)
    def test_guardian_initial_rejection_cannot_pass_raw_replay(self):
        with self.assertRaises(ValueError):tools.crash(FakeCrash({'accepted':False}),'rust',True)
    def test_guardian_known_effect_cannot_pass_raw_replay(self):
        value=unknown();value['value']['effects']=protocol.NumberToken('1',True)
        with self.assertRaises(ValueError):tools.crash(FakeCrash(value),'rust',True)
    def test_harness_cleanup_cannot_hide_surviving_descendant(self):
        with self.assertRaisesRegex(ValueError,'surviving process'):tools.crash(FakeCrash(None,True),'rust',False)
    def test_missing_cwd_checks_actual_workspace_marker(self):
        with tempfile.TemporaryDirectory(prefix='ansa-m6-negative-') as directory:
            root=Path(directory);(root/'home/workspace').mkdir(parents=True);(root/'home/workspace/unexpected').write_text('effect')
            context=SimpleNamespace(root=root,live=True,call=lambda *a,**k:unknown(),artifact=lambda name,payload:payload)
            with self.assertRaises(ValueError):tools.missing_cwd(context,'rust')
    def test_existing_capture_preserved(self):
        with tempfile.TemporaryDirectory(prefix='ansa-m6-negative-') as directory:
            report=Path(directory)/'tools-report.json';report.write_bytes(b'original')
            args=SimpleNamespace(cpp_build='build',rust_target='target',output=directory,action='compare')
            with patch.object(tools,'manifest',return_value={}),contextlib.redirect_stdout(io.StringIO()):self.assertEqual(tools.capture(args),1)
            self.assertEqual(report.read_bytes(),b'original')
    def test_case_inventory_closed(self):
        with self.assertRaises(ValueError):tools.execute(SimpleNamespace(),'fault',[],False)
    def test_wrong_numeric_lexeme_rejected(self):
        with self.assertRaises(ValueError):tools.same(protocol.strict_loads('1.0'),1)
    def test_unrelated_control_child_survives_failed_topology_validation(self):
        binary=(tools.common.ROOT/os.environ.get('ANSA_PARITY_CPP_BUILD','build-ansa')/'ansa_tools_oracle').resolve()
        libc=ctypes.CDLL(None,use_errno=True);libc.prctl.argtypes=[ctypes.c_int]+[ctypes.c_ulong]*4;libc.prctl.restype=ctypes.c_int
        self.assertEqual(libc.prctl(36,1,0,0,0),0)
        with tempfile.TemporaryDirectory(prefix='ansa-m4-m6-ownership-',dir='/tmp') as temporary:
            root=Path(temporary);marker=root/'fixture-owner';marker.write_text('ansa-m4-synthetic-v1\n');marker.chmod(0o600)
            control=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)',str(root)],start_new_session=True)
            runtime=subprocess.Popen([str(binary)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
            try:
                cwd=root/'home/workspace';request={'op':'process','root':str(root),'argv':[sys.executable,str(tools.common.ROOT/'tests/parity/process_fixture.py'),'tree',str(cwd)],'timeout_ms':30000}
                runtime.stdin.write((json.dumps(request)+'\n').encode());runtime.stdin.flush()
                deadline=time.monotonic()+5
                while True:
                    try:pids={name:int((cwd/name).read_text()) for name in ('guardian','primary','descendant')}
                    except (ValueError,OSError):pids={}
                    if len(pids)==3 and all(pid>1 for pid in pids.values()):break
                    self.assertLess(time.monotonic(),deadline);time.sleep(.005)
                pids.update(runtime=runtime.pid,descendant=control.pid)
                with self.assertRaises(ValueError):tools.pin_topology(pids,runtime.pid,root,binary)
                self.assertIsNone(control.poll(),'unrelated control child was killed by failed validation')
            finally:
                if control.poll() is None:control.terminate()
                control.wait(timeout=5)
                if runtime.poll() is None:runtime.kill()
                runtime.communicate(timeout=5)
                risk_process.cleanup()
if __name__=='__main__':unittest.main()
