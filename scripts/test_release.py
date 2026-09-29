#!/usr/bin/env python3
"""Test the release boundaries without Docker or GitHub credentials."""
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import release


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / 'Cargo.toml').write_text('[workspace.package]\nversion = "1.2.3"\n')
        for service in ('agent', 'controller'):
            directory = self.root / 'crates' / ('pier-' + service)
            directory.mkdir(parents=True)
            (directory / 'Cargo.toml').write_text('[package]\nversion.workspace = true\n')

    def test_tag_versions_and_revisions(self):
        self.assertEqual(release.metadata(self.root, 'v1.2.3')['revision'], 1)
        self.assertEqual(release.metadata(self.root, 'v1.2.3-r1')['revision'], 2)
        self.assertEqual(release.metadata(self.root, 'v1.2.3-r7')['upgrade_revision'], 9)
        self.assertEqual(release.metadata(self.root, revision='12')['revision'], 12)
        self.assertEqual(release.metadata(self.root, revision='12')['release_tag'], 'v1.2.3-r11')

    def test_root_version_updates_both_services_and_packages(self):
        (self.root / 'Cargo.toml').write_text('[workspace.package]\nversion = "2.0.0"\n')
        meta = release.metadata(self.root)
        self.assertEqual(meta['agent_version'], '2.0.0')
        self.assertEqual(meta['controller_version'], '2.0.0')
        self.assertEqual(meta['release_tag'], 'v2.0.0')
        self.assertIn('pier-agent_2.0.0-1.ubuntu24.04_amd64.deb', release.expected_packages(meta))
        self.assertIn('pier-controller-2.0.0-1.el8.aarch64.rpm', release.expected_packages(meta))

    def test_reject_invalid_tags(self):
        for tag in ('v1.2.4', 'v1.2.3-rc1', 'v01.2.3', 'v1.2.3-r0', 'v1.2.3-r02',
                    'v1.2.3\nrevision=999', '1.2.3'):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.metadata(self.root, tag)

    def test_both_services_must_inherit_shared_version(self):
        for declaration in ('version = "1.2.3"', 'version = "1.2.4"',
                            'version.workspace = false', 'version.workspace = 1'):
            (self.root / 'crates/pier-controller/Cargo.toml').write_text('[package]\n' + declaration + '\n')
            with self.subTest(declaration=declaration), self.assertRaisesRegex(ValueError, 'inherit'):
                release.metadata(self.root)

    def test_invalid_revision_and_overflow(self):
        for value in ('0', '-1', '01', '1.5', '$(id)', '2\nother=1', str(2**64 - 2), str(2**64)):
            with self.subTest(revision=value), self.assertRaises(ValueError):
                release.metadata(self.root, revision=value)
        self.assertEqual(release.metadata(self.root, revision=str(2**64 - 3))['upgrade_revision'], 2**64 - 2)

    def test_invalid_cargo_version(self):
        for version in ('"1.2.3-beta"', '"01.2.3"', '"' + str(2**64) + '.0.0"', '123', 'true'):
            (self.root / 'Cargo.toml').write_text('[workspace.package]\nversion = ' + version + '\n')
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.metadata(self.root)

    def test_allocate_from_highest_existing_suffix(self):
        cases = [
            (set(), 'v1.2.3', 1),
            ({'v1.2.3'}, 'v1.2.3-r1', 2),
            ({'v1.2.3', 'v1.2.3-r1'}, 'v1.2.3-r2', 3),
            ({'v1.2.3-r3'}, 'v1.2.3-r4', 5),
            ({'v1.2.3-r9', 'v1.2.3-r10', 'v1.2.3-r2'}, 'v1.2.3-r11', 12),
            ({'v1.2.2-r50', 'v2.0.0', 'v1.2.3-rc1', 'v1.2.3-r0', 'v1.2.3-r01'}, 'v1.2.3', 1),
        ]
        for tags, tag, revision in cases:
            with self.subTest(tags=tags), patch.object(release, 'github_tags', return_value=tags):
                meta = release.resolve(self.root, 'owner/repo')
                self.assertEqual(meta['release_tag'], tag)
                self.assertEqual(meta['revision'], revision)
                self.assertEqual(meta['upgrade_revision'], revision + 1)

    def test_all_history_pages_include_drafts_and_standalone_tags(self):
        responses = [
            json.dumps([[{'tag_name': 'v1.2.3'}], [{'tag_name': 'v1.2.3-r8', 'draft': True}]]),
            json.dumps([[{'name': 'v1.2.3-r2'}], [{'name': 'v1.2.3-r10'}]]),
        ]
        with patch.object(release.subprocess, 'check_output', side_effect=responses) as gh:
            self.assertEqual(release.resolve(self.root, 'owner/repo')['release_tag'], 'v1.2.3-r11')
        commands = [call.args[0] for call in gh.call_args_list]
        self.assertIn('repos/owner/repo/releases?per_page=100', commands[0])
        self.assertIn('repos/owner/repo/tags?per_page=100', commands[1])
        for command in commands:
            self.assertIn('--paginate', command)
            self.assertIn('--slurp', command)

    def test_draft_alone_reserves_version(self):
        with patch.object(release.subprocess, 'check_output', side_effect=[
                '[[{"tag_name":"v1.2.3","draft":true}]]', '[[]]']):
            self.assertEqual(release.resolve(self.root, 'owner/repo')['release_tag'], 'v1.2.3-r1')

    def test_history_failure_never_falls_back_to_first_revision(self):
        for responses in ([subprocess.CalledProcessError(1, 'gh')],
                          ['[[]]', subprocess.CalledProcessError(1, 'gh')]):
            with self.subTest(responses=responses):
                with patch.object(release.subprocess, 'check_output', side_effect=responses):
                    with self.assertRaises(subprocess.CalledProcessError):
                        release.resolve(self.root, 'owner/repo')
        for response in ('not JSON', '{}', '[]', '[{}]', '[[{}]]', '[[{"tag_name":null}]]'):
            with self.subTest(response=response):
                with patch.object(release.subprocess, 'check_output', return_value=response):
                    with self.assertRaises(ValueError):
                        release.resolve(self.root, 'owner/repo')

    def test_invalid_repository_fails_before_query(self):
        for repository in ('', 'repo', 'owner/repo/extra', 'owner/repo?query=1'):
            with self.subTest(repository=repository), patch.object(release.subprocess, 'check_output') as gh:
                with self.assertRaises(ValueError):
                    release.resolve(self.root, repository)
                gh.assert_not_called()

    def test_history_revision_overflow(self):
        with patch.object(release, 'github_tags', return_value={'v1.2.3-r' + str(release.MAX_U64 - 4)}):
            self.assertEqual(release.resolve(self.root, 'owner/repo')['revision'], release.MAX_U64 - 2)
        with patch.object(release, 'github_tags', return_value={'v1.2.3-r' + str(release.MAX_U64 - 3)}):
            with self.assertRaisesRegex(ValueError, 'revision'):
                release.resolve(self.root, 'owner/repo')

    def test_resolve_writes_identical_metadata_for_all_jobs(self):
        output = self.root / 'github-output'
        with patch.object(release, 'ROOT', self.root), patch.object(release, 'github_tags', return_value={'v1.2.3'}):
            with patch.dict(os.environ, {'GH_REPO': 'owner/repo', 'GITHUB_OUTPUT': str(output)}):
                with patch('sys.argv', ['release.py', 'resolve']), patch('sys.stdout', new_callable=io.StringIO) as stdout:
                    release.main()
        meta = json.loads(stdout.getvalue())
        self.assertEqual(meta['release_tag'], 'v1.2.3-r1')
        self.assertEqual(meta['revision'], 2)
        self.assertEqual(dict(line.split('=', 1) for line in output.read_text().splitlines()),
                         {key: str(value) for key, value in meta.items()})

    def make_publish_assets(self, tag='v1.2.3-r1'):
        meta = release.metadata(self.root, tag)
        source = self.root / 'assets'
        source.mkdir()
        checksums = []
        for name in sorted(release.expected_packages(meta)):
            content = name.encode()
            (source / name).write_bytes(content)
            checksums.append(hashlib.sha256(content).hexdigest() + '  ' + name + '\n')
        (source / 'SHA256SUMS').write_text(''.join(checksums))
        return source, meta

    def test_publish_pins_commit_uploads_all_assets_then_marks_latest(self):
        source, meta = self.make_publish_assets()
        commit = 'a' * 40
        with patch.object(release, 'github_tags', return_value=set()), patch.object(release.subprocess, 'run') as gh:
            release.publish(source, meta, 'owner/repo', commit)
        create_tag, create_release, publish_release = [call.args[0] for call in gh.call_args_list]
        self.assertIn('ref=refs/tags/v1.2.3-r1', create_tag)
        self.assertIn('sha=' + commit, create_tag)
        self.assertIn('POST', create_tag)
        self.assertEqual(create_release[:4], ['gh', 'release', 'create', 'v1.2.3-r1'])
        self.assertEqual(create_release[create_release.index('--target') + 1], commit)
        self.assertIn('--verify-tag', create_release)
        self.assertIn('--draft', create_release)
        for name in [*release.expected_packages(meta), 'SHA256SUMS']:
            self.assertIn(str(source / name), create_release)
        self.assertEqual(publish_release[:4], ['gh', 'release', 'edit', 'v1.2.3-r1'])
        self.assertIn('--draft=false', publish_release)
        self.assertIn('--prerelease=false', publish_release)
        self.assertIn('--latest', publish_release)

    def test_publish_conflict_and_query_failure_never_mutate_remote(self):
        source, meta = self.make_publish_assets()
        with patch.object(release, 'github_tags', return_value={'v1.2.3-r1'}):
            with patch.object(release.subprocess, 'run') as gh:
                with self.assertRaisesRegex(ValueError, 'already exists'):
                    release.publish(source, meta, 'owner/repo', 'a' * 40)
                gh.assert_not_called()
        with patch.object(release, 'github_tags', side_effect=subprocess.CalledProcessError(1, 'gh')):
            with patch.object(release.subprocess, 'run') as gh:
                with self.assertRaises(subprocess.CalledProcessError):
                    release.publish(source, meta, 'owner/repo', 'a' * 40)
                gh.assert_not_called()

    def test_publish_failure_stops_without_deleting_tag_or_exposing_draft(self):
        source, meta = self.make_publish_assets()
        for failure_at in (0, 1, 2):
            effects = [None] * failure_at + [subprocess.CalledProcessError(1, 'gh')]
            with self.subTest(failure_at=failure_at), patch.object(release, 'github_tags', return_value=set()):
                with patch.object(release.subprocess, 'run', side_effect=effects) as gh:
                    with self.assertRaises(subprocess.CalledProcessError):
                        release.publish(source, meta, 'owner/repo', 'a' * 40)
                    self.assertEqual(gh.call_count, failure_at + 1)

    def test_publish_requires_commit_sha(self):
        source, meta = self.make_publish_assets()
        for commit in ('master', 'a' * 7, ''):
            with self.subTest(commit=commit), patch.object(release, 'github_tags') as gh:
                with self.assertRaisesRegex(ValueError, 'commit SHA'):
                    release.publish(source, meta, 'owner/repo', commit)
                gh.assert_not_called()

    def test_publish_rejects_modified_assets_before_any_remote_operation(self):
        source, meta = self.make_publish_assets()
        (source / next(iter(release.expected_packages(meta)))).write_bytes(b'changed')
        with patch.object(release, 'github_tags') as gh:
            with self.assertRaisesRegex(ValueError, 'checksums'):
                release.publish(source, meta, 'owner/repo', 'a' * 40)
            gh.assert_not_called()

    def test_publish_rejects_extra_or_missing_assets(self):
        source, meta = self.make_publish_assets()
        extra = source / 'test-fixture.deb'
        extra.write_bytes(b'not a formal package')
        with patch.object(release, 'github_tags') as gh:
            with self.assertRaisesRegex(ValueError, 'exactly twelve'):
                release.publish(source, meta, 'owner/repo', 'a' * 40)
            extra.unlink()
            (source / 'SHA256SUMS').unlink()
            with self.assertRaisesRegex(ValueError, 'exactly twelve'):
                release.publish(source, meta, 'owner/repo', 'a' * 40)
            gh.assert_not_called()

    def test_missing_or_test_packages_cannot_be_published(self):
        source = self.root / 'input'
        source.mkdir()
        meta = release.metadata(self.root)
        expected = release.expected_packages(meta)
        self.assertEqual(len(expected), 12)
        self.assertIn('pier-agent-1.2.3-1.el8.aarch64.rpm', expected)
        self.assertIn('pier-agent-1.2.3-1.el9.aarch64.rpm', expected)
        self.assertIn('pier-controller_1.2.3-1.ubuntu24.04_arm64.deb', expected)
        with self.assertRaisesRegex(ValueError, 'exactly the twelve'):
            release.assemble(source, self.root / 'output', meta)
        for name in expected:
            (source / name).write_bytes(b'package')
        (source / 'pier-agent_1.2.3-2.ubuntu24.04_amd64.deb').write_bytes(b'test fixture')
        with self.assertRaisesRegex(ValueError, 'exactly the twelve'):
            release.assemble(source, self.root / 'output', meta)

    def make_bundle_archive(self, corrupt=False, duplicate=False, wrong_manifest=False):
        content = b'agent package'
        digest = hashlib.sha256(content).hexdigest()
        manifest = {'schema': 1, 'releases': [
            {'package': {'version': '1.2.3', 'revision': 1}, 'format': 'deb',
             'architecture': 'amd64', 'system': 'ubuntu24.04', 'sha256': digest, 'size': len(content)}]}
        embedded = dict(manifest, schema=2) if wrong_manifest else manifest
        files = [('manifest.json', json.dumps(embedded).encode()),
                 (digest + '.deb', b'changed' if corrupt else content)]
        if duplicate:
            files.append(files[-1])
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode='w') as archive:
            for name, data in files:
                member = tarfile.TarInfo('./usr/share/pier-controller/agent-releases/' + name)
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
        return stream.getvalue(), manifest

    def test_controller_bundle_and_native_identity(self):
        archive, manifest = self.make_bundle_archive()
        identity = ('pier-controller', '1.2.3-1.ubuntu24.04', 'amd64')
        with patch.object(release, 'package_command', side_effect=[b'\n'.join(v.encode() for v in identity), archive]):
            release.verify_controller(Path('controller.deb'), identity, manifest, 'amd64')
        with patch.object(release, 'package_command', return_value=b'pier-controller\n1.2.3-2.ubuntu24.04\namd64'):
            with self.assertRaisesRegex(ValueError, 'identity'):
                release.verify_controller(Path('controller.deb'), identity, manifest, 'amd64')

    def test_reject_modified_or_duplicate_embedded_packages(self):
        for mutation in ('corrupt', 'duplicate', 'wrong_manifest'):
            archive, manifest = self.make_bundle_archive(**{mutation: True})
            with self.subTest(mutation=mutation):
                with patch.object(release, 'package_command', side_effect=[b'pier-controller\n1.2.3-1.ubuntu24.04\namd64', archive]):
                    with self.assertRaises(ValueError):
                        release.verify_controller(Path('controller.deb'), ('pier-controller', '1.2.3-1.ubuntu24.04', 'amd64'), manifest, 'amd64')


if __name__ == '__main__':
    unittest.main()
