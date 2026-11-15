"""Native nightly quality gate; failures and unsupported tools stay non-PASS."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import re
import shutil
import run as common
import protocol

TARGET='aarch64-unknown-linux-gnu'

def environment(mode,compiler):
    env=dict(os.environ)
    for key in tuple(env):
        if key.startswith('CARGO_TARGET_') and key.endswith('_RUNNER'):env.pop(key)
    for key in ('RUSTC_WRAPPER','RUSTC_WORKSPACE_WRAPPER','RUSTC_BOOTSTRAP','RUSTFLAGS','RUSTDOCFLAGS','CARGO_ENCODED_RUSTFLAGS','CARGO_ENCODED_RUSTDOCFLAGS','MIRI','MIRI_SYSROOT','MIRI_LIB_SRC','MIRI_SETUP_FLAGS','MIRI_SKIP_SETUP','MIRI_BE_RUSTC','MIRIFLAGS'):
        env.pop(key,None)
    env.update(RUSTC=str(compiler),RUSTDOC=str(compiler.parent/'rustdoc'),RUSTC_WRAPPER='',RUSTC_WORKSPACE_WRAPPER='',CARGO_TARGET_DIR=str(common.ROOT/'build-quality'/mode),CARGO_BUILD_JOBS='2',CARGO_INCREMENTAL='0')
    env['PATH']=str(compiler.parent)+os.pathsep+env.get('PATH','')
    flags=[] if mode=='miri' else [f'-Zsanitizer={mode}','-Cdebuginfo=1']
    if mode in ('address','thread'):flags.append('-Ctarget-feature=-outline-atomics')
    env['CARGO_ENCODED_RUSTFLAGS']='\x1f'.join(flags)
    if mode=='miri':env.update(MIRI=str(compiler.parent/'miri'),MIRIFLAGS='-Zmiri-strict-provenance')
    else:env.update(ASAN_OPTIONS='detect_leaks=1:halt_on_error=1',LSAN_OPTIONS='exitcode=23',TSAN_OPTIONS='halt_on_error=1')
    return env,flags

def executed_tests(data,mode):
    text=data.decode('utf-8','strict')
    rows=re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;',text)
    expected=2 if mode=='miri' else 117
    if not rows or sum(int(row[0]) for row in rows)!=expected or any(any(int(v) for v in row[1:]) for row in rows):raise ValueError('quality executed/ignored test inventory')
    return expected

def capture(args):
    output=Path(args.output).resolve();output.mkdir(parents=True,exist_ok=False)
    source=common.source_info()
    report={'schema_version':1,'kind':'nightly-quality','source':source,'platform':common.platform_info(),'mode':args.mode,'status':'FAIL','commands':{},'errors':[]}
    try:
        compiler=Path(shutil.which('rustc')).resolve(strict=True)
        version=subprocess.check_output([compiler,'--version'],text=True).strip()
        expected=json.loads((common.ROOT/'nix/quality-toolchain.json').read_text())
        if version!='rustc '+expected['components']['rustc']['version'] or not compiler.parent.parent.name.endswith('ansa-quality-nightly-'+expected['date']):raise ValueError('wrong quality compiler')
        if source['worktree_status']:raise ValueError('clean quality checkpoint required')
        report.update(compiler=version,toolchain_pin_sha256=common.sha(common.ROOT/'nix/quality-toolchain.json'))
        if args.mode=='miri':
            command=[str(compiler.parent/'cargo'),'miri','test','--manifest-path','rust/Cargo.toml','--locked','-p','ansa','--test','miri_pure','--target',TARGET]
        else:
            runner=Path(shutil.which('env')).resolve().parent/'env'
            if not str(runner).startswith('/nix/store/') or not runner.is_file():raise ValueError('trusted native runner unavailable')
            command=[str(compiler.parent/'cargo'),'--config',f'target.{TARGET}.runner='+json.dumps([str(runner),'--']),'test',*(['-Zbuild-std'] if args.mode in ('address','thread') else []),'--manifest-path','rust/Cargo.toml','--workspace','--locked','--lib','--tests','--target',TARGET]
            report.update(runner=str(runner),runner_sha256=common.sha(runner))
        report['std_instrumented']=args.mode in ('address','thread')
        env,flags=environment(args.mode,compiler)
        report.update(compiler_path=str(compiler),compiler_sha256=common.sha(compiler),rustflags=flags)
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
        report['executed_tests']=executed_tests(out.read_bytes(),args.mode)
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
