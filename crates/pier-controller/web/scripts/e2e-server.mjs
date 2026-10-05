import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { execFileSync, spawn } from 'node:child_process';
import http from 'node:http';
import https from 'node:https';
const scheme = process.env.PIER_E2E_SCHEME ?? 'https';
if (!['http', 'https'].includes(scheme)) throw new Error('PIER_E2E_SCHEME must be http or https');
const root = resolve(import.meta.dirname, '../../../../');
const temporary = mkdtempSync(join(tmpdir(), 'pier-web-e2e-'));
const repository = join(temporary, 'repo');
writeFileSync(join(root, 'target/web-e2e-repository.txt'), repository);
mkdirSync(join(repository, 'apps/demo'), { recursive: true });
mkdirSync(join(repository, 'blueprints/web'), { recursive: true });
writeFileSync(
  join(repository, 'apps/demo/pier-pkg.yml'),
  `schema: 2
name: demo
version: "1.0.0"
variables:
  SECRET: {}
  PORT: {default: "8080"}
source: {type: git, repo: "https://example.invalid/demo.git", ref: main}
build: {language: rust, commands: ["true"]}
files: [{from: demo, to: bin/demo, executable: true}]
service: {command: [bin/demo]}
`,
);
writeFileSync(
  join(repository, 'blueprints/web/pier-blueprint.yml'),
  `schema: 1
name: web-server
variables:
  SECRET: {}
  PORT: {default: "8080"}
apps:
  - id: api
    app: apps/demo
    variables: {SECRET: "{{ SECRET }}", PORT: "{{ PORT }}"}
`,
);
for (const args of [
  ['init', '-q', '-b', 'main'],
  ['add', '.'],
  ['-c', 'user.name=Pier tests', '-c', 'user.email=tests@localhost', 'commit', '-qm', 'fixture'],
])
  execFileSync('git', args, { cwd: repository });
writeFileSync(
  join(temporary, 'controller.yml'),
  `state_dir: ${temporary}/state
http_listen: 127.0.0.1:18084
`,
);
if (scheme === 'https') {
  execFileSync(
    'openssl',
    [
      'req',
      '-x509',
      '-newkey',
      'rsa:2048',
      '-nodes',
      '-days',
      '1',
      '-subj',
      '/CN=localhost',
      '-addext',
      'subjectAltName=DNS:localhost',
      '-keyout',
      join(temporary, 'key.pem'),
      '-out',
      join(temporary, 'cert.pem'),
    ],
    { stdio: 'ignore' },
  );
}
const controller = spawn(
  join(root, 'target/debug/pier-controller'),
  ['--config', join(temporary, 'controller.yml')],
  { stdio: ['ignore', 'inherit', 'inherit'] },
);
let proxy;
if (scheme === 'https') {
  proxy = https.createServer(
    {
      key: readFileSync(join(temporary, 'key.pem')),
      cert: readFileSync(join(temporary, 'cert.pem')),
    },
    (req, res) => {
      const upstream = http.request(
        { host: '127.0.0.1', port: 18084, path: req.url, method: req.method, headers: req.headers },
        (response) => {
          res.writeHead(response.statusCode, response.headers);
          response.pipe(res);
        },
      );
      upstream.on('error', () => {
        res.writeHead(502);
        res.end();
      });
      req.pipe(upstream);
    },
  );
  proxy.listen(8444, '127.0.0.1');
  proxy.on('upgrade', (req, socket, head) => {
    const upstream = http.request({
      host: '127.0.0.1',
      port: 18084,
      path: req.url,
      method: req.method,
      headers: req.headers,
    });
    upstream.on('upgrade', (response, peer, upstreamHead) => {
      socket.write(
        `HTTP/1.1 ${response.statusCode} ${response.statusMessage}\r\n` +
          Object.entries(response.headers)
            .map(([key, value]) => `${key}: ${value}\r\n`)
            .join('') +
          '\r\n',
      );
      if (head.length) peer.write(head);
      if (upstreamHead.length) socket.write(upstreamHead);
      socket.pipe(peer).pipe(socket);
      socket.on('error', () => peer.destroy());
      peer.on('error', () => socket.destroy());
      socket.on('close', () => peer.destroy());
      peer.on('close', () => socket.destroy());
    });
    upstream.on('response', (response) => {
      response.resume();
      socket.end(`HTTP/1.1 ${response.statusCode} Rejected\r\nConnection: close\r\n\r\n`);
    });
    upstream.on('error', () => socket.destroy());
    upstream.end();
  });
}
let stopping = false;
function stop() {
  if (stopping) return;
  stopping = true;
  proxy?.close();
  controller.kill('SIGTERM');
}
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
controller.on('exit', (code) => {
  proxy?.close();
  rmSync(temporary, { recursive: true, force: true });
  rmSync(join(root, 'target/web-e2e-repository.txt'), { force: true });
  process.exit(stopping ? 0 : code || 1);
});
