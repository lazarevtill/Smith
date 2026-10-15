"""Explicit own-bundle qualification only; never selects an existing user profile."""
import argparse
import ctypes
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--bundle', required=True)
parser.add_argument('--native-sha256', required=True)
parser.add_argument('--backend-sha256', required=True)
parser.add_argument('--output', required=True)
args = parser.parse_args()
bundle = Path(args.bundle).resolve(strict=True)
native = bundle / 'Contents/MacOS/Smith'
backend = bundle / 'Contents/Resources/smith-next'

def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

assert digest(native) == args.native_sha256
assert digest(backend) == args.backend_sha256
libproc = ctypes.CDLL('/usr/lib/libproc.dylib')
libproc.proc_pidpath.argtypes = (ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32)
libproc.proc_pidpath.restype = ctypes.c_int

def executable(pid):
    value = ctypes.create_string_buffer(4096)
    count = libproc.proc_pidpath(pid, value, len(value))
    return Path(os.fsdecode(value.value)).resolve() if count > 0 else None

def exists(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False

@contextmanager
def owned_directory():
    directory = Path(tempfile.mkdtemp(prefix='smith-bundle-acceptance-')).resolve()
    complete = False
    try:
        yield directory
        complete = True
    finally:
        if complete:
            shutil.rmtree(directory)
        else:
            print('Qualification failed; owned diagnostic directory retained:', directory)

results = []
with owned_directory() as temporary:
    root = Path(temporary)
    profile = root / 'profile'
    profile.mkdir(mode=0o700)
    home = root / 'synthetic-home'
    home.mkdir(mode=0o700)
    environment = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(home), 'TMPDIR': str(root), 'LANG': 'en_US.UTF-8'}
    for case in ('fresh-workspace', 'reopen-same-workspace'):
        process = subprocess.Popen([str(native), '--profile', str(profile), '--qualification-bounded'], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        owned = set()
        deadline = time.monotonic() + 35
        try:
            while process.poll() is None:
                assert time.monotonic() < deadline, 'Native qualification exceeded its deadline'
                children = subprocess.run(['/usr/bin/pgrep', '-P', str(process.pid)], capture_output=True, text=True, timeout=2)
                assert children.returncode in (0, 1)
                for raw in children.stdout.split():
                    pid = int(raw)
                    if executable(pid) == backend:
                        owned.add(pid)
                time.sleep(0.1)
            output, errors = process.communicate(timeout=2)
            assert process.returncode == 0, (case, 'native exit', process.returncode, errors[:1024].decode(errors='replace'))
            assert output.strip() == b'PASS: bundled native workspace ready; owned backend stopped cleanly.', (case, 'native success marker absent')
            assert errors == b'', (case, 'unexpected stderr', len(errors))
            assert len(owned) == 1, (case, 'expected exactly one owned bundled runtime', len(owned))
            for pid in owned:
                assert not exists(pid), (case, 'owned runtime remains alive', pid)
            assert any(profile.iterdir()), 'Direct automatic workspace was not created'
            results.append({'case': case, 'native_exit': 0, 'native_pid': process.pid, 'owned_backend_pids': sorted(owned), 'bundled_backend_path_verified': True, 'owned_backend_reaped': True, 'workspace_ready_verified_by_native': True, 'normal_quit_verified_by_native': True, 'stdout_bytes': len(output), 'stderr_bytes': len(errors)})
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate(timeout=5)
            cleanup_deadline = time.monotonic() + 10
            while any(exists(pid) for pid in owned) and time.monotonic() < cleanup_deadline:
                time.sleep(0.1)
            assert not any(exists(pid) for pid in owned), 'Owned runtime did not finish cleanup; qualification failed'
    # LaunchServices exercises the .app metadata/entrypoint with an explicit
    # disposable profile. No input automation or account/home profile is used.
    def exact_native_pids():
        found = subprocess.run(['/usr/bin/pgrep', '-x', 'Smith'], capture_output=True, text=True, timeout=2)
        assert found.returncode in (0, 1)
        return {int(raw) for raw in found.stdout.split() if executable(int(raw)) == native}

    assert not exact_native_pids(), 'This exact bundle is already running; never adopt or close it'
    captured_output = root / 'launchservices-stdout.txt'
    captured_errors = root / 'launchservices-stderr.txt'
    launcher = subprocess.Popen(['/usr/bin/open', '-W', '-n', '-o', str(captured_output), '--stderr', str(captured_errors), '--env', 'HOME=' + str(home), '--env', 'TMPDIR=' + str(root), str(bundle), '--args', '--profile', str(profile), '--qualification-bounded'], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    launched = set()
    owned = set()
    deadline = time.monotonic() + 35
    try:
        while launcher.poll() is None:
            assert time.monotonic() < deadline, 'LaunchServices qualification exceeded its deadline'
            current = exact_native_pids()
            launched.update(current)
            assert len(launched) <= 1, 'Unexpected multiple instances of the qualification bundle'
            for native_pid in current:
                children = subprocess.run(['/usr/bin/pgrep', '-P', str(native_pid)], capture_output=True, text=True, timeout=2)
                assert children.returncode in (0, 1)
                for raw in children.stdout.split():
                    pid = int(raw)
                    if executable(pid) == backend:
                        owned.add(pid)
            time.sleep(0.1)
        launcher_output, launcher_errors = launcher.communicate(timeout=2)
        assert launcher.returncode == 0, ('LaunchServices failed', launcher.returncode, launcher_errors[:1024].decode(errors='replace'))
        assert len(launched) == 1 and len(owned) == 1, ('Expected one launched native and bundled runtime', len(launched), len(owned))
        with captured_output.open('rb') as stream:
            output = stream.read(4097)
        with captured_errors.open('rb') as stream:
            errors = stream.read(4097)
        assert len(output) <= 4096 and len(errors) <= 4096, 'LaunchServices output exceeded its bounded log limit'
        assert output.strip() == b'PASS: bundled native workspace ready; owned backend stopped cleanly.', ('Native success marker absent', errors[:1024].decode(errors='replace'))
        assert errors == b'', ('LaunchServices unexpected stderr', len(errors))
        assert not any(exists(pid) for pid in launched | owned), 'LaunchServices native/runtime did not exit'
        results.append({'case': 'launchservices-bundle-open', 'launcher_exit': 0, 'native_pids': sorted(launched), 'owned_backend_pids': sorted(owned), 'bundled_backend_path_verified': True, 'owned_backend_reaped': True, 'workspace_ready_verified_by_native': True, 'normal_quit_verified_by_native': True, 'native_success_marker': True, 'stdout_bytes': len(output), 'stderr_bytes': len(errors)})
    finally:
        # An exact executable path does not prove that a concurrently opened
        # instance belongs to this harness. Never signal discovered native PIDs.
        # Only stop our Popen-owned launcher; the bounded native self-quit must
        # finish on its own, otherwise fail and retain the diagnostic directory.
        if launcher.poll() is None:
            launcher.terminate()
            launcher.communicate(timeout=5)
        cleanup_deadline = time.monotonic() + 10
        while any(exists(pid) for pid in launched | owned) and time.monotonic() < cleanup_deadline:
            time.sleep(0.1)
        assert not any(exists(pid) for pid in launched | owned), 'Owned LaunchServices processes did not finish cleanup'
    assert digest(native) == args.native_sha256
    assert digest(backend) == args.backend_sha256
    report = {'schema': 1, 'bundle': str(bundle), 'native_sha256': args.native_sha256, 'backend_sha256': args.backend_sha256, 'architecture': platform.machine(), 'macos': platform.mac_ver()[0], 'actual_native_window': True, 'profile': 'explicit disposable automatic local file-key workspace', 'normal_home_profile_selected': False, 'os_input_or_capture_used': False, 'app_launch_mode': 'bundled executable with explicit bounded qualification CLI', 'launchservices_bundle_open_qualified': True, 'finder_click_automation_used': False, 'full_tool_parity_qualified': False, 'acceleration_qualified': False, 'results': results, 'disposable_profile_removed': True}
    report['harness_sha256'] = digest(Path(__file__).resolve())
    report['package_manifest_sha256'] = digest(bundle / 'Contents/Resources/PACKAGE-MANIFEST.json')
Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report, indent=2))
