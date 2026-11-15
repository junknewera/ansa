"""Kill production Rust replay drop/reorder mutants without CMake or C++."""
import argparse
import json
import os
from pathlib import Path
import shutil
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent/'parity'))
import run as common

SITE = 'input.extend(response.items.iter().map(|i| i.value().clone()));'
TEST = 'replay_serializes_prefix_native_values_and_matching_results'
MUTANTS = {
    'drop': ('/* mutation: omit native items */', 'replay oracle: native item count'),
    'reorder': ('input.extend(response.items.iter().rev().map(|i| i.value().clone()));',
                'replay oracle: native item order and values'),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--target-dir', type=Path, default=common.ROOT/'build-replay-mutations/target')
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    report = dict(status='FAIL', killed=[], commands={}, errors=[])
    try:
        source = common.source_info()
        assert not source['worktree_status'], 'clean checkpoint required'
        report['source'] = source
        workspace = args.output/'workspace'
        workspace.mkdir()
        for name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml'):
            shutil.copy2(common.ROOT/'rust'/name, workspace/name)
        shutil.copytree(common.ROOT/'rust/crates', workspace/'crates')
        shutil.copytree(common.ROOT/'tests/fixtures', workspace/'tests/fixtures')
        os.environ['ANSA_PROTOCOL_FIXTURES'] = str(workspace/'tests/fixtures')
        path = workspace/'crates/ansa/src/provider/replay.rs'
        original = path.read_text()
        assert original.count(SITE) == 1, 'production replay mutation site changed'
        argv = ['cargo', 'test', '--manifest-path', str(workspace/'Cargo.toml'), '--locked',
                '--target-dir', str(args.target_dir.resolve()), '-j', '2', '-p', 'ansa',
                '--test', 'protocol_parity', TEST, '--', '--exact']
        os.environ['CARGO_TERM_COLOR'] = 'never'
        for mode in ('baseline', *MUTANTS):
            path.write_text(original if mode == 'baseline' else original.replace(SITE, MUTANTS[mode][0]))
            result = common.command(argv, common.ROOT, args.output, mode, timeout=600)
            report['commands'][mode] = result
            stdout = (args.output/result['stdout']['file']).read_bytes()
            stderr = (args.output/result['stderr']['file']).read_bytes()
            assert not result['timed_out'], 'mutation build timed out'
            assert b'Finished `test` profile' in stderr and b'could not compile' not in stderr, 'mutation did not reach a compiled test'
            if mode == 'baseline':
                assert result['exit_code'] == 0 and b'1 passed; 0 failed;' in stdout, 'unmodified replay failed'
            else:
                assert result['exit_code'] == 101 and b'0 passed; 1 failed;' in stdout, 'mutant survived or failed outside the assertion'
                assert MUTANTS[mode][1].encode() in stdout+stderr, 'wrong mutation failure'
                report['killed'].append(mode)
        path.write_text(original)
        assert report['killed'] == list(MUTANTS)
        assert common.source_info() == source, 'source changed during mutation capture'
        report['status'] = 'PASS'
    except (AssertionError, OSError, ValueError, KeyError) as error:
        report['errors'].append(str(error) or type(error).__name__)
    common.write_json(args.output/'report.json', report)
    print(json.dumps({key: report[key] for key in ('status', 'killed', 'errors')}))
    return int(report['status'] != 'PASS')


if __name__ == '__main__':
    raise SystemExit(main())
