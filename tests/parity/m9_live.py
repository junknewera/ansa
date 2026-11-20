"""Explicit M9 live acceptance over production executables, not a benchmark runner."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import tempfile
import time

import performance_evidence as proof
import performance_metrics as measured
import protocol
import run as common
import session_cases

MODEL = 'gpt-6.1-sol'
CAP = 12
CONFIG = {'schema_version': 1, 'model': MODEL, 'returned_model': MODEL,
          'compact_bytes': 4 * 1024 * 1024, 'tool_rounds': 2, 'tool_calls': 4,
          'process_timeout_ms': 30000, 'process_output_bytes': 1024}
MARKER = 'M9_ORCHID_42'
PRIMARY_COMMAND = 'cat seed.txt > result.txt; printf M9_COMMAND_OK'
CANCEL_COMMAND = 'printf ready > cancel_started; echo $$ > cancel_pid; sleep 20; printf done > cancel_finished'


def need(condition, reason):
    if not condition:
        raise ValueError(reason)


class Budget:
    """Reserve an upper bound BEFORE each one-Submit process; never refund it.

    Two tool rounds permit at most three generation requests. One pre-Submit
    compact request is also reserved. No Resume or second Submit is sent.
    Auth/catalog requests are not model generation/summary requests.
    """
    def __init__(self, path):
        self.path = Path(path)

    def reserve(self, label):
        need(label in ('primary', 'continue', 'cancel'), 'unknown live segment')
        flags = os.O_RDWR | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW | os.O_CLOEXEC
        with os.fdopen(os.open(self.path, flags, 0o600), 'r+b') as file:
            fcntl.flock(file, fcntl.LOCK_EX)
            meta = os.fstat(file.fileno())
            need(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.geteuid()
                 and meta.st_nlink == 1 and meta.st_mode & 0o077 == 0
                 and meta.st_size <= 4096, 'budget file metadata')
            rows = [json.loads(line) for line in file.read().splitlines()]
            need(all(set(row) == {'segment', 'reserved'}
                     and row['segment'] in ('primary', 'continue', 'cancel')
                     and type(row['reserved']) is int and row['reserved'] == 4
                     for row in rows), 'invalid budget ledger')
            labels = [row['segment'] for row in rows]
            need(len(labels) == len(set(labels)) and label not in labels,
                 'segment already reserved; no automatic retry')
            total = sum(row['reserved'] for row in rows) + 4
            need(total <= CAP, 'model request budget exhausted')
            file.write((json.dumps({'segment': label, 'reserved': 4}) + '\n').encode())
            file.flush()
            os.fsync(file.fileno())
            return total


def auth_path():
    home = Path.home()
    xdg = os.environ.get('XDG_STATE_HOME')
    legacy = (Path(xdg) if xdg else home / '.local/state') / 'oh-my-harness'
    return legacy if legacy.exists() else home / '.ansa/auth'


def save_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def verify_profile(binary, base, workspace, output, label):
    need((workspace / '.ansa/config.json').read_text() == json.dumps(CONFIG),
         'project configuration changed during acceptance')
    result = command(binary, [*base, '--print-config'], workspace, output, label)
    settings = json.loads(result.stdout)
    need(all(settings.get(key) == value for key, value in CONFIG.items()),
         'effective configuration differs from bounded M9 profile')


def audit_turn(events, turn, workspace, names, account_sha256, cancelled=False):
    rows = [event for event in events if event.get('turn') == turn]
    kinds = [event['kind'] for event in rows]
    expected = ['accepted'] + ['response', 'intent', 'outcome', 'continued'] * len(names) + ['response', 'done']
    if cancelled:
        need(kinds in (['accepted', 'response', 'intent', 'cancel', 'outcome'],
                       ['accepted', 'response', 'intent', 'outcome', 'cancel']),
             'cancel transition order differs')
    else:
        need(kinds == expected, 'durable transition order differs')
    accepted = [event for event in rows if event['kind'] == 'accepted']
    need(len(accepted) == 1 and accepted[0]['mode'] == 'basic'
         and proof.number(accepted[0]['round_limit']) == 2
         and proof.number(accepted[0]['call_limit']) == 4, 'bounded accepted turn required')
    binding = accepted[0]['next']['binding']
    need(binding['provider'] == 'openai-siwc' and binding['account']
         and hashlib.sha256(binding['account'].encode()).hexdigest() == account_sha256
         and binding['requested_model'] == binding['returned_model'] == MODEL,
         'accepted provider/account/model binding')
    responses = [event['response'] for event in rows if event['kind'] == 'response']
    intents = [event['invocation'] for event in rows if event['kind'] == 'intent']
    outcomes = [event['result'] for event in rows if event['kind'] == 'outcome']
    need(len(responses) == len(names) + (0 if cancelled else 1)
         and len(intents) == len(outcomes) == len(names), 'response/operation inventory')
    need(sum(event['kind'] == 'continued' for event in rows) == (0 if cancelled else len(names)),
         'separate tool continuations required')
    need(sum(event['kind'] == 'done' for event in rows) == (0 if cancelled else 1)
         and sum(event['kind'] == 'cancel' for event in rows) == int(cancelled)
         and not any(event['kind'] in ('pause', 'resume', 'compacted') for event in rows),
         'unexpected terminal/retry/compact transition')
    need(protocol.structural(responses[0]['request']) == protocol.structural(accepted[0]['next']),
         'first provider request differs from durable acceptance')
    revisions = {item['name']: item['revision']
                 for item in accepted[0]['next']['tools']['descriptors']}
    for index, response in enumerate(responses):
        need(response['model'] == MODEL and response['request']['binding'] == binding,
             'response model/binding differs')
        if index:
            previous = responses[index - 1]
            expected_input = previous['request']['input'] + previous['items'] + [{
                'type': 'function_call_output', 'call_id': outcomes[index - 1]['call'],
                'output': outcomes[index - 1]['output']}]
            expected_request = dict(previous['request'], input=expected_input)
            need(protocol.structural(response['request']) == protocol.structural(expected_request),
                 'production continuation changed epoch/tools/native items/results')
        calls = [item for item in response['items'] if item['type'] == 'function_call']
        if index == len(names):
            need(not calls, 'final response unexpectedly calls tools')
            continue
        invocation, outcome = intents[index], outcomes[index]
        need(len(calls) == 1 and calls[0]['name'] == invocation['name'] == names[index]
             and calls[0]['call_id'] == invocation['call'] == outcome['call']
             and invocation['operation'] == outcome['operation']
             and invocation['operation'] == f"{accepted[0]['operation_run']}:op:{index + 1}"
             and invocation['revision'] == revisions[names[index]]
             and outcome['dispatched'] is True and proof.number(outcome['effects']) == 1
             and proof.number(outcome['execution']) == 0, 'call/operation/known outcome identity')
        args = json.loads(calls[0]['arguments'])
        need(protocol.structural(args) == protocol.structural(invocation['arguments']),
             'executed arguments differ from admitted call')
        body = json.loads(outcome['output'])
        if names[index] == 'read_file':
            path = workspace / args['path']
            need(path.resolve() == workspace / 'seed.txt' and args['offset'] == 0
                 and body['content'] == MARKER + '\n' and body['truncated'] is False,
                 'read_file did not return the fixture')
        else:
            expected_command = CANCEL_COMMAND if cancelled else PRIMARY_COMMAND
            need(args['argv'] == ['/bin/sh', '-c', expected_command]
                 and (workspace / args['cwd']).resolve() == workspace
                 and args['timeout_ms'] == (30000 if cancelled else 5000)
                 and body['cleanup_complete'] is True and body['timed_out'] is False
                 and body['cancelled'] is cancelled, 'process specification/cleanup differs')
            if not cancelled:
                need(body['exit_code'] == 0 and body['stdout'] == 'M9_COMMAND_OK',
                     'foreground process result differs')
    return responses


def reopen(binary, base, root, workspace, session, state, tools, output, label, marker=False):
    screen = measured.ObservedPty(str(binary), root, Path.home(), workspace, 'live', True,
                                  argv=[*base, '--session', session])
    try:
        screen.wait('basic', timeout=10)
        need(session_id(screen.raw) == session, 'reopen selected a different session')
        if marker:
            screen.wait(MARKER)
        screen.send('/status\r')
        screen.wait(f'{state} tools={tools}')
        screen.send(b'\x04')
        result = screen.finish()
        need(result['exit'] == 0 and result['termios_restored'] and result['flags_restored'],
             'read-only reopen terminal restoration')
        save_json(output / (label + '.json'), {'pty': result, 'trace': screen.trace})
    finally:
        (output / (label + '.raw')).write_bytes(screen.raw)
        screen.close()


def command(binary, args, cwd, output, label, input_data=None, timeout=120):
    begin = time.monotonic()
    result = subprocess.run([str(binary), *args], cwd=cwd, input=input_data,
                            capture_output=True, timeout=timeout)
    need(len(result.stdout) + len(result.stderr) <= 32 * 1024 * 1024,
         'CLI output limit')
    (output / (label + '.stdout')).write_bytes(result.stdout)
    (output / (label + '.stderr')).write_bytes(result.stderr)
    save_json(output / (label + '.json'), {'argv': [str(binary), *args],
              'exit': result.returncode, 'elapsed_seconds': time.monotonic() - begin})
    need(result.returncode == 0, label + ' failed; raw diagnostic retained')
    return result


def session_id(data):
    matches = re.findall(rb'ansa session ([a-zA-Z0-9_-]{1,128}) ', data)
    need(len(matches) == 1, 'one explicit session identity required')
    return matches[0].decode()


def journal(session, output, label):
    need(re.fullmatch(r'[a-zA-Z0-9_-]{1,128}', session), 'invalid session identity')
    directory = Path.home() / '.ansa/sessions' / session
    parts = {}
    for path in sorted(directory.glob('part-*.log')):
        meta = path.lstat()
        need(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.geteuid()
             and meta.st_nlink == 1 and meta.st_size <= 32 * 1024 * 1024,
             'owned acceptance journal required')
        parts[path.name] = path.read_bytes()
    need(parts, 'acceptance journal missing')
    target = output / label
    target.mkdir()
    for name, data in parts.items():
        (target / name).write_bytes(data)
    return parts, session_cases.decode(parts)


def run(args):
    output = args.output.resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'status': 'FAIL', 'model': MODEL, 'model_request_cap': CAP,
              'errors': [], 'checks': {}, 'source': common.source_info()}
    need(not report['source']['worktree_status'], 'clean acceptance checkpoint required')
    root = Path(tempfile.mkdtemp(prefix='ansa-m9-'))
    workspace = root / 'workspace'
    workspace.mkdir(mode=0o700)
    (workspace / '.ansa').mkdir(mode=0o700)
    (workspace / '.ansa/config.json').write_text(json.dumps(CONFIG))
    (workspace / 'seed.txt').write_text(MARKER + '\n')
    budget = Budget(output / 'model-budget.jsonl')
    binary, fallback = args.binary.resolve(strict=True), args.cpp.resolve(strict=True)
    auth = auth_path()
    base = ['--cwd', str(workspace), '--auth-dir', str(auth), '--model', MODEL]
    report.update(workspace=str(workspace), binary=str(binary), cpp=str(fallback),
                  binary_sha256=common.sha(binary), cpp_sha256=common.sha(fallback))
    try:
        report['package_source_proofs'] = {
            language: proof.installed_source(path, language, report['source']['files'],
                                             report['source']['head'])
            for language, path in [('rust', binary), ('cpp', fallback)]}
        verify_profile(binary, base, workspace, output, 'config-primary')
        report['checks']['bounded_configuration'] = True
        # The production program alone opens its current auth store. Its status
        # API returns metadata, never a bearer/refresh token or credential body.
        status = subprocess.run([str(binary), '--auth-status', '--auth-dir', str(auth)],
                                capture_output=True, timeout=30, check=True)
        metadata = json.loads(status.stdout)
        need(metadata['signed_in'] and metadata['plan_enabled']
             and not metadata['refresh_pending'], 'existing authorization unavailable')
        report['account_sha256'] = hashlib.sha256(metadata['account'].encode()).hexdigest()
        command(binary, ['--refresh-auth', '--auth-dir', str(auth)],
                workspace, output, 'explicit-refresh', timeout=90)
        report['checks']['explicit_refresh'] = True

        prompt = ('Работай только в текущем каталоге. Сначала вызови read_file для seed.txt. '
                  'Затем через run_command выполни ровно команду /bin/sh -c '
                  '"cat seed.txt > result.txt; printf M9_COMMAND_OK" в текущем каталоге '
                  'с timeout_ms=5000. Используй instruction_token из начальных инструкций. '
                  'Других инструментов не вызывай. В финале сообщи прочитанную метку '
                  'и M9_COMMAND_OK. Ответ краткий.')
        report['reserved_model_requests'] = budget.reserve('primary')
        first = command(binary, [*base, prompt], workspace, output, 'primary')
        session = session_id(first.stderr)
        need((workspace / 'result.txt').read_text() == MARKER + '\n', 'command effect differs')
        need(MARKER.encode() in first.stdout and b'M9_COMMAND_OK' in first.stdout,
             'final answer did not preserve the observed tool results')
        before, events = journal(session, output, 'primary-journal')
        turn = next(event['turn'] for event in events if event['kind'] == 'accepted')
        primary_responses = audit_turn(events, turn, workspace, ['read_file', 'run_command'],
                                       report['account_sha256'])
        report['checks']['multi_round_tools'] = True

        # Fallback reads the NEWEST store. No credential/journal backup is restored.
        reopen(fallback, base, root, workspace, session, 'completed', 2, output,
               'cpp-reopen', marker=True)
        after, _ = journal(session, output, 'cpp-reopened-journal')
        need(after == before, 'read-only C++ fallback changed the journal')
        report['checks']['cpp_latest_state_reopen'] = True

        verify_profile(binary, base, workspace, output, 'config-continue')
        report['reserved_model_requests'] = budget.reserve('continue')
        second = command(binary, [*base, '--continue',
                         'Без инструментов: какую метку ты прочитал в предыдущей задаче? '
                         'Ответь только самой меткой.'], workspace, output, 'continue')
        need(session_id(second.stderr) == session and MARKER.encode() in second.stdout,
             'restart/continue lost the selected session or history')
        _, events = journal(session, output, 'continued-journal')
        need(sum(event['kind'] == 'done' for event in events) == 2
             and sum(event['kind'] == 'intent' for event in events) == 2,
             'continue repeated tools or missed durable completion')
        report['checks']['native_replay_continue'] = True
        second_turn = [event for event in events if event['kind'] == 'accepted'][-1]
        audit_turn(events, second_turn['turn'], workspace, [], report['account_sha256'])
        need(protocol.structural(second_turn['next']['input'][:-1]) == protocol.structural(
            primary_responses[-1]['request']['input'] + primary_responses[-1]['items']),
            'restart changed native history before the new user item')
        reasoning = [item for event in events if event['kind'] == 'response'
                     for item in event['response']['items'] if item['type'] == 'reasoning']
        report['encrypted_reasoning'] = 'observed' if any(
            item.get('encrypted_content') for item in reasoning) else 'not_observed'

        verify_profile(binary, base, workspace, output, 'config-cancel')
        report['reserved_model_requests'] = budget.reserve('cancel')
        screen = measured.ObservedPty(str(binary), root, Path.home(), workspace, 'live',
                                      True, argv=base)
        pidfds = []
        try:
            screen.wait('basic', timeout=10)
            screen.send(('Вызови только run_command: argv=["/bin/sh","-c",'
                         '"printf ready > cancel_started; echo $$ > cancel_pid; '
                         'sleep 20; printf done > cancel_finished"], '
                         'cwd текущий, timeout_ms=30000, instruction_token из инструкций. '
                         'Другие инструменты не вызывай.\r'))
            deadline = time.monotonic() + 90
            while not (workspace / 'cancel_pid').is_file() or not (workspace / 'cancel_pid').stat().st_size:
                need(time.monotonic() < deadline and screen.child.poll() is None,
                     'live cancellation command did not start')
                screen.drain(.02)
            pid = int((workspace / 'cancel_pid').read_text())
            pidfds, ancestry = measured.pin_ancestors(pid, screen.child)
            before_exited = [measured.exited(fd) for fd in pidfds]
            need(not any(before_exited), 'primary already exited')
            cancel_at = time.monotonic()
            at = len(screen.raw)
            screen.signal(signal.SIGINT)
            screen.wait('отменено', timeout=10)
            deadline = time.monotonic() + 5
            while not all(measured.exited(fd) for fd in pidfds):
                need(time.monotonic() < deadline and screen.child.poll() is None,
                     'cancel cleanup before harness EOF deadline')
                screen.drain(.01)
            save_json(output / 'cancel-process.json', {'ancestry': ancestry,
                      'popen_pid': screen.child.pid, 'parent_pid': os.getpid(),
                      'before_exited': before_exited,
                      'after_exited': [measured.exited(fd) for fd in pidfds],
                      'cancel_ns': screen.trace['signals'][-1]['at_ns'],
                      'ack_ns': measured.seen(screen.trace, screen.raw, 'отменено', at),
                      'cleanup_ns': time.perf_counter_ns()})
            screen.send(b'\x04')
            result = screen.finish()
            report['cancel_seconds'] = time.monotonic() - cancel_at
            save_json(output / 'cancel-pty.json', result)
            need(result['exit'] in (0, 130) and result['termios_restored']
                 and result['flags_restored'], 'cancel did not restore the terminal')
            need(not (workspace / 'cancel_finished').exists(), 'cancelled command ran its tail')
            cancelled_session = session_id(screen.raw)
            save_json(output / 'cancel-trace.json', screen.trace)
        finally:
            (output / 'cancel-pty.raw').write_bytes(screen.raw)
            screen.close()
            for fd in pidfds:
                os.close(fd)
        cancel_parts, events = journal(cancelled_session, output, 'cancelled-journal')
        cancelled_turn = next(event['turn'] for event in events if event['kind'] == 'accepted')
        audit_turn(events, cancelled_turn, workspace, ['run_command'], report['account_sha256'], cancelled=True)
        reopen(binary, base, root, workspace, cancelled_session, 'cancelled', 1, output,
               'cancelled-reopen')
        reopened, _ = journal(cancelled_session, output, 'cancelled-reopened-journal')
        need(reopened == cancel_parts, 'cancelled restart dispatched or appended')
        report['checks']['targeted_cancel_known_outcome_restart'] = True
        report['status'] = 'PASS'
    except (OSError, ValueError, AssertionError, KeyError, subprocess.SubprocessError) as error:
        report['errors'].append(str(error))
    save_json(output / 'm9-live-report.json', report)
    print(json.dumps({'status': report['status'], 'checks': report['checks'],
                      'reserved_model_requests': report.get('reserved_model_requests', 0),
                      'errors': report['errors']}))
    return int(report['status'] != 'PASS')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--cpp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--approved-live', action='store_true', required=True)
    raise SystemExit(run(parser.parse_args()))
