"""Meaningful negative controls for M8 evidence and synthetic isolation."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import quality
import copy
import e2e
import sessions
import session_cases as v

class RawControls(unittest.TestCase):
    def witness(self):
        path=Path(__file__).parent/'fixtures/e2e-checker-controls.json'
        case=json.loads(path.read_text())['pipeline-basic-1']
        return case['value'],case['request'],v.decode(sessions.parts(case['parts'])),Path(case['root'])
    def test_actual_witness_passes(self):e2e.validate(*self.witness())
    def test_missing_wire_events(self):
        value,request,events,root=self.witness();value['events']=value['events'][:1]
        with self.assertRaisesRegex(ValueError,'ToolStarted'):e2e.validate(value,request,events,root)
    def test_repeated_requests(self):
        value,request,events,root=self.witness();value['requests']=[value['requests'][0]]*len(value['requests'])
        with self.assertRaisesRegex(ValueError,'ordered production'):e2e.validate(value,request,events,root)
    def test_wrong_done_turn(self):
        value,request,events,root=self.witness();events[-1]['turn']='other'
        with self.assertRaisesRegex(ValueError,'transition turn'):e2e.validate(value,request,events,root)
    def test_forged_continuation_even_when_raw_requests_agree(self):
        value,request,events,root=self.witness()
        for actual in value['requests']+[event['response']['request'] for event in events if event['kind']=='response']:
            for item in actual['input']:
                if item.get('type')=='function_call_output' and item['call_id']=='call_fixture0_0':item['output']='FORGED'
        with self.assertRaisesRegex(ValueError,'continuation/results'):e2e.validate(value,request,events,root)
    def test_forged_snapshot_tool(self):
        value,request,events,root=self.witness();value['snapshot']['tools'][0]['output']='FORGED'
        with self.assertRaisesRegex(ValueError,'wire/canonical'):e2e.validate(value,request,events,root)
    def test_forged_partial_outcomes(self):
        _,_,events,root=self.witness();cut=events[:9]
        for event in cut:
            if event['kind']=='outcome':event['result']['output']='"FORGED_CRASH_OUTCOME"'
        with self.assertRaisesRegex(ValueError,'canonical tool result'):e2e.canonical_tools(cut,root)
    def test_boolean_counter_is_not_integer(self):
        value,request,events,root=self.witness();value['generation_count']=True
        with self.assertRaisesRegex(ValueError,'invalid integer'):e2e.validate(value,request,events,root)
    def test_returned_model_is_not_requested_model(self):
        value,request,events,root=self.witness();next(e for e in reversed(events) if e['kind']=='response')['response']['model']='unapproved-model'
        with self.assertRaisesRegex(ValueError,'returned model'):e2e.validate(value,request,events,root)
    def test_terminal_assistant_native_payload_is_bound(self):
        value,request,events,root=self.witness();next(e for e in reversed(events) if e['kind']=='response')['response']['items'][0]['content'][0]['text']='FORGED_TERMINAL_TEXT'
        with self.assertRaisesRegex(ValueError,'terminal native'):e2e.validate(value,request,events,root)
    def test_wire_text_delta_is_bound(self):
        value,request,events,root=self.witness();next(e for e in value['events'] if e['type']=='text_delta')['text']='FORGED_DELTA'
        with self.assertRaisesRegex(ValueError,'wire final text'):e2e.validate(value,request,events,root)

class QualityControls(unittest.TestCase):
    def test_encoded_flags_and_wrappers_cannot_disable_instrumentation(self):
        with patch.dict(os.environ,{'CARGO_ENCODED_RUSTFLAGS':'-Copt-level=0','RUSTFLAGS':'untrusted','RUSTC_WRAPPER':'untrusted','MIRIFLAGS':'-Zmiri-disable-validation','MIRI':'untrusted','CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER':'untrusted'},clear=False):
            env,flags=quality.environment('address',Path('/nix/store/fixture/bin/rustc'))
            self.assertEqual(env['CARGO_ENCODED_RUSTFLAGS'],'-Zsanitizer=address\x1f-Cdebuginfo=1\x1f-Ctarget-feature=-outline-atomics')
            self.assertEqual(flags,['-Zsanitizer=address','-Cdebuginfo=1','-Ctarget-feature=-outline-atomics'])
            self.assertEqual(env['RUSTC_WRAPPER'],'')
            self.assertNotIn('MIRIFLAGS',env)
            self.assertNotIn('CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER',env)
            env,_=quality.environment('miri',Path('/nix/store/fixture/bin/rustc'))
            self.assertEqual(env['MIRI'],'/nix/store/fixture/bin/miri')
            self.assertEqual(env['MIRIFLAGS'],'-Zmiri-strict-provenance')

    def test_missing_ignored_failed_and_filtered_tests_never_pass(self):
        def events(*rows):
            return '\n'.join(map(json.dumps, rows)).encode()

        listing = events(
            {'type': 'suite', 'event': 'discovery'},
            {'type': 'test', 'event': 'discovered', 'name': 'expected', 'ignore': False},
            {'type': 'suite', 'event': 'completed', 'tests': 1, 'benchmarks': 0, 'ignored': 0, 'total': 1},
        )
        reference = quality.test_inventory(listing, listing=True)
        valid = events(
            {'type': 'suite', 'event': 'started', 'test_count': 1},
            {'type': 'test', 'event': 'started', 'name': 'expected'},
            {'type': 'test', 'event': 'ok', 'name': 'expected'},
            {'type': 'suite', 'event': 'ok', 'passed': 1, 'failed': 0, 'ignored': 0, 'measured': 0, 'filtered_out': 0},
        )
        self.assertEqual(quality.executed_tests(valid, reference), 1)
        nested = b'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out;\n'
        self.assertEqual(quality.executed_tests(nested + valid, reference), 1)
        for mutation in [
            b'', valid.rsplit(b'\n', 1)[0], valid.replace(b'expected', b'replacement'),
            valid.replace(b'"passed": 1', b'"passed": 0'),
            valid.replace(b'"ignored": 0', b'"ignored": 1'),
            valid.replace(b'"failed": 0', b'"failed": 1'),
            valid.replace(b'"filtered_out": 0', b'"filtered_out": 1'),
            valid + b'\n' + valid,
        ]:
            with self.subTest(mutation=mutation):
                with self.assertRaises(ValueError):
                    quality.executed_tests(mutation, reference)
        with self.assertRaises(ValueError):
            quality.test_inventory(listing.replace(b'"ignore": false', b'"ignore": true'), listing=True)

class FixtureControls(unittest.TestCase):
    def test_scoped_aliases_and_stale_readiness_rejected(self):
        project=Path(__file__).resolve().parents[2]
        binaries=[project/'build-ansa/ansa_e2e_oracle',project/'rust/target/debug/examples/e2e_adapter']
        variants=[(name,kind) for name in ('value.txt','marker','primary','ready') for kind in ('symlink','dangling','hardlink','fifo')]+[('primary','regular'),('ready','regular'),('directory','symlink')]
        for binary in binaries:
            self.assertTrue(binary.is_file(),str(binary))
            for child,kind in variants:
                with self.subTest(binary=binary.name,child=child,kind=kind),tempfile.TemporaryDirectory(prefix='ansa-m4-',dir='/tmp') as name,tempfile.TemporaryDirectory(prefix='ansa-m8-outside-',dir='/tmp') as other:
                    root=Path(name);outside=Path(other);owner=root/'fixture-owner';owner.write_bytes(b'ansa-m4-synthetic-v1\n');owner.chmod(0o600)
                    sentinel=outside/'sentinel';sentinel.write_bytes(b'outside');sentinel.chmod(0o600)
                    home=root/'home';cwd=home/'workspace';cwd.mkdir(parents=True,mode=0o700);home.chmod(0o700)
                    scope=cwd/'metrics_review'
                    if child=='directory':scope.symlink_to(outside,target_is_directory=True)
                    else:
                        scope.mkdir(mode=0o700);target=scope/child
                        if kind in ('symlink','dangling'):target.symlink_to(sentinel if kind=='symlink' else outside/'absent')
                        elif kind=='hardlink':os.link(sentinel,target)
                        elif kind=='fifo':os.mkfifo(target,0o600)
                        else:target.write_bytes(b'stale');target.chmod(0o600)
                    request={'root':str(root),'scope':'metrics_review','mode':'basic','turns':0,'start':0}
                    result=subprocess.run([binary],input=json.dumps(request).encode()+b'\n',stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=3)
                    self.assertNotEqual(result.returncode,0)
                    self.assertEqual(sentinel.read_bytes(),b'outside')
                    self.assertEqual({p.name for p in outside.iterdir()},{'sentinel'})

    def test_aliases_rejected_before_fixture_writes(self):
        root=Path(__file__).resolve().parents[2]
        binaries=[root/'build-ansa/ansa_e2e_oracle',root/'rust/target/debug/examples/e2e_adapter']
        for binary in binaries:
            self.assertTrue(binary.is_file(),str(binary))
            for variant in ['home','workspace','value','dangling-value','marker','hard-value','hard-marker','fifo']:
                with self.subTest(binary=binary.name,variant=variant),tempfile.TemporaryDirectory(prefix='ansa-m4-',dir='/tmp') as name,tempfile.TemporaryDirectory(prefix='ansa-m8-outside-',dir='/tmp') as outside_name:
                    fixture=Path(name);outside=Path(outside_name);marker=fixture/'fixture-owner';marker.write_bytes(b'ansa-m4-synthetic-v1\n');marker.chmod(0o600)
                    sentinel=outside/'sentinel';sentinel.write_bytes(b'outside');sentinel.chmod(0o600)
                    home=fixture/'home';cwd=home/'workspace'
                    if variant=='home':home.symlink_to(outside,target_is_directory=True)
                    elif variant=='workspace':home.mkdir(mode=0o700);cwd.symlink_to(outside,target_is_directory=True)
                    else:
                        cwd.mkdir(parents=True,mode=0o700);home.chmod(0o700)
                        target=cwd/('marker' if 'marker' in variant else 'value.txt')
                        if variant=='fifo':os.mkfifo(target,0o600)
                        elif variant.startswith('hard'):os.link(sentinel,target)
                        else:target.symlink_to(outside/('absent' if variant=='dangling-value' else 'sentinel'))
                    request={'root':str(fixture),'mode':'basic','turns':0,'start':0}
                    result=subprocess.run([binary],input=json.dumps(request).encode()+b'\n',stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=3)
                    self.assertNotEqual(result.returncode,0)
                    self.assertEqual(sentinel.read_bytes(),b'outside')
                    self.assertEqual({p.name for p in outside.iterdir()},{'sentinel'})

if __name__=='__main__':unittest.main()
