"""Exercise release platform boundaries without Docker or GitHub credentials."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import native_packages as native
import release

spec = importlib.util.spec_from_file_location('agent_bundle', Path(__file__).with_name('bundle-agent-packages.py'))
agent_bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(agent_bundle)


class NativePackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / 'packages'
        self.source.mkdir()
        meta = dict(agent_version='1.2.3', controller_version='1.2.3', revision=2)
        self.identities = {name: identity for name, identity in release.expected_packages(meta).items()
                           if identity[0] == 'pier-agent'}
        for name in self.identities:
            (self.source / name).write_bytes(name.encode())

    def query(self, command, **kwargs):
        if command[:2] == ['docker', 'info']:
            return 'x86_64\n'
        name = Path(command[-1]).name
        system = native.filename_system(Path(name))
        self.assertIn(native.tool_image(system, 'amd64'), command)
        return '\n'.join(self.identities[name])

    def bundle(self):
        with patch.object(agent_bundle.subprocess, 'run'), \
                patch.object(agent_bundle.subprocess, 'check_output', side_effect=self.query):
            agent_bundle.bundle(self.source, self.root / 'bundle')
        return json.loads((self.root / 'bundle/manifest.json').read_text())

    def test_complete_bundle_preserves_distribution_and_revision(self):
        manifest = self.bundle()
        self.assertEqual(manifest['schema'], 1)
        self.assertEqual(len(manifest['releases']), 6)
        self.assertEqual({(v['system'], v['architecture']) for v in manifest['releases']},
                         {(s, a) for s in native.SYSTEMS for a in native.ARCHITECTURES})
        for entry in manifest['releases']:
            self.assertEqual(entry['package'], {'version': '1.2.3', 'revision': 2})
            data = self.root / 'bundle' / (entry['sha256'] + '.' + entry['format'])
            self.assertEqual(data.stat().st_size, entry['size'])

    def test_missing_or_extra_agent_package_fails_before_docker(self):
        name = next(iter(self.identities))
        (self.source / name).unlink()
        with patch.object(agent_bundle.subprocess, 'check_output') as docker:
            with self.assertRaisesRegex(ValueError, 'exactly six'):
                agent_bundle.bundle(self.source, self.root / 'bundle')
            docker.assert_not_called()
        (self.source / name).write_bytes(b'package')
        (self.source / 'pier-agent_1.2.3-1_amd64.deb').write_bytes(b'legacy test fixture')
        with self.assertRaisesRegex(ValueError, 'exactly six'):
            self.bundle()

    def test_duplicate_native_platform_is_rejected(self):
        name = 'pier-agent_1.2.3-2.ubuntu24.04_arm64.deb'
        self.identities[name] = ('pier-agent', '1.2.3-2.ubuntu24.04', 'amd64')
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            self.bundle()

    def test_mixed_package_revisions_are_rejected(self):
        name = 'pier-agent-1.2.3-2.el9.aarch64.rpm'
        self.identities[name] = ('pier-agent', '1.2.3-3.el9', 'aarch64', '0')
        with self.assertRaisesRegex(ValueError, 'same version and revision'):
            self.bundle()

    def test_el8_filename_cannot_hide_el9_native_metadata(self):
        name = 'pier-agent-1.2.3-2.el8.x86_64.rpm'
        self.identities[name] = ('pier-agent', '1.2.3-2.el9', 'x86_64', '0')
        with self.assertRaisesRegex(ValueError, 'unsupported package'):
            self.bundle()

    def test_native_versions_require_exact_distribution_and_format(self):
        for system, (fmt, suffix, _) in native.SYSTEMS.items():
            self.assertEqual(native.parse_version('1.2.3-12' + suffix, fmt),
                             ({'version': '1.2.3', 'revision': 12}, system))
        for version, fmt in [
            ('1.2.3-1', 'deb'), ('1.2.3-1.ubuntu22.04', 'deb'),
            ('1.2.3-1.el9', 'deb'), ('1.2.3-1.ubuntu24.04', 'rpm'),
            ('1.2.3-1.el10', 'rpm'), ('1.2.3-01.el9', 'rpm'),
            ('1:1.2.3-1.el9', 'rpm'), ('1.2.3-0.el9', 'rpm'),
            ('1.2.3-18446744073709551616.el9', 'rpm'),
        ]:
            with self.subTest(version=version, fmt=fmt), self.assertRaises(ValueError):
                native.parse_version(version, fmt)

    def test_rpm_inspection_uses_matching_tools(self):
        for system in ('almalinux8', 'almalinux9'):
            dist = native.SYSTEMS[system][1]
            package = Path('pier-controller-1.2.3-2%s.x86_64.rpm' % dist)
            with patch.object(release.subprocess, 'check_output', return_value=b'') as docker:
                release.package_command(package, ['rpm', '-qp', '/package'], 'amd64')
                self.assertIn(native.tool_image(system, 'amd64'), docker.call_args.args[0])


if __name__ == '__main__':
    unittest.main()
