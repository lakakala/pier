"""Local smart-HTTP Git origins and authenticated HTTP(S) proxies for Rust tests.

Requires Python 3 and OpenSSL, as do the controller browser tests. All servers
bind loopback; the .invalid repository hostname can only work through our proxy.
"""

import base64
import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
from urllib.parse import urlsplit


def main():
    with tempfile.TemporaryDirectory(prefix="pier-repository-proxy-") as temporary:
        root = Path(temporary)
        repo = root / "repo"
        repo.mkdir()
        (repo / "README.md").write_text("repository proxy fixture\n")
        for args in [
            ["init", "-q", "-b", "main"],
            ["add", "."],
            ["-c", "user.name=Test", "-c", "user.email=test@localhost", "commit", "-qm", "fixture"],
        ]:
            subprocess.run(["git", *args], cwd=repo, check=True)
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
             "-subj", "/CN=repository.invalid", "-addext",
             "subjectAltName=DNS:repository.invalid,DNS:localhost,IP:127.0.0.1",
             "-keyout", str(root / "key.pem"), "-out", str(root / "cert.pem")],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(root / "cert.pem", root / "key.pem")
        log = root / "requests.jsonl"
        log.touch()
        log_lock = threading.Lock()

        def record(kind, method, path):
            with log_lock, log.open("a") as output:
                output.write(json.dumps({"kind": kind, "method": method, "path": path}) + "\n")

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def respond(self, status, headers, body):
                self.send_response(status)
                for name, value in headers:
                    if name.lower() not in ("content-length", "connection", "transfer-encoding"):
                        self.send_header(name, value)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(body)

        class Origin(Handler):
            def do_GET(self):
                record("origin", self.command, self.path)
                parsed = urlsplit(self.path)
                payload = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                env = dict(os.environ, GIT_PROJECT_ROOT=str(root), GIT_HTTP_EXPORT_ALL="1",
                           PATH_INFO=parsed.path, QUERY_STRING=parsed.query,
                           REQUEST_METHOD=self.command, CONTENT_LENGTH=str(len(payload)),
                           CONTENT_TYPE=self.headers.get("Content-Type", ""),
                           REMOTE_ADDR="127.0.0.1", HTTP_GIT_PROTOCOL=self.headers.get("Git-Protocol", ""))
                result = subprocess.run(["git", "http-backend"], input=payload, env=env,
                                        capture_output=True, check=True)
                head, body = result.stdout.split(b"\r\n\r\n", 1)
                headers = [line.decode().split(": ", 1) for line in head.split(b"\r\n")]
                status = next((int(value.split()[0]) for name, value in headers if name == "Status"), 200)
                self.respond(status, [(k, v) for k, v in headers if k != "Status"], body)

            do_POST = do_GET

        credentials = "proxy-user:repo-proxy-secret"
        authorization = "Basic " + base64.b64encode(credentials.encode()).decode()

        class Proxy(Handler):
            def authorized(self):
                record(self.server.kind, self.command, self.path)
                if self.headers.get("Proxy-Authorization") == authorization:
                    return True
                self.respond(407, [("Proxy-Authenticate", 'Basic realm="fixture"')], b"")
                return False

            def do_GET(self):
                if not self.authorized():
                    return
                parsed = urlsplit(self.path)
                assert parsed.scheme == "http"
                assert parsed.hostname in ("repository.invalid", "127.0.0.1", "localhost")
                connection = http.client.HTTPConnection("127.0.0.1", parsed.port, timeout=10)
                payload = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                headers = {k: v for k, v in self.headers.items()
                           if k.lower() not in ("proxy-authorization", "proxy-connection")}
                connection.request(self.command, parsed.path + ("?" + parsed.query if parsed.query else ""),
                                   body=payload, headers=headers)
                response = connection.getresponse()
                self.respond(response.status, response.getheaders(), response.read())
                connection.close()

            do_POST = do_GET

            def do_CONNECT(self):
                if not self.authorized():
                    return
                host, port = self.path.rsplit(":", 1)
                assert host in ("repository.invalid", "127.0.0.1", "localhost")
                with socket.create_connection(("127.0.0.1", int(port)), timeout=10) as upstream:
                    self.send_response(200)
                    self.end_headers()
                    self.wfile.flush()

                    def forward(source, destination):
                        try:
                            while data := source.recv(65536):
                                destination.sendall(data)
                        except (OSError, ssl.SSLError):
                            pass
                        finally:
                            try:
                                destination.shutdown(socket.SHUT_WR)
                            except OSError:
                                pass

                    worker = threading.Thread(target=forward, args=(self.connection, upstream), daemon=True)
                    worker.start()
                    forward(upstream, self.connection)
                    worker.join(timeout=10)

        servers = []

        def serve(handler, secure=False):
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
            server.daemon_threads = True
            server.kind = "proxy-https" if secure else "proxy-http"
            if secure:
                server.socket = tls.wrap_socket(server.socket, server_side=True)
            servers.append(server)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            return server.server_port

        origin_http, origin_https = serve(Origin), serve(Origin, True)
        proxy_http, proxy_https = serve(Proxy), serve(Proxy, True)
        # Reserve a port without listening, so failures cannot hit another server.
        with socket.socket() as refused:
            refused.bind(("127.0.0.1", 0))
            dead = f"http://127.0.0.1:{refused.getsockname()[1]}"
            fixture = {
                "root": str(root), "repo": str(repo), "log": str(log), "dead": dead,
                "http": f"http://repository.invalid:{origin_http}/repo",
                "https": f"https://repository.invalid:{origin_https}/repo",
                "direct_http": f"http://127.0.0.1:{origin_http}/repo",
                "direct_https": f"https://localhost:{origin_https}/repo",
                "proxy_http": f"http://{credentials}@127.0.0.1:{proxy_http}",
                "proxy_https": f"https://{credentials}@localhost:{proxy_https}",
            }
            config = root / "gitconfig"
            config.write_text(f'[http]\n proxy = {dead}\n[remote "origin"]\n proxy = {dead}\n'
                              + "".join(f'[http "{fixture[key]}"]\n proxy = {dead}\n'
                                        for key in ("http", "https", "direct_http", "direct_https")))
            # Hostile inherited proxy settings exercise command-level precedence.
            env = dict(os.environ, PIER_REPOSITORY_PROXY_FIXTURE=json.dumps(fixture),
                       GIT_CONFIG_GLOBAL=str(config), GIT_CONFIG_NOSYSTEM="1",
                       GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="remote.origin.proxy", GIT_CONFIG_VALUE_0=dead,
                       GIT_CONFIG_PARAMETERS=f"'remote.origin.proxy={dead}'",
                       GIT_SSL_CAINFO=str(root / "cert.pem"), GIT_PROXY_SSL_CAINFO=str(root / "cert.pem"),
                       GIT_SSH_COMMAND=f"sh {root / 'ssh.sh'}", GIT_SSH_VARIANT="ssh")
            # A local SSH transport double runs Git's actual upload-pack protocol.
            (root / "ssh.sh").write_text('exec git upload-pack "$PIER_SSH_REPO"\n')
            env["PIER_SSH_REPO"] = str(repo)
            for key in ("http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
                env[key] = dead
            try:
                for bypass in ("ambient.invalid", "*"):
                    env["no_proxy"] = env["NO_PROXY"] = bypass
                    result = subprocess.run(
                        [sys.argv[1], "--exact", "catalog::proxy_tests::sync_proxy_child", "--ignored", "--nocapture"],
                        env=env, timeout=90,
                    )
                    if result.returncode:
                        return result.returncode
                return 0
            finally:
                for server in servers:
                    server.shutdown()
                    server.server_close()


if __name__ == "__main__":
    sys.exit(main())
