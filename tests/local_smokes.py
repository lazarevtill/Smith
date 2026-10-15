#!/usr/bin/env python3
"""Run the product and HTTP boundary checks only against our disposable app."""
import argparse
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default=str(root / 'target/debug/smith-next'))
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    with tempfile.TemporaryDirectory(prefix='smith-owned-smokes-') as folder:
        temp = Path(folder)
        log = temp / 'app.log'
        with log.open('w') as output:
            process = subprocess.Popen([binary], stdout=output, stderr=output, env={
                **os.environ, 'SMITH_NEXT_SYNTHETIC_PROFILE': 'plaintext', 'SMITH_NEXT_DATA_DIR': str(temp / 'profile'), 'SMITH_NEXT_PORT': '0',
            })
            try:
                base = None
                for _ in range(200):
                    match = re.search(r'http://127\.0\.0\.1:\d+', log.read_text())
                    if match:
                        base = match[0]
                        break
                    assert process.poll() is None, 'Disposable app stopped before readiness'
                    time.sleep(.025)
                assert base, 'Disposable app readiness timeout'
                env = {**os.environ, 'SMITH_NEXT_SMOKE_URL': base}
                subprocess.run([sys.executable, str(root / 'tests/product_smoke.py')], env=env, check=True, timeout=90)
                subprocess.run([sys.executable, str(root / 'tests/security_smoke.py'), '--disposable-profile'], env=env, check=True, timeout=120)
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try:
                        process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=3)
                        raise AssertionError('Disposable app did not shut down gracefully')
                assert process.returncode == 0, 'Disposable app exited unsuccessfully'


if __name__ == '__main__':
    main()
