"""Check the Actions target matrix and script routing without compiling packages."""
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import native_packages
import release

ROOT = Path(__file__).resolve().parent.parent


class PackageJobTests(unittest.TestCase):
    def test_matrix_covers_each_supported_native_platform_once(self):
        jobs = native_packages.build_matrix()['include']
        platforms = {(job['system'], job['arch']) for job in jobs}
        self.assertEqual(len(jobs), 6)
        self.assertEqual(platforms, {(s, a) for s in ('ubuntu24.04', 'almalinux8', 'almalinux9')
                                     for a in ('amd64', 'arm64')})
        for job in jobs:
            self.assertEqual(job['runner'], {'amd64': 'ubuntu-24.04', 'arm64': 'ubuntu-24.04-arm'}[job['arch']])
            self.assertEqual(job['format'], 'deb' if job['system'] == 'ubuntu24.04' else 'rpm')

    def test_matrix_output_is_json_for_github_actions(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'github-output'
            with patch.dict(os.environ, {'GITHUB_OUTPUT': str(output)}), \
                    patch('sys.argv', ['release.py', 'matrix']), \
                    patch('sys.stdout', new_callable=io.StringIO) as stdout:
                release.main()
            self.assertEqual(json.loads(stdout.getvalue()), native_packages.build_matrix())
            key, value = output.read_text().strip().split('=', 1)
            self.assertEqual(key, 'matrix')
            self.assertEqual(json.loads(value), native_packages.build_matrix())

    def test_each_system_uses_its_own_builder_with_the_same_rust_version(self):
        versions, images = set(), set()
        for system, base in [('ubuntu24.04', 'ubuntu:24.04'), ('almalinux8', 'almalinux:8.10'),
                             ('almalinux9', 'almalinux:9.8')]:
            output = subprocess.check_output([
                'bash', '-eu', '-c',
                'source scripts/package-target.sh; pier_package_target "$1"; '
                'printf "%s\\n" "$pier_builder_dockerfile" "$pier_builder_image" "$pier_package"',
                'target-test', system,
            ], cwd=ROOT, text=True).splitlines()
            dockerfile, image, fmt = output
            content = (ROOT / dockerfile).read_text()
            self.assertIn('FROM ' + base + '\n', content)
            self.assertEqual(fmt, native_packages.SYSTEMS[system][0])
            versions.add(re.search(r'^ARG RUST_VERSION=(.+)$', content, re.M)[1])
            images.add(image)
        self.assertEqual(len(images), 3)
        self.assertEqual(len(versions), 1)

    def test_missing_and_unsupported_systems_fail_before_docker(self):
        scripts = [
            ['package-agent.sh', '--image', 'unused', '--arch', 'amd64'],
            ['package-controller.sh', '--image', 'unused', '--arch', 'amd64'],
            ['prepare-upgrade-fixtures.sh', 'agent', 'amd64', '2', 'unused'],
            ['test-agent-auto-upgrade.sh', 'amd64'],
        ]
        with tempfile.TemporaryDirectory() as directory:
            # Even if Docker is installed on CI, these error paths must never call it.
            docker = Path(directory) / 'docker'
            marker = Path(directory) / 'called'
            docker.write_text('#!/bin/sh\ntouch "$PIER_DOCKER_MARKER"\nexit 99\n')
            docker.chmod(0o755)
            env = dict(os.environ, PATH=directory + os.pathsep + os.environ['PATH'], PIER_DOCKER_MARKER=str(marker))
            for script in scripts:
                for options in ([], ['--system', 'almalinux10']):
                    with self.subTest(script=script[0], options=options):
                        result = subprocess.run(['bash', str(ROOT / 'scripts' / script[0]), *script[1:], *options],
                                                env=env, capture_output=True, text=True)
                        self.assertEqual(result.returncode, 2, result.stderr)
                        self.assertIn('system', result.stderr.lower())
                        self.assertFalse(marker.exists())

    def test_upgrade_runner_selects_one_system_and_only_ubuntu_requires_legacy(self):
        for system, distro in [('ubuntu24.04', 'ubuntu2404'), ('almalinux8', 'almalinux8'),
                               ('almalinux9', 'almalinux9')]:
            with self.subTest(system=system), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / 'packages').mkdir()
                (root / 'fixtures/arm64').mkdir(parents=True)
                legacy = root / 'legacy'
                if system == 'ubuntu24.04':
                    legacy.mkdir()
                log = root / 'calls.jsonl'
                docker = root / 'docker'
                docker.write_text('#!/usr/bin/env python3\nimport json, os, sys\n'
                                  'with open(os.environ["PIER_DOCKER_LOG"], "a") as out:\n'
                                  '    out.write(json.dumps(sys.argv[1:]) + "\\n")\n'
                                  'if sys.argv[1] == "run": print("test-container")\n')
                docker.chmod(0o755)
                env = dict(os.environ, PATH=directory + os.pathsep + os.environ['PATH'], PIER_DOCKER_LOG=str(log))
                env.pop('GITHUB_OUTPUT', None)
                subprocess.run([
                    'bash', str(ROOT / 'scripts/test-agent-auto-upgrade.sh'), 'arm64', '--system', system,
                    '--packages', str(root / 'packages'), '--fixtures', str(root / 'fixtures'),
                    '--legacy-packages', str(legacy), '--logs', str(root / 'logs'),
                ], env=env, check=True, capture_output=True, text=True)
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                builds = [args for args in calls if args[0] == 'build']
                runs = [args for args in calls if args[0] == 'run']
                self.assertEqual(len(builds), 1)
                self.assertEqual(len(runs), 1)
                self.assertIn(str(ROOT / ('docker/controller-test-' + distro + '.Dockerfile')), builds[0])
                self.assertEqual(runs[0][-1], 'pier-controller-test-' + distro + ':arm64')
                self.assertIn('linux/arm64', runs[0])
                self.assertEqual('PIER_TEST_LEGACY_PACKAGES=/legacy-packages' in runs[0], system == 'ubuntu24.04')


if __name__ == '__main__':
    unittest.main()
