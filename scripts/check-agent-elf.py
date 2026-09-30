#!/usr/bin/env python3
"""Validate architecture and the distro ABI before publishing a native package."""
import re
import subprocess
import sys
binary, arch, system = sys.argv[1:]
header = subprocess.check_output(['readelf', '-h', binary], universal_newlines=True)
expected = {'amd64': 'Advanced Micro Devices X86-64', 'arm64': 'AArch64'}[arch]
if expected not in header:
    sys.exit('ELF architecture does not match the requested package architecture')
versions = subprocess.check_output(['readelf', '--version-info', binary], universal_newlines=True)
limit = {'almalinux8': (2, 28), 'almalinux9': (2, 34), 'ubuntu24.04': (2, 39)}[system]
for version in re.findall(r'\bGLIBC_(\d+(?:\.\d+)+)\b', versions):
    if tuple(map(int, version.split('.'))) > limit:
        sys.exit('ELF requires GLIBC_%s; %s requires <= %s. Use a Rust builder matching the requested target system.' % (version, system, '.'.join(map(str, limit))))
