#!/usr/bin/env python3
"""Disposable offline chat-management and embedded-font HTTP parity checks.
No existing profile, external inference service, credentials, or real data used.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default=str(root / 'target/debug/smith-next'))
    args = parser.parse_args()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
    base = f'http://127.0.0.1:{port}'
    checks = []
    with tempfile.TemporaryDirectory(prefix='smith-management-') as temp:
        profile = Path(temp) / 'profile'
        process = subprocess.Popen([str(Path(args.binary).resolve())], env={**os.environ, 'SMITH_NEXT_SYNTHETIC_PROFILE': 'plaintext', 'SMITH_NEXT_DATA_DIR': str(profile), 'SMITH_NEXT_PORT': str(port)}, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        def raw(path, data=None, method=None):
            request = urllib.request.Request(base + path, data=None if data is None else json.dumps(data).encode(), method=method, headers={'Origin': base, 'Content-Type': 'application/json'})
            try:
                with urllib.request.urlopen(request, timeout=10) as response:
                    return response.status, response.headers, response.read()
            except urllib.error.HTTPError as error:
                return error.code, error.headers, error.read()
        def api(path, data=None, method=None, want=200):
            if data is not None and (path.endswith('/messages') or path.endswith('/retry')):
                visible = api(path.rsplit('/', 1)[0])
                data = dict(data)
                data.setdefault('version', visible['chat']['version'])
                if path.endswith('/retry'):
                    data.setdefault('message_id', visible['active_messages'][-1]['id'])
            code, _, body = raw('/api' + path, data, method)
            assert code == want, (path, code, body)
            return json.loads(body) if body else None
        try:
            for _ in range(150):
                if process.poll() is not None: raise AssertionError(process.stdout.read())
                try:
                    config = api('/config'); break
                except OSError: time.sleep(.025)
            else: raise AssertionError('Readiness timeout')
            assert config['provider'] == 'mock'
            target = {key: config[key] for key in ('revision', 'provider', 'endpoint')}
            first = api('/chats', {'title': 'First', **target})
            second = api('/chats', {'title': 'Unrelated', **target})
            renamed = api('/chats/' + first['id'], {'title': '  ПРОЕКТ 🦀  ', 'version': first['version'], **target}, 'PATCH')
            assert renamed['title'] == 'ПРОЕКТ 🦀' and renamed['version'] > first['version']
            api('/chats/' + first['id'], {'title': 'stale', 'version': first['version'], **target}, 'PATCH', 409)
            api('/chats/' + first['id'], {'title': '   ', 'version': renamed['version'], **target}, 'PATCH', 400)
            assert api('/chats?q=' + urllib.parse.quote('проект'))[0]['id'] == first['id']
            assert api('/chats')[0]['id'] == first['id']
            checks.append('rename-trim-unicode-search-latest-order-version-conflict')
            api('/chats/' + first['id'] + '/messages', {'content': 'Synthetic ordinary prompt', **target})
            current = api('/chats/' + first['id'])
            assert current['chat']['title'] == renamed['title'], 'First prompt overwrote user title'
            delete = {'confirm_id': first['id'], 'version': current['chat']['version'], 'artifact_policy': 'keep', **target}
            api('/chats/' + first['id'] + '/purge', delete, 'POST', 409)
            run_id = current['messages'][-1]['run_id']
            api('/runs/' + run_id + '/cancel', {})
            for _ in range(100):
                current = api('/chats/' + first['id'])
                if current['messages'][-1]['status'] != 'streaming': break
                time.sleep(.01)
            assert current['messages'][-1]['status'] == 'cancelled'
            checks.append('user-title-preserved-active-delete-blocked')
            kept = api('/artifacts', {'chat_id': first['id'], 'name': 'keep.txt', 'content': 'Synthetic independent artifact 🦀'})
            api('/chats/' + first['id'] + '/purge', {**delete, 'artifact_policy': 'delete'}, 'POST', 409)
            assert raw('/api/artifacts/' + kept['id'])[2].decode() == kept['content']
            assert api('/chats/' + first['id'])['messages']
            checks.append('artifact-save-invalidates-old-delete-intent-without-data-loss')
            delete['version'] = api('/chats/' + first['id'])['chat']['version']
            api('/chats/' + first['id'] + '/purge', {**delete, 'confirm_id': 'wrong'}, 'POST', 400)
            result = api('/chats/' + first['id'] + '/purge', delete, 'POST')
            assert result['artifacts_kept'] == 1
            api('/chats/' + first['id'], want=404)
            assert raw('/api/chats/' + first['id'] + '/export')[0] == 404
            assert raw('/api/runs/' + run_id + '/events')[0] == 404
            retained = api('/artifacts')
            assert len(retained) == 1 and retained[0]['id'] == kept['id'] and retained[0]['chat_id'] is None
            assert raw('/api/artifacts/' + kept['id'])[2].decode() == kept['content']
            checks.append('atomic-delete-chat-attempts-retained-independent-artifact')
            gone = api('/artifacts', {'chat_id': second['id'], 'name': 'delete.txt', 'content': 'Synthetic explicitly deleted artifact'})
            second = api('/chats/' + second['id'])['chat']
            result = api('/chats/' + second['id'] + '/purge', {'confirm_id': second['id'], 'version': second['version'], 'artifact_policy': 'delete', **target}, 'POST')
            assert result['artifacts_deleted'] == 1
            assert raw('/api/artifacts/' + gone['id'])[0] == 404
            assert api('/artifacts')[0]['id'] == kept['id']
            checks.append('explicit-delete-artifacts-keeps-unrelated-artifact')
            third = api('/chats', {'title': 'Stale intent protected', **target})
            api('/config', {key: config[key] for key in ('provider', 'endpoint', 'model')}, 'PUT')
            api('/chats/' + third['id'], {'title': 'must not apply', 'version': third['version'], **target}, 'PATCH', 409)
            api('/chats/' + third['id'] + '/purge', {'confirm_id': third['id'], 'version': third['version'], 'artifact_policy': 'delete', **target}, 'POST', 409)
            assert api('/chats/' + third['id'])['chat']['title'] == third['title']
            checks.append('stale-consent-rename-delete-no-write')
            for filename in ('inter-400.woff2', 'inter-500.woff2', 'inter-600.woff2', 'poppins-500.woff2', 'poppins-600.woff2', 'spline-sans-mono-400.woff2'):
                code, headers, body = raw('/assets/fonts/' + filename)
                assert code == 200 and headers.get_content_type() == 'font/woff2'
                assert hashlib.sha256(body).digest() == hashlib.sha256((root / 'web/assets/fonts' / filename).read_bytes()).digest()
            checks.append('six-fixed-embedded-fonts-correct-mime-unchanged-hashes')
            for filename in ('Inter-OFL.txt', 'Poppins-OFL.txt', 'SplineSansMono-OFL.txt'):
                code, headers, body = raw('/assets/fonts/licenses/' + filename)
                assert code == 200 and headers.get_content_type() == 'text/plain'
                assert body == (root / 'web/assets/fonts/licenses' / filename).read_bytes()
            for unknown in ('missing.woff2', '../Cargo.toml', 'licenses/missing.txt'):
                assert raw('/assets/fonts/' + unknown)[0] == 404
            checks.append('three-exact-license-notices-unknown-assets-404')
            process.send_signal(signal.SIGINT); process.wait(timeout=3)
            assert process.returncode == 0
            db = sqlite3.connect(profile / 'smith-next.sqlite3')
            assert db.execute('PRAGMA user_version').fetchone()[0] == 6
            assert db.execute('PRAGMA foreign_key_check').fetchall() == []
            assert db.execute('SELECT count(*) FROM messages').fetchone()[0] == 0
            assert db.execute('SELECT count(*) FROM run_configs').fetchone()[0] == 0
            assert db.execute('SELECT content FROM artifacts').fetchone()[0] == kept['content']
            db.close()
            checks.append('schema6-foreign-keys-and-durable-cascade')
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try: process.wait(timeout=3)
                except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=3)
    print(json.dumps({'result': 'pass', 'checks': checks, 'total': len(checks)}, ensure_ascii=False))


if __name__ == '__main__': main()
