#!/usr/bin/env python3
"""Shared service version, release allocation and native-package publication (Python 3.11+)."""
import argparse
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

from native_packages import ARCHITECTURES, SYSTEMS, build_matrix, filename_system, tool_image

ROOT = Path(__file__).resolve().parent.parent
NUMBER = r'(?:0|[1-9][0-9]*)'
VERSION = rf'{NUMBER}\.{NUMBER}\.{NUMBER}'
TAG = re.compile(rf'v({VERSION})(?:-r([1-9][0-9]*))?')
MAX_U64 = 2**64 - 1


def service_version(root):
    with (root / 'Cargo.toml').open('rb') as source:
        value = tomllib.load(source)['workspace']['package']['version']
    if (not isinstance(value, str) or len(value) > 64 or not re.fullmatch(VERSION, value)
            or any(int(part) > MAX_U64 for part in value.split('.'))):
        raise ValueError('workspace.package.version must be a numeric X.Y.Z native package version')
    for service in ('agent', 'controller'):
        with (root / 'crates' / ('pier-' + service) / 'Cargo.toml').open('rb') as source:
            inherited = tomllib.load(source)['package']['version']
        if not isinstance(inherited, dict) or inherited.get('workspace') is not True:
            raise ValueError('pier-' + service + ' must inherit the workspace version')
    return value


def metadata(root, tag='', revision='1'):
    version = service_version(root)
    if tag:
        match = TAG.fullmatch(tag)
        if match is None:
            raise ValueError('tag must be vX.Y.Z or vX.Y.Z-rN')
        if version != match[1]:
            raise ValueError('tag version must match the shared Cargo version')
        revision = int(match[2] or '0') + 1
    # Tests also need revision + 1 (upgrade) and revision + 2 (invalid metadata).
    if not re.fullmatch(r'[1-9][0-9]*', str(revision)) or not 1 <= int(revision) <= MAX_U64 - 2:
        raise ValueError('revision must be a positive integer with room for two test revisions')
    revision = int(revision)
    tag = 'v' + version + (f'-r{revision - 1}' if revision > 1 else '')
    return dict(agent_version=version, controller_version=version, release_tag=tag,
                revision=revision, upgrade_revision=revision + 1)


def github_tags(repository):
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository):
        raise ValueError('--repo (or GH_REPO) must be OWNER/REPO')
    tags = set()
    # Release history includes drafts; standalone Git tags also reserve a name.
    # --slurp preserves page boundaries so API errors cannot look like empty history.
    for resource, key in (('releases', 'tag_name'), ('tags', 'name')):
        pages = json.loads(subprocess.check_output([
            'gh', 'api', f'repos/{repository}/{resource}?per_page=100',
            '--method', 'GET', '--paginate', '--slurp',
        ], text=True))
        if not isinstance(pages, list) or not pages:
            raise ValueError('invalid paginated GitHub ' + resource + ' response')
        for page in pages:
            if not isinstance(page, list):
                raise ValueError('invalid GitHub ' + resource + ' page')
            for entry in page:
                if not isinstance(entry, dict) or not isinstance(entry.get(key), str) or not entry[key]:
                    raise ValueError('missing tag in GitHub ' + resource + ' response')
                tags.add(entry[key])
    return tags


def resolve(root, repository):
    version = service_version(root)
    latest_suffix = -1
    for tag in github_tags(repository):
        match = TAG.fullmatch(tag)
        if match and match[1] == version:
            latest_suffix = max(latest_suffix, int(match[2] or '0'))
    return metadata(root, revision=latest_suffix + 2)


def expected_packages(meta):
    result = {}
    for service in ('agent', 'controller'):
        name = 'pier-' + service
        version = meta[service + '_version']
        revision = meta['revision']
        for arch, rpm_arch in ARCHITECTURES.items():
            for fmt, suffix, _ in SYSTEMS.values():
                native = f'{version}-{revision}{suffix}'
                if fmt == 'deb':
                    result[f'{name}_{native}_{arch}.deb'] = (name, native, arch)
                else:
                    result[f'{name}-{native}.{rpm_arch}.rpm'] = (name, native, rpm_arch, '0')
    return result


def package_command(package, command, host):
    return subprocess.check_output([
        'docker', 'run', '--rm', '--pull=never', '--network=none',
        '--platform', 'linux/' + host, '-v', str(package.resolve()) + ':/package:ro',
        tool_image(filename_system(package), host), *command,
    ])


def verify_controller(package, expected_identity, expected_manifest, host):
    if package.suffix == '.deb':
        query = ['dpkg-deb', '-W', '--showformat=${Package}\n${Version}\n${Architecture}', '/package']
        archive = ['dpkg-deb', '--fsys-tarfile', '/package']
    else:
        query = ['rpm', '-qp', '--qf', '%{NAME}\n%{VERSION}-%{RELEASE}\n%{ARCH}\n%{EPOCHNUM}', '/package']
        archive = ['bash', '-euo', 'pipefail', '-c',
                   'mkdir /tmp/payload; cd /tmp/payload; rpm2cpio /package | cpio -idmu --quiet; '
                   'tar -cf - ./usr/share/pier-controller/agent-releases']
    actual = tuple(package_command(package, query, host).decode().strip().splitlines())
    if actual != expected_identity:
        raise ValueError('incorrect controller package identity: ' + package.name)
    data = package_command(package, archive, host)
    prefix = 'usr/share/pier-controller/agent-releases/'
    contents = {}
    with tarfile.open(fileobj=io.BytesIO(data)) as payload:
        for member in payload:
            name = member.name.removeprefix('./')
            if name.startswith(prefix) and not member.isdir():
                relative = name[len(prefix):]
                if not member.isfile() or relative in contents:
                    raise ValueError('invalid embedded bundle member: ' + name)
                contents[relative] = payload.extractfile(member).read()
    if json.loads(contents.pop('manifest.json')) != expected_manifest:
        raise ValueError('embedded agent manifest differs from release packages: ' + package.name)
    expected_files = {entry['sha256'] + '.' + entry['format']: entry
                      for entry in expected_manifest['releases']}
    if contents.keys() != expected_files.keys():
        raise ValueError('embedded agent package set is incomplete: ' + package.name)
    for name, content in contents.items():
        entry = expected_files[name]
        if len(content) != entry['size'] or hashlib.sha256(content).hexdigest() != entry['sha256']:
            raise ValueError('embedded agent package differs from release: ' + name)


def assemble(source, destination, meta):
    expected = expected_packages(meta)
    if {file.name for file in source.iterdir()} != expected.keys():
        raise ValueError('release input must contain exactly the twelve expected DEB/RPM packages')
    for file in source.iterdir():
        if file.is_symlink() or not file.is_file() or file.stat().st_size == 0:
            raise ValueError('invalid release input: ' + file.name)
    if destination.exists() and any(destination.iterdir()):
        raise ValueError('release output directory must be empty')
    spec = importlib.util.spec_from_file_location('agent_bundle', ROOT / 'scripts/bundle-agent-packages.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    with tempfile.TemporaryDirectory(prefix='pier-release-') as temporary:
        bundle = Path(temporary) / 'bundle'
        module.bundle(source, bundle)
        manifest = json.loads((bundle / 'manifest.json').read_text())
        expected_agent = {'version': meta['agent_version'], 'revision': meta['revision']}
        if any(entry['package'] != expected_agent for entry in manifest['releases']):
            raise ValueError('agent native metadata differs from release version')
        by_digest = {entry['sha256']: entry for entry in manifest['releases']}
        for name, identity in expected.items():
            if identity[0] != 'pier-agent':
                continue
            entry = by_digest[hashlib.sha256((source / name).read_bytes()).hexdigest()]
            arch = {'x86_64': 'amd64', 'aarch64': 'arm64'}.get(identity[2], identity[2])
            if (entry['architecture'] != arch or entry['format'] != Path(name).suffix[1:]
                    or entry['system'] != filename_system(Path(name))):
                raise ValueError('agent filename does not match its native platform: ' + name)
        host = subprocess.check_output(['docker', 'info', '--format', '{{.Architecture}}'], text=True).strip()
        host = {'x86_64': 'amd64', 'aarch64': 'arm64', 'amd64': 'amd64', 'arm64': 'arm64'}[host]
        for name, identity in expected.items():
            if identity[0] == 'pier-controller':
                verify_controller(source / name, identity, manifest, host)
    destination.mkdir(parents=True, exist_ok=True)
    checksums = []
    for name in sorted(expected):
        target = destination / name
        shutil.copyfile(source / name, target)
        target.chmod(0o644)
        checksums.append(hashlib.sha256(target.read_bytes()).hexdigest() + '  ' + name + '\n')
    (destination / 'SHA256SUMS').write_text(''.join(checksums))


def publish(source, meta, repository, commit):
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('publish requires the full triggering commit SHA')
    source = source.resolve()
    names = sorted(expected_packages(meta))
    if {file.name for file in source.iterdir()} != set(names) | {'SHA256SUMS'}:
        raise ValueError('publish input must contain exactly twelve packages and SHA256SUMS')
    for name in [*names, 'SHA256SUMS']:
        file = source / name
        if file.is_symlink() or not file.is_file() or file.stat().st_size == 0:
            raise ValueError('invalid publish input: ' + name)
    checksums = []
    for name in names:
        with (source / name).open('rb') as package:
            checksums.append(hashlib.file_digest(package, 'sha256').hexdigest() + '  ' + name + '\n')
    if (source / 'SHA256SUMS').read_text() != ''.join(checksums):
        raise ValueError('release package checksums do not match SHA256SUMS')

    tag = meta['release_tag']
    if tag in github_tags(repository):
        raise ValueError('tag or Release already exists; rerun all jobs to allocate a new revision')
    # Create the ref atomically: a competing tag creation must fail, never attach
    # these packages to somebody else's commit. A failed release keeps this ref.
    subprocess.run([
        'gh', 'api', f'repos/{repository}/git/refs', '--method', 'POST',
        '-f', 'ref=refs/tags/' + tag, '-f', 'sha=' + commit, '--silent',
    ], check=True)
    subprocess.run([
        'gh', 'release', 'create', tag, '--repo', repository,
        *[str(source / name) for name in names], str(source / 'SHA256SUMS'),
        '--verify-tag', '--target', commit, '--draft', '--prerelease=false',
        '--title', tag, '--generate-notes',
    ], check=True)
    # gh create uploads every attachment before returning; failures leave a draft.
    subprocess.run([
        'gh', 'release', 'edit', tag, '--repo', repository,
        '--draft=false', '--prerelease=false', '--latest',
    ], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['version', 'matrix', 'metadata', 'resolve', 'assemble', 'publish'])
    version_args = parser.add_mutually_exclusive_group()
    version_args.add_argument('--tag', default='')
    version_args.add_argument('--revision', default='1')
    parser.add_argument('--repo', default=os.environ.get('GH_REPO', ''))
    parser.add_argument('--commit', default='')
    parser.add_argument('--input', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    try:
        if args.command == 'matrix':
            value = json.dumps(build_matrix(), separators=(',', ':'))
            print(value)
            if os.environ.get('GITHUB_OUTPUT'):
                with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
                    output.write('matrix=' + value + '\n')
            return
        if args.command == 'version':
            print(service_version(ROOT))
            return
        meta = (resolve(ROOT, args.repo) if args.command == 'resolve'
                else metadata(ROOT, args.tag, args.revision))
        if args.command in ('metadata', 'resolve'):
            print(json.dumps(meta))
            if os.environ.get('GITHUB_OUTPUT'):
                with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
                    output.writelines(f'{key}={value}\n' for key, value in meta.items())
        elif args.command == 'assemble':
            if args.input is None or args.output is None:
                parser.error('assemble requires --input and --output')
            assemble(args.input, args.output, meta)
        else:
            if not args.tag or args.input is None or not args.commit:
                parser.error('publish requires --tag, --input and --commit')
            publish(args.input, meta, args.repo, args.commit)
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'release: {error}\n')


if __name__ == '__main__':
    main()
