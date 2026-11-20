"""Owned JSONL shutdown regression: progressing, stalled and interrupted output."""
import argparse
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent / 'parity'))
import frontend_io as fixture
import session_cases

class PacedLines(fixture.Lines):
    def paced(self):
        for pipe in select.select([self.child.stdout, self.child.stderr], [], [], .001)[0]:
            try:
                data = os.read(pipe.fileno(), 4096)
            except BlockingIOError:
                continue
            if pipe is self.child.stdout:
                self.out += data
                self.buffer += data
                while b'\n' in self.buffer:
                    line, self.buffer = self.buffer.split(b'\n', 1)
                    if line:
                        self.events.append(json.loads(line))
            else:
                self.err += data
        assert len(self.out)+len(self.err) <= fixture.MAX


def run(binary, output, case):
    with tempfile.TemporaryDirectory(prefix='ansa-m4-', dir='/tmp') as name:
        root = Path(name)
        marker = root / 'fixture-owner'
        marker.write_bytes(b'ansa-m4-synthetic-v1\n')
        marker.chmod(0o600)
        root, home, cwd = fixture.fixture(root)
        child = subprocess.Popen([str(binary), '--frontend-fixture', str(root), 'large-jsonl'],
                                 cwd=cwd, env=fixture.environment(home, root, True),
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, start_new_session=True)
        lines = PacedLines(child)
        started = time.monotonic()
        commands = []
        try:
            lines.until(lambda es: bool(es))
            prefix = {key: lines.events[0][key] for key in ('schema_version', 'session', 'incarnation')}
            lines.prefix = prefix
            def send(kind, identifier, **fields):
                value = dict(prefix, turn='turn', command_id=identifier, type=kind, **fields)
                commands.append(value)
                lines.send(value)
            send('submit', 'submit', text='question', mode='basic')
            lines.until(lambda es: any(e['type'] == 'done' or e['type'] == 'snapshot' and e['state'] == 'completed' and e['durable'] for e in es), timeout=15)
            lines.prefix = None
            send('transcript', 'page', before=0, limit=16, forward=False)
            send('snapshot', 'snapshot', after_sequence=0)
            child.stdin.close()
            eof_at = time.monotonic()
            # Only the parent's reading rate changes; the real runtime/writer
            # must handle backpressure. No fixture sleep in production output.
            deadline = eof_at + 20
            signalled = False
            while child.poll() is None:
                now = time.monotonic()
                assert now < deadline, 'output shutdown did not terminate'
                reading = case == 'slow' or (case in ('stall-after-progress', 'interrupt') and now-eof_at < .8)
                if reading:
                    lines.paced()
                else:
                    if select.select([child.stderr], [], [], .001)[0]:
                        lines.err += child.stderr.read() or b''
                if case == 'interrupt' and now-eof_at >= .8 and not signalled:
                    child.send_signal(signal.SIGINT)
                    signalled = True
                time.sleep(.002)
            exited_at = time.monotonic()
            # Collect bytes already accepted by the pipe after child exit.
            for _ in range(1000):
                before = len(lines.out) + len(lines.err)
                lines.drain(0)
                if len(lines.out) + len(lines.err) == before:
                    break
            parts = fixture.journals(root)
            (output / (case+'.stdout')).write_bytes(lines.out)
            (output / (case+'.stderr')).write_bytes(lines.err)
            (output / (case+'.commands.json')).write_text(json.dumps(commands)+'\n')
            (output / (case+'.journals.json')).write_text(json.dumps(parts)+'\n')
            records = session_cases.decode({Path(k).name: bytes.fromhex(v) for k,v in parts.items()})
            # Canonical success is independent of frontend delivery failure.
            assert sum(e['kind'] == 'done' for e in records) == 1
            assert sum(e['kind'] == 'outcome' for e in records) == 12
            if case == 'slow':
                assert child.returncode == 0 and not lines.err, (child.returncode, lines.err)
                assert exited_at-eof_at > .5, 'reader did not exercise the old absolute deadline'
                assert not lines.buffer, 'partial JSONL event'
                assert [json.loads(line) for line in lines.out.splitlines()] == lines.events
                views = [e for e in lines.events if e['type'] == 'snapshot' and e['command_id'] == 'snapshot']
                assert views and views[-1]['completed_tools'] == 12
                assert all(t['output'] == 'x'*(100*1024) for t in views[-1]['tools'])
            else:
                expected = 130 if case == 'interrupt' else 1
                assert child.returncode == expected, child.returncode
                errors = [json.loads(line) for line in lines.err.splitlines()]
                code = 'cli_output_interrupted' if case == 'interrupt' else 'cli_output_stalled'
                assert any(e['type'] == 'error' and e['diagnostic'] == code for e in errors), errors
                assert exited_at-eof_at < (2.5 if case != 'blocked' else 1.5)
            return dict(case=case, passed=True, exit=child.returncode,
                        elapsed=time.monotonic()-started, drain_seconds=exited_at-eof_at,
                        output_bytes=len(lines.out), complete_events=len(lines.events))
        finally:
            # Retain failures as well, without printing large payloads.
            (output / (case+'.stdout')).write_bytes(lines.out)
            (output / (case+'.stderr')).write_bytes(lines.err)
            (output / (case+'.journals.json')).write_text(json.dumps(fixture.journals(root))+'\n')
            fixture.stop(child)
            for pipe in (child.stdin, child.stdout, child.stderr):
                if pipe and not pipe.closed:
                    pipe.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture-binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--case', choices=('slow', 'blocked', 'stall-after-progress', 'interrupt'))
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    cases = [args.case] if args.case else ['slow', 'blocked', 'stall-after-progress', 'interrupt']
    rows = [run(args.fixture_binary.resolve(strict=True), args.output, case) for case in cases]
    report = dict(passed=len(rows), cases=rows)
    (args.output/'report.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()
