#!/usr/bin/env python3
"""Pure custody regressions for the opt-in real-model test harness.

No sockets, model processes, profiles, or runtime assets are opened.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

from real_native_model_acceptance import OwnedApp


class CustodyTests(unittest.TestCase):
    def app(self):
        app = OwnedApp.__new__(OwnedApp)
        app.listener = Mock()
        app.child = Mock(pid=12345, returncode=0)
        app.child.poll.return_value = 0
        app.native_pids = set()
        app.app_pids = set()
        app.session = 'synthetic-session'
        return app

    def test_nonzero_exit_is_failure_after_pipe_drain(self):
        app = self.app()
        app.child.returncode = 1
        with self.assertRaisesRegex(AssertionError, 'shutdown failed'):
            app.close()
        app.child.communicate.assert_called_once_with(timeout=5)
        app.listener.close.assert_called_once()

    def test_uncertain_child_keeps_listener_until_exact_reap(self):
        app = self.app()
        app.child.poll.side_effect = [None, 0]
        failure = RuntimeError('first cleanup failure')
        calls = []

        def stop():
            app.listener.close.assert_not_called()
            calls.append(True)
            if len(calls) == 1:
                raise failure

        app.stop = stop
        with patch('real_native_model_acceptance.time.sleep'), self.assertRaises(RuntimeError) as raised:
            app.close()
        self.assertIs(raised.exception, failure)
        self.assertEqual(len(calls), 2)
        app.listener.close.assert_called_once()

    def test_pipe_eof_timeout_retains_custody_and_failed_result(self):
        app = self.app()
        app.stop = Mock()
        timeout = subprocess.TimeoutExpired('owned-child', 5)

        def communicate(timeout=5):
            app.listener.close.assert_not_called()
            if app.child.communicate.call_count == 1:
                raise failure
            return b'', b''

        failure = timeout
        app.child.communicate.side_effect = communicate
        with patch('real_native_model_acceptance.time.sleep'), self.assertRaises(subprocess.TimeoutExpired) as raised:
            app.close()
        self.assertIs(raised.exception, timeout)
        self.assertEqual(app.child.communicate.call_count, 2)
        app.listener.close.assert_called_once()

    def test_interrupt_during_cleanup_cannot_become_success(self):
        app = self.app()
        interrupt = KeyboardInterrupt()
        app.stop = Mock(side_effect=interrupt)
        with self.assertRaises(KeyboardInterrupt) as raised:
            app.close()
        self.assertIs(raised.exception, interrupt)
        app.child.communicate.assert_called_once_with(timeout=5)
        app.listener.close.assert_called_once()

    def test_recorded_runtime_must_exit_before_listener_release(self):
        app = self.app()
        app.native_pids = {22222}
        app.stop = Mock()
        with patch('real_native_model_acceptance.gone', side_effect=[False, True]) as gone, patch('real_native_model_acceptance.time.sleep'), self.assertRaisesRegex(AssertionError, 'cleanup was not confirmed'):
            app.close()
        self.assertEqual(gone.call_count, 2)
        self.assertEqual(app.stop.call_count, 2)
        app.listener.close.assert_called_once()

    def test_readiness_failure_retains_exact_child_and_restores_tmpdir(self):
        app = self.app()
        child = app.child
        app.child = None
        app.binary = Path('/synthetic/unused-binary')
        with tempfile.TemporaryDirectory(prefix='smith-helper-review-') as root:
            app.profile = Path(root) / 'profile'

            def launch(binary, profile, listener, owner, *, on_spawn):
                self.assertEqual(os.environ['TMPDIR'], str(Path(root) / 'app-temporary'))
                on_spawn(child)
                raise RuntimeError('synthetic readiness failure')

            with patch.dict(os.environ, {'TMPDIR': '/synthetic/prior-tmp'}), patch('real_native_model_acceptance.launch', side_effect=launch):
                with self.assertRaisesRegex(RuntimeError, 'readiness failure'):
                    app.start()
                self.assertEqual(os.environ['TMPDIR'], '/synthetic/prior-tmp')
        self.assertIs(app.child, child)
        self.assertIn(child.pid, app.app_pids)


if __name__ == '__main__':
    unittest.main()
