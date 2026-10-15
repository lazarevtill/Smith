#!/usr/bin/env python3
"""Compile normal backend or native UI; never launch it or install dependencies."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time
from verify_ui_snapshot import verify_snapshot

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {'linux': 'x86_64-unknown-linux-gnu', 'windows': 'x86_64-pc-windows-msvc'}
COMPONENTS = {
    'backend': ('1.89.0', 'Cargo.toml', 'smith-next'),
    'native': ('1.92.0', 'desktop/slint/Cargo.toml', 'smith-native-desktop'),
}


def command_for(component, target, offline=False):
    toolchain, manifest, binary = COMPONENTS[component]
    command = ['rustup', 'run', toolchain, 'cargo', 'build', '--locked', '--release',
               '--manifest-path', str(ROOT / manifest), '--target', target, '--bin', binary]
    if component == 'native':
        command.extend(['--features', 'native-ui'])
    if offline:
        command.append('--offline')
    return command


def output(command, **kwargs):
    return subprocess.check_output(command, text=True, timeout=30, **kwargs).strip()


def verify_ui(path):
    return verify_snapshot(path)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        while data := stream.read(1024 * 1024):
            h.update(data)
    return h.hexdigest()


def build_environment(env):
    # Explicit metadata allowlist only. Never dump environment values or tokens.
    def bounded(value):
        if value is None:
            return None
        return ''.join(ch for ch in str(value) if ch.isprintable())[:256]
    return {
        'os': {'system': bounded(platform.system()), 'release': bounded(platform.release()),
               'version': bounded(platform.version()), 'architecture': bounded(platform.machine())},
        'runner_image': {key: bounded(env.get(key)) for key in
                         ('ImageOS', 'ImageVersion', 'RUNNER_OS', 'RUNNER_ARCH')},
        'system_package_versions': 'not_captured; runner image labels are not an exact dependency lock',
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--component', choices=COMPONENTS, required=True)
    parser.add_argument('--platform', choices=TARGETS, required=True)
    parser.add_argument('--ui-dir', type=Path, default=ROOT / 'vendor/smith-ui',
                        help='Explicit snapshot directory; defaults to this repository vendor/smith-ui')
    parser.add_argument('--offline', action='store_true', help='Require existing Cargo dependencies')
    parser.add_argument('--dry-run', action='store_true', help='Print command only; perform no subprocess or write')
    args = parser.parse_args()
    target = TARGETS[args.platform]
    command = command_for(args.component, target, args.offline)
    if args.dry_run:
        print(json.dumps({'command': command, 'build_only': True,
                          'ui_source_required': args.component == 'native'}, indent=2))
        return
    if platform.system().lower() != args.platform or platform.machine().lower() not in ('x86_64', 'amd64'):
        raise ValueError('Run on the matching native x64 OS; this script does not pretend cross-compilation is native CI')
    for key in ('RUSTC_BOOTSTRAP', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC', 'RUSTC_WRAPPER',
                'RUSTC_WORKSPACE_WRAPPER'):
        if os.environ.get(key):
            raise ValueError(f'Unset {key}; the build must use the declared normal compiler and cfgs')
    env = dict(os.environ)
    env['CARGO_BUILD_JOBS'] = '1'
    toolchain, manifest, binary = COMPONENTS[args.component]
    version = output(['rustup', 'run', toolchain, 'rustc', '-vV'])
    if f'release: {toolchain}\n' not in version + '\n' or f'host: {target}\n' not in version + '\n':
        raise ValueError('Selected compiler version/host differs from the declared build lane')
    ui_snapshot = None
    if args.component == 'native':
        ui_dir, ui_snapshot = verify_ui(args.ui_dir)
        env['SMITH_ORIGINAL_UI_DIR'] = str(ui_dir)
    # Never share a target directory with a developer or another CI component.
    target_dir = ROOT / 'target/ci' / args.platform / args.component
    target_dir.mkdir(parents=True, exist_ok=True)
    env['CARGO_TARGET_DIR'] = str(target_dir)
    lock = (ROOT / manifest).parent / 'Cargo.lock'
    lock_hash = digest(lock)
    cargo_version = output(['rustup', 'run', toolchain, 'cargo', '--version'])
    source_commit = output(['git', '-C', str(ROOT), 'rev-parse', 'HEAD'])
    source_changed = bool(output(['git', '-C', str(ROOT), 'status', '--porcelain', '--untracked-files=normal']))
    run_id = str(time.time_ns())
    log = target_dir / ('build-' + run_id + '.log')
    report_path = target_dir / ('build-' + run_id + '.json')
    report = {'component': args.component, 'target': target, 'rustc': version, 'cargo': cargo_version,
              'command': command, 'cargo_lock_sha256': lock_hash,
              'manifest_sha256': digest(ROOT / manifest), 'source_commit': source_commit,
              'source_has_changes': source_changed, 'build_environment': build_environment(env),
              'reviewed_ui_snapshot': ui_snapshot, 'build_only': True,
              'runtime_tested': False, 'hardware_qualified': False, 'published': False}
    began = time.monotonic()
    with log.open('x', encoding='utf-8') as stream:
        result = subprocess.run(command, cwd=ROOT, env=env, stdout=stream, stderr=subprocess.STDOUT)
    report.update(returncode=result.returncode, elapsed_seconds=round(time.monotonic() - began, 3))
    if digest(lock) != lock_hash:
        report['lockfile_changed'] = True
    executable = target_dir / target / 'release' / (binary + ('.exe' if args.platform == 'windows' else ''))
    if result.returncode == 0 and executable.is_file() and not report.get('lockfile_changed'):
        report['executable_sha256'] = digest(executable)
        report['executable_bytes'] = executable.stat().st_size
        report['passed'] = True
    else:
        report['passed'] = False
    with report_path.open('x', encoding='utf-8') as stream:
        json.dump(report, stream, indent=2)
        stream.write('\n')
    # Job logs retain build identity even though this template uploads no files.
    print(json.dumps({'passed': report['passed'], 'report': str(report_path), 'log': str(log),
                      'component': args.component, 'target': target,
                      'source_commit': source_commit, 'source_has_changes': source_changed,
                      'compiler': version, 'cargo': cargo_version,
                      'cargo_lock_sha256': lock_hash, 'manifest_sha256': report['manifest_sha256'],
                      'executable_sha256': report.get('executable_sha256'),
                      'executable_bytes': report.get('executable_bytes'),
                      'reviewed_ui_snapshot': ui_snapshot,
                      'build_environment': report['build_environment'],
                      'runtime_tested': False, 'hardware_qualified': False}))
    if not report['passed']:
        with log.open('rb') as stream:
            stream.seek(max(0, log.stat().st_size - 32000))
            sys.stderr.write(stream.read().decode('utf-8', 'replace'))
        raise SystemExit(result.returncode or 1)


if __name__ == '__main__':
    main()
