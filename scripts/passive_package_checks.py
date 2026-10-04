"""Isolated passive-agent networking and interactive enrollment for CI packages."""
import base64
import json
import pathlib
import socket
import re
import shutil
import subprocess

from package_wizard import Wizard


def initialize(api, command, wait, socks5=False):
    # This namespace and its firewall are confined to the disposable CI container.
    # Responses to inbound TCP are allowed; every outbound TCP SYN is rejected.
    script = pathlib.Path('/usr/local/sbin/pier-test-agent-network')
    script.write_text('''#!/bin/sh
set -eu
ip netns add pier-test-agent
ip link add pier-host type veth peer name pier-peer
ip link set pier-peer netns pier-test-agent
ip address add 10.203.0.1/30 dev pier-host
ip link set pier-host up
ip netns exec pier-test-agent ip address add 10.203.0.2/30 dev pier-peer
ip netns exec pier-test-agent ip link set pier-peer up
ip netns exec pier-test-agent ip link set lo up
ip netns exec pier-test-agent iptables -A OUTPUT -p tcp --syn -j REJECT
''')
    if socks5:
        with script.open('a') as output:
            output.write('iptables -A OUTPUT -d 10.203.0.2 -p tcp --syn -m owner --uid-owner pier-controller -j REJECT\n')
    script.chmod(0o755)
    pathlib.Path('/etc/systemd/system/pier-test-agent-network.service').write_text('''[Unit]
Description=CI passive agent network
Before=pier-agent.service pier-agent-upgrade.service
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/pier-test-agent-network
''')
    ip = shutil.which('ip')
    assert ip, 'iproute tools are required in the test image'
    for service, arguments in [('pier-agent', 'run --config /etc/pier/agent.yml'),
                               ('pier-agent-upgrade', '__apply-upgrade')]:
        dropin = pathlib.Path('/etc/systemd/system/' + service + '.service.d')
        dropin.mkdir(parents=True, exist_ok=True)
        (dropin / 'network.conf').write_text('''[Unit]
Requires=pier-test-agent-network.service
After=pier-test-agent-network.service
[Service]
ExecStart=
ExecStart=%s netns exec pier-test-agent /usr/bin/pier-agent %s
''' % (ip, arguments))
    command('systemctl', 'daemon-reload')
    command('systemctl', 'start', 'pier-test-agent-network')
    if socks5:
        pathlib.Path('/etc/systemd/system/pier-test-socks5.service').write_text('''[Unit]
Description=CI SOCKS5 proxy
Requires=pier-test-agent-network.service
After=pier-test-agent-network.service
[Service]
ExecStart=/usr/bin/python3 /src/scripts/socks5_package_proxy.py
Restart=always
[Install]
WantedBy=multi-user.target
''')
        command('systemctl', 'daemon-reload')
        command('systemctl', 'enable', '--now', 'pier-test-socks5')
        def proxy_ready():
            with socket.create_connection(('127.0.0.1', 1080), timeout=2):
                return True
        wait('SOCKS5 fixture ready', proxy_ready)
        try:
            socket.getaddrinfo('agent.socks.test', 7444)
        except socket.gaierror:
            pass
        else:
            raise AssertionError('fixture domain unexpectedly resolves without proxy')
    prefix = [ip, 'netns', 'exec', 'pier-test-agent']
    # Prove the filter actually rejects a reachable controller before enrollment.
    probe = subprocess.run(prefix + ['python3', '-c',
        "import socket; s=socket.socket(); s.settimeout(3); s.connect(('10.203.0.1',17443))"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    assert probe.returncode != 0, 'agent outbound connect unexpectedly permitted'
    rules = command(*(prefix + ['iptables', '-L', 'OUTPUT', '-vnx']))
    assert any('REJECT' in row and int(row.split()[0]) > 0 for row in rules.splitlines() if 'REJECT' in row), 'outbound firewall was not exercised'
    command(*(prefix + ['iptables', '-Z', 'OUTPUT']))
    wizard = Wizard(prefix=prefix)
    wizard.expect('Controller 网页地址'); wizard.send('https://localhost:8443\n')
    wizard.expect('连接方式'); wizard.expect('输入编号'); wizard.send('2\n')
    wizard.expect('Agent 本机监听地址'); wizard.send('\n')
    wizard.expect('Agent 名称'); wizard.send('passive-package-test\n')
    wizard.expect('数据目录'); wizard.expect('输入编号'); wizard.send('\n')
    wizard.expect('确认初始化信息'); wizard.expect('输入编号'); wizard.send('\n')
    wizard.expect('授权方式'); wizard.expect('输入编号'); wizard.send('\n')
    wizard.expect('粘贴网页提供的配对凭据')
    encoded = re.search(rb'/agent/init#([A-Za-z0-9_-]+)', wizard.buffer).group(1)
    request = json.loads(base64.urlsafe_b64decode(encoded + b'=' * (-len(encoded) % 4)))
    assert request['connection_mode'] == 'controller_to_agent'
    assert request['listen'] == '0.0.0.0:7444'
    request['agent_endpoint'] = ('agent.socks.test' if socks5 else '10.203.0.2') + ':7444'
    if socks5:
        # Agent listener is now open: prove the controller-specific direct block.
        with socket.create_connection(('10.203.0.2', 7444), timeout=2):
            pass
        probe = subprocess.run(['runuser', '-u', 'pier-controller', '--', 'python3', '-c',
            "import socket; socket.create_connection(('10.203.0.2',7444),timeout=2)"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        assert probe.returncode != 0
        rules = command('iptables', '-L', 'OUTPUT', '-vnx')
        assert any('REJECT' in row and int(row.split()[0]) > 0 for row in rules.splitlines() if 'REJECT' in row)
        command('iptables', '-Z', 'OUTPUT')
        request['agent_proxy'] = 'socks5://pier-ci:wrong@127.0.0.1:1080'
    grant = api('POST', '/v1/enrollments', request)
    if socks5:
        wait('wrong proxy authentication reported', lambda: api('GET', '/v1/enrollments/' + grant['id']).get('last_error'))
        request['agent_proxy'] = 'socks5://pier-ci:proxy-secret@127.0.0.1:1080'
    assert api('POST', '/v1/enrollments', request)['pairing'] == grant['pairing']
    wizard.send(grant['pairing'] + '\n')
    wizard.expect('pier-agent 已启动并启用开机自启', timeout=120)
    wizard.finish()
    assert grant['pairing'].encode() not in wizard.buffer
    result = wait('passive enrollment completed', lambda: completed(api, grant['id']))
    assert pathlib.Path('/etc/pier/agent.token').stat().st_mode & 0o777 == 0o600
    if socks5:
        for path in ['/etc/pier/agent.yml', '/etc/pier/agent.token']:
            assert 'proxy-secret' not in pathlib.Path(path).read_text()
        agent = api('GET', '/v1/agents/' + result['agent_id'])
        assert agent['connection']['proxy_configured'] is True
        assert 'proxy-secret' not in json.dumps(agent)
    return {'id': result['agent_id']}


def completed(api, grant):
    value = api('GET', '/v1/enrollments/' + grant)
    return value if value['state'] == 'completed' else None


def assert_no_outbound(command):
    rules = command('ip', 'netns', 'exec', 'pier-test-agent', 'iptables', '-L', 'OUTPUT', '-vnx')
    rejected = [row for row in rules.splitlines() if 'REJECT' in row]
    assert rejected and all(int(row.split()[0]) == 0 for row in rejected), 'agent or upgrade helper attempted outbound TCP'


def assert_socks5(command):
    rules = command('iptables', '-L', 'OUTPUT', '-vnx')
    rejected = [row for row in rules.splitlines() if 'REJECT' in row and '10.203.0.2' in row]
    assert rejected and all(int(row.split()[0]) == 0 for row in rejected), 'controller attempted a direct agent connection'
    log = pathlib.Path('/var/lib/pier-controller-package-test/socks5-channels.jsonl')
    purposes = {json.loads(line)['purpose'] for line in log.read_text().splitlines()}
    assert {'enrollment', 'control', 'artifact', 'upgrade', 'terminal'} <= purposes, purposes
