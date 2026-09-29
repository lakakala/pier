#!/usr/bin/env python3
"""Repack a CI agent DEB with the old numeric revision for migration testing.

The binary and maintainer scripts are unchanged; this fixture exercises the
native-version transition, configuration preservation and subsequent updates.
It must never be included in formal release artifacts.
"""
from pathlib import Path
import subprocess
import sys
import tempfile

from native_packages import parse_version


def prepare(package, destination):
    fields = subprocess.check_output([
        'dpkg-deb', '-W', '--showformat=${Package}\n${Version}\n${Architecture}', str(package),
    ], text=True).strip().splitlines()
    if len(fields) != 3 or fields[0] != 'pier-agent' or fields[2] not in ('amd64', 'arm64'):
        raise ValueError('expected an agent DEB')
    version, _ = parse_version(fields[1], 'deb')
    legacy = '%s-%s' % (version['version'], version['revision'])
    destination.mkdir(parents=True, exist_ok=True)
    output = destination / ('pier-agent_%s_%s.deb' % (legacy, fields[2]))
    if output.exists():
        raise ValueError('legacy fixture already exists')
    with tempfile.TemporaryDirectory() as temporary:
        subprocess.run(['dpkg-deb', '--raw-extract', str(package), temporary], check=True)
        control = Path(temporary) / 'DEBIAN/control'
        lines = control.read_text().splitlines()
        control.write_text('\n'.join('Version: ' + legacy if line.startswith('Version: ') else line
                                     for line in lines) + '\n')
        subprocess.run(['dpkg-deb', '--build', '--root-owner-group', temporary, str(output)], check=True)


if __name__ == '__main__':
    prepare(Path(sys.argv[1]), Path(sys.argv[2]))
