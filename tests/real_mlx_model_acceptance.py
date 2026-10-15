#!/usr/bin/env python3
"""Opt-in pretrained native MLX language/lifecycle test; disposable local data only.

Acquisition is separate. This harness never downloads, discovers user models,
contacts an existing inference service, or rewrites responses to pass predicates.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
from real_native_model_acceptance import OwnedApp, free_percent, gone, identity, sha, send, complete


class MLXApp(OwnedApp):
    def guard(self):
        super().guard()
        state = self.api('/mlx/status')['runtime']
        if state and state['pid']:
            pid = state['pid']
            self.native_pids.add(pid)
            self.report['last_mlx_runtime'] = state
            result = subprocess.run(['/bin/ps', '-o', 'rss=', '-p', str(pid)],
                                    capture_output=True, timeout=3)
            if result.returncode == 0 and result.stdout.strip():
                rss = int(result.stdout.strip())
                self.report['memory']['peak_sampled_child_rss_kib'] = max(
                    self.report['memory']['peak_sampled_child_rss_kib'], rss)
                assert rss < 3 * 1024 * 1024, 'MLX worker exceeded three-GiB RSS guard'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--binary-sha256', required=True)
    parser.add_argument('--runtime-bundle', type=Path, required=True)
    parser.add_argument('--worker-sha256', required=True)
    parser.add_argument('--model-directory', type=Path, required=True)
    parser.add_argument('--package-sha256', required=True)
    parser.add_argument('--thinking-mode', choices=['model_default', 'disabled'],
                        help='Explicit reviewed mode; omission preserves the legacy request shape.')
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    assert not args.report.exists(), 'Use a new report path; earlier qualification is immutable.'
    binary, bundle, model = args.binary.resolve(), args.runtime_bundle.resolve(), args.model_directory.resolve()
    worker = bundle / 'smith-mlx-worker'
    assert sha(binary) == args.binary_sha256
    assert sha(worker) == args.worker_sha256
    records = {p: (identity(p), sha(p)) for p in [binary, *sorted(bundle.rglob("*")), *sorted(model.iterdir())] if p.is_file()}
    before = free_percent()
    report = {'schema': 'smith.real-mlx-language.v2', 'passed': False,
              'harness_sha256': sha(Path(__file__).resolve()),
              'shared_helper_sha256': sha(Path(__file__).with_name('real_native_model_acceptance.py').resolve()),
              'launch_helper_sha256': sha(Path(__file__).with_name('desktop_boundary_review.py').resolve()),
              'app_sha256': args.binary_sha256, 'worker_sha256': args.worker_sha256,
              'model_package_sha256': args.package_sha256,
              'model': 'mlx-community/Qwen3-0.6B-4bit@73e3e38d981303bc594367cd910ea6eb48349da8',
              'language_model_is_pretrained': True, 'prompts_are_synthetic': True,
              'requested_thinking_mode': args.thinking_mode or 'model_default',
              'thinking_mode_explicitly_selected': args.thinking_mode is not None,
              'existing_profiles_accessed': False, 'network_provider_used': False,
              'model_downloaded_by_test': False, 'cases': [], 'checks': [],
              'memory': {'initial_free_percent': before, 'minimum_sampled_free_percent': before,
                         'peak_sampled_child_rss_kib': 0,
                         'limits': '>=45% free before load, abort below20%; three-GiB worker RSS, twelve-minute overall deadline; sampled RSS does not cover all Metal allocations'}}
    app = None
    temporary = None
    started = time.monotonic()
    try:
        assert before >= 45, 'Insufficient observed memory headroom; no model load'
        temporary = Path(tempfile.mkdtemp(prefix='smith-real-mlx-')).resolve()
        app = MLXApp(binary, Path(temporary).resolve() / 'profile', report)
        app.start()
        status = lambda: app.api('/mlx/status')
        assert status()['runtime'] is None
        report['checks'].append('automatic-local-profile-no-login-no-runtime')
        def settle(operation, seconds=190):
            value = app.poll(status, lambda s: s['operation'] and s['operation']['id'] == operation
                             and s['operation']['status'] != 'running', seconds)
            assert value['operation']['status'] == 'succeeded', value['operation']
            return value
        prepare_started = time.monotonic()
        prepare_input = {
            'worker': {'path': str(worker), 'source': 'local native Swift MLX worker with exact resource closure',
                       'source_revision': 'qwen3-exact-template-compatibility-v1'},
            'model_directory': str(model), 'model_id': 'qwen3-0.6b-4bit', 'capability': 'chat',
            'sampling': {'max_tokens': 256, 'temperature': 0.0, 'top_p': 1.0, 'seed': 7},
            'settings_revision': status()['settings']['revision'], 'expected': app.target()
        }
        if args.thinking_mode is not None:
            prepare_input['thinking_mode'] = args.thinking_mode
        p = settle(app.api('/mlx/prepare', prepare_input)['operation_id'])['prepared']
        report['prepare_ms'] = round((time.monotonic() - prepare_started) * 1000)
        assert p['plan']['model']['package_sha256'] == args.package_sha256
        assert len(p['plan']['runtime_files']) == 7
        report['plan'] = p['plan']
        assert re.fullmatch(r'[0-9a-f]{64}', p['review_id'])
        assert p['thinking_mode'] == (args.thinking_mode or 'model_default')
        review = {'review_id': p['review_id']}
        report['host_review_id'] = p['review_id']
        app.api('/mlx/load', {'plan_id': p['plan']['id'], 'settings_revision': p['settings_revision'],
                             **review, 'confirm': False, 'expected': app.target()}, want=400)
        assert status()['runtime'] is None
        assert free_percent() >= 45, 'Insufficient observed memory before load'
        load_started = time.monotonic()
        loaded = settle(app.api('/mlx/load', {'plan_id': p['plan']['id'],
                       'settings_revision': p['settings_revision'], 'confirm': True,
                       **review, 'expected': app.target()})['operation_id'])
        report['load_ms'] = round((time.monotonic() - load_started) * 1000)
        assert loaded['runtime']['phase'] == 'ready'
        assert loaded['runtime']['backend'] == 'mlx_swift_metal_only'
        assert loaded['runtime']['manifest']['template'] == 'qwen3-default-thinking-v1'
        requested_mode = args.thinking_mode or 'model_default'
        def provenance(message):
            value = message.get('model_provenance')
            assert value and value['format_version'] == 1 and value['provider'] == 'mlx', message
            assert value['review_id'] == p['review_id']
            assert value['runtime_plan_id'] == p['plan']['id']
            assert value['settings_revision'] == p['settings_revision'] + 1
            assert value['worker_sha256'] == args.worker_sha256
            assert value['runtime_bundle_sha256'] == p['plan']['runtime_bundle_sha256']
            assert value['package_sha256'] == args.package_sha256
            assert value['template'] == 'qwen3-default-thinking-v1'
            assert value['thinking_mode'] == requested_mode
            assert value['sampling'] == prepare_input['sampling']
            return value
        def completed(chat_id, accepted, began):
            result = complete(app, chat_id, accepted, began)
            message = next(m for m in app.api('/chats/' + chat_id)['messages']
                           if m['id'] == accepted['message_id'])
            result['model_provenance'] = provenance(message)
            return result
        report['checks'].append('exact-reviewed-pretrained-package-loaded-in-native-metal-only-worker')
        source = ('Synthetic project Nova ships 14 sensors on 19 May 2027. '
                  'Mira Chen is the approval owner. The total budget is USD900. '
                  'A battery test moved dispatch from 12 May to 19 May. No extra sensors may be purchased.')
        chat_id, accepted, began = send(app, 'Real MLX synthetic summary',
            'Summarize this text in two short sentences, including the final date, sensor count, and budget. '
            'Use only the text. /no_think\n' + source)
        summary = completed(chat_id, accepted, began)
        summary['prompt'] = 'Summarize this text in two short sentences, including the final date, sensor count, and budget. Use only the text. /no_think\n' + source
        summary['case'] = 'grounded_summary'
        lower = summary['content'].lower()
        summary['content_predicate_passed'] = all(x in lower for x in ['14', '19', 'may', '900'])
        summary['strict_format_passed'] = (not any(marker in summary['content'] for marker in ['<think>', '</think>', '```'])
                                           and len(re.findall(r'[.!?](?:\s|$)', summary['content'])) == 2)
        summary['predicate_scope'] = 'Required fact tokens and two-sentence punctuation only; not a semantic-faithfulness benchmark.'
        report['cases'].append(summary)
        follow_prompt = 'Using the earlier text, who owns approval? Reply with only the full name. /no_think'
        _, accepted, began = send(app, 'Follow-up', follow_prompt, chat_id)
        follow = completed(chat_id, accepted, began)
        follow.update({'case': 'follow_up_context', 'prompt': follow_prompt,
                       'content_predicate_passed': 'Mira Chen' in follow['content'],
                       'strict_format_passed': follow['content'].strip() == 'Mira Chen'})
        report['cases'].append(follow)
        prompt = 'Translate the Russian phrase into English. Reply with only the translation: Доброе утро. /no_think'
        translated_id, accepted, began = send(app, 'Russian translation', prompt)
        translated = completed(translated_id, accepted, began)
        translated.update({'case': 'russian_to_english', 'prompt': prompt,
                           'content_predicate_passed': 'good morning' in translated['content'].lower(),
                           'strict_format_passed': translated['content'].strip().lower().rstrip('.') == 'good morning'})
        report['cases'].append(translated)
        for case in report['cases']:
            assert case['status'] == 'completed' and case['finish_reason'] == 'stop', case
            assert case['transport_incomplete'] is False, case
        report['checks'].append('three-actual-native-language-responses-saved-with-exact-output-and-provenance')
        # Require a visibly active generation before cancellation; no retries.
        cancel_id, accepted, began = send(app, 'Real MLX cancel',
            'Write a long detailed story with 100 numbered paragraphs about a fictional orange robot. /no_think')
        def active():
            message = next(m for m in app.api('/chats/' + cancel_id)['messages'] if m['id'] == accepted['message_id'])
            return {'message': message, 'runtime': status()['runtime']}
        observed = app.poll(active, lambda v: v['runtime'] and v['runtime']['phase'] == 'generating'
                            and v['message']['status'] == 'streaming', 20)
        worker_pid = observed['runtime']['pid']
        app.native_pids.add(worker_pid)
        start_cancel = time.monotonic()
        app.api('/runs/' + accepted['run_id'] + '/cancel', {})
        cancelled = app.poll(active, lambda v: v['message']['status'] != 'streaming', 20)
        assert cancelled['message']['status'] == 'cancelled', cancelled
        assert gone(worker_pid), 'Cancellation did not reap the actual worker'
        report['cancel_ms'] = round((time.monotonic() - start_cancel) * 1000)
        report['cancelled_turn'] = cancelled['message']
        provenance(cancelled['message'])
        report['checks'].append('active-pretrained-generation-cancelled-and-owned-worker-reaped')
        settings = status()['settings']
        app.stop()
        app.start()
        assert status()['runtime'] is None and status()['settings'] == settings
        for case in report['cases']:
            saved = next(m for m in app.api('/chats/' + case['chat_id'])['messages'] if m['id'] == case['message_id'])
            assert saved['content'] == case['content'] and saved.get('snapshot') == case['snapshot']
            assert provenance(saved) == case['model_provenance']
        saved_cancel = next(m for m in app.api('/chats/' + cancel_id)['messages']
                            if m['id'] == cancelled['message']['id'])
        assert saved_cancel['status'] == 'cancelled'
        assert saved_cancel['content'] == cancelled['message']['content']
        assert provenance(saved_cancel) == cancelled['message'].get('model_provenance')
        report['checks'].append('restart-retains-exact-language-answers-and-settings-without-autoload')
        app.stop()
        db = Path(temporary) / 'profile' / 'local-workspace' / 'profile.sqlite3'
        assert not db.read_bytes().startswith(b'SQLite format 3')
        assert source.encode() not in db.read_bytes()
        assert all(identity(path) == before_id and sha(path) == digest for path, (before_id, digest) in records.items())
        report['checks'].append('encrypted-disposable-data-and-unchanged-selected-model-and-binaries')
        report['language_predicates_passed'] = sum(c['content_predicate_passed'] for c in report['cases'])
        report['language_case_count'] = len(report['cases'])
        report['strict_format_predicates_passed'] = sum(c.get('strict_format_passed', False) for c in report['cases'])
        report['strict_format_case_count'] = sum('strict_format_passed' in c for c in report['cases'])
        report['passed'] = (report['language_predicates_passed'] == len(report['cases'])
                            and report['strict_format_predicates_passed'] == report['strict_format_case_count'])
        report['qualification_scope'] = 'Compact pretrained model on this Apple M1; factual predicates and exact outputs retained. No general language benchmark, other accelerator, or other OS certification.'
    except BaseException as error:
        report['passed'] = False
        report['error'] = str(error)
        raise
    finally:
        cleanup_error = None
        try:
            if app:
                app.close()
                report['owned_pids_gone'] = all(gone(pid) for pid in app.native_pids | app.app_pids)
                assert report['owned_pids_gone'], 'Recorded owned process still exists after cleanup'
            else:
                report['owned_pids_gone'] = True
            if temporary:
                shutil.rmtree(temporary)
        except BaseException as error:
            cleanup_error = error
            report['passed'] = False
            report['cleanup_error'] = str(error)
            if temporary and temporary.exists():
                report['retained_disposable_directory'] = str(temporary)
        finally:
            report['elapsed_seconds'] = round(time.monotonic() - started, 3)
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
            print(json.dumps({'passed': report['passed'], 'checks': len(report['checks']),
                              'language_predicates_passed': report.get('language_predicates_passed'),
                              'strict_format_predicates_passed': report.get('strict_format_predicates_passed'),
                              'report': str(args.report), 'error': report.get('error'),
                              'cleanup_error': report.get('cleanup_error')}, indent=2))
        if cleanup_error:
            raise RuntimeError('Qualification cleanup failed; disposable data retained') from cleanup_error
    if not report['passed']:
        raise SystemExit('Real MLX response acceptance failed; exact outputs retained in report')


if __name__ == '__main__':
    main()
