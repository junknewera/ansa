#!/usr/bin/env python3
"""Isolated synthetic process-spike controller. Signals only waitable children."""
import ctypes
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time

CASES = ("cancel-detached", "control-eof", "inherited-signals", "missing-proc",
         "missing-pidfd", "cancel-after-intent", "consume-once", "supervisor-death",
         "direct-helper", "directory-substitution", "helper-consume-once")


def own_children():
    return [int(value) for value in Path(f'/proc/self/task/{os.getpid()}/children').read_text().split()]


def cleanup():
    until = time.monotonic() + 3
    reaped = 0
    while own_children():
        for pid in own_children():
            try:
                os.waitid(os.P_PID, pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                fd = os.pidfd_open(pid)
                try:
                    signal.pidfd_send_signal(fd, signal.SIGKILL)
                finally:
                    os.close(fd)
                os.waitpid(pid, 0)
                reaped += 1
            except ChildProcessError:
                pass
        if time.monotonic() > until:
            raise AssertionError('test-owned descendants escaped cleanup')
    return reaped


def event(child):
    if not select.select([child.stdout], [], [], 3)[0]:
        raise AssertionError('supervisor handshake timeout')
    line = child.stdout.readline(4096)
    if not line.endswith(b'\n'):
        raise AssertionError('supervisor handshake missing')
    return json.loads(line)


def start(binary, path, mode='--m1-supervisor'):
    return subprocess.Popen([binary, mode, str(path)], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def stop(child, eof=False):
    if not eof:
        child.stdin.write(b'x')
        child.stdin.flush()
    child.stdin.close()
    child.stdin = None
    out, err = child.communicate(timeout=3)
    assert child.returncode == 0, (out, err)
    result = json.loads(out)
    assert result['effect'] == 'known_result' and result['reaped'] is True, result
    assert cleanup() == 0, 'worker left adopted descendants'
    return result


def rejected(binary, path, mode='--m1-supervisor', extra=()):
    # Keep control open during dispatch. stdin=DEVNULL would test early EOF only.
    child = subprocess.Popen([binary, mode, str(path), *extra], stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        child.wait(timeout=3)
        out = child.stdout.read()
        err = child.stderr.read()
        assert child.returncode != 0, (mode, out, err)
        if mode == '--m1-missing-pidfd':
            assert b'pidfd_capability_probe:' in err and b'os error 1' in err, err
    finally:
        child.stdin.close()
        child.stdout.close()
        child.stderr.close()


def run(binary):
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
    libc.prctl.restype = ctypes.c_int
    if libc.prctl(36, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), 'test controller subreaper unavailable')
    results = []
    with tempfile.TemporaryDirectory(prefix='ansa-risk-process-') as temporary:
        root = Path(temporary)
        for case in CASES:
            path = root / case
            path.mkdir(mode=0o700)
            try:
                if case in ('cancel-detached', 'control-eof', 'inherited-signals', 'consume-once', 'helper-consume-once', 'supervisor-death'):
                    mode = '--m1-inherited-supervisor' if case == 'inherited-signals' else '--m1-supervisor'
                    child = start(binary, path, mode)
                    ready = event(child)
                    assert ready['state'] == 'ready' and ready['threads'] == 1, ready
                    assert (path/'consumed').is_file() and (path/'primary.claim').is_file()
                    if case == 'supervisor-death':
                        child.kill()  # Owned Popen child, not a reported PID.
                        child.communicate(timeout=3)
                        assert not (path/'outcome').exists()
                        assert cleanup() >= 1  # Adopt and reap primary/detached even across setsid.
                        rejected(binary, path)
                        results.append({'id':case,'status':'pass','effect':'effect_unknown','retry':'blocked'})
                        continue
                    stop(child, case == 'control-eof')
                    assert (path/'outcome').read_text() == 'known_cancelled\n'
                    if case == 'consume-once':
                        before = hashlib.sha256((path/'primary.started').read_bytes()).hexdigest()
                        rejected(binary, path)
                        assert hashlib.sha256((path/'primary.started').read_bytes()).hexdigest() == before
                    if case == 'helper-consume-once':
                        meta = path.stat()
                        rejected(binary, path, '--m1-detached', (f'{meta.st_dev}:{meta.st_ino}',))
                elif case in ('missing-proc', 'missing-pidfd'):
                    rejected(binary, path, '--m1-' + case)
                    assert not (path/'consumed').exists() and not (path/'primary.started').exists()
                elif case == 'cancel-after-intent':
                    child = start(binary, path, '--m1-intent-pause')
                    assert event(child)['state'] == 'intent'
                    child.stdin.close()
                    child.stdin = None
                    child.communicate(timeout=3)
                    assert child.returncode != 0 and (path/'consumed').exists()
                    assert not (path/'primary.started').exists()
                elif case == 'direct-helper':
                    meta = path.stat()
                    rejected(binary, path, '--m1-detached', (f'{meta.st_dev}:{meta.st_ino}',))
                    assert not (path/'detached.pid').exists()
                elif case == 'directory-substitution':
                    old = path.stat()
                    path.rename(root/'old-directory')
                    path.mkdir(mode=0o700)
                    marker = path/'consumed'
                    marker.write_text('m1-fixture-operation-v1\n')
                    marker.chmod(0o600)
                    rejected(binary, path, '--m1-detached', (f'{old.st_dev}:{old.st_ino}',))
                    assert not (path/'detached.pid').exists()
                assert cleanup() == 0, case
                results.append({'id':case,'status':'pass'})
            finally:
                cleanup()
    assert [row['id'] for row in results] == list(CASES)
    return {'schema_version':1,'suite':'process-risk','cases':results}


if __name__ == '__main__':
    print(json.dumps(run(str(Path(sys.argv[1]).resolve())), sort_keys=True))
