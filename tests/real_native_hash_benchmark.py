#!/usr/bin/env python3
"""Opt-in selected-file hashing benchmark; no model execution or root build changes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib


SOURCE = r'''
use smith_managed_runtime::{inspect_selected_artifact, ArtifactKind, Cancellation};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args.len(), 3);
    let path = std::path::Path::new(&args[1]);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let began = std::time::Instant::now();
    let artifact = rt.block_on(inspect_selected_artifact(path, ArtifactKind::Model,
        "explicit selected-file hash benchmark; no execution".into(), args[2].clone(), &Cancellation::new())).unwrap();
    assert_eq!(artifact.sha256, args[2]);
    println!("{{\"sha256\":\"{}\",\"elapsed_ms\":{},\"bytes\":{}}}",artifact.sha256,began.elapsed().as_millis(),path.metadata().unwrap().len());
}
'''


def stamp(path):
    value = path.stat()
    return (value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--model', type=Path, required=True)
    parser.add_argument('--sha256', required=True)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    model = args.model.resolve(strict=True)
    before = stamp(model)
    repo = Path(__file__).resolve().parents[1]
    versions = [package['version'] for package in tomllib.loads((repo / 'Cargo.lock').read_text())['package'] if package['name'] == 'sha2']
    assert len(versions) == 1, 'Require one exact root sha2 version'
    version = versions[0]
    report = {'passed': False, 'selected_model_sha256': args.sha256, 'runs': [],
              'sha2_version': version,
              'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
              'validation_source_sha256': hashlib.sha256((repo / 'crates/smith-managed-runtime/src/plan.rs').read_bytes()).hexdigest(),
              'scope': 'Same production inspect_selected_artifact path, same immutable selected GGUF; no inference, root Cargo change, hash caching, or installed-file write.',
              'limitations': 'One baseline and two optimized warm-file measurements under current host load; not a general storage or inference benchmark.'}
    with tempfile.TemporaryDirectory(prefix='smith-native-hash-benchmark-') as temporary:
        root = Path(temporary)
        (root / 'src').mkdir()
        (root / 'src/main.rs').write_text(SOURCE)
        manifest = '[package]\nname="smith-native-hash-benchmark"\nversion="0.0.0"\nedition="2021"\n[dependencies]\nsmith-managed-runtime={path=' + json.dumps(str(repo / 'crates/smith-managed-runtime')) + '}\ntokio={version="1",features=["rt-multi-thread","time"]}\n[workspace]\n'
        manifest = manifest.replace('[workspace]', 'sha2="=' + version + '"\n[workspace]')
        environment = dict(os.environ, CARGO_BUILD_JOBS='1')
        environment.pop('CARGO_TARGET_DIR', None)
        try:
            for optimized in (False, True):
                (root / 'Cargo.toml').write_text(manifest + ('\n[profile.dev.package.sha2]\nopt-level=3\n' if optimized else ''))
                print('Building isolated ' + ('sha2-opt3' if optimized else 'unoptimized') + ' hash benchmark', flush=True)
                built = subprocess.run(['cargo', 'build', '--offline', '--manifest-path', str(root / 'Cargo.toml')], env=environment, capture_output=True, timeout=180)
                if built.returncode:
                    raise RuntimeError(built.stderr.decode(errors='replace')[-4000:])
                lock = (root / 'Cargo.lock').read_bytes()
                resolved = [package['version'] for package in tomllib.loads(lock.decode())['package'] if package['name'] == 'sha2']
                assert resolved == [version], 'Benchmark must use the exact root hash dependency'
                report['benchmark_lock_sha256'] = hashlib.sha256(lock).hexdigest()
                for attempt in range(2 if optimized else 1):
                    result = subprocess.run([str(root / 'target/debug/smith-native-hash-benchmark'), str(model), args.sha256], capture_output=True, timeout=240)
                    if result.returncode:
                        raise RuntimeError(result.stderr.decode(errors='replace')[-2000:])
                    value = json.loads(result.stdout)
                    assert value['sha256'] == args.sha256 and stamp(model) == before
                    value.update(sha2_opt_level=3 if optimized else 0, attempt=attempt + 1)
                    report['runs'].append(value)
                    print(json.dumps(value), flush=True)
            report['passed'] = True
            report['source_file_identity_unchanged'] = True
            report['measured_at_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
        finally:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    main()
