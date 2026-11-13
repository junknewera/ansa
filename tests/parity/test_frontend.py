"""M7 false-PASS counterexamples and evidence-preservation regressions."""
import contextlib
import copy
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import frontend
import sessions
import frontend_io

class Fake:
    root=Path('/tmp/ansa-m4-counterexample')
    def __init__(self,report):self.report=report
    def driver(self,*_):return copy.deepcopy(self.report)
def pty():return {'exit':0,'termios_restored':True,'flags_restored':True,'stdout_hex':b'frame'.hex(),'observations':[{'text_hex':b'frame'.hex(),'since':0,'through':5}],'journals':{},'marks':{'stopped':True}}
def control(name):return json.loads((Path(__file__).parent/'fixtures/frontend-checker-controls.json').read_bytes())[name]
def replace_events(value):value['stdout_hex']=''.join(json.dumps(e,separators=(',',':'))+'\n' for e in value['events']).encode().hex()
def bridge_fake(name):
    value=control(name);fake=Fake(value);fake.root=Path(json.loads(bytes.fromhex(value['request_hex']))['root']);return fake
class FrontendEvidenceTests(unittest.TestCase):
    def test_inventory_is_closed_and_unique(self):
        for mode in ('compare','fault'):
            ids=frontend.inventory(mode);self.assertEqual(len(ids),len(set(ids)));self.assertGreater(len(ids),100)
    def test_manifest_cannot_drop_a_required_case(self):
        value=frontend.manifest();value['fault_ids'].pop()
        with patch.object(frontend.auth,'json_bytes',return_value=value),self.assertRaises(ValueError):frontend.manifest()
    def test_empty_raw_suite_cannot_pass(self):
        with self.assertRaises(ValueError):frontend.execute(sessions.Bundle(Path('/tmp'),{},{}),'compare',[],False)
    def test_failed_termios_restore_is_not_success(self):
        value=pty();value['termios_restored']=False
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'job-control')
    def test_alias_flags_restore_is_required(self):
        value=pty();value['flags_restored']=False
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'job-control')
    def test_bool_exit_is_not_numeric_success(self):
        value=pty();value['exit']=False
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'job-control')
    def test_out_of_range_observation_is_not_evidence(self):
        value=pty();value['observations'][0]['through']=True
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'job-control')
    def test_job_control_must_actually_stop(self):
        value=pty();value['marks']['stopped']=False
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'job-control')
    def test_timing_rejects_nonfinite_and_bool(self):
        for value in (float('nan'),float('inf'),True,-1):
            with self.assertRaises(ValueError):frontend.duration(value,5)
    def test_hex_cannot_hide_invalid_bytes(self):
        for value in ('a','xx','AB','00x'):
            with self.assertRaises(ValueError):frontend.raw_hex(value)
    def test_cursor_requires_real_unsigned_u64(self):
        for value in (True,-1,2**64,1.0):
            with self.assertRaises(ValueError):frontend.integer(value)
    def test_journal_paths_cannot_escape_root(self):
        with self.assertRaises(ValueError):frontend.journal_events({'../part-00000000000000000000.log':''})
    def test_all_reject_oracle_cannot_pass_valid_control(self):
        class Reject:
            def call(self,*_):return {'accepted':False}
        with self.assertRaises(ValueError):frontend.pair(Reject(),{'op':'events'})
    def test_existing_capture_remains_byte_identical(self):
        with tempfile.TemporaryDirectory(prefix='ansa-m7-checker-') as directory:
            path=Path(directory);report=path/'frontend-report.json';report.write_bytes(b'original\n')
            args=SimpleNamespace(cpp_build='build',rust_target='target',output=str(path),action='compare')
            with contextlib.redirect_stdout(io.StringIO()):self.assertEqual(frontend.capture(args),1)
            self.assertEqual(report.read_bytes(),b'original\n');self.assertEqual(list(path.iterdir()),[report])
    def test_real_roundtrip_and_cancel_controls_pass_semantic_replay(self):
        frontend.jsonl_case(Fake(control('complete')),'cppcli','complete')
        frontend.jsonl_case(Fake(control('cancel')),'cppcli','cancel')
    def test_missing_cancel_admission_cannot_pass(self):
        value=control('cancel');value['events']=value['events'][:1];replace_events(value)
        with self.assertRaises(ValueError):frontend.jsonl_case(Fake(value),'cppcli','cancel')
    def test_missing_signal_admission_cannot_pass(self):
        value=control('signal');value['events']=value['events'][:1];replace_events(value)
        with self.assertRaises(ValueError):frontend.jsonl_case(Fake(value),'cppcli','signal')
    def test_cancel_command_identity_is_bound(self):
        value=control('cancel')
        for e in value['events']:
            if e['type']=='cancelled':e['command_id']='wrong'
        replace_events(value)
        with self.assertRaises(ValueError):frontend.jsonl_case(Fake(value),'cppcli','cancel')
    def test_roundtrip_incarnation_cannot_change(self):
        value=control('complete')
        for e in value['events'][1:]:e['incarnation']='b'*32
        replace_events(value)
        with self.assertRaises(ValueError):frontend.jsonl_case(Fake(value),'cppcli','complete')
    def test_known_tool_output_is_checked(self):
        value=control('complete')
        for e in value['events']:
            if e['type']=='tool_finished':e['output']='WRONG_TOOL_OUTPUT'
        replace_events(value)
        with self.assertRaises(ValueError):frontend.jsonl_case(Fake(value),'cppcli','complete')
    def test_operation_identity_is_checked(self):
        value=control('complete')
        for e in value['events']:
            if e['type']=='tool_started':e['operation_id']='wrong'
        replace_events(value)
        with self.assertRaises(ValueError):frontend.jsonl_case(Fake(value),'cppcli','complete')
    def test_history_mark_is_a_real_raw_prefix(self):
        value=control('terminal');value['marks']['first']=b'> EARLIER_CONTEXT forged\n> '.hex()
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'terminal')
    def test_rejected_draft_mark_is_a_real_raw_prefix(self):
        value=control('reject');value['marks']['rejected']=b'secondAFTER'.hex()
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'reject')
    def test_empty_observation_cannot_pass(self):
        value=control('terminal');value['observations'][0]['text_hex']=''
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'terminal')
    def test_missing_durable_preview_trace_cannot_pass(self):
        value=control('preview');value['journals']={}
        with self.assertRaises(ValueError):frontend.pty_case(Fake(value),'preview-invalid')
    def test_missing_durable_pending_trace_cannot_pass(self):
        value=control('pending');value['journals']={}
        with self.assertRaises(ValueError):frontend.fault_case(Fake(value),'pending')
    def test_real_bridge_control_passes_semantic_replay(self):frontend.bridge_case(bridge_fake('bridge'),'valid-control',False)
    def test_bridge_request_body_is_bound(self):
        fake=bridge_fake('bridge');fake.report['observations'][0]['request_body_hex']=b'{}'.hex()
        with self.assertRaises(ValueError):frontend.bridge_case(fake,'valid-control',False)
    def test_bridge_request_target_is_bound(self):
        fake=bridge_fake('bridge');fake.report['observations'][0]['target']='/wrong'
        with self.assertRaises(ValueError):frontend.bridge_case(fake,'valid-control',False)
    def test_bridge_auth_header_is_checked_without_logging_bearer(self):
        fake=bridge_fake('bridge');fake.report['observations'][0]['authorization_matches']=False
        with self.assertRaises(ValueError):frontend.bridge_case(fake,'valid-control',False)
    def test_chunk_padding_is_not_terminal_evidence(self):
        fake=bridge_fake('chunk');result=json.loads(bytes.fromhex(fake.report['stdout_hex']));size=len(bytes.fromhex(fake.report['body_hex']));result['value']['received']=size;result['value']['received_hex']=bytes.fromhex(result['value']['received_hex'])[:size].hex();fake.report['stdout_hex']=(json.dumps(result)+'\n').encode().hex()
        with self.assertRaises(ValueError):frontend.bridge_case(fake,'chunk-budget',False)
    def test_fixture_owner_is_private_regular_single_link_and_bounded(self):
        import os
        with tempfile.TemporaryDirectory(prefix='ansa-m4-',dir='/tmp') as name:
            root=Path(name);owner=root/'fixture-owner';owner.write_bytes(b'ansa-m4-synthetic-v1\n');owner.chmod(0o600);frontend_io.fixture(root)
            owner.write_bytes(b'x'*100000)
            with self.assertRaises(AssertionError):frontend_io.fixture(root)
            owner.unlink();os.mkfifo(owner,0o600)
            with self.assertRaises(AssertionError):frontend_io.fixture(root)
            owner.unlink();target=root/'public-marker';target.write_bytes(b'ansa-m4-synthetic-v1\n');target.chmod(0o600);owner.symlink_to(target)
            with self.assertRaises(OSError):frontend_io.fixture(root)
