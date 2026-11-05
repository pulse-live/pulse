"""Exercise release workflow shell gates with isolated Git/data fixtures.

Requires Python 3.11+ and PyYAML. No tags, commits, or releases are created in Pulse.
"""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

REPOSITORY = Path(__file__).resolve().parents[1]
WORKFLOW = yaml.safe_load((REPOSITORY / '.github/workflows/release.yml').read_text())
VERIFY_STEPS = {step.get('id'): step for step in WORKFLOW['jobs']['verify']['steps']}
ARTIFACT_SCRIPT = next(
    step['run'] for step in WORKFLOW['jobs']['release']['steps']
    if step.get('id') == 'verify-artifacts'
)


class ReleaseGates(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='pulse-release-test-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.environment = {key: value for key, value in os.environ.items() if not key.startswith('GIT_')}
        self.environment.update(
            GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1', GIT_EDITOR='true',
            GIT_AUTHOR_NAME='Pulse fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid',
            GIT_COMMITTER_NAME='Pulse fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid',
        )
        self.git('init', '--quiet', '-b', 'main')
        self.commit_version('0.1.0')

    def git(self, *arguments):
        return subprocess.check_output(
            ['git', '-c', 'core.hooksPath=/dev/null', '-C', str(self.root), *arguments],
            env=self.environment, text=True, stderr=subprocess.DEVNULL,
        ).strip()

    def commit_version(self, version):
        (self.root / 'Cargo.toml').write_text(f'[package]\nname="pulse"\nversion="{version}"\n')
        self.git('add', 'Cargo.toml')
        self.git('commit', '--quiet', '-m', 'Create fixture version')
        self.git('update-ref', 'refs/remotes/origin/main', self.git('rev-parse', 'HEAD'))

    def execute(self, script, tag, directory=None):
        directory = directory or self.root
        output = directory / 'step-output'
        output.write_text('')
        result = subprocess.run(
            ['bash', '-euo', 'pipefail', '-c', script], cwd=directory,
            env=dict(self.environment, PULSE_RELEASE_TAG=tag, GITHUB_OUTPUT=str(output)),
            capture_output=True, text=True, timeout=10,
        )
        values = dict(line.split('=', 1) for line in output.read_text().splitlines())
        return result, values

    def test_annotated_and_lightweight_tags_resolve_to_the_source_commit(self):
        self.git('tag', '-a', 'v0.1.0', '-m', 'Annotated fixture')
        self.git('tag', '0.1.0')
        for tag in ['v0.1.0', '0.1.0']:
            with self.subTest(tag=tag):
                syntax, _ = self.execute(VERIFY_STEPS['tag']['run'], tag)
                self.assertEqual(syntax.returncode, 0, syntax.stderr)
                result, values = self.execute(VERIFY_STEPS['verify']['run'], tag)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(values['commit'], self.git('rev-parse', 'HEAD'))
                self.assertEqual(values['prerelease'], 'false')

    def test_invalid_tag_input_cannot_execute_shell_commands(self):
        for tag in ['main', '../v0.1.0', 'v0.1.0+build', 'v0.1.0;touch injected', 'v0.1.0\ncommit=evil']:
            with self.subTest(tag=tag):
                result, values = self.execute(VERIFY_STEPS['tag']['run'], tag)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('Use an existing version tag', result.stdout)
                self.assertEqual(values, {})
        self.assertFalse((self.root / 'injected').exists())

    def test_version_mismatch_and_unmerged_source_are_rejected(self):
        self.git('tag', 'v0.2.0')
        result, _ = self.execute(VERIFY_STEPS['verify']['run'], 'v0.2.0')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('must match package.version', result.stdout)
        self.git('checkout', '--quiet', '-b', 'unmerged')
        (self.root / 'Cargo.toml').write_text('[package]\nname="pulse"\nversion="0.1.1"\n')
        self.git('add', 'Cargo.toml')
        self.git('commit', '--quiet', '-m', 'Create unmerged fixture')
        self.git('tag', 'v0.1.1')
        result, _ = self.execute(VERIFY_STEPS['verify']['run'], 'v0.1.1')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('must point to a commit in main history', result.stdout)

    def test_prerelease_flag_comes_from_the_manifest(self):
        self.commit_version('0.2.0-rc.1')
        self.git('tag', '-a', 'v0.2.0-rc.1', '-m', 'Prerelease fixture')
        result, values = self.execute(VERIFY_STEPS['verify']['run'], 'v0.2.0-rc.1')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(values['prerelease'], 'true')

    def assets(self, directory, version):
        destination = directory / 'dist'
        destination.mkdir()
        names = []
        native = version.replace('-', '~')
        for target, debian, rpm in [
            ('x86_64-unknown-linux-musl', 'amd64', 'x86_64'),
            ('aarch64-unknown-linux-musl', 'arm64', 'aarch64'),
        ]:
            for suffix in ['', '-telemetry']:
                package = 'pulse' + suffix
                names.extend([
                    f'pulse-{version}-{target}{suffix}.tar.gz',
                    f'{package}_{native}-1_{debian}.deb',
                    f'{package}-{native}-1.{rpm}.rpm',
                ])
        for name in names:
            # These are checksum-test data, not native-package payload fixtures.
            payload = ('fixture: ' + name).encode()
            (destination / name).write_bytes(payload)
            (destination / (name + '.sha256')).write_text(hashlib.sha256(payload).hexdigest() + '  ' + name + '\n')
        return names

    def test_all_formats_architectures_and_prerelease_filenames_are_verified(self):
        for version in ['0.1.0', '0.1.0-rc.1', '0.1.0-rc-test.1']:
            with self.subTest(version=version), tempfile.TemporaryDirectory(dir=self.root) as path:
                directory = Path(path)
                names = self.assets(directory, version)
                result, _ = self.execute(ARTIFACT_SCRIPT, 'v' + version, directory)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                combined = (directory / 'dist/SHA256SUMS').read_text().splitlines()
                self.assertEqual(len(combined), 12)
                self.assertEqual({line.split()[1] for line in combined}, set(names))

    def test_missing_mislabeled_corrupt_and_misdirected_artifacts_are_rejected(self):
        for mutation in [
            'missing', 'wrong_architecture', 'corrupt', 'wrong_checksum_reference',
            'multiple_records', 'invalid_digest',
        ]:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory(dir=self.root) as path:
                directory = Path(path)
                names = self.assets(directory, '0.1.0')
                destination = directory / 'dist'
                first = destination / names[0]
                digest = destination / (names[0] + '.sha256')
                if mutation == 'missing':
                    first.unlink()
                elif mutation == 'wrong_architecture':
                    replacement = destination / 'wrong-architecture.tar.gz'
                    first.rename(replacement)
                    digest.rename(destination / (replacement.name + '.sha256'))
                elif mutation == 'corrupt':
                    first.write_bytes(b'corrupted')
                elif mutation == 'multiple_records':
                    digest.write_text(digest.read_text() * 2)
                elif mutation == 'invalid_digest':
                    digest.write_text('invalid  ' + names[0] + '\n')
                else:
                    # A valid checksum for a different file must not leave this one unchecked.
                    first.write_bytes(b'corrupted')
                    digest.write_text((destination / (names[1] + '.sha256')).read_text())
                result, _ = self.execute(ARTIFACT_SCRIPT, 'v0.1.0', directory)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse((destination / 'SHA256SUMS').exists())
                expected = {
                    'missing': 'Expected all three formats',
                    'wrong_architecture': 'A required format, architecture or feature artifact is missing',
                    'corrupt': 'FAILED',
                    'wrong_checksum_reference': 'Checksum record must match its artifact',
                    'multiple_records': 'Each checksum file must contain exactly one record',
                    'invalid_digest': 'Checksum record must match its artifact',
                }
                self.assertIn(expected[mutation], result.stdout)


if __name__ == '__main__':
    unittest.main(verbosity=2)
