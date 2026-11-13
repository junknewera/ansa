"""Bounded loopback HTTP peer for M2 transfer/EOF admission proofs."""
import hashlib
import json
from pathlib import Path
import socket
import threading
from urllib.parse import urlsplit


NETWORK_CASES = ('valid-control', 'interim-control', 'duplicate-head-mime', 'wrong-mime',
                 'repeated-charset', 'unterminated-charset', 'truncated-after-terminal',
                 'mime-trailer-after-terminal', 'missing-head-mime-with-trailer')


class ProtocolPeer:
    def __init__(self, fixture_body):
        self.body = fixture_body.encode('utf-8')
        self.socket = socket.socket()
        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.socket.bind(('127.0.0.1', 0))
        self.socket.listen(8)
        self.socket.settimeout(0.1)
        self.port = self.socket.getsockname()[1]
        self.stop = threading.Event()
        self.records = []
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def __enter__(self):
        return self

    def __exit__(self, kind, value, traceback):
        self.close()

    def url(self, case_id):
        if case_id not in NETWORK_CASES:
            raise ValueError('unknown loopback protocol scenario')
        return 'http://127.0.0.1:%d/%s' % (self.port, case_id)

    @staticmethod
    def _read_request(connection):
        data = b''
        while b'\r\n\r\n' not in data:
            chunk = connection.recv(4096)
            if not chunk:
                raise EOFError('request ended before headers')
            data += chunk
            if len(data) > 65536:
                raise ValueError('request header limit')
        head, body = data.split(b'\r\n\r\n', 1)
        lines = head.split(b'\r\n')
        method, target, version = lines[0].decode('ascii').split(' ', 2)
        length = 0
        authorization = False
        for line in lines[1:]:
            name, sep, value = line.partition(b':')
            if not sep:
                raise ValueError('malformed request header')
            if name.lower() == b'content-length':
                length = int(value.strip())
            if name.lower() == b'authorization':
                authorization = True
        if not 0 <= length <= 1024 * 1024:
            raise ValueError('request body limit')
        while len(body) < length:
            chunk = connection.recv(length - len(body))
            if not chunk:
                raise EOFError('request body truncated')
            body += chunk
        return method, target, version, head, body[:length], authorization

    def _response(self, case_id):
        return self.expected_wire(case_id, self.body), case_id != 'truncated-after-terminal'

    @staticmethod
    def expected_wire(case_id, body):
        prelude = (b'HTTP/1.1 103 Early Hints\r\nContent-Type: ignored/type\r\n\r\n'
                   if case_id == 'interim-control' else b'')
        mime = {
            'wrong-mime': b'text/html',
            'repeated-charset': b'text/event-stream; charset=utf-8; charset=utf-8',
            'unterminated-charset': b'text/event-stream; charset="utf-8',
        }.get(case_id, b'text/event-stream; charset=utf-8')
        head = b'HTTP/1.1 200 OK\r\nContent-Type: ' + mime + b'\r\n'
        if case_id == 'missing-head-mime-with-trailer':
            head = b'HTTP/1.1 200 OK\r\n'
        if case_id == 'duplicate-head-mime':
            head += b'Content-Type: text/event-stream\r\n'
        head += b'Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n'
        first = body[:len(body) // 2]
        second = body[len(body) // 2:]
        chunks = (('%x\r\n' % len(first)).encode() + first + b'\r\n'
                  + ('%x\r\n' % len(second)).encode() + second + b'\r\n')
        if case_id == 'valid-control':
            return prelude + head + chunks + b'0\r\n\r\n'
        if case_id == 'interim-control':
            return prelude + head + chunks + b'0\r\n\r\n'
        if case_id in ('duplicate-head-mime', 'wrong-mime', 'repeated-charset', 'unterminated-charset'):
            return head + chunks + b'0\r\n\r\n'
        if case_id == 'truncated-after-terminal':
            return head + chunks + b'100\r\nx'
        if case_id == 'mime-trailer-after-terminal':
            return head + chunks + b'0\r\nContent-Type: text/html\r\n\r\n'
        if case_id == 'missing-head-mime-with-trailer':
            return head + chunks + b'0\r\nContent-Type: text/event-stream\r\n\r\n'
        raise ValueError('unknown loopback protocol scenario')

    def serve(self):
        while not self.stop.is_set():
            try:
                connection, _ = self.socket.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            connection.settimeout(5)
            try:
                method, target, version, head, body, auth = self._read_request(connection)
                parsed = urlsplit(target)
                case_id = parsed.path.lstrip('/')
                response, transfer_complete = self._response(case_id)
                sent = 0
                try:
                    # Deliberately short writes ensure the real client handles network framing.
                    for offset in range(0, len(response), 97):
                        piece = response[offset:offset + 97]
                        connection.sendall(piece)
                        sent += len(piece)
                except OSError:
                    pass
                record = {'case': case_id, 'method': method, 'target': target, 'version': version,
                          'local_port': self.port,
                          'request_body_bytes': len(body), 'request_body_sha256': hashlib.sha256(body).hexdigest(),
                          'request_body_hex': body.hex(),
                          'authorization_present': auth,
                          'request_headers': [line.decode('latin1') for line in head.split(b'\r\n')[1:]
                                              if not line.lower().startswith(b'authorization:')],
                          'response_bytes': len(response), 'response_bytes_sent': sent,
                          'response_sha256': hashlib.sha256(response).hexdigest(),
                          'response_wire_hex': response.hex(), 'transfer_complete': transfer_complete}
                with self.lock:
                    self.records.append(record)
            except (OSError, EOFError, ValueError):
                pass
            finally:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                connection.close()

    def close(self):
        self.stop.set()
        self.socket.close()
        self.thread.join(timeout=3)
        if self.thread.is_alive():
            raise RuntimeError('loopback protocol peer thread escaped')

    def observations(self):
        with self.lock:
            return list(self.records)


def validate_observations(records, requested, fixture_body):
    from protocol import expected_initial_request, strict_loads, structural
    if len(records) != len(requested):
        raise AssertionError('loopback peer request inventory differs')
    by_path = {urlsplit(request['network_url']).path.lstrip('/'): request for request in requested}
    if set(by_path) != set(NETWORK_CASES):
        raise AssertionError('loopback requested scenarios differ')
    indexed = {record['case']: record for record in records}
    if set(indexed) != set(NETWORK_CASES):
        raise AssertionError('loopback peer observed wrong scenarios')
    for case_id, record in indexed.items():
        request_url = urlsplit(by_path[case_id]['network_url'])
        if (request_url.scheme != 'http' or request_url.hostname != '127.0.0.1' or not request_url.port
                or request_url.username is not None or request_url.password is not None
                or request_url.port != record['local_port']):
            raise AssertionError('network adapter URL escaped local HTTP peer')
        if (record['method'] != 'POST' or record['version'] != 'HTTP/1.1'
                or record['target'] != '/' + case_id):
            raise AssertionError('loopback request method/version differs')
        if record['request_body_bytes'] <= 0 or record['request_body_bytes'] > 1024 * 1024:
            raise AssertionError('loopback synthetic request size differs')
        request_body = bytes.fromhex(record['request_body_hex'])
        if (len(request_body) != record['request_body_bytes']
                or hashlib.sha256(request_body).hexdigest() != record['request_body_sha256']):
            raise AssertionError('loopback request body evidence digest differs')
        request_json = strict_loads(request_body)
        if (structural(request_json) != structural(expected_initial_request())
                or record.get('authorization_present') is not False):
            raise AssertionError('loopback request body is not the synthetic Responses request')
        if record['transfer_complete'] is not (case_id != 'truncated-after-terminal'):
            raise AssertionError('loopback transfer outcome differs')
        wire = bytes.fromhex(record['response_wire_hex'])
        expected_wire = ProtocolPeer.expected_wire(case_id, fixture_body.encode('utf-8'))
        if (len(wire) != record['response_bytes'] or hashlib.sha256(wire).hexdigest() != record['response_sha256']
                or wire != expected_wire):
            raise AssertionError('loopback response wire differs from independently generated scenario')
        # Header rejection can legitimately close the socket before the peer
        # finishes writing a large body. Require receipt of the complete head;
        # for admitted streams, require all framing bytes to have been sent.
        complete_head = expected_wire.index(b'\r\n\r\n') + 4
        minimum_sent = (complete_head if case_id in ('duplicate-head-mime', 'wrong-mime',
                                                     'repeated-charset', 'unterminated-charset',
                                                     'missing-head-mime-with-trailer')
                        else len(expected_wire))
        if record['response_bytes_sent'] < minimum_sent:
            raise AssertionError('loopback peer did not send enough bytes for scenario')
    return indexed


def evidence(path, records):
    path.write_text(json.dumps(records, indent=2, sort_keys=True) + '\n')
