"""Actual native observations and mutations that must never pass the perf gate."""
import copy
import hashlib
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch
import os
import performance
import performance_metrics as measure
import performance_evidence as verify
import session_cases as journal
import tty_flood

class PerformanceControls(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary=tempfile.TemporaryDirectory(prefix='ansa-m8-perf-control-')
        cls.directory=Path(cls.temporary.name)
        root=Path(__file__).resolve().parents[2]
        cls.adapters={'cpp':str(root/'build-ansa/ansa_e2e_oracle'),'rust':str(root/'rust/target/debug/examples/e2e_adapter')}
        cls.binaries={'cpp':str(root/'build-ansa-release/ansa'),'rust':str(root/'rust/target/release/ansa-rs')}
        cls.pipeline=measure.pipeline({k:Path(p) for k,p in cls.adapters.items()},cls.directory,counts=(0,),samples=1)
        cls.sse=measure.sse({k:Path(p) for k,p in cls.adapters.items()},root/'build-ansa/libansa_allocation_counter.so',cls.directory,samples=1,iterations=1)
        cls.startup=performance.startup({k:Path(p) for k,p in cls.binaries.items()},cls.directory,Path(cls.adapters['cpp']),counts=(0,),samples=1)
        cls.frontend=Path(cls.adapters['rust']).with_name('frontend_adapter');cls.stress=tty_flood.capture(cls.frontend,cls.directory)
    @classmethod
    def tearDownClass(cls):cls.temporary.cleanup()
    def setUp(self):
        self.working=tempfile.TemporaryDirectory(prefix='ansa-m8-perf-mutation-');self.addCleanup(self.working.cleanup)
        self.path=Path(self.working.name);shutil.copytree(self.directory,self.path,dirs_exist_ok=True)
        self.report=copy.deepcopy(self.pipeline)
    def replay(self):return verify.pipeline(self.report,self.path,self.adapters,counts=(0,),samples=1)
    def mutate_raw(self,item,change):
        path=self.path/item['path'];value=json.loads(path.read_bytes());change(value);path.write_text(json.dumps(value));item['sha256']=hashlib.sha256(path.read_bytes()).hexdigest()
    def test_actual_native_observations_replay(self):
        self.replay();verify.startup(self.startup,self.path,self.binaries,self.adapters['cpp'],counts=(0,),samples=1);verify.sse(self.sse,self.path,self.adapters,samples=1,iterations=1);tty_flood.verify(self.stress,self.path,self.frontend)
    def test_flood_must_continue_until_cancel_ack(self):
        report=copy.deepcopy(self.stress);row=report['runs'][0];row['written_at_ack']=row['written_before_cancel']
        with self.assertRaisesRegex(ValueError,'continuous input'):tty_flood.verify(report,self.path,self.frontend)
    def test_flood_no_dispatch_after_cancel(self):
        report=copy.deepcopy(self.stress);row=report['runs'][0];part=row['snapshot'][0];events=journal.decode({bytes.fromhex(part['name_hex']).decode():bytes.fromhex(part['bytes_hex'])});events.append({'kind':'done','turn':events[-1]['turn']});part['bytes_hex']=journal.encode(events).hex()
        with self.assertRaisesRegex(ValueError,'dispatch after Cancel'):tty_flood.verify(report,self.path,self.frontend)
    def test_closed_inventory_missing_duplicate_and_reordered(self):
        for rows in (self.report['runs'][:-1],self.report['runs']+[self.report['runs'][0]],list(reversed(self.report['runs']))):
            with self.subTest(rows=len(rows)):
                original=self.report['runs'];self.report['runs']=rows
                with self.assertRaisesRegex(ValueError,'inventory'):self.replay()
                self.report['runs']=original
    def test_summary_cannot_hide_measured_latency(self):
        self.report['summary']['rust-0-turn']['submit_preview_seconds']['p95_nearest_rank']=0
        with self.assertRaisesRegex(ValueError,'distributions'):self.replay()
    def test_report_latency_bound_to_retained_clocks(self):
        self.report['runs'][0]['submit_preview_seconds']=0
        with self.assertRaisesRegex(ValueError,'clocks'):self.replay()
    def test_missing_read_trace_cannot_pass(self):
        self.mutate_raw(self.report['runs'][0]['observation'],lambda value:value['trace'].update(reads=[]))
        with self.assertRaisesRegex(ValueError,'read trace'):self.replay()
    def test_native_duration_cannot_replace_raw_footer(self):
        self.report['runs'][0]['native']['metrics']['terminal_durable_ns']=[1]
        with self.assertRaisesRegex(ValueError,'raw footer'):self.replay()
    def test_cleanup_required_before_harness_intervention(self):
        row=next(r for r in self.report['runs'] if r['kind']=='cancel')
        self.mutate_raw(row['observation'],lambda value:value['process'].update(after_exited=[False]*len(value['process']['ancestry'])))
        with self.assertRaisesRegex(ValueError,'before harness'):self.replay()
    def test_cleanup_after_exit_rejected(self):
        row=next(r for r in self.report['runs'] if r['kind']=='cancel')
        self.mutate_raw(row['observation'],lambda value:value['process'].update(cleanup_ns=value['trace']['exit_ns']+1))
        with self.assertRaisesRegex(ValueError,'cleanup clocks'):self.replay()
    def test_unrelated_pid_chain_rejected(self):
        row=next(r for r in self.report['runs'] if r['kind']=='cancel')
        self.mutate_raw(row['observation'],lambda value:value['process']['ancestry'][-1].update(ppid=value['process']['parent_pid']))
        with self.assertRaisesRegex(ValueError,'cleanup ancestry'):self.replay()
    def test_leaf_pid_bound_to_actual_file_and_pidfd(self):
        row=next(r for r in self.report['runs'] if r['kind']=='cancel')
        self.mutate_raw(row['observation'],lambda value:value['process']['ancestry'][0].update(pid=987654321))
        with self.assertRaisesRegex(ValueError,'actual primary'):self.replay()
    def test_submit_clock_cannot_jump_over_intervening_reads(self):
        row=self.report['runs'][0]
        def forge(value):
            trace=value['trace'];write=next(s for s in trace['sends'] if bytes.fromhex(s['bytes_hex']).startswith(b'fixture'))
            data=(self.path/row['raw']['path']).read_bytes();write['at_ns']=verify.crossing(trace,data,'E2E_PREVIEW',write['offset'])-1
        self.mutate_raw(row['observation'],forge)
        with self.assertRaisesRegex(ValueError,'read timeline'):self.replay()
    def test_allocation_library_identity_is_checked(self):
        report=copy.deepcopy(self.sse);report['counter']={'path':'/not/the/used/interposer','sha256':'0'*64}
        with self.assertRaisesRegex(ValueError,'interposer'):verify.sse(report,self.path,self.adapters,samples=1,iterations=1)
    def test_resealed_forged_known_result_rejected(self):
        row=self.report['runs'][0];base=self.report['datasets'][0]['parts'];name=next(iter(base));prefix=(self.path/base[name]['path']).read_bytes();delta=row['journal_deltas'][name]
        events=journal.decode({name:prefix+(self.path/delta['path']).read_bytes()})
        next(e for e in events if e['kind']=='outcome')['result']['output']='"FORGED"'
        encoded=journal.encode(events);self.assertTrue(encoded.startswith(prefix));path=self.path/delta['path'];path.write_bytes(encoded[len(prefix):])
        delta.update(sha256=hashlib.sha256(path.read_bytes()).hexdigest(),after_bytes=len(encoded),after_sha256=hashlib.sha256(encoded).hexdigest())
        with self.assertRaisesRegex(ValueError,'continuation|known result'):self.replay()
    def test_missing_sse_admissions_rejected(self):
        report=copy.deepcopy(self.sse);command=report['runs'][0]['command']
        self.mutate_raw(command['stdout'],lambda value:value['value'].update(admissions=0))
        with self.assertRaisesRegex(ValueError,'actual admissions'):verify.sse(report,self.path,self.adapters,samples=1,iterations=1)
    def test_boolean_allocation_count_rejected(self):
        report=copy.deepcopy(self.sse);item=report['allocations']['rust'];self.mutate_raw(item['raw'],lambda value:value.update(allocations=True));item['counts']['allocations']=True
        with self.assertRaisesRegex(ValueError,'allocation work'):verify.sse(report,self.path,self.adapters,samples=1,iterations=1)
    def test_short_latency_floor_and_ratio_are_exact(self):
        def report(cpp,rust):return {'startup':{'summary':{'cpp-0':{'input_ack_seconds':{'p95_nearest_rank':cpp}},'rust-0':{'input_ack_seconds':{'p95_nearest_rank':rust}}}},'pipeline':{'summary':{}},'sse':{'summary':{'cpp':{'p95_nearest_rank':2.0},'rust':{'p95_nearest_rank':3.0}}}}
        policy={'new_latency':{'max_ratio':1.5,'additive_floor_seconds':.005},'new_time_per_work':{'max_ratio':1.5}}
        self.assertEqual(verify.new_errors(report(.001,.006),policy),[])
        self.assertEqual(len(verify.new_errors(report(.001,.006001),policy)),1)
        self.assertEqual(verify.new_errors(report(1.0,1.5),policy),[])
        self.assertEqual(len(verify.new_errors(report(1.0,1.500001),policy)),1)
    def test_measurement_metadata_is_saved_before_progress_append(self):
        (self.path/'logs').mkdir()
        with patch.object(performance.m,'OUT',self.path),patch.object(performance.m,'RUNS',[]):
            row=performance.m.run('metadata-proof',[self.binaries['cpp'],'--self-check'],self.path,metadata={'cache_condition':'observed','changed_source_sha256':'a'*64})
            self.assertEqual(json.loads((self.path/'progress.jsonl').read_bytes()),row)
    def test_prefix_sync_failure_stops_before_frontend_launch(self):
        out=self.path/'unsynced-prefix';out.mkdir()
        with patch.object(measure.os,'fsync',side_effect=OSError('synthetic fsync failure')),patch.object(measure,'ObservedPty') as frontend:
            with self.assertRaisesRegex(OSError,'synthetic fsync failure'):measure.pipeline({k:Path(p) for k,p in self.adapters.items()},out,counts=(0,),samples=1)
            frontend.assert_not_called()
    def test_release_profile_rejects_debug_launcher_and_link_flags(self):
        cache=(Path(__file__).resolve().parents[2]/'build-ansa-release/CMakeCache.txt').read_bytes()
        compiler=next(line.split('=',1)[1] for line in cache.decode().splitlines() if line.startswith('CMAKE_CXX_COMPILER:'))
        verify.release_cache(cache,compiler)
        for mutated in (cache.replace(b'CMAKE_BUILD_TYPE:STRING=Release',b'CMAKE_BUILD_TYPE:STRING=Debug'),cache+b'\nCMAKE_CXX_COMPILER_LAUNCHER:STRING=/tmp/untrusted-launcher\n',cache+b'\nCMAKE_EXE_LINKER_FLAGS:STRING=-fsanitize=address\n'):
            with self.subTest(mutation=mutated[-80:]):
                with self.assertRaises(ValueError):verify.release_cache(mutated,compiler)
    def test_pinned_nix_v2_basename_keys_are_normalized(self):
        name='abc-ansa-rust-0.1.0';self.assertEqual(verify.nix_info({'storeDir':'/nix/store','info':{name:{'closureSize':1}}}),{'/nix/store/'+name:{'closureSize':1}})
    def test_binary_guard_rejects_special_files_and_aliases(self):
        fifo=self.path/'e2e_adapter';os.mkfifo(fifo,0o700)
        with self.assertRaises(ValueError):verify.binary_hash(fifo,'e2e_adapter')
        fifo.unlink();fifo.symlink_to(Path(self.adapters['rust']))
        with self.assertRaises(ValueError):verify.binary_hash(fifo,'e2e_adapter')
        # No forbidden file is created or read: the guard precedes filesystem I/O.
        with self.assertRaises(ValueError):verify.binary_hash(self.path/'.env.never-open'/'e2e_adapter','e2e_adapter')
    def test_installed_package_must_use_exact_pinned_derivation(self):
        # A matching src/target alone must not admit a separately overridden
        # Debug package; reject before asking for any source file bytes.
        with patch.object(verify,'expected_derivation',return_value='/nix/store/expected-release.drv'),patch.object(verify.subprocess,'check_output',return_value='/nix/store/unrelated-debug.drv\n'):
            with self.assertRaisesRegex(ValueError,'pinned Release derivation'):verify.installed_source(Path('/nix/store/synthetic-ansa/bin/ansa'),'cpp',{},'a'*40)

if __name__=='__main__':unittest.main()
