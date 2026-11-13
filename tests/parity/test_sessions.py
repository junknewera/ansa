import copy,json,tempfile,unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
import sessions as s
import session_cases as v

class SessionEvidenceTests(unittest.TestCase):
    def test_valid_command_capture_and_raw_replay(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory=Path(temporary);paths={'cpp':directory/'adapter'};bundle=s.Bundle(directory,paths,{})
            def command(argv,cwd,output,label,**_):
                out=output/(label+'.stdout');err=output/(label+'.stderr');out.write_bytes(b'{"accepted":true,"value":{}}\n');err.write_bytes(b'')
                return {'argv':[str(p) for p in argv],'cwd':str(cwd),'exit_code':0,'timed_out':False,'elapsed_seconds':0.1,'stdout':s.protocol.raw(out),'stderr':s.protocol.raw(err)}
            row={'id':'control','status':'pass'};context=s.Context(bundle,row,True)
            with patch.object(s.common,'command',side_effect=command):self.assertEqual(context.call('cpp',{'op':'request'}),{'accepted':True,'value':{}})
            context.close();replay=s.Context(s.Bundle(directory,paths,bundle.commands),row,False)
            self.assertEqual(replay.call('cpp',{'op':'request'}),{'accepted':True,'value':{}});replay.close()
    def test_inventory_and_actual_cpp_frames(self):
        data=s.manifest();self.assertEqual(data['compare_ids'],s.inventory('compare'));self.assertEqual(data['fault_ids'],s.inventory('fault'))
        self.assertEqual(len(s.crash_specs()),11)
        for name,events in v.golden()['traces'].items():self.assertEqual(v.decode({s.PART:(v.GOLDEN.parent/(name+'.log')).read_bytes()}),events)
        self.assertEqual(v.encode([{'kind':'test','value':1}]),(v.GOLDEN.parent/'minimal.log').read_bytes())
    def test_checksum_chain_and_complete_header_control(self):
        original=(v.GOLDEN.parent/'minimal.log').read_bytes()
        self.assertEqual(s.protocol.structural(v.decode({s.PART:original})),s.protocol.structural([{'kind':'test','value':1}]))
        for offset in (0,10,27,110,174,len(original)-1):
            altered=bytearray(original);altered[offset]^=1
            with self.assertRaises((ValueError,UnicodeError,json.JSONDecodeError)):v.decode({s.PART:bytes(altered)})
        self.assertEqual(v.decode({s.PART:original[:92]}),[])
    def test_report_metadata_cannot_claim_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);paths={}
            for name,basename in [('cpp','ansa_session_oracle'),('rust','session_adapter')]:path=root/basename;path.write_bytes(b'synthetic');path.chmod(0o700);paths[name]=path
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
    def test_domain_numbers_diagnostics_and_empty_codecs_cannot_pass(self):
        for id in ('codec-request','codec-response-2','codec-response-6','summary-request','summary-native'):
            case=next(c for c in v.cases() if c['id']==id)
            with self.subTest(id=id),self.assertRaises(ValueError):v.output_pass(case,{'accepted':True,'value':{}})
        case=next(c for c in v.cases() if c['id']=='codec-request');original=copy.deepcopy(case['request']['value'])
        v.output_pass(case,{'accepted':True,'value':original})
        for raw in ('true','1.0','1e0','-0','0.10000000000000001'):
            altered=copy.deepcopy(original);altered['epoch']['exact_state']['revision']=s.protocol.strict_loads(raw)
            with self.subTest(raw=raw),self.assertRaises(ValueError):v.output_pass(case,{'accepted':True,'value':altered})
        altered=copy.deepcopy(original);altered['epoch']['exact_state']['diagnostic']='changed'
        self.assertNotEqual(s.comparable(case,original),s.comparable(case,altered))
    def test_binary_identity_rejects_aliases_fifo_and_forbidden_paths(self):
        import os
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);cpp=root/'ansa_session_oracle';rust=root/'session_adapter';cpp.write_bytes(b'executable');rust.write_bytes(b'executable');cpp.chmod(0o700);rust.chmod(0o700)
            s.binary_info({'cpp':cpp,'rust':rust})
            for kind in ('symlink','hardlink','fifo'):
                directory=root/kind;directory.mkdir();path=directory/'ansa_session_oracle'
                if kind=='symlink':path.symlink_to(cpp)
                elif kind=='hardlink':os.link(cpp,path)
                else:os.mkfifo(path,0o700)
                with self.subTest(kind=kind),self.assertRaises((ValueError,OSError)):s.binary_info({'cpp':path,'rust':rust})
                path.unlink()

if __name__=='__main__':unittest.main()
