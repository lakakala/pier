#!/usr/bin/env python3
"""Controller packages in disposable systemd containers, including an inner Docker daemon."""
import glob
import hashlib
import http.cookiejar
import http.server
import json
import os
import pathlib
import pwd
import re
import shutil
import ssl
import socket
import subprocess
import sys
import tarfile
import threading
import time
import urllib.error
import urllib.request

assert os.geteuid() == 0 and pathlib.Path('/.dockerenv').exists()
assert pathlib.Path('/proc/1/comm').read_text().strip() == 'systemd'
distro, arch = sys.argv[1:3]
auto_upgrade = '--auto-upgrade' in sys.argv
base_revision = int(os.environ.get('PIER_TEST_REVISION', '1'))
upgrade_revision = base_revision + 1
invalid_revision = base_revision + 2
assert 1 <= base_revision <= 2**64 - 3
packages_dir = os.environ.get('PIER_TEST_PACKAGES', '/src/dist')
fixtures_dir = os.environ['PIER_TEST_FIXTURES']
os.environ['DEBIAN_FRONTEND'] = 'noninteractive'
root = pathlib.Path('/var/lib/pier-controller-package-test')
state = pathlib.Path('/var/lib/pier-controller')
meta_file = root / 'meta.json'


def command(*args):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
    assert result.returncode == 0, '%s\n%s' % (' '.join(args), result.stdout[-6000:])
    return result.stdout.strip()


def wait(label, poll, seconds=120):
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


def pid():
    return int(command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-controller'))


def checksum(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


def git(path, *args):
    return command('git', '-c', 'safe.directory=' + str(path), '-C', str(path), *args)


def commit(path):
    git(path, 'add', '.')
    git(path, '-c', 'user.name=Test', '-c', 'user.email=test@localhost', 'commit', '-qm', 'fixture')
    return git(path, 'rev-parse', 'HEAD')


def make_client():
    jar = http.cookiejar.LWPCookieJar(str(root / 'cookies'))
    if (root / 'cookies').exists():
        jar.load(ignore_discard=True)
    context = ssl.create_default_context(cafile=str(root / 'cert.pem'))
    client = urllib.request.build_opener(urllib.request.HTTPSHandler(context=context), urllib.request.HTTPCookieProcessor(jar))
    return jar, client


csrf = ''
def api(method, path, data=None):
    body = None if data is None else json.dumps(data).encode()
    request = urllib.request.Request('https://localhost:8443' + path, data=body, method=method,
        headers={'X-CSRF-Token': csrf, 'Origin': 'https://localhost:8443', 'Content-Type': 'application/json'})
    with client.open(request, timeout=60) as response:
        return json.load(response)


if '--after-boot' in sys.argv:
    meta = json.loads(meta_file.read_text())
    jar, client = make_client()
    wait('controller after boot', lambda: api('GET', '/v1/auth/status')['initialized'])
    csrf = api('GET', '/v1/auth/session')['csrf_token']
    assert api('GET', '/v1/repository')['commit'] == meta['commit'], 'boot must not synchronize changed Git HEAD'
    assert checksum('/etc/pier/controller.yml') == meta['config_hash']
    assert pathlib.Path('/proc/%s' % pid()).stat().st_uid == meta['uid']
    assert api('POST', '/v1/repository/sync', {})['commit'] == meta['next_commit']
    assert api('GET', '/v1/deployments/' + meta['job'])['state'] == 'succeeded'
    if distro == 'ubuntu2404':
        command('apt-get', 'remove', '-y', 'pier-controller')
        command('apt-get', 'purge', '-y', 'pier-controller')
    else:
        command('dnf', '--disablerepo=*', 'remove', '-y', 'pier-controller')
    assert subprocess.call(['systemctl', 'is-active', '--quiet', 'pier-controller']) != 0
    assert subprocess.call(['systemctl', 'is-enabled', '--quiet', 'pier-controller']) != 0
    assert checksum('/etc/pier/controller.yml') == meta['config_hash']
    assert (state / 'controller.db').exists() and (state / 'artifacts').is_dir()
    assert pwd.getpwnam('pier-controller').pw_uid == meta['uid']
    print('PASS %s/%s: boot without auto-sync, session/data recovery, manual sync, uninstall retention' % (distro, arch), flush=True)
    sys.exit(0)

wait('systemd', lambda: pathlib.Path('/run/systemd/system').exists())
root.mkdir(mode=0o755)
if distro == 'ubuntu2404':
    suffix = '_*-%s.ubuntu24.04_%s.deb' % (base_revision, arch)
    upgrade = glob.glob(fixtures_dir + '/pier-controller_*-%s.ubuntu24.04_%s.deb' % (upgrade_revision, arch))
    installer = ['apt-get', 'install', '-y']
else:
    rpm_arch = {'amd64': 'x86_64', 'arm64': 'aarch64'}[arch]
    rpm_dist = {'almalinux8': 'el8', 'almalinux9': 'el9'}[distro]
    suffix = '-*-%s.%s.%s.rpm' % (base_revision, rpm_dist, rpm_arch)
    upgrade = glob.glob(fixtures_dir + '/pier-controller-*-%s.%s.%s.rpm' % (upgrade_revision, rpm_dist, rpm_arch))
    installer = ['dnf', '--disablerepo=*', 'install', '-y']
original = glob.glob(packages_dir + '/pier-controller' + suffix)
agent_package = glob.glob(packages_dir + '/pier-agent' + suffix)
assert len(original) == len(upgrade) == len(agent_package) == 1
initial_agent = agent_package
if auto_upgrade and distro == 'ubuntu2404':
    initial_agent = glob.glob(os.environ['PIER_TEST_LEGACY_PACKAGES'] + '/pier-agent_*-%s_%s.deb' % (base_revision, arch))
    assert len(initial_agent) == 1, 'one legacy Ubuntu fixture is required'
command(*(installer + original + initial_agent))
if auto_upgrade:
    shutil.copytree('/usr/share/pier-controller/agent-releases', str(root / 'original-agent-bundle'))
    dropin = pathlib.Path('/etc/systemd/system/pier-agent-upgrade.service.d')
    dropin.mkdir(parents=True)
    (dropin / 'test.conf').write_text('[Service]\nExecStartPre=/bin/sleep 5\n')
    command('systemctl', 'daemon-reload')
assert pid() == 0
assert subprocess.call(['systemctl', 'is-enabled', '--quiet', 'pier-controller']) != 0
account = pwd.getpwnam('pier-controller')
assert account.pw_uid != 0 and account.pw_shell.endswith('nologin')
assert state.stat().st_uid == account.pw_uid and state.stat().st_mode & 0o777 == 0o700
assert pathlib.Path('/etc/pier/controller.yml').stat().st_mode & 0o777 == 0o640
assert pathlib.Path('/etc/pier/controller.yml').stat().st_gid == account.pw_gid
command('systemd-analyze', 'verify', '/usr/lib/systemd/system/pier-controller.service')
command('openssl', 'req', '-x509', '-nodes', '-newkey', 'rsa:2048', '-days', '1', '-subj', '/CN=localhost',
    '-addext', 'subjectAltName=DNS:localhost', '-keyout', str(root / 'key.pem'), '-out', str(root / 'cert.pem'))
pathlib.Path('/etc/nginx/conf.d/pier-controller-test.conf').write_text('server { listen 8443 ssl; server_name localhost; ssl_certificate %s/cert.pem; ssl_certificate_key %s/key.pem; location / { proxy_pass http://127.0.0.1:8080; proxy_set_header Host $http_host; proxy_http_version 1.1; proxy_set_header Upgrade $http_upgrade; proxy_set_header Connection "upgrade"; } }\n' % (root, root))
command('systemctl', 'enable', '--now', 'nginx', 'pier-controller')
# Base images can already have Nginx enabled when PID 1 starts. Apply the
# freshly written HTTPS test configuration even when the service is running.
command('systemctl', 'restart', 'nginx')
jar, client = make_client()
wait('uninitialized controller', lambda: api('GET', '/v1/auth/status'))
assert api('GET', '/v1/auth/status')['initialized'] is False
with socket.socket() as probe:
    assert probe.connect_ex(('127.0.0.1', 7443)) != 0, 'agent listener must wait for web init'
startup_fields = {line.strip() for line in pathlib.Path('/etc/pier/controller.yml').read_text().splitlines() if line.strip() and not line.lstrip().startswith('#')}
assert startup_fields == {'http_listen: 127.0.0.1:8080', 'state_dir: /var/lib/pier-controller'}
assert pathlib.Path('/proc/%s' % pid()).stat().st_uid == account.pw_uid
program = b'#!/bin/sh\nset -eu\necho $$ > "$PIER_DATA_DIR/pid"\ntrap "exit 0" TERM INT\nwhile :; do sleep 1; done\n'
class Source(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.send_header('Content-Length', str(len(program))); self.end_headers(); self.wfile.write(program)
    def log_message(self, *args):
        pass
source = http.server.HTTPServer(('127.0.0.1', 0), Source)
threading.Thread(target=source.serve_forever, daemon=True).start()
repo = root / 'repository'
(repo / 'apps/demo').mkdir(parents=True)
(repo / 'blueprints/demo').mkdir(parents=True)
recipe = repo / 'apps/demo/pier-pkg.yml'
recipe.write_text('schema: 2\nname: demo\nversion: "1"\nsource: {type: binary, url: "http://127.0.0.1:%s/program", format: raw}\nfiles: [{from: download, to: bin/demo, executable: true}]\nservice: {command: [bin/demo], env: {TERMINAL_TEST_SECRET: app-only}}\n' % source.server_port)
(repo / 'blueprints/demo/pier-blueprint.yml').write_text('schema: 1\nname: demo\napps: [{id: demo, app: apps/demo}]\n')
git(repo, 'init', '-q', '-b', 'main'); commit(repo)
command('chown', '-R', 'pier-controller:pier-controller', str(repo))
csrf = api('POST', '/v1/auth/init', {'username': 'admin', 'password': 'controller-package-test-123', 'repository': {'url': str(repo)}, 'settings': {'tcp_listen': '127.0.0.1:17443', 'max_concurrent_builds': 3}})['csrf_token']
jar.save(ignore_discard=True)
assert api('GET', '/v1/settings')['agent_listener']['listening'] is True
assert api('GET', '/v1/settings')['active']['agent_endpoint'] == 'localhost:17443'
with socket.create_connection(('127.0.0.1', 17443), timeout=5): pass
assert api('GET', '/v1/repository')['commit'] is None
assert not (state / 'snapshots').exists(), 'initialization must not fetch'
head = api('POST', '/v1/repository/sync', {})['commit']
assert api('GET', '/v1/repository')['needs_sync'] is False
identity = api('POST', '/v1/agents', {'name': 'package-test'})
pathlib.Path('/etc/pier/agent.token').write_text(identity['token'])
pathlib.Path('/etc/pier/agent.yml').write_text('agent_id: %s\ntoken_file: /etc/pier/agent.token\ncontroller_tcp: 127.0.0.1:17443\nstate_dir: /var/lib/pier-agent\n' % identity['id'])
if auto_upgrade:
    with open('/etc/pier/agent.yml', 'a') as out:
        out.write('heartbeat_seconds: 1\nruntime:\n  startup_grace_seconds: 30\n  stop_timeout_seconds: 2\n')
os.chmod('/etc/pier/agent.token', 0o600); os.chmod('/etc/pier/agent.yml', 0o600)
command('systemctl', 'enable', '--now', 'pier-agent')
agent_path = '/v1/agents/' + identity['id']
wait('agent online', lambda: api('GET', agent_path)['online'])
api('PUT', agent_path + '/binding', {'blueprint': 'blueprints/demo', 'variables': {}})


def deploy(images=None):
    job = api('POST', '/v1/deployments', {'agent_id': identity['id'], 'commit': api('GET', '/v1/repository')['commit'], 'images': images or {}})['id']
    def completed():
        result = api('GET', '/v1/deployments/' + job)
        assert result['state'] not in ('failed', 'rolled_back', 'rollback_failed'), json.dumps(result)
        return result['state'] == 'succeeded'
    wait('real deployment', completed, seconds=180)
    return job

job = deploy()
if auto_upgrade and distro == 'ubuntu2404':
    # The legacy fixture uses this build's binary with the old native version.
    # Verify the one-time manual migration before exercising automatic updates.
    legacy_version = command('dpkg-query', '-W', '-f=${Version}', 'pier-agent')
    assert legacy_version.endswith('-%s' % base_revision)
    agent_config_before = checksum('/etc/pier/agent.yml')
    token_before = checksum('/etc/pier/agent.token')
    agent_pid_before = command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-agent')
    command(*(['dpkg', '--force-confold', '--install'] + agent_package))
    assert command('dpkg-query', '-W', '-f=${Version}', 'pier-agent') == legacy_version + '.ubuntu24.04'
    assert command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-agent') == agent_pid_before
    command('systemctl', 'restart', 'pier-agent')
    def migrated():
        view = api('GET', agent_path)
        return (view['online'] and view['software']['supported']
                and view['software']['system'] == 'ubuntu24.04'
                and view['software']['package']['revision'] == base_revision
                and view['report']['apps'] and all(a['state'] == 'running' for a in view['report']['apps'])
                and command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-agent') not in ('0', agent_pid_before))
    wait('manual Ubuntu migration and restored apps', migrated, seconds=180)
    assert checksum('/etc/pier/agent.yml') == agent_config_before
    assert checksum('/etc/pier/agent.token') == token_before
    print('PASS %s/%s: manual migration from numeric DEB revision preserves configuration and identity' % (distro, arch), flush=True)
from terminal_package_checks import run_terminal_checks
job, csrf = run_terminal_checks(api, jar, root, agent_path, wait, deploy)
jar.save(ignore_discard=True)
old_pid = pid()
config_hash = checksum('/etc/pier/controller.yml')
command(*(installer + upgrade))
if distro == 'ubuntu2404': command('needrestart', '-r', 'a', '-b')
assert pid() == old_pid, 'upgrade restarted controller'
assert checksum('/etc/pier/controller.yml') == config_hash
if auto_upgrade:
    # Installing a controller package must not change its running release catalog.
    old_agent_pid = int(command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-agent'))
    agent_config_hash, token_hash = checksum('/etc/pier/agent.yml'), checksum('/etc/pier/agent.token')
    view = api('GET', agent_path)
    assert view['software']['supported'] and view['software']['package']['revision'] == base_revision, view
    assert view['upgrade']['target']['package']['revision'] == base_revision, view
# Saving must not change the current listener, build limit, origin or process.
change = api('PUT', '/v1/settings', {'max_concurrent_builds': 4})
assert change['active']['max_concurrent_builds'] == 3
assert change['saved']['max_concurrent_builds'] == 4 and change['restart_required']
assert pid() == old_pid
if auto_upgrade:
    pending = api('POST', '/v1/deployments', {'agent_id': identity['id'], 'commit': head, 'images': {}})['id']
    wait('deployment applying before controller restart', lambda: api('GET', '/v1/deployments/' + pending)['state'] == 'applying')
command('systemctl', 'restart', 'pier-controller')
wait('explicit restart', lambda: pid() not in (0, old_pid))
wait('session survives restart', lambda: api('GET', '/v1/auth/session'))
assert api('GET', '/v1/settings')['active']['max_concurrent_builds'] == 4
assert api('GET', '/v1/settings')['restart_required'] is False
if auto_upgrade:
    wait('update downloaded while deployment active', lambda: (api('GET', agent_path)['upgrade']['status'] or {}).get('phase') == 'waiting')
    assert api('GET', '/v1/deployments/' + pending)['state'] == 'applying'
    assert int(command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-agent')) == old_agent_pid
    wait('installation reserved after deployment', lambda: (api('GET', agent_path)['upgrade']['status'] or {}).get('phase') == 'installing')
    assert api('GET', '/v1/deployments/' + pending)['state'] == 'succeeded'
    try:
        api('POST', '/v1/deployments', {'agent_id': identity['id'], 'commit': head, 'images': {}})
        raise AssertionError('deployment accepted during upgrade')
    except urllib.error.HTTPError as error:
        assert error.code == 409
    def upgraded(allow_previous_failure=False):
        value = api('GET', agent_path)
        status = value['upgrade']['status'] or {}
        if not allow_previous_failure:
            assert status.get('phase') != 'failed', value
        return (value['online'] and status.get('phase') == 'succeeded'
                and value['software']['package']['revision'] == upgrade_revision
                and value['report']['apps'] and all(app['state'] == 'running' for app in value['report']['apps']))
    wait('automatic native upgrade and restored apps', upgraded, seconds=240)
    new_agent_pid = int(command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-agent'))
    assert new_agent_pid not in (0, old_agent_pid)
    assert checksum('/etc/pier/agent.yml') == agent_config_hash and checksum('/etc/pier/agent.token') == token_hash
    view = api('GET', agent_path)
    assert view['report']['apps'] and all(app['state'] == 'running' for app in view['report']['apps'])
    assert 'grant' not in view['upgrade']['status']
    journal = pathlib.Path('/var/lib/pier-agent-upgrade/transaction.json')
    assert journal.stat().st_uid == 0 and journal.stat().st_mode & 0o777 == 0o600
    assert json.loads(journal.read_text())['status']['phase'] == 'succeeded'
    if distro == 'ubuntu2404':
        assert command('dpkg-query', '-W', '-f=${Version}', 'pier-agent').endswith('-%s.ubuntu24.04' % upgrade_revision)
    else:
        assert command('rpm', '-q', '--qf', '%{RELEASE}', 'pier-agent') == '%s.%s' % (upgrade_revision, rpm_dist)
    # A lower controller bundle cannot downgrade or restart the agent.
    bundle_path = pathlib.Path('/usr/share/pier-controller/agent-releases')
    manifest = (bundle_path / 'manifest.json').read_bytes()
    for file in (root / 'original-agent-bundle').iterdir():
        shutil.copyfile(str(file), str(bundle_path / file.name))
    command('systemctl', 'restart', 'pier-controller')
    wait('lower bundle loaded', lambda: api('GET', agent_path)['online'] and api('GET', agent_path)['upgrade']['target']['package']['revision'] == base_revision)
    time.sleep(3)
    assert int(command('systemctl', 'show', '-p', 'MainPID', '--value', 'pier-agent')) == new_agent_pid
    assert api('GET', agent_path)['software']['package']['revision'] == upgrade_revision
    if arch == 'amd64':
        # Inject a failing native installer only inside this disposable container.
        # This exercises persistent pause and explicit manual repair for both formats.
        (bundle_path / 'manifest.json').write_bytes(manifest)
        command('systemctl', 'reset-failed', 'pier-controller')
        command('systemctl', 'restart', 'pier-controller')
        wait('normal update catalog restored', lambda: api('GET', agent_path)['upgrade']['target']['package']['revision'] == upgrade_revision)
        failing = root / 'failing-installer'
        failing.mkdir()
        native = 'dpkg' if distro == 'ubuntu2404' else 'rpm'
        wrapper = failing / native
        wrapper.write_text('#!/bin/sh\nif [ "$1" = --upgrade ] || [ "$1" = --force-confold ]; then exit 42; fi\nexec /usr/bin/' + native + ' "$@"\n')
        wrapper.chmod(0o755)
        (dropin / 'test.conf').write_text('[Service]\nEnvironment=PATH=' + str(failing) + ':/usr/bin:/bin\n')
        command('systemctl', 'daemon-reload')
        # Explicitly downgrade the test fixture so a second upgrade can be attempted.
        command(*((['dpkg', '--force-downgrade', '--install'] if distro == 'ubuntu2404'
                   else ['rpm', '--upgrade', '--oldpackage']) + agent_package))
        command('systemctl', 'restart', 'pier-agent')
        wait('installer failure reported', lambda: (api('GET', agent_path)['upgrade']['status'] or {}).get('phase') == 'failed')
        failed_install = json.loads(journal.read_text())['status']
        assert failed_install['release']['package']['revision'] == upgrade_revision and failed_install['grant']
        assert 'installation or startup failed' in failed_install['error']
        assert api('GET', agent_path)['software']['package']['revision'] == base_revision
        command('systemctl', 'restart', 'pier-agent')
        wait('failed installer stays paused', lambda: api('GET', agent_path)['online'])
        time.sleep(3)
        assert json.loads(journal.read_text())['status']['updated_at'] == failed_install['updated_at']
        (dropin / 'test.conf').unlink()
        command('systemctl', 'daemon-reload')
        release = failed_install['release']
        downloaded = '/var/lib/pier-agent-upgrade/' + release['sha256'] + '.' + release['format']
        command(*(['dpkg', '--install', downloaded] if distro == 'ubuntu2404' else ['rpm', '--upgrade', downloaded]))
        command('systemctl', 'restart', 'pier-agent')
        wait('manual recovery acknowledged', lambda: upgraded(True), seconds=180)
        assert checksum('/etc/pier/agent.yml') == agent_config_hash and checksum('/etc/pier/agent.token') == token_hash
        print('PASS %s/%s: real installer failure, persistent pause and manual recovery' % (distro, arch), flush=True)
    # A signed-in controller with incorrect native metadata must still be rejected.
    invalid = json.loads(manifest)
    for release in invalid['releases']:
        release['package']['revision'] = invalid_revision
    (bundle_path / 'manifest.json').write_text(json.dumps(invalid))
    command('systemctl', 'restart', 'pier-controller')
    wait('native metadata rejection', lambda: (api('GET', agent_path)['upgrade']['status'] or {}).get('phase') == 'failed')
    failed = json.loads(journal.read_text())['status']
    assert failed['phase'] == 'failed' and failed['release']['package']['revision'] == invalid_revision
    command('systemctl', 'restart', 'pier-agent')
    wait('agent restarts with failed update paused', lambda: api('GET', agent_path)['online'])
    time.sleep(3)
    assert json.loads(journal.read_text())['status']['updated_at'] == failed['updated_at']
    assert api('GET', agent_path)['software']['package']['revision'] == upgrade_revision
    (bundle_path / 'manifest.json').write_bytes(manifest)
    command('systemctl', 'reset-failed', 'pier-controller')
    command('systemctl', 'restart', 'pier-controller')
    wait('restored controller bundle', lambda: api('GET', agent_path)['upgrade']['target']['package']['revision'] == upgrade_revision)
    print('PASS %s/%s: authenticated auto-upgrade, idle reservation, deployment conflict, readiness, identity/data retention, no downgrade, native validation and persistent failure pause' % (distro, arch), flush=True)
# A bad saved port must not lock the administrator out of the web console.
with socket.socket() as occupied:
    occupied.bind(('127.0.0.1', 0)); occupied.listen()
    api('PUT', '/v1/settings', {'tcp_listen': '127.0.0.1:%s' % occupied.getsockname()[1]})
    command('systemctl', 'restart', 'pier-controller')
    wait('web available with broken agent listener', lambda: api('GET', '/v1/auth/session'))
    broken = api('GET', '/v1/settings')
    assert not broken['agent_listener']['listening'] and broken['agent_listener']['error']
    repaired = api('PUT', '/v1/settings', {'tcp_listen': '127.0.0.1:17443'})
    assert repaired['restart_required']
command('systemctl', 'restart', 'pier-controller')
wait('listener repaired after restart', lambda: api('GET', '/v1/settings')['agent_listener']['listening'])
crashed = pid()
# These scenarios deliberately restart several times within systemd's rate-limit
# window. Reset the test history before exercising automatic crash recovery.
command('systemctl', 'reset-failed', 'pier-controller')
command('systemctl', 'kill', '--kill-who=main', '--signal=SIGKILL', 'pier-controller')
wait('crash recovery', lambda: pid() not in (0, crashed))
wait('session survives crash', lambda: api('GET', '/v1/auth/session'))
# An isolated inner daemon verifies real Docker access and bind mounts for the
# service user without exposing the host Docker socket or changing host groups.
command('groupadd', '--system', 'docker')
command('usermod', '-aG', 'docker', 'pier-controller')
docker_log = open(str(root / 'docker.log'), 'w')
daemon = subprocess.Popen(['dockerd', '--storage-driver=vfs', '--iptables=false', '--ip-masq=false', '--data-root=' + str(root / 'docker')], stdout=docker_log, stderr=docker_log)
wait('inner Docker', lambda: subprocess.call(['docker', 'info'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) == 0)
rootfs = root / 'rootfs'
rootfs.mkdir()
pending_binaries = ['/bin/bash', '/usr/bin/uname', '/usr/bin/cp']
copied_binaries = set()
while pending_binaries:
    binary = pending_binaries.pop()
    if binary in copied_binaries:
        continue
    copied_binaries.add(binary)
    contents = pathlib.Path(binary).read_bytes()
    if contents.startswith(b'#!'):
        # AlmaLinux uses small coreutils shebang wrappers for cp and uname.
        pending_binaries.append(contents.splitlines()[0][2:].split()[0].decode())
        libraries = []
    else:
        linked = subprocess.run(['ldd', binary], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
        assert linked.returncode == 0 or 'not a dynamic executable' in linked.stdout or 'statically linked' in linked.stdout, linked.stdout
        libraries = re.findall(r'(/[\w/+.\-]+)', linked.stdout)
    for path in [binary] + libraries:
        dest = rootfs / path.lstrip('/')
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, str(dest)); shutil.copymode(path, str(dest))
(rootfs / 'bin/sh').symlink_to('bash')
(rootfs / 'tmp').mkdir(mode=0o1777)
with tarfile.open(str(root / 'rootfs.tar'), 'w') as tar: tar.add(str(rootfs), arcname='.')
command('docker', 'import', '--platform', 'linux/' + arch, str(root / 'rootfs.tar'), 'pier-package-smoke:local')
command('systemctl', 'restart', 'pier-controller')
wait('Docker permissions refreshed', lambda: api('GET', '/v1/auth/session'))
command('runuser', '-u', 'pier-controller', '--', 'docker', 'info')
source_repo = root / 'source'
source_repo.mkdir(); (source_repo / 'program.sh').write_bytes(program)
git(source_repo, 'init', '-q', '-b', 'main'); commit(source_repo)
command('chown', '-R', 'pier-controller:pier-controller', str(source_repo))
recipe.write_text('schema: 2\nname: demo\nversion: "2"\nsource: {type: git, repo: "%s", ref: main}\nbuild: {language: rust, commands: ["cp program.sh /output/demo"]}\nfiles: [{from: demo, to: bin/demo, executable: true}]\nservice: {command: [bin/demo], env: {TERMINAL_TEST_SECRET: app-only}}\n' % source_repo)
commit(repo)
assert api('GET', '/v1/repository')['commit'] == head
api('POST', '/v1/repository/sync', {})
wait('agent reconnects', lambda: api('GET', agent_path)['online'])
job = deploy({'demo': 'pier-package-smoke:local'})
assert api('GET', '/v1/deployments/' + job)['plan']['architecture'] == arch
head = api('GET', '/v1/repository')['commit']
(repo / 'README.md').write_text('This commit must not be fetched automatically on reboot.\n')
next_commit = commit(repo)
meta_file.write_text(json.dumps({'commit': head, 'next_commit': next_commit, 'config_hash': config_hash, 'uid': account.pw_uid, 'job': job}))
source.shutdown(); daemon.terminate(); daemon.wait(timeout=30)
print('PASS %s/%s: install, web init, saved/active settings, listener failure and repair, manual sync, binary/Docker source deployments, upgrade without restart, crash recovery' % (distro, arch), flush=True)
