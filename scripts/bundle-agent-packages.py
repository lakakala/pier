#!/usr/bin/env python3
"""Validate six native packages using Docker and prepare an immutable bundle."""
import hashlib
import json
import pathlib
import shutil
import subprocess
import sys

from native_packages import ARCHITECTURES, SYSTEMS, filename_system, parse_version, tool_image


def bundle(source, destination):
    source, destination = source.resolve(), destination.resolve()
    packages = sorted([*source.glob('pier-agent_*.deb'), *source.glob('pier-agent-*.rpm')])
    expected_platforms = {(system, arch) for system in SYSTEMS for arch in ARCHITECTURES}
    if len(packages) != len(expected_platforms):
        raise ValueError('exactly six pier-agent packages are required (three OSes and both architectures)')
    root = pathlib.Path(__file__).resolve().parent.parent
    host = subprocess.check_output(['docker', 'info', '--format', '{{.Architecture}}'], text=True).strip()
    architecture = {'x86_64': 'amd64', 'aarch64': 'arm64', 'amd64': 'amd64', 'arm64': 'arm64'}[host]
    for system, (_, _, packager) in SYSTEMS.items():
        subprocess.run(['docker', 'build', '--platform', 'linux/' + architecture,
                        '-f', str(root / ('docker/agent-' + packager + '.Dockerfile')),
                        '-t', tool_image(system, architecture),
                        '--build-arg', 'http_proxy', '--build-arg', 'https_proxy',
                        '--build-arg', 'no_proxy', '--build-arg', 'NO_PROXY', str(root)], check=True)
    releases, platforms = [], set()
    for package in packages:
        if package.is_symlink() or not package.is_file() or not 0 < package.stat().st_size <= 256 * 1024 * 1024:
            raise ValueError('invalid package file: ' + package.name)
        fmt = package.suffix[1:]
        expected_system = filename_system(package)
        query = (['dpkg-deb', '-W', '--showformat=${Package}\n${Version}\n${Architecture}', '/packages/' + package.name]
                 if fmt == 'deb' else ['rpm', '-qp', '--qf', '%{NAME}\n%{VERSION}-%{RELEASE}\n%{ARCH}\n%{EPOCHNUM}', '/packages/' + package.name])
        fields = subprocess.check_output(['docker', 'run', '--rm', '--pull=never', '--network=none',
                                          '--platform', 'linux/' + architecture, '-v', str(source) + ':/packages:ro',
                                          tool_image(expected_system, architecture), *query], text=True, timeout=120).strip().splitlines()
        if len(fields) != (3 if fmt == 'deb' else 4) or fields[0] != 'pier-agent' or (fmt == 'rpm' and fields[3] != '0'):
            raise ValueError('incorrect package identity or epoch: ' + package.name)
        version, system = parse_version(fields[1], fmt)
        arch = ({'amd64': 'amd64', 'arm64': 'arm64'} if fmt == 'deb' else {'x86_64': 'amd64', 'aarch64': 'arm64'}).get(fields[2])
        if arch is None or system != expected_system:
            raise ValueError('unsupported package version or architecture: ' + package.name)
        if (system, arch) in platforms:
            raise ValueError('duplicate agent platform')
        platforms.add((system, arch))
        digest = hashlib.sha256(package.read_bytes()).hexdigest()
        releases.append({'package': version, 'format': fmt,
                         'architecture': arch, 'system': system, 'sha256': digest, 'size': package.stat().st_size})
    if platforms != expected_platforms:
        raise ValueError('incomplete agent platform set')
    if any(r['package'] != releases[0]['package'] for r in releases):
        raise ValueError('all six agent packages must have the same version and revision')
    destination.mkdir(parents=True, exist_ok=True)
    destination.chmod(0o755)
    if any(destination.iterdir()):
        raise ValueError('bundle output must be empty')
    for package, release in zip(packages, releases):
        target = destination / (release['sha256'] + '.' + release['format'])
        shutil.copyfile(package, target)
        target.chmod(0o644)
        if hashlib.sha256(target.read_bytes()).hexdigest() != release['sha256']:
            raise ValueError('package changed during bundle preparation')
    (destination / 'manifest.json').write_text(json.dumps({'schema': 1, 'releases': releases}, indent=2) + '\n')
    (destination / 'manifest.json').chmod(0o644)


if __name__ == '__main__':
    if len(sys.argv) != 3:
        sys.exit('Usage: python3 scripts/bundle-agent-packages.py PACKAGE_DIR EMPTY_BUNDLE_DIR')
    bundle(pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]))
