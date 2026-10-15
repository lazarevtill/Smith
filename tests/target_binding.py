#!/usr/bin/env python3
"""Two-tab target privacy regression using only disposable profile and two loopback fixtures."""
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
import urllib.parse

SEEN = []
class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def do_GET(self):
        SEEN.append((self.server.server_port, self.path))
        self.reply(json.dumps({'models': [{'name': 'same-model'}]}).encode())
    def do_POST(self):
        SEEN.append((self.server.server_port, self.path))
        self.rfile.read(int(self.headers.get('Content-Length', '0')))
        if self.path.endswith('/show'):
            self.reply(json.dumps({'model_info': {'general.architecture': 'fixture'}, 'capabilities': ['completion']}).encode())
        else:
            self.reply(b'{"message":{"content":"synthetic answer"},"done":false}\n{"done":true,"done_reason":"stop"}\n')
    def reply(self, body):
        self.send_response(200); self.send_header('Content-Type', 'application/json'); self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)

def main():
    p = argparse.ArgumentParser(); p.add_argument('--binary', default=str(Path(__file__).resolve().parents[1] / 'target/debug/smith-next')); args = p.parse_args()
    servers = [http.server.ThreadingHTTPServer(('127.0.0.1', 0), Provider) for _ in range(2)]
    for s in servers: threading.Thread(target=s.serve_forever, daemon=True).start()
    endpoints = ['http://127.0.0.1:' + str(s.server_port) for s in servers]
    with socket.socket() as probe: probe.bind(('127.0.0.1', 0)); port = probe.getsockname()[1]
    base = 'http://127.0.0.1:' + str(port)
    checks = []
    with tempfile.TemporaryDirectory(prefix='smith-target-qa-') as td:
        log = open(Path(td) / 'app.log', 'wb')
        proc = subprocess.Popen([args.binary], env=dict(os.environ, SMITH_NEXT_SYNTHETIC_PROFILE='plaintext', SMITH_NEXT_DATA_DIR=td, SMITH_NEXT_PORT=str(port)), stdout=log, stderr=subprocess.STDOUT)
        def api(path, data=None, method=None):
            if data is not None and (path.endswith('/messages') or path.endswith('/retry')):
                visible = api(path.rsplit('/', 1)[0])[1]
                data = dict(data)
                data.setdefault('version', visible['chat']['version'])
                if path.endswith('/retry'):
                    data.setdefault('message_id', visible['active_messages'][-1]['id'])
            r = urllib.request.Request(base + '/api' + path, data=None if data is None else json.dumps(data).encode(), headers={'Content-Type':'application/json', 'Origin':base}, method=method)
            try:
                with urllib.request.urlopen(r, timeout=10) as response: code, raw = response.status, response.read()
            except urllib.error.HTTPError as e: code, raw = e.code, e.read()
            try: return code, json.loads(raw)
            except ValueError: return code, raw.decode()
        def config(n):
            assert api('/config', {'provider':'ollama', 'endpoint':endpoints[n], 'model':'same-model'}, 'PUT')[0] == 200
            code, c = api('/config'); assert code == 200
            assert api('/connection/test', expected(c))[0] == 200
            return c
        def expected(c):
            return {'provider':c['provider'], 'endpoint':c['endpoint'], 'revision':c['revision']}
        def get_messages(cid):
            code, chat = api('/chats/' + cid); assert code == 200
            return chat['messages']
        def rejected(cid, snapshot, retry=False, want=409):
            before = get_messages(cid); requests = list(SEEN)
            body = dict(expected(snapshot), model='same-model')
            if not retry: body.update(content='PRIVATE SYNTHETIC TAB A DRAFT', model='same-model')
            code, result = api('/chats/' + cid + ('/retry' if retry else '/messages'), body)
            assert code == want, (code, result)
            assert get_messages(cid) == before, 'Rejected action wrote history'
            assert SEEN == requests, 'Rejected action contacted provider'
        def completed(cid):
            for _ in range(200):
                messages = get_messages(cid)
                if messages[-1]['status'] != 'streaming':
                    assert messages[-1]['status'] == 'completed', messages[-1]
                    return messages
                time.sleep(.025)
            raise AssertionError('Fixture did not complete')
        try:
            for _ in range(100):
                if proc.poll() is not None: raise AssertionError('App exited')
                try:
                    if api('/config')[0] == 200: break
                except OSError: pass
                time.sleep(.05)
            a = config(0)
            _, chat = api('/chats', dict(expected(a), title='Synthetic two-tab privacy')); cid = chat['id']
            b = config(1)
            assert a['revision'] != b['revision']
            SEEN.clear()
            before_chats = api('/chats')[1]
            code, _ = api('/chats', dict(expected(a), title='PRIVATE SYNTHETIC DRAFT TITLE'))
            assert code == 409
            assert api('/chats')[1] == before_chats and not SEEN
            checks.append('stale new chat rejected before prompt-derived title rows or network')
            before = list(SEEN)
            assert api('/connection/test', expected(a))[0] == 409
            assert SEEN == before
            assert api('/models?revision=' + urllib.parse.quote(a['revision'], safe=''))[0] == 409
            code, models = api('/models?revision=' + urllib.parse.quote(b['revision'], safe=''))
            assert code == 200 and models['revision'] == b['revision'] and models['endpoint'] == endpoints[1], models
            checks.append('stale connection test and model cache access rejected; models bound to current target')
            rejected(cid, a)
            checks.append('stale Tab A send rejected before rows or network despite identical model ID')
            code, run = api('/chats/' + cid + '/messages', dict(expected(b), content='Synthetic Tab B prompt'))
            assert code == 200, run
            completed(cid)
            assert any(port == servers[1].server_port and path.endswith('/chat') for port, path in SEEN)
            assert not any(port == servers[0].server_port for port, _ in SEEN)
            checks.append('current Tab B target accepted and only B contacted')
            rejected(cid, a, retry=True)
            checks.append('stale retry rejected before rows or network')
            a2 = config(0)
            assert a2['revision'] != a['revision']
            rejected(cid, a)
            checks.append('A to B to A change cannot resurrect stale A snapshot')
            before = get_messages(cid); requests = list(SEEN)
            code, _ = api('/chats/' + cid + '/messages', {'content':'missing target', 'model':'same-model'})
            assert code in (400, 409, 422), code
            assert get_messages(cid) == before and SEEN == requests
            checks.append('missing target binding rejected before rows or network')
            forged = dict(a2, endpoint=endpoints[1]); rejected(cid, forged)
            checks.append('endpoint mismatch rejected even with current revision')
            # Prior attempt belongs to B. Current view A must not silently retry that attempt elsewhere.
            rejected(cid, a2, retry=True)
            checks.append('retry cannot silently redirect saved B attempt to current A')
            # A hidden invalid address must not make offline Demo depend on URL
            # validation or contact either of the real-provider fixtures.
            requests = list(SEEN)
            for retained in ['', 'not a URL', 'https://example.com/private', endpoints[0]]:
                code, _ = api('/config', {'provider':'mock', 'endpoint':retained, 'model':'smith-demo'}, 'PUT')
                assert code == 200
                _, demo = api('/config')
                assert demo['endpoint'] == '', demo
                assert api('/connection/test', expected(demo))[0] == 200
                _, chat = api('/chats', dict(expected(demo), title='Isolated offline Demo recovery'))
                code, run = api('/chats/' + chat['id'] + '/messages', dict(expected(demo), content='Synthetic offline Demo'))
                assert code == 200, run
                result = completed(chat['id'])[-1]
                assert result['snapshot']['provider'] == 'mock' and result['snapshot']['endpoint'] == ''
                assert 'no model or network request was used' in result['content']
                assert SEEN == requests, 'Demo contacted an inference provider'
            checks.append('Demo after empty invalid public or retained address completes without provider requests')
            print(json.dumps({'result':'pass','checks':checks}))
        finally:
            proc.terminate(); proc.wait(timeout=10); log.close()
            for s in servers: s.shutdown()

if __name__ == '__main__': main()
