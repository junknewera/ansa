"""Installed Rust CLI safety gates, without a C++ executable or CMake."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent / 'parity'))
import frontend
import frontend_io as io
import protocol
import run as common
import session_cases
import tty_flood

FAULTS = ('pending', 'pending-signal', 'read-error', 'stderr', 'epipe',
          'blocked-cancel', 'blocked-shutdown')
INVENTORY = ['legacy-reopen', 'credential-canaries', *FAULTS,
             *('tty-flood-' + str(i) for i in range(tty_flood.REPEATS))]


@contextlib.contextmanager
def owned():
    with tempfile.TemporaryDirectory(prefix='ansa-m4-', dir='/tmp') as name:
        root = Path(name)
        marker = root / 'fixture-owner'
        marker.write_bytes(b'ansa-m4-synthetic-v1\n')
        marker.chmod(0o600)
        yield io.fixture(root)


def close(child):
    io.stop(child)
    for pipe in (child.stdin, child.stdout, child.stderr):
        if pipe and not pipe.closed:
            pipe.close()


def legacy(binary, output):
    golden = common.ROOT / 'tests/fixtures/sessions/cli-cpp-v1'
    manifest = json.loads((golden / 'manifest.json').read_bytes())
    assert manifest['reference_tag'] == 'migration/m9-cutover'
    parts = {name: protocol.owned_bytes(golden / name, 1024*1024)
             for name in manifest['files']}
    assert {name: common.sha(golden/name) for name in parts} == manifest['files']
    assert [r['kind'] for r in session_cases.decode(parts)] == manifest['record_kinds']
    with owned() as (root, home, cwd):
        journal = root / 'journal'
        journal.mkdir(mode=0o700)
        for name, data in parts.items():
            path = journal / name
            path.write_bytes(data)
            path.chmod(0o600)
        child = subprocess.Popen([str(binary), '--offline-jsonl', '--session', str(journal)],
                                 cwd=cwd, env=io.environment(home, root, True),
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, start_new_session=True)
        lines = io.Lines(child)
        try:
            lines.until(lambda es: bool(es))
            first = lines.events[0]
            assert first['type'] == 'snapshot' and first['state'] == 'completed'
            prefix = {key: first[key] for key in ('schema_version', 'session', 'incarnation')}
            lines.send(dict(prefix, turn='turn', command_id='readonly', type='transcript',
                            before=0, limit=16, forward=False))
            lines.until(lambda es: any(e['type'] == 'transcript' for e in es))
            result = lines.finish()
            assert result['exit'] == 0 and not lines.err
            assert all(e['type'] in ('snapshot', 'transcript') for e in lines.events)
            page = next(e for e in lines.events if e['type'] == 'transcript')
            assert page['entries'] == manifest['transcript']
            assert {p.name: p.read_bytes() for p in journal.glob('part-*.log')} == parts
            (output/'legacy.stdout').write_bytes(lines.out)
            (output/'legacy.stderr').write_bytes(lines.err)
        finally:
            (output/'legacy.stdout').write_bytes(lines.out)
            (output/'legacy.stderr').write_bytes(lines.err)
            close(child)
    return dict(id='legacy-reopen', status='PASS', parts=manifest['files'],
                reference_binary_sha256=manifest['reference_binary_sha256'])


def canaries(binary, output):
    # These fixed values belong only to this synthetic test, never a real profile.
    secrets = (b'm10-synthetic-access-canary', b'm10-synthetic-refresh-canary',
               b'm10-synthetic-nonce-canary')
    with owned() as (root, home, cwd):
        auth = root/'auth'
        auth.mkdir(mode=0o700)
        identity = dict(version=1, host_id='synthetic-host', client_id='synthetic-client',
                        subject='synthetic-subject')
        credentials = dict(identity, scope='openid profile email offline_access resource.invoke chatgpt.tokens.use.direct',
                           session='synthetic-session', expires_at=0, earliest_refresh_at=0,
                           refresh_pending=False, access_token=secrets[0].decode(),
                           refresh_token=secrets[1].decode(), nonce=secrets[2].decode())
        for name, value in [('registration.json', identity), ('credentials.json', credentials)]:
            path = auth/name
            path.write_text(json.dumps(value))
            path.chmod(0o600)
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen(4)
            proxy = 'http://127.0.0.1:' + str(listener.getsockname()[1])
            env = io.environment(home, root, True)
            for key in ('http_proxy', 'HTTP_PROXY', 'https_proxy', 'HTTPS_PROXY', 'all_proxy', 'ALL_PROXY'):
                env[key] = proxy
            env.update(no_proxy='', NO_PROXY='')
            child = subprocess.Popen([str(binary), '--live-jsonl', '--session', str(root/'journal'),
                                      '--auth-dir', str(auth), '--model', 'gpt-6.1-sol',
                                      '--returned-model', 'gpt-6.1-sol'], cwd=cwd, env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, start_new_session=True)
            lines = io.Lines(child)
            try:
                lines.until(lambda es: bool(es))
                first = lines.events[0]
                assert first['type'] == 'snapshot' and first['state'] == 'idle'
                assert not select.select([listener], [], [], 0)[0]
                prefix = {k: first[k] for k in ('schema_version', 'session', 'incarnation')}
                lines.send(dict(prefix, turn='canary', command_id='submit', type='submit', text='hello', mode='basic'))
                lines.until(lambda es: any(e['type'] == 'command_accepted' for e in es))
                assert select.select([listener], [], [], 3)[0], 'no auth dispatch'
                with listener.accept()[0] as connection:
                    connection.settimeout(3)
                    head = b''
                    while b'\r\n\r\n' not in head:
                        data = connection.recv(4096)
                        assert data and len(head) < 16384
                        head += data
                    assert head.startswith(b'CONNECT auth.openai.com:443 ')
                    connection.sendall(b'HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
                lines.until(lambda es: any(e['type'] == 'snapshot' and e['state'] == 'paused' for e in es))
                result = lines.finish()
                assert result['exit'] == 1
                assert not any(e['type'] in ('tool_started', 'tool_finished', 'done', 'text_delta') for e in lines.events)
                parts = {Path(name).name: bytes.fromhex(data) for name, data in io.journals(root).items()}
                records = session_cases.decode(parts)
                assert [e['kind'] for e in records] == ['init', 'accepted', 'pause']
                assert not select.select([listener], [], [], 0)[0], 'unexpected auth retry'
                status = subprocess.run([str(binary), '--auth-status', '--auth-dir', str(auth)],
                                        cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=4)
                assert status.returncode == 0
                blobs = [lines.out, lines.err, status.stdout, status.stderr, *parts.values()]
                assert all(secret not in data for secret in secrets for data in blobs), 'credential canary leaked'
                (output/'canary.stdout').write_bytes(lines.out)
                (output/'canary.stderr').write_bytes(lines.err)
                (output/'canary-status.stdout').write_bytes(status.stdout)
                (output/'canary-status.stderr').write_bytes(status.stderr)
                (output/'canary-journal.json').write_text(json.dumps({k:v.hex() for k,v in parts.items()})+'\n')
            finally:
                (output/'canary.stdout').write_bytes(lines.out)
                (output/'canary.stderr').write_bytes(lines.err)
                common.write_json(output/'canary-journal.json', io.journals(root))
                close(child)
    return dict(id='credential-canaries', status='PASS', scanned_canaries=len(secrets))


def fault(binary, fixture, case, output):
    with owned() as (root, home, cwd):
        result = io.fault(str(binary), dict(case=case, fixture_binary=str(fixture)), root, home, cwd)
    common.write_json(output/(case+'.json'), result)
    class Observation:
        def driver(self, adapter, row):
            assert adapter == 'rustcli' and row == dict(kind='fault', case=case)
            return result
    # Reuse the established independent safety assertions, without its C++ capture.
    frontend.fault_case(Observation(), case)
    return dict(id=case, status='PASS')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--fixture-binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    report = dict(status='FAIL', cases=[], errors=[])
    try:
        source = common.source_info()
        assert not source['worktree_status'], 'clean checkpoint required'
        assert not (common.ROOT/'src').exists() and not (common.ROOT/'CMakeLists.txt').exists()
        platform = common.platform_info()
        assert platform['machine'] == 'aarch64' and platform['os_release']['ID'] == 'nixos'
        binary, fixture = args.binary.resolve(strict=True), args.fixture_binary.resolve(strict=True)
        report.update(source=source, platform=platform, binaries={str(p):common.sha(p) for p in (binary,fixture)})
        report['cases'].append(legacy(binary, args.output))
        report['cases'].append(canaries(binary, args.output))
        for case in FAULTS:
            report['cases'].append(fault(binary, fixture, case, args.output))
        flood = tty_flood.capture(fixture, args.output)
        common.write_json(args.output/'tty-flood.json', flood)
        report['cases'].extend(dict(id='tty-flood-'+str(r['sample']), status=r['status']) for r in flood['runs'])
        assert [r['id'] for r in report['cases']] == INVENTORY
        assert all(r['status'] == 'PASS' for r in report['cases'])
        assert common.source_info() == source, 'source changed during acceptance'
        report['status'] = 'PASS'
    except (AssertionError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        report['errors'].append(str(error) or type(error).__name__)
    common.write_json(args.output/'report.json', report)
    print(json.dumps(dict(status=report['status'], passed=len(report['cases']), errors=report['errors'])))
    return int(report['status'] != 'PASS')


if __name__ == '__main__':
    raise SystemExit(main())
