#!/usr/bin/env python3
"""Authenticated SOCKS5 fixture, only for disposable GitHub Actions containers."""
import json
import pathlib
import select
import socket
import socketserver
import struct
import threading

LOG = pathlib.Path('/var/lib/pier-controller-package-test/socks5-channels.jsonl')
LOCK = threading.Lock()


def exact(stream, count):
    data = b''
    while len(data) < count:
        part = stream.recv(count - len(data))
        if not part:
            raise EOFError()
        data += part
    return data


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        client = self.request
        client.settimeout(15)
        try:
            assert exact(client, 3) == b'\x05\x01\x02'
            client.sendall(b'\x05\x02')
            assert exact(client, 1) == b'\x01'
            username = exact(client, exact(client, 1)[0])
            password = exact(client, exact(client, 1)[0])
            if (username, password) != (b'pier-ci', b'proxy-secret'):
                client.sendall(b'\x01\x01')
                return
            client.sendall(b'\x01\x00')
            # Require a domain CONNECT, proving DNS resolution happens here.
            assert exact(client, 4) == b'\x05\x01\x00\x03'
            domain = exact(client, exact(client, 1)[0])
            assert domain == b'agent.socks.test'
            port = struct.unpack('!H', exact(client, 2))[0]
            assert port in (7444, 7445)
            try:
                upstream = socket.create_connection(('10.203.0.2', port), timeout=5)
            except OSError:
                client.sendall(b'\x05\x05\x00\x01\x00\x00\x00\x00\x00\x00')
                return
            with upstream:
                client.sendall(b'\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00')
                magic = exact(client, 8)
                assert magic == b'PIERv2\x00\x00'
                size = exact(client, 2)
                length = struct.unpack('!H', size)[0]
                assert 0 < length <= 1024
                raw = exact(client, length)
                purpose = json.loads(raw.decode())['purpose']
                assert purpose in ('enrollment', 'control', 'artifact', 'upgrade', 'terminal')
                # Record only purpose, never prelude contents, identities or secrets.
                with LOCK:
                    with LOG.open('a') as output:
                        output.write(json.dumps({'purpose': purpose}) + '\n')
                upstream.sendall(magic + size + raw)
                client.settimeout(None)
                upstream.settimeout(None)
                while True:
                    readable, _, _ = select.select([client, upstream], [], [], 120)
                    if not readable:
                        return
                    for source in readable:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is client else client).sendall(data)
        except (OSError, EOFError, AssertionError, ValueError, KeyError):
            # Deliberately do not echo authentication input into systemd logs.
            return


class Server(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == '__main__':
    assert pathlib.Path('/.dockerenv').exists(), 'CI fixture requires a disposable container'
    with Server(('127.0.0.1', 1080), Handler) as server:
        server.serve_forever()
