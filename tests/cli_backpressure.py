"""Linux PTY regression for Cancel when the JSONL writer is blocked."""

import fcntl
import json
import os
import pathlib
import pty
import subprocess
import sys
import termios
import time

master, slave = pty.openpty()
attrs = termios.tcgetattr(slave)
attrs[3] &= ~termios.ECHO
termios.tcsetattr(slave, termios.TCSANOW, attrs)
reader, writer = os.pipe()
capacity = fcntl.fcntl(writer, fcntl.F_SETPIPE_SZ, 4096)
with subprocess.Popen([sys.argv[1], '--offline-jsonl'], stdin=slave, stdout=writer, stderr=subprocess.PIPE) as proc:
    try:
        os.close(slave)
        out = os.fdopen(reader, 'rb', buffering=0)
        initial = json.loads(out.readline())
        prefix = {k: initial[k] for k in ('schema_version', 'session', 'incarnation')}

        def send(kind, ident, **payload):
            message = {**prefix, 'turn': 'test', 'type': kind, 'command_id': ident, **payload}
            os.write(master, json.dumps(message).encode() + b'\n')

        send('submit', 'submit', text='fixture')
        accepted = json.loads(out.readline())
        assert accepted['type'] == 'command_accepted'
        assert json.loads(out.readline())['state'] == 'generating'
        assert os.write(writer, b'X' * capacity) == capacity
        os.close(writer)

        send('snapshot', 'view', after_sequence=accepted['stream_sequence'])
        limit = time.monotonic() + 0.2
        while time.monotonic() < limit:
            waits = [task.read_text() for task in pathlib.Path(f'/proc/{proc.pid}/task').glob('*/wchan')]
            if any('pipe_write' in w for w in waits):
                break
            time.sleep(0.005)
        else:
            raise RuntimeError('stdout writer did not block')

        # Wake the read already in progress, then force the next glibc underflow.
        os.write(master, b'\n')
        time.sleep(0.05)
        send('cancel', 'cancel', target=accepted['request_id'])
        time.sleep(0.65)

        assert out.read(capacity) == b'X' * capacity
        os.write(master, b'\x04')
        assert proc.wait(timeout=3) == 0
        events = [json.loads(line) for line in out]
        kinds = [e['type'] for e in events]
        assert 'cancelled' in kinds, proc.stderr.read().decode()
        assert 'tool_started' not in kinds, 'stdout backpressure delayed Cancel past tool dispatch'
    finally:
        if proc.poll() is None:
            proc.kill()
        os.close(master)
