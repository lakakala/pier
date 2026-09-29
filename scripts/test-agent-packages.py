#!/usr/bin/env python3
"""Real package/systemd/PTY integration. Run only inside the test containers."""
import base64
import glob
import hashlib
import http.server
import json
import os
import pathlib
import pty
import re
import select
import signal
import ssl
import subprocess
import sys
import termios
import threading
import time
import urllib.request

assert os.geteuid() == 0 and pathlib.Path('/.dockerenv').exists(), 'disposable root container required'
assert pathlib.Path('/proc/1/comm').read_text().strip() == 'systemd', 'systemd must be PID 1'
distro, arch = sys.argv[1:3]
os.environ['DEBIAN_FRONTEND'] = 'noninteractive'
META = pathlib.Path('/var/lib/pier-package-test.json')


def command(*args):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
    if result.returncode:
        raise AssertionError('command failed: %s\n%s' % (' '.join(args), result.stdout[-4000:]))
    return result.stdout.strip()


def wait(label, poll, seconds=90):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            value = poll()
            if value:
                return value
        except (OSError, ValueError, urllib.error.URLError):
            pass
        time.sleep(0.2)
    raise AssertionError('timed out: ' + label)


def running():
    return subprocess.call(['systemctl', 'is-active', '--quiet', 'pier-agent']) == 0


def pid():
    return int(command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-agent'))


def alive(value):
    try:
        return not re.search(r'^State:\s+Z', pathlib.Path('/proc/%s/status' % value).read_text(), re.M)
    except OSError:
        return False


def checksum(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


if '--after-boot' in sys.argv:
    meta = json.loads(META.read_text())
    wait('enabled service starts after boot with controller offline', running)
    wait('app restored from local state after boot', lambda: pathlib.Path(meta['pid_file']).exists())
    app_pid = int(pathlib.Path(meta['pid_file']).read_text())
    assert alive(app_pid)
    assert checksum('/etc/pier/agent.token') == meta['token_hash']
    if distro == 'ubuntu2404':
        removed = command('apt-get', 'remove', '-y', 'pier-agent')
        removed += command('apt-get', 'purge', '-y', 'pier-agent')
    else:
        removed = command('dnf', '--disablerepo=*', 'remove', '-y', 'pier-agent')
    assert not running(), removed
    assert subprocess.call(['systemctl', 'is-enabled', '--quiet', 'pier-agent']) != 0
    wait('apps stopped on uninstall', lambda: not alive(app_pid))
    assert pathlib.Path(meta['pid_file']).exists(), 'app data must survive uninstall'
    assert pathlib.Path('/etc/pier/agent.yml').exists()
    assert checksum('/etc/pier/agent.token') == meta['token_hash']
    print('PASS %s/%s: boot recovery, uninstall and data retention' % (distro, arch), flush=True)
    sys.exit(0)

wait('systemd boot', lambda: pathlib.Path('/run/systemd/system').exists())
assert not pathlib.Path('/usr/sbin/policy-rc.d').exists(), 'test image must allow normal package service hooks'
if distro == 'ubuntu2404':
    original = glob.glob('/src/dist/pier-agent_*-1.ubuntu24.04_%s.deb' % arch)
    upgrade = glob.glob('/src/target/package-upgrade-fixtures/%s/pier-agent_*-2.ubuntu24.04_%s.deb' % (arch, arch))
    installer = ['apt-get', 'install', '-y']
else:
    rpm_arch = {'amd64':'x86_64', 'arm64':'aarch64'}[arch]
    rpm_dist = {'almalinux8': 'el8', 'almalinux9': 'el9'}[distro]
    original = glob.glob('/src/dist/pier-agent-*-1.%s.%s.rpm' % (rpm_dist, rpm_arch))
    upgrade = glob.glob('/src/target/package-upgrade-fixtures/%s/pier-agent-*-2.%s.%s.rpm' % (arch, rpm_dist, rpm_arch))
    # Runtime dependencies are installed by the test image; test the local RPM
    # without refreshing remote repository metadata on every container boot.
    installer = ['dnf', '--disablerepo=*', 'install', '-y']
assert len(original) == 1 and len(upgrade) == 1, 'exactly one original and upgrade fixture required'
command(*(installer + original))
assert not running(), 'installation must not start the service'
assert subprocess.call(['systemctl', 'is-enabled', '--quiet', 'pier-agent']) != 0
assert not pathlib.Path('/etc/pier/agent.yml').exists()
command('systemd-analyze', 'verify', '/usr/lib/systemd/system/pier-agent.service')

root = pathlib.Path('/tmp/pier-package-test')
root.mkdir(mode=0o755)
repo = root / 'repo'
(repo / 'apps/demo').mkdir(parents=True)
(repo / 'blueprints/demo').mkdir(parents=True)
program = b'#!/bin/sh\nset -eu\necho $$ > "$PIER_DATA_DIR/pid"\necho kept > "$PIER_DATA_DIR/value"\ntrap "exit 0" TERM INT\nwhile :; do sleep 1; done\n'


class Source(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Length', str(len(program)))
        self.end_headers()
        self.wfile.write(program)

    def log_message(self, *args):
        pass


source = http.server.HTTPServer(('127.0.0.1', 0), Source)
threading.Thread(target=source.serve_forever, daemon=True).start()
(repo / 'apps/demo/pier-pkg.yml').write_text('schema: 2\nname: demo\nversion: "1"\nsource:\n  type: binary\n  url: http://127.0.0.1:%s/program\n  format: raw\nfiles:\n- from: download\n  to: bin/demo\n  executable: true\nservice:\n  command: [bin/demo]\n' % source.server_port)
(repo / 'blueprints/demo/pier-blueprint.yml').write_text('schema: 1\nname: demo\napps:\n- id: demo\n  app: apps/demo\n')
command('git', '-C', str(repo), 'init', '-q')
command('git', '-C', str(repo), 'checkout', '-qb', 'main')
command('git', '-C', str(repo), 'add', '.')
command('git', '-C', str(repo), '-c', 'user.name=Test', '-c', 'user.email=test@localhost', 'commit', '-qm', 'fixture')
admin = os.urandom(32).hex()
(root / 'controller.yml').write_text('state_dir: /tmp/pier-package-test/controller\nrepository:\n  url: %s\n  reference: main\n  sync_interval_seconds: 3600\nhttp_listen: 127.0.0.1:8080\ntcp_listen: 127.0.0.1:7443\npublic_url: https://localhost:8443\nagent_endpoint: localhost:7443\n' % repo)
log = open(str(root / 'controller.log'), 'w')
controller = subprocess.Popen(['/test/pier-controller', '--config', str(root / 'controller.yml')], stdout=log, stderr=log)
# A real HTTPS reverse proxy. Only the browser-side client trusts this test CA;
# pier-agent receives no certificates and makes no HTTPS connections.
(root / 'openssl.cnf').write_text('[req]\ndistinguished_name=dn\n[dn]\n[ext]\nbasicConstraints=critical,CA:TRUE\nsubjectAltName=DNS:localhost\n')
command('openssl', 'req', '-x509', '-nodes', '-newkey', 'rsa:2048', '-days', '1', '-subj', '/CN=localhost', '-config', str(root / 'openssl.cnf'), '-extensions', 'ext', '-keyout', str(root / 'key.pem'), '-out', str(root / 'cert.pem'))
(root / 'nginx.conf').write_text('pid /tmp/pier-package-test/nginx.pid; error_log /tmp/pier-package-test/nginx.log; events {} http { access_log off; server { listen 8443 ssl; server_name localhost; ssl_certificate /tmp/pier-package-test/cert.pem; ssl_certificate_key /tmp/pier-package-test/key.pem; location / { proxy_pass http://127.0.0.1:8080; proxy_set_header Host $host; } } }')
command('nginx', '-c', str(root / 'nginx.conf'))
context = ssl.create_default_context(cafile=str(root / 'cert.pem'))


import http.cookiejar
cookie_jar = http.cookiejar.CookieJar()
client = urllib.request.build_opener(urllib.request.HTTPSHandler(context=context), urllib.request.HTTPCookieProcessor(cookie_jar))
csrf = ''


def api(method, path, data=None):
    body = None if data is None else json.dumps(data).encode()
    request = urllib.request.Request('https://localhost:8443' + path, data=body, method=method, headers={'X-CSRF-Token':csrf, 'Content-Type':'application/json', 'Origin':'https://localhost:8443'})
    with client.open(request, timeout=10) as response:
        return json.load(response)


wait('controller auth status', lambda: api('GET', '/v1/auth/status'))
csrf = api('POST', '/v1/auth/init', {'username':'admin','password':admin})['csrf_token']
api('POST', '/v1/repository/sync', {})
wait('controller catalog', lambda: api('GET', '/v1/repository').get('commit'))
with urllib.request.urlopen('https://localhost:8443/agent/init', context=context) as response:
    assert response.headers['Cache-Control'] == 'no-store'
    assert 'frame-ancestors' in response.headers['Content-Security-Policy']
    html = response.read()
    assert b'id="root"' in html
    assert b'/assets/' in html


class Wizard:
    def __init__(self, term='dumb'):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ['TERM'] = term
            os.execv('/usr/bin/pier-agent', ['pier-agent', 'init'])
        self.buffer = b''
        self.cursor = 0
        self.original_terminal = termios.tcgetattr(self.fd)

    def expect(self, marker, timeout=60):
        needle = marker.encode()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            index = self.buffer.find(needle, self.cursor)
            if index >= 0:
                self.cursor = index + len(needle)
                return
            if select.select([self.fd], [], [], 0.2)[0]:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                self.buffer += data
        # Do not dump the buffer: an echo regression could contain credentials.
        raise AssertionError('wizard did not show expected prompt: ' + marker)

    def send(self, value):
        os.write(self.fd, value.encode())

    def finish(self, success=True):
        def exited():
            result = os.waitpid(self.pid, os.WNOHANG)
            return result if result[0] else None
        result = wait('wizard exit', exited, seconds=30)
        if success:
            assert os.WIFEXITED(result[1]) and os.WEXITSTATUS(result[1]) == 0, 'wizard failed'
        now = termios.tcgetattr(self.fd)
        assert now[3] & termios.ECHO, 'terminal echo was not restored'
        os.close(self.fd)


wizard = Wizard()
wizard.expect('Controller 网页地址')
wizard.send('http://invalid\n')
wizard.expect('请输入 HTTPS 地址')
wizard.send('https://localhost:8443\n')
wizard.expect('Agent 名称')
wizard.send('\n')
wizard.expect('输入编号')
wizard.send('9\n')
wizard.expect('请输入有效编号')
wizard.send('\n')
wizard.expect('确认初始化信息')
wizard.expect('输入编号')
wizard.send('\n')
wizard.expect('授权方式')
wizard.expect('输入编号')
wizard.send('\n')
wizard.expect('粘贴网页提供的配对凭据')
# Cancellation during hidden input must preserve a resumable request and echo.
wizard.send('\x03')
wizard.finish(success=False)
assert pathlib.Path('/etc/pier/agent.init.json').exists()
assert not pathlib.Path('/etc/pier/agent.yml').exists()
assert not running()

wizard = Wizard()
wizard.expect('检测到未完成的初始化')
wizard.expect('输入编号')
wizard.send('\n')
wizard.expect('授权方式')
wizard.expect('输入编号')
wizard.send('\n')
wizard.expect('粘贴网页提供的配对凭据')
link = re.search(rb'https://localhost:8443/agent/init#([A-Za-z0-9_-]+)', wizard.buffer).group(1)
request = json.loads(base64.urlsafe_b64decode(link + b'=' * (-len(link) % 4)))
grant = api('POST', '/v1/enrollments', request)
wizard.send('invalid\n')
wizard.expect('配对凭据无效')
wizard.expect('粘贴网页提供的配对凭据')
wizard.send(grant['pairing'] + '\n')
wizard.expect('pier-agent 已启动并启用开机自启', timeout=120)
wizard.finish()
assert grant['pairing'].encode() not in wizard.buffer
assert admin.encode() not in wizard.buffer
assert pathlib.Path('/etc/pier/agent.yml').stat().st_mode & 0o777 == 0o600
assert pathlib.Path('/etc/pier/agent.token').stat().st_mode & 0o777 == 0o600
assert not pathlib.Path('/etc/pier/agent.init.json').exists()
command('systemctl', 'is-enabled', '--quiet', 'pier-agent')
agent = wait('agent online', lambda: next((a for a in api('GET', '/v1/agents')['agents'] if a['online']), None))
agent_id = agent['id']
api('PUT', '/v1/agents/' + agent_id + '/binding', {'blueprint':'blueprints/demo','variables':{}})
commit = api('GET', '/v1/repository')['commit']
job = api('POST', '/v1/deployments', {'agent_id':agent_id,'commit':commit})['id']
def deployed():
    result = api('GET', '/v1/deployments/' + job)
    assert result['state'] not in ('failed', 'rolled_back', 'rollback_failed'), 'fixture deployment failed: ' + json.dumps(result)
    return result['state'] == 'succeeded'
wait('deployment succeeds', deployed)
app = wait('app running', lambda: next((a for a in api('GET', '/v1/agents/' + agent_id)['report']['apps'] if a['state'] == 'running'), None))
old_pid, app_pid = pid(), app['pid']
token_hash = checksum('/etc/pier/agent.token')
command(*(installer + upgrade))
if distro == 'ubuntu2404':
    command('needrestart', '-r', 'a', '-b')
assert pid() == old_pid and alive(app_pid), 'upgrade restarted a running process'
assert checksum('/etc/pier/agent.token') == token_hash
command('systemctl', 'restart', 'pier-agent')
wait('new agent PID after manual restart', lambda: running() and pid() != old_pid)
wait('old app process exits', lambda: not alive(app_pid))
wait('agent reconnects', lambda: api('GET', '/v1/agents/' + agent_id)['online'])
crashed = pid()
command('systemctl', 'kill', '--kill-who=main', '--signal=SIGKILL', 'pier-agent')
wait('automatic agent restart', lambda: running() and pid() != crashed, seconds=120)
wait('online after crash', lambda: api('GET', '/v1/agents/' + agent_id)['online'])
# Arrow-key menu: choose Exit from the existing-config menu, no new identity.
wizard = Wizard(term='xterm')
wizard.expect('现有服务')
wizard.send('\x1b[B\x1b[B\n')
wizard.finish()
assert len(api('GET', '/v1/agents')['agents']) == 1
app = wait('app after crash', lambda: next((a for a in api('GET', '/v1/agents/' + agent_id)['report']['apps'] if a['state'] == 'running' and alive(a['pid'])), None))
pid_file = '/var/lib/pier-agent/apps/%s/data/pid' % app['instance']
META.write_text(json.dumps({'pid_file':pid_file, 'token_hash':token_hash}))
# The next phase restarts the container: the app must recreate this file without
# the controller (which is deliberately not installed as a boot service).
pathlib.Path(pid_file).unlink()
controller.terminate()
controller.wait(timeout=30)
print('PASS %s/%s: PTY init, encrypted enrollment, systemd, upgrade without restart and crash recovery' % (distro, arch), flush=True)
