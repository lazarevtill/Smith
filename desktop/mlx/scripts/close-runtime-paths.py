#!/usr/bin/env python3
"""Build-time only: remove unused SwiftPM search paths after proving OS-only imports."""
import pathlib
import re
import subprocess
import sys

binary = pathlib.Path(sys.argv[1]).resolve(strict=True)


def run(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, timeout=30)


def inspect():
    links = []
    for line in run('/usr/bin/otool', '-L', str(binary)).splitlines():
        if not line.startswith('\t'):
            continue
        name = line.strip().split(' (', 1)[0]
        if not name.startswith(('/System/Library/', '/usr/lib/')) or '..' in name.split('/'):
            raise SystemExit('Refusing to remove search paths: a non-system dylib is still imported.')
        links.append(name)
    if not links:
        raise SystemExit('No native dylib imports were inspected.')
    commands = run('/usr/bin/otool', '-l', str(binary))
    if 'LC_DYLD_ENVIRONMENT' in commands:
        raise SystemExit('Embedded dyld environment is unsupported.')
    rpaths = []
    for command in commands.split('Load command ')[1:]:
        if re.search(r'(?m)^\s*cmd LC_RPATH\s*$', command):
            match = re.search(r'(?m)^\s*path (.+) \(offset \d+\)\s*$', command)
            if not match:
                raise SystemExit('Unrecognized RPATH command.')
            rpaths.append(match.group(1))
    return rpaths


rpaths = inspect()
if rpaths:
    args = ['/usr/bin/install_name_tool']
    for path in rpaths:
        args += ['-delete_rpath', path]
    run(*args, str(binary))
    # Mach-O mutation invalidates the SwiftPM ad-hoc signature. No identity/keychain is used.
    run('/usr/bin/codesign', '--force', '--sign', '-', '--timestamp=none', str(binary))
if inspect():
    raise SystemExit('Native runtime search paths remain after cleanup.')
run('/usr/bin/codesign', '--verify', '--strict', str(binary))
print('Verified OS-only native dylibs and zero runtime search paths.')
