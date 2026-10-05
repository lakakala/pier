"""Real WebSocket/PTY checks, called inside each disposable package test container."""
import base64
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import signal
import socket
import ssl
import struct
import time
import urllib.request


class TerminalSocket:
    def __init__(self, path, cookie, certificate, origin='https://localhost:8443', expected=101):
        context = ssl.create_default_context(cafile=str(certificate))
        self.socket = context.wrap_socket(socket.create_connection(('localhost', 8443), timeout=10), server_hostname='localhost')
        self.socket.settimeout(15)
        key = base64.b64encode(os.urandom(16)).decode()
        self.socket.sendall(('GET %s HTTP/1.1\r\nHost: localhost:8443\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: %s\r\nCookie: %s\r\nOrigin: %s\r\n\r\n' % (path, key, cookie, origin)).encode())
        header = b''
        while not header.endswith(b'\r\n\r\n'):
            header += self.exact(1)
            assert len(header) <= 16384
        assert int(header.split(b' ', 2)[1]) == expected, header
        if expected != 101:
            self.close()
            return
        accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        assert accept.lower() in header.lower(), header

    def exact(self, length):
        data = b''
        while len(data) < length:
            chunk = self.socket.recv(length - len(data))
            if not chunk:
                raise EOFError('terminal closed')
            data += chunk
        return data

    def send(self, data, opcode=2):
        mask = os.urandom(4)
        size = len(data)
        assert size <= 32768
        header = bytes([0x80 | opcode, 0x80 | size]) if size < 126 else bytes([0x80 | opcode, 0x80 | 126]) + struct.pack('!H', size)
        self.socket.sendall(header + mask + bytes(byte ^ mask[i % 4] for i, byte in enumerate(data)))

    def control(self, value):
        self.send(json.dumps(value).encode(), 1)

    def receive(self, closing=False):
        while True:
            first, second = self.exact(2)
            assert first & 0x80 and not second & 0x80
            size = second & 127
            if size == 126:
                size = struct.unpack('!H', self.exact(2))[0]
            elif size == 127:
                size = struct.unpack('!Q', self.exact(8))[0]
            assert size <= 65536
            data = self.exact(size)
            opcode = first & 15
            if opcode == 9:
                if not closing:
                    self.send(data, 10)
            elif opcode == 10:
                continue
            elif opcode == 1:
                return json.loads(data)
            elif opcode == 2:
                if not closing:
                    self.control({'type': 'ack', 'bytes': len(data)})
                return data
            else:
                raise EOFError('WebSocket closed')

    def until(self, marker):
        output = b''
        deadline = time.monotonic() + 30
        while marker not in output:
            assert time.monotonic() < deadline, output[-4096:]
            value = self.receive()
            assert isinstance(value, bytes), value
            output = (output + value)[-512 * 1024:]
        return output

    def command(self, command):
        marker = '__DONE_' + os.urandom(8).hex() + '__'
        self.send((command + "\nprintf '\\n" + marker + "\\n'\n").encode())
        return self.until(('\r\n' + marker + '\r\n').encode())

    def close(self):
        self.socket.close()


def alive(pid):
    try:
        return not re.search(r'^State:\s+Z', Path('/proc/%s/status' % pid).read_text(), re.M)
    except FileNotFoundError:
        return False


def run_terminal_checks(api, jar, root, agent_path, wait, deploy):
    wait('terminal capability and deployed app', lambda: 'app_terminal_v1' in api('GET', agent_path)['report'].get('capabilities', []) and api('GET', agent_path)['report']['apps'])
    instance = api('GET', agent_path)['report']['apps'][0]['instance']
    path = agent_path + '/apps/' + instance + '/terminals'
    cookie = '; '.join(c.name + '=' + c.value for c in jar)
    cert = root / 'cert.pem'
    context = ssl.create_default_context(cafile=str(cert))
    # A separate browser login must not be able to claim the owner's ticket.
    request = urllib.request.Request('https://localhost:8443/v1/auth/login',
        data=json.dumps({'username': 'admin', 'password': 'controller-package-test-123'}).encode(),
        headers={'Origin': 'https://localhost:8443', 'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, context=context, timeout=15) as response:
        other_cookie = response.headers['Set-Cookie'].split(';', 1)[0]
        response.read()

    def opening():
        ticket = api('POST', path, {'cols': 100, 'rows': 30})
        # An unauthorized attempt must not consume the owner's one-time ticket.
        TerminalSocket(ticket['websocket_url'], '', cert, expected=401)
        TerminalSocket(ticket['websocket_url'], cookie, cert, origin='https://other.test', expected=403)
        TerminalSocket(ticket['websocket_url'], other_cookie, cert, expected=404)
        ws = TerminalSocket(ticket['websocket_url'], cookie, cert)
        ready = ws.receive()
        assert ready['type'] == 'ready', ready
        TerminalSocket(ticket['websocket_url'], cookie, cert, expected=409)
        ws.send(b"stty -echo; printf '\\n__READY__\\n'\n")
        ws.until(b'\r\n__READY__\r\n')
        output = ws.command("printf '__PID__%s__' \"$$\"")
        pid = int(re.search(rb'__PID__(\d+)__', output).group(1))
        return ws, ready, pid

    ws, ready, shell_pid = opening()
    account = pwd.getpwnam(ready['user'])
    assert account.pw_uid != 0 and account.pw_dir == ready['home']
    assert account.pw_shell.endswith('/nologin')
    output = ws.command("printf 'IDENTITY:%s:%s:%s\\n' \"$(id -u)\" \"$(id -g)\" \"$PWD\"; test -t 0 && echo REAL_TTY; test -z \"${TERMINAL_TEST_SECRET+x}\" && echo CLEAN_ENV")
    assert ('IDENTITY:%s:%s:%s' % (account.pw_uid, account.pw_gid, account.pw_dir)).encode() in output, output
    assert b'REAL_TTY' in output and b'CLEAN_ENV' in output, output
    ws.control({'type': 'resize', 'cols': 101, 'rows': 37})
    assert b'37 101' in ws.command('stty size')
    assert '中文终端'.encode() in ws.command("printf '中文终端\\n'")
    ws.send(b'yes\n')
    count = 0
    while count < 512 * 1024:
        chunk = ws.receive()
        assert isinstance(chunk, bytes), chunk
        count += len(chunk)
    assert api('GET', agent_path)['online']
    ws.send(b'\x03')
    assert b'INTERRUPTED' in ws.command('echo INTERRUPTED')
    data_pid = Path(account.pw_dir) / 'pid'
    for _ in range(2):
        previous = int(data_pid.read_text())
        os.kill(previous, signal.SIGKILL)
        wait('application automatically restarts', lambda: int(data_pid.read_text()) != previous and alive(int(data_pid.read_text())))
        assert alive(shell_pid), 'application recovery killed the terminal'
        assert b'STILL_CONNECTED' in ws.command('echo STILL_CONNECTED')
    service_pid = int(data_pid.read_text())
    output = ws.command("sleep 600 & printf '__JOB__%s__' \"$!\"")
    background_pid = int(re.search(rb'__JOB__(\d+)__', output).group(1))
    ws.close()
    wait('disconnected Bash and background job reaped', lambda: not alive(shell_pid) and not alive(background_pid))
    assert alive(service_pid), 'closing terminal killed service'
    ws, _, shell_pid = opening()
    job = deploy()
    wait('deployment closes old terminal', lambda: not alive(shell_pid))
    deadline = time.monotonic() + 15
    while True:
        assert time.monotonic() < deadline
        # Deployment has already terminated the shell. Drain buffered frames
        # without writing ACKs/Pongs to a transport the server may have closed.
        event = ws.receive(closing=True)
        if isinstance(event, dict) and event['type'] == 'exit':
            assert event['reason'] == 'deployment_started', event
            break
    ws.close()
    ws, _, shell_pid = opening()
    token = api('GET', '/v1/auth/session')['csrf_token']
    request = urllib.request.Request('https://localhost:8443/v1/auth/logout', data=b'',
        headers={'Cookie': cookie, 'Origin': 'https://localhost:8443', 'X-CSRF-Token': token})
    with urllib.request.urlopen(request, context=context, timeout=15) as response:
        assert response.status == 204
    wait('logout terminates Bash', lambda: not alive(shell_pid))
    ws.close()
    session = api('POST', '/v1/auth/login', {'username': 'admin', 'password': 'controller-package-test-123'})
    print('PASS terminal: account/home/environment, real PTY, resize/UTF-8, flow control, Ctrl+C, app restart isolation, disconnect/deployment cleanup and WebSocket authentication', flush=True)
    return job, session['csrf_token']
