#!/usr/bin/env python3
"""Ephemeral bearer security suite. Starts its own disposable Smith profile and synthetic loopback servers.
No existing app profile, real credentials or model services are used.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

SECRET = 'synthetic-' + uuid.uuid4().hex
SEEN = []
REDIRECT = False
SINK = ''

class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def do_GET(self): self.handle_request()
    def do_POST(self): self.handle_request()
    def handle_request(self):
        auth = self.headers.get('Authorization')
        SEEN.append((self.server.server_port, self.path, auth))
        self.rfile.read(int(self.headers.get('Content-Length', '0')))
        if self.server.server_port == SINK_PORT:
            self.send_response(401); self.end_headers(); return
        if auth != 'Bearer ' + SECRET:
            self.send_response(401); self.end_headers(); return
        if REDIRECT:
            self.send_response(307); self.send_header('Location', SINK + '/capture'); self.end_headers(); return
        if self.path.endswith('/tags'):
            data = json.dumps({'models': [{'name': 'fixture-local'}]}).encode()
        elif self.path.endswith('/models'):
            data = json.dumps({'data': [{'id': 'fixture-local'}]}).encode()
        elif self.path.endswith('/show'):
            data = json.dumps({'capabilities': ['completion'], 'model_info': {'general.architecture': 'fixture'}}).encode()
        elif self.path.endswith('/completions'):
            data = b'data: {"choices":[{"index":0,"delta":{"content":"synthetic <script>bad()</script>"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
        else:
            data = b'{"message":{"content":"synthetic <script>bad()</script>"},"done":false}\n{"done":true,"done_reason":"stop"}\n'
        self.send_response(200); self.send_header('Content-Type', 'application/json'); self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data)

def free_port():
    with socket.socket() as s: s.bind(('127.0.0.1', 0)); return s.getsockname()[1]

def main():
    global SINK_PORT, SINK, REDIRECT
    parser = argparse.ArgumentParser(); parser.add_argument('--binary', default=str(Path(__file__).resolve().parents[1] / 'target/debug/smith-next')); args = parser.parse_args()
    fixtures = [http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture) for _ in range(2)]
    for server in fixtures: threading.Thread(target=server.serve_forever, daemon=True).start()
    target = 'http://127.0.0.1:' + str(fixtures[0].server_port)
    SINK_PORT = fixtures[1].server_port; SINK = 'http://127.0.0.1:' + str(SINK_PORT)
    checks = []
    with tempfile.TemporaryDirectory(prefix='smith-auth-qa-') as temp:
        profile = Path(temp) / 'profile'; log = Path(temp) / 'process.log'; port = free_port(); base = 'http://127.0.0.1:' + str(port)
        env = dict(os.environ, SMITH_NEXT_SYNTHETIC_PROFILE='plaintext', SMITH_NEXT_DATA_DIR=str(profile), SMITH_NEXT_PORT=str(port))
        process = None
        def req(path, data=None, method=None, origin=True):
            if data is not None and (path.endswith('/messages') or path.endswith('/retry')):
                visible = req(path.rsplit('/', 1)[0])[1]
                data = dict(data)
                data.setdefault('version', visible['chat']['version'])
                if path.endswith('/retry'):
                    data.setdefault('message_id', visible['active_messages'][-1]['id'])
            if path in ('/connection/test', '/chats') and data is not None or path.endswith('/messages') or path.endswith('/retry'):
                code, config = req('/config')
                assert code == 200
                data = dict(data or {})
                for key in ['revision', 'provider', 'endpoint']:
                    data.setdefault(key, config[key])
            headers = {'Content-Type': 'application/json'}
            if origin: headers['Origin'] = base
            request = urllib.request.Request(base + '/api' + path, data=None if data is None else json.dumps(data).encode(), headers=headers, method=method)
            try:
                with urllib.request.urlopen(request, timeout=10) as response: status, raw = response.status, response.read()
            except urllib.error.HTTPError as error: status, raw = error.code, error.read()
            assert SECRET.encode() not in raw, 'Token leaked in API response'
            try: data = json.loads(raw)
            except ValueError: data = raw.decode()
            return status, data
        def launch():
            nonlocal process
            output = open(log, 'ab')
            process = subprocess.Popen([args.binary], env=env, stdout=output, stderr=subprocess.STDOUT); output.close()
            for _ in range(100):
                if process.poll() is not None: raise AssertionError('App exited: ' + log.read_text())
                try:
                    if req('/config')[0] == 200: return
                except OSError: pass
                time.sleep(.05)
            raise AssertionError('App did not start')
        def stop():
            if process and process.poll() is None:
                process.terminate(); process.wait(timeout=10)
        def cfg(endpoint=target, provider='ollama'):
            return {'provider': provider, 'endpoint': endpoint, 'model': 'fixture-local'}
        def set_auth(token=SECRET, endpoint=target, provider='ollama'):
            return req('/session-auth', {'token': token, 'endpoint': endpoint, 'provider': provider}, 'PUT')
        def chat_run():
            code, chat = req('/chats', {'title': 'Synthetic authenticated test'}); assert code == 200
            code, run = req('/chats/' + chat['id'] + '/messages', {'content': 'Synthetic fixture only'}); assert code == 200, run
            for _ in range(100):
                code, result = req('/chats/' + chat['id']); message = result['messages'][-1]
                if message['status'] != 'streaming': break
                time.sleep(.025)
            assert message['status'] == 'completed', message
            assert req('/artifacts', {'chat_id': chat['id'], 'name': 'Synthetic.txt', 'content': message['content']})[0] == 200
            assert req('/chats/' + chat['id'] + '/export')[0] == 200
            assert req('/artifacts?chat_id=' + chat['id'])[0] == 200
        try:
            launch()
            assert req('/config', cfg(), 'PUT')[0] == 200
            assert req('/connection/test', {})[0] == 400
            assert req('/session-auth')[1]['present'] is False
            checks.append('auth required and absent by default')
            assert set_auth()[0] == 200
            assert req('/config')[1]['auth_present'] is True
            assert req('/session-auth')[1]['present'] is True
            assert req('/connection/test', {})[0] == 200
            chat_run()
            authenticated = [(path, auth) for serverport, path, auth in SEEN if auth]
            assert all(auth == 'Bearer ' + SECRET for path, auth in authenticated)
            assert all(any(path.endswith(suffix) for path, _ in authenticated) for suffix in ['/tags', '/show', '/chat'])
            checks.append('bearer on selected discovery show chat only; no API echo')
            REDIRECT = True; SEEN.clear()
            assert req('/connection/test', {})[0] == 400
            assert not any(serverport == SINK_PORT for serverport, _, _ in SEEN)
            REDIRECT = False
            checks.append('redirect sink receives no request or credential')
            assert req('/config', cfg(SINK), 'PUT')[0] == 200
            assert req('/config')[1]['auth_present'] is False
            SEEN.clear(); assert req('/connection/test', {})[0] == 400
            assert all(auth is None for _, _, auth in SEEN)
            assert set_auth(endpoint=target)[0] == 409, 'Stale auth target accepted'
            checks.append('endpoint change clears auth and stale target rejected')
            assert req('/config', cfg(), 'PUT')[0] == 200
            assert set_auth()[0] == 200
            assert req('/config', cfg(target + '/alternate'), 'PUT')[0] == 200
            assert req('/config')[1]['auth_present'] is False
            assert req('/config', cfg(), 'PUT')[0] == 200
            assert set_auth()[0] == 200
            assert req('/config', cfg(provider='openai'), 'PUT')[0] == 200
            assert req('/config')[1]['auth_present'] is False
            checks.append('path and provider change clear auth')
            openai_target = target + '/v1'
            assert req('/config', cfg(openai_target, 'openai'), 'PUT')[0] == 200
            assert set_auth(endpoint=openai_target, provider='openai')[0] == 200
            assert req('/connection/test', {})[0] == 200
            chat_run()
            checks.append('OpenAI discovery and chat authorization')
            assert req('/session-auth', {}, 'DELETE', origin=False)[0] == 403
            assert req('/session-auth', None, 'DELETE')[0] == 200
            assert req('/config')[1]['auth_present'] is False
            assert req('/connection/test', {})[0] == 400
            assert set_auth('', openai_target, 'openai')[0] == 400
            assert set_auth(SECRET + '\r\nX-Evil: yes', openai_target, 'openai')[0] == 400
            checks.append('explicit clear empty-invalid rejection and Origin guard')
            assert set_auth(endpoint=openai_target, provider='openai')[0] == 200
            stop(); launch()
            assert req('/session-auth')[1]['present'] is False
            assert req('/config')[1]['auth_present'] is False
            assert req('/connection/test', {})[0] == 400
            checks.append('restart forgets credential')
            stop()
            for path in Path(temp).rglob('*'):
                if path.is_file(): assert SECRET.encode() not in path.read_bytes(), 'Credential persisted in ' + str(path.name)
            checks.append('token absent from SQLite WAL and process logs')
            print(json.dumps({'result': 'pass', 'checks': checks}))
        finally:
            stop()
            for server in fixtures: server.shutdown()

if __name__ == '__main__': main()
