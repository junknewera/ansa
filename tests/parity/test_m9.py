"""Offline controls for the explicitly approved M9 live driver."""
import copy
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from m9_live import (Budget, CAP, CONFIG, MODEL, MARKER, PRIMARY_COMMAND, CANCEL_COMMAND,
                     audit_turn, reopen, session_id, verify_profile)
from performance_evidence import CPP_PACKAGE_SELECTOR

ACCOUNT_HASH = hashlib.sha256(b'synthetic').hexdigest()

def turn_fixture(workspace, cancelled=False):
    binding = {'provider': 'openai-siwc', 'account': 'synthetic',
               'requested_model': MODEL, 'returned_model': MODEL}
    request = {'binding': binding, 'input': [{'role': 'user', 'content': 'fixture'}],
               'epoch': {'id': 'fixture', 'instructions': 'fixture', 'exact_state': {}},
               'tools': {'revision': 'fixture', 'descriptors': [
                   {'name': 'read_file', 'revision': 'ansa_local_v1'},
                   {'name': 'run_command', 'revision': 'ansa_process_v1'}]}}
    rows = [{'kind': 'accepted', 'turn': 't1', 'mode': 'basic',
             'round_limit': 2, 'call_limit': 4, 'operation_run': 'fixture',
             'next': copy.deepcopy(request)}]
    names = ['run_command'] if cancelled else ['read_file', 'run_command']
    for index, name in enumerate(names):
        if name == 'read_file':
            args = {'path': 'seed.txt', 'offset': 0, 'max_bytes': 8192}
            body = {'content': MARKER + '\n', 'truncated': False}
        else:
            args = {'argv': ['/bin/sh', '-c', CANCEL_COMMAND if cancelled else PRIMARY_COMMAND],
                    'cwd': str(workspace), 'timeout_ms': 30000 if cancelled else 5000}
            body = {'cleanup_complete': True, 'timed_out': False, 'cancelled': cancelled,
                    'exit_code': -1 if cancelled else 0, 'stdout': 'M9_COMMAND_OK'}
        item = {'type': 'function_call', 'name': name, 'call_id': f'call{index}',
                'arguments': json.dumps(args)}
        items = [{'type': 'reasoning', 'encrypted_content': 'synthetic-only'}, item]
        response = {'request': copy.deepcopy(request), 'model': MODEL, 'items': items}
        invocation = {'name': name, 'arguments': args, 'call': item['call_id'],
                      'operation': f'fixture:op:{index + 1}',
                      'revision': 'ansa_local_v1' if name == 'read_file' else 'ansa_process_v1'}
        result = {'operation': invocation['operation'], 'call': invocation['call'],
                  'dispatched': True, 'execution': 0, 'effects': 1,
                  'output': json.dumps(body)}
        rows.extend([{'kind': 'response', 'turn': 't1', 'response': response},
                     {'kind': 'intent', 'turn': 't1', 'invocation': invocation},
                     {'kind': 'outcome', 'turn': 't1', 'result': result}])
        if not cancelled:
            rows.append({'kind': 'continued', 'turn': 't1'})
        request['input'] += items + [{'type': 'function_call_output',
                                     'call_id': result['call'], 'output': result['output']}]
    if not cancelled:
        rows.append({'kind': 'response', 'turn': 't1', 'response': {
            'request': request, 'model': MODEL, 'items': [{'type': 'message'}]}})
    rows.append({'kind': 'cancel' if cancelled else 'done', 'turn': 't1'})
    return rows, names


class M9Controls(unittest.TestCase):
    def test_reservations_survive_reopen_and_cannot_be_repeated(self):
        with tempfile.TemporaryDirectory() as name:
            path = Path(name) / 'budget.jsonl'
            self.assertEqual(Budget(path).reserve('primary'), 4)
            before = path.read_bytes()
            with self.assertRaisesRegex(ValueError, 'already reserved'):
                Budget(path).reserve('primary')
            self.assertEqual(path.read_bytes(), before)
            self.assertEqual(Budget(path).reserve('continue'), 8)
            self.assertEqual(Budget(path).reserve('cancel'), CAP)
            with self.assertRaises(ValueError):
                Budget(path).reserve('retry')

    def test_corrupt_or_shared_ledgers_fail_before_reserving(self):
        with tempfile.TemporaryDirectory() as name:
            path = Path(name) / 'budget.jsonl'
            for raw in (b'broken', b'{"segment":"primary","reserved":true}\n',
                        b'{"segment":"primary","reserved":0}\n',
                        (json.dumps({'segment': 'primary', 'reserved': 4}) + '\n').encode() * 2):
                path.write_bytes(raw)
                path.chmod(0o600)
                with self.assertRaises(ValueError):
                    Budget(path).reserve('cancel')
                self.assertEqual(path.read_bytes(), raw)
            path.chmod(0o644)
            with self.assertRaisesRegex(ValueError, 'metadata'):
                Budget(path).reserve('cancel')

    def test_session_identity_is_explicit_and_cannot_escape_catalog(self):
        self.assertEqual(session_id(b'ansa session own_session (full access)'), 'own_session')
        for value in (b'', b'ansa session ../other ', b'ansa session a ansa session b '):
            with self.assertRaises(ValueError):
                session_id(value)

    def test_one_submit_per_segment_has_a_conservative_request_bound(self):
        self.assertEqual(CONFIG['tool_rounds'], 2)
        # Final generation plus at most one pre-Submit summary; no Resume.
        self.assertEqual(3 * (CONFIG['tool_rounds'] + 2), CAP)

    def test_config_change_between_segments_is_rejected_before_submit(self):
        with tempfile.TemporaryDirectory() as name:
            workspace = Path(name)
            (workspace / '.ansa').mkdir()
            config = workspace / '.ansa/config.json'
            config.write_text(json.dumps(CONFIG))
            with patch('m9_live.command', return_value=SimpleNamespace(stdout=json.dumps(CONFIG))):
                verify_profile(None, [], workspace, workspace, 'valid')
            unsafe = dict(CONFIG, tool_rounds=64)
            config.write_text(json.dumps(unsafe))
            with patch('m9_live.command') as process:
                with self.assertRaisesRegex(ValueError, 'configuration changed'):
                    verify_profile(None, [], workspace, workspace, 'modified')
                process.assert_not_called()
            config.write_text(json.dumps(CONFIG))
            with patch('m9_live.command', return_value=SimpleNamespace(stdout=json.dumps(unsafe))):
                with self.assertRaisesRegex(ValueError, 'effective configuration'):
                    verify_profile(None, [], workspace, workspace, 'global-change')

    def test_native_rounds_and_cancel_cannot_hide_corrupted_results(self):
        workspace = Path('/tmp/ansa-m9-test/workspace')
        for cancelled in (False, True):
            rows, names = turn_fixture(workspace, cancelled)
            audit_turn(rows, 't1', workspace, names, ACCOUNT_HASH, cancelled)
            mutations = []
            bad = copy.deepcopy(rows)
            next(event for event in bad if event['kind'] == 'outcome')['result']['operation'] = 'foreign'
            mutations.append(bad)
            bad = copy.deepcopy(rows)
            bad[0]['next']['input'] = [{'role': 'user', 'content': 'foreign'}]
            mutations.append(bad)
            bad = copy.deepcopy(rows)
            next(event for event in bad if event['kind'] == 'response')['response']['request']['epoch']['id'] = 'foreign'
            mutations.append(bad)
            bad = copy.deepcopy(rows)
            for event in bad:
                if event['kind'] == 'accepted':
                    event['next']['binding']['account'] = 'foreign-account'
                if event['kind'] == 'response':
                    event['response']['request']['binding']['account'] = 'foreign-account'
            mutations.append(bad)
            bad = copy.deepcopy(rows)
            next(event for event in bad if event['kind'] == 'intent')['invocation']['operation'] = 'foreign:op:1'
            next(event for event in bad if event['kind'] == 'outcome')['result']['operation'] = 'foreign:op:1'
            mutations.append(bad)
            mutations.append(rows[1:] + rows[:1])
            bad = copy.deepcopy(rows)
            next(event for event in bad if event['kind'] == 'intent')['invocation']['revision'] = 'foreign'
            mutations.append(bad)
            bad = copy.deepcopy(rows)
            bad[0]['round_limit'] = 64
            mutations.append(bad)
            bad = copy.deepcopy(rows)
            outcome = [event['result'] for event in bad if event['kind'] == 'outcome'][-1]
            body = json.loads(outcome['output'])
            body['cleanup_complete'] = False
            outcome['output'] = json.dumps(body)
            mutations.append(bad)
            if not cancelled:
                bad = copy.deepcopy(rows)
                responses = [event for event in bad if event['kind'] == 'response']
                responses[1]['response']['request']['input'].pop(1)
                mutations.append(bad)
                bad = copy.deepcopy(rows)
                responses = [event for event in bad if event['kind'] == 'response']
                responses[0]['response']['items'] += responses[1]['response']['items']
                bad.remove(responses[1])
                mutations.append(bad)
            for bad in mutations:
                with self.subTest(cancelled=cancelled), self.assertRaises(ValueError):
                    audit_turn(bad, 't1', workspace, names, ACCOUNT_HASH, cancelled)

    def test_noop_executable_cannot_pass_reopen(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            with self.assertRaises((ValueError, AssertionError)):
                reopen(Path(shutil.which('true')), [], root, root, 'fixture',
                       'completed', 2, root, 'noop', marker=True)

    def test_cpp_package_selection_preserves_legacy_and_fails_on_broken_override(self):
        for attrs, expected in [('{ default.drvPath = "legacy-cpp"; }', 'legacy-cpp'),
                                ('{ default.drvPath = "rust"; ansa-cpp.drvPath = "cpp"; }', 'cpp')]:
            value = subprocess.check_output(['nix', 'eval', '--raw', '--expr',
                                              f'({CPP_PACKAGE_SELECTOR}) {attrs}'], text=True)
            self.assertEqual(value, expected)
        broken = '{ default.drvPath = "rust"; ansa-cpp = throw "broken-explicit-cpp"; }'
        value = subprocess.run(['nix', 'eval', '--raw', '--expr',
                                f'({CPP_PACKAGE_SELECTOR}) {broken}'], capture_output=True)
        self.assertNotEqual(value.returncode, 0)


if __name__ == '__main__':
    unittest.main()
