"""M2 Responses corpus, independent output oracle and raw-evidence checker.

This is deliberately a small stdlib harness.  It drives the two native JSONL
adapters and never substitutes a Python implementation for either decoder or
replay builder.
"""
from dataclasses import dataclass
from contextlib import ExitStack
from datetime import datetime, timezone
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import random
import re
import stat
import subprocess
import tempfile

import run as common
import protocol_network

FIXTURES = common.ROOT / 'tests/fixtures'
MANIFEST = common.ROOT / 'tests/parity/fixtures/protocol.json'


@dataclass(frozen=True)
class NumberToken:
    lexeme: str
    integer: bool


def _pairs(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError('duplicate decoded JSON key')
        out[key] = value
    return out


def strict_loads(data):
    return json.loads(data, parse_int=lambda s: NumberToken(s, True),
                      parse_float=lambda s: NumberToken(s, False),
                      parse_constant=lambda s: (_ for _ in ()).throw(ValueError('nonfinite JSON number')),
                      object_pairs_hook=_pairs)


def structural(value):
    """Object order is structural; array order and numeric spellings are exact."""
    if isinstance(value, NumberToken):
        return ('number-token', value.integer, value.lexeme)
    if isinstance(value, int) and not isinstance(value, bool):
        return ('number-token', True, str(value))
    if isinstance(value, float):
        return ('number-token', False, repr(value))
    if isinstance(value, dict):
        return ('object', tuple((k, structural(value[k])) for k in sorted(value)))
    if isinstance(value, list):
        return ('array', tuple(structural(v) for v in value))
    return (type(value).__name__, value)


def _quote(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':'))


def exact_dumps(value):
    """Serialize mutation trees while preserving number lexemes from fixtures."""
    if isinstance(value, NumberToken):
        return value.lexeme
    if isinstance(value, int) and not isinstance(value, bool):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if value is None:
        return 'null'
    if value is True:
        return 'true'
    if value is False:
        return 'false'
    if isinstance(value, str):
        return _quote(value)
    if isinstance(value, list):
        return '[' + ','.join(exact_dumps(v) for v in value) + ']'
    if isinstance(value, dict):
        return '{' + ','.join(_quote(k) + ':' + exact_dumps(value[k]) for k in value) + '}'
    raise TypeError('unsupported JSON value in corpus')


def manifest():
    value = strict_loads(MANIFEST.read_bytes())
    if value.get('schema_version') != NumberToken('1', True) or value.get('suite') != 'protocol':
        raise ValueError('unsupported protocol corpus manifest')
    if [r['id'] for r in value.get('required_groups', [])] != [
            'dependencies', 'sse', 'reducer', 'replay', 'tools', 'transport', 'contracts']:
        raise ValueError('required protocol suite inventory differs')
    cpp_source = (common.ROOT / 'tests/protocol_tests.cpp').read_text()
    rust_source = (common.ROOT / 'rust/crates/ansa/tests/protocol_parity.rs').read_text()
    limits_source = (common.ROOT / 'include/omh/json.hpp').read_text()
    limit_expressions = {
        'line': '1024 * 1024', 'event': '1024 * 1024', 'body': '16 * 1024 * 1024',
        'arguments': '256 * 1024', 'items': '64', 'calls': '16', 'events': '65536',
        'json_depth': '64', 'json_nodes': '131072',
    }
    if structural(value.get('default_limits')) != structural({
            'line': 1024 * 1024, 'event': 1024 * 1024, 'body': 16 * 1024 * 1024,
            'arguments': 256 * 1024, 'items': 64, 'calls': 16, 'events': 65536,
            'json_depth': 64, 'json_nodes': 131072}):
        raise ValueError('manifest protocol parser default limits differ')
    for key, expression in limit_expressions.items():
        if 'std::size_t ' + key + ' = ' + expression + ';' not in limits_source:
            raise ValueError('C++ default parser limit source differs: ' + key)
    for group in value['required_groups']:
        cpp_name = group.get('cpp_test')
        rust_names = group.get('rust_tests', [])
        if not cpp_name or ('void ' + cpp_name + '(') not in cpp_source:
            raise ValueError('C++ native test mapping missing: ' + str(group.get('id')))
        if not rust_names or any(('fn ' + name + '(') not in rust_source for name in rust_names):
            raise ValueError('Rust native test mapping missing: ' + str(group.get('id')))
    baseline = value.get('baseline', {})
    suite_counts = [(row['id'], row['runtime_assertions']) for row in baseline.get('suites', [])]
    if suite_counts != [('dependencies', NumberToken('38', True)), ('sse', NumberToken('2290', True)),
                        ('reducer', NumberToken('202', True)), ('replay', NumberToken('37', True)),
                        ('tools', NumberToken('35', True)), ('transport', NumberToken('50', True)),
                        ('contracts', NumberToken('7', True))] or baseline.get('total_runtime_assertions') != NumberToken('2659', True):
        raise ValueError('C++ protocol baseline inventory differs')
    for anchor in value.get('native_assertion_anchors', []):
        if anchor.get('cpp') not in cpp_source or anchor.get('rust') not in rust_source:
            raise ValueError('native assertion source anchor missing: ' + str(anchor.get('case_id')))
    for name, digest in value.get('fixtures', {}).items():
        path = FIXTURES / name
        if path.is_symlink() or not path.resolve().is_relative_to(FIXTURES.resolve()):
            raise ValueError('protocol fixture path escapes fixture root')
        if common.sha(path) != digest:
            raise ValueError('protocol fixture digest differs: ' + name)
    return value


def fixture(name):
    return (FIXTURES / name).read_bytes().decode('utf-8')


def parse_events(body):
    # The committed fixtures and generated mutations use one JSON data field per event.
    events = []
    for event in body.split('\n\n'):
        data = [line[6:] for line in event.splitlines() if line.startswith('data: ')]
        if data:
            events.append(strict_loads('\n'.join(data)))
    return events


def encode_events(events, renumber=True, newline='\n'):
    encoded = []
    for i, original in enumerate(events):
        event = dict(original)
        if renumber:
            event['sequence_number'] = NumberToken(str(i), True)
        encoded.append('data: ' + exact_dumps(event) + newline + newline)
    return ''.join(encoded)


def replace(value, key, replacement):
    result = dict(value)
    result[key] = replacement
    return result


def _items_and_calls(events):
    terminal = events[-1]
    response = terminal['response']
    items = response.get('output', [])
    calls = [{'id': item['id'], 'call_id': item['call_id'], 'name': item['name'],
              'arguments': item['arguments']} for item in items if item.get('type') == 'function_call']
    return items, calls


def _two_calls(events):
    events = [dict(e) for e in events]
    added_item = dict(events[3]['item'])
    added_item.update(id='fc_b', call_id='call_b', arguments='')
    call = dict(events[7]['item'])
    call.update(id='fc_b', call_id='call_b')
    terminal = events.pop()
    events.extend([
        {'type': 'response.output_item.added', 'output_index': NumberToken('2', True),
         'item': added_item},
        {'type': 'response.output_item.done', 'output_index': NumberToken('2', True), 'item': call},
    ])
    output = list(terminal['response']['output']) + [call]
    terminal = dict(terminal)
    terminal['response'] = replace(terminal['response'], 'output', output)
    events.append(terminal)
    for i, event in enumerate(events):
        if 'sequence_number' in event:
            event['sequence_number'] = NumberToken(str(i), True)
    return events


def _case(case_id, body, accepted, batch=True, expected_count=None, **fields):
    declared_executor_count = fields.pop('executor_count', None)
    declared_calls = fields.pop('expected_calls', None)
    if expected_count is None:
        expected_count = declared_executor_count
    if expected_count is None:
        expected_count = declared_calls if accepted and batch else 0
    request = {'id': case_id, 'status': fields.pop('status', 200),
               'mime': fields.pop('mime', 'text/event-stream'), 'body': body}
    request.update(fields.pop('request', {}))
    expected = {'accepted': accepted, 'batch_accepted': batch if accepted else None,
                'executor_count': expected_count}
    expected.update(fields)
    return {'id': case_id, 'request': request, 'expected': expected}


def _json_case(case_id, payload=None, *, payload_hex=None, other=None, index_key=None,
               lifetime_probe=False, limits=None, expected_dump=None, expected_sha=None,
               expected_equal=None, expected_index=None, expected_lifetime=None,
               accepted=True, diagnostic=None, error_code=None):
    request = {'id': case_id, 'op': 'json'}
    if payload is not None:
        request['payload'] = payload
    if payload_hex is not None:
        request['payload_hex'] = payload_hex
    if other is not None:
        request['other'] = other
    if index_key is not None:
        request['index_key'] = index_key
    if lifetime_probe:
        request['lifetime_probe'] = True
    if limits is not None:
        request['limits'] = limits
    expected = {'accepted': accepted, 'batch_accepted': None, 'executor_count': 0}
    if accepted:
        if expected_dump is None:
            expected_dump = payload
        if expected_dump is None:
            raise ValueError('positive JSON operation requires independent expected dump')
        expected.update({'json_dump': expected_dump,
                         'json_sha256': hashlib.sha256(expected_dump.encode('utf-8')).hexdigest()})
        if expected_equal is not None:
            expected['equal'] = expected_equal
        if expected_index is not None:
            expected['index'] = expected_index
        if expected_lifetime is not None:
            expected['lifetime_text'] = expected_lifetime
    else:
        if diagnostic is None:
            diagnostic = 'json_invalid'
        if error_code is None:
            error_code = NumberToken('0', True)
        expected['error'] = {'schema_version': NumberToken('1', True), 'code': error_code,
                             'stage': 'json', 'diagnostic': diagnostic}
    return {'id': case_id, 'request': request, 'expected': expected}


def json_cases():
    rows = []
    invalid = [
        ('empty', '', 'json_invalid'), ('trailing-data', '{}junk', 'json_invalid'),
        ('malformed', '{broken}', 'json_invalid'), ('nan', '[NaN]', 'json_invalid'),
        ('infinity', '[Infinity]', 'json_invalid'), ('negative-infinity', '[-Infinity]', 'json_invalid'),
        ('float-overflow', '[1e9999]', 'json_invalid'), ('duplicate-key', '{"x":1,"x":2}', 'json_invalid'),
        ('decoded-duplicate-key', '{"a":1,"\\u0061":2}', 'json_invalid'),
        ('nested-duplicate-key', '{"a":{"x":1,"x":1}}', 'json_invalid'),
        ('lone-surrogate', '{"x":"\\ud800"}', 'json_invalid'), ('trailing-comma', '[1,]', 'json_invalid'),
        ('comment', '/*comment*/{}', 'json_invalid'),
    ]
    for name, payload, diagnostic in invalid:
        rows.append(_json_case('dependencies-json-' + name, payload, accepted=False, diagnostic=diagnostic))
    rows.append(_json_case('dependencies-json-invalid-utf8', payload_hex=b'{"x":"\xc0\xaf"}'.hex(),
                           accepted=False, diagnostic='json_invalid'))
    index_inputs = [
        ('true', 'true', 'json_index'), ('false', 'false', 'json_index'),
        ('negative', '-1', 'json_index'), ('decimal', '1.0', 'json_index'),
        ('exponent', '1e0', 'json_index'),
        ('overflow', '18446744073709551616', 'json_index'),
        ('null', 'null', 'json_index'), ('string', '"0"', 'json_index'),
    ]
    for name, value, diagnostic in index_inputs:
        rows.append(_json_case('dependencies-index-invalid-' + name, '{"i":' + value + '}',
                               index_key='i', accepted=False, diagnostic=diagnostic))
    rows.extend([
        _json_case('dependencies-index-zero', '{"i":0}', index_key='i', expected_dump='{"i":0}', expected_index=0),
        _json_case('dependencies-object-semantic-equality', '{"b":2,"a":1}', other='{"a":1,"b":2}',
                   expected_dump='{"b":2,"a":1}', expected_equal=True),
        _json_case('dependencies-precise-number-lexemes',
                   '{"integer":123456789012345678901234567890,"decimal":0.123456789012345678901234567890}',
                   expected_dump='{"integer":123456789012345678901234567890,"decimal":0.123456789012345678901234567890}'),
        _json_case('dependencies-binary-nul-key', '{"x":1,"x\\u0000y":2}', index_key='x\x00y',
                   expected_dump='{"x":1,"x\\u0000y":2}', expected_index=2),
        _json_case('dependencies-owning-string-lifetime', '{"a":[{"text":"λ"}],"opaque":"a\\u0000b"}',
                   expected_dump='{"a":[{"text":"λ"}],"opaque":"a\\u0000b"}', lifetime_probe=True,
                   expected_lifetime='λ'),
        _json_case('dependencies-limit-bytes', '{}', limits={'bytes': 1}, accepted=False,
                   diagnostic='json_invalid'),
        _json_case('dependencies-limit-depth', '[[[[]]]]', limits={'bytes': 64, 'depth': 3}, accepted=False,
                   diagnostic='json_invalid'),
        _json_case('dependencies-limit-nodes', '[1,2,3]', limits={'bytes': 64, 'depth': 64, 'nodes': 2}, accepted=False,
                   diagnostic='json_invalid'),
    ])
    expanded_payload = '{"s":"' + ('a' * (16 * 1024 * 1024)) + '"}'
    rows.append(_json_case('dependencies-limit-bytes-custom-above-default', expanded_payload,
                           limits={'bytes': 17 * 1024 * 1024, 'depth': 64, 'nodes': 4},
                           expected_dump=expanded_payload))
    return rows


def validate_unique_ids(rows):
    ids = [row['id'] for row in rows]
    if len(ids) != len(set(ids)):
        raise ValueError('protocol corpus generated duplicate case IDs')
    return rows


def cases(data, mode):
    """Build explicit positive and negative rows from the checked-in inventory."""
    one = fixture('round1.sse')
    two = fixture('round2.sse')
    original = parse_events(one)
    text_events = parse_events(two)
    rows = json_cases()

    # Positives independently anchor terminal state, native items, tool calls and replay.
    rows.append(_case('fixture-round1', one, True, expected_calls=1,
                      expected_items=_items_and_calls(original)[0], expected_calls_value=_items_and_calls(original)[1],
                      replay='round1'))
    rows.append(_case('fixture-round2', two, True, expected_calls=0,
                      expected_items=_items_and_calls(text_events)[0], expected_calls_value=[], replay='none'))
    two_call_body = encode_events(_two_calls(original))
    rows.append(_case('fixture-two-calls', two_call_body, True, expected_calls=2,
                      expected_items=_items_and_calls(_two_calls(original))[0],
                      expected_calls_value=_items_and_calls(_two_calls(original))[1], replay='multi'))
    rows.append(_case('fixture-mime-casefold', one, True, expected_calls=1,
                      mime='Text/Event-Stream; charset=UTF-8', expected_items=_items_and_calls(original)[0],
                      expected_calls_value=_items_and_calls(original)[1], replay='round1'))
    rows.append(_case('fixture-mime-quoted-charset', one, True, expected_calls=1,
                      mime='text/event-stream; charset="utf-8"', expected_items=_items_and_calls(original)[0],
                      expected_calls_value=_items_and_calls(original)[1], replay='round1'))
    long_events = [dict(event) for event in original]
    long_opaque = 'x' * (70 * 1024)
    for index in (1, 2):
        long_item = replace(long_events[index]['item'], 'encrypted_content', long_opaque)
        long_events[index] = replace(long_events[index], 'item', long_item)
    terminal = long_events[-1]['response']
    long_output = [replace(item, 'encrypted_content', long_opaque) if item.get('type') == 'reasoning' else item
                   for item in terminal['output']]
    long_events[-1] = replace(long_events[-1], 'response', replace(terminal, 'output', long_output))
    long_body = encode_events(long_events)
    long_items, long_calls = _items_and_calls(long_events)
    rows.append(_case('reducer-default-line-limit-above-64k', long_body, True, expected_calls=1,
                      expected_items=long_items, expected_calls_value=long_calls, replay='round1'))
    metadata_event = {'type': 'response.metadata', 'sequence_number': NumberToken('1', True),
                      'response_id': original[0]['response']['id'],
                      'metadata': {'openai_chatgpt_moderation_metadata': {}}}
    with_metadata = [dict(e) for e in original]
    with_metadata.insert(1, metadata_event)
    rows.append(_case('reducer-siwc-metadata-preserved', encode_events(with_metadata), True,
                      expected_calls=1, expected_items=_items_and_calls(with_metadata)[0],
                      expected_calls_value=_items_and_calls(with_metadata)[1]))

    # The whole SSE suite is represented by executable partition rows, not counters.
    sse_body = (b'\xef\xbb\xbf: comment\r\ndata: {"type":"response.created","sequence_number":0,'
                b'"response":{"id":"p","model":"gpt-6.1-sol","status":"in_progress","output":[]}}'
                b'\r\n\r\ndata: {"type":"response.completed","sequence_number":1,'
                b'"response":{"id":"p","model":"gpt-6.1-sol","status":"completed","output":[]}}\r\n\r\n')
    sse_text = sse_body.decode('utf-8')
    for split in range(len(sse_body) + 1):
        rows.append(_case('sse-crlf-split-%04d' % split, sse_text, True, expected_calls=0,
                          request={'chunks_hex': [sse_body[:split].hex(), sse_body[split:].hex()]},
                          expected_items=[], expected_calls_value=[]))
    multiline_created = ('{"type":"response.created",\n"sequence_number":0,\n'
                         '"response":{"id":"multi","model":"gpt-6.1-sol",\n'
                         '"status":"in_progress","output":[]}}')
    multiline_terminal = ('{"type":"response.completed","sequence_number":1,\n'
                          '"response":{"id":"multi","model":"gpt-6.1-sol",\n'
                          '"status":"completed","output":[]}}')
    multiline_body = ('\ufeff: comment\nunknown: ignored\nid: 1\nretry: 10\n'
                      'event\nid\nretry\n\nevent: discarded\n\n' +
                      'data: ' + multiline_created.replace('\n', '\ndata: ') + '\n\n' +
                      'data: ' + multiline_terminal.replace('\n', '\ndata: ') + '\n\n')
    rows.append(_case('sse-multiline-fields-reset', multiline_body, True, expected_calls=0,
                      expected_items=[], expected_calls_value=[]))
    bare_fields = ('event\nid\nretry\n\nevent: discarded\n\n' +
                   'data: ' + multiline_created.replace('\n', '\ndata: ') + '\n\n' +
                   'data: ' + multiline_terminal.replace('\n', '\ndata: ') + '\n\n')
    rows.append(_case('sse-bare-fields-reset', bare_fields, True, expected_calls=0,
                      expected_items=[], expected_calls_value=[]))
    round1_raw = one.encode('utf-8')
    for split in range(len(round1_raw) + 1):
        rows.append(_case('sse-round1-production-split-%04d' % split, one, True,
                          expected_calls=1, request={'chunks_hex': [round1_raw[:split].hex(), round1_raw[split:].hex()]},
                          expected_items=_items_and_calls(original)[0], expected_calls_value=_items_and_calls(original)[1]))
    # Each newline form checks BOM, multi-data framing/reset and every byte boundary.
    for newline_id, nl in (('lf', '\n'), ('cr', '\r'), ('crlf', '\r\n')):
        # The committed round2 fixture exercises multibyte text under all line endings.
        bytes_body = '\ufeff: split comment' + nl + 'event: discarded' + nl + nl + two.replace('\n', nl)
        raw = bytes_body.encode()
        for split in range(len(raw) + 1):
            rows.append(_case('sse-%s-round2-split-%04d' % (newline_id, split), bytes_body, True,
                              expected_calls=0, request={'chunks_hex': [raw[:split].hex(), raw[split:].hex()]},
                              expected_items=_items_and_calls(text_events)[0], expected_calls_value=[]))
        rows.append(_case('sse-%s-round2-bytewise' % newline_id, bytes_body, True, expected_calls=0,
                          request={'chunks_hex': [bytes([b]).hex() for b in raw]},
                          expected_items=_items_and_calls(text_events)[0], expected_calls_value=[]))
    rng = random.Random(0x0A11CE)
    raw = one.encode()
    for partition in range(32):
        chunks = []
        offset = 0
        while offset < len(raw):
            size = min(len(raw) - offset, 1 + rng.randrange(47))
            chunks.append(raw[offset:offset + size].hex())
            offset += size
        rows.append(_case('sse-seeded-partition-%04d' % partition, one, True, expected_calls=1,
                          request={'chunks_hex': chunks}, expected_items=_items_and_calls(original)[0],
                          expected_calls_value=_items_and_calls(original)[1]))

    def invalid(case_id, mutate, source=original, renumber=True, **extra):
        es = [dict(e) for e in source]
        mutate(es)
        return _case(case_id, encode_events(es, renumber=renumber), False, **extra)

    # Actual reducer mutation inventory, including every parameterized family in protocol_tests.cpp.
    reducers = [
        ('orphan-argument-item', lambda e: e.__setitem__(4, replace(e[4], 'item_id', 'orphan'))),
        ('argument-output-index', lambda e: e.__setitem__(4, replace(e[4], 'output_index', NumberToken('0', True)))),
        ('argument-call-identity', lambda e: e.__setitem__(4, replace(e[4], 'call_id', 'other'))),
        ('argument-done-name', lambda e: e.__setitem__(6, replace(e[6], 'name', 'other'))),
        ('added-done-item-id', lambda e: e.__setitem__(7, replace(e[7], 'item', replace(e[7]['item'], 'id', 'other')))),
        ('added-done-call-id', lambda e: e.__setitem__(7, replace(e[7], 'item', replace(e[7]['item'], 'call_id', 'other')))),
        ('added-done-namespace', lambda e: e.__setitem__(7, replace(e[7], 'item', replace(e[7]['item'], 'namespace', 'other')))),
        ('orphan-done-index', lambda e: e.__setitem__(7, replace(e[7], 'output_index', NumberToken('4', True)))),
        ('argument-delta-final-conflict', lambda e: e.__setitem__(4, replace(e[4], 'delta', '[]'))),
        ('argument-done-item-conflict', lambda e: e.__setitem__(6, replace(e[6], 'arguments', '{"x":1}'))),
        ('conflicting-duplicate-args-done', lambda e: e.insert(7, replace(e[6], 'arguments', '{"x":1}'))),
        ('duplicate-identical-args-done', lambda e: e.insert(7, e[6])),
        ('delta-after-args-done', lambda e: e.insert(7, e[4])),
        ('delta-after-item-done', lambda e: e.insert(8, e[4])),
        ('wrong-terminal-response-id', lambda e: e.__setitem__(-1, replace(e[-1], 'response', replace(e[-1]['response'], 'id', 'wrong')))),
        ('wrong-event-response-id', lambda e: e.__setitem__(4, replace(e[4], 'response_id', 'wrong'))),
        ('wrong-created-model', lambda e: e.__setitem__(0, replace(e[0], 'response', replace(e[0]['response'], 'model', 'gpt-other')))),
        ('wrong-returned-model', lambda e: e.__setitem__(-1, replace(e[-1], 'response', replace(e[-1]['response'], 'model', 'gpt-other')))),
        ('model-prefix-spoof', lambda e: e.__setitem__(0, replace(e[0], 'response', replace(e[0]['response'], 'model', 'gpt-6.1-sol-spoof')))),
        ('duplicate-added', lambda e: e.insert(4, e[3])),
        ('duplicate-item-id', lambda e: e.__setitem__(3, replace(e[3], 'item', replace(e[3]['item'], 'id', 'rs_a')))),
        ('duplicate-item-done', lambda e: e.insert(8, e[7])),
        ('duplicate-terminal', lambda e: e.append(e[-1])),
        ('duplicate-created', lambda e: e.insert(1, e[0])),
        ('event-before-created', lambda e: e.pop(0)),
        ('unknown-event', lambda e: e.__setitem__(4, replace(e[4], 'type', 'response.unknown_execution.delta'))),
        ('unknown-tool', lambda e: e.__setitem__(3, replace(e[3], 'item', replace(e[3]['item'], 'name', 'bash')))),
        ('missing-namespace', lambda e: e.__setitem__(3, replace(e[3], 'item', {k:v for k,v in e[3]['item'].items() if k!='namespace'}))),
        ('missing-call-id', lambda e: e.__setitem__(3, replace(e[3], 'item', {k:v for k,v in e[3]['item'].items() if k!='call_id'}))),
        ('empty-call-id', lambda e: e.__setitem__(3, replace(e[3], 'item', replace(e[3]['item'], 'call_id', '')))),
        ('missing-opaque-reasoning', lambda e: e.__setitem__(2, replace(e[2], 'item', {k:v for k,v in e[2]['item'].items() if k!='encrypted_content'}))),
        ('empty-opaque-reasoning', lambda e: e.__setitem__(2, replace(e[2], 'item', replace(e[2]['item'], 'encrypted_content', '')))),
        ('opaque-added-done-conflict', lambda e: e.__setitem__(1, replace(e[1], 'item', replace(e[1]['item'], 'encrypted_content', 'other')))),
        ('terminal-native-order', lambda e: e.__setitem__(-1, replace(e[-1], 'response', replace(e[-1]['response'], 'output', list(reversed(e[-1]['response']['output'])))))),
        ('missing-done-item', lambda e: e.pop(7)),
        ('noncompleted-function', lambda e: e.__setitem__(7, replace(e[7], 'item', replace(e[7]['item'], 'status', 'in_progress')))),
        ('terminal-bad-status', lambda e: e.__setitem__(-1, replace(e[-1], 'response', replace(e[-1]['response'], 'status', 'incomplete')))),
        ('completed-with-error', lambda e: e.__setitem__(-1, replace(e[-1], 'response', replace(e[-1]['response'], 'error', {})))),
        ('completed-incomplete-details', lambda e: e.__setitem__(-1, replace(e[-1], 'response', replace(e[-1]['response'], 'incomplete_details', {})))),
        ('response-failed', lambda e: e.__setitem__(-1, replace(e[-1], 'type', 'response.failed'))),
        ('response-incomplete', lambda e: e.__setitem__(-1, replace(e[-1], 'type', 'response.incomplete'))),
        ('error-terminal', lambda e: e.__setitem__(-1, replace(e[-1], 'type', 'error'))),
        ('eof-before-terminal', lambda e: e.pop()),
    ]
    for case_id, mutate in reducers:
        rows.append(invalid('reducer-' + case_id, mutate))
    for case_id, mutate in (
            ('metadata-wrong-response-id', lambda e: e.__setitem__(1, replace(e[1], 'response_id', 'wrong'))),
            ('metadata-missing-response-id', lambda e: e.__setitem__(1, {k:v for k,v in e[1].items() if k!='response_id'})),
            ('metadata-wrong-shape', lambda e: e.__setitem__(1, replace(e[1], 'metadata', []))),
            ('metadata-failed-terminal', lambda e: e.__setitem__(-1, replace(e[-1], 'type', 'response.failed')))):
        rows.append(invalid('reducer-' + case_id, mutate, source=with_metadata))
    for token in ('true', 'false', '-1', '1.0', '1e0', '18446744073709551616', 'null', '"0"'):
        rows.append(_case('dependencies-invalid-index-' + hashlib.sha256(token.encode()).hexdigest()[:8],
                          one.replace('"output_index":1', '"output_index":' + token, 1), False))
    for token in ('true', 'false', '1.0', '-1', '1e0', '18446744073709551616', '"1"'):
        es = [dict(e) for e in original]
        es[3] = replace(es[3], 'output_index', strict_loads(token))
        rows.append(_case('reducer-invalid-index-' + hashlib.sha256(token.encode()).hexdigest()[:8], encode_events(es), False))
    duplicate_sequence = [dict(e) for e in original]
    duplicate_sequence[4] = replace(duplicate_sequence[4], 'sequence_number', NumberToken('3', True))
    rows.append(_case('reducer-duplicate-sequence', encode_events(duplicate_sequence, renumber=False), False))
    boolean_sequence = [dict(e) for e in original]
    boolean_sequence[4] = replace(boolean_sequence[4], 'sequence_number', True)
    rows.append(_case('reducer-boolean-sequence', encode_events(boolean_sequence, renumber=False), False))
    for kind in ('custom_tool_call', 'local_shell_call', 'computer_call', 'tool_search_call', 'future_execute'):
        rows.append(invalid('reducer-unknown-executable-' + kind,
                            lambda e, kind=kind: e.__setitem__(3, replace(e[3], 'item', replace(e[3]['item'], 'type', kind)))))
    for token in ('NaN', 'Infinity', '-Infinity', '1e9999'):
        malformed = one.replace('"sequence_number":0', '"metadata":{"n":' + token + '},"sequence_number":0', 1)
        rows.append(_case('reducer-nonfinite-' + token, malformed, False))
    for index, body in enumerate((b'data: unclosed', b'data: no boundary\n', b'event: unfinished\n',
                                  b'data: x\r', b'\xef\xbb', b'\xf0\x9f')):
        rows.append(_case('sse-partial-eof-%02d' % index, '', False,
                          request={'body_hex': body.hex()}))
    for index, bad in enumerate((b'\xc0\xaf', b'\xed\xa0\x80', b'\xf4\x90\x80\x80',
                                 b'\x80', b'\xe2\x28\xa1', b'\xf5\x80\x80\x80')):
        rows.append(_case('sse-invalid-utf8-%02d' % index, '', False,
                          request={'body_hex': (b': ' + bad + b'\n\n' + one.encode()).hex()}))
    for case_id, body in (
            ('malformed-json', 'data: {broken}\n\n'),
            ('duplicate-json-field', 'data: {"type":"response.created","type":"response.completed"}\n\n'),
            ('invalid-utf8', None), ('sse-event-mismatch', 'event: response.failed\n' + one),
            ('trailing-partial-after-terminal', one + 'data: {unfinished'),
            ('terminal-without-blank-line', one[:-1]), ('event-after-completed', one + 'data: {}\n\n'),
            ('done-sentinel', one + 'data: [DONE]\n\n')):
        request = {'body_hex': '3a20c0af0a0a' + one.encode().hex()} if body is None else {}
        rows.append(_case('reducer-' + case_id, body or '', False, request=request))
    multi = _two_calls(original)
    duplicate_calls = [dict(e) for e in multi]
    duplicate_calls[8] = replace(duplicate_calls[8], 'item', replace(duplicate_calls[8]['item'], 'call_id', 'call_a'))
    rows.append(_case('reducer-duplicate-call-ids', encode_events(duplicate_calls), False))
    final_only = [e for i, e in enumerate(original) if i not in (4, 5, 6)]
    rows.append(_case('reducer-final-only-arguments-accepted', encode_events(final_only), True, expected_calls=1,
                      expected_items=_items_and_calls(final_only)[0], expected_calls_value=_items_and_calls(final_only)[1]))
    precise = [dict(e) for e in original]
    provenance = {'fixture': True, 'integer': NumberToken('123456789012345678901234567890', True),
                  'decimal': NumberToken('0.123456789012345678901234567890', False)}
    precise[2] = replace(precise[2], 'item', replace(precise[2]['item'], 'provenance', provenance))
    terminal_output = list(precise[-1]['response']['output'])
    terminal_output[0] = replace(terminal_output[0], 'provenance', provenance)
    precise[-1] = replace(precise[-1], 'response', replace(precise[-1]['response'], 'output', terminal_output))
    precise_body = encode_events(precise)
    rows.append(_case('dependencies-lossless-native-number-lexemes', precise_body, True, expected_calls=1,
                      expected_items=_items_and_calls(precise)[0], expected_calls_value=_items_and_calls(precise)[1],
                      replay='round1'))
    for limit_name, limit in (('arguments-delta', {'arguments': 1}), ('arguments-final', {'arguments': 1}),
                              ('calls-zero', {'calls': 0}), ('items-one', {'items': 1}),
                              ('event-small', {'event': 128}), ('line-small', {'line': 4}),
                              ('events-zero', {'events': 0}), ('depth-small', {'json_depth': 2}),
                              ('nodes-small', {'json_nodes': 1}),
                              ('body-one-short', {'body': len(one.encode()) - 1})):
        body = encode_events(final_only) if limit_name == 'arguments-final' else one
        rows.append(_case('reducer-limit-' + limit_name, body, False, request={'limits': limit}))
    huge_args = '{"payload":"' + ('x' * (256 * 1024)) + '"}'
    oversized = [dict(e) for e in final_only]
    oversized[4] = replace(oversized[4], 'item', replace(oversized[4]['item'], 'arguments', huge_args))
    rows.append(_case('reducer-default-final-argument-limit', encode_events(oversized), False))
    for args in ('true', '[]', '{broken}', '{"x":NaN}', '{"a":1,"a":2}'):
        bad = [dict(e) for e in final_only]
        bad[4] = replace(bad[4], 'item', replace(bad[4]['item'], 'arguments', args))
        rows.append(_case('reducer-invalid-final-arguments-' + hashlib.sha256(args.encode()).hexdigest()[:8], encode_events(bad), False))
    for name, mutate in (
            ('orphan-text-item', lambda e: e.__setitem__(3, replace(e[3], 'item_id', 'orphan'))),
            ('text-delta-done-conflict', lambda e: e.__setitem__(4, replace(e[4], 'text', 'conflicting text'))),
            ('content-part-conflict', lambda e: e.__setitem__(5, replace(e[5], 'part', replace(e[5]['part'], 'text', 'conflicting part')))),
            ('boolean-content-index', lambda e: e.__setitem__(2, replace(e[2], 'content_index', True))),
            ('failed-after-visible-text', lambda e: e.__setitem__(-1, replace(e[-1], 'type', 'response.failed')))):
        rows.append(invalid('reducer-' + name, mutate, source=text_events))
    preview_events = [dict(e) for e in text_events]
    preview_events[3] = replace(preview_events[3], 'delta', 'Привет')
    preview_events[4] = replace(preview_events[4], 'text', 'Привет')
    first_part = replace(preview_events[5]['part'], 'text', 'Привет')
    preview_events[5] = replace(preview_events[5], 'part', first_part)
    refusal = {'type': 'refusal', 'refusal': '!'}
    refusal_added = replace(refusal, 'refusal', '')
    preview_events[6:6] = [
        {'type': 'response.content_part.added', 'item_id': 'msg_a', 'output_index': NumberToken('0', True),
         'content_index': NumberToken('1', True), 'part': refusal_added},
        {'type': 'response.refusal.delta', 'item_id': 'msg_a', 'output_index': NumberToken('0', True),
         'content_index': NumberToken('1', True), 'delta': '!'},
        {'type': 'response.refusal.done', 'item_id': 'msg_a', 'output_index': NumberToken('0', True),
         'content_index': NumberToken('1', True), 'refusal': '!'},
        {'type': 'response.content_part.done', 'item_id': 'msg_a', 'output_index': NumberToken('0', True),
         'content_index': NumberToken('1', True), 'part': refusal},
    ]
    preview_item = replace(preview_events[10]['item'], 'content', [first_part, refusal])
    preview_events[10] = replace(preview_events[10], 'item', preview_item)
    preview_events[11] = replace(preview_events[11], 'response',
                                 replace(preview_events[11]['response'], 'output', [preview_item]))
    preview_body = encode_events(preview_events)
    rows.append(_case('reducer-preview-text-refusal-order', preview_body, True, expected_calls=0,
                      request={'preview': True}, expected_items=_items_and_calls(preview_events)[0],
                      expected_calls_value=[], preview_texts=['Привет', 'Привет!']))
    preview_failed = [dict(e) for e in preview_events]
    preview_failed[-1] = replace(preview_failed[-1], 'type', 'response.failed')
    rows.append(_case('reducer-preview-visible-text-then-failure', encode_events(preview_failed), False,
                      request={'preview': True}, preview_texts=['Привет', 'Привет!']))
    rows.append(_case('reducer-preview-cancel-on-first', preview_body, False, request={
        'preview': True, 'cancel_preview_after_first': True}, preview_texts=['Привет']))
    rows.append(_case('tools-invalid-second-call', encode_events(_two_calls(original)), True, batch=False,
                      expected_calls=2, request={'batch': {'invalid_call_index': 1}},
                      expected_items=_items_and_calls(_two_calls(original))[0], expected_calls_value=_items_and_calls(_two_calls(original))[1]))
    for behavior in ('throw', 'oversized', 'cancelled_unknown', 'invalid_utf8', 'inconsistent'):
        rows.append(_case('tools-' + behavior + '-stops-tail', encode_events(_two_calls(original)), True,
                          batch=True, expected_calls=2, expected_count=1,
                          request={'batch': {'executor_behaviors': [behavior, 'ok']}}, expected_items=_items_and_calls(_two_calls(original))[0],
                          expected_calls_value=_items_and_calls(_two_calls(original))[1], execution_failed=True))
    rows.append(_case('tools-cancel-before-dispatch', one, True, batch=True, expected_calls=1, executor_count=0,
                      request={'batch': {'cancel_before_dispatch': True}}, expected_items=_items_and_calls(original)[0],
                      expected_calls_value=_items_and_calls(original)[1], cancelled=True,
                      cancelled_output='{"error":"cancelled_before_dispatch"}'))

    # Invalid transport head cases still enter the same offline production decoder.
    for case_id, status, mime in (
            ('missing-mime', 200, ''), ('wrong-mime', 200, 'text/html'), ('json-mime', 200, 'application/json'),
            ('empty-mime', 200, ''), ('duplicate-mime', 200, ['text/event-stream', 'text/event-stream']),
            ('comma-mime', 200, 'text/event-stream, text/html'), ('bad-charset', 200, 'text/event-stream; charset=latin1'),
            ('repeated-charset', 200, 'text/event-stream; charset=utf-8; charset=utf-8'),
            ('unterminated-quoted-charset', 200, 'text/event-stream; charset="utf-8'),
            ('redirect', 301, 'text/event-stream'), ('unauthorized', 401, 'text/event-stream'),
            ('rate-limited', 429, 'text/event-stream'), ('server-error', 500, 'text/event-stream')):
        rows.append(_case('transport-' + case_id, one, False, status=status, mime=mime))
    for endpoint_case, path in (
            ('endpoint-host', 'https://other.example/v1/responses'), ('endpoint-scheme', 'http://api.openai.com/v1/responses'),
            ('endpoint-query', 'https://api.openai.com/v1/responses?x=1'), ('endpoint-models', 'https://api.openai.com/v1/models')):
        rows.append({'id': 'transport-' + endpoint_case, 'request': {'id': 'transport-' + endpoint_case, 'op': 'admit_request', 'url': path},
                     'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}})
    for case_id, mime in (('default', 'text/event-stream'), ('casefold', 'Text/Event-Stream; charset=UTF-8'),
                          ('quoted-charset', 'text/event-stream; charset="utf-8"')):
        rows.append({'id': 'transport-request-valid-' + case_id,
                     'request': {'id': 'transport-request-valid-' + case_id, 'op': 'admit_request',
                                 'url': 'https://api.openai.com/v1/responses', 'method': 'POST',
                                 'status': 200, 'mime': mime, 'stream': True, 'store': False},
                     'expected': {'accepted': True, 'batch_accepted': None, 'executor_count': 0}})
    for case_id, extra in (('get-method', {'method': 'GET'}), ('stream-disabled', {'stream': False}),
                           ('store-enabled', {'store': True})):
        row = {'id': 'transport-request-rejected-' + case_id, 'op': 'admit_request',
               'url': 'https://api.openai.com/v1/responses', 'method': 'POST', 'status': 200,
               'mime': 'text/event-stream', 'stream': True, 'store': False}
        row.update(extra)
        rows.append({'id': 'transport-request-rejected-' + case_id, 'request': row,
                     'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}})

    rows.extend([
        {'id': 'network-valid-control', 'request': {'id': 'network-valid-control', 'network_case': 'valid-control'},
         'expected': {'accepted': True, 'batch_accepted': True, 'executor_count': 1,
                      'expected_items': _items_and_calls(original)[0],
                      'expected_calls_value': _items_and_calls(original)[1], 'replay': 'round1'}},
        {'id': 'network-interim-control', 'request': {'id': 'network-interim-control', 'network_case': 'interim-control'},
         'expected': {'accepted': True, 'batch_accepted': True, 'executor_count': 1,
                      'expected_items': _items_and_calls(original)[0],
                      'expected_calls_value': _items_and_calls(original)[1], 'replay': 'round1'}},
        {'id': 'network-duplicate-head-mime',
         'request': {'id': 'network-duplicate-head-mime', 'network_case': 'duplicate-head-mime'},
         'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}},
        {'id': 'network-wrong-mime', 'request': {'id': 'network-wrong-mime', 'network_case': 'wrong-mime'},
         'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}},
        {'id': 'network-repeated-charset', 'request': {'id': 'network-repeated-charset', 'network_case': 'repeated-charset'},
         'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}},
        {'id': 'network-unterminated-charset',
         'request': {'id': 'network-unterminated-charset', 'network_case': 'unterminated-charset'},
         'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}},
        {'id': 'network-truncated-after-terminal',
         'request': {'id': 'network-truncated-after-terminal', 'network_case': 'truncated-after-terminal'},
         'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}},
        {'id': 'network-mime-trailer-after-terminal',
         'request': {'id': 'network-mime-trailer-after-terminal', 'network_case': 'mime-trailer-after-terminal'},
         'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}},
        {'id': 'network-missing-head-mime-with-trailer',
         'request': {'id': 'network-missing-head-mime-with-trailer', 'network_case': 'missing-head-mime-with-trailer'},
         'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0}},
    ])

    preview_events = parse_events(fixture('round2.sse'))
    for suffix, mutate in (
            ('missing-created', lambda events: events.pop(0)),
            ('model', lambda events: events[0]['response'].__setitem__('model', 'wrong-model')),
            ('response-id', lambda events: next(event for event in events
                 if event['type'] == 'response.output_text.delta').__setitem__('response_id', 'wrong-response')),
            ('item-order', lambda events: next(event for event in events
                 if event['type'] == 'response.output_item.added').__setitem__('output_index', NumberToken('1', True)))):
        altered = copy.deepcopy(preview_events)
        mutate(altered)
        name = 'reducer-preview-rejects-invalid-' + suffix
        rows.append({'id': name, 'request': {'id': name, 'status': 200, 'mime': 'text/event-stream',
                                           'body': encode_events(altered), 'preview': True},
                     'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0,
                                  'preview_texts': []}})
    reordered = _two_calls(parse_events(fixture('round1.sse')))
    first_done = next(event for event in reordered if event['type'] == 'response.output_item.done'
                      and event['item']['type'] == 'function_call')
    reordered.remove(first_done)
    reordered.insert(len(reordered) - 1, first_done)
    name = 'reducer-completion-order-follows-native-output'
    items, calls = _items_and_calls(reordered)
    rows.append({'id': name, 'request': {'id': name, 'status': 200, 'mime': 'text/event-stream',
                                       'body': encode_events(reordered)},
                 'expected': {'accepted': True, 'batch_accepted': True, 'executor_count': 2,
                              'expected_items': items, 'expected_calls_value': calls, 'replay': 'multi'}})

    # Independent review regressions: complete the original valid response
    # after each malformed event, so a skipped guard would authorize execution.
    regressions = []
    for suffix, event in (
            ('orphan-arguments-done', {'type': 'response.function_call_arguments.done',
                                      'output_index': NumberToken('999', True), 'arguments': '{}'}),
            ('orphan-arguments-done-with-id', {'type': 'response.function_call_arguments.done',
                      'output_index': NumberToken('999', True), 'item_id': 'fc_a', 'arguments': '{}'})):
        altered = copy.deepcopy(original)
        altered.insert(1, event)
        regressions.append((suffix, altered, []))
    altered = copy.deepcopy(original)
    next(e for e in altered if e['type'] == 'response.function_call_arguments.done').pop('item_id')
    regressions.append(('arguments-done-missing-item-id', altered, []))
    altered = copy.deepcopy(preview_events)
    altered.insert(2, {'type': 'response.function_call_arguments.delta',
                       'output_index': NumberToken('0', True), 'item_id': 'msg_a', 'delta': '{}'})
    regressions.append(('arguments-target-message', altered, []))
    for suffix, anchor in (('arguments-delta-after-arguments-done', 'response.function_call_arguments.done'),
                           ('arguments-delta-after-item-done', 'response.output_item.done')):
        altered = copy.deepcopy(original)
        index = next(i for i, e in enumerate(altered) if e['type'] == anchor
                     and (anchor != 'response.output_item.done' or e['item']['type'] == 'function_call'))
        altered.insert(index + 1, {'type': 'response.function_call_arguments.delta',
                                  'output_index': NumberToken('1', True), 'item_id': 'fc_a', 'delta': ''})
        regressions.append((suffix, altered, []))
    altered = copy.deepcopy(original)
    next(e for e in altered if e['type'] == 'response.output_item.added'
         and e['item']['type'] == 'function_call')['item']['arguments'] = '[1]'
    regressions.append(('added-arguments-conflict', altered, []))
    altered = copy.deepcopy(original)
    altered.insert(len(altered) - 1, {'type': 'response.output_item.added',
        'output_index': NumberToken('2', True),
        'item': {'type': 'reasoning', 'id': 'rs_extra', 'summary': [], 'status': 'in_progress'}})
    regressions.append(('terminal-omits-unfinished-item', altered, []))
    altered = copy.deepcopy(preview_events)
    altered.insert(len(altered) - 2, {'type': 'response.content_part.added',
        'output_index': NumberToken('0', True), 'item_id': 'msg_a', 'content_index': NumberToken('1', True),
        'part': {'type': 'output_text', 'text': ''}})
    regressions.append(('final-omits-unfinished-part', altered, ['OMH_TOOL_OK:OMH_FIXED_VALUE_42']))
    altered = copy.deepcopy(preview_events)
    altered[0]['response']['id'] = altered[-1]['response']['id'] = 'r' * 257
    regressions.append(('response-id-bound-before-preview', altered, []))
    altered = copy.deepcopy(preview_events)
    for index in range(1, 18):
        altered.insert(index + 1, {'type': 'response.output_item.added',
            'output_index': NumberToken(str(index), True), 'item': {'type': 'function_call',
            'id': 'extra_' + str(index), 'call_id': 'call_extra_' + str(index),
            'name': 'get_test_value', 'namespace': 'omh', 'arguments': '', 'status': 'in_progress'}})
    regressions.append(('added-call-limit-before-preview', altered, []))
    for suffix, altered, previews in regressions:
        name = 'reducer-review-' + suffix
        rows.append({'id': name, 'request': {'id': name, 'status': 200, 'mime': 'text/event-stream',
                                           'body': encode_events(altered), 'preview': True},
                     'expected': {'accepted': False, 'batch_accepted': None, 'executor_count': 0,
                                  'preview_texts': previews}})

    if mode == 'fault':
        controls = {'fixture-round1', 'fixture-round2', 'fixture-two-calls',
                    'network-valid-control', 'network-interim-control'}
        rows = [r for r in rows if (r['expected']['accepted'] is False
                                    or r['expected']['batch_accepted'] is False
                                    or r['expected'].get('execution_failed')
                                    or r['id'] in controls)]
    rows = validate_unique_ids(rows)
    all_ids = [row['id'] for row in rows]
    for group in data['required_groups']:
        for prefix in group.get('adapter_case_prefixes', []):
            if not any(case_id.startswith(prefix) for case_id in all_ids):
                raise ValueError('native suite has no mapped adapter evidence: ' + group['id'] + '/' + prefix)
    if mode != 'fault':
        for anchor in data.get('native_assertion_anchors', []):
            if anchor.get('case_id') not in all_ids:
                raise ValueError('native regression has no adapter evidence row: ' + str(anchor.get('case_id')))
    requirements = data.get('case_requirements', {})
    ids = [row['id'] for row in rows]
    if mode == 'compare':
        stable = [case_id for case_id in ids if not case_id.startswith('sse-')]
        if stable != requirements.get('compare_stable_ids'):
            raise ValueError('stable protocol corpus inventory differs from manifest')
        for prefix, count in requirements.get('generated_prefix_counts', {}).items():
            expected = {prefix + ('%04d' % i) for i in range(int(count.lexeme))}
            actual = {case_id for case_id in ids if case_id.startswith(prefix)}
            if actual != expected:
                raise ValueError('generated protocol case inventory differs: ' + prefix)
        exact_generated = requirements.get('generated_exact_ids', [])
        if not set(exact_generated).issubset(ids) or len(ids) != int(requirements.get('compare_total').lexeme):
            raise ValueError('generated protocol case count differs from manifest')
    elif mode == 'fault' and ids != requirements.get('fault_ids'):
        raise ValueError('fault protocol corpus inventory differs from manifest')
    return rows


def binaries(cpp_build, rust_target):
    cpp = Path(cpp_build)
    rust = Path(rust_target)
    if not cpp.is_absolute():
        cpp = common.ROOT / cpp
    if not rust.is_absolute():
        rust = common.ROOT / rust
    return {'cpp': cpp.resolve() / 'ansa_protocol_oracle',
            'rust': rust.resolve() / 'debug/examples/protocol_adapter'}


def binary_info(paths):
    return {name: {'path': str(path), 'sha256': binary_digest(path, name)} for name, path in paths.items()}


def owned_bytes(path, maximum, executable=False):
    path = Path(path)
    if any(part == '.env' or part.startswith('.env.') for part in path.parts):
        raise ValueError('forbidden evidence path')
    if not path.is_absolute() or path != path.resolve() or path.is_symlink():
        raise ValueError('noncanonical evidence path')
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK), 'rb') as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_size > maximum
                or (not executable and info.st_nlink != 1)
                or (executable and not info.st_mode & 0o111)):
            raise ValueError('evidence file type/size differs')
        value = stream.read(maximum + 1)
    if len(value) > maximum:
        raise ValueError('evidence read limit')
    return value


def binary_digest(path, adapter):
    expected = {'cpp': 'ansa_protocol_oracle', 'rust': 'protocol_adapter'}
    if not isinstance(path, (str, Path)) or Path(path).name != expected[adapter]:
        raise ValueError('adapter binary path differs')
    return hashlib.sha256(owned_bytes(path, 256 * 1024 * 1024, executable=True)).hexdigest()


def retained_bytes(directory, item, filename, maximum=64 * 1024 * 1024):
    if (not isinstance(item, dict) or item.get('file') != filename
            or not isinstance(item.get('sha256'), str)
            or re.fullmatch('[0-9a-f]{64}', item['sha256']) is None):
        raise ValueError('raw evidence identity differs')
    value = owned_bytes(directory / filename, maximum)
    if hashlib.sha256(value).hexdigest() != item['sha256']:
        raise ValueError('raw evidence digest differs')
    if 'size' in item:
        size = item['size']
        if isinstance(size, NumberToken) and size.integer:
            size = int(size.lexeme)
        if type(size) is not int or size != len(value):
            raise ValueError('raw evidence size differs')
    return value


def raw(path):
    return {'file': path.name, 'sha256': common.sha(path), 'size': path.stat().st_size}


def outcome_pass(case, actual):
    expected = case['expected']
    if not isinstance(actual, dict) or 'adapter_error' in actual:
        return False
    accepted = expected['accepted']
    if actual.get('accepted') is not accepted:
        return False
    if 'preview_texts' in expected and structural(actual.get('preview_texts')) != structural(expected['preview_texts']):
        return False
    if case['request'].get('op') == 'admit_request':
        return set(actual) == {'accepted'}
    if case['request'].get('op') == 'json':
        if accepted:
            expected_keys = {'accepted', 'dump', 'sha256'}
            for field in ('equal', 'index', 'lifetime_text'):
                if field in expected:
                    expected_keys.add(field)
            if set(actual) != expected_keys:
                return False
            if actual.get('dump') != expected.get('json_dump'):
                return False
            if actual.get('sha256') != expected.get('json_sha256'):
                return False
            for field in ('equal', 'index', 'lifetime_text'):
                if field in expected and structural(actual.get(field)) != structural(expected[field]):
                    return False
            return True
        error = actual.get('error')
        if set(actual) != {'accepted', 'error'} or not isinstance(error, dict):
            return False
        if set(error) != {'schema_version', 'code', 'stage', 'diagnostic'}:
            return False
        return (structural(error.get('schema_version')) == structural(expected['error']['schema_version'])
                and error.get('code') == expected['error']['code']
                and error.get('stage') == 'json'
                and error.get('diagnostic') == expected['error']['diagnostic'])
    count = actual.get('executors')
    if not isinstance(count, NumberToken) or count.lexeme != str(expected['executor_count']):
        return False
    if not accepted:
        return count.lexeme == '0' and 'continuation' not in actual
    if actual.get('batch_accepted') is not expected['batch_accepted']:
        return False
    if 'expected_items' in expected:
        if structural(actual.get('items')) != structural(expected['expected_items']):
            return False
    if 'expected_calls_value' in expected:
        if structural(actual.get('calls')) != structural(expected['expected_calls_value']):
            return False
    if expected['batch_accepted'] is False:
        return count.lexeme == '0' and 'continuation' not in actual
    if expected.get('execution_failed'):
        return (actual.get('execution_failed') is True and count.lexeme == '1'
                and 'continuation' not in actual)
    if structural(actual.get('context_provenance')) != structural(expected_context_provenance()):
        return False
    continuation = actual.get('continuation')
    if not continuation_base_matches(continuation):
        return False
    output = (expected['cancelled_output'] if expected.get('cancelled')
              else '{"ok":true,"value":"OMH_FIXED_VALUE_42"}')
    expected_input = [*expected_initial_request()['input'], *expected['expected_items'],
                      *[{'type': 'function_call_output', 'call_id': call['call_id'], 'output': output}
                        for call in expected['expected_calls_value']]]
    return structural(continuation.get('input')) == structural(expected_input)


def expected_cases(mode, data):
    return [row['id'] for row in cases(data, mode)]


def expected_commands(mode, data):
    del data
    commands = {'protocol-' + mode + '-' + name for name in ('cpp', 'rust')}
    if mode == 'fault':
        commands |= {'rust-replay-mutant-drop', 'rust-replay-mutant-reorder'}
    return commands


def source_hashes():
    return common.source_info()


def command_succeeded(command):
    exit_code = command.get('exit_code') if isinstance(command, dict) else None
    zero = (isinstance(exit_code, NumberToken) and exit_code.integer and exit_code.lexeme == '0') or (type(exit_code) is int and exit_code == 0)
    if 'elapsed_seconds' in command:
        elapsed = command['elapsed_seconds']
        if isinstance(elapsed, NumberToken):
            elapsed = float(elapsed.lexeme)
        if type(elapsed) not in (int, float) or not math.isfinite(elapsed) or not 0 <= elapsed <= 3600:
            return False
    return zero and command.get('timed_out') is False


def runtime_request(case, peers):
    request = dict(case['request'])
    network_case = request.pop('network_case', None)
    if network_case:
        request['network_url'] = peers[network_case].url(network_case)
    return request


def request_matches(actual, expected):
    marker = expected.get('network_case')
    if marker is None:
        return structural(actual) == structural(expected)
    if not isinstance(actual, dict) or set(actual) != {'id', 'network_url'} or actual.get('id') != expected.get('id'):
        return False
    from urllib.parse import urlsplit
    if not isinstance(actual['network_url'], str):
        return False
    try:
        url = urlsplit(actual['network_url'])
        return (url.scheme == 'http' and url.hostname == '127.0.0.1' and url.port is not None
                and 0 < url.port < 65536 and url.path == '/' + marker and not url.query and not url.fragment
                and url.username is None and url.password is None)
    except ValueError:
        return False


def expected_context_provenance():
    schema = strict_loads('{"type":"object","properties":{},"required":[],"additionalProperties":false}')
    descriptor_record = {'name': 'get_test_value',
                         'description': 'Return a fixed test JSON value without side effects',
                         'implementation_revision': '1', 'schema': schema}
    revision = hashlib.sha256(exact_dumps([descriptor_record]).encode()).hexdigest()
    return {
        'provider': 'openai-siwc', 'account': 'offline-fixture',
        'requested_model': 'gpt-6.1-sol', 'returned_model': 'gpt-6.1-sol',
        'epoch': {'id': 'epoch_1', 'instructions': 'Exact fixture instructions',
                  'exact_state': strict_loads('{"constraints":["offline","pure only"],"root":"fixture","revision":1}')},
        'tools': {'revision': revision, 'descriptors': [{
            'name': 'get_test_value', 'description': 'Return a fixed test JSON value without side effects',
            'revision': '1', 'schema': schema,
        }]},
    }


def expected_continuation_tools():
    schema = strict_loads('{"type":"object","properties":{},"required":[],"additionalProperties":false}')
    return [{
        'type': 'namespace', 'name': 'omh', 'description': 'Compiled local tools',
        'tools': [{
            'type': 'function', 'name': 'get_test_value',
            'description': 'Return a fixed test JSON value without side effects',
            'strict': True, 'parameters': schema,
        }],
    }]


def continuation_base_matches(continuation):
    if not isinstance(continuation, dict):
        return False
    if (continuation.get('model') != 'gpt-6.1-sol' or continuation.get('store') is not False
            or continuation.get('stream') is not True
            or continuation.get('instructions') != 'Exact fixture instructions'
            or continuation.get('include') != ['reasoning.encrypted_content']
            or structural(continuation.get('tools')) != structural(expected_continuation_tools())):
        return False
    return set(continuation) == {'model', 'store', 'stream', 'instructions', 'input', 'tools', 'include'}


def expected_initial_request():
    return {'model': 'gpt-6.1-sol', 'store': False, 'stream': True,
            'instructions': 'Exact fixture instructions', 'include': ['reasoning.encrypted_content'],
            'tools': expected_continuation_tools(),
            'input': [{'role': 'developer', 'content': 'fixture'},
                      {'role': 'user', 'content': 'Call omh.get_test_value'}]}


def execute(args, directory, data):
    paths = binaries(args.cpp_build, args.rust_target)
    network_fixture = fixture('round1.sse')
    report = {'schema_version': 1, 'suite': 'protocol', 'mode': args.action, 'status': 'FAIL',
              'created_at_utc': datetime.now(timezone.utc).isoformat(), 'source': source_hashes(),
              'manifest_sha256': common.sha(MANIFEST), 'fixtures': data['fixtures'],
              'platform': common.platform_info(), 'binaries': binary_info(paths), 'commands': {},
              'cases': [], 'network_peers': {}, 'errors': []}
    commands, results, errors = report['commands'], {}, report['errors']
    required = cases(data, args.action)
    with tempfile.TemporaryDirectory(prefix='ansa-m2-protocol-') as cwd:
        per_adapter = {}
        for adapter in ('cpp', 'rust'):
            label = 'protocol-' + args.action + '-' + adapter
            network_scenarios = {row['request']['network_case'] for row in required
                                 if 'network_case' in row['request']}
            with ExitStack() as peer_stack:
                peers = {scenario: peer_stack.enter_context(protocol_network.ProtocolPeer(network_fixture))
                         for scenario in sorted(network_scenarios)}
                requests = [runtime_request(row, peers) for row in required]
                stdin = directory / (label + '.stdin')
                stdin.write_bytes(b''.join((json.dumps(request, ensure_ascii=False,
                                                       separators=(',', ':')) + '\n').encode()
                                           for request in requests))
                result = common.command([paths[adapter]], cwd, directory, label, timeout=300, input_file=stdin)
                result['stdin'] = raw(stdin)
                commands[label] = result
                per_adapter[adapter] = []
                try:
                    if not command_succeeded(result):
                        raise ValueError('adapter failed or timed out; raw streams retained')
                    output = directory / result['stdout']['file']
                    if output.stat().st_size > 64 * 1024 * 1024:
                        raise ValueError('adapter output exceeds suite cap')
                    lines = output.read_bytes().splitlines()
                    if len(lines) != len(required):
                        raise ValueError('adapter output row count differs')
                    per_adapter[adapter] = [strict_loads(line) for line in lines]
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    errors.append(label + ': ' + str(error))
                    per_adapter[adapter] = [None] * len(required)
            try:
                network_requests = [request for request in requests if 'network_url' in request]
                records = [record for peer in peers.values() for record in peer.observations()]
                peer_path = directory / ('protocol-network-' + adapter + '.json')
                protocol_network.evidence(peer_path, records)
                report['network_peers'][adapter] = raw(peer_path)
                protocol_network.validate_observations(records, network_requests, network_fixture)
            except (AssertionError, OSError, ValueError) as error:
                errors.append('network peer ' + adapter + ': ' + str(error))
                report['network_peers'].setdefault(adapter, None)
        if args.action == 'fault':
            cmake = common.which('cmake') if hasattr(common, 'which') else 'cmake'
            for mode in ('drop', 'reorder'):
                label = 'rust-replay-mutant-' + mode
                result = common.command([
                    cmake,
                    '-DSOURCE=' + str(common.ROOT),
                    '-DBINARY=' + str(Path(args.cpp_build).resolve()),
                    '-DMODE=' + mode,
                    '-P', str(common.ROOT / 'tests/rust_mutation.cmake')],
                    cwd, directory, label, timeout=900)
                commands[label] = result
                stdout = (directory / result['stdout']['file']).read_bytes()
                stderr = (directory / result['stderr']['file']).read_bytes()
                expected = ('native item count' if mode == 'drop'
                            else 'native item order and values')
                if (not command_succeeded(result)
                        or ('Killed Rust production replay ' + mode + ' mutant at replay oracle: ' + expected).encode()
                        not in stdout + stderr):
                    errors.append(label + ': production replay mutant was not killed by its mapped assertion')
        for index, case in enumerate(required):
            actuals = {name: per_adapter[name][index] for name in ('cpp', 'rust')}
            passed = all(outcome_pass(case, actuals.get(n)) for n in ('cpp', 'rust'))
            if actuals['cpp'] is not None and actuals['rust'] is not None:
                passed = passed and structural(actuals['cpp']) == structural(actuals['rust'])
            report['cases'].append({'id': case['id'], 'status': 'pass' if passed else 'failed',
                                    'expected': {'accepted': case['expected']['accepted'],
                                                 'executor_count': case['expected']['executor_count']},
                                    'adapters': {name: {'command': 'protocol-' + args.action + '-' + name,
                                                        'line': index}
                                                 for name in ('cpp', 'rust')}})
    if [r['id'] for r in report['cases']] != expected_cases(args.action, data):
        errors.append('protocol case inventory missing, duplicated, reordered or skipped')
    if set(commands) != expected_commands(args.action, data):
        errors.append('protocol command inventory differs')
    errors += [row['id'] + ' failed' for row in report['cases'] if row['status'] != 'pass']
    if report['source'] != source_hashes() or report['binaries'] != binary_info(paths):
        errors.append('source or adapter binary changed during capture')
    report['status'] = 'FAIL' if errors else 'PASS'
    return report


def capture(args):
    directory = None
    created = False
    try:
        if not all((args.cpp_build, args.rust_target, args.output)):
            raise ValueError('protocol suite requires --cpp-build --rust-target --output')
        directory = Path(args.output).resolve()
        directory.mkdir(parents=True, exist_ok=False)
        created = True
        data = manifest()
        report = execute(args, directory, data)
    except (OSError, ValueError, KeyError, TypeError, AssertionError, subprocess.SubprocessError) as error:
        report = {'schema_version': 1, 'suite': 'protocol', 'mode': args.action, 'status': 'FAIL',
                  'errors': [str(error)], 'cases': [], 'commands': {}}
    if created:
        common.write_json(directory / 'protocol-report.json', report)
    print(json.dumps({'suite': 'protocol', 'mode': args.action, 'status': report['status'],
                      'cases': len(report['cases']), 'errors': report['errors']}, indent=2))
    return int(report['status'] != 'PASS')


def check(evidence, fault_evidence=None):
    data = manifest()
    paths = {'compare': Path(evidence).resolve(),
             'fault': Path(fault_evidence).resolve() if fault_evidence else Path(str(evidence) + '-fault')}
    errors = []
    for mode, directory in paths.items():
        try:
            report = strict_loads(owned_bytes(directory / 'protocol-report.json', 32 * 1024 * 1024))
        except (OSError, ValueError, TypeError):
            errors.append(mode + ' report is missing/unsafe/invalid')
            continue
        if report.get('schema_version') != NumberToken('1', True) or report.get('suite') != 'protocol' or report.get('mode') != mode:
            errors.append(mode + ' evidence identity differs')
        if report.get('status') != 'PASS' or report.get('errors'):
            errors.append(mode + ' report claims no valid pass')
        if report.get('manifest_sha256') != common.sha(MANIFEST) or report.get('fixtures') != data['fixtures']:
            errors.append(mode + ' manifest/fixture identity differs')
        cases_expected = cases(data, mode)
        rows = report.get('cases', [])
        if [row.get('id') for row in rows] != [row['id'] for row in cases_expected]:
            errors.append(mode + ' required cases missing, duplicate, reordered or skipped')
        commands = report.get('commands', {})
        if set(commands) != expected_commands(mode, data):
            errors.append(mode + ' required command inventory differs')
        identities = report.get('binaries', {})
        if not isinstance(identities, dict) or set(identities) != {'cpp', 'rust'}:
            errors.append(mode + ' required binary identities missing/different')
            continue
        for adapter, item in identities.items():
            try:
                if binary_digest(item['path'], adapter) != item['sha256']:
                    raise ValueError('binary digest differs')
            except (OSError, ValueError, KeyError, TypeError):
                errors.append(mode + ' adapter binary missing/unsafe/changed: ' + adapter)
        parsed = {}
        network_requests_by_adapter = {}
        for adapter in ('cpp', 'rust'):
            label = 'protocol-' + mode + '-' + adapter
            command = commands.get(label)
            parsed[adapter] = []
            if not isinstance(command, dict):
                continue
            try:
                retained = {stream: retained_bytes(directory, command.get(stream), label + '.' + stream)
                            for stream in ('stdin', 'stdout', 'stderr')}
            except (OSError, ValueError, TypeError):
                errors.append(mode + ' raw evidence unsafe/invalid: ' + label)
                continue
            if not command_succeeded(command):
                errors.append(mode + ' command failed: ' + label)
                continue
            try:
                if command.get('argv') != [identities[adapter].get('path')]:
                    errors.append(mode + ' wrong adapter argv: ' + label)
                input_lines = retained['stdin'].splitlines()
                output_lines = retained['stdout'].splitlines()
                if len(input_lines) != len(cases_expected) or len(output_lines) != len(cases_expected):
                    errors.append(mode + ' retained row inventory differs: ' + label)
                parsed[adapter] = [(strict_loads(i), strict_loads(o)) for i, o in zip(input_lines, output_lines)]
                network_requests_by_adapter[adapter] = [request for request, _ in parsed[adapter]
                                                        if isinstance(request, dict) and 'network_url' in request]
            except (OSError, ValueError, KeyError, TypeError):
                errors.append(mode + ' retained raw JSONL is invalid: ' + label)
                network_requests_by_adapter[adapter] = []
        for index, case in enumerate(cases_expected):
            row = next((r for r in rows if r.get('id') == case['id']), {})
            if row.get('status') != 'pass':
                errors.append(mode + ' case failed/skipped: ' + case['id'])
            for adapter in ('cpp', 'rust'):
                if len(parsed[adapter]) <= index:
                    errors.append(mode + ' output unavailable: ' + case['id'] + '-' + adapter)
                    continue
                request, actual = parsed[adapter][index]
                if not request_matches(request, case['request']):
                    errors.append(mode + ' request differs: ' + case['id'] + '-' + adapter)
                if not outcome_pass(case, actual):
                    errors.append(mode + ' retained output violates independent oracle: ' + case['id'] + '-' + adapter)
            if len(parsed['cpp']) > index and len(parsed['rust']) > index:
                if structural(parsed['cpp'][index][1]) != structural(parsed['rust'][index][1]):
                    errors.append(mode + ' adapter outputs differ: ' + case['id'])
        if report.get('source', {}).get('files') != source_hashes()['files']:
            errors.append(mode + ' source snapshot differs')
        network_peers = report.get('network_peers', {})
        if set(network_peers) != {'cpp', 'rust'}:
            errors.append(mode + ' local peer evidence inventory differs')
        for adapter in ('cpp', 'rust'):
            item = network_peers.get(adapter)
            try:
                records = json.loads(retained_bytes(directory, item, 'protocol-network-' + adapter + '.json'))
                protocol_network.validate_observations(
                    records, network_requests_by_adapter.get(adapter, []), fixture('round1.sse'))
            except (OSError, ValueError, KeyError, TypeError, AssertionError):
                errors.append(mode + ' local peer evidence invalid: ' + adapter)
        if mode == 'fault':
            for mutant, expected in (('drop', 'native item count'),
                                     ('reorder', 'native item order and values')):
                label = 'rust-replay-mutant-' + mutant
                command = commands.get(label)
                if not isinstance(command, dict):
                    errors.append('fault mutation command missing: ' + mutant)
                    continue
                try:
                    output = retained_bytes(directory, command.get('stdout'), label + '.stdout')
                    error = retained_bytes(directory, command.get('stderr'), label + '.stderr')
                    marker = ('Killed Rust production replay ' + mutant + ' mutant at replay oracle: ' + expected).encode()
                    if not command_succeeded(command) or marker not in output + error:
                        errors.append('fault production replay mutant not killed at expected assertion: ' + mutant)
                    argv = command.get('argv', [])
                    expected_source = '-DSOURCE=' + str(common.ROOT)
                    expected_binary = '-DBINARY=' + str(Path(report['binaries']['cpp']['path']).parent)
                    if (not isinstance(argv, list) or not argv or not all(isinstance(arg, str) for arg in argv)
                            or Path(argv[0]).name != 'cmake' or argv[1:] != [
                                expected_source, expected_binary, '-DMODE=' + mutant,
                                '-P', str(common.ROOT / 'tests/rust_mutation.cmake')]):
                        errors.append('fault mutation invocation differs: ' + mutant)
                except (OSError, KeyError, TypeError, ValueError):
                    errors.append('fault mutation evidence unavailable: ' + mutant)
        host = report.get('platform', {})
        if (not isinstance(host, dict) or host.get('system') != 'Linux'
                or host.get('machine') != 'aarch64' or host.get('os_release', {}).get('ID') != 'nixos'):
            errors.append(mode + ' native NixOS/aarch64 evidence required')
    status = 'FAIL' if errors else 'PASS'
    print(json.dumps({'milestone': 'M2', 'status': status, 'errors': errors}, indent=2))
    return int(bool(errors))
