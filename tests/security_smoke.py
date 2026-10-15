#!/usr/bin/env python3
"""Adversarial tests against an explicitly disposable profile; synthetic loopback only.
Run: python3 tests/security_smoke.py --disposable-profile
Never point this at an existing user profile: changes connection configuration.
"""
import argparse
import http.server
import json
import os
import threading
import time
import urllib.error
import urllib.request

BASE = os.environ.get('SMITH_NEXT_SMOKE_URL', 'http://127.0.0.1:18471').rstrip('/')
assert BASE.startswith('http://127.0.0.1:'), 'Tests require a loopback app URL'
MODE = 'ok'
SEEN = []

class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def do_GET(self):
        SEEN.append(self.path)
        if MODE == 'redirect':
            self.send_response(302)
            self.send_header('Location', '/redirect-target')
            self.end_headers()
            return
        models = [{'name': 'fixture:cloud', 'remote_model': 'fixture', 'remote_host': 'https://ollama.com'}] if MODE == 'cloud' else [{'name': 'fixture-local'}]
        self.reply(json.dumps({'models': models, 'data': [{'id': 'fixture-local'}]}).encode())
    def do_POST(self):
        SEEN.append(self.path)
        size = int(self.headers.get('Content-Length', '0'))
        self.rfile.read(size)
        if self.path.endswith('/show'):
            self.reply(json.dumps({'capabilities': ['completion'], 'details': {'family': 'fixture'}, 'model_info': {'general.architecture': 'fixture'}}).encode())
            return
        if MODE == 'stall':
            time.sleep(3)
            self.reply(b'{"done":true}\n')
            return
        chunks = {
            'ok': b'{"message":{"content":"synthetic <script>alert(1)</script>"},"done":false}\n{"done":true}\n',
            'error': b'{"message":{"content":"partial"},"done":false}\n{"error":"synthetic failure"}\n',
            'eof': b'{"message":{"content":"partial"},"done":false}\n',
            'openaiok': b'data: {"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}\n\ndata: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n',
            'openaieof': b'data: {"choices":[{"delta":{"content":"partial"},"finish_reason":"stop"}]}\n\n',
            'openaidoneonly': b'data: {"choices":[{"delta":{"content":"partial"}}]}\n\ndata: [DONE]\n\n',
            'openailength': b'data: {"choices":[{"delta":{"content":"partial"},"finish_reason":"length"}]}\n\ndata: [DONE]\n\n',
            'openaierror': b'data: {"choices":[{"delta":{"content":"partial"}}]}\n\ndata: {"error":{"message":"fixture"}}\n\n',
            'malformed': b'{"message":{"content":"partial"},"done":false}\nBAD\n',
        }
        self.reply(chunks.get(MODE, b'{"done":true}\n'))
    def reply(self, body):
        try:
            self.send_response(200)
            self.send_header('Content-Type', 'application/x-ndjson')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError): pass

def request(path, data=None, method=None, origin=True, extra=None):
    if data is not None and (path.endswith('/messages') or path.endswith('/retry')):
        visible = request(path.rsplit('/', 1)[0])[1]
        data = dict(data)
        data.setdefault('version', visible['chat']['version'])
        if path.endswith('/retry'):
            data.setdefault('message_id', visible['active_messages'][-1]['id'])
    if path in ('/api/connection/test', '/api/chats') and data is not None or path.endswith('/messages') or path.endswith('/retry'):
        code, config = request('/api/config')
        assert code == 200
        data = dict(data or {})
        for key in ['revision', 'provider', 'endpoint']:
            data.setdefault(key, config[key])
    headers = {'Content-Type': 'application/json'}
    if origin: headers['Origin'] = BASE if origin is True else origin
    headers.update(extra or {})
    req = urllib.request.Request(BASE + path, data=None if data is None else json.dumps(data).encode(), headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            body = r.read()
            return r.status, json.loads(body) if body else None
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()

def wait_chat(cid, timeout=4):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        code, chat = request('/api/chats/' + cid)
        assert code == 200, chat
        assistants = [m for m in chat['messages'] if m['role'] == 'assistant']
        if assistants and assistants[-1]['status'] != 'streaming': return assistants[-1]
        time.sleep(.025)
    raise AssertionError('run did not terminate')

def main():
    global MODE
    p = argparse.ArgumentParser(); p.add_argument('--disposable-profile', action='store_true'); args = p.parse_args()
    assert args.disposable_profile, 'Explicit disposable-profile acknowledgement required'
    status, original = request('/api/config'); assert status == 200 and original['provider'] == 'mock', 'Require a fresh demo profile'
    checks = []
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    endpoint = 'http://127.0.0.1:' + str(server.server_port)
    cfg = {'provider': 'ollama', 'endpoint': endpoint, 'model': 'fixture-local'}
    try:
        for origin in (False, 'https://hostile.example', 'null', BASE + '.evil'):
            code, _ = request('/api/chats', {'title': 'must reject'}, origin=origin)
            assert code == 403, ('Origin accepted', origin, code)
        checks.append('mutation origin rejection')
        assert request('/api/config', extra={'Host': 'hostile.example'})[0] == 403
        checks.append('DNS rebinding Host rejection')
        for target in ['https://example.com', 'http://8.8.8.8', 'http://169.254.169.254', 'http://user:pass@127.0.0.1', 'http://127.0.0.1/?token=secret', 'http://[::ffff:8.8.8.8]', 'http://[fe80::1]']:
            assert request('/api/config', dict(cfg, endpoint=target), 'PUT')[0] == 400, target
        checks.append('endpoint policy')
        assert request('/api/config', cfg, 'PUT')[0] == 200
        MODE = 'redirect'; SEEN.clear()
        assert request('/api/connection/test', {})[0] == 400
        assert '/redirect-target' not in SEEN
        checks.append('redirect refused')
        MODE = 'cloud'
        code, data = request('/api/connection/test', {})
        assert code != 200 or not data.get('models'), ('Cloud model exposed', data)
        checks.append('known cloud model rejected')
        for mode, expected in [('ok', 'completed'), ('error', 'failed'), ('eof', 'failed'), ('malformed', 'failed'), ('stall', 'cancelled')]:
            MODE = mode
            assert request('/api/connection/test', {})[0] == 200
            _, chat = request('/api/chats', {'title': 'Synthetic security ' + mode})
            code, run = request('/api/chats/' + chat['id'] + '/messages', {'content': 'Synthetic fixture only'})
            assert code == 200, run
            if mode == 'stall':
                time.sleep(.1); began = time.monotonic()
                assert request('/api/runs/' + run['run_id'] + '/cancel', {})[0] == 200
            result = wait_chat(chat['id'])
            assert result['status'] == expected, (mode, result)
            if mode == 'stall': assert time.monotonic() - began < 1, 'Cancellation exceeded one second'
            if mode in ['error', 'eof', 'malformed']: assert result['content'] == 'partial', result
            checks.append(mode + ' stream lifecycle')
        assert request('/api/config', dict(cfg, provider='openai', endpoint=endpoint + '/v1'), 'PUT')[0] == 200
        for mode, expected in [('openaiok', 'completed'), ('openaieof', 'completed'), ('openaierror', 'failed'), ('openaidoneonly', 'failed'), ('openailength', 'limited')]:
            MODE = mode
            assert request('/api/connection/test', {})[0] == 200
            _, chat = request('/api/chats', {'title': 'Synthetic security ' + mode})
            code, run = request('/api/chats/' + chat['id'] + '/messages', {'content': 'Synthetic fixture only'})
            assert code == 200, run
            result = wait_chat(chat['id'])
            assert result['status'] == expected, (mode, result)
            assert result['content'] == 'partial', result
            if mode == 'openaieof':
                assert result['transport_incomplete'] is True and result['warning'], result
            if mode == 'openailength':
                assert result['finish_reason'] == 'length' and result['warning'], result
            checks.append(mode + ' SSE lifecycle')
        print(json.dumps({'result': 'pass', 'checks': checks}))
    finally:
        request('/api/config', {k: original[k] for k in ['provider', 'endpoint', 'model']}, 'PUT')
        server.shutdown()

if __name__ == '__main__': main()
