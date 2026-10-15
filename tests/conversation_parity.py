#!/usr/bin/env python3
"""Synthetic branch/UTF-8 attachment HTTP acceptance; starts only owned loopback fixtures.
No real profile, keys, paid API, model server or downloads. The fake provider records
exact request context, so these checks do not certify model quality or real inference.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Provider(BaseHTTPRequestHandler):
    requests = []
    def log_message(self, *args): pass
    def do_GET(self):
        self.send_response(200); self.send_header('Content-Type', 'application/json'); self.end_headers()
        self.wfile.write(b'{"data":[{"id":"fixture-text"}]}')
    def do_POST(self):
        data = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.requests.append(data)
        self.send_response(200); self.send_header('Content-Type', 'text/event-stream'); self.end_headers()
        for event in [{'choices': [{'delta': {'content': 'Synthetic answer ' + str(len(self.requests))}, 'finish_reason': None}]}, {'choices': [{'delta': {}, 'finish_reason': 'stop'}]}]:
            self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
        self.wfile.write(b'data: [DONE]\n\n')


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(); parser.add_argument('--binary', default=str(root / 'target/debug/smith-next')); args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
    base = f'http://127.0.0.1:{port}'
    server = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    checks = []
    with tempfile.TemporaryDirectory(prefix='smith-branches-') as temp:
        profile = Path(temp) / 'profile'
        env = {**os.environ, 'SMITH_NEXT_SYNTHETIC_PROFILE': 'plaintext', 'SMITH_NEXT_DATA_DIR': str(profile), 'SMITH_NEXT_PORT': str(port)}
        process = None
        def api(path, data=None, method=None, want=200, text=False):
            request = urllib.request.Request(base + '/api' + path, data=None if data is None else json.dumps(data).encode(), method=method, headers={'Origin': base, 'Content-Type': 'application/json'})
            try:
                with urllib.request.urlopen(request, timeout=10) as response: status, body = response.status, response.read()
            except urllib.error.HTTPError as error: status, body = error.code, error.read()
            assert status == want, (path, status, body)
            return body.decode() if text else json.loads(body)
        def start():
            nonlocal process
            process = subprocess.Popen([binary], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            for _ in range(150):
                if process.poll() is not None: raise AssertionError(process.stdout.read())
                try: return api('/config')
                except OSError: time.sleep(.02)
            raise AssertionError('App readiness timeout')
        def stop():
            if process and process.poll() is None:
                process.send_signal(signal.SIGINT)
                try: process.wait(timeout=5)
                except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=5)
        def complete(chat, run):
            deadline = time.monotonic() + 8
            while True:
                state = api('/chats/' + chat)
                message = next(m for m in state['messages'] if m['id'] == run['message_id'])
                if message['status'] != 'streaming':
                    assert message['status'] == 'completed', message
                    return state
                assert time.monotonic() < deadline, message
                time.sleep(.02)
        try:
            config = start(); assert config['provider'] == 'mock'
            endpoint = f'http://127.0.0.1:{server.server_port}/v1'
            api('/config', {'provider': 'openai', 'endpoint': endpoint, 'model': 'fixture-text'}, 'PUT')
            config = api('/config'); target = {k: config[k] for k in ('revision', 'provider', 'endpoint')}
            assert api('/connection/test', target)['ok']
            chat = api('/chats', {'title': 'Branch fixture', **target}); cid = chat['id']; route = '/chats/' + cid
            first = api(route + '/messages', {'content': 'ORIGINAL ROOT', 'version': 0, 'attachments': [{'name': 'context.txt', 'media_type': 'text/plain', 'content': 'DURABLE CONTEXT 🦀\n<script>inert</script>'}], **target})
            state = complete(cid, first)
            assert state['messages'] == first['conversation']['messages'] or state['messages'][-1]['status'] == 'completed'
            assert state['active_messages'][0]['attachments'][0]['content'].startswith('DURABLE CONTEXT')
            assert 'DURABLE CONTEXT' in Provider.requests[-1]['messages'][0]['content']
            second = api(route + '/messages', {'content': 'OLD DOWNSTREAM', 'version': state['chat']['version'], **target}); state = complete(cid, second)
            # Non-root edits fork at the prior assistant and retain all ancestor context.
            nonroot = api(route + '/edit', {'message_id': second['user_message_id'], 'version': state['chat']['version'], 'content': 'EDITED DOWNSTREAM', **target}); state = complete(cid, nonroot)
            assert state['active_messages'][2]['parent_id'] == first['message_id']
            assert len(Provider.requests[-1]['messages']) == 3
            assert Provider.requests[-1]['messages'][-1]['content'] == 'EDITED DOWNSTREAM'
            state = api(route + '/branch', {'message_id': second['user_message_id'], 'version': state['chat']['version'], **target})
            checks.append('non-root-edit-retains-ancestor-path-and-old-sibling')
            original_rows = state['messages']; original_version = state['chat']['version']
            edit = api(route + '/edit', {'message_id': first['user_message_id'], 'version': original_version, 'content': 'EDITED ROOT', **target}); state = complete(cid, edit)
            assert len(state['messages']) == 8 and len(state['active_messages']) == 2
            assert state['active_messages'][0]['parent_id'] is None
            assert state['active_messages'][0]['siblings'] == [first['user_message_id'], edit['user_message_id']]
            assert state['active_messages'][0]['attachments'][0]['content'].startswith('DURABLE CONTEXT')
            assert all(row in state['messages'] for row in original_rows)
            assert len(Provider.requests[-1]['messages']) == 1
            assert 'EDITED ROOT' in Provider.requests[-1]['messages'][0]['content'] and 'OLD DOWNSTREAM' not in str(Provider.requests[-1])
            checks.append('root-edit-sibling-preserves-original-downstream-status-and-attachment-context')
            for suffix, body in [('edit', {'message_id': first['user_message_id'], 'content': 'stale'}), ('regenerate', {'message_id': edit['message_id'], 'model': 'fixture-text'}), ('branch', {'message_id': first['user_message_id']})]:
                api(route + '/' + suffix, {**body, 'version': original_version, **target}, want=409)
            assert api(route)['messages'] == state['messages']
            checks.append('stale-edit-regenerate-navigation-atomic-rejection')
            state = api(route + '/branch', {'message_id': first['user_message_id'], 'version': state['chat']['version'], **target})
            assert state['active_leaf'] == nonroot['message_id'] and len(state['active_messages']) == 4
            state = api(route + '/branch', {'message_id':second['user_message_id'], 'version':state['chat']['version'], **target})
            assert state['active_leaf'] == second['message_id']
            continued = api(route + '/messages', {'content': 'CONTINUED ORIGINAL', 'version': state['chat']['version'], **target}); state = complete(cid, continued)
            assert state['active_messages'][-2]['parent_id'] == second['message_id']
            assert 'EDITED ROOT' not in str(Provider.requests[-1])
            assert [m['role'] for m in Provider.requests[-1]['messages']] == ['user', 'assistant', 'user', 'assistant', 'user']
            checks.append('old-branch-navigation-continues-its-own-context')
            regen = api(route + '/regenerate', {'message_id': first['message_id'], 'version': state['chat']['version'], 'model': 'fixture-text', **target}); state = complete(cid, regen)
            assert len(state['active_messages']) == 2
            assert state['active_messages'][-1]['siblings'] == [first['message_id'], regen['message_id']]
            assert len(Provider.requests[-1]['messages']) == 1 and 'ORIGINAL ROOT' in Provider.requests[-1]['messages'][0]['content']
            checks.append('regenerate-earlier-answer-excludes-later-turns-and-old-alternatives')
            wrong = api('/chats', {'title': 'Other', **target})
            other = api('/chats/' + wrong['id'] + '/messages', {'content': 'OTHER CHAT', 'version':wrong['version'], **target}); complete(wrong['id'], other)
            api(route + '/branch', {'message_id': other['message_id'], 'version': state['chat']['version'], **target}, want=404)
            api(route + '/edit', {'message_id': other['user_message_id'], 'version': state['chat']['version'], 'content': 'cross-chat', **target}, want=409)
            before_missing = api(route); provider_before_missing = len(Provider.requests)
            for missing in [None, 'omitted']:
                body = {'content':'must not guess branch', **target}
                if missing is None: body['version'] = None
                api(route + '/messages', body, want=400)
            api(route + '/retry', {'model':'fixture-text', **target}, want=400)
            api(route + '/retry', {'model':'fixture-text', 'version':state['chat']['version'], **target}, want=400)
            api(route + '/messages', {'content':'stale branch', 'version':original_version, **target}, want=409)
            assert api(route) == before_missing and len(Provider.requests) == provider_before_missing
            checks.append('public-send-and-retry-require-captured-version-and-response-before-any-write')
            count = len(api(route)['messages'])
            for file in [{'name': 'image.png', 'media_type': 'image/png', 'content': 'not-supported'}, {'name': '../outside.txt', 'content': 'outside'}, {'name': 'binary.txt', 'content': 'NUL\0'}, {'name': 'large.txt', 'content': 'x'*100001}]:
                api(route + '/messages', {'content': 'reject', 'version':state['chat']['version'], 'attachments': [file], **target}, want=400)
            api(route + '/messages', {'content': 'reject unknown multimodal', 'images': ['unsupported'], **target}, want=422, text=True)
            assert len(api(route)['messages']) == count
            checks.append('cross-chat-and-unsupported-oversized-binary-attachment-rejection')
            before = api(route); provider_count = len(Provider.requests)
            with sqlite3.connect(profile / 'smith-next.sqlite3') as db:
                db.execute("CREATE TRIGGER fail_new_attachments BEFORE INSERT ON text_attachments BEGIN SELECT RAISE(ABORT,'synthetic attachment failure'); END")
            api(route + '/messages', {'content': 'rollback', 'version': before['chat']['version'], 'attachments': [{'name': 'fails.txt', 'content': 'recover draft'}], **target}, want=500)
            assert api(route) == before and len(Provider.requests) == provider_count
            with sqlite3.connect(profile / 'smith-next.sqlite3') as db: db.execute('DROP TRIGGER fail_new_attachments')
            checks.append('attachment-failure-rolls-back-prompt-graph-head-version-and-provider-request')
            artifact = api('/artifacts', {'chat_id': cid, 'name': 'kept.txt', 'content': 'independent'})
            assert artifact['previous_chat_version'] == before['chat']['version'] and artifact['chat_version'] == before['chat']['version'] + 1
            api(route + '/branch', {'message_id': edit['message_id'], 'version': before['chat']['version'], **target}, want=409)
            state = api(route)
            state = api(route + '/branch', {'message_id': edit['user_message_id'], 'version': state['chat']['version'], **target})
            removed_files = api(route + '/edit', {'message_id': edit['user_message_id'], 'version': state['chat']['version'], 'content':'PROMPT WITHOUT FILES', 'attachments':[], **target}); state = complete(cid, removed_files)
            assert not state['active_messages'][0]['attachments']
            assert 'DURABLE CONTEXT' not in str(Provider.requests[-1])
            original_file = next(m for m in state['messages'] if m['id'] == first['user_message_id'])
            assert original_file['attachments'][0]['content'].startswith('DURABLE CONTEXT')
            state = api(route + '/branch', {'message_id': edit['user_message_id'], 'version': state['chat']['version'], **target})
            checks.append('explicit-attachment-removal-forks-without-modifying-original-file')
            exported = api(route + '/export', text=True)
            for word in ['ORIGINAL ROOT', 'EDITED ROOT', 'OLD DOWNSTREAM', 'DURABLE CONTEXT', 'parent=']: assert word in exported
            checks.append('artifact-version-invalidates-navigation-and-export-includes-all-branches-and-files')
            stop(); start(); restored = api(route)
            assert restored == state, (restored, state)
            config = api('/config'); target = {k: config[k] for k in ('revision', 'provider', 'endpoint')}; assert api('/connection/test', target)['ok']
            checks.append('restart-retains-selected-branch-all-statuses-and-attachment-content')
            # Deletion cascades branch links and attachments, retaining explicit artifact keep policy.
            api(route + '/purge', {'confirm_id': cid, 'version': state['chat']['version'], 'artifact_policy': 'keep', **target}, 'POST')
            api(route, want=404)
            assert api('/artifacts/' + artifact['id'], text=True) == 'independent'
            with sqlite3.connect(profile / 'smith-next.sqlite3') as db:
                assert db.execute('PRAGMA user_version').fetchone()[0] == 6
                assert not db.execute('PRAGMA foreign_key_check').fetchall()
                assert not db.execute('SELECT * FROM text_attachments').fetchall()
            checks.append('delete-cascades-graph-attachments-keeps-selected-artifact-policy')
            print(json.dumps({'result': 'pass', 'checks': checks, 'provider_requests': len(Provider.requests)}))
        finally:
            stop(); server.shutdown(); server.server_close(); thread.join(timeout=2)

if __name__ == '__main__': main()
