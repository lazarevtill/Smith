#!/usr/bin/env python3
"""Pure policy and reviewed markup tests on the actual host, no application launch."""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--toolchain', default='1.92.0')
    parser.add_argument('--ui-dir', type=Path, required=True)
    args = parser.parse_args()
    if args.toolchain not in ('1.89.0', '1.92.0'):
        raise ValueError('Only reviewed compiler versions are accepted')
    ui = args.ui_dir.resolve(strict=True)
    if not (ui / 'ui/app-window.slint').is_file():
        raise ValueError('Explicit reviewed UI source is required')
    env = dict(os.environ)
    env['SMITH_ORIGINAL_UI_DIR'] = str(ui)
    # The enclosing CI build verifies exact original commit and a clean tree.
    with tempfile.TemporaryDirectory(prefix='smith-platform-checks-') as directory:
        for source in ['desktop/slint/src/platform_ui.rs', 'desktop/slint/tests/platform_markup.rs']:
            binary = Path(directory) / (Path(source).stem + ('.exe' if os.name == 'nt' else ''))
            subprocess.run(['rustup', 'run', args.toolchain, 'rustc', '--edition=2021', '--test',
                            str(ROOT / source), '-o', str(binary)], check=True, env=env, timeout=120)
            subprocess.run([str(binary), '--test-threads=1'], check=True, env=env, timeout=30)


if __name__ == '__main__':
    main()
