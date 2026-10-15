#!/usr/bin/env python3
"""Black-box provider faults. Only synthetic prompts, ephemeral loopback servers/data.
Run: python3 tests/provider_faults.py --binary target/debug/smith-next
Build the binary first. No real provider, credentials, DNS, or user profile is used.
"""
import argparse
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


class Fixture(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.0'

    def log_message(self, *_):
        pass

    def respond(self, value, code=200):
        raw = json.dumps(value).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        self.server.requests.append(('GET', self.path))
        if self.server.mode == 'redirect':
            self.send_response(302)
            self.send_header('Location', self.server.redirect_target)
            self.end_headers()
        elif self.path.endswith('/api/tags'):
            self.respond({'models': [{'name': self.server.model, 'remote_host': 'https://ollama.com'}] if self.server.mode == 'cloud' else [{'name': self.server.model, 'size': 1000000, 'details': {'family': 'llama', 'parameter_size': '1B'}}]})
        elif self.path.endswith('/models'):
            self.respond({'data': [{'id': self.server.model}]})
        else:
            self.respond({'error': 'unknown synthetic route'}, 404)

    def do_POST(self):
        self.server.requests.append(('POST', self.path))
        body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
        if self.path.endswith('/api/show'):
            self.respond({'remote_host': 'https://ollama.com', 'capabilities': ['completion']} if self.server.mode == 'cloud' else {'capabilities': ['completion'], 'details': {'family': 'llama'}, 'model_info': {'general.architecture': 'llama'}})
            return
        self.server.prompts.append(body)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream' if '/chat/completions' in self.path else 'application/x-ndjson')
        self.end_headers()
        if self.server.mode == 'silent':
            self.server.release.wait(8)
            return
        ollama = '/api/chat' in self.path
        if not ollama and self.server.mode.startswith('sse_'):
            try:
                raw = self.sse_fixture(self.server.mode)
                if self.server.mode == 'sse_error_coalesced':
                    self.wfile.write(raw)
                    self.wfile.flush()
                    return
                # Force every UTF-8 byte and CRLF boundary through independent writes.
                for byte in raw:
                    self.wfile.write(bytes([byte]))
                    self.wfile.flush()
                    time.sleep(.0002)
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        frames = [{'message': {'content': 'Synthetic partial'}, 'done': False}] if ollama else [{'choices': [{'index': 0, 'delta': {'content': 'Synthetic partial'}, 'finish_reason': None}]}]
        mode = self.server.mode
        if mode in ('error', 'coalesced_error'):
            frames.append({'error': {'message': 'Synthetic provider fault'}})
        elif mode == 'success':
            frames.append({'done': True} if ollama else {'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]})
        elif mode == 'finish_without_done':
            frames.append({'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]})
        try:
            if mode == 'coalesced_error':
                self.wfile.write(b''.join((json.dumps(frame) + '\n').encode() for frame in frames))
                self.wfile.flush()
                return
            for frame in frames:
                self.wfile.write((('' if ollama else 'data: ') + json.dumps(frame) + ('\n' if ollama else '\n\n')).encode())
                self.wfile.flush()
            if not ollama and mode == 'success':
                self.wfile.write(b'data: [DONE]\n\n')
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    @staticmethod
    def sse_fixture(mode):
        def event(value, ending='\n', multiline=False):
            payload = json.dumps(value, ensure_ascii=False)
            if multiline:
                # SSE joins consecutive data fields with LF, valid JSON whitespace.
                payload = payload.replace(', "finish_reason"', ',\ndata: "finish_reason"')
            return ('data: ' + payload + ending + ending).encode()
        delta = {'choices': [{'index': 0, 'delta': {'content': 'Привет 🌍'}, 'finish_reason': None}]}
        stop = {'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]}
        if mode == 'sse_crlf_bom':
            return b'\xef\xbb\xbf: heartbeat\r\n\r\n' + event(delta, '\r\n') + event(stop, '\r\n') + b'data: [DONE]\r\n\r\n'
        if mode == 'sse_cr':
            return event(delta, '\r') + event(stop, '\r') + b'data: [DONE]\r\r'
        if mode == 'sse_multiline':
            return event(delta, multiline=True) + event(stop) + b'data: [DONE]\n\n'
        if mode == 'sse_choice_order':
            return event({'choices': [{'index': 1, 'delta': {'content': 'WRONG'}, 'finish_reason': None}, delta['choices'][0]]}) + event(stop) + b'data: [DONE]\n\n'
        if mode in ('sse_error_event', 'sse_error_coalesced'):
            return event(delta) + b'event: error\ndata: {"message":"Synthetic error event"}\n\n'
        if mode == 'sse_error_terminal_payload':
            return event(delta) + b'event: error\n' + event(stop)
        if mode == 'sse_done_only':
            return event(delta) + b'data: [DONE]\n\n'
        if mode in ('sse_length', 'sse_tool_calls'):
            reason = mode.removeprefix('sse_')
            terminal = {'choices': [{'index': 0, 'delta': {}, 'finish_reason': reason}]}
            return event(delta) + event(terminal) + b'data: [DONE]\n\n'
        raise AssertionError(mode)


class Suite:
    def __init__(self, binary):
        self.binary = str(pathlib.Path(binary).resolve())
        self.results = []
        self.mock = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
        self.mock.daemon_threads = True
        self.mock.requests, self.mock.prompts = [], []
        self.mock.release = threading.Event()
        self.mock.model = 'synthetic-local:1b'
        self.mock.mode = 'success'
        threading.Thread(target=self.mock.serve_forever, daemon=True).start()
        self.endpoint = 'http://127.0.0.1:%d' % self.mock.server_port

    def req(self, path, data=None, method=None):
        req = urllib.request.Request(self.base + path, data=None if data is None else json.dumps(data).encode(), method=method, headers={'Content-Type': 'application/json', 'Origin': self.base})
        try:
            with urllib.request.urlopen(req, timeout=3) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.load(error)

    def configure(self, provider='ollama', mode='success', endpoint=None):
        self.mock.mode = mode
        self.mock.model = 'synthetic-cloud' if mode == 'cloud' else 'synthetic-local:1b'
        code, data = self.req('/api/config', {'provider': provider, 'endpoint': endpoint or (self.endpoint + ('/v1' if provider == 'openai' else '')), 'model': self.mock.model}, 'PUT')
        assert code == 200, (code, data)
        code, self.consent = self.req('/api/config')
        assert code == 200 and self.consent.get('revision'), self.consent
        assert self.consent['provider'] == provider, self.consent
        assert self.consent['endpoint'] == (endpoint or (self.endpoint + ('/v1' if provider == 'openai' else ''))), self.consent
        result = self.req('/api/connection/test', {key: self.consent[key] for key in ('revision', 'provider', 'endpoint')}, 'POST')
        if result[0] == 200:
            code, catalog = self.req('/api/models?' + urllib.parse.urlencode({'revision': self.consent['revision']}))
            assert code == 200, catalog
            for key in ('revision', 'provider', 'endpoint'):
                assert catalog[key] == self.consent[key], catalog
            assert isinstance(catalog['models'], list), catalog
        return result

    def start(self, model=None):
        code, chat = self.req('/api/chats', {'title': 'Synthetic fault fixture', **{key: self.consent[key] for key in ('revision', 'provider', 'endpoint')}})
        assert code == 200, chat
        code, run = self.req('/api/chats/%s/messages' % chat['id'], {'content': 'Synthetic provider fixture only.', 'version': chat['version'], **{key: self.consent[key] for key in ('revision', 'provider', 'endpoint')}, 'model': model or self.consent['model']})
        return code, run, chat['id']

    def terminal(self, chat, timeout=4):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            _, saved = self.req('/api/chats/' + chat)
            answer = saved['messages'][-1]
            if answer['status'] != 'streaming':
                return answer
            time.sleep(.025)
        raise AssertionError('No terminal persisted state within %.1fs' % timeout)

    def case(self, name, fn):
        try:
            detail = fn()
            self.results.append({'test': name, 'result': 'pass', 'evidence': detail})
        except Exception as error:
            self.results.append({'test': name, 'result': 'fail', 'evidence': str(error)})

    def stream_case(self, provider, mode, expected):
        code, probe = self.configure(provider, mode)
        assert code == 200, probe
        before = len(self.mock.prompts)
        code, run, chat = self.start()
        assert code == 200, run
        answer = self.terminal(chat)
        assert answer['status'] == expected, answer
        assert answer['content'] == 'Synthetic partial', answer
        if provider == 'openai' and mode == 'finish_without_done':
            assert answer.get('warning') and answer.get('transport_incomplete') is True, answer
            assert answer.get('finish_reason') == 'stop', answer
        assert len(self.mock.prompts) == before + 1, 'Unexpected fallback or duplicate request'
        return {'status': answer['status'], 'partial_saved': True, 'generation_requests': 1}

    def sse_case(self, mode, expected):
        assert self.configure('openai', mode)[0] == 200
        before = len(self.mock.prompts)
        code, run, chat = self.start()
        assert code == 200, run
        answer = self.terminal(chat)
        assert answer['status'] == expected, answer
        assert answer['content'] == 'Привет 🌍', answer
        if mode == 'sse_length':
            assert answer.get('finish_reason') == 'length' and answer.get('warning'), answer
        assert len(self.mock.prompts) == before + 1, 'Unexpected fallback'
        return {'status': answer['status'], 'exact_unicode_saved': True, 'single_byte_writes': mode != 'sse_error_coalesced'}

    def cancel(self):
        assert self.configure(mode='silent')[0] == 200
        code, run, chat = self.start()
        assert code == 200, run
        time.sleep(.1)
        began = time.monotonic()
        assert self.req('/api/runs/%s/cancel' % run['run_id'], {}, 'POST')[0] == 200
        answer = self.terminal(chat, 2)
        assert answer['status'] == 'cancelled' and answer['content'] == '', answer
        return {'status': 'cancelled', 'elapsed_ms': round((time.monotonic()-began)*1000)}

    def cloud(self):
        before = len(self.mock.prompts)
        code, probe = self.configure(mode='cloud')
        code, result, _ = self.start()
        assert code >= 400, {'code': code, 'response': result, 'probe': probe}
        assert len(self.mock.prompts) == before, 'Cloud-labelled model received prompt'
        return 'Cloud-labelled model blocked before prompt'

    def unavailable(self):
        assert self.configure()[0] == 200
        before = len(self.mock.prompts)
        code, result, _ = self.start('arbitrary-cloud-model')
        assert code >= 400, result
        assert len(self.mock.prompts) == before
        return 'Undiscovered model rejected without generation'

    def public(self):
        before = len(self.mock.requests)
        for endpoint in ['https://example.com', 'http://8.8.8.8', 'http://169.254.169.254', 'http://127.0.0.1/?secret=x']:
            code, result = self.req('/api/config', {'provider': 'ollama', 'endpoint': endpoint, 'model': 'synthetic'}, 'PUT')
            assert code >= 400, (endpoint, result)
        assert len(self.mock.requests) == before
        return 'Public DNS/IP, metadata IP and query-bearing endpoint rejected'

    def redirect(self):
        sink = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
        sink.daemon_threads = True
        sink.requests = []
        threading.Thread(target=sink.serve_forever, daemon=True).start()
        try:
            self.mock.redirect_target = 'http://127.0.0.1:%d/api/tags' % sink.server_port
            code, response = self.configure(mode='redirect')
            assert code >= 400, response
            assert sink.requests == [], 'Discovery followed redirect'
            return '302 rejected; redirected loopback destination received zero requests'
        finally:
            sink.shutdown()
            sink.server_close()

    def run(self):
        with tempfile.TemporaryDirectory(prefix='smith-provider-faults-') as profile:
            self.base = 'http://127.0.0.1:%d' % free_port()
            env = dict(os.environ, SMITH_NEXT_SYNTHETIC_PROFILE='plaintext', SMITH_NEXT_DATA_DIR=profile, SMITH_NEXT_PORT=self.base.rsplit(':', 1)[1])
            with tempfile.TemporaryFile() as log:
                process = subprocess.Popen([self.binary], env=env, stdout=log, stderr=log)
                try:
                    for _ in range(150):
                        try:
                            if self.req('/api/config')[0] == 200:
                                break
                        except (OSError, urllib.error.URLError):
                            time.sleep(.05)
                    else:
                        raise RuntimeError('Disposable app failed to start')
                    for provider in ['ollama', 'openai']:
                        self.case(provider + '_success', lambda p=provider: self.stream_case(p, 'success', 'completed'))
                        self.case(provider + '_error_after_200', lambda p=provider: self.stream_case(p, 'error', 'failed'))
                        self.case(provider + '_EOF_without_completion', lambda p=provider: self.stream_case(p, 'eof', 'failed'))
                    self.case('ollama_coalesced_error', lambda: self.stream_case('ollama', 'coalesced_error', 'failed'))
                    self.case('openai_finish_reason_without_DONE', lambda: self.stream_case('openai', 'finish_without_done', 'completed'))
                    for mode in ['sse_crlf_bom', 'sse_cr', 'sse_multiline', 'sse_choice_order']:
                        self.case(mode, lambda m=mode: self.sse_case(m, 'completed'))
                    for mode in ['sse_error_event', 'sse_error_coalesced', 'sse_error_terminal_payload', 'sse_done_only', 'sse_tool_calls']:
                        self.case(mode, lambda m=mode: self.sse_case(m, 'failed'))
                    self.case('sse_length', lambda: self.sse_case('sse_length', 'limited'))
                    self.case('cancel_before_first_token', self.cancel)
                    self.case('cloud_provenance_block', self.cloud)
                    self.case('undiscovered_model_block', self.unavailable)
                    self.case('public_endpoint_block', self.public)
                    self.case('redirect_block', self.redirect)
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                    self.mock.release.set()
                    self.mock.shutdown()
                    self.mock.server_close()
        return self.results


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default='target/debug/smith-next')
    args = parser.parse_args()
    result = Suite(args.binary).run()
    print(json.dumps({'suite': 'synthetic provider faults', 'results': result, 'passed': sum(r['result'] == 'pass' for r in result), 'total': len(result)}, indent=2))
    raise SystemExit(any(r['result'] != 'pass' for r in result))
