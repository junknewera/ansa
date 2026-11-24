"""Native nightly quality gate; failures and unsupported tools stay non-PASS."""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import subprocess
import sys
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
    flags=[] if mode in ('miri','reference') else [f'-Zsanitizer={mode}','-Cdebuginfo=1']
    if mode in ('address','thread'):flags.append('-Ctarget-feature=-outline-atomics')
    env['CARGO_ENCODED_RUSTFLAGS']='\x1f'.join(flags)
    if mode=='miri':env.update(MIRI=str(compiler.parent/'miri'),MIRIFLAGS='-Zmiri-strict-provenance')
    elif mode!='reference':env.update(ASAN_OPTIONS='detect_leaks=1:halt_on_error=1',LSAN_OPTIONS='exitcode=23',TSAN_OPTIONS='halt_on_error=1')
    env.setdefault('RUST_TEST_THREADS','1')
    return env,flags

def test_inventory(data, *, listing=False):
    suites = []
    names = None
    started = set()
    count = None
    for line in data.decode('utf-8', 'strict').splitlines():
        if not line.startswith('{'):
            continue  # A test can forward a child's own pretty libtest output.
        row = json.loads(line)
        kind = row.get('type')
        event = row.get('event')
        if kind == 'suite' and event == ('discovery' if listing else 'started'):
            if names is not None:
                raise ValueError('overlapping quality test suites')
            names = set()
            started = set()
            count = None if listing else row['test_count']
        elif names is None:
            raise ValueError('quality result outside a suite')
        elif kind == 'test':
            name = row['name']
            if not listing and event == 'started' and name not in started:
                started.add(name)
                continue
            acceptable = (event == 'discovered' and row['ignore'] is False
                          if listing else event == 'ok' and name in started)
            if not acceptable or name in names:
                raise ValueError('quality test failed, skipped or repeated: '+name)
            names.add(name)
        elif kind == 'suite' and event == ('completed' if listing else 'ok'):
            expected = row['tests'] if listing else row['passed']
            unexecuted = ('benchmarks', 'ignored') if listing else ('failed', 'ignored', 'measured', 'filtered_out')
            if len(names) != expected or any(row[field] != 0 for field in unexecuted):
                raise ValueError('incomplete quality test suite')
            if listing and row['total'] != expected or not listing and (names != started or count != expected):
                raise ValueError('quality suite count does not match named tests')
            suites.append(tuple(sorted(names)))
            names = None
        else:
            raise ValueError('unexpected quality test event')
    if names is not None or not any(suites):
        raise ValueError('missing or unfinished quality test inventory')
    return suites


def executed_tests(data, reference):
    executed = test_inventory(data)
    if Counter(executed) != Counter(reference):
        raise ValueError('passed quality tests differ from the reference inventory')
    return sum(map(len, executed))

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
        reference_command = None
        if args.mode=='miri':
            command=[str(compiler.parent/'cargo'),'miri','test','--manifest-path','rust/Cargo.toml','--locked','-p','ansa','--test','miri_pure','--target',TARGET]
        else:
            runner=Path(shutil.which('env')).resolve().parent/'env'
            if not str(runner).startswith('/nix/store/') or not runner.is_file():raise ValueError('trusted native runner unavailable')
            cargo=[str(compiler.parent/'cargo'),'--config',f'target.{TARGET}.runner='+json.dumps([str(runner),'--']),'test']
            selection=['--manifest-path','rust/Cargo.toml','--workspace','--locked','--lib','--tests','--target',TARGET,'--profile','test']
            reference_command=cargo+selection
            command=cargo+(['-Zbuild-std'] if args.mode in ('address','thread') else [])+selection
            report.update(runner=str(runner),runner_sha256=common.sha(runner))
        report['std_instrumented']=args.mode in ('address','thread')
        env,flags=environment(args.mode,compiler)
        report.update(compiler_path=str(compiler),compiler_sha256=common.sha(compiler),rustflags=flags)
        # Environment is passed to this process only, never dumped into evidence.
        import signal,time
        def run(label, command, env):
            out=output/(label+'.stdout');err=output/(label+'.stderr')
            begin=time.monotonic()
            timed_out = False
            with out.open('wb') as stdout,err.open('wb') as stderr:
                child=subprocess.Popen(command,cwd=common.ROOT,env=env,stdout=stdout,stderr=stderr,start_new_session=True)
                try:code=child.wait(timeout=1800)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid,signal.SIGKILL)
                    code=child.wait()
                    timed_out=True
            report['commands'][label]={'argv':command,'exit_code':code,'stdout':protocol.raw(out),'stderr':protocol.raw(err),'elapsed':time.monotonic()-begin}
            if timed_out:
                raise ValueError('quality timeout: '+label)
            if code:
                raise ValueError('quality command failed: '+label)
            return out.read_bytes()
        libtest=['--','-Z','unstable-options','--format','json']
        if reference_command is not None:
            reference_env,_=environment('reference',compiler)
            reference_raw=run('reference',reference_command+libtest+['--list'],reference_env)
            reference=test_inventory(reference_raw,listing=True)
        else:
            reference=[(
                'frontend_editor_and_controls_remain_owned_and_bounded',
                'owned_json_survives_source_drop_and_preserves_number_lexemes',
            )]
        report['reference_test_suites']=reference
        data=run('workspace',command+libtest+['--test-threads=1'],env)
        report['executed_tests']=executed_tests(data,reference)
        if common.source_info()!=source:raise ValueError('quality source changed')
        report['status']='PASS'
    except (OSError,ValueError,KeyError,TypeError,subprocess.SubprocessError) as error:report['errors'].append(str(error))
    common.write_json(output/'quality-report.json',report)
    print(json.dumps({'mode':args.mode,'status':report['status'],'errors':report['errors']},indent=2))
    return int(report['status']!='PASS')

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode',choices=['address','leak','thread','miri'],required=True)
    parser.add_argument('--output',required=True)
    raise SystemExit(capture(parser.parse_args()))
