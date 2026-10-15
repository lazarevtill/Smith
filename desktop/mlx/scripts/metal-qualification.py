#!/usr/bin/env python3
"""Owned native-process test only. Creates tiny weights locally; never downloads a model."""
import hashlib
import json
import os
import pathlib
import select
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
BINARY = (ROOT / 'dist/smith-mlx-worker').resolve()
GENERATOR = (ROOT / 'dist/smith-mlx-fixture').resolve()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Child:
    def __init__(self, scratch, env, binary=BINARY):
        self.process = subprocess.Popen(
            [str(binary), '--scratch-root', str(scratch)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=env, bufsize=0)
        self.buffer = b''

    def send(self, request_id, op, **fields):
        line = json.dumps({'v': 1, 'id': request_id, 'op': op, **fields}) + '\n'
        self.process.stdin.write(line.encode())
        self.process.stdin.flush()

    def event(self, timeout=40):
        deadline = time.monotonic() + timeout
        while b'\n' not in self.buffer:
            assert time.monotonic() < deadline, 'native response timeout'
            ready, _, _ = select.select([self.process.stdout], [], [], 0.1)
            if ready:
                chunk = os.read(self.process.stdout.fileno(), 4096)
                if not chunk:
                    self.process.wait(timeout=5)
                    error = self.process.stderr.read(8192).decode(errors='replace')
                    raise AssertionError(f'native EOF exit={self.process.returncode}: {error}')
                self.buffer += chunk
                assert len(self.buffer) <= 131072 + 4096, 'unbounded native response'
        line, self.buffer = self.buffer.split(b'\n', 1)
        return json.loads(line)

    def request(self, request_id, op, **fields):
        self.send(request_id, op, **fields)
        events = []
        while True:
            event = self.event()
            assert event['v'] == 1 and event['id'] == request_id, event
            events.append(event)
            if event['event'] in ['done', 'error']:
                return events

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)


def reconstructed(events):
    value = b''
    for event in events:
        if event['event'] == 'delta':
            offset = event['replace_from_utf8']
            assert 0 <= offset <= len(value)
            value[:offset].decode('utf-8')
            value = value[:offset] + event['text'].encode('utf-8')
    return value.decode('utf-8')


checks = []
started = time.monotonic()
with tempfile.TemporaryDirectory(prefix='metal-qualification-', dir=ROOT / '.build') as temporary:
    base = pathlib.Path(temporary).resolve()
    scratch = base / 'scratch'
    scratch.mkdir(mode=0o700)
    model = base / 'synthetic-model'
    env = {'PATH': '/usr/bin:/bin', 'TMPDIR': str(base),
           'CLANG_MODULE_CACHE_PATH': str(ROOT / '.module-cache')}
    generated = subprocess.run([str(GENERATOR), str(model)], env=env,
                               capture_output=True, timeout=45, check=True)
    fixture = json.loads(generated.stdout)
    assert fixture['fixture'] == 'smith-synthetic-llama-constant-A-v1'
    assert fixture['mlx_device'] == 'gpu' and not fixture['weights_downloaded']
    weights_sha256 = digest(model / 'model.safetensors')
    weights_bytes = (model / 'model.safetensors').stat().st_size
    checks.append('official-native-constructors-and-save-local-deterministic-weights')
    runtime = base / 'reviewed-runtime'
    runtime.mkdir(mode=0o700)
    shutil.copy2(BINARY, runtime / BINARY.name)
    shutil.copy2(ROOT / 'dist/mlx.metallib', runtime / 'mlx.metallib')
    for directory in ['mlx-swift_Cmlx.bundle', 'swift-transformers_Hub.bundle',
                      'swift-crypto_Crypto.bundle']:
        shutil.copytree(ROOT / 'dist' / directory, runtime / directory)
    assert digest(runtime / BINARY.name) == digest(BINARY)
    child = Child(scratch, env, binary=runtime / BINARY.name)
    try:
        result = child.request('hello', 'hello')[-1]
        assert result['backend'] == 'mlx-swift-metal', result
        result = child.request('inspect', 'inspect', model_root=str(model))[-1]
        assert result['event'] == 'done' and result['capabilities'] == ['raw_completion', 'chat'], result
        manifest = result['manifest']
        assert manifest['template'] == 'simple-chatml-v1'
        files = manifest['files']
        expected = ''.join(f"{f['name']}\0{f['bytes']}\0{f['sha256']}\n" for f in files).encode()
        assert hashlib.sha256(expected).hexdigest() == manifest['package_sha256']
        for entry in files:
            assert digest(model / entry['name']) == entry['sha256']
        checks.append('independent-selected-package-hash-and-chat-capability')
        result = child.request('load', 'load', model_root=str(model),
                               expected_package_sha256=manifest['package_sha256'])[-1]
        assert result['event'] == 'done' and result['manifest'] == manifest, result
        assert len(list(scratch.iterdir())) == 1
        checks.append('actual-mlx-metal-load-from-copied-runtime-with-approved-content-hash')
        for mode, fields in [('raw', {'prompt': 'Synthetic fixture only.'}),
                             ('chat', {'messages': [
                                 {'role': 'system', 'content': 'Synthetic fixture only.'},
                                 {'role': 'user', 'content': 'Тест GPU.'}]})]:
            events = child.request(mode, 'generate', mode=mode, max_tokens=8,
                                   temperature=0, top_p=1, seed=7, **fields)
            assert events[-1]['event'] == 'done', events[-1]
            assert events[-1]['output_tokens'] == 8 and events[-1]['finish_reason'] == 'length'
            assert reconstructed(events) == 'A' * 8
            checks.append(f'actual-metal-{mode}-eight-exact-tokens-and-utf8-patches')
        result = child.request('budget', 'generate', mode='raw', prompt='x' * 8190,
                               max_tokens=8, temperature=0, top_p=1)[-1]
        assert result['code'] == 'context_limit', result
        checks.append('complete-input-context-budget-refusal')
        child.send('long', 'generate', mode='raw', prompt='Cancel synthetic GPU work.',
                   max_tokens=2048, temperature=0, top_p=1)
        while True:
            event = child.event()
            assert event['id'] == 'long' and event['event'] not in ['done', 'error'], event
            if event['event'] == 'delta':
                break
        cancelled_at = time.monotonic()
        child.send('cancel', 'cancel', target_id='long')
        terminals = {}
        while len(terminals) < 2:
            event = child.event(timeout=10)
            assert event['id'] in ['long', 'cancel'], event
            if event['event'] in ['done', 'error']:
                terminals[event['id']] = event
        cancel_seconds = round(time.monotonic() - cancelled_at, 4)
        assert terminals['cancel']['event'] == 'done'
        assert terminals['long']['code'] == 'cancelled', terminals
        checks.append('cancel-active-metal-generation-and-drain-owned-producer')
        result = child.request('unload', 'unload')[-1]
        assert result['event'] == 'done' and list(scratch.iterdir()) == [], result
        result = child.request('empty', 'generate', mode='raw', prompt='x', max_tokens=1,
                               temperature=0, top_p=1)[-1]
        assert result['code'] == 'no_model', result
        checks.append('unload-removes-private-snapshot-and-model-authority')
        result = child.request('reload', 'load', model_root=str(model),
                               expected_package_sha256=manifest['package_sha256'])[-1]
        assert result['event'] == 'done', result
        child.send('eof-active', 'generate', mode='raw', prompt='EOF synthetic GPU work.',
                   max_tokens=2048, temperature=0, top_p=1)
        while True:
            event = child.event()
            assert event['id'] == 'eof-active' and event['event'] not in ['done', 'error'], event
            if event['event'] == 'delta':
                break
        child.process.stdin.close()
        result = child.event(timeout=10)
        while result['event'] == 'delta':
            result = child.event(timeout=10)
        assert result['event'] == 'error' and result['code'] == 'cancelled', result
        assert child.process.wait(timeout=10) == 0
        assert list(scratch.iterdir()) == []
        checks.append('eof-during-metal-work-drains-unloads-and-owned-child-reaped')
    finally:
        child.close()
    # The upstream runtime has build/bundle fallback paths. A corrupt reviewed local library
    # must be refused explicitly, even though a valid library remains in the original bundle.
    corrupt = base / 'corrupt-runtime'
    corrupt.mkdir()
    shutil.copy2(BINARY, corrupt / BINARY.name)
    for directory in ['mlx-swift_Cmlx.bundle', 'swift-transformers_Hub.bundle',
                      'swift-crypto_Crypto.bundle']:
        shutil.copytree(ROOT / 'dist' / directory, corrupt / directory)
    (corrupt / 'mlx.metallib').write_bytes(b'MTLBsynthetic-invalid-library')
    (corrupt / 'mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib').write_bytes(
        b'MTLBsynthetic-invalid-library')
    child = Child(scratch, env, binary=corrupt / BINARY.name)
    try:
        result = child.request('corrupt-library', 'load', model_root=str(model),
                               expected_package_sha256=manifest['package_sha256'])[-1]
        assert result['code'] == 'invalid_metallib', result
        assert child.request('close', 'shutdown')[-1]['event'] == 'done'
        child.process.stdin.close()
        assert child.process.wait(timeout=10) == 0 and list(scratch.iterdir()) == []
        checks.append('corrupt-colocated-metal-library-refused-without-upstream-fallback')
    finally:
        child.close()

report = {
    'schema': 1, 'date_utc': '2026-10-15', 'fixture': fixture,
    'worker_sha256': digest(BINARY), 'fixture_generator_sha256': digest(GENERATOR),
    'metallib_sha256': digest(ROOT / 'dist/mlx.metallib'),
    'synthetic_weights_sha256': weights_sha256, 'synthetic_weights_bytes': weights_bytes,
    'synthetic_package_sha256': manifest['package_sha256'],
    'native_metal_inference': True, 'external_weights_or_services': False,
    'language_quality_evaluated': False, 'cancel_seconds': cancel_seconds,
    'elapsed_seconds': round(time.monotonic() - started, 3),
    'checks': checks, 'passed': len(checks),
}
print(json.dumps(report, indent=2))
