#!/usr/bin/env python3
"""Opt-in real-model language and lifecycle acceptance on an owned normal app.

Requires explicit existing artifact paths and hashes. Never discovers models,
downloads assets, calls an existing model server, or changes installed files.
All prompts are synthetic. The parent retains the exact app listener throughout.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import subprocess
import tempfile
import time

from desktop_boundary_review import launch


def sha(path):
    value = hashlib.sha256()
    with path.open('rb') as source:
        while chunk := source.read(4 * 1024 * 1024):
            value.update(chunk)
    return value.hexdigest()


def identity(path):
    s = path.stat()
    return (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)


def free_percent():
    result = subprocess.run(['/usr/bin/memory_pressure', '-Q'], capture_output=True,
                            check=True, timeout=3, env={'LANG': 'C'})
    match = re.search(rb'System-wide memory free percentage:\s*(\d+)%', result.stdout)
    if not match:
        raise RuntimeError('Memory headroom observation unavailable; no load permitted')
    return int(match[1])


def gone(pid):
    try:
        os.kill(pid, 0)
        return False
    except ProcessLookupError:
        return True


class OwnedApp:
    def __init__(self, binary, profile, report):
        self.binary, self.profile, self.report = binary, profile, report
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.listener.listen(16)
        self.owner, self.session, self.child = None, None, None
        self.native_pids = set()
        self.app_pids = set()
        self.last_memory = 0
        self.began = time.monotonic()

    def start(self):
        self.owner = secrets.token_hex(32)
        # Keep app-created temporary agent workspaces inside this test's owned
        # directory, including across the explicit restart. Runtime environment
        # inheritance is independently cleared by its production supervisor.
        scratch = self.profile.parent / 'app-temporary'
        scratch.mkdir(mode=0o700, exist_ok=True)
        prior_tmp = os.environ.get('TMPDIR')
        os.environ['TMPDIR'] = str(scratch)
        def retain(child):
            self.child = child
            self.app_pids.add(child.pid)
        try:
            self.child = launch(self.binary, self.profile, self.listener, self.owner,
                                on_spawn=retain)
        finally:
            if prior_tmp is None:
                os.environ.pop('TMPDIR', None)
            else:
                os.environ['TMPDIR'] = prior_tmp
        self.app_pids.add(self.child.pid)
        state = self.api('/profile/status')
        assert state['state'] == 'unlocked' and state['automatic']
        self.session = state['session']
        assert self.api('/native/status')['runtime'] is None

    def api(self, path, data=None, method=None, want=200):
        assert self.child is not None and self.child.poll() is None, 'Owned app stopped'
        port = self.listener.getsockname()[1]
        headers = {'Host': f'127.0.0.1:{port}', 'X-Smith-Desktop-Owner': self.owner}
        if self.session:
            headers['X-Smith-Profile-Session'] = self.session
        body = None
        if data is not None:
            headers.update({'Origin': f'http://127.0.0.1:{port}', 'Content-Type': 'application/json'})
            body = json.dumps(data).encode()
        connection = http.client.HTTPConnection('127.0.0.1', port, timeout=8)
        try:
            connection.request(method or ('POST' if data is not None else 'GET'),
                               '/api' + path, body, headers)
            response = connection.getresponse()
            payload = response.read(2 * 1024 * 1024 + 1)
            assert len(payload) <= 2 * 1024 * 1024, 'Response exceeded bound'
            value = json.loads(payload)
            assert response.status == want, f'{path}: HTTP {response.status}: {value}'
            return value
        finally:
            connection.close()

    def target(self):
        config = self.api('/config')
        return {key: config[key] for key in ('revision', 'provider', 'endpoint')}

    def guard(self):
        if time.monotonic() - self.began > 720:
            raise RuntimeError('Qualification exceeded its twelve-minute overall deadline')
        state = self.api('/native/status')['runtime']
        if state and state['pid']:
            pid = state['pid']
            self.native_pids.add(pid)
            self.report['last_runtime'] = state
            result = subprocess.run(['/bin/ps', '-o', 'rss=', '-p', str(pid)],
                                    capture_output=True, timeout=3)
            if result.returncode == 0 and result.stdout.strip():
                rss = int(result.stdout.strip())
                self.report['memory']['peak_sampled_child_rss_kib'] = max(
                    self.report['memory']['peak_sampled_child_rss_kib'], rss)
                if rss > 8 * 1024 * 1024:
                    raise RuntimeError('Owned runtime exceeded the eight-GiB RSS guard')
        if time.monotonic() - self.last_memory >= 1:
            self.last_memory = time.monotonic()
            percent = free_percent()
            self.report['memory']['minimum_sampled_free_percent'] = min(
                self.report['memory']['minimum_sampled_free_percent'], percent)
            if percent < 20:
                raise RuntimeError('System memory free percentage fell below twenty')

    def poll(self, read, predicate, seconds):
        deadline = time.monotonic() + seconds
        while True:
            self.guard()
            value = read()
            if predicate(value):
                return value
            assert time.monotonic() < deadline, 'Bounded operation deadline exceeded'
            time.sleep(.1)

    def setup_operation(self, accepted, seconds=300):
        state = self.poll(lambda: self.api('/acceleration'),
                          lambda s: s['operation'] and s['operation']['id'] == accepted['operation_id']
                          and s['operation']['status'] != 'running', seconds)
        self.report['last_setup_operation'] = state['operation']
        assert state['operation']['status'] == 'succeeded', state['operation']
        return state

    def stop(self):
        if self.child and self.child.poll() is None:
            self.child.send_signal(signal.SIGINT)
            try:
                self.child.wait(timeout=30)
            except subprocess.TimeoutExpired:
                # Emergency cleanup is limited to a recorded, still-direct child.
                # This path fails qualification; it never becomes clean-stop evidence.
                for pid in self.native_pids:
                    p = subprocess.run(['/bin/ps', '-o', 'ppid=', '-p', str(pid)],
                                       capture_output=True, timeout=3)
                    if p.returncode == 0 and p.stdout.strip() == str(self.child.pid).encode():
                        os.kill(pid, signal.SIGKILL)
                self.child.kill()
                self.child.wait(timeout=5)
                raise AssertionError('Owned app did not confirm graceful cleanup')
        if self.child:
            assert self.child.returncode == 0, 'Owned app shutdown failed'
        self.session = None
        for pid in self.native_pids:
            assert gone(pid), 'Owned runtime remains after app shutdown'
        if self.child:
            # Drain before a restart can replace this exact child handle. The
            # readiness owner value and logs are discarded, never published.
            self.child.communicate(timeout=5)

    def close(self):
        failure = None
        try:
            self.stop()
        except BaseException as error:
            failure = error
        # A raised cleanup error must not release the address by terminating
        # this parent while an owned child or pipe writer remains. Retain all
        # custody and retry bounded waits; never signal an orphan/reused PID.
        while True:
            try:
                child_gone = self.child is None or self.child.poll() is not None
                runtimes_gone = all(gone(pid) for pid in self.native_pids)
                if child_gone and runtimes_gone:
                    if self.child:
                        self.child.communicate(timeout=5)
                    break
                if failure is None:
                    failure = AssertionError('Owned cleanup was not confirmed')
                self.stop()
                time.sleep(.1)
            except BaseException as error:
                if failure is None:
                    failure = error
                # Retrying never changes a failed run into success. Interrupts
                # are also failures, rather than evidence of process cleanup.
                try:
                    time.sleep(.1)
                except BaseException:
                    pass
        self.listener.close()
        if failure is not None:
            raise failure


SOURCE = (
    'Synthetic project Cedar-27 supplies 18 sensor kits to the fictional North Pier Lab. '
    'The initial dispatch date was 14 March 2027. Two network defects discovered in the pilot '
    'moved dispatch to 21 March 2027. The approved budget remains USD 12000; the team must not '
    'purchase extra kits. Mina Park owns release approval. Each kit must pass power, network, '
    'and handoff checks before dispatch. Twelve kits have passed all three checks; six are still '
    'waiting for the network fix. No other approval owner or deadline is stated.'
)


def send(app, title, prompt, chat_id=None):
    if chat_id is None:
        chat = app.api('/chats', {'title': title, **app.target()})
        chat_id, version = chat['id'], chat['version']
    else:
        version = app.api('/chats/' + chat_id)['chat']['version']
    began = time.monotonic()
    accepted = app.api('/chats/' + chat_id + '/messages',
                       {'version': version, 'content': prompt, **app.target()})
    return chat_id, accepted, began


def complete(app, chat_id, accepted, began):
    message = app.poll(
        lambda: next(m for m in app.api('/chats/' + chat_id)['messages']
                     if m['id'] == accepted['message_id']),
        lambda m: m['status'] != 'streaming', 75)
    return {'chat_id': chat_id, 'message_id': accepted['message_id'],
            'latency_ms': round((time.monotonic() - began) * 1000),
            'status': message['status'], 'finish_reason': message.get('finish_reason'),
            'content': message['content'], 'snapshot': message.get('snapshot'),
            'transport_incomplete': message.get('transport_incomplete')}


def evaluate_json(result, expected):
    """Keep strict acceptance distinct from diagnostics; never rewrite output."""
    try:
        result['semantic_check'] = json.loads(result['content']) == expected
    except ValueError:
        result['semantic_check'] = False
    result['bare_json_contract_passed'] = result['semantic_check']
    fenced = re.fullmatch(r'```json\s*\n(.*)\n```', result['content'].strip(), re.DOTALL)
    result['exact_expected_values_inside_markdown_fence'] = False
    if fenced:
        try:
            result['exact_expected_values_inside_markdown_fence'] = json.loads(fenced[1]) == expected
        except ValueError:
            pass
    if not result['semantic_check'] and result['exact_expected_values_inside_markdown_fence']:
        result['failure_category'] = 'model_output_formatting_not_history_loss'



def run_agent_task(app, report):
    expected = app.target()
    selection = app.api('/agent/model-selection', expected)
    resources = [{'kind': 'provider_binding', 'id': selection['scope_id']}]
    workspace = app.api('/agent/workspaces', {
        'label': 'Synthetic real Granite read-only workspace',
        'capabilities': ['list', 'read', 'model_inference'],
        'resources': resources, 'confirmed': True})['workspace']
    prompt = 'List the selected workspace once using list_directory, then summarize the result.'
    created = app.api('/agent/tasks', {'workspace_id': workspace['id'],
                      'policy_revision': workspace['policy_revision'], 'input': prompt})
    task = created['task']
    assert task['revision'] == 0 and task['state']['kind'] == 'pending'
    assert set(task['capabilities']) == {'list', 'read', 'model_inference'}
    assert task['resources'] == resources
    assert task['extensions']['items'] == []
    request = {'expected_revision': task['revision'], 'expected': expected,
               'max_steps': 2, 'confirmed': True, 'binding_id': selection['binding_id']}
    started = time.monotonic()
    admitted = app.api('/agent/tasks/' + task['id'] + '/model-runs', request, want=202)['operation']
    result = {'workspace': workspace, 'created': created, 'prompt': prompt,
              'selected_target': selection, 'request': request, 'admitted': admitted,
              'admission_ms': round((time.monotonic() - started) * 1000),
              'replayed': False, 'semantic_check': False,
              'raw_rejected_model_output_retained': False}
    report['agent_task'] = result
    path = '/agent/tasks/' + task['id']
    terminal = app.poll(lambda: app.api(path + '/model-runs/' + admitted['id'])['operation'],
                        lambda value: value['phase'] != 'running', 300)
    result['latency_ms'] = round((time.monotonic() - started) * 1000)
    result['terminal'] = terminal
    result['task'] = app.api(path)['task']
    result['trace'] = app.api(path + '/trace?limit=100')
    assert result['trace']['next_after'] is None, 'Two-step trace must fit bounded page'
    assert terminal['worker_drained'] and not terminal['cleanup_unconfirmed']
    events = [row['event'] for row in result['trace']['events']]
    rejection = result['task'].get('last_model_rejection')
    result['raw_rejected_model_output_retained'] = rejection is not None
    if rejection is not None:
        evidence = rejection['reply']
        prefix = evidence['text'].encode('utf-8')
        assert len(prefix) <= 16 * 1024 and evidence['original_bytes'] >= len(prefix)
        assert re.fullmatch(r'[0-9a-f]{64}', evidence['sha256'])
        if not evidence['truncated']:
            assert evidence['original_bytes'] == len(prefix)
            assert hashlib.sha256(prefix).hexdigest() == evidence['sha256']
        assert any(event['kind'] == 'model_rejected'
                   and event['step_id'] == rejection['step_id']
                   and event['reply'] == evidence for event in events)
        result['rejected_reply_diagnostic'] = rejection
    starts = [e for e in events if e['kind'] == 'tool_started']
    finishes = [e for e in events if e['kind'] == 'tool_finished']
    assert all(e['call']['name'] in ('list_directory', 'read_file') for e in starts), 'Tool escaped read-only grant'
    replies = [e['reply'] for e in events if e['kind'] == 'model_finished']
    result['durable_model_replies'] = replies
    result['dispatched_tools'] = [e['call']['name'] for e in starts]
    result['tool_results'] = finishes
    result['semantic_check'] = (terminal['phase'] == 'succeeded'
        and result['task']['state']['kind'] == 'completed'
        and result['dispatched_tools'] == ['list_directory']
        and len(finishes) == 1 and 'Ok' in finishes[0]['result'])
    report['checks'].append('one real model task admitted once with list/read-only grants; exact terminal and durable trace retained without retry')
    print(json.dumps({'case': 'real_read_only_agent_task', 'phase': terminal['phase'],
                      'latency_ms': result['latency_ms'], 'semantic_check': result['semantic_check'],
                      'dispatched_tools': result['dispatched_tools']}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('binary', 'executable', 'model', 'report', 'raw_report'):
        parser.add_argument('--' + key.replace('_', '-'), required=True, type=Path)
    for key in ('binary_sha256', 'executable_sha256', 'model_sha256', 'runtime_revision'):
        parser.add_argument('--' + key.replace('_', '-'), required=True)
    parser.add_argument('--suite', choices=('language', 'agent', 'both'), default='language')
    parser.add_argument('--require-startup-observer', action='store_true')
    args = parser.parse_args()
    for key in ('binary', 'executable', 'model'):
        path = getattr(args, key).resolve(strict=True)
        setattr(args, key, path)
        assert path.is_file() and sha(path) == getattr(args, key + '_sha256'), f'Changed {key}'
    assert os.uname().sysname == 'Darwin', 'This resource guard is qualified only on macOS'
    initial = free_percent()
    source_identity = {key: identity(getattr(args, key)) for key in ('binary', 'executable', 'model')}
    report = {'passed': False, 'executed_at_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'harness_sha256': sha(Path(__file__)),
              'app_sha256': args.binary_sha256,
              'executable': {'path': str(args.executable), 'sha256': args.executable_sha256,
                             'revision': args.runtime_revision},
              'model': {'path': str(args.model), 'sha256': args.model_sha256,
                        'bytes': args.model.stat().st_size, 'source': 'registry.ollama.ai/ibm/granite4.1:8b'},
              'content': 'synthetic prompts and real installed Granite4.1 8B weights',
              'memory': {'initial_free_percent': initial, 'minimum_sampled_free_percent': initial,
                         'peak_sampled_child_rss_kib': 0, 'stop_below_free_percent': 20,
                         'stop_above_child_rss_kib': 8 * 1024 * 1024,
                         'limitations': 'memory_pressure -Q is system-wide; process RSS does not measure all Metal unified-memory allocations. These guards are observations, not a validated memory-fit bound.'},
              'suite': args.suite, 'outputs': [], 'checks': [], 'timings_ms': {},
              'scope': {'downloads': False, 'existing_server_calls': False, 'private_prompts': False,
                        'installed_asset_writes': False, 'native_window': False,
                        'hardware_placement_proven': False, 'general_optimization_proven': False}}
    app = None
    with tempfile.TemporaryDirectory(prefix='smith-real-native-acceptance-') as temporary:
        try:
            assert initial >= 45, f'Defer model load: observed {initial}% system free-memory reading; require at least45%'
            app = OwnedApp(args.binary, Path(temporary).resolve() / 'profile', report)
            app.start()
            app.api('/config/reviewed', {'expected': app.target(),
                    'config': {'provider': 'managed', 'endpoint': '', 'model': ''}}, 'PUT')
            inventory = app.setup_operation(app.api('/acceleration/inventory', {'expected_revision': 0}))['inventory']
            report['initial_hardware'] = inventory
            devices = [d for d in inventory['inventory']['devices']
                       if d['class'] == 'gpu' and d['vendor'] == 'apple']
            assert len(devices) == 1, 'Require exactly one observed Apple GPU identity'
            intent = {'mode': 'manual', 'preference': 'balanced', 'capability': 'text_chat',
                      'local_fallback': 'never', 'overrides': {'backend': 'llama_metal',
                      'device_id': devices[0]['id'],
                      'context_tokens': 2048, 'output_tokens': 256, 'threads': 4,
                      'prefill_threads': 4, 'batch_tokens': 128, 'microbatch_tokens': 64,
                      'gpu_layers': 40, 'kv_type': 'f16', 'flash_attention': False,
                      'mmap': True, 'mlock': False}}
            settings = app.api('/acceleration/preferences', {'expected_revision': 0, 'intent': intent}, 'PUT')
            revision = settings['preferences']['revision']
            started = time.monotonic()
            selection = app.setup_operation(app.api('/acceleration/inspect', {
                'executable': {'path': str(args.executable), 'source': 'explicit installed Ollama runtime',
                               'source_revision': args.runtime_revision},
                'model': {'path': str(args.model), 'source': report['model']['source'],
                          'source_revision': args.model_sha256},
                'model_id': 'granite-4.1-8b-real-language',
                'expected_revision': revision, **app.target()}))['selection']
            report['timings_ms']['inspect'] = round((time.monotonic() - started) * 1000)
            assert selection['files']['executable']['sha256'] == args.executable_sha256
            assert selection['files']['model']['sha256'] == args.model_sha256
            report['metadata'] = selection['files']['metadata']
            started = time.monotonic()
            planned = app.setup_operation(app.api('/acceleration/plan', {
                'selection_id': selection['id'], 'probe_id': selection['probe']['id'],
                'expected_revision': revision, 'confirm_probe': True, **app.target()}))
            report['timings_ms']['probe_and_prepare'] = round((time.monotonic() - started) * 1000)
            report['hardware'] = planned['inventory']
            report['preview'] = planned['preview']
            report['reviewed_plan'] = planned['prepared']
            plan = planned['prepared']['plan']
            assert planned['prepared']['memory_fit'] == 'unknown'
            assert planned['prepared']['qualification'] == 'unvalidated'
            assert plan['config']['backend'] == {'device': {'name': 'MTL0', 'gpu_layers': 40}}
            assert plan['config']['context_tokens'] == 2048
            assert app.api('/native/status')['runtime']['pid'] is None
            report['checks'].append('exact selected hashes and current CLI/OS Metal identity reviewed before explicit load')
            before_apply = free_percent()
            report['memory']['before_explicit_apply_free_percent'] = before_apply
            assert before_apply >= 45, f'Defer explicit load: observed {before_apply}% system free-memory reading; require at least45%'
            print('Real native acceptance: reviewed plan; loading exact MTL0/40-layer request', flush=True)
            started = time.monotonic()
            admitted = app.api('/acceleration/apply', {
                'plan_id': plan['id'], 'expected_revision': revision,
                'confirm': True, **app.target()})
            native = app.poll(lambda: app.api('/native/status'),
                              lambda s: s['operation'] and s['operation']['id'] == admitted['operation_id']
                              and s['operation']['status'] != 'running', 300)
            assert native['operation']['status'] == 'succeeded', native['operation']
            loaded = app.api('/acceleration')
            ready = native['runtime']
            report['timings_ms']['load_and_revalidation'] = round((time.monotonic() - started) * 1000)
            assert ready['phase'] == 'ready' and ready['execution_kind'] == 'native'
            assert ready['observed_backend'] == 'unknown'
            report['ready_runtime'] = ready
            observation = native.get('startup_observation')
            report['startup_observation'] = observation
            report['runtime_reported_allocation_records_present'] = bool(
                observation and observation['placement']['kind'] == 'runtime_reported')
            if args.require_startup_observer:
                assert observation is not None, 'This qualification requires the new bounded startup observer'
            if observation:
                observed_identity = observation['identity']
                assert observed_identity['target'] == ready['target'] and observed_identity['pid'] == ready['pid']
                assert observed_identity['plan_id'] == ready['plan_id'] == plan['id']
                assert observed_identity['executable_sha256'] == args.executable_sha256
                assert observed_identity['execution_kind'] == 'native' and observation['retention_closed']
                assert observation['kernel_execution_verified'] is False
                assert all(observation[stream]['inspected_bytes'] <= observation['max_bytes_per_stream']
                           and observation[stream]['retained_lines'] <= observation['max_lines_per_stream']
                           for stream in ('stdout', 'stderr'))
                report['checks'].append('startup metadata matches exact native executable/plan/PID/generation; runtime-reported claims remain distinct from kernel profiling')
            report['persisted_preferences'] = loaded['preferences']
            app.native_pids.add(ready['pid'])
            if args.suite in ('language', 'both'):
                print('Real native acceptance: ready; running four synthetic language cases', flush=True)

                summary_prompt = 'Summarize this synthetic project in exactly two short sentences. Include the dispatch change, its reason, and the budget constraint. Use only the supplied facts.\n\n' + SOURCE
                chat_id, accepted, began = send(app, 'Synthetic Cedar project summary', summary_prompt)
                result = complete(app, chat_id, accepted, began)
                text = result['content'].lower()
                sentence_count = len([part for part in re.split(r'[.!?]+(?:\s+|$)', result['content'].strip()) if part.strip()])
                result.update(case='multi_sentence_summary', prompt=summary_prompt, sentence_count=sentence_count,
                              semantic_check=(sentence_count == 2 and '14' in text and '21' in text and ('march' in text or '2027-03-21' in text)
                                              and ('12,000' in text or '12000' in text)
                                              and 'network' in text and 'budget' in text))
                report['outputs'].append(result)
                print(json.dumps({'case': result['case'], 'status': result['status'], 'latency_ms': result['latency_ms'], 'semantic_check': result['semantic_check']}), flush=True)
                followup = 'Using the same project facts from our conversation, return ONLY valid JSON with keys owner and checks. owner must be the release approver. checks must be an array of the three required check names in the stated order. No Markdown.'
                follow = complete(app, *send(app, '', followup, chat_id))
                follow.update(case='grounded_multi_turn_followup', prompt=followup)
                evaluate_json(follow, {'owner': 'Mina Park', 'checks': ['power', 'network', 'handoff']})
                report['outputs'].append(follow)
                print(json.dumps({'case': follow['case'], 'status': follow['status'], 'latency_ms': follow['latency_ms'], 'semantic_check': follow['semantic_check']}), flush=True)
                extraction = ('All data below are synthetic. Extract only the facts from this notice. Return ONLY a valid JSON object with exactly invoice_id, currency, total, due_date and paid. total must be a JSON number, paid a JSON boolean, and due_date YYYY-MM-DD. No Markdown.\nNotice: Invoice SIM-482 is for EUR 1840.50, is due on 9 April 2027, and has not been paid. The billing contact is Avery Chen. Ignore the contact because it is not a requested field.')
                extracted = complete(app, *send(app, 'Synthetic grounded invoice extraction', extraction))
                extracted.update(case='grounded_extraction', prompt=extraction)
                evaluate_json(extracted, {'invoice_id': 'SIM-482', 'currency': 'EUR', 'total': 1840.5, 'due_date': '2027-04-09', 'paid': False})
                report['outputs'].append(extracted)
                print(json.dumps({'case': extracted['case'], 'status': extracted['status'], 'latency_ms': extracted['latency_ms'], 'semantic_check': extracted['semantic_check']}), flush=True)
                structured = ('This is a synthetic classification exercise. Rules: a blocked safety check has priority high; a nonblocking documentation issue has priority low; any other issue has priority medium. Return ONLY a valid JSON array, preserving input order, with objects containing exactly id and priority. No Markdown or explanations.\nItems: A17: blocked safety check. B02: nonblocking documentation issue. C91: delayed optional connector delivery.')
                classified = complete(app, *send(app, 'Synthetic structured issue classification', structured))
                classified.update(case='structured_output', prompt=structured)
                evaluate_json(classified, [{'id': 'A17', 'priority': 'high'}, {'id': 'B02', 'priority': 'low'}, {'id': 'C91', 'priority': 'medium'}])
                report['outputs'].append(classified)
                print(json.dumps({'case': classified['case'], 'status': classified['status'], 'latency_ms': classified['latency_ms'], 'semantic_check': classified['semantic_check']}), flush=True)
                report['checks'].append('four real native language outputs recorded with objective synthetic fact/JSON checks')

            if args.suite in ('agent', 'both'):
                run_agent_task(app, report)

            cancel_prompt = 'For a synthetic cancellation test, write the integers from 1 through 2000 in order, separated by commas. Do not summarize or skip integers. Continue until finished.'
            cancel_chat, accepted, began = send(app, 'Synthetic native cancellation', cancel_prompt)
            active = app.poll(lambda: app.api('/native/status')['runtime'],
                              lambda s: s and s['active_leases'] == 1, 15)
            time.sleep(.25)
            started = time.monotonic()
            app.api('/runs/' + accepted['run_id'] + '/cancel', {})
            cancelled = complete(app, cancel_chat, accepted, began)
            report['timings_ms']['cancel_to_terminal'] = round((time.monotonic() - started) * 1000)
            stopped = app.api('/native/status')['runtime']
            assert cancelled['status'] == 'cancelled', cancelled
            assert stopped['pid'] is None and stopped['active_leases'] == 0
            assert stopped['spawned'] == stopped['reaped'] == 1 and gone(active['pid'])
            report['cancellation'] = {'active_runtime_witness': active, 'message': cancelled,
                                      'after_runtime': stopped, 'owned_pid_absent': True,
                                      'streaming_partial_text_supported': False}
            report['checks'].append('active real generation cancellation reaps exact owned child before terminal chat state')
            saved = {entry['message_id']: entry['content'] for entry in report['outputs']}
            app.stop()
            app.start()
            assert app.api('/native/status')['runtime'] is None
            for entry in report['outputs']:
                persisted = next(m for m in app.api('/chats/' + entry['chat_id'])['messages']
                                 if m['id'] == entry['message_id'])
                assert persisted['content'] == saved[entry['message_id']]
                assert persisted['snapshot']['provider'] == 'managed'
            if 'agent_task' in report:
                recorded = report['agent_task']
                path = '/agent/tasks/' + recorded['created']['task']['id']
                restored = app.api(path)['task']
                trace = app.api(path + '/trace?limit=100')
                prior = recorded['trace']['events']
                assert trace['events'][:len(prior)] == prior
                assert restored['model_steps'] == recorded['task']['model_steps']
                assert restored['last_model_reply'] == recorded['task']['last_model_reply']
                assert restored.get('last_model_rejection') == recorded['task'].get('last_model_rejection')
                if recorded['task']['state']['kind'] == 'completed':
                    assert restored['state'] == recorded['task']['state']
                operations = app.api(path + '/model-runs')['operations']
                assert operations == [], 'Restart must not retain a live operation or replay inference'
                recorded['after_restart'] = {'task': restored, 'trace': trace, 'operations': operations}
                report['checks'].append('restart preserves task/reply/trace without model reload, operation replay or repeated tool dispatch')
            retained = next(m for m in app.api('/chats/' + cancel_chat)['messages']
                            if m['id'] == cancelled['message_id'])
            assert retained['status'] == 'cancelled'
            report['checks'].append('restart retains exact outputs/provenance/cancelled turn and never automatically loads model')
            app.stop()
            report['lifecycle'] = {'native_processes': len(app.native_pids),
                                   'backend_processes': len(app.app_pids),
                                   'all_native_pids_absent': all(gone(pid) for pid in app.native_pids),
                                   'all_backend_pids_absent': all(gone(pid) for pid in app.app_pids)}
            assert all(identity(getattr(args, key)) == before for key, before in source_identity.items())
            report['source_file_identity_unchanged'] = True
            report['passed'] = (all(r['semantic_check'] and r['status'] == 'completed' for r in report['outputs'])
                                and report.get('agent_task', {'semantic_check': True})['semantic_check'])
        except BaseException as error:
            report['error'] = str(error)
            raise
        finally:
            try:
                if app:
                    app.close()
            finally:
                raw = json.dumps(report, indent=2, ensure_ascii=False) + '\n'
                args.raw_report.parent.mkdir(parents=True, exist_ok=True)
                args.raw_report.write_text(raw)
                args.raw_report.chmod(0o600)
                sanitized = raw.replace(str(args.model.parent.parent), '[selected local Ollama model cache]')
                sanitized = sanitized.replace(str(Path.home()), '[local home]')
                public = json.loads(sanitized)
                public['path_redaction'] = 'Selected personal model-cache/home prefixes redacted. Plan identity was computed with the original selected absolute path; hashes unchanged.'
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(public, indent=2, ensure_ascii=False) + '\n')
                print(json.dumps({'passed': report['passed'], 'report': str(args.report),
                                  'cases': [(r['case'], r['status'], r['semantic_check']) for r in report['outputs']]}), flush=True)
    if not report['passed']:
        raise SystemExit('Real model acceptance did not meet all recorded objective checks')


if __name__ == '__main__':
    main()
