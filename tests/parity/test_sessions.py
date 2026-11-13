import copy,json,tempfile,unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
import sessions as s
import session_cases as v

class SessionEvidenceTests(unittest.TestCase):
    def test_inventory_and_actual_cpp_frames(self):
        data=s.manifest();self.assertEqual(data['compare_ids'],s.inventory('compare'));self.assertEqual(data['fault_ids'],s.inventory('fault'))
        self.assertEqual(len(s.crash_specs()),11)
        for name,events in v.golden()['traces'].items():self.assertEqual(v.decode({s.PART:(v.GOLDEN.parent/(name+'.log')).read_bytes()}),events)
        self.assertEqual(v.encode([{'kind':'test','value':1}]),(v.GOLDEN.parent/'minimal.log').read_bytes())
    def test_checksum_chain_and_complete_header_control(self):
        original=(v.GOLDEN.parent/'minimal.log').read_bytes()
        self.assertEqual(v.decode({s.PART:original}),[{'kind':'test','value':1}])
        for offset in (0,10,27,110,174,len(original)-1):
            altered=bytearray(original);altered[offset]^=1
            with self.assertRaises((ValueError,UnicodeError,json.JSONDecodeError)):v.decode({s.PART:bytes(altered)})
        self.assertEqual(v.decode({s.PART:original[:92]}),[])
    def test_report_metadata_cannot_claim_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);paths={}
            for name in ('cpp','rust'):path=root/name;path.write_bytes(b'synthetic');paths[name]=path
            report={'schema_version':1,'suite':'sessions','mode':'compare','status':'PASS','errors':[], 'source':{'files':{},'worktree_status':''},'platform':{'system':'Linux','machine':'aarch64','os_release':{'ID':'nixos'}},'fixtures':s.fixtures(),'manifest_sha256':s.common.sha(s.MANIFEST),'binaries':s.binary_info(paths),'commands':{},'cases':[]}
            with patch.object(s.common,'source_info',return_value={'files':{}}):
                self.assertTrue(s.verify(report,root))
                for change in ({'schema_version':True},{'platform':{'system':'Linux','machine':'x86_64','os_release':{'ID':'nixos'}}},{'source':{'files':{},'worktree_status':'dirty'}},{'fixtures':{}},{'manifest_sha256':'0'*64}):
                    with self.subTest(change=change):self.assertTrue(s.verify({**report,**change},root))
    def test_existing_evidence_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            path=Path(temporary);report=path/'session-report.json';report.write_bytes(b'original')
            args=SimpleNamespace(action='compare',cpp_build=Path('build-m4'),rust_target=Path('rust/target'),output=path)
            self.assertEqual(s.capture(args),1);self.assertEqual(report.read_bytes(),b'original')
    def test_unsafe_or_duplicate_snapshot_cannot_pass(self):
        row={'name_hex':s.PART.encode().hex(),'kind':'file','bytes_hex':'00'}
        self.assertEqual(s.parts([row]),{s.PART:b'\0'})
        for values in ([row,row],[dict(row,kind='unsafe')],[dict(row,name_hex=b'../outside'.hex())],[dict(row,bytes_hex='not-hex')]):
            with self.assertRaises(ValueError):s.parts(values)

if __name__=='__main__':unittest.main()
