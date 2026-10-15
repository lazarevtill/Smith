"""Pure lifecycle regressions. No backend, socket, profile or key is opened."""
import signal
import subprocess
import unittest
from unittest.mock import Mock, patch

from historical_message_app import OwnedApp


def host(code=0):
    app = OwnedApp.__new__(OwnedApp)
    app.process = Mock()
    app.process.returncode = code
    app.process.poll.return_value = code
    app.process.stdout = Mock()
    app.listener = Mock()
    app.ready = True
    app.owner = 'synthetic-owner'
    app.session = 'synthetic-session'
    app.port = 23456
    app.origin = 'http://127.0.0.1:23456'
    app.drains = []
    return app


class Ownership(unittest.TestCase):
    def test_already_exited_failure_is_not_clean_shutdown(self):
        app = host(7)
        with self.assertRaisesRegex(AssertionError, 'owned-backend-shutdown-failed'):
            app.close()
        app.process.send_signal.assert_not_called()
        app.process.kill.assert_not_called()
        app.listener.close.assert_not_called()
        self.assertTrue(app.cleanup_resolved())

    def test_clean_exit_closes_listener_only_after_reader_join(self):
        app = host()
        thread, stream = Mock(), Mock()
        thread.is_alive.return_value = False
        app.drains = [(thread, stream)]
        order = []
        thread.join.side_effect = lambda **_: order.append('joined')
        app.listener.close.side_effect = lambda: order.append('listener-closed')
        app.close()
        self.assertEqual(order, ['joined', 'listener-closed'])
        self.assertIsNone(app.owner)

    def test_timeout_never_claims_success_even_when_kill_reaps(self):
        app = host(None)
        app.process.wait.side_effect = [subprocess.TimeoutExpired('owned', 25), -9]
        with self.assertRaisesRegex(AssertionError, 'owned-backend-shutdown-deadline'):
            app.close()
        app.process.send_signal.assert_called_once_with(signal.SIGINT)
        app.process.kill.assert_called_once_with()
        app.listener.close.assert_not_called()

    def test_uncertain_wait_retains_exact_child_and_listener_until_reaped(self):
        app = host(None)
        thread, stream = Mock(), Mock()
        thread.is_alive.return_value = True
        app.drains = [(thread, stream)]
        attempts = []

        def wait(**_):
            attempts.append('wait')
            app.listener.close.assert_not_called()
            if len(attempts) == 1:
                raise subprocess.TimeoutExpired('owned', 5)
            app.process.returncode = -9
            app.process.poll.return_value = -9

        app.process.wait.side_effect = wait
        thread.join.side_effect = lambda **_: setattr(thread.is_alive, 'return_value', False)
        with patch('historical_message_app.time.sleep'):
            app.retain_cleanup_until_resolved()
        self.assertEqual(len(attempts), 2)
        self.assertTrue(app.cleanup_resolved())
        app.listener.close.assert_called_once_with()
        self.assertEqual(app.process.returncode, -9)  # still failed, never converted to zero

    def test_completed_child_with_live_reader_keeps_custody(self):
        app = host()
        thread, stream = Mock(), Mock()
        thread.is_alive.return_value = True
        app.drains = [(thread, stream)]
        self.assertFalse(app.cleanup_resolved())
        with self.assertRaisesRegex(AssertionError, 'owned-backend-output-drain-unconfirmed'):
            app.close()
        app.listener.close.assert_not_called()

    def test_header_credentials_require_confirmed_handshake_and_live_child(self):
        app = host(None)
        app.ready = False
        with self.assertRaises(AssertionError):
            app.headers()
        app.ready = True
        headers = app.headers()
        self.assertEqual(headers['X-Smith-Desktop-Owner'], 'synthetic-owner')
        self.assertEqual(headers['X-Smith-Profile-Session'], 'synthetic-session')
        self.assertNotIn('X-Smith-Profile-Session', app.headers(None))
        app.process.poll.return_value = 0
        with self.assertRaises(AssertionError):
            app.headers()


if __name__ == '__main__':
    unittest.main()
