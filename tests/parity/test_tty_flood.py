"""Pure controls for the visible Cancel clock and retained TTY flood evidence."""
import copy
import json
import os
from pathlib import Path
import signal
import tempfile
import unittest
from unittest.mock import patch

import session_cases as journal
import tty_flood


def initial(phase='ожидание модели'):
    return b'\x1b[2J\x1b[24;1Hbasic  ' + phase.encode() + b'  0 c'


def suffix():
    # Retain the initial о from ожидание and repaint only тменено.
    return b'\x1b[24;9H\x1b[K' + 'тменено  0 с'.encode()


def whole():
    return b'\x1b[24;1H\x1b[2Kbasic  ' + 'отменено  0 с'.encode()


def observation(chunks):
    raw = b''
    reads = []
    for index, chunk in enumerate(chunks):
        reads.append({'begin': len(raw), 'end': len(raw) + len(chunk),
                      'at_ns': 50 if index == 0 else 100 + index * 100})
        raw += chunk
    sig = {'number': signal.SIGINT, 'offset': len(chunks[0]), 'at_ns': 100}
    return raw, {'reads': reads}, sig


class VisibleCancelTests(unittest.TestCase):
    def test_suffix_and_whole_repaint_have_the_same_visible_ack(self):
        for delta in (suffix(), whole()):
            with self.subTest(delta=delta):
                raw, trace, sig = observation([initial(), delta])
                self.assertEqual(tty_flood.cancel_ack(trace, raw, sig), 200)
                if delta == suffix():
                    self.assertNotIn('отменено'.encode(), raw[sig['offset']:])

    def test_body_draft_and_footer_detail_words_cannot_ack(self):
        for delta in (
            b'\x1b[2;1H' + 'отменено'.encode(),
            b'\x1b[23;1H> ' + 'отменено'.encode(),
            b'\x1b[24;1Hbasic  ' + 'ожидание модели  вывод: отменено'.encode(),
        ):
            with self.subTest(delta=delta):
                raw, trace, sig = observation([initial(), delta])
                with self.assertRaisesRegex(ValueError, 'missing visible Cancel'):
                    tty_flood.cancel_ack(trace, raw, sig)

    def test_split_csi_and_utf8_wait_for_the_complete_visible_phase(self):
        phase = 'отменено'.encode()
        chunks = [initial(), b'\x1b[24;', b'1H\x1b[2Kbasic  ' + phase[:-1], phase[-1:]]
        observed = tty_flood.CancelAck(observation(chunks)[2])
        for end in range(1, len(chunks) + 1):
            raw, trace, _ = observation(chunks[:end])
            observed.update(trace, raw)
            self.assertEqual(observed.ack_ns, 400 if end == len(chunks) else None)

    def test_cancelled_footer_before_signal_is_not_a_new_ack(self):
        for delta in (b'\r', whole()):
            raw, trace, sig = observation([initial('отменено'), delta])
            with self.subTest(delta=delta), self.assertRaisesRegex(ValueError, 'before signal'):
                tty_flood.cancel_ack(trace, raw, sig)

    def test_clock_is_the_first_read_prefix_showing_cancel(self):
        raw, trace, sig = observation([initial(), suffix(), b'\r', whole()])
        self.assertEqual(tty_flood.cancel_ack(trace, raw, sig), trace['reads'][1]['at_ns'])
        # A cancelled state earlier in the session is harmless once the current
        # footer shows the new running turn at this signal's offset.
        before = initial('отменено') + initial()
        raw, trace, sig = observation([before, suffix()])
        self.assertEqual(tty_flood.cancel_ack(trace, raw, sig), 200)

    def test_missing_gapped_reordered_and_misdated_reads_are_rejected(self):
        raw, original, sig = observation([initial(), suffix(), b'\r'])
        mutations = [
            lambda reads: reads.pop(0),
            lambda reads: reads.pop(),
            lambda reads: reads[1].update(begin=reads[1]['begin'] + 1),
            lambda reads: reads[1].update(end=len(raw) + 1),
            lambda reads: reads[1].update(at_ns=99),
            lambda reads: reads[0].update(at_ns=101),
            lambda reads: reads[2].update(at_ns=199),
            lambda reads: reads[1].update(at_ns=True),
            lambda reads: reads.reverse(),
        ]
        for mutate in mutations:
            trace = copy.deepcopy(original)
            mutate(trace['reads'])
            with self.subTest(reads=trace['reads']), self.assertRaisesRegex(ValueError, 'read'):
                tty_flood.cancel_ack(trace, raw, sig)


class FloodEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='ansa-tty-oracle-')
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.binary = self.directory / 'frontend_adapter'
        self.binary.write_bytes(b'synthetic verifier identity\n')
        configuration = journal.configuration()
        accepted = journal.accepted(configuration)
        accepted.update(text='question', request='request_1', turn='turn_1',
                        next=journal.request(configuration, [], 'question'))
        self.events = [{'kind': 'init', 'version': 1, 'session': 'fixture',
                        'configuration': configuration}, accepted,
                       {'kind': 'cancel', 'turn': 'turn_1'}]
        self.report = {'binary': {'path': str(self.binary), 'sha256': tty_flood.common.sha(self.binary)},
                       'runs': []}
        for index in range(tty_flood.REPEATS):
            root = Path(f'/tmp/ansa-m4-oracle-{index}')
            raw, trace, sig = observation([initial(), suffix(), b'\r'])
            path = self.directory / f'tty-flood-{index}.pty'
            path.write_bytes(raw)
            trace.update(argv=[str(self.binary), '--frontend-fixture', str(root), 'cancel'],
                         parent_pid=os.getpid(), popen_pid=os.getpid() + 1,
                         popen_identity=[os.getpid(), 1], exit_code=143,
                         signals=[sig, {'number': signal.SIGTERM, 'offset': len(raw), 'at_ns': 400}])
            self.report['runs'].append({
                'sample': index, 'root': str(root), 'status': 'PASS', 'generated_hex': '78',
                'written_before_cancel': 32768, 'written_at_ack': 36864,
                'ack_ns': 200, 'cancel_seconds': (200 - sig['at_ns']) / 1e9,
                'snapshot': self.snapshot(self.events), 'trace': trace,
                'raw': tty_flood.metrics.artifact(path),
                'terminal': {'exit': 143, 'termios_restored': True,
                             'flags_restored': True, 'stdout_hex': raw.hex()},
            })

    @staticmethod
    def snapshot(events):
        return [{'name_hex': b'part-00000000000000000000.log'.hex(), 'kind': 'file',
                 'bytes_hex': journal.encode(events).hex()}]

    def verify(self, report):
        return tty_flood.verify(report, self.directory, self.binary)

    def test_well_formed_suffix_repaint_evidence_replays(self):
        self.verify(self.report)

    def test_input_must_be_saturated_and_continue_until_ack(self):
        for fields in (
            {'written_before_cancel': 32767},
            {'written_at_ack': 32768},
            {'written_before_cancel': True},
            {'written_at_ack': True},
        ):
            report = copy.deepcopy(self.report)
            report['runs'][0].update(fields)
            with self.subTest(fields=fields), self.assertRaisesRegex(ValueError, 'continuous input'):
                self.verify(report)

    def test_generation_and_canonical_cancel_constraints_remain_required(self):
        report = copy.deepcopy(self.report)
        report['runs'][0]['generated_hex'] = '7878'
        with self.assertRaisesRegex(ValueError, 'generation/dispatch count'):
            self.verify(report)
        for mutation in (
            lambda events: events.append({'kind': 'done', 'turn': 'turn_1'}),
            lambda events: events.pop(),
            lambda events: events[1].update(text='different question'),
            lambda events: events[1].update(request='different_request'),
            lambda events: events[2].update(turn='different_turn'),
        ):
            report = copy.deepcopy(self.report)
            events = copy.deepcopy(self.events)
            mutation(events)
            report['runs'][0]['snapshot'] = self.snapshot(events)
            with self.subTest(events=events), self.assertRaisesRegex(ValueError, 'dispatch after Cancel|actual cancelled turn'):
                self.verify(report)

    def test_ack_cannot_use_a_later_read_or_exceed_one_second(self):
        report = copy.deepcopy(self.report)
        report['runs'][0].update(ack_ns=300, cancel_seconds=(300 - 100) / 1e9)
        with self.assertRaisesRegex(ValueError, 'Cancel acknowledgement'):
            self.verify(report)
        report = copy.deepcopy(self.report)
        row = report['runs'][0]
        for index, read in enumerate(row['trace']['reads'][1:]):
            read['at_ns'] = 1_000_000_100 + index
        row['trace']['signals'][1]['at_ns'] = 1_000_000_200
        row.update(ack_ns=1_000_000_100, cancel_seconds=1.0)
        with self.assertRaisesRegex(ValueError, 'Cancel acknowledgement'):
            self.verify(report)

    def test_failed_capture_keeps_its_read_trace_and_raw_output(self):
        class FailedPty:
            raw = initial()
            trace = {'reads': [{'begin': 0, 'end': len(raw), 'at_ns': 50}], 'signals': []}
            def wait(self, *_):
                raise ValueError('forced initial wait failure')
            def close(self):
                pass
        output = self.directory / 'failed-capture'
        output.mkdir()
        with patch.object(tty_flood.metrics, 'ObservedPty', return_value=FailedPty()), \
             self.assertRaisesRegex(ValueError, 'forced initial wait failure'):
            tty_flood.capture(self.binary, output)
        row = json.loads((output / 'tty-flood-progress.json').read_bytes())['runs'][0]
        self.assertEqual(row['status'], 'FAIL')
        self.assertEqual(row['trace'], FailedPty.trace)
        self.assertEqual((output / row['raw']['path']).read_bytes(), FailedPty.raw)


if __name__ == '__main__':
    unittest.main()
