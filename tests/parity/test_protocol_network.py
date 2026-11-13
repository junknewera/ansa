"""Local peer framing tests; adapters themselves are exercised by protocol capture."""
import socket
import copy
import hashlib
import json
import subprocess
import unittest

import protocol_network
import protocol
import os


class ProtocolNetworkPeerTests(unittest.TestCase):
    def test_native_adapters_reject_url_userinfo_before_connect(self):
        for adapter, binary in protocol.binaries(os.environ.get('ANSA_PARITY_CPP_BUILD', 'build-m2'), 'rust/target').items():
            with self.subTest(adapter=adapter), socket.socket() as trap:
                trap.bind(('127.0.0.1', 0))
                trap.listen(1)
                trap.settimeout(0.1)
                request = {'network_url': 'http://127.0.0.1:1@127.0.0.1:%d/valid-control' % trap.getsockname()[1]}
                result = subprocess.run([str(binary)], input=(json.dumps(request)+'\n').encode(),
                                        capture_output=True, timeout=5, check=False)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(json.loads(result.stdout), {'accepted': False, 'executors': 0})
                with self.assertRaises(socket.timeout):
                    trap.accept()

    def test_loopback_peer_serves_wire_exact_scenarios(self):
        body = 'data: synthetic fixture\n\n'
        request_body = protocol.exact_dumps(protocol.expected_initial_request()).encode()
        peers = {}
        records = []
        requests = []
        try:
            for case in protocol_network.NETWORK_CASES:
                peer = peers[case] = protocol_network.ProtocolPeer(body)
                url = peer.url(case)
                requests.append({'network_url': url})
                with socket.create_connection(('127.0.0.1', peer.port), timeout=2) as client:
                    request = ('POST /%s HTTP/1.1\r\nHost: 127.0.0.1:%d\r\n'
                               'Content-Length: %d\r\n\r\n' % (case, peer.port, len(request_body))).encode() + request_body
                    client.sendall(request)
                    received = bytearray()
                    while True:
                        data = client.recv(4096)
                        if not data:
                            break
                        received.extend(data)
                self.assertEqual(bytes(received), protocol_network.ProtocolPeer.expected_wire(case, body.encode()))
            records = [record for peer in peers.values() for record in peer.observations()]
            self.assertEqual(len(records), len(protocol_network.NETWORK_CASES))
            indexed = protocol_network.validate_observations(records, requests, body)
            self.assertEqual(set(indexed), set(protocol_network.NETWORK_CASES))
            for altered in (
                    {'model': 'gpt-6.1-sol', 'store': False, 'stream': True},
                    {**protocol.expected_initial_request(), 'tools': []},
                    {**protocol.expected_initial_request(), 'input': []}):
                changed = copy.deepcopy(records)
                raw = protocol.exact_dumps(altered).encode()
                changed[0].update(request_body_hex=raw.hex(), request_body_bytes=len(raw),
                                  request_body_sha256=hashlib.sha256(raw).hexdigest())
                with self.assertRaises(AssertionError):
                    protocol_network.validate_observations(changed, requests, body)
            changed = copy.deepcopy(records)
            changed[0]['authorization_present'] = True
            with self.assertRaises(AssertionError):
                protocol_network.validate_observations(changed, requests, body)
        finally:
            for peer in peers.values():
                peer.close()


if __name__ == '__main__':
    unittest.main()
