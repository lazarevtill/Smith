#!/usr/bin/env python3
"""Local synthetic inference fixture for manual browser review; not an AI model.

Run: python3 tests/fixture_server.py --port 18479
Choose llama.cpp in Smith, endpoint http://127.0.0.1:18479/v1.
Optional --require-demo-token requires the public fixture string smith-fixture-only.
Never forward requests, load models, access files, or use real credentials.
"""
import argparse
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Fixture(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass  # Do not log prompts, authorization headers or request bodies.

    def allowed(self):
        if self.server.require_token and self.headers.get('Authorization') != 'Bearer smith-fixture-only':
            self.respond(401, {'error': 'Synthetic fixture requires its public demo token.'})
            return False
        return True

    def respond(self, code, value):
        body = json.dumps(value).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if not self.allowed():
            return
        if self.path == '/v1/models':
            self.respond(200, {'data': [{'id': 'smith-http-fixture', 'object': 'model'}]})
        else:
            self.respond(404, {'error': 'Synthetic route not found.'})

    def do_POST(self):
        if not self.allowed():
            return
        length = int(self.headers.get('Content-Length', '0'))
        if length > 120_000:
            self.respond(413, {'error': 'Synthetic fixture request limit.'})
            return
        try:
            payload = json.loads(self.rfile.read(length))
        except (ValueError, OSError):
            self.respond(400, {'error': 'Invalid synthetic request.'})
            return
        if self.path != '/v1/chat/completions' or payload.get('model') != 'smith-http-fixture':
            self.respond(404, {'error': 'Choose smith-http-fixture.'})
            return
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        text = ('This answer travelled through a real local HTTP connection. '
                'It is a synthetic test fixture, not neural-network inference.\n\n'
                'Привет 🌍 — streaming, cancellation, saved history and text artifacts '
                'can be reviewed without a downloaded model or external service.')
        try:
            for word in text.split(' '):
                self.event({'choices': [{'index': 0, 'delta': {'content': word + ' '}, 'finish_reason': None}]})
                time.sleep(0.1)
            self.event({'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]})
            self.wfile.write(b'data: [DONE]\n\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # Client cancellation is expected.

    def event(self, data):
        self.wfile.write(('data: ' + json.dumps(data, ensure_ascii=False) + '\n\n').encode())
        self.wfile.flush()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=18479)
    parser.add_argument('--require-demo-token', action='store_true')
    args = parser.parse_args()
    server = ThreadingHTTPServer(('127.0.0.1', args.port), Fixture)
    server.require_token = args.require_demo_token
    print(f'Synthetic HTTP fixture: http://127.0.0.1:{server.server_port}/v1; no real AI or forwarding.', flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        server.server_close()
