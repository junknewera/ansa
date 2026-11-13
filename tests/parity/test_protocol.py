"""Adversarial regressions for M2 corpus and independent evidence oracles."""
from pathlib import Path
import hashlib
import json
import copy
import os
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

import protocol

CPP_BUILD = os.environ.get('ANSA_PARITY_CPP_BUILD', 'build-m2')


class ProtocolOracleTests(unittest.TestCase):
    def test_run_cli_dispatches_protocol_compare_and_m2_check(self):
        runner = Path(protocol.common.__file__).resolve()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            capture = subprocess.run(
                [sys.executable, runner, 'compare', '--suite', 'protocol', '--cpp-build', root / 'missing-cpp',
                 '--rust-target', root / 'missing-rust', '--output', root / 'evidence'],
                cwd=protocol.common.ROOT, capture_output=True, text=True, check=False)
            self.assertNotEqual(capture.returncode, 0)
            self.assertIn('"suite": "protocol"', capture.stdout)
            self.assertTrue((root / 'evidence' / 'protocol-report.json').is_file())
            check = subprocess.run([sys.executable, runner, 'check', '--milestone', 'M2',
                                    '--evidence', root / 'missing-evidence'],
                                   cwd=protocol.common.ROOT, capture_output=True, text=True, check=False)
            self.assertNotEqual(check.returncode, 0)
            self.assertNotIn('invalid choice', check.stderr)

    def test_numeric_lexemes_are_tagged_and_not_strings(self):
        self.assertNotEqual(protocol.structural(protocol.strict_loads('1')),
                            protocol.structural(protocol.strict_loads('1.0')))
        self.assertNotEqual(protocol.structural(protocol.strict_loads('1')),
                            protocol.structural(protocol.strict_loads('"1"')))
        self.assertNotEqual(protocol.structural(protocol.strict_loads('-0')),
                            protocol.structural(protocol.strict_loads('0')))
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            protocol.strict_loads('{"x":1,"\\u0078":2}')

    def test_custom_byte_limit_positive_case_exceeds_default_bound(self):
        case = next(row for row in protocol.cases(protocol.manifest(), 'compare')
                    if row['id'] == 'dependencies-limit-bytes-custom-above-default')
        payload = case['request']['payload']
        self.assertGreater(len(payload.encode('utf-8')), 16 * 1024 * 1024)
        self.assertGreater(case['request']['limits']['bytes'], len(payload.encode('utf-8')))
        self.assertEqual(case['expected']['json_dump'], payload)
        self.assertEqual(case['expected']['json_sha256'], hashlib.sha256(payload.encode()).hexdigest())

    def test_default_line_limit_accepts_opaque_data_above_64k(self):
        case = next(row for row in protocol.cases(protocol.manifest(), 'compare')
                    if row['id'] == 'reducer-default-line-limit-above-64k')
        line_sizes = [len(line.encode('utf-8')) for line in case['request']['body'].splitlines()
                      if line.startswith('data: ')]
        default_line_limit = int(protocol.manifest()['default_limits']['line'].lexeme)
        self.assertGreater(max(line_sizes), 64 * 1024)
        self.assertLess(max(line_sizes), default_line_limit)
        self.assertTrue(case['expected']['accepted'])

    def test_generated_inserted_events_follow_native_wire_sequence_contract(self):
        events = protocol._two_calls(protocol.parse_events(protocol.fixture('round1.sse')))
        added = next(row for row in events if row.get('type') == 'response.output_item.added'
                     and row.get('output_index') == protocol.NumberToken('2', True))
        done = next(row for row in events if row.get('type') == 'response.output_item.done'
                    and row.get('output_index') == protocol.NumberToken('2', True))
        self.assertEqual((added['item']['status'], added['item']['arguments']), ('in_progress', ''))
        self.assertEqual((done['item']['status'], done['item']['arguments']), ('completed', '{}'))
        metadata = {'type': 'response.metadata', 'response_id': 'resp_a', 'metadata': {}}
        wire = protocol.encode_events([metadata])
        self.assertEqual(protocol.strict_loads(wire[len('data: '):].strip()).get('sequence_number'),
                         protocol.NumberToken('0', True))

    def test_structural_object_order_is_ignored_but_native_array_order_is_exact(self):
        self.assertEqual(protocol.structural(protocol.strict_loads('{"a":1,"b":2}')),
                         protocol.structural(protocol.strict_loads('{"b":2,"a":1}')))
        self.assertNotEqual(protocol.structural(protocol.strict_loads('[1,2]')),
                            protocol.structural(protocol.strict_loads('[2,1]')))

    def test_all_reject_and_agreeing_native_item_loss_do_not_pass(self):
        data = protocol.manifest()
        positive = next(case for case in protocol.cases(data, 'compare') if case['id'] == 'fixture-round1')
        self.assertFalse(protocol.outcome_pass(positive, {'accepted': False, 'executors': 0}))
        complete = self._valid_fixture_output(positive)
        self.assertTrue(protocol.outcome_pass(positive, complete))
        complete['items'] = complete['items'][1:]
        self.assertFalse(protocol.outcome_pass(positive, complete))

    def test_replay_tools_are_exact_and_cannot_be_silently_changed(self):
        positive = next(case for case in protocol.cases(protocol.manifest(), 'compare')
                        if case['id'] == 'fixture-round1')
        actual = self._valid_fixture_output(positive)
        self.assertTrue(protocol.outcome_pass(positive, actual))
        changed = copy.deepcopy(actual)
        changed['continuation']['tools'][0]['tools'][0]['description'] = 'different tool'
        self.assertFalse(protocol.outcome_pass(positive, changed))
        changed = copy.deepcopy(actual)
        changed['continuation']['previous_response_id'] = 'implicit-state'
        self.assertFalse(protocol.outcome_pass(positive, changed))

    def test_replay_output_type_is_required_even_when_values_match(self):
        for case_id in ('fixture-round1', 'fixture-round2'):
            case = next(row for row in protocol.cases(protocol.manifest(), 'compare') if row['id'] == case_id)
            actual = self._valid_fixture_output(case)
            self.assertTrue(protocol.outcome_pass(case, actual))
            changed = copy.deepcopy(actual)
            changed['continuation']['input'] = changed['continuation']['input'][:2]
            self.assertFalse(protocol.outcome_pass(case, changed))
            if actual['calls']:
                for kind in (None, 'message'):
                    changed = copy.deepcopy(actual)
                    output = changed['continuation']['input'][-1]
                    if kind is None:
                        output.pop('type')
                    else:
                        output['type'] = kind
                    self.assertFalse(protocol.outcome_pass(case, changed))

    def test_exit_code_boolean_and_nonfinite_time_cannot_pass(self):
        self.assertFalse(protocol.command_succeeded({'exit_code': False, 'timed_out': False}))
        self.assertFalse(protocol.command_succeeded({'exit_code': 0, 'timed_out': False,
                                                   'elapsed_seconds': protocol.NumberToken('1e999', False)}))

    def test_checker_rejects_missing_binary_foreign_platform_and_extra_argv(self):
        def missing_binary(mode, report, _root):
            report['binaries'].pop('rust', None)
            report['commands']['protocol-' + mode + '-rust']['argv'] = [None]

        for mutation in (
                missing_binary,
                lambda mode, report, root: report.update(platform={'system': 'Darwin', 'machine': 'x86_64'}),
                lambda mode, report, root: report['commands']['protocol-' + mode + '-cpp']['argv'].append('--extra'),
                lambda mode, report, root: report['commands']['protocol-' + mode + '-cpp'].update(exit_code=False)):
            with tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary) / 'bundle'
                self._write_bundle(directory, self._positive_case, mutate=mutation)
                self.assertNotEqual(self._run_bundle_check(directory), 0)

    def test_raw_reader_rejects_outside_symlink_hardlink_and_oversize(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            payload = directory / 'owned.stdout'
            payload.write_bytes(b'fixture')
            item = protocol.raw(payload)
            self.assertEqual(protocol.retained_bytes(directory, item, payload.name), b'fixture')
            with self.assertRaises(ValueError):
                protocol.retained_bytes(directory, {**item, 'file': str(payload)}, payload.name)
            with self.assertRaises(ValueError):
                protocol.retained_bytes(directory, item, payload.name, maximum=2)
            alias = directory / 'link.stdout'
            alias.symlink_to(payload)
            with self.assertRaises(ValueError):
                protocol.retained_bytes(directory, {**item, 'file': alias.name}, alias.name)
            alias.unlink()
            alias.hardlink_to(payload)
            with self.assertRaises(ValueError):
                protocol.retained_bytes(directory, item, payload.name)

    def test_raw_reader_rejects_fifo_without_waiting_for_writer(self):
        with tempfile.TemporaryDirectory() as temporary:
            fifo = Path(temporary) / 'blocked.stdout'
            os.mkfifo(fifo)
            result = subprocess.run(
                [sys.executable, '-c',
                 'from pathlib import Path; import protocol,sys\n'
                 'try: protocol.owned_bytes(Path(sys.argv[1]), 1024)\n'
                 'except ValueError: sys.exit(0)\n'
                 'sys.exit(1)\n', str(fifo)],
                cwd=Path(protocol.__file__).parent, capture_output=True, timeout=2, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_agreeing_number_lexeme_loss_does_not_pass(self):
        data = protocol.manifest()
        case = next(case for case in protocol.cases(data, 'compare')
                    if case['id'] == 'dependencies-lossless-native-number-lexemes')
        actual = protocol.strict_loads(protocol.exact_dumps(self._valid_fixture_output(case)))
        self.assertTrue(protocol.outcome_pass(case, actual))
        lost = copy.deepcopy(actual)
        lost['items'][0]['provenance']['decimal'] = protocol.NumberToken('0.1234567890123457', False)
        self.assertFalse(protocol.outcome_pass(case, lost))

    def test_negative_requires_zero_executor_and_no_continuation(self):
        data = protocol.manifest()
        case = next(case for case in protocol.cases(data, 'fault') if case['id'] == 'reducer-malformed-json')
        self.assertTrue(protocol.outcome_pass(case, {'accepted': False, 'executors': protocol.NumberToken('0', True)}))
        self.assertFalse(protocol.outcome_pass(case, {'accepted': False, 'executors': protocol.NumberToken('1', True)}))
        self.assertFalse(protocol.outcome_pass(case, {
            'accepted': False, 'executors': protocol.NumberToken('0', True), 'continuation': {'input': []}}))

    def test_duplicate_case_ids_fail_generation(self):
        with self.assertRaisesRegex(ValueError, 'duplicate case IDs'):
            protocol.validate_unique_ids([{'id': 'same'}, {'id': 'same'}])

    def test_capture_does_not_overwrite_existing_evidence_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'evidence'
            path.mkdir()
            marker = path / 'protocol-report.json'
            marker.write_bytes(b'preserve-me')
            args = SimpleNamespace(action='compare', cpp_build='missing-build', rust_target='missing-target', output=path)
            with patch('builtins.print'):
                code = protocol.capture(args)
            self.assertNotEqual(code, 0)
            self.assertEqual(marker.read_bytes(), b'preserve-me')

    @staticmethod
    def _valid_fixture_output(case):
        items = case['expected']['expected_items']
        calls = case['expected']['expected_calls_value']
        input_rows = [
            {'role': 'developer', 'content': 'fixture'},
            {'role': 'user', 'content': 'Call omh.get_test_value'},
            *items,
            *[{'type': 'function_call_output', 'call_id': call['call_id'],
               'output': '{"ok":true,"value":"OMH_FIXED_VALUE_42"}'} for call in calls],
        ]
        value = {'accepted': True, 'batch_accepted': True, 'items': items, 'calls': calls, 'executors': len(calls),
                'context_provenance': protocol.expected_context_provenance(),
                'continuation': {'model': 'gpt-6.1-sol', 'store': False, 'stream': True,
                                 'instructions': 'Exact fixture instructions', 'input': input_rows,
                                 'tools': protocol.expected_continuation_tools(),
                                 'include': ['reasoning.encrypted_content']}}
        return protocol.strict_loads(protocol.exact_dumps(value))

    def _write_bundle(self, directory, case, output_factory=None, mutate=None):
        directory.mkdir()
        paths = protocol.binaries(CPP_BUILD, 'rust/target')
        binaries = protocol.binary_info(paths)
        for mode in ('compare', 'fault'):
            mode_dir = directory / mode
            mode_dir.mkdir()
            commands = {}
            for adapter, binary in paths.items():
                label = 'protocol-' + mode + '-' + adapter
                request_path = mode_dir / (label + '.stdin')
                output_path = mode_dir / (label + '.stdout')
                error_path = mode_dir / (label + '.stderr')
                request_path.write_text(json.dumps(case['request'], separators=(',', ':')) + '\n')
                actual = output_factory(case, adapter) if output_factory else self._valid_fixture_output(case)
                output_path.write_text(protocol.exact_dumps(actual) + '\n')
                error_path.write_bytes(b'')
                commands[label] = {'argv': [str(binary)], 'cwd': '/tmp/protocol-harness-test', 'exit_code': 0,
                                   'timed_out': False, 'stdin': protocol.raw(request_path),
                                   'stdout': protocol.raw(output_path), 'stderr': protocol.raw(error_path)}
            if mode == 'fault':
                for mutant, assertion in (('drop', 'native item count'),
                                          ('reorder', 'native item order and values')):
                    label = 'rust-replay-mutant-' + mutant
                    output_path = mode_dir / (label + '.stdout')
                    error_path = mode_dir / (label + '.stderr')
                    output_path.write_text('Killed Rust production replay ' + mutant +
                                           ' mutant at replay oracle: ' + assertion + '\n')
                    error_path.write_bytes(b'')
                    commands[label] = {
                        'argv': ['cmake', '-DSOURCE=' + str(protocol.common.ROOT),
                                 '-DBINARY=' + str(Path(CPP_BUILD).resolve()), '-DMODE=' + mutant,
                                 '-P', str(protocol.common.ROOT / 'tests/rust_mutation.cmake')],
                        'cwd': '/tmp/protocol-harness-test', 'exit_code': 0, 'timed_out': False,
                        'stdout': protocol.raw(output_path), 'stderr': protocol.raw(error_path)}
            peer_records = {}
            for adapter in ('cpp', 'rust'):
                peer_path = mode_dir / ('protocol-network-' + adapter + '.json')
                peer_path.write_text('[]\n')
                peer_records[adapter] = protocol.raw(peer_path)
            report = {
                'schema_version': 1, 'suite': 'protocol', 'mode': mode, 'status': 'PASS',
                'created_at_utc': 'synthetic', 'source': protocol.source_hashes(),
                'manifest_sha256': protocol.common.sha(protocol.MANIFEST),
                'fixtures': protocol.manifest()['fixtures'], 'platform': protocol.common.platform_info(),
                'binaries': binaries, 'commands': commands,
                'cases': [{'id': case['id'], 'status': 'pass',
                           'expected': {'accepted': True, 'executor_count': 1},
                           'adapters': {name: {'command': 'protocol-' + mode + '-' + name, 'line': 0}
                                        for name in ('cpp', 'rust')}}],
                'network_peers': peer_records, 'errors': [],
            }
            if mutate:
                mutate(mode, report, mode_dir)
            (mode_dir / 'protocol-report.json').write_text(json.dumps(report, ensure_ascii=False))

    def _run_bundle_check(self, directory):
        with patch.object(protocol, 'cases', side_effect=lambda _data, _mode: [self._positive_case]), \
             patch.object(protocol.protocol_network, 'validate_observations', return_value={}), \
             patch('builtins.print'):
            return protocol.check(directory / 'compare', directory / 'fault')

    def setUp(self):
        self._positive_case = next(case for case in protocol.cases(protocol.manifest(), 'compare')
                                   if case['id'] == 'fixture-round1')

    def test_checker_recomputes_raw_results_despite_fake_pass_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / 'bundle'
            self._write_bundle(directory, self._positive_case,
                               output_factory=lambda _case, _adapter: {'accepted': False,
                                   'executors': 0})
            self.assertNotEqual(self._run_bundle_check(directory), 0)

    def test_complete_synthetic_bundle_passes_including_killed_mutants(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / 'bundle'
            self._write_bundle(directory, self._positive_case)
            self.assertEqual(self._run_bundle_check(directory), 0)

    def test_checker_rejects_wrong_request_argv_source_and_raw_digest(self):
        def fake_mutant_pass(mode, report, root):
            if mode == 'fault':
                label = 'rust-replay-mutant-drop'
                output = root / report['commands'][label]['stdout']['file']
                output.write_text('PASS\n')
                report['commands'][label]['stdout'] = protocol.raw(output)

        def wrong_mutant_binary(mode, report, _root):
            if mode == 'fault':
                report['commands']['rust-replay-mutant-reorder']['argv'][1] = '-DSOURCE=/tmp/wrong-tree'

        mutations = (
            lambda mode, report, root: (root / report['commands']['protocol-' + mode + '-cpp']['stdin']['file']).write_text('{}\n'),
            lambda mode, report, root: report['commands']['protocol-' + mode + '-rust']['argv'].__setitem__(0, '/tmp/wrong-adapter'),
            lambda mode, report, root: report['source']['files'].__setitem__('tests/parity/protocol.py', '0' * 64),
            lambda mode, report, root: (root / report['commands']['protocol-' + mode + '-cpp']['stdout']['file']).write_text('{}\n'),
            lambda mode, report, root: report['binaries']['rust'].__setitem__('sha256', '0' * 64),
            lambda mode, report, root: report['cases'].append(dict(report['cases'][0])),
            lambda mode, report, root: report['commands'].pop('protocol-' + mode + '-rust'),
            lambda mode, report, root: report['commands']['protocol-' + mode + '-cpp'].update({'exit_code': 124, 'timed_out': True}),
            fake_mutant_pass,
            wrong_mutant_binary,
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary) / 'bundle'
                self._write_bundle(directory, self._positive_case, mutate=mutation)
                self.assertNotEqual(self._run_bundle_check(directory), 0)

    def test_timeout_capture_keeps_partial_stdout_and_stderr(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            script = root / 'partial.py'
            script.write_text('import sys,time\nsys.stdout.write("partial-out")\nsys.stdout.flush()\n'
                              'sys.stderr.write("partial-error")\nsys.stderr.flush()\ntime.sleep(5)\n')
            result = protocol.common.command([sys.executable, script], root, root, 'partial-timeout', timeout=1)
            self.assertTrue(result['timed_out'])
            self.assertNotEqual(result['exit_code'], 0)
            self.assertEqual((root / result['stdout']['file']).read_bytes(), b'partial-out')
            self.assertEqual((root / result['stderr']['file']).read_bytes(), b'partial-error')


if __name__ == '__main__':
    unittest.main()
