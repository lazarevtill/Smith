#!/usr/bin/env python3
"""Owned synthetic UDS runtime through the real app HTTP API; no real model/service."""
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
import urllib.request


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default=str(root / 'target/debug/smith-next'))
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    checks = []
    with tempfile.TemporaryDirectory(prefix='smith-native-app-') as temp:
        temp = Path(temp)
        fixture = temp / 'llama-fixture'
        subprocess.run(['rustc', '--edition=2021', str(root / 'crates/smith-managed-runtime/tests/fixtures/llama_compatible.rs'), '-o', str(fixture)], check=True)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        profile = temp / 'profile'
        env = {**os.environ, 'SMITH_NEXT_SYNTHETIC_PROFILE': 'plaintext', 'SMITH_NEXT_DATA_DIR': str(profile), 'SMITH_NEXT_PORT': str(port)}
        process = None
        child_pids = set()
        def api(path, data=None, method=None, want=200, raw=False):
            req = urllib.request.Request(base + '/api' + path, data=None if data is None else json.dumps(data).encode(), method=method, headers={'Origin': base, 'Content-Type': 'application/json'})
            try:
                with urllib.request.urlopen(req, timeout=10) as res: status, body = res.status, res.read()
            except urllib.error.HTTPError as error: status, body = error.code, error.read()
            assert status == want, (path, status, body)
            return body.decode() if raw else json.loads(body)
        def poll(fn, predicate, seconds=10):
            deadline = time.monotonic() + seconds
            while True:
                result = fn()
                if predicate(result): return result
                assert time.monotonic() < deadline, result
                time.sleep(.02)
        def status():
            state = api('/native/status')
            if state['runtime'] and state['runtime']['pid']: child_pids.add(state['runtime']['pid'])
            return state
        def operation(op, outcome='succeeded'):
            state = poll(status, lambda s: s['operation'] and s['operation']['id'] == op['operation_id'] and s['operation']['status'] != 'running')
            assert state['operation']['status'] == outcome, state
            return state
        def target(): return {k: api('/config')[k] for k in ('revision', 'provider', 'endpoint')}
        def start():
            nonlocal process
            process = subprocess.Popen([binary], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            for _ in range(200):
                if process.poll() is not None: raise AssertionError(process.stdout.read())
                try: return api('/config')
                except OSError: time.sleep(.02)
            raise AssertionError('App readiness timeout')
        def stop():
            if process and process.poll() is None:
                process.send_signal(signal.SIGINT)
                try: process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill(); process.wait(timeout=3); raise AssertionError('App did not finish owned shutdown')
                assert process.returncode == 0, process.stdout.read()
        def gone(pid):
            try: os.kill(pid, 0); return False
            except ProcessLookupError: return True
        def artifact(path):
            return {'path': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest(), 'source': 'owned synthetic integration fixture', 'source_revision': 'fixture-v1'}
        def payload(mode='normal', name='fixture-model'):
            model = temp / (name + '.gguf'); model.write_bytes(('GGUF' + mode).encode())
            return {**target(), 'executable': artifact(fixture), 'model': artifact(model), 'model_id': name, 'execution_kind': 'compatible_fixture', 'context_tokens': 128, 'threads': 1, 'startup_seconds': 30, 'generation_seconds': 30}
        def prepare(data):
            state = operation(api('/native/plans', data))
            assert state['prepared'] and state['prepared']['max_tokens'] == 64, state
            api('/native/cancel', {**target(), 'operation_id':state['operation']['id']}, want=409)
            return state['prepared']
        def load(prepared):
            return api('/native/load', {**target(), 'plan_id': prepared['plan']['id'], 'runtime_revision': prepared['runtime_revision'], 'confirm': True})
        def unload():
            s = status()['runtime']
            return operation(api('/native/unload', {**target(), 'owner': s['target']['owner'], 'generation': s['target']['generation'], 'runtime_revision': s['revision'], 'model_id': s['model_id']}))
        def chat():
            c = api('/chats', {**target(), 'title': 'Native synthetic journey'})
            return '/chats/' + c['id']
        def send(route, content='Synthetic prompt', attachments=None, want=200):
            body = {**target(), 'version': api(route)['chat']['version'], 'content': content}
            if attachments is not None: body['attachments'] = attachments
            return api(route + '/messages', body, want=want)
        def complete(route, run, expected='completed'):
            state = poll(lambda: api(route), lambda s: next(m for m in s['messages'] if m['id'] == run['message_id'])['status'] != 'streaming')
            message = next(m for m in state['messages'] if m['id'] == run['message_id'])
            assert message['status'] == expected, message
            return message
        try:
            assert start()['provider'] == 'mock'
            assert status()['runtime'] is None
            api('/config', {'provider': 'managed', 'endpoint': '', 'model': ''}, 'PUT')
            c = api('/config'); assert not c['tested']
            route = chat(); before = api(route)
            send(route, want=400); assert api(route) == before
            api('/native/plans', {**payload(), 'shell_args': ['--unsafe']}, want=422, raw=True)
            checks.append('opt-in-no-auto-probe-load-or-prompt-write-and-fixed-fields')
            prepared = prepare(payload())
            assert status()['runtime']['spawned'] == 0
            plan = prepared['plan']; assert plan['manifest']['execution_kind'] == 'compatible_fixture'
            assert plan['manifest']['executable']['sha256'] == artifact(fixture)['sha256']
            assert Path(plan['manifest']['model']['path']).is_absolute()
            for override, want in [({'confirm': False},400),({'plan_id':'wrong'},409),({'runtime_revision':prepared['runtime_revision']+1},409),({'revision':'stale'},409)]:
                api('/native/load', {**target(), 'plan_id':plan['id'], 'runtime_revision':prepared['runtime_revision'], 'confirm':True, **override}, want=want)
            loaded = operation(load(prepared)); s = loaded['runtime']
            assert s['phase'] == 'ready' and s['observed_backend'] == 'unknown' and s['spawned'] == 1 and s['reaped'] == 0
            assert api('/config')['tested'] and api('/config')['endpoint'] == 'managed://' + plan['id']
            checks.append('hash-reviewed-server-owned-plan-exact-approval-runtime-revision-and-unknown-backend')
            run = send(route, attachments=[{'name':'context.txt','content':'fixture attachment'}]); message = complete(route, run)
            assert message['content'] == 'fixture output' and message['snapshot']['provider'] == 'managed'
            events = api('/runs/' + run['run_id'] + '/events', raw=True)
            assert 'fixture output' in events and 'completed' in events
            state = api(route)
            regenerated = api(route + '/regenerate', {**target(), 'message_id':message['id'], 'version':state['chat']['version'], 'model':'fixture-model'})
            assert complete(route, regenerated)['content'] == 'fixture output'
            assert len(api(route)['messages']) == 3 and len(api(route)['active_messages']) == 2
            checks.append('complete-text-response-durable-attachment-regeneration-and-existing-branch-contract')
            before = api(route)
            send(route, content='x'*70000, attachments=[{'name': 'context.txt','content':'y'*70000}], want=400)
            assert api(route) == before
            checks.append('oversized-native-context-rejected-before-prompt-graph-write')
            # Changed file after preparation cannot replace the current runtime.
            broken = payload(name='changed-model'); prepared = prepare(broken); Path(broken['model']['path']).write_bytes(b'GGUFchanged-after-review')
            failed = operation(load(prepared), 'failed')
            assert failed['runtime']['target'] == s['target'] and failed['runtime']['phase'] == 'ready'
            assert failed['runtime']['spawned'] == 1
            assert api('/config')['model'] == 'fixture-model'
            checks.append('file-change-at-load-preserves-original-owned-runtime-and-selected-target')
            before_unload = status()['runtime']
            api('/native/unload', {**target(), 'owner':before_unload['target']['owner'], 'generation':before_unload['target']['generation'], 'runtime_revision':before_unload['revision']-1, 'model_id':'fixture-model'}, want=409)
            stopped = unload()['runtime']; assert stopped['spawned'] == stopped['reaped'] == 1 and gone(s['pid'])
            checks.append('stale-unload-rejection-and-explicit-owned-reap')
            # A loading operation blocks app target and session-auth changes.
            prepared = prepare(payload('health_stall','load-stall')); op = load(prepared)
            loading = poll(status, lambda s: s['runtime']['phase'] == 'loading' and s['runtime']['pid'])
            api('/config', {'provider':'mock','endpoint':'','model':'smith-demo'}, 'PUT', want=409)
            api('/session-auth', {'provider':'managed','endpoint':target()['endpoint'],'token':'synthetic'}, 'PUT', want=409)
            api('/session-auth', method='DELETE', want=409)
            api('/native/cancel', {**target(), 'operation_id':'wrong'}, want=409)
            api('/native/cancel', {**target(), 'operation_id':op['operation_id']})
            cancelled = operation(op, 'cancelled')['runtime']
            assert cancelled['spawned'] == cancelled['reaped'] and gone(loading['runtime']['pid'])
            checks.append('load-cancel-latched-target-auth-guards-and-reaped-before-terminal-operation')
            prepared = prepare(payload('generation_stall','response-stall')); operation(load(prepared))
            route2 = chat(); run = send(route2)
            running = poll(status, lambda s: s['runtime']['active_leases'] == 1)
            api('/native/unload', {**target(), 'owner':running['runtime']['target']['owner'], 'generation':running['runtime']['target']['generation'], 'runtime_revision':running['runtime']['revision'], 'model_id':'response-stall'}, want=409)
            api('/runs/' + run['run_id'] + '/cancel', {})
            cancelled_message = complete(route2, run, 'cancelled'); after = status()['runtime']
            assert cancelled_message['content'] == '' and after['spawned'] == after['reaped'] and gone(running['runtime']['pid'])
            assert not api('/config')['tested']
            checks.append('admitted-response-cancel-unloads-and-reaps-before-durable-terminal-state')
            # Shutdown during an admitted response waits for cleanup and persists terminal state.
            prepared = prepare(payload('generation_stall','shutdown-response')); operation(load(prepared))
            route3 = chat(); run3 = send(route3); owned = poll(status, lambda s:s['runtime']['active_leases']==1)['runtime']['pid']
            stop(); assert gone(owned)
            config = start(); assert config['provider']=='managed' and not config['tested'] and status()['runtime'] is None
            assert next(m for m in api(route3)['messages'] if m['id']==run3['message_id'])['status']=='cancelled'
            checks.append('shutdown-during-response-awaits-reap-persists-cancel-and-restart-never-autoloads')
            prepared = prepare(payload('health_stall','shutdown-load')); op = load(prepared)
            owned = poll(status, lambda s:s['runtime']['phase']=='loading' and s['runtime']['pid'])['runtime']['pid']
            stop(); assert gone(owned); start(); assert status()['runtime'] is None
            checks.append('shutdown-during-load-awaits-owned-child-cleanup')
            # Storage failure after native load cannot strand a newly created process.
            prepared = prepare(payload(name='failed-config'))
            with sqlite3.connect(profile / 'smith-next.sqlite3') as db:
                db.execute("CREATE TRIGGER fail_native_config BEFORE INSERT ON settings BEGIN SELECT RAISE(ABORT,'synthetic config failure'); END")
            failed = operation(load(prepared), 'failed')['runtime']; assert failed['spawned']==failed['reaped']==1 and failed['phase']=='stopped'
            with sqlite3.connect(profile / 'smith-next.sqlite3') as db: db.execute('DROP TRIGGER fail_native_config')
            checks.append('configuration-commit-failure-reaps-new-runtime-without-retargeting')
            stop(); assert all(gone(pid) for pid in child_pids)
            print(json.dumps({'result':'pass','checks':checks,'observed_owned_pids':len(child_pids),'real_inference':False}))
        finally:
            stop()

if __name__ == '__main__': main()
