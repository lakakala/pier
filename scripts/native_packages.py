"""Native distribution identities used by the Actions release and bundle checks."""
import re

SYSTEMS = {
    'ubuntu24.04': ('deb', '.ubuntu24.04', 'deb'),
    'almalinux8': ('rpm', '.el8', 'rpm'),
    'almalinux9': ('rpm', '.el9', 'rpm-almalinux9'),
}
ARCHITECTURES = {'amd64': 'x86_64', 'arm64': 'aarch64'}
NUMBER = r'(?:0|[1-9][0-9]*)'
NATIVE_VERSION = re.compile(rf'({NUMBER}\.{NUMBER}\.{NUMBER})-([1-9][0-9]*)(\.ubuntu24\.04|\.el8|\.el9)')


def build_matrix():
    """One native build per release system and architecture on GitHub runners."""
    return {'include': [
        {'system': system, 'arch': arch, 'format': target[0],
         'runner': 'ubuntu-24.04' if arch == 'amd64' else 'ubuntu-24.04-arm'}
        for system, target in SYSTEMS.items() for arch in ARCHITECTURES
    ]}


def parse_version(value, fmt):
    match = NATIVE_VERSION.fullmatch(value)
    if match is None:
        raise ValueError('unsupported native package version')
    version, revision, suffix = match.groups()
    if len(version) > 64 or any(int(v) > 2**64 - 1 for v in [*version.split('.'), revision]):
        raise ValueError('package version is out of range')
    system = next(system for system, target in SYSTEMS.items() if target[1] == suffix)
    if SYSTEMS[system][0] != fmt:
        raise ValueError('package format and system differ')
    return {'version': version, 'revision': int(revision)}, system


def filename_system(package):
    # Used only to select inspection tools. Native metadata is checked separately.
    for system, (fmt, suffix, _) in SYSTEMS.items():
        separator = '_' if fmt == 'deb' else '.'
        if re.search(re.escape(suffix + separator) + r'[^.]+\.' + fmt + r'$', package.name):
            return system
    raise ValueError('unsupported native package filename: ' + package.name)


def tool_image(system, architecture):
    return 'pier-agent-package-' + SYSTEMS[system][2] + ':' + architecture
