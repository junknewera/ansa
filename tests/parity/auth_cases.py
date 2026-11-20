"""Frozen, synthetic auth vectors and independent safety expectations."""
import base64
import copy
import hashlib
import json
import random
import re
from functools import lru_cache

import protocol

NOW = 1800000000
ISSUER = 'https://auth.openai.com'
NATIVE = ['jwt', 'lifecycle', 'permission', 'refresh-crash', 'pending-cleanup',
          'dispatch-logout', 'logout-login', 'logout-refresh', 'logout-crash',
          'logout-marker', 'process-refresh', 'refresh-scope', 'uncertain-login',
          'held-dispatch', 'committed-login-crash', 'execution-logout']
DEVIATIONS = {
    'jwt-exponent-34-bits': 'ring bounds public exponent to 33 bits; C++ accepts this valid 34-bit key',
    'jwt-exponent-64-bits': 'ring bounds public exponent to 33 bits; C++ accepts this valid 64-bit key',
    'jwt-leading-zero-modulus': 'ring requires canonical unsigned RSA components',
    'jwt-leading-zero-exponent': 'ring requires canonical unsigned RSA components',
    'store-ancestor-symlink': 'Rust rejects each symlink ancestor before store access; C++ checks the final directory',
}

@lru_cache(maxsize=1)
def diagnostics():
    paths=['src/auth.cpp','src/auth_crypto.cpp','src/json.cpp','tests/parity/auth_adapter.cpp',
           'rust/crates/ansa/src/protocol/json.rs','rust/crates/ansa-linux/src/auth_store.rs',
           'rust/crates/ansa/examples/auth_adapter.rs','rust/crates/ansa/tests/support/auth_fixture.rs']
    paths += ['rust/crates/ansa/src/auth/'+name+'.rs' for name in
              ('mod','claims','pkce','callback','registration','manager','dispatch','store_v1')]
    return set(code for path in paths for code in re.findall(r'"((?:auth|json|fixture)_[a-z0-9_]+)"',
                                                            (protocol.common.ROOT/path).read_text()))


def compact(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':'), allow_nan=False)


def b64(value):
    return base64.urlsafe_b64encode(value).rstrip(b'=').decode('ascii')


def unb64(value):
    if not isinstance(value, str) or not value or len(value) > 65536:
        raise ValueError('invalid fixture encoding')
    decoded = base64.b64decode(value + '=' * (-len(value) % 4), altchars=b'-_', validate=True)
    if b64(decoded) != value:
        raise ValueError('noncanonical fixture encoding')
    return decoded


def good_claims():
    return {'iss': ISSUER, 'sub': 'fixture_subject', 'aud': 'fixture_client',
            'iat': NOW, 'exp': NOW + 3600, 'nonce': 'nonce'}


def jwt_specs():
    rows = []

    def add(name, accepted=True, changes=None, remove=(), **options):
        claims = good_claims()
        claims.update(changes or {})
        for key in remove:
            claims.pop(key)
        row = {'id': 'jwt-' + name, 'generate': {'op': 'generate', 'claims': claims},
               'expected': {'cpp': accepted, 'rust': accepted}}
        for field in ('bits', 'exponent', 'algorithm', 'header_raw', 'payload_raw'):
            if field in options:
                row['generate'][field] = options.pop(field)
        row.update(options)
        rows.append(row)

    add('valid')
    for field, values in {
        'iss': ['', 'https://attacker.invalid', None], 'sub': ['', 's' * 257, 1],
        'aud': ['other', [], ['other'], ['fixture_client', 'fixture_client'], 1, True, None],
        'azp': ['other', 1], 'nonce': ['other', 1], 'exp': [NOW, NOW - 1, -1, str(NOW + 3600), float(NOW + 3600)],
        'iat': [NOW + 61, -1, str(NOW)], 'nbf': [NOW + 61, -1, None],
    }.items():
        for index, value in enumerate(values):
            add(field + '-invalid-' + str(index), False, {field: value})
    for field in ('iss', 'sub', 'aud', 'iat', 'exp', 'nonce'):
        add('missing-' + field, False, remove=[field])
    add('aud-single-array', changes={'aud': ['fixture_client']})
    add('aud-multiple-with-azp', changes={'aud': ['other', 'fixture_client'], 'azp': 'fixture_client'})
    add('aud-multiple-without-azp', False, {'aud': ['other', 'fixture_client']})
    add('aud-array-limit', changes={'aud': ['fixture_client'] + ['a' + str(i) for i in range(7)], 'azp': 'fixture_client'})
    add('aud-array-over-limit', False, {'aud': ['fixture_client'] + ['a' + str(i) for i in range(8)], 'azp': 'fixture_client'})
    add('azp-single-valid', changes={'azp': 'fixture_client'})
    add('iat-skew-boundary', changes={'iat': NOW + 60})
    add('nbf-skew-boundary', changes={'nbf': NOW + 60})
    add('exp-one-second', changes={'exp': NOW + 1})
    add('exp-not-after-iat', False, {'exp': NOW + 1, 'iat': NOW + 1})
    add('refresh-missing-nonce', remove=['nonce'], refresh=True)
    add('refresh-mismatched-nonce', False, {'nonce': 'other'}, refresh=True)
    add('subject-unicode', changes={'sub': 'fixture_λ'})
    add('opaque-number-lexemes', payload_raw=compact(good_claims())[:-1] + ',"opaque":{"decimal":0.123456789012345678901234,"huge":123456789012345678901234567890,"negative_zero":-0}}')
    for field in ('crit', 'jku', 'x5u'):
        header = {'alg': 'RS256', 'kid': 'fixture_key', field: [] if field == 'crit' else 'https://attacker.invalid/key'}
        add('header-' + field, False, header_raw=compact(header))
    for value in ('none', 'HS256', 'RS512', None):
        add('algorithm-' + str(value), False, header_raw=compact({'alg': value, 'kid': 'fixture_key'}))
    for name, header in [('missing-kid', {'alg': 'RS256'}), ('missing-alg', {'kid': 'fixture_key'}),
                         ('empty-kid', {'alg': 'RS256', 'kid': ''}), ('long-kid', {'alg': 'RS256', 'kid': 'k' * 257}),
                         ('unknown-kid', {'alg': 'RS256', 'kid': 'other'}), ('numeric-kid', {'alg': 'RS256', 'kid': 1})]:
        add(name, False, header_raw=compact(header))
    add('duplicate-header-key', False, header_raw='{"alg":"RS256","a\\u006cg":"RS256","kid":"fixture_key"}')
    add('duplicate-claim-key', False, payload_raw=compact(good_claims())[:-1] + ',"s\\u0075b":"fixture_subject"}')
    add('header-not-object', False, header_raw='[]')
    add('claims-not-object', False, payload_raw='[]')
    add('header-byte-limit', False, header_raw=compact({'alg': 'RS256', 'kid': 'fixture_key', 'padding': 'x' * 4096}))
    add('claims-byte-limit', False, changes={'padding': 'x' * 16384})
    add('jwt-byte-limit', False, changes={'padding': 'x' * 32768})
    add('claims-node-limit', changes={'opaque': [0] * 1016})
    add('claims-node-over-limit', False, {'opaque': [0] * 1017})
    add('claims-depth-limit', False, payload_raw=compact(good_claims())[:-1] + ',"opaque":' + '[' * 20 + '0' + ']' * 20 + '}')
    for edit, accepted in [('duplicate-key', False), ('keys-65', False), ('keys-64', True), ('empty-jwks', False),
                            ('kty', False), ('key-alg', False), ('key-use', False), ('key-ops', False), ('verify-ops', True),
                            ('missing-n', False), ('missing-e', False), ('empty-n', False), ('even-e', False),
                            ('one-e', False), ('large-n', False), ('padded-e', False), ('noncanonical-e', False)]:
        add('jwk-' + edit, accepted, edit=edit)
    for bits in (1024, 2040, 2041, 2047, 2048):
        add('modulus-' + str(bits) + '-bits', bits >= 2048, bits=bits, exact_bits=bits)
    for bits, exponent in [(2, '3'), (33, '4294967297'), (34, '8589934593'), (64, '9223372036854775809')]:
        add('exponent-' + str(bits) + '-bits', exponent=exponent)
        if bits > 33:
            rows[-1]['expected']['rust'] = False
    for edit in ('leading-zero-modulus', 'leading-zero-exponent'):
        add(edit, edit=edit)
        rows[-1]['expected']['rust'] = False
    for edit in ('damaged-signature', 'empty-signature', 'extra-dot', 'malformed-encoding'):
        add(edit, False, token_edit=edit)
    return rows


def validate_generated(spec, fixture):
    """Independent RSA public operation; test-only RFC8017 sections8.2.2/9.2.

    https://www.rfc-editor.org/rfc/rfc8017.html#section-9.2
    This also proves weak/noncanonical policy vectors have real signatures.
    """
    if set(fixture) != {'jwt', 'jwks'} or not isinstance(fixture['jwt'], str):
        raise ValueError('fixture shape')
    parts = fixture['jwt'].split('.')
    if len(parts) != 3:
        raise ValueError('fixture token shape')
    header, payload, signature = map(unb64, parts)
    generator = spec['generate']
    expected_header = generator.get('header_raw', compact({'alg': generator.get('algorithm', 'RS256'), 'kid': 'fixture_key'}))
    expected_payload = generator.get('payload_raw', compact(generator['claims']))
    if header != expected_header.encode() or payload != expected_payload.encode():
        raise ValueError('generator changed signed fixture bytes')
    if (not isinstance(fixture['jwks'],dict) or set(fixture['jwks'])!={'keys'}
            or not isinstance(fixture['jwks']['keys'],list) or len(fixture['jwks']['keys'])!=1):
        raise ValueError('fixture key shape')
    key = fixture['jwks']['keys'][0]
    if (not isinstance(key,dict) or set(key)!={'kty','kid','alg','use','n','e'}
            or any(key.get(field)!=value for field,value in {'kty':'RSA','kid':'fixture_key','alg':'RS256','use':'sig'}.items())):
        raise ValueError('fixture key shape')
    n_bytes, e_bytes = unb64(key['n']), unb64(key['e'])
    if len(n_bytes) > 1024 or len(e_bytes) > 8:
        raise ValueError('fixture RSA bounds')
    n, e = int.from_bytes(n_bytes), int.from_bytes(e_bytes)
    size = (n.bit_length() + 7) // 8
    if size < 62 or len(signature) != size or int.from_bytes(signature) >= n or e != int(generator.get('exponent', '65537')):
        raise ValueError('fixture RSA identity')
    if 'exact_bits' in spec and n.bit_length() != spec['exact_bits']:
        raise ValueError('fixture modulus bit length')
    digest_info = bytes.fromhex('3031300d060960864801650304020105000420') + hashlib.sha256('.'.join(parts[:2]).encode()).digest()
    expected = b'\x00\x01' + b'\xff' * (size - len(digest_info) - 3) + b'\x00' + digest_info
    if pow(int.from_bytes(signature), e, n).to_bytes(size) != expected:
        raise ValueError('fixture signature is invalid')


def materialize(spec, fixture):
    validate_generated(spec, fixture)
    request = {'op': 'jwt', 'jwt': fixture['jwt'], 'jwks': copy.deepcopy(fixture['jwks']),
               'client': 'fixture_client', 'nonce': 'nonce', 'now': NOW, 'refresh': spec.get('refresh', False)}
    key = request['jwks']['keys'][0]
    edit = spec.get('edit')
    if edit == 'duplicate-key': request['jwks']['keys'] *= 2
    elif edit == 'keys-65': request['jwks']['keys'] *= 65
    elif edit == 'keys-64': request['jwks']['keys'] = [{'kid': 'other-' + str(i)} for i in range(63)] + [key]
    elif edit == 'empty-jwks': request['jwks']['keys'] = []
    elif edit == 'kty': key['kty'] = 'EC'
    elif edit == 'key-alg': key['alg'] = 'RS512'
    elif edit == 'key-use': key['use'] = 'enc'
    elif edit == 'key-ops': key['key_ops'] = ['sign']
    elif edit == 'verify-ops': key['key_ops'] = ['verify']
    elif edit in ('missing-n', 'missing-e'): key.pop(edit[-1])
    elif edit == 'empty-n': key['n'] = ''
    elif edit == 'even-e': key['e'] = b64(b'\x02')
    elif edit == 'one-e': key['e'] = b64(b'\x01')
    elif edit == 'large-n': key['n'] = b64(b'\x80' + b'\x01' * 1024)
    elif edit == 'padded-e': key['e'] += '='
    elif edit == 'noncanonical-e': key['e'] = 'AB'
    elif edit == 'leading-zero-modulus': key['n'] = b64(b'\x00' + unb64(key['n']))
    elif edit == 'leading-zero-exponent': key['e'] = b64(b'\x00' + unb64(key['e']))
    elif edit is not None: raise ValueError('unknown JWK mutation')
    token_edit = spec.get('token_edit')
    parts = request['jwt'].split('.')
    if token_edit == 'damaged-signature':
        damaged = bytearray(unb64(parts[2])); damaged[0] ^= 1; parts[2] = b64(damaged); request['jwt'] = '.'.join(parts)
    elif token_edit == 'empty-signature': request['jwt'] = '.'.join(parts[:2]) + '.'
    elif token_edit == 'extra-dot': request['jwt'] += '.extra'
    elif token_edit == 'malformed-encoding': request['jwt'] = '!.' + '.'.join(parts[1:])
    expected = {'accepted': dict(spec['expected'])}
    if any(spec['expected'].values()):
        claims = protocol.strict_loads(unb64(fixture['jwt'].split('.')[1]))
        expected.update(subject=claims['sub'], claims_sha256=hashlib.sha256(protocol.exact_dumps(claims).encode()).hexdigest())
    return {'id': spec['id'], 'request': request, 'expected': expected}


def simple_cases():
    rows = []

    def add(name, request, accepted=True, **fields):
        rows.append({'id': name, 'request': request, 'expected': {'accepted': {'cpp': accepted, 'rust': accepted}, **fields}})

    for verifier in ('dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk', '', 'λ' * 16, 'x' * 128):
        add('pkce-' + str(len(rows)), {'op': 'pkce', 'verifier': verifier}, challenge=b64(hashlib.sha256(verifier.encode()).digest()))
    for fields in ([{'key': 'scope', 'value': 'openid profile'}], [{'key': 'a b~', 'value': 'λ/+=\u0000'}], [],
                   [{'key': 'grant_type', 'value': 'refresh_token'}, {'key': 'resource', 'value': 'https://api.openai.com/v1'}]):
        from urllib.parse import quote
        encoded = '&'.join(quote(item['key'], safe='~') + '=' + quote(item['value'], safe='~') for item in fields)
        add('form-' + str(len(rows)), {'op': 'form', 'fields': fields}, encoded=encoded)
    randomizer = random.Random(864178)
    for index in range(32):
        payload = bytes(randomizer.randrange(256) for _ in range(index + 1))
        add('base64-valid-' + str(index), {'op': 'base64', 'encoded': b64(payload)}, encoded=b64(payload), bytes=len(payload))
    for index, value in enumerate(('', 'A', 'AB', 'AAB', 'AA=', 'AA==', '+A', '/A', ' A', 'AA\n', '_', 'λ', 'A' * 65537)):
        add('base64-invalid-' + str(index), {'op': 'base64', 'encoded': value}, False)
    discovery = {'issuer': ISSUER, 'authorization_endpoint': ISSUER + '/api/accounts/authorize',
                 'token_endpoint': ISSUER + '/api/accounts/oauth/token', 'jwks_uri': ISSUER + '/.well-known/jwks.json',
                 'revocation_endpoint': ISSUER + '/api/accounts/oauth/revoke'}
    add('discovery-valid', {'op': 'discovery', 'discovery': discovery})
    for field in discovery:
        add('discovery-wrong-' + field, {'op': 'discovery', 'discovery': {**discovery, field: 'https://attacker.invalid/path'}}, False)
        altered = dict(discovery); altered.pop(field)
        add('discovery-missing-' + field, {'op': 'discovery', 'discovery': altered}, False)

    def callback(name, query='state=fixture_state&code=fixture_code', headers='Host: 127.0.0.1:4321\r\n',
                 accepted=True, method='GET', path='/auth/callback', version='HTTP/1.1', state='fixture_state',
                 body='', code='fixture_code', count=2, terminate=True):
        request = f'{method} {path}?{query} {version}\r\n{headers}' + ('\r\n' if terminate else '') + body
        add('callback-' + name, {'op': 'callback', 'request': request, 'state': state, 'port': 4321}, accepted,
            **({'field_count': count, 'code_present': True, 'code_sha256': hashlib.sha256(code.encode()).hexdigest()} if accepted else {}))

    callback('valid')
    callback('lowercase-host', headers='host: 127.0.0.1:4321\r\n')
    callback('host-ows', headers='Host:\t127.0.0.1:4321 \t\r\n')
    callback('wrong-host', headers='Host: attacker.invalid\r\n', accepted=False)
    callback('missing-host', headers='', accepted=False)
    callback('duplicate-host', headers='Host: 127.0.0.1:4321\r\nHost: attacker.invalid\r\n', accepted=False)
    callback('mixed-duplicate-host', headers='Host: 127.0.0.1:4321\r\nhOsT: 127.0.0.1:4321\r\n', accepted=False)
    callback('host-only-in-body', headers='Host: attacker.invalid\r\n', body='Host: 127.0.0.1:4321\r\n', accepted=False)
    callback('folded-host', headers=' Host: 127.0.0.1:4321\r\n', accepted=False)
    callback('wrong-method', method='POST', accepted=False)
    callback('wrong-path', path='/other', accepted=False)
    callback('wrong-version', version='HTTP/1.0', accepted=False)
    callback('unterminated', terminate=False, accepted=False)
    callback('wrong-state', state='other', accepted=False)
    callback('duplicate-state', query='state=fixture_state&%73tate=fixture_state&code=fixture_code', accepted=False)
    callback('missing-code', query='state=fixture_state', accepted=False)
    callback('empty-code', query='state=fixture_state&code=', accepted=False)
    callback('long-code', query='state=fixture_state&code=' + 'x' * 8193, accepted=False)
    callback('access-denied', query='state=fixture_state&error=access_denied', accepted=False)
    callback('unicode-code', query='state=fixture_state&code=%CE%BB', code='λ')
    callback('encoded-key', query='state=fixture_state&co%64e=fixture_code')
    callback('trailing-ampersand', query='state=fixture_state&code=fixture_code&')
    callback('double-trailing-ampersand', query='state=fixture_state&code=fixture_code&&', accepted=False)
    for index, bad in enumerate(('%00', '%1F', '%7F', '%', '%GG', '%+A', '\x7f')):
        callback('encoding-' + str(index), query='state=fixture_state&code=' + bad, accepted=False)
    callback('fields-limit', query='state=fixture_state&code=fixture_code' + ''.join('&a' + str(i) + '=b' for i in range(14)), count=16)
    callback('fields-over-limit', query='state=fixture_state&code=fixture_code' + ''.join('&a' + str(i) + '=b' for i in range(15)), accepted=False)
    callback('header-limit', headers='Host: 127.0.0.1:4321\r\nX: ' + 'x' * 16384 + '\r\n', accepted=False)
    for name in NATIVE:
        rows.append({'id': 'native-auth-' + name, 'request': {'op': 'scenario', 'case': name}, 'expected': {'passed': True, 'case': name}})
    rows.append({'id': 'store-ancestor-symlink', 'request': {'op': 'ancestor'},
                 'expected': {'accepted': {'cpp': True, 'rust': False}, 'zero_posts': True, 'real_path_compatible': True}})
    return rows


def output_pass(case, actual, adapter):
    if not isinstance(actual, dict) or 'adapter_error' in actual:
        return False
    expected = case['expected']
    if 'passed' in expected:
        return protocol.structural(actual) == protocol.structural(expected)
    accepted = expected['accepted'][adapter]
    if actual.get('accepted') is not accepted:
        return False
    if case['request']['op'] == 'ancestor':
        return protocol.structural(actual) == protocol.structural({'accepted': accepted, 'zero_posts': True, 'real_path_compatible': True})
    if not accepted:
        diagnostic = actual.get('diagnostic')
        return set(actual) == {'accepted', 'diagnostic'} and isinstance(diagnostic, str) and diagnostic in diagnostics()
    wanted = {key: value for key, value in expected.items() if key != 'accepted'}
    wanted['accepted'] = True
    return protocol.structural(actual) == protocol.structural(wanted)
