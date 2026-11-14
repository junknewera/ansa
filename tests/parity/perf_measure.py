"""One-shot M0 measurements; no runtime bootstrap, auth, or benchmark framework."""
import ctypes
from datetime import datetime, timezone
import hashlib
import json
import math
import mmap
import os
from pathlib import Path
import select
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time

# M0 measurement method preserved; capture sets these owned paths explicitly.
ROOT = Path(__file__).resolve().parents[2]
OUT = None
PAGE = os.sysconf('SC_PAGE_SIZE')
RUNS = []
TIMER = Path('/nix/store/yyf42wq6x1w3sknclkb29zm0vs2fvk9d-time-1.10/bin/time')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write('\n')


def state():
    def values(pattern, names):
        result = {}
        for directory in sorted(Path('/sys').glob(pattern)):
            result[directory.name] = {}
            for name in names:
                try:
                    result[directory.name][name] = (directory / name).read_text().strip()
                except OSError:
                    pass
        return result
    return {
        'utc': datetime.now(timezone.utc).isoformat(),
        'loadavg': Path('/proc/loadavg').read_text().split()[:3],
        'power': values('class/power_supply/*', ('type', 'online', 'status', 'capacity')),
        'cpufreq': values('devices/system/cpu/cpufreq/policy*', ('scaling_governor', 'scaling_min_freq', 'scaling_max_freq')),
        'thermal': values('class/thermal/thermal_zone*', ('type', 'temp')),
    }


def identity(pid):
    fields = (Path('/proc') / str(pid) / 'stat').read_text().rsplit(')', 1)[1].split()
    return int(fields[1]), int(fields[19])


def owned_tree(root, root_identity):
    # Ninja creates job process groups. Ownership is ancestry plus PID starttime.
    pending, visited, result = [(root, root_identity)], set(), []
    while pending:
        pid, expected = pending.pop()
        if pid in visited:
            continue
        visited.add(pid)
        try:
            proc = Path('/proc') / str(pid)
            if identity(pid) != expected:
                continue
            rss = int((proc / 'statm').read_text().split()[1]) * PAGE
            if identity(pid) != expected:
                continue
            result.append((pid, expected, rss))
            for task in (proc / 'task').iterdir():
                for child in map(int, (task / 'children').read_text().split()):
                    try:
                        child_identity = identity(child)
                        if child_identity[0] == pid and identity(pid) == expected:
                            pending.append((child, child_identity))
                    except (OSError, ValueError, IndexError):
                        pass
        except (OSError, ValueError, IndexError):
            pass
    return result


def run(label, argv, cwd, group=False, memory=False, timeout=600, process_env=None):
    before = state() if group else None
    peak = [0]
    done = threading.Event()
    def sample(pid, expected):
        while not done.is_set():
            peak[0] = max(peak[0], sum(row[2] for row in owned_tree(pid, expected)))
            done.wait(0.020)
    out, err = OUT / 'logs' / (label + '.stdout'), OUT / 'logs' / (label + '.stderr')
    actual = list(map(str, argv))
    rss_file = OUT / 'logs' / (label + '.rss-kib')
    if memory or group:
        actual = [str(TIMER), '-f', '%M', '-o', str(rss_file), '--', *actual]
    with out.open('xb') as stdout, err.open('xb') as stderr:
        start = time.perf_counter_ns()
        child = subprocess.Popen(actual, cwd=cwd, stdin=subprocess.DEVNULL,
                                 stdout=stdout, stderr=stderr, start_new_session=True,env=process_env)
        root_identity = identity(child.pid)
        watcher = threading.Thread(target=sample, args=(child.pid, root_identity)) if group else None
        if watcher:
            watcher.start()
        pidfd = os.pidfd_open(child.pid)
        poll = select.poll()
        poll.register(pidfd, select.POLLIN)
        timed_out = not bool(poll.poll(timeout * 1000))
        if timed_out:
            # Best-effort cleanup of identified owned descendants across Ninja groups.
            for pid, expected, _ in reversed(owned_tree(child.pid, root_identity)):
                try:
                    owned_fd = os.pidfd_open(pid)
                    if identity(pid) == expected:
                        signal.pidfd_send_signal(owned_fd, signal.SIGKILL)
                    os.close(owned_fd)
                except (ProcessLookupError, FileNotFoundError):
                    pass
        elapsed = (time.perf_counter_ns() - start) / 1e9
        done.set()
        if watcher:
            watcher.join()
        # Root remains waitable while the watcher uses its identity.
        _, status, usage = os.wait4(child.pid, 0)
        os.close(pidfd)
        child.returncode = os.waitstatus_to_exitcode(status)
    result = {
        'label': label, 'argv': actual, 'cwd': str(cwd),
        'wall_seconds': elapsed, 'launcher_maxrss_bytes': usage.ru_maxrss * 1024,
        'sampled_group_rss_bytes': peak[0] if group else None,
        'exit_code': child.returncode, 'timed_out': timed_out,
        'stdout': {'path': str(out.relative_to(OUT)), 'sha256': digest(out)},
        'stderr': {'path': str(err.relative_to(OUT)), 'sha256': digest(err)},
    }
    if memory or group:
        result['runtime_maxrss_bytes'] = int(rss_file.read_text().strip()) * 1024
        result['rss_raw'] = {'path': str(rss_file.relative_to(OUT)), 'sha256': digest(rss_file)}
    if group:
        result['host_before'], result['host_after'] = before, state()
    RUNS.append(result)
    with (OUT / 'progress.jsonl').open('a') as stream:
        stream.write(json.dumps(result, sort_keys=True) + '\n')
    if timed_out or child.returncode != 0:
        raise RuntimeError('measurement command failed: ' + label)
    return result


def validate_cli(result, flag):
    data = json.loads((OUT / result['stdout']['path']).read_bytes())
    if flag == '--self-check':
        assert data['project'] == 'ansa' and data['target'] == 'aarch64-linux'
    else:
        assert data == {'status': 'passed', 'mode': 'offline', 'tool_invocations': 1,
                        'native_replay': True, 'final_text': 'OMH_TOOL_OK:OMH_FIXED_VALUE_42'}


def evict_and_count(path):
    # Hashing/copying must finish before this point; do not read bytes after mincore.
    with path.open('rb') as stream:
        os.posix_fadvise(stream.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
        mapping = mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_COPY)
        address = ctypes.c_char.from_buffer(mapping)
        pages = math.ceil(len(mapping) / PAGE)
        vector = (ctypes.c_ubyte * pages)()
        libc = ctypes.CDLL(None, use_errno=True)
        libc.mincore.argtypes = (ctypes.c_void_p, ctypes.c_size_t, ctypes.POINTER(ctypes.c_ubyte))
        libc.mincore.restype = ctypes.c_int
        if libc.mincore(ctypes.addressof(address), len(mapping), vector) != 0:
            raise OSError(ctypes.get_errno(), 'mincore failed')
        resident = sum(bool(value & 1) for value in vector)
        del address
        mapping.close()
    assert resident == 0, ('executable pages remain resident', resident, pages)
    return {'resident_pages_before_exec': resident, 'file_pages': pages, 'page_bytes': PAGE}


def stats(rows, field):
    values = sorted(row[field] for row in rows)
    return {'count': len(values), 'min': values[0], 'median': statistics.median(values),
            'p95_nearest_rank': values[math.ceil(len(values) * .95) - 1], 'max': values[-1]}

