#!/usr/bin/env python3
"""Self-launch synthetic lifecycle checks; no existing profile or inference service.
Run: python3 tests/lifecycle_faults.py [target/debug/smith-next]
"""
import http.cookiejar
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/smith-next').resolve()

def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]

class Server:
    def __init__(self, profile, port):
        self.base = f'http://127.0.0.1:{port}'
        self.jar = http.cookiejar.CookieJar()
        self.http = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar))
        self.proc = subprocess.Popen([str(BINARY)], env={**os.environ, 'SMITH_NEXT_SYNTHETIC_PROFILE': 'plaintext', 'SMITH_NEXT_DATA_DIR': str(profile), 'SMITH_NEXT_PORT': str(port)}, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    def ready(self):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise AssertionError('Server exited: ' + self.proc.stdout.read())
            try:
                with self.http.open(self.base + '/', timeout=.3) as response:
                    response.read()
                self.call('/api/config')
                return self
            except (OSError, urllib.error.URLError):
                time.sleep(.05)
        raise AssertionError('Server readiness timed out')
    def call(self, path, data=None):
        if data is not None and (path.endswith('/messages') or path.endswith('/retry')):
            visible = self.call(path.rsplit('/', 1)[0])
            data = dict(data)
            data.setdefault('version', visible['chat']['version'])
            if path.endswith('/retry'):
                data.setdefault('message_id', visible['active_messages'][-1]['id'])
        req = urllib.request.Request(self.base + path, data=None if data is None else json.dumps(data).encode(), headers={'Origin': self.base, 'Content-Type': 'application/json'})
        with self.http.open(req, timeout=10) as response:
            return json.load(response)
    def stop(self, crash=False):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGKILL if crash else signal.SIGINT)
            self.proc.wait(timeout=8)
        return self.proc.returncode


def main():
    assert BINARY.is_file(), f'Build binary first: {BINARY}'
    results = {}
    servers = []
    with tempfile.TemporaryDirectory(prefix='smith-next-lifecycle-') as tmp:
        profile = Path(tmp) / 'profile'
        port = free_port()
        try:
            s = Server(profile, port).ready(); servers.append(s)
            config = s.call('/api/config')
            assert config['provider'] == 'mock'
            target = {key: config[key] for key in ('revision', 'provider', 'endpoint')}
            c = s.call('/api/chats', {'title': 'Synthetic lifecycle', **target})
            r = s.call(f"/api/chats/{c['id']}/messages", {'content': 'Synthetic crash recovery checkpoint', **target})
            deadline = time.monotonic() + 3
            while True:
                messages = s.call(f"/api/chats/{c['id']}")['messages']
                if messages[-1]['content']:
                    break
                assert time.monotonic() < deadline
                time.sleep(.025)
            partial = messages[-1]['content']
            # A second process may neither mutate the profile nor stop the owner.
            duplicate = Server(profile, free_port()); servers.append(duplicate)
            code = duplicate.proc.wait(timeout=8)
            output = duplicate.proc.stdout.read()
            assert code != 0 and 'already open' in output.lower(), output
            assert s.call('/api/config')['provider'] == 'mock'
            results['duplicate_profile_owner_untouched'] = 'pass'
            # A different profile competing for the active port must fail cleanly.
            occupied = Server(Path(tmp) / 'other', port); servers.append(occupied)
            code = occupied.proc.wait(timeout=8)
            output = occupied.proc.stdout.read()
            assert code != 0 and ('port' in output.lower() or 'address' in output.lower()), output
            assert 'panicked' not in output.lower(), 'Occupied-port error must be friendly: ' + output
            assert s.call('/api/config')['provider'] == 'mock'
            assert not (Path(tmp) / 'other' / 'smith-next.sqlite3').exists(), 'Port conflict must not touch SQLite'
            results['occupied_port_owner_untouched'] = 'pass'
            s.stop(crash=True)
            s = Server(profile, port).ready(); servers.append(s)
            restored = s.call(f"/api/chats/{c['id']}")['messages']
            assert restored[0]['content'] == 'Synthetic crash recovery checkpoint'
            assert restored[-1]['status'] == 'interrupted', restored
            assert restored[-1]['content'].startswith(partial), restored
            results['crash_restart_partial_and_prompt'] = 'pass'
            config = s.call('/api/config')
            target = {key: config[key] for key in ('revision', 'provider', 'endpoint')}
            active = s.call(f"/api/chats/{c['id']}/messages", {'content': 'Synthetic active SSE shutdown', **target})
            stream = s.http.open(s.base + f"/api/runs/{active['run_id']}/events", timeout=5)
            # Keep a real HTTP SSE response open while requesting graceful shutdown.
            before_shutdown = time.monotonic()
            try:
                assert s.stop() == 0, 'SIGINT with active SSE must exit cleanly'
            finally:
                stream.close()
            elapsed = time.monotonic() - before_shutdown
            assert elapsed < 2, f'Active SSE delayed graceful shutdown by {elapsed:.3f}s'
            results['active_sse_shutdown_under_2s'] = round(elapsed, 3)

            s = Server(profile, port).ready(); servers.append(s)
            again = s.call(f"/api/chats/{c['id']}")['messages']
            assert again[:len(restored)] == restored
            assert again[-2]['content'] == 'Synthetic active SSE shutdown'
            assert again[-1]['status'] in ('cancelled', 'interrupted'), again
            assert len(again) == len(restored) + 2
            assert s.stop() == 0
            results['graceful_shutdown_restart_exact_history'] = 'pass'
        finally:
            for server in servers:
                if server.proc.poll() is None:
                    server.stop(crash=True)
    print(json.dumps({'result': 'pass', 'checks': results}))

if __name__ == '__main__':
    main()
