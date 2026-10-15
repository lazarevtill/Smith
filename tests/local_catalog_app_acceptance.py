#!/usr/bin/env python3
"""Owned normal-app catalog acceptance. No model, selected executable or probe runs.

Requires an explicit frozen backend and its exact SHA256. All source files and
profile data are disposable synthetic fixtures; no installed asset is read.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import selectors
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import uuid

from desktop_boundary_review import clean_env


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        while data := stream.read(1024 * 1024):
            h.update(data)
    return h.hexdigest()


def new_id():
    return str(uuid.uuid4())


def gone(pid):
    try:
        os.kill(pid, 0)
        return False
    except ProcessLookupError:
        return True


def reserve_report(path):
    """Refuse an existing report before launching any backend; never truncate."""
    path.parent.mkdir(parents=True, exist_ok=True)
    return os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | os.O_NOFOLLOW, 0o600)


def write_reserved_report(fd, report, root):
    encoded = json.dumps(report, indent=2, ensure_ascii=False).replace(str(root), '[owned synthetic fixture]')
    with os.fdopen(fd, 'w', encoding='utf-8') as stream:
        stream.write(encoded + '\n')
        stream.flush()
        os.fsync(stream.fileno())


def publish_with_custody(report_fd, report_path, report, root, host, confirmed):
    """Reporting failures cannot abandon an unconfirmed owned child/listener."""
    try:
        write_reserved_report(report_fd, report, root)
        summary = {'passed': report['passed'], 'groups': len(report['checks']), 'report': str(report_path)}
        if report['diagnostic_tree_retained']:
            summary['retained_owned_diagnostics'] = str(root)
        print(json.dumps(summary), flush=True)
    finally:
        if not confirmed:
            try:
                print(json.dumps({'custody': 'retained until owned backend and readers exit',
                                  'pid': host.child.pid if host.child else None}), flush=True)
            except (OSError, ValueError, KeyboardInterrupt):
                pass
            finally:
                host.retain_until_confirmed()
            # Never rewrite or promote the initial failure. This best-effort
            # exclusive companion only records eventual ownership closure.
            try:
                companion = report_path.with_name(report_path.stem + '-cleanup-' + new_id() + '.json')
                write_reserved_report(reserve_report(companion), {
                    'original_report': report_path.name, 'qualification_passed': False,
                    'cleanup_eventually_confirmed': True, 'diagnostic_tree_retained': True,
                    'returncode': host.child.returncode if host.child else None}, root)
                print(json.dumps({'eventual_cleanup_report': str(companion)}), flush=True)
            except (OSError, ValueError, KeyboardInterrupt):
                pass


class Host:
    def __init__(self, binary, root):
        self.binary, self.root = binary, root
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.listener.listen(16)
        self.child, self.session, self.owner = None, None, None
        self.pids, self.drains, self.output_counts = [], [], []
        self.exits = []

    def start(self):
        assert self.cleanup_confirmed(), 'Previous backend/readers still owned'
        self.owner = secrets.token_hex(32)
        scratch = self.root.parent / 'scratch'
        scratch.mkdir(exist_ok=True, mode=0o700)
        prior = os.environ.get('TMPDIR')
        os.environ['TMPDIR'] = str(scratch)
        try:
            # Retain Popen immediately. A handshake error must not hide a still
            # owned child inside a launch helper that never returned its handle.
            self.child = subprocess.Popen([str(self.binary)], env=clean_env(self.root, self.owner),
                                          stdin=self.listener, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            self.pids.append(self.child.pid)
        finally:
            if prior is None:
                os.environ.pop('TMPDIR', None)
            else:
                os.environ['TMPDIR'] = prior
        deadline = time.monotonic() + 20
        pending = bytearray()
        observed = False
        with selectors.DefaultSelector() as ready:
            ready.register(self.child.stdout, selectors.EVENT_READ)
            while time.monotonic() < deadline:
                if self.child.poll() is not None:
                    raise AssertionError(f'Owned backend exited before readiness: {self.child.returncode}')
                if not ready.select(.05):
                    continue
                block = os.read(self.child.stdout.fileno(), 4096)
                assert block, 'Owned readiness pipe closed'
                pending.extend(block)
                assert len(pending) <= 65536, 'Owned readiness exceeded bound'
                if b'\n' in pending:
                    line = pending.partition(b'\n')[0]
                    value = json.loads(line)
                    assert value['smith_desktop'] == 1 and value['pid'] == self.child.pid
                    assert value['owner'] == self.owner
                    assert value['url'] == 'http://127.0.0.1:' + str(self.listener.getsockname()[1])
                    observed = True
                    break
        assert observed, 'Owned backend readiness deadline'
        self.session = None
        # Continuously discard child logs. Never include credentials or private
        # raw output in the qualification report, even on a failing assertion.
        for pipe in (self.child.stdout, self.child.stderr):
            count = [0]
            self.output_counts.append(count)
            def drain(stream=pipe, total=count):
                while block := stream.read(4096):
                    total[0] += len(block)
            thread = threading.Thread(target=drain, daemon=True)
            thread.start()
            self.drains.append((thread, pipe))
        state = self.api('/profile/status')
        assert state['automatic'] and state['state'] == 'unlocked'
        self.session = state['session']
        self.no_runtime()

    def headers(self, body=None, session=None):
        port = self.listener.getsockname()[1]
        value = {'Host': f'127.0.0.1:{port}', 'X-Smith-Desktop-Owner': self.owner}
        if session is not None:
            value['X-Smith-Profile-Session'] = session
        elif self.session:
            value['X-Smith-Profile-Session'] = self.session
        if body is not None:
            value.update({'Origin': f'http://127.0.0.1:{port}', 'Content-Type': 'application/json'})
        return value

    def api(self, path, body=None, method=None, want=200, session=None):
        assert self.child and self.child.poll() is None, 'Owned backend stopped unexpectedly'
        data = json.dumps(body).encode() if body is not None else None
        connection = http.client.HTTPConnection('127.0.0.1', self.listener.getsockname()[1], timeout=8)
        try:
            connection.request(method or ('POST' if body is not None else 'GET'), '/api' + path,
                               data, self.headers(data, session))
            response = connection.getresponse()
            payload = response.read(3 * 1024 * 1024 + 1)
            assert len(payload) <= 3 * 1024 * 1024
            value = json.loads(payload)
            expected = (want,) if isinstance(want, int) else want
            assert response.status in expected, f'{path}: HTTP {response.status}: {value}'
            return value
        finally:
            connection.close()

    def discard_accepted_body(self, path, body):
        payload = json.dumps(body).encode()
        headers = self.headers(payload)
        headers.update({'Content-Length': str(len(payload)), 'Connection': 'close'})
        request = ('POST /api' + path + ' HTTP/1.1\r\n' +
                   ''.join(key + ': ' + value + '\r\n' for key, value in headers.items()) + '\r\n').encode() + payload
        with socket.create_connection(self.listener.getsockname(), timeout=8) as client:
            client.sendall(request)
            first_line = bytearray()
            while not first_line.endswith(b'\r\n'):
                block = client.recv(1)
                assert block and len(first_line) < 1024
                first_line.extend(block)
            assert bytes(first_line).split()[1] == b'202', first_line
            # Deliberately lose the operation/result body; retain caller UUID.

    def poll(self, read, predicate, seconds=20):
        deadline = time.monotonic() + seconds
        while True:
            value = read()
            if predicate(value):
                return value
            assert time.monotonic() < deadline, 'Owned catalog operation deadline'
            time.sleep(.02)

    def operation(self, accepted):
        return self.poll(lambda: self.api('/local-catalog/operations/' + accepted['operation_id'])['operation'],
                         lambda op: op['status'] != 'running')

    def listing(self):
        result = self.api('/local-catalog')
        assert result['inspection'] == 'stored_metadata_only'
        assert not result['automatic_load'] and not result['hints_applied']
        return result

    def expected(self):
        config = self.api('/config')
        return {key: config[key] for key in ('revision', 'provider', 'endpoint')}

    def no_runtime(self):
        assert self.api('/native/status')['runtime'] is None
        assert self.api('/mlx/status')['runtime'] is None

    def observe(self):
        revision = self.api('/acceleration')['preferences']['revision']
        accepted = self.api('/acceleration/inventory', {'expected_revision': revision})
        state = self.poll(lambda: self.api('/acceleration'), lambda s:
                          s['operation'] and s['operation']['id'] == accepted['operation_id']
                          and s['operation']['status'] != 'running')
        assert state['operation']['status'] == 'succeeded', state['operation']
        return state['inventory']['inventory']['generation']

    def prepare(self, entry):
        accepted = self.api('/local-catalog/prepare', {
            'catalog': self.listing()['catalog'], 'entry_id': entry['id'],
            'entry_revision': entry['revision'], 'expected': self.expected()}, want=202)
        op = self.operation(accepted)
        assert op['status'] == 'succeeded' and op['worker_drained'], op
        assert op['result']['qualification'] == 'not_established'
        assert op['result']['execution'] == 'none' and not op['result']['hints_applied']
        self.no_runtime()
        return op['result']['prepared']

    def refuse_handoff(self, review):
        self.api('/local-catalog/setup', {'adapter': 'gguf', 'review_id': review['id'],
                 'model_id': 'synthetic-no-execution', 'settings_revision': self.api('/acceleration')['preferences']['revision'],
                 'expected': self.expected()}, want=409)
        self.no_runtime()

    def stop(self):
        stop_error = None
        if self.child and self.child.poll() is None:
            self.child.send_signal(signal.SIGTERM)
            try:
                self.child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                stop_error = AssertionError('Owned backend exceeded graceful shutdown deadline')
                # Only the retained direct child is eligible for emergency kill.
                self.child.kill()
                try:
                    self.child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    raise AssertionError('Owned backend exit remains unconfirmed after emergency kill') from stop_error
        if self.child:
            code = self.child.poll()
            if code is not None and not any(e['pid'] == self.child.pid for e in self.exits):
                self.exits.append({'pid': self.child.pid, 'returncode': code})
            if code != 0 and stop_error is None:
                stop_error = AssertionError(f'Owned backend did not exit cleanly: {code}')
        for thread, pipe in self.drains:
            thread.join(timeout=2)
            if thread.is_alive() and stop_error is None:
                stop_error = AssertionError('Owned output reader remained after process exit')
        if self.cleanup_confirmed():
            self.close_finished_pipes()
        self.session = None
        if stop_error:
            raise stop_error

    def cleanup_confirmed(self):
        return ((self.child is None or self.child.poll() is not None)
                and not any(thread.is_alive() for thread, _ in self.drains))

    def close_finished_pipes(self):
        assert self.cleanup_confirmed()
        for _, pipe in self.drains:
            pipe.close()
        self.drains = []
        # A failed handshake has not started readers but still owns these pipes.
        if self.child:
            for pipe in (self.child.stdout, self.child.stderr):
                if pipe and not pipe.closed:
                    pipe.close()

    def release_listener(self):
        assert self.cleanup_confirmed(), 'Retain endpoint custody until child/readers exit'
        self.close_finished_pipes()
        self.listener.close()

    def close(self):
        try:
            self.stop()
        finally:
            if self.cleanup_confirmed():
                self.release_listener()

    def retain_until_confirmed(self):
        """Exceptional custody hold, not a clean-stop result or another launch.

        Leave the runner alive with its listener and diagnostic tree if the OS
        cannot confirm cleanup. Parent can inspect this owned execution session.
        A keyboard interrupt does not silently abandon a still-owned backend.
        """
        while not self.cleanup_confirmed():
            try:
                if self.child and self.child.poll() is None:
                    try:
                        self.child.wait(timeout=.5)
                    except subprocess.TimeoutExpired:
                        pass
                for thread, _ in self.drains:
                    thread.join(timeout=.1)
            except KeyboardInterrupt:
                continue
        self.release_listener()


def selection(root, model='model.gguf'):
    source = {'label': 'Owned synthetic explicit selection', 'revision': 'catalog-http-v1', 'origin': 'external_user_selected'}
    return {'description': {'format': 'gguf', 'architecture': 'synthetic',
             'declared_capabilities': ['text_chat'], 'declared_context_limit': 4096,
             'hints': {'adapter': 'llama_gguf', 'hints': {'threads': 7, 'mmap': True}}},
            'runtime': {'root': str(root), 'source': source, 'files': [{'name': 'runner', 'role': 'executable'}]},
            'model': {'root': str(root), 'source': source, 'files': [{'name': model, 'role': 'weights'}]}}


def mutate(host, path, entry, method='POST', extra=None):
    body = {'request_id': new_id(), 'catalog': host.listing()['catalog'], 'entry_revision': entry['revision']}
    body.update(extra or {})
    return host.api(path, body, method)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--binary-sha256', required=True)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    assert sha(binary) == args.binary_sha256, 'Supplied frozen backend hash changed'
    report_fd = reserve_report(args.report)
    report = {'passed': False, 'binary_sha256': args.binary_sha256,
              'harness_sha256': sha(Path(__file__)),
              'executed_at_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'checks': [], 'receipts': {}, 'scope': {'owned_synthetic_files_only': True,
              'model_execution': False, 'selected_executable_execution': False,
              'fixed_runtime_probes': False, 'native_window': False, 'placement_claim': False,
              'hardware_inventory': 'explicit read-only OS observation for review generation'}}
    host = None
    # No TemporaryDirectory finalizer: uncertain cleanup must never remove the
    # live child's profile tree during stack unwinding or interpreter shutdown.
    root = Path(tempfile.mkdtemp(prefix='smith-catalog-http-')).resolve()
    try:
        files = root / 'selected-files'
        files.mkdir(mode=0o700)
        runner_bytes = b'\xcf\xfa\xed\xfeSYNTHETIC NONRUNNABLE HEADER; NEVER EXECUTE'
        model_bytes = b'GGUF' + (3).to_bytes(4, 'little') + bytes(56)
        runner, model = files / 'runner', files / 'model.gguf'
        runner.write_bytes(runner_bytes)
        runner.chmod(0o700)  # Catalog requires an execute bit; bytes cannot run.
        model.write_bytes(model_bytes)
        external_initial = {p.name: sha(p) for p in (runner, model)}
        host = Host(binary, root / 'profile')
        host.start()
        assert host.listing()['catalog'] == {'revision': 0, 'sha256': None}
        assert host.listing()['entries'] == []
        preferences = host.api('/acceleration')['preferences']
        intent = preferences['intent']
        intent['overrides'].update(threads=3, context_tokens=1024, output_tokens=128, mmap=False)
        pins = host.api('/acceleration/preferences', {'expected_revision': preferences['revision'], 'intent': intent}, 'PUT')['preferences']
        active = host.api('/config')
        request = {'request_id': new_id(), 'catalog': host.listing()['catalog'],
                   'label': 'SYNTHETIC_PRIVATE_CATALOG_5829', 'selection': selection(files)}
        host.discard_accepted_body('/local-catalog/entries', request)
        recovered = host.poll(lambda: host.api('/local-catalog/requests/' + request['request_id']),
                              lambda r: r['receipt'] is not None)
        receipt = recovered['receipt']['result']
        report['receipts']['registration'] = receipt
        entry = receipt['entry']
        assert entry['origin'] == 'external_user_selected' and entry['completeness'] == 'selected_files_only'
        before_restart = host.listing()['catalog']
        runner.unlink()
        model.unlink()
        host.stop()
        host.start()
        assert host.listing()['catalog'] == before_restart
        assert host.listing()['entries'] == [entry]
        detail = host.api('/local-catalog/entries/' + entry['id'])
        assert detail['file_availability'] == 'not_checked'
        replay = host.api('/local-catalog/entries', request)
        assert replay['replayed'] and replay['receipt'] == receipt
        assert replay['catalog'] == before_restart and len(host.listing()['entries']) == 1
        host.api('/local-catalog/entries', {**request, 'label': 'wrong input, same UUID'}, want=409)
        renamed = mutate(host, '/local-catalog/entries/' + entry['id'] + '/rename', entry,
                         extra={'label': 'Renamed while source files are absent'})
        entry = renamed['receipt']['entry']
        assert host.api('/acceleration')['preferences'] == pins
        # Connection revision is deliberately renewed on startup; target fields persist.
        assert all(host.api('/config')[k] == active[k] for k in ('provider', 'endpoint', 'model'))
        report['checks'].append('lost accepted response recovered by exact UUID; restart and missing files preserve receipt/list/details/rename without duplicate or source read requirement')
        runner.write_bytes(runner_bytes)
        runner.chmod(0o700)
        model.write_bytes(model_bytes)

        selected = host.api('/local-catalog/select', {'request_id': new_id(), 'catalog': host.listing()['catalog'],
                            'entry_id': entry['id'], 'entry_revision': entry['revision']})
        before_trash = {p.name: sha(p) for p in (runner, model)}
        trash_input = {'request_id': new_id(), 'catalog': selected['catalog'], 'entry_revision': entry['revision']}
        path = '/local-catalog/entries/' + entry['id']
        trashed = host.api(path, trash_input, 'DELETE')
        assert host.listing()['entries'] == [] and host.listing()['selected_for_review'] is None
        assert len(host.api('/local-catalog?trashed=true')['entries']) == 1
        assert host.api(path, trash_input, 'DELETE')['receipt'] == trashed['receipt']
        trashed_entry = trashed['receipt']['entry']
        host.api('/local-catalog/select', {'request_id': new_id(), 'catalog': host.listing()['catalog'],
                 'entry_id': entry['id'], 'entry_revision': trashed_entry['revision']}, want=(400, 409))
        restored = mutate(host, path + '/restore', trashed_entry)
        entry = restored['receipt']['entry']
        assert not entry['selected_for_review'] and host.listing()['selected_for_review'] is None
        assert {p.name: sha(p) for p in (runner, model)} == before_trash == external_initial
        assert host.api('/acceleration')['preferences'] == pins
        host.no_runtime()
        report['receipts']['trash'] = trashed['receipt']
        report['receipts']['restore'] = restored['receipt']
        report['checks'].append('selection and recoverable Trash/restore preserve every external byte and manual pin, never switch active model, restore never selects')

        stale_catalog = host.listing()['catalog']
        old_entry = dict(entry)
        model.write_bytes(model_bytes + b'EXPLICIT SYNTHETIC REPLACEMENT')
        replace_input = {'request_id': new_id(), 'catalog': stale_catalog, 'entry_revision': entry['revision'],
                         'label': 'Explicit replacement', 'selection': selection(files)}
        accepted = host.api(path, replace_input, 'PUT', want=202)
        replacement = host.operation(accepted)
        assert replacement['status'] == 'succeeded' and replacement['worker_drained']
        entry = replacement['result']['receipt']['entry']
        assert entry['id'] == old_entry['id'] and entry['revision'] > old_entry['revision']
        host.api(path + '/rename', {'request_id': new_id(), 'catalog': stale_catalog,
                 'entry_revision': old_entry['revision'], 'label': 'stale'}, want=409)
        assert host.api(path, replace_input, 'PUT')['receipt'] == replacement['result']['receipt']
        assert host.api('/local-catalog/entries', request)['receipt'] == receipt
        assert host.api(path)['entry']['manifest']['model']['files'][0]['sha256'] == sha(model)
        report['receipts']['replacement'] = replacement['result']['receipt']
        report['checks'].append('explicit replacement retains entry ID, changes exact digest/revision, refuses stale edits, and old receipt replay never restores old bytes')

        host.api('/local-catalog/select', {'request_id': new_id(), 'catalog': host.listing()['catalog'],
                 'entry_id': entry['id'], 'entry_revision': entry['revision']})
        host.observe()
        review = host.prepare(entry)
        renamed = mutate(host, path + '/rename', entry, extra={'label': 'Changes invalidate review'})
        entry = renamed['receipt']['entry']
        host.refuse_handoff(review)
        review = host.prepare(entry)
        host.observe()
        host.refuse_handoff(review)
        review = host.prepare(entry)
        old_expected = host.expected()
        host.api('/config/reviewed', {'expected': old_expected, 'config': {'provider': 'managed', 'endpoint': '', 'model': ''}}, 'PUT')
        host.refuse_handoff(review)
        host.api('/local-catalog/prepare', {'catalog': host.listing()['catalog'], 'entry_id': entry['id'],
                 'entry_revision': entry['revision'], 'expected': old_expected}, want=409)
        review = host.prepare(entry)
        old_session = host.session
        host.stop()
        host.start()
        host.api('/local-catalog', session=old_session, want=409)
        host.refuse_handoff(review)
        assert host.listing()['prepared'] is None and host.listing()['operations'] == []
        host.observe()
        prepared = host.prepare(entry)
        report['prepared'] = prepared
        saved_hash = host.api(path)['entry']['manifest']['model']['files'][0]['sha256']
        model.write_bytes(b'CHANGED AFTER SAVED CAPTURE')
        op = host.operation(host.api('/local-catalog/prepare', {'catalog': host.listing()['catalog'],
             'entry_id': entry['id'], 'entry_revision': entry['revision'], 'expected': host.expected()}, want=202))
        assert op['status'] == 'failed' and op['worker_drained']
        assert host.listing()['prepared'] is None
        assert host.api(path)['entry']['manifest']['model']['files'][0]['sha256'] == saved_hash
        report['checks'].append('prepare fully rehashes; catalog/config/hardware/session changes revoke review; changed files fail without substitution or stored hash rewrite')

        large = files / 'cancel-only-sparse.gguf'
        with large.open('wb') as stream:
            stream.write(b'GGUF SYNTHETIC CANCELLATION ONLY')
            stream.truncate(512 * 1024 * 1024)
        cancellation_request = {'request_id': new_id(), 'catalog': host.listing()['catalog'],
                                'label': 'Must cancel before publication', 'selection': selection(files, large.name)}
        accepted = host.api('/local-catalog/entries', cancellation_request, want=202)
        cancel = host.api('/local-catalog/operations/' + accepted['operation_id'] + '/cancel', {})
        assert cancel['operation']['cancel_requested'], 'Worker completed before cancellation witness'
        settled = host.operation(accepted)
        assert settled['status'] == 'cancelled' and settled['worker_drained'] and settled['result'] is None
        recovered = host.api('/local-catalog/requests/' + cancellation_request['request_id'])
        assert recovered['receipt'] is None and recovered['operation']['status'] == 'cancelled'
        assert host.listing()['catalog'] == cancellation_request['catalog']
        report['cancellation'] = {'acknowledgement': cancel, 'terminal': settled}
        report['checks'].append('cancelled blocking hash remains owned until drained and publishes neither entry nor durable mutation receipt')

        close_request = {**cancellation_request, 'request_id': new_id(), 'label': 'Shutdown before publication'}
        accepted = host.api('/local-catalog/entries', close_request, want=202)
        running = host.api('/local-catalog/operations/' + accepted['operation_id'])['operation']
        assert running['status'] == 'running' and not running['worker_drained'], 'No in-flight shutdown witness'
        began = time.monotonic()
        host.stop()
        shutdown_ms = round((time.monotonic() - began) * 1000)
        host.start()
        assert host.listing()['catalog'] == close_request['catalog']
        host.api('/local-catalog/requests/' + close_request['request_id'], want=404)
        assert host.listing()['operations'] == [] and host.listing()['prepared'] is None
        assert host.api('/acceleration')['preferences'] == pins
        host.no_runtime()
        host.stop()
        db = host.root / 'local-workspace' / 'profile.sqlite3'
        assert db.stat().st_size <= 32 * 1024 * 1024
        stored = db.read_bytes()
        assert stored[:16] != b'SQLite format 3\x00'
        assert b'SYNTHETIC_PRIVATE_CATALOG_5829' not in stored
        assert all(gone(pid) for pid in host.pids)
        report['shutdown'] = {'in_flight_witness': running, 'graceful_shutdown_ms': shutdown_ms,
                              'backend_processes': len(host.pids), 'all_backend_pids_absent': True,
                              'encrypted_header_and_private_label_check': True}
        report['checks'].append('shutdown drains accepted hash before clean exit; restart has no uncommitted entry/receipt, stale review, runtime or worker; owned profile remains encrypted')
        report['passed'] = True
    except BaseException as error:
        report['passed'] = False
        report['error'] = str(error)
    finally:
        try:
            if host:
                host.close()
        except BaseException as error:
            report['passed'] = False
            report['cleanup_error'] = str(error)
        confirmed = host is None or host.cleanup_confirmed()
        report['cleanup_confirmed'] = confirmed
        report['backend_exits'] = host.exits if host else []
        if not confirmed:
            report['passed'] = False
        report['diagnostic_tree_retained'] = not report['passed']
        if report['passed']:
            # Only successful checks AND confirmed process/reader cleanup allow
            # removal. Failures keep all owned fixtures/profile data for review.
            try:
                shutil.rmtree(root)
            except OSError as error:
                report['passed'] = False
                report['diagnostic_tree_retained'] = root.exists()
                report['fixture_cleanup_error'] = str(error)
        publish_with_custody(report_fd, args.report, report, root, host, confirmed)
    if not report['passed']:
        raise SystemExit('Catalog acceptance did not pass; retained report has the exact failure')


if __name__ == '__main__':
    main()
