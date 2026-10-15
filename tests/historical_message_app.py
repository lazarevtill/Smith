#!/usr/bin/env python3
"""Owned normal HTTP/SQLCipher historical-card qualification; synthetic data only."""
import argparse
import base64
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import secrets
import selectors
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlencode

ROOT = Path(__file__).resolve().parents[1]
UNSET = object()


def require(condition, code):
    if not condition:
        raise AssertionError(code)


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def inventory(root):
    result = {}
    for path in sorted(root.rglob('*')):
        require(not path.is_symlink(), 'synthetic-tree-has-symlink')
        if path.is_file():
            result[str(path.relative_to(root))] = sha(path)
        else:
            require(path.is_dir(), 'synthetic-tree-has-nonregular-entry')
    return result


class Listener(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        self.connections = 0
        self.counter_lock = threading.Lock()
        super().__init__(('127.0.0.1', 0), Handler)

    def get_request(self):
        connection, address = super().get_request()
        with self.counter_lock:
            self.connections += 1
        return connection, address


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'Synthetic forbidden fetch witness')

    do_POST = do_GET
    do_HEAD = do_GET

    def log_message(self, *_):
        pass


class OwnedApp:
    def __init__(self, binary, root):
        self.binary, self.root = binary, root
        self.process = None
        self.session = None
        self.owner = None
        self.ready = False
        self.drains = []
        # Retained until the exact owned child has exited. No released-port race.
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.listener.listen(16)
        self.port = self.listener.getsockname()[1]
        self.origin = f'http://127.0.0.1:{self.port}'

    def headers(self, session=UNSET):
        require(self.ready and self.process is not None and self.process.poll() is None,
                'owned-backend-not-ready-or-exited')
        headers = {'Host': f'127.0.0.1:{self.port}', 'Origin': self.origin,
                   'Content-Type': 'application/json', 'X-Smith-Desktop-Owner': self.owner}
        token = self.session if session is UNSET else session
        if token is not None:
            headers['X-Smith-Profile-Session'] = token
        return headers

    def request(self, path, body=None, method=None, want=200, session=UNSET, raw=False,
                cap=2 * 1024 * 1024):
        headers = self.headers(session)
        payload = None if body is None else json.dumps(body).encode()
        client = http.client.HTTPConnection('127.0.0.1', self.port, timeout=25)
        try:
            client.request(method or ('POST' if body is not None else 'GET'), '/api' + path,
                           body=payload, headers=headers)
            response = client.getresponse()
            data = response.read(cap + 1)
            require(len(data) <= cap, 'http-response-exceeds-bound')
            require(response.status == want, f'unexpected-http-status-{response.status}-wanted-{want}')
            if raw:
                return data, {key.lower(): value for key, value in response.getheaders()}
            return json.loads(data)
        finally:
            client.close()

    def status(self):
        return self.request('/profile/status')

    def start(self):
        require(self.process is None or self.process.poll() == 0,
                'previous-owned-backend-not-cleanly-reaped')
        self.ready = False
        self.session = None
        self.owner = secrets.token_hex(32)
        environment = dict(os.environ)
        for key in ['SMITH_NEXT_SYNTHETIC_PROFILE', 'SMITH_NEXT_MANUAL_PROFILE']:
            environment.pop(key, None)
        scratch = self.root.parent / 'app-temporary'
        scratch.mkdir(mode=0o700, exist_ok=True)
        environment.update(SMITH_NEXT_DATA_DIR=str(self.root), SMITH_NEXT_PORT='0',
                           SMITH_NEXT_DESKTOP_OWNER=self.owner,
                           SMITH_NEXT_DESKTOP_LISTENER_FD='0', TMPDIR=str(scratch))
        # Store the exact direct-child handle before reading anything. Failed
        # handshakes still have an owned cleanup path; never signal a received PID.
        self.process = subprocess.Popen([str(self.binary)], env=environment, stdin=self.listener,
                                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 20
        pending = bytearray()
        with selectors.DefaultSelector() as readable:
            readable.register(self.process.stdout, selectors.EVENT_READ)
            while time.monotonic() < deadline:
                require(self.process.poll() is None, 'owned-backend-exited-before-ready')
                if not readable.select(.05):
                    continue
                chunk = os.read(self.process.stdout.fileno(), 4096)
                require(bool(chunk), 'owned-backend-readiness-eof')
                pending.extend(chunk)
                require(len(pending) <= 65536, 'owned-backend-readiness-exceeds-bound')
                if b'\n' not in pending:
                    continue
                line, _, _ = pending.partition(b'\n')
                try:
                    handshake = json.loads(line)
                except (UnicodeDecodeError, ValueError):
                    raise AssertionError('owned-backend-readiness-malformed') from None
                require(handshake == {'smith_desktop': 1, 'pid': self.process.pid,
                                      'owner': self.owner, 'url': self.origin},
                        'owned-backend-readiness-identity-mismatch')
                self.ready = True
                break
        require(self.ready, 'owned-backend-start-deadline')
        # Discard, never retain or print, any later child output.
        stream = self.process.stdout
        def drain():
            try:
                while stream.read(4096):
                    pass
            except (OSError, ValueError):
                pass
        thread = threading.Thread(target=drain, daemon=True)
        thread.start()
        self.drains.append((thread, stream))
        state = self.status()
        require(state['automatic'] and state['state'] == 'unlocked', 'not-automatic-ready')
        self.session = state['session']

    def stop(self):
        self.ready = False
        self.session = None
        if self.process is not None:
            if self.process.poll() is None:
                # Only the retained Popen child is signalled. Never use a PID
                # from the handshake, HTTP data, a file or a process search.
                self.process.send_signal(signal.SIGINT)
                try:
                    self.process.wait(timeout=25)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)
                    raise AssertionError('owned-backend-shutdown-deadline') from None
            # Includes children that exited before stop() was called.
            require(self.process.returncode == 0, 'owned-backend-shutdown-failed')
        for thread, stream in self.drains:
            thread.join(timeout=3)
            require(not thread.is_alive(), 'owned-backend-output-drain-unconfirmed')
            stream.close()
        self.drains.clear()
        if self.process and self.process.stdout:
            self.process.stdout.close()
        self.owner = None

    def close(self):
        self.stop()
        # Keep the parent listener on any uncertain shutdown path.
        self.listener.close()

    def cleanup_resolved(self):
        try:
            reaped = self.process is None or self.process.poll() is not None
            return reaped and all(not thread.is_alive() for thread, _ in self.drains)
        except (OSError, subprocess.SubprocessError):
            return False

    def retain_cleanup_until_resolved(self):
        # Failure-only custody owner. Do not exit Python and release the retained
        # socket while an owned child or output reader remains unresolved. Each
        # wait is bounded; uncertainty keeps this parent alive and the data intact.
        # Ctrl-C is not evidence that cleanup happened, so it cannot bypass hold.
        while not self.cleanup_resolved():
            try:
                if self.process is not None:
                    if self.process.poll() is None:
                        self.process.kill()
                    self.process.wait(timeout=5)
                for thread, _ in self.drains:
                    thread.join(timeout=1)
                if not self.cleanup_resolved():
                    time.sleep(1)
            except (OSError, subprocess.SubprocessError, KeyboardInterrupt):
                try:
                    time.sleep(1)
                except KeyboardInterrupt:
                    pass
        # Nonzero exit still fails qualification, but the direct child is now
        # reaped and its reader has finished. Never retry any profile operation.
        for _, stream in self.drains:
            stream.close()
        self.drains.clear()
        if self.process and self.process.stdout:
            self.process.stdout.close()
        self.listener.close()
        self.owner = self.session = None

    def operation(self, kind, fields, expected_status='succeeded', expected_error=None):
        accepted = self.request('/profile/' + kind,
                                {'revision': self.status()['revision'], **fields}, want=202)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            state = self.status()
            operation = state.get('operation')
            require(operation and operation['id'] == accepted['operation_id'],
                    'profile-operation-correlation-failed')
            if operation['status'] != 'running':
                require(operation['status'] == expected_status, 'profile-operation-unexpected-terminal-status')
                if expected_error is not None:
                    require(operation['error'] == expected_error, 'profile-operation-unexpected-refusal')
                require(state['state'] == 'unlocked', 'profile-did-not-reopen')
                self.session = state['session']
                return state
            time.sleep(.02)
        raise AssertionError('profile-operation-deadline')

    def disconnected_busy_reader(self, heavy_row, probe_row):
        client = socket.create_connection(('127.0.0.1', self.port), timeout=5)
        try:
            headers = {**self.headers(), 'Connection': 'close'}
            request = (f'GET /api/legacy-import/message-cards/{heavy_row} HTTP/1.1\r\n' +
                       ''.join(f'{name}: {value}\r\n' for name, value in headers.items()) + '\r\n')
            client.sendall(request.encode('ascii'))
            # An actual 503 witnesses the single parser slot being held, without
            # needing a test-only runtime hook or guessing from a sleep.
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                probe = http.client.HTTPConnection('127.0.0.1', self.port, timeout=20)
                try:
                    probe.request('GET', f'/api/legacy-import/message-cards/{probe_row}',
                                  headers=self.headers())
                    response = probe.getresponse()
                    status = response.status
                    response.read(512 * 1024 + 1)
                finally:
                    probe.close()
                if status == 503:
                    client.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
                    return
                require(status == 200, 'reader-probe-unexpected-status')
            raise AssertionError('no-active-reader-admission-witness')
        finally:
            client.close()


def bytes_path(card, index):
    source = card['source']
    return (f"/legacy-import/message-cards/{source['row_number']}/images/{index}/bytes?" +
            urlencode({key: source[key] for key in ('import_id', 'source_schema', 'payload_sha256')}))


def dormant(app):
    for path, field in [('/agent/tasks', 'tasks'), ('/agent/workspaces', 'workspaces'),
                        ('/agent/definitions', 'definitions'), ('/memory', 'memories')]:
        require(app.request(path)[field] == [], 'historical-content-became-active')
    require(app.request('/mlx/status')['runtime'] is None, 'historical-content-launched-mlx')
    tasks = app.request('/legacy-import/tasks')['tasks']
    require(tasks and all(not task['resumable'] for task in tasks), 'legacy-task-gained-resume-authority')


def encrypted(directory, master):
    for name in ('profile.sqlite3', 'import.db'):
        data = (directory / name).read_bytes()
        require(not data.startswith(b'SQLite format 3\0'), 'plaintext-database-header')
        for marker in (master.encode(), b'Synthetic historical card fixture complete',
                       b'Separate original reasoning', b'synthetic-password'):
            require(marker not in data, 'plaintext-synthetic-sentinel-in-encrypted-file')


def safe_diagnostic(error):
    # Only our static hyphenated assertion codes may be disclosed.
    if isinstance(error, AssertionError) and str(error).replace('-', '').isalnum():
        return str(error)
    return type(error).__name__


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--binary-sha256', required=True)
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--fixture-sha256', required=True)
    parser.add_argument('--target-schema', type=int, required=True)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    binary, fixture, report = args.binary.resolve(), args.fixture.resolve(), args.report.resolve()
    require(not report.exists(), 'report-already-exists-preserve-previous-evidence')
    require(sha(binary) == args.binary_sha256, 'wrong-frozen-backend-identity')
    require(sha(fixture) == args.fixture_sha256, 'wrong-fixture-identity')
    checks, phase, app = [], 'setup', None
    listener = Listener()
    listener_thread = threading.Thread(target=listener.serve_forever, kwargs={'poll_interval': .05}, daemon=True)
    listener_thread.start()
    started = time.monotonic()
    master = secrets.token_hex(32)
    root, result, failure = None, None, None
    try:
        # Explicit ownership instead of TemporaryDirectory's unconditional
        # finalizer: any failure retains this private synthetic evidence tree.
        root = Path(tempfile.mkdtemp(prefix='smith-historical-card-owned-')).resolve()
        os.chmod(root, 0o700)
        source, host = root / 'original-copy', root / 'host'
        phase = 'synthetic-source-generation'
        created = subprocess.run([str(fixture), str(source), str(listener.server_port)],
                                 input=(master + '\n').encode(), stdout=subprocess.DEVNULL,
                                 stderr=subprocess.DEVNULL, timeout=25)
        require(created.returncode == 0, 'synthetic-fixture-generation-failed')
        before = inventory(source)
        app = OwnedApp(binary, host)
        app.start()
        old_session = app.session
        fields = {'source_copy': str(source), 'managed_copy': str(source / 'artifacts'),
                  'offline_copy_attested': True, 'confirm': True}
        key = {'format': 'master', 'master_hex': master}
        phase = 'verified-import'
        preflight = app.operation('legacy-preflight', {**fields, 'original_key': key})
        require(preflight['operation']['result']['source_schema'] == 25, 'wrong-original-schema')
        require(preflight['operation']['result']['target_schema'] == args.target_schema, 'wrong-preflight-target-schema')
        require(inventory(source) == before, 'preflight-mutated-source')
        imported = app.operation('legacy-import', {**fields, 'original_key': key})
        require(imported['operation']['result']['target_schema'] == args.target_schema, 'wrong-import-target-schema')
        require(imported['selection'] == 'imported', 'import-not-selected')
        require(inventory(source) == before, 'import-mutated-source')
        destination = host / 'local-workspace/imported'
        archive_before = sha(destination / 'import.db')
        encrypted(destination, master)
        checks.append('verified-new-schema25-copy-import-retains-source-and-encrypted-archive')

        phase = 'read-only-card-and-byte-reconciliation'
        originals = app.request('/legacy-import/records/messages?limit=100')['rows']
        rows = {row['id']: row['row_number'] for row in originals}
        require(len(rows) == 17, 'fixture-message-count-mismatch')
        cards = {}
        for identifier in rows:
            if identifier != 'slow-images':
                cards[identifier] = app.request(f'/legacy-import/message-cards/{rows[identifier]}', cap=512 * 1024)
        for original in originals:
            if original['id'] == 'slow-images':
                continue
            record = original['record']
            # Match the archive's documented ExactRow/ExactValue encoding,
            # without depending on object-key order in the HTTP response.
            exact_values = [dict(type=value['type'], **({'value': value['value']} if 'value' in value else {}))
                            for value in record['values']]
            raw = json.dumps({'columns': record['columns'], 'values': exact_values},
                             ensure_ascii=False, separators=(',', ':')).encode()
            require(cards[original['id']]['source']['payload_sha256'] == hashlib.sha256(raw).hexdigest(),
                    'full-exact-source-payload-digest-mismatch')
        root_card = cards['root']
        require(root_card['source']['table'] == 'messages' and root_card['source']['source_schema'] == 25,
                'source-receipt-mismatch')
        require(root_card['image_count'] == 9, 'image-order-count-mismatch')
        fixtures = json.loads((ROOT / 'crates/smith-profile-storage/examples/support/historical-message-images-v1.json').read_text())
        for index in range(3):
            original_bytes = base64.b64decode(fixtures[index]['data_uri'].split(',', 1)[1], validate=True)
            image, headers = app.request(bytes_path(root_card, index), raw=True, cap=8 * 1024 * 1024)
            require(image == original_bytes, 'decoded-image-bytes-changed')
            require(headers['content-type'] == fixtures[index]['mime'], 'image-mime-mismatch')
            require(headers['x-smith-content-sha256'] == hashlib.sha256(original_bytes).hexdigest(), 'image-hash-header-mismatch')
            require(headers['cache-control'] == 'no-store' and headers['x-content-type-options'] == 'nosniff', 'image-response-privacy-headers')
            require(root_card['images'][index]['state']['decoded_sha256'] == hashlib.sha256(image).hexdigest(), 'image-card-content-digest-mismatch')
        require(root_card['images'][3]['state'] == {'status': 'remote_unresolved', 'scheme': 'http'}, 'remote-reference-was-not-unresolved')
        for index, reason in [(4, 'unsupported_scheme'), (5, 'invalid_base64'), (6, 'unsupported_image_format'),
                              (7, 'unsupported_image_format'), (8, 'mime_mismatch')]:
            require(root_card['images'][index]['state'] == {'status': 'retained', 'reason': reason}, 'invalid-reference-classification')
            app.request(bytes_path(root_card, index), want=409)
        app.request(bytes_path(root_card, 3), want=409)
        require(all(card['runtime_activation'] == 'dormant' for card in cards.values()), 'card-activation-not-dormant')
        require(all(not item['fetched'] for card in cards.values() for item in card['images']), 'card-claimed-fetch')
        card_wire = json.dumps(cards)
        require(all(value not in card_wire for value in ('synthetic-password', 'synthetic-token', '127.0.0.1', 'data:image/')),
                'sensitive-reference-leaked-into-card-label')
        require(cards['invalid-images']['image_count'] is None, 'invalid-image-json-silently-empty')
        require(any(issue['reason'] == 'invalid_json' for issue in cards['invalid-images']['issues']), 'invalid-json-not-explicit')
        checks.append('exact-static-image-bytes-mime-hashes-with-animated-invalid-remote-and-file-refusals')

        phase = 'dormant-tool-and-branch-links'
        call_cards = cards['branch1']['calls']
        require([call['id'] for call in call_cards] == ['call', 'clarify'], 'call-order-changed')
        require(call_cards[0]['arguments']['text'] == '{literal invalid' and not call_cards[0]['arguments_valid_json'], 'literal-invalid-arguments-changed')
        require(call_cards[0]['unknown_fields'] == ['future'], 'unknown-call-field-not-retained')
        require(cards['tool']['result_link']['status'] == 'unique', 'unique-link-not-resolved')
        require(cards['tool']['result_link']['call']['original_message_id'] == 'branch1', 'wrong-ancestor-linked')
        require(cards['clarify-result']['result_link']['call']['call_index'] == 1, 'wrong-call-index')
        require(cards['reuse-result']['result_link'] == {'status': 'ambiguous', 'call_id': 'reused', 'candidates': 2}, 'reused-call-id-not-ambiguous')
        require(cards['orphan-result']['result_link'] == {'status': 'unmatched', 'call_id': 'orphan-call'}, 'sibling-call-used-as-ancestor')
        require(cards['malformed-result']['result_link'] == {'status': 'unresolved', 'reason': 'incomplete_calls'}, 'malformed-call-became-authority')
        require(cards['reuse-result']['original_status'] == {'type': 'text', 'value': 'failed'}, 'original-status-changed')
        chats = app.request('/chats/session')['messages']
        historical_ids = {'root', 'branch1', 'tool', 'reuse-result', 'malformed-call', 'invalid-images'}
        require(historical_ids <= {message['id'] for message in chats}, 'historical-chat-rows-missing')
        require(all(message['status'] == 'imported_reference' for message in chats if message['id'] in historical_ids),
                'historical-context-became-provider-history')
        dormant(app)
        checks.append('ordered-literal-tool-cards-unique-earlier-ancestor-ambiguity-and-dormant-domains')

        phase = 'stale-source-and-session-refusal'
        wrong = json.loads(json.dumps(root_card))
        wrong['source']['payload_sha256'] = '0' * 64
        app.request(bytes_path(wrong, 0), want=409)
        wrong['source']['payload_sha256'] = root_card['source']['payload_sha256']
        wrong['source']['import_id'] = 'different-import'
        app.request(bytes_path(wrong, 0), want=409)
        app.request(bytes_path(root_card, 0), want=409, session=old_session)
        app.request(bytes_path(root_card, 0), want=409, session=None)
        app.request(bytes_path(root_card, 0) + '&profile_session=' + app.session, want=409, session=None)
        checks.append('full-source-digest-import-and-profile-header-receipts-required')

        phase = 'disconnected-reader-profile-rotation-drain'
        old_session = app.session
        app.disconnected_busy_reader(rows['slow-images'], rows['cards-final'])
        # The selected imported slot already exists. files::destination must
        # refuse replacement; profile_api still drains and reopens this same
        # selection with a fresh session after the expected operation failure.
        reopened = app.operation('legacy-preflight', {**fields, 'original_key': key},
                                 expected_status='failed',
                                 expected_error='Destination already exists; it will never be overwritten.')
        require(reopened['selection'] == 'imported' and app.session != old_session, 'profile-rotation-did-not-revoke')
        app.request(bytes_path(root_card, 0), want=409, session=old_session)
        require(app.request(f"/legacy-import/message-cards/{rows['root']}") == root_card, 'cards-changed-after-reader-drain')
        checks.append('observed-active-reader-disconnect-drains-before-existing-destination-refusal-reopens-and-rejects-old-session')

        phase = 'disconnected-reader-process-drain-restart'
        old_session = app.session
        app.disconnected_busy_reader(rows['slow-images'], rows['cards-final'])
        app.stop()
        encrypted(destination, master)
        require(sha(destination / 'import.db') == archive_before, 'archive-changed-on-close')
        app.start()
        require(app.session != old_session and app.status()['selection'] == 'imported', 'restart-profile-identity')
        app.request(bytes_path(root_card, 0), want=409, session=old_session)
        require(app.request(f"/legacy-import/message-cards/{rows['root']}") == root_card, 'cards-changed-after-restart')
        current_originals = app.request('/legacy-import/records/messages?limit=100')['rows']
        require(current_originals == originals, 'exact-typed-originals-changed')
        require(inventory(source) == before and sha(destination / 'import.db') == archive_before,
                'read-only-journey-changed-source-or-archive')
        dormant(app)
        checks.append('observed-active-reader-disconnect-drains-on-exit-exact-cards-and-originals-survive-restart')

        phase = 'read-only-deactivation-eligibility'
        rolled = app.operation('legacy-rollback', fields)
        rollback = rolled['operation']['result']
        require(rolled['selection'] == 'default', 'deactivation-did-not-return-prior-workspace')
        require(all(rollback[key] for key in ('eligible', 'deactivated', 'source_unchanged', 'destination_unchanged')),
                'read-only-cards-changed-rollback-eligibility')
        require(destination.is_dir() and inventory(source) == before and sha(destination / 'import.db') == archive_before,
                'deactivation-did-not-retain-copies')
        app.stop()
        require(listener.connections == 0, 'historical-reference-was-fetched')
        require(sha(binary) == args.binary_sha256 and sha(fixture) == args.fixture_sha256,
                'qualification-executable-changed')
        checks.append('read-only-deactivation-remains-eligible-retains-all-copies-and-zero-listener-connections')
        result = {'passed': True, 'synthetic_only': True, 'target_schema': args.target_schema,
                  'app_sha256': args.binary_sha256, 'fixture_sha256': args.fixture_sha256,
                  'runner_sha256': sha(Path(__file__)), 'groups': len(checks), 'checks': checks,
                  'listener_connections': listener.connections, 'source_inventory_unchanged': True,
                  'archive_bytes_unchanged': True, 'controlled_disconnect_busy_witnesses': 2,
                  'seconds': round(time.monotonic() - started, 2),
                  'remaining_scope': ['native image/card adoption', 'generated managed-artifact collection'],
                  'real_profiles_accessed': False, 'remote_requests': False, 'tool_or_model_execution': False}
        result['harness_expectation_correction'] = {
            'previous_report': 'native-historical-messages-schema11.json',
            'previous_completed_groups': 4,
            'reason': 'A second preflight after import must refuse the existing fixed destination; success was the wrong expectation.',
            'source_contract': ['crates/smith-profile-storage/src/files.rs:destination',
                                'src/profile_api.rs:legacy_operation'],
            'corrected_assertion': 'Exact existing-destination refusal, followed by reopened imported selection, new session, unchanged cards and stale-header rejection.',
            'production_changed': False,
            'failed_evidence_retained': True,
        }
    except BaseException as error:
        failure = {'phase': phase, 'diagnostic': safe_diagnostic(error)}
    finally:
        try:
            if app:
                app.close()
        except BaseException as error:
            failure = {'phase': 'owned-backend-cleanup', 'diagnostic': safe_diagnostic(error)}
        try:
            listener.shutdown()
            listener.server_close()
            listener_thread.join(timeout=3)
            require(not listener_thread.is_alive(), 'owned-witness-listener-drain-unconfirmed')
            require(listener.connections == 0, 'historical-reference-was-fetched')
        except BaseException as error:
            failure = {'phase': 'owned-witness-cleanup', 'diagnostic': safe_diagnostic(error)}
        master = key = None
    if app and not app.cleanup_resolved():
        failure = {'phase': 'owned-backend-cleanup', 'diagnostic': 'retained-cleanup-hold'}
        try:
            print(json.dumps({'passed': False, **failure, 'synthetic_only': True,
                              'diagnostics_retained': True}), flush=True)
        except (OSError, KeyboardInterrupt):
            pass
        finally:
            # Broken stdout is not permission to abandon process custody.
            app.retain_cleanup_until_resolved()
    if failure is None:
        try:
            require(result is not None and root is not None, 'missing-completed-journey')
            # Only reached after exact child exit0, joined pipe drain, closed
            # inherited listener and joined no-fetch witness listener.
            shutil.rmtree(root)
        except BaseException as error:
            failure = {'phase': 'owned-directory-cleanup', 'diagnostic': safe_diagnostic(error)}
    if failure is not None:
        result = {'passed': False, **failure, 'completed_groups': len(checks),
                  'synthetic_only': True, 'diagnostics_retained': bool(root and root.exists()),
                  'retained_owned_directory': root.name if root and root.exists() else None,
                  'app_sha256': args.binary_sha256, 'fixture_sha256': args.fixture_sha256}
    else:
        result['cleanup_confirmed'] = True
        result['retained_listener_and_exact_child_handshake'] = True
        result['seconds'] = round(time.monotonic() - started, 2)
    # No request bodies, keys, raw URLs, personal paths or backend logs appear.
    # Success is published only after confirmed cleanup; failures retain data.
    report.parent.mkdir(parents=True, exist_ok=True)
    with report.open('x') as output:
        json.dump(result, output, indent=2)
        output.write('\n')
    print(json.dumps(result, indent=2))
    if failure is not None:
        raise SystemExit(1) from None


if __name__ == '__main__':
    main()
