"""Pure catalog harness ownership regressions; no backend, socket or model runs."""
import errno
import io
import json
from pathlib import Path
import signal
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

import local_catalog_app_acceptance as runner


def host(code=0):
    value = runner.Host.__new__(runner.Host)
    value.child = Mock()
    value.child.pid = 12345
    value.child.returncode = code
    value.child.poll.return_value = code
    value.child.stdout, value.child.stderr = io.BytesIO(), io.BytesIO()
    value.listener = Mock()
    value.session, value.owner = 'synthetic-session', 'synthetic-owner'
    value.pids, value.drains, value.output_counts, value.exits = [], [], [], []
    return value


class CatalogHarnessOwnership(unittest.TestCase):
    def test_unconfirmed_kill_retains_exact_child_listener_and_pipes(self):
        value = host(None)
        child = value.child
        child.wait.side_effect = subprocess.TimeoutExpired('owned', 5)
        with self.assertRaisesRegex(AssertionError, 'exit remains unconfirmed'):
            value.close()
        child.send_signal.assert_called_once_with(signal.SIGTERM)
        child.kill.assert_called_once_with()
        self.assertIs(value.child, child)
        self.assertFalse(value.cleanup_confirmed())
        value.listener.close.assert_not_called()
        self.assertFalse(child.stdout.closed)
        self.assertFalse(child.stderr.closed)

    def test_already_exited_failure_is_preserved_after_confirmed_release(self):
        value = host(7)
        with self.assertRaisesRegex(AssertionError, 'did not exit cleanly: 7'):
            value.close()
        value.child.send_signal.assert_not_called()
        value.child.kill.assert_not_called()
        self.assertTrue(value.cleanup_confirmed())
        self.assertEqual(value.exits, [{'pid': 12345, 'returncode': 7}])
        value.listener.close.assert_called_once_with()
        self.assertTrue(value.child.stdout.closed)

    def test_live_reader_retains_listener_after_child_exit(self):
        value = host()
        reader, stream = Mock(), io.BytesIO()
        reader.is_alive.return_value = True
        value.drains = [(reader, stream)]
        with self.assertRaisesRegex(AssertionError, 'reader remained'):
            value.close()
        self.assertFalse(value.cleanup_confirmed())
        value.listener.close.assert_not_called()
        self.assertFalse(stream.closed)

    def test_exceptional_hold_releases_only_after_child_and_reader_finish(self):
        value = host(None)
        reader, stream = Mock(), io.BytesIO()
        reader.is_alive.return_value = True
        value.drains = [(reader, stream)]
        attempts = []

        def wait(**_):
            value.listener.close.assert_not_called()
            attempts.append('wait')
            if len(attempts) == 1:
                raise subprocess.TimeoutExpired('owned', .5)
            value.child.poll.return_value = -9
            value.child.returncode = -9

        value.child.wait.side_effect = wait
        reader.join.side_effect = lambda **_: setattr(reader.is_alive, 'return_value', False)
        value.retain_until_confirmed()
        self.assertEqual(len(attempts), 2)
        self.assertTrue(value.cleanup_confirmed())
        self.assertEqual(value.child.returncode, -9)
        value.listener.close.assert_called_once_with()

    def test_failed_readiness_keeps_new_popen_owned(self):
        value = host()
        new_child = host(12).child
        with tempfile.TemporaryDirectory(prefix='catalog-harness-review-') as temporary:
            value.root = Path(temporary) / 'profile'
            value.binary = Path('/synthetic/not-executed')
            with patch.object(runner.subprocess, 'Popen', return_value=new_child), \
                    patch.object(runner.selectors, 'DefaultSelector'):
                with self.assertRaisesRegex(AssertionError, 'exited before readiness: 12'):
                    value.start()
        self.assertIs(value.child, new_child)
        self.assertEqual(value.pids, [new_child.pid])
        value.listener.close.assert_not_called()

    def test_exclusive_report_preserves_existing_bytes_and_redacts_owned_root(self):
        with tempfile.TemporaryDirectory(prefix='catalog-report-review-') as temporary:
            root = Path(temporary)
            report = root / 'evidence.json'
            fd = runner.reserve_report(report)
            runner.write_reserved_report(fd, {'owned': str(root / 'profile')}, root)
            original = report.read_bytes()
            self.assertEqual(json.loads(original), {'owned': '[owned synthetic fixture]/profile'})
            self.assertEqual(stat.S_IMODE(report.stat().st_mode), 0o600)
            with self.assertRaises(FileExistsError):
                runner.reserve_report(report)
            self.assertEqual(report.read_bytes(), original)
            linked = root / 'linked.json'
            linked.symlink_to(report)
            with self.assertRaises(FileExistsError):
                runner.reserve_report(linked)
            self.assertEqual(report.read_bytes(), original)

    def test_report_enospc_and_closed_stdout_still_hold_custody(self):
        value = Mock()
        value.child.pid, value.child.returncode = 12345, -9
        report = {'passed': False, 'checks': [], 'diagnostic_tree_retained': True}
        with patch.object(runner, 'write_reserved_report', side_effect=OSError(errno.ENOSPC, 'synthetic full')), \
                patch.object(runner, 'reserve_report', return_value=42), \
                patch('builtins.print', side_effect=ValueError('synthetic closed stdout')):
            with self.assertRaises(OSError) as error:
                runner.publish_with_custody(41, Path('/synthetic/evidence.json'), report,
                                            Path('/synthetic/tree'), value, False)
        self.assertEqual(error.exception.errno, errno.ENOSPC)
        value.retain_until_confirmed.assert_called_once_with()
        self.assertFalse(report['passed'])

    def test_broken_summary_still_holds_custody_without_rewriting_primary(self):
        value = Mock()
        value.child.pid, value.child.returncode = 12345, -9
        path = Path('/synthetic/evidence.json')
        report = {'passed': False, 'checks': [], 'diagnostic_tree_retained': True}
        with patch.object(runner, 'write_reserved_report') as write, \
                patch.object(runner, 'reserve_report', return_value=42) as reserve, \
                patch('builtins.print', side_effect=BrokenPipeError('synthetic broken pipe')):
            with self.assertRaises(BrokenPipeError):
                runner.publish_with_custody(41, path, report, Path('/synthetic/tree'), value, False)
        value.retain_until_confirmed.assert_called_once_with()
        self.assertEqual(write.call_count, 2)
        self.assertEqual(write.call_args_list[0].args[0], 41)
        companion = reserve.call_args.args[0]
        self.assertNotEqual(companion, path)
        self.assertTrue(companion.name.startswith('evidence-cleanup-'))
        self.assertFalse(write.call_args_list[1].args[1]['qualification_passed'])


if __name__ == '__main__':
    unittest.main()
