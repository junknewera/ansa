"""Native nightly quality gate; failures and unsupported tools stay non-PASS."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import run as common
import protocol

TARGET='aarch64-unknown-linux-gnu'

def capture(args):
    output=Path(args.output).resolve();output.mkdir(parents=True,exist_ok=False)
    source=common.source_info()
    report={'schema_version':1,'kind':'nightly-quality','source':source,'platform':common.platform_info(),'mode':args.mode,'status':'FAIL','commands':{},'errors':[]}
    try:
        version=subprocess.check_output(['rustc','--version'],text=True).strip()
        expected=json.loads((common.ROOT/'nix/quality-toolchain.json').read_text())
        if version!='rustc '+expected['components']['rustc']['version']:raise ValueError('wrong quality compiler')
        if source['worktree_status']:raise ValueError('clean quality checkpoint required')
        report.update(compiler=version,toolchain_pin_sha256=common.sha(common.ROOT/'nix/quality-toolchain.json'))
        if args.mode=='miri':
            command=['cargo','miri','test','--manifest-path','rust/Cargo.toml','--locked','-p','ansa','--test','miri_pure','--target',TARGET]
        else:
            command=['cargo','test','-Zbuild-std','--manifest-path','rust/Cargo.toml','--workspace','--locked','--lib','--tests','--target',TARGET]
        env=dict(os.environ,CARGO_TARGET_DIR=str(common.ROOT/'build-quality'/args.mode),CARGO_BUILD_JOBS='2',CARGO_INCREMENTAL='0')
        if args.mode!='miri':
            env['RUSTFLAGS']=f'-Zsanitizer={args.mode} -Cdebuginfo=1'
            env['ASAN_OPTIONS']='detect_leaks=1:halt_on_error=1'
            env['LSAN_OPTIONS']='exitcode=23'
            env['TSAN_OPTIONS']='halt_on_error=1'
        # Environment is passed to this process only, never dumped into evidence.
        label='workspace';out=output/(label+'.stdout');err=output/(label+'.stderr')
        import signal,time
        begin=time.monotonic()
        with out.open('wb') as stdout,err.open('wb') as stderr:
            child=subprocess.Popen(command,cwd=common.ROOT,env=env,stdout=stdout,stderr=stderr,start_new_session=True)
            try:code=child.wait(timeout=1800)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid,signal.SIGKILL);child.wait();raise ValueError('quality timeout')
        report['commands'][label]={'argv':command,'exit_code':code,'stdout':protocol.raw(out),'stderr':protocol.raw(err),'elapsed':time.monotonic()-begin}
        if code:raise ValueError('quality command failed; raw diagnostics retained')
        if common.source_info()!=source:raise ValueError('quality source changed')
        report['status']='PASS'
    except (OSError,ValueError,subprocess.SubprocessError) as error:report['errors'].append(str(error))
    common.write_json(output/'quality-report.json',report)
    print(json.dumps({'mode':args.mode,'status':report['status'],'errors':report['errors']},indent=2))
    return int(report['status']!='PASS')

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode',choices=['address','leak','thread','miri'],required=True)
    parser.add_argument('--output',required=True)
    raise SystemExit(capture(parser.parse_args()))
