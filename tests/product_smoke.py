#!/usr/bin/env python3
"""Synthetic demo-only acceptance smoke. Run against a disposable Smith Next profile.

SMITH_NEXT_SMOKE_URL defaults to http://127.0.0.1:18471.
Mutates only demo conversations/artifacts; refuses to generate unless provider is mock.
"""
import json
import os
import time
import urllib.error
import urllib.request

BASE = os.environ.get('SMITH_NEXT_SMOKE_URL', 'http://127.0.0.1:18471').rstrip('/')

def request(path, data=None, method=None):
    if data is not None and (path.endswith('/messages') or path.endswith('/retry')):
        visible = request(path.rsplit('/', 1)[0])
        data = dict(data)
        data.setdefault('version', visible['chat']['version'])
        if path.endswith('/retry'):
            data.setdefault('message_id', visible['active_messages'][-1]['id'])
    headers = {'Content-Type': 'application/json', 'Origin': BASE}
    payload = None if data is None else json.dumps(data).encode()
    req = urllib.request.Request(BASE + path, data=payload, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=15) as response:
        raw = response.read()
        return (json.loads(raw) if response.headers.get('Content-Type', '').startswith('application/json') else raw.decode()) if raw else None

def main():
    config = request('/api/config')
    assert config['provider'] == 'mock', 'Refusing to generate against non-demo provider'
    target = {key: config[key] for key in ('revision', 'provider', 'endpoint')}
    probe = request('/api/connection/test', target)
    assert probe['ok'], probe
    assert probe['models'], 'Demo must offer a model'
    chat = request('/api/chats', {'title': 'Synthetic acceptance smoke', **target})
    chat_id = chat.get('id') or chat.get('chat', {}).get('id')
    assert chat_id, chat
    run = request(f'/api/chats/{chat_id}/messages', {'content': 'Synthetic test: explain privacy in one paragraph.', **target})
    run_id = run['run_id']
    events = []
    req = urllib.request.Request(BASE + f'/api/runs/{run_id}/events', headers={'Accept': 'text/event-stream'})
    with urllib.request.urlopen(req, timeout=20) as response:
        for line in response:
            text = line.decode().strip()
            if text.startswith('data:'):
                event = json.loads(text[5:].strip())
                events.append(event)
                if event.get('status') in ('completed', 'cancelled', 'failed', 'interrupted'):
                    break
    assert events and events[-1]['status'] == 'completed', events
    content = events[-1]['content']
    assert content, 'Completed demo response must have content'
    saved = request(f'/api/chats/{chat_id}')
    assert any(m.get('content') == content for m in saved['messages']), saved
    artifact = request('/api/artifacts', {'chat_id': chat_id, 'name': 'Synthetic note', 'content': content})
    artifact_id = artifact.get('id') or artifact.get('artifact', {}).get('id')
    assert artifact_id, artifact
    loaded = request(f'/api/artifacts/{artifact_id}')
    assert loaded == content, loaded
    second = request(f'/api/chats/{chat_id}/messages', {'content': 'Synthetic stop and retry test.', **target})
    time.sleep(0.18)
    request(f"/api/runs/{second['run_id']}/cancel", {})
    deadline = time.monotonic() + 2
    while True:
        cancelled = request(f'/api/chats/{chat_id}')['messages'][-1]
        if cancelled['status'] == 'cancelled':
            break
        assert time.monotonic() < deadline, cancelled
        time.sleep(0.025)
    assert cancelled['content'], 'Partial text should survive cancellation'
    before_retry = request(f'/api/chats/{chat_id}')['messages']
    retry = request(f'/api/chats/{chat_id}/retry', {**target, 'model': config['model']})
    assert retry['run_id'] != second['run_id'], 'Retry must be a new attempt'
    request(f"/api/runs/{retry['run_id']}/cancel", {})
    time.sleep(0.1)
    after_retry = request(f'/api/chats/{chat_id}')['messages']
    assert len(after_retry) == len(before_retry) + 1, 'Retry should not duplicate user prompt'
    assert sum(m['role'] == 'user' for m in after_retry) == 2, after_retry
    assert after_retry[-2]['status'] == 'cancelled', 'Original attempt must remain cancelled'
    exported = request(f'/api/chats/{chat_id}/export')
    assert content in exported and 'cancelled' in exported, exported
    print(json.dumps({'result': 'pass', 'chat_id': chat_id, 'run_id': run_id,
                      'stream_events': len(events), 'artifact_id': artifact_id}))

if __name__ == '__main__':
    main()
