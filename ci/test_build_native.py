"""Build-plan tests only: never install, compile, execute Smith or open profiles."""
import contextlib
import io
import json
from pathlib import Path
import unittest
from unittest.mock import patch

import build_native


class BuildPlans(unittest.TestCase):
    def test_normal_backend_has_exact_compiler_locked_release_and_no_test_feature(self):
        command = build_native.command_for('backend', build_native.TARGETS['windows'])
        self.assertEqual(command[:5], ['rustup', 'run', '1.89.0', 'cargo', 'build'])
        self.assertIn('--locked', command)
        self.assertIn('--release', command)
        self.assertNotIn('--features', command)
        self.assertNotIn('--ignore-rust-version', command)
        self.assertEqual(command[-1], 'smith-next')

    def test_native_is_actual_shell_with_declared_msrv_and_offline_optional(self):
        command = build_native.command_for('native', build_native.TARGETS['linux'], True)
        self.assertEqual(command[2], '1.92.0')
        self.assertEqual(command[command.index('--bin') + 1], 'smith-native-desktop')
        self.assertEqual(command[command.index('--features') + 1], 'native-ui')
        self.assertEqual(command[-1], '--offline')

    def test_all_eight_dry_runs_do_not_spawn_or_write(self):
        for component in build_native.COMPONENTS:
            for target in build_native.TARGETS:
                with self.subTest(component=component, target=target), \
                        patch('sys.argv', ['build_native.py', '--component', component, '--platform', target, '--dry-run']), \
                        patch.object(build_native.subprocess, 'check_output') as read, \
                        patch.object(build_native.subprocess, 'run') as run, \
                        patch.object(Path, 'mkdir') as mkdir, \
                        contextlib.redirect_stdout(io.StringIO()) as stdout:
                    build_native.main()
                    self.assertTrue(json.loads(stdout.getvalue())['build_only'])
                    read.assert_not_called()
                    run.assert_not_called()
                    mkdir.assert_not_called()

    def test_mac_architectures_cannot_be_misreported_as_native_cross_compiles(self):
        self.assertTrue(build_native.matching_host('macos-arm64', 'Darwin', 'arm64'))
        self.assertTrue(build_native.matching_host('macos-intel', 'Darwin', 'x86_64'))
        self.assertFalse(build_native.matching_host('macos-intel', 'Darwin', 'arm64'))
        self.assertFalse(build_native.matching_host('macos-arm64', 'Linux', 'aarch64'))
        self.assertFalse(build_native.matching_host('windows', 'Linux', 'x86_64'))
        self.assertEqual(build_native.TARGETS['macos-arm64'], 'aarch64-apple-darwin')
        self.assertEqual(build_native.TARGETS['macos-intel'], 'x86_64-apple-darwin')

    def test_reviewed_vendor_snapshot_is_verified_without_git_or_credentials(self):
        path = Path('/synthetic/vendor/smith-ui')
        with patch.object(Path, 'is_file', return_value=True), \
                patch.object(Path, 'resolve', autospec=True, side_effect=lambda p, **_: p), \
                patch.object(build_native, 'verify_snapshot', return_value=(path, {'files': 96})) as verify, \
                patch.object(build_native, 'output') as output:
            self.assertEqual(build_native.verify_ui(path), (path, {'files': 96}))
            verify.assert_called_once_with(path)
            output.assert_not_called()

    def test_wrong_original_revision_refuses_before_status_or_build(self):
        path = Path(__file__).resolve().parent.parent / 'crates/smith-ui'
        with patch.object(Path, 'resolve', autospec=True, side_effect=lambda p, **_: p), \
                patch.object(build_native, 'output', side_effect=[str(path.parent.parent), '0' * 40]) as output:
            with self.assertRaisesRegex(ValueError, 'differs from the exact reviewed'):
                build_native.verify_ui(path)
        self.assertEqual(output.call_count, 2)

    def test_environment_metadata_is_explicit_allowlist_bounded_and_has_no_secret_dump(self):
        evidence = build_native.build_environment({
            'ImageOS': 'ubuntu24\nforged-line', 'ImageVersion': '20261001.1',
            'RUNNER_OS': 'Linux', 'RUNNER_ARCH': 'X64',
            'GITHUB_TOKEN': 'must-never-appear', 'HOME': '/private-user',
            'UNRELATED_SECRET': 'must-never-appear',
        })
        self.assertEqual(set(evidence['runner_image']), {'ImageOS', 'ImageVersion', 'RUNNER_OS', 'RUNNER_ARCH'})
        encoded = json.dumps(evidence)
        self.assertNotIn('must-never-appear', encoded)
        self.assertNotIn('/private-user', encoded)
        self.assertNotIn('\n', evidence['runner_image']['ImageOS'])
        self.assertIn('not_captured', evidence['system_package_versions'])
        self.assertEqual(len(build_native.build_environment({'ImageOS': 'x' * 1000})['runner_image']['ImageOS']), 256)


if __name__ == '__main__':
    unittest.main()
