#!/usr/bin/env python3
"""Offline, disposable Docker integration test: controller + three real agents.

Uses existing Ubuntu test images and target/debug binaries. No host users,
firewall rules or daemon settings are changed. Containers/network are removed.
"""
import concurrent.futures
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
IMAGE = os.environ.get("PIER_FORWARD_TEST_IMAGE", "pier-agent-test-ubuntu2404:amd64")


def command(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait(label, fn, seconds=45):
    end = time.monotonic() + seconds
    last = None
    while time.monotonic() < end:
        try:
            value = fn()
            if value:
                return value
        except (OSError, urllib.error.URLError, AssertionError) as error:
            last = error
        time.sleep(0.2)
    raise AssertionError(f"timed out: {label}: {last}")


def main():
    prefix = f"pier-forward-{uuid.uuid4().hex[:10]}"
    containers = []
    command("docker", "network", "create", prefix)
    try:
        with tempfile.TemporaryDirectory(prefix="pier-forward-", dir=ROOT / "target") as temporary:
            fixture = Path(temporary)
            source = fixture / "source"
            source.mkdir()
            (source / "echo.py").write_text('''#!/usr/bin/python3
import os, socket, threading, time
if os.environ.get("FAIL") == "yes": raise SystemExit(42)
def client(conn):
    with conn:
        while True:
            data = conn.recv(65536)
            if not data: break
            conn.sendall(data)
def tcp():
    with socket.socket() as server:
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", 19000)); server.listen()
        while True:
            conn, _ = server.accept()
            threading.Thread(target=client, args=(conn,), daemon=True).start()
def udp():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as server:
        server.bind(("127.0.0.1", 19001))
        while True:
            data, peer = server.recvfrom(65535); server.sendto(data, peer)
threading.Thread(target=tcp, daemon=True).start()
threading.Thread(target=udp, daemon=True).start()
while True: time.sleep(60)
''')
            repo = fixture / "repo"
            (repo / "app").mkdir(parents=True)
            (repo / "web").mkdir()
            (repo / "app/pier-pkg.yml").write_text('''schema: 2
name: echo
version: '1'
variables:
  FAIL: {default: 'no'}
  PORT: {default: '19000'}
ports:
  tcp: {protocol: tcp, port: '{{ PORT }}'}
  udp: {protocol: udp, port: 19001}
source: {type: binary, url: 'http://controller:8001/echo.py', format: raw}
files: [{from: download, to: bin/echo, executable: true}]
service:
  command: [bin/echo]
  env: {FAIL: '{{ FAIL }}'}
''')
            (repo / "web/pier-blueprint.yml").write_text('''schema: 1
name: ForwardTest
variables:
  FAIL: {default: 'no'}
apps:
- id: echo
  app: app
  variables: {FAIL: '{{ FAIL }}'}
''')
            command("git", "init", "-q", "-b", "main", str(repo))
            command("git", "-C", str(repo), "add", ".")
            command("git", "-C", str(repo), "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture")
            http = free_port()
            origin = f"http://localhost:{http}"
            (fixture / "controller.yml").write_text(f"state_dir: /state\nhttp_listen: 0.0.0.0:8080\n")

            def container(name, args, ports=()):
                full = f"{prefix}-{name}"
                containers.append(full)
                bindings = [value for port in ports for value in ("-p", port)]
                command("docker", "run", "--detach", "--pull=never", "--name", full,
                        "--network", prefix, "--network-alias", name,
                        "-v", f"{fixture}:/fixture:ro", "-v", f"{ROOT / 'target/debug'}:/binaries:ro",
                        "--entrypoint", args[0], *bindings, IMAGE, *args[1:])
                return full

            controller = container("controller", ["/bin/sh", "-ec",
                "git config --global --add safe.directory /fixture/repo; git config --global --add safe.directory /fixture/repo/.git; "
                "python3 -m http.server 8001 --bind 0.0.0.0 --directory /fixture/source >/tmp/source.log 2>&1 & "
                "while true; do /binaries/pier-controller --config /fixture/controller.yml & "
                "echo $! >/tmp/controller.pid; wait $! || true; sleep 1; done"], [f"127.0.0.1:{http}:8080"])
            cookie = ""
            csrf = ""

            def api(path, body=None, method=None):
                nonlocal cookie, csrf
                request = urllib.request.Request(origin + path, data=None if body is None else json.dumps(body).encode(),
                    method=method or ("GET" if body is None else "POST"), headers={
                        "Origin": origin, "Content-Type": "application/json", "Cookie": cookie, "X-CSRF-Token": csrf})
                try:
                    with urllib.request.urlopen(request, timeout=10) as response:
                        if response.headers.get("Set-Cookie"):
                            cookie = response.headers["Set-Cookie"].split(";", 1)[0]
                        value = json.load(response)
                        if "csrf_token" in value: csrf = value["csrf_token"]
                        return value
                except urllib.error.HTTPError as error:
                    raise AssertionError(f"{path}: {error.code}: {error.read().decode()}") from error

            wait("controller", lambda: api("/v1/auth/status"))
            api("/v1/auth/init", {"username": "admin", "password": "port-test-password-123",
                "repository": {"url": "/fixture/repo", "reference": "main"},
                "settings": {"public_url": origin, "tcp_listen": "0.0.0.0:7443", "agent_endpoint": "controller:7443", "max_concurrent_builds": 2}})
            api("/v1/repository/sync", {})
            wait("catalog", lambda: api("/v1/repository").get("commit"))
            ids = {}
            gateways = []
            for name, passive in [("backend", True), ("edge1", False), ("edge2", True)]:
                record = api("/v1/agents", {"name": name})
                ids[name] = record["id"]
                (fixture / f"{name}.token").write_text(record["token"])
                connection = "connection_mode: controller_to_agent\nlisten: 0.0.0.0:7444\n" if passive else "controller_tcp: controller:7443\n"
                (fixture / f"{name}.yml").write_text(f"agent_id: {record['id']}\ntoken_file: /fixture/{name}.token\nstate_dir: /state\nheartbeat_seconds: 1\nruntime: {{startup_grace_seconds: 1, stop_timeout_seconds: 1}}\n{connection}")
                published = ["127.0.0.1::18080", "127.0.0.1::18081/udp"] if name.startswith("edge") else []
                if passive:
                    # Seed an existing passive identity in the isolated controller DB.
                    # Enrollment has its own tests; production APIs intentionally forbid mode changes.
                    command("docker", "exec", controller, "python3", "-c",
                        "import sqlite3,json,sys; db=sqlite3.connect('/state/controller.db'); "
                        "row=json.loads(db.execute('select value from objects where namespace=? and key=?',('agents',sys.argv[1])).fetchone()[0]); "
                        "row['connection']={'mode':'controller_to_agent','endpoint':sys.argv[2]+':7444'}; "
                        "db.execute('update objects set value=? where namespace=? and key=?',(json.dumps(row),'agents',sys.argv[1])); db.commit()",
                        record["id"], name)
                full = container(name, ["/binaries/pier-agent", "run", "--config", f"/fixture/{name}.yml"], published)
                wait(f"{name} online", lambda: api(f"/v1/agents/{record['id']}")["online"])
                if name.startswith("edge"):
                    api(f"/v1/agents/{record['id']}/tags", {"tags": ["edge"]}, "PUT")
                    tcp = int(command("docker", "port", full, "18080/tcp").split(":")[-1])
                    udp = int(command("docker", "port", full, "18081/udp").split(":")[-1])
                    gateways.append((tcp, udp))

            binding = {"blueprint": "web", "variables": {}, "exposures": {"echo": {"tcp": {"tag": "edge", "port": 18080}, "udp": {"tag": "edge", "port": 18081}}}}
            api(f"/v1/agents/{ids['backend']}/bindings", binding)
            def deploy(action="deploy"):
                body = {"agent_id": ids["backend"], "blueprint": "web", "action": action}
                if action == "deploy": body["commit"] = api("/v1/repository")["commit"]
                job = api("/v1/deployments", body)["id"]
                return wait("deployment result", lambda: (value if (value := api(f"/v1/deployments/{job}"))["state"] in ["succeeded", "failed", "rolled_back", "rollback_failed"] else None), 90)
            # Occupy one entrance locally: other entrances and the deployed app must stay usable.
            occupier = f"{prefix}-edge1"
            command("docker", "exec", "--detach", occupier, "python3", "-c",
                "import socket,time,os; s=socket.socket(); s.bind(('0.0.0.0',18080)); s.listen(); open('/tmp/occupier.pid','w').write(str(os.getpid())); time.sleep(180)")
            wait("occupied entrance", lambda: command("docker", "exec", occupier, "cat", "/tmp/occupier.pid"))
            assert deploy()["state"] == "succeeded"
            def rows(): return api(f"/v1/agents/{ids['backend']}/exposures")["exposures"]
            wait("independent bind failure", lambda: len(value := rows()) == 4 and sum(row["state"] == "error" for row in value) == 1 and sum(row["state"] == "ready" for row in value) == 3)
            assert api(f"/v1/agents/{ids['backend']}")["report"]["blueprints"][0]["state"] == "running"
            command("docker", "exec", occupier, "/bin/sh", "-c", 'kill "$(cat /tmp/occupier.pid)"')
            wait("four listeners ready", lambda: len(value := rows()) == 4 and all(row["state"] == "ready" for row in value))

            def tcp_echo(port, payload=b"hello"):
                with socket.create_connection(("127.0.0.1", port), timeout=10) as client:
                    client.sendall(payload)
                    client.shutdown(socket.SHUT_WR)
                    reply = bytearray()
                    while data := client.recv(65536): reply.extend(data)
                    assert reply == payload
            def udp_echo(port, payload):
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
                    client.settimeout(10)
                    client.sendto(payload, ("127.0.0.1", port))
                    try:
                        assert client.recv(65535) == payload
                    except OSError:
                        print(f"UDP failed: {port}, {len(payload)} bytes; routes: {rows()}", flush=True)
                        raise
            for tcp, udp in gateways:
                tcp_echo(tcp, os.urandom(2 * 1024 * 1024))
                for payload in [b"udp", os.urandom(60000)]: udp_echo(udp, payload)
            # Docker's host UDP port proxy may drop empty datagrams; exercise them directly on the bridge.
            for name in ["edge1", "edge2"]:
                command("docker", "exec", controller, "python3", "-c",
                    "import socket,sys; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(10); s.sendto(b'',(sys.argv[1],18081)); assert s.recv(65535)==b''", name)
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                list(pool.map(lambda n: udp_echo(gateways[n % 2][1], bytes([n]) * 1000), range(16)))
            held = [socket.create_connection(("127.0.0.1", tcp), timeout=5) for tcp, _ in gateways for _ in range(3)]
            try:
                for connection in held: connection.sendall(b"open"); assert connection.recv(4) == b"open"
                physical = int(command("docker", "exec", controller, "python3", "-c",
                    "rows=[line.split() for line in open('/proc/net/tcp').readlines()[1:]]; print(sum(row[3]=='01' and any(int(row[i].split(':')[1],16) in [7443,7444] for i in [1,2]) for row in rows))"))
                assert physical == 3, f"expected exactly three agent control connections, got {physical}"
                print("PASS: TCP/UDP through two gateways; forwarding uses exactly three existing control connections", flush=True)
                api(f"/v1/agents/{ids['edge1']}/tags", {"tags": []}, "PUT")
                wait("tag removal", lambda: len(rows()) == 2)
                held[0].settimeout(10)
                try: assert held[0].recv(1) == b""
                except ConnectionResetError: pass
                tcp_echo(gateways[1][0])
            finally:
                for connection in held: connection.close()
            api(f"/v1/agents/{ids['edge1']}/tags", {"tags": ["edge"]}, "PUT")
            wait("tag restore", lambda: len(value := rows()) == 4 and all(row["state"] == "ready" for row in value))
            path = f"/v1/agents/{ids['backend']}/bindings/" + api(f"/v1/agents/{ids['backend']}/bindings")["bindings"][0]["id"]
            api(path, {"blueprint": "web", "variables": {"FAIL": "yes"}, "exposures": {"echo": {"tcp": {"tag": "edge", "port": 18082}}}}, "PATCH")
            assert {row["port"] for row in rows()} == {18080, 18081}, "saving draft must not change active listeners"
            assert deploy()["state"] == "rolled_back"
            wait("rollback listeners", lambda: len(value := rows()) == 4 and all(row["state"] == "ready" for row in value))
            tcp_echo(gateways[0][0])
            # Restart only the controller; application processes keep running.
            command("docker", "exec", controller, "/bin/sh", "-c", 'kill -TERM "$(cat /tmp/controller.pid)"')
            wait("controller restart", lambda: api("/v1/agents"))
            wait("reconnected listeners", lambda: len(value := rows()) == 4 and all(row["state"] == "ready" for row in value))
            tcp_echo(gateways[1][0]); udp_echo(gateways[0][1], b"restart")
            assert deploy("stop")["state"] == "succeeded"
            wait("stopped exposures", lambda: not rows())
            print("PASS: tag removal/restoration, pending binding, rollback, restart recovery and stop", flush=True)
    except BaseException:
        for name in containers:
            subprocess.run(["docker", "logs", "--tail", "70", name], check=False)
        raise
    finally:
        for name in reversed(containers):
            subprocess.run(["docker", "rm", "--force", name], stdout=subprocess.DEVNULL, check=False)
        subprocess.run(["docker", "network", "rm", prefix], stdout=subprocess.DEVNULL, check=False)


if __name__ == "__main__":
    main()
