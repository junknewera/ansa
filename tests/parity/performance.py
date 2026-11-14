"""Release measurements. Frozen M0 budgets are read-only; missing M8 metrics stay open."""
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import hashlib
import re
import perf_measure as m
import run as common
import protocol
import frontend_io

BUDGET=common.ROOT/'tests/parity/perf_budget.json'
POLICY=common.ROOT/'tests/parity/fixtures/m8_perf_policy.json'
NEW_METRICS=json.loads(POLICY.read_text())['metrics']
def need(value,message):
    if not value:raise ValueError(message)
def statistics(values):
    need(values and all(type(v) in (int,float) and math.isfinite(v) and v>=0 for v in values),'invalid performance samples')
    return m.stats([{'value':value} for value in values],'value')
def safe_cli(binaries,directory):
    cwd=directory/'synthetic-cwd';cwd.mkdir();summary={}
    for flag,name in [('--self-check','self-check'),('--offline-demo','offline-demo')]:
        for language,binary in binaries.items():m.validate_cli(m.run(f'{language}-{name}-warmup',[binary,flag],cwd),flag)
        for category,count,memory,cold in [('warm',50,False,False),('cold',20,False,True),('rss-warm',20,True,False),('rss-cold',10,True,True)]:
            for index in range(count):
                for language,binary in binaries.items():
                    selected=binary;residency=None;label=f'{language}-{name}-{category}-{index:02}'
                    if cold:
                        copies=directory/'cold-executables';copies.mkdir(exist_ok=True);selected=copies/label;shutil.copyfile(binary,selected);selected.chmod(0o700)
                        with selected.open('rb') as file:os.fsync(file.fileno())
                        need(common.sha(selected)==common.sha(binary),'cold binary copy differs');residency=m.evict_and_count(selected)
                    result=m.run(label,[selected,flag],cwd,memory=memory);result['cache_condition']=residency if cold else 'warm-after-warmup'
                    m.validate_cli(result,flag)
            for language in binaries:
                prefix=f'{language}-{name}-{category}-';rows=[row for row in m.RUNS if row['label'].startswith(prefix)]
                summary[prefix[:-1]]=m.stats(rows,'runtime_maxrss_bytes' if memory else 'wall_seconds')
        print(name,'CLI samples complete',flush=True)
    return summary
def footprints(binaries,directory):
    result={}
    for language,binary in binaries.items():
        stripped=directory/(language+'-stripped');shutil.copyfile(binary,stripped);stripped.chmod(0o700)
        m.run(language+'-strip',['strip','--strip-all',stripped],common.ROOT)
        package=binary.parent.parent;need(str(package).startswith('/nix/store/'),'Nix package required for closure measurement')
        row=m.run(language+'-closure',['nix','path-info','--json','--json-format','2','--closure-size',package],common.ROOT)
        raw=json.loads((directory/row['stdout']['path']).read_bytes());info=next(iter(raw['paths'].values())) if 'paths' in raw else next(iter(raw.values()))
        result[language]={'path':str(binary),'sha256':common.sha(binary),'installed_binary_bytes':binary.stat().st_size,'strip_all_binary_bytes':stripped.stat().st_size,'strip_all_sha256':common.sha(stripped),'nix_closure_nar_bytes':info['closureSize']}
    return result
def build_measurements(directory):
    head=subprocess.check_output(['git','rev-parse','HEAD'],cwd=common.ROOT,text=True).strip()
    names=subprocess.check_output(['git','ls-tree','-r','--name-only',head,'--','CMakeLists.txt','src','include','rust'],cwd=common.ROOT,text=True).splitlines()
    names=[name for name in names if not any(p=='.env' or p.startswith('.env.') for p in Path(name).parts)]
    source=directory/'build-source';source.mkdir()
    for name in names:
        path=source/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(subprocess.check_output(['git','show',head+':'+name],cwd=common.ROOT))
    rust_target=directory/'rust-build-target'
    env=dict(os.environ,CARGO_TARGET_DIR=str(rust_target),CARGO_INCREMENTAL='0',RUSTC_WRAPPER='',RUSTC_WORKSPACE_WRAPPER='',CARGO_ENCODED_RUSTFLAGS='')
    for key in ('RUSTFLAGS','RUSTC_BOOTSTRAP','CARGO_PROFILE_RELEASE_OPT_LEVEL','CARGO_PROFILE_RELEASE_LTO','CARGO_PROFILE_RELEASE_DEBUG','CARGO_PROFILE_RELEASE_CODEGEN_UNITS'):env.pop(key,None)
    rust=['cargo','build','--manifest-path',source/'rust/Cargo.toml','--locked','--release','--bin','ansa-rs','-j2']
    m.run('rust-dependencies-warmup',rust,source,group=True,process_env=env)
    for case in range(3):
        cpp_target=directory/f'cpp-build-{case}'
        m.run(f'cpp-configure-{case}',['cmake','-S',source,'-B',cpp_target,'-G','Ninja','-DCMAKE_BUILD_TYPE=Release','-DBUILD_TESTING=OFF','-DCMAKE_CXX_COMPILER='+shutil.which('g++'),'-DCMAKE_CXX_COMPILER_LAUNCHER='],source,group=True)
        m.run(f'cpp-clean-build-{case}',['cmake','--build',cpp_target,'--target','ansa','-j2'],source,group=True)
        m.run(f'rust-clean-prep-{case}',['cargo','clean','--manifest-path',source/'rust/Cargo.toml','--release','-p','ansa','-p','ansa-linux'],source,process_env=env)
        m.run(f'rust-clean-build-{case}',rust,source,group=True,process_env=env)
        for language,path in [('cpp',source/'src/main.cpp'),('rust',source/'rust/crates/ansa/src/bin/ansa.rs')]:
            original=path.read_bytes()
            for edit in range(3):
                path.write_bytes(original+f'\n// M8 isolated entry point edit {case}-{edit}\n'.encode())
                command=rust if language=='rust' else ['cmake','--build',cpp_target,'--target','ansa','-j2']
                row=m.run(f'{language}-incremental-build-{case}-{edit}',command,source,group=True,process_env=env if language=='rust' else None)
                data=(directory/row['stderr' if language=='rust' else 'stdout']['path']).read_text()
                need(('Compiling ansa ' in data) if language=='rust' else ('src/main.cpp.o' in data and 'Linking CXX executable ansa' in data),'entry point was not rebuilt/relinked')
                row['changed_source_sha256']=common.sha(path)
            path.write_bytes(original)
        print('target-only build',case+1,'/3 complete',flush=True)
    return {language+'-'+kind:{field:m.stats([row for row in m.RUNS if row['label'].startswith(language+'-'+kind+'-')],field) for field in ('wall_seconds','runtime_maxrss_bytes','sampled_group_rss_bytes')} for language in ('cpp','rust') for kind in ('clean-build','incremental-build')}
def frozen_errors(report,budget):
    errors=[];bounds=budget['budgets'];rust=report['footprints']['rust']
    for metric in ('installed_binary_bytes','strip_all_binary_bytes','nix_closure_nar_bytes'):
        if rust[metric]>bounds[metric]:errors.append(metric+' exceeds frozen M0 ceiling')
    for command in ('self-check','offline-demo'):
        for cache in ('warm','cold'):
            label=f'rust-{command}-{cache}';limit=bounds['cli_wall_p95_seconds'][f'{command}_{cache}']
            if report['cli'][label]['p95_nearest_rank']>limit:errors.append(label+' p95 exceeds frozen M0 budget')
            label=f'rust-{command}-rss-{cache}';limit=bounds['cli_runtime_maxrss_bytes'][f'{command}_{cache}']
            if report['cli'][label]['max']>limit:errors.append(label+' exceeds frozen M0 budget')
    for kind in ('clean-build','incremental-build'):
        values=report['builds']['rust-'+kind]
        if values['wall_seconds']['max']>bounds[('clean' if kind=='clean-build' else 'incremental')+'_build_max_wall_seconds']:errors.append('rust-'+kind+' exceeds frozen M0 wall budget')
        for metric,bound in [('runtime_maxrss_bytes','build_single_process_maxrss_bytes'),('sampled_group_rss_bytes','build_sampled_descendants_maxrss_bytes')]:
            if values[metric]['max']>bounds[bound]:errors.append('rust-'+kind+' exceeds frozen M0 '+metric)
    return errors
def capture(args):
    output=Path(args.output).resolve();need(not any(p=='.env' or p.startswith('.env.') for p in output.parts),'forbidden perf path');output.mkdir(parents=True,exist_ok=False);(output/'logs').mkdir()
    m.OUT=output;m.RUNS=[];report={'schema_version':1,'suite':'performance','status':'FAIL','errors':[],'open_metrics':list(NEW_METRICS)}
    try:
        source=common.source_info();need(source['worktree_status']=='','clean performance checkpoint required')
        need(common.platform_info()['machine']=='aarch64','native aarch64 required')
        binaries={'cpp':Path(args.cpp).resolve(strict=True),'rust':Path(args.rust).resolve(strict=True)}
        need(binaries['cpp'].name=='ansa' and binaries['rust'].name=='ansa-rs','performance binary identity')
        budget=json.loads(BUDGET.read_text());need(budget['status']=='FROZEN_M0','frozen M0 budget required')
        need(common.sha(m.TIMER)==budget['environment']['timer']['sha256'],'GNU time identity')
        report.update(source=source,platform=common.platform_info(),budget_sha256=common.sha(BUDGET),policy_sha256=common.sha(POLICY),host_before=m.state(),method={'cli':'Unchanged M0 timer/RSS/cache method; interleaved C++ and Rust, same release profile','builds':'External dependencies primed once; own Rust packages cleaned each run; target-only -j2; entry point comment edits; sampled descendants every20ms','cold':'Only executable pages, mincore=0 before exec; filesystem/dependency caches not cold-controlled'})
        report['cli']=safe_cli(binaries,output);report['footprints']=footprints(binaries,output);report['builds']=build_measurements(output)
        report['errors']=frozen_errors(report,budget)
        need(source==common.source_info(),'performance source changed')
        report['status']='INCOMPLETE' if not report['errors'] else 'FAIL'
    except (OSError,ValueError,RuntimeError,KeyError,AssertionError,subprocess.SubprocessError) as error:report['errors'].append(str(error))
    report.update(runs=m.RUNS,host_after=m.state());common.write_json(output/'perf-report.json',report)
    print(json.dumps({key:report[key] for key in ('status','errors','open_metrics')},indent=2));return int(report['status']!='PASS')
