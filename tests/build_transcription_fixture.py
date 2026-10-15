#!/usr/bin/env python3
"""Build an owned synthetic native protocol fixture; this is not an ASR model."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess
import wave


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, help='new fixture directory')
    args = parser.parse_args()
    root = Path(args.output).resolve()
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    source = Path(__file__).resolve().parent / 'fixtures' / 'transcription_child.c'
    executable = root / 'synthetic-transcriber'
    subprocess.run(['/usr/bin/clang', '-O1', '-o', str(executable), str(source)], check=True)
    os.chmod(executable, 0o500)
    model = root / 'synthetic-header-only-model.bin'
    model.write_bytes(b'lmgg' + struct.pack('<11I', 50000, 1500, 384, 6, 4, 448, 384, 6, 4, 80, 1))
    for name, mode in [('synthetic.wav', 0), ('synthetic-hold.wav', 1)]:
        with wave.open(str(root / name), 'wb') as output:
            output.setnchannels(1)
            output.setsampwidth(2)
            output.setframerate(16000)
            output.writeframes(bytes([mode]) + bytes(31999))
    records = [{'name': p.name, 'bytes': p.stat().st_size,
                'sha256': hashlib.sha256(p.read_bytes()).hexdigest()}
               for p in [executable, model, root / 'synthetic.wav', root / 'synthetic-hold.wav']]
    report = {'schema': 'smith.transcription-protocol-fixture.v1', 'real_asr': False,
              'notice': 'Native C protocol fixture emits predetermined text; header-only model has no tensors.',
              'files': records}
    (root / 'fixture.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()
