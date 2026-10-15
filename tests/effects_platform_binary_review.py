#!/usr/bin/env python3
"""Independent, synthetic-only qualification of native Apple executable copies.

No profile/user documents, network, package signing, or system changes. Run on
macOS outside an enclosing sandbox so the product's Seatbelt policy can apply.
"""
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import tempfile


def limits():
    os.setpgid(0, 0)
    for kind, amount in (
        (resource.RLIMIT_CORE, 0),
        (resource.RLIMIT_FSIZE, 8 * 1024 * 1024),
        (resource.RLIMIT_NOFILE, 64),
        (resource.RLIMIT_CPU, 31),
    ):
        resource.setrlimit(kind, (amount, amount))


def execute(executable, workspace, sandboxed):
    literal = json.dumps(str(executable))
    policy = (
        '(version 1)(deny default)(allow process-exec (literal ' + literal + '))'
        '(allow file-read-metadata)(allow sysctl-read)'
        '(allow file-read* (literal "/") (subpath "/System/Library")'
        ' (subpath "/usr/lib") (literal "/dev/null")'
        ' (literal "/dev/urandom") (literal ' + literal + '))'
        '(allow file-write* (literal "/dev/null"))'
        '(allow file-read* file-write* (subpath ' + json.dumps(str(workspace)) + '))'
    )
    argv = [str(executable), "synthetic-reviewed-echo"]
    if sandboxed:
        argv = ["/usr/bin/sandbox-exec", "-p", policy] + argv
    completed = subprocess.run(
        argv, cwd=workspace, env={"LC_ALL": "C", "LANG": "C", "PATH": ""},
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=5, preexec_fn=limits, close_fds=True,
    )
    assert len(completed.stdout) + len(completed.stderr) < 8192
    return {
        "returncode": completed.returncode,
        "signal": -completed.returncode if completed.returncode < 0 else None,
        "stdout": completed.stdout.decode("utf-8", "replace"),
        "stderr": completed.stderr.decode("utf-8", "replace"),
    }


def main():
    assert os.uname().sysname == "Darwin"
    source = Path("/bin/echo")
    original = source.read_bytes()
    with tempfile.TemporaryDirectory(prefix="smith-review-platform-binary-") as tmp:
        base = Path(tmp).resolve()
        workspace = base / "workspace"
        workspace.mkdir(mode=0o700)
        copied = base / "echo-snapshot"
        descriptor = os.open(copied, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o500)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(original)
            stream.flush()
            os.fsync(stream.fileno())
        assert copied.read_bytes() == original
        cases = {}
        for label, executable in (("original", source), ("snapshot", copied)):
            for sandboxed in (False, True):
                cases[label + ("_seatbelt" if sandboxed else "_direct")] = execute(
                    executable, workspace, sandboxed
                )
        codesign = subprocess.run(
            ["/usr/bin/codesign", "--verify", "--strict", str(copied)],
            capture_output=True, timeout=5,
        )
        assert len(codesign.stderr) < 8192
        print(json.dumps({
            "source_sha256": hashlib.sha256(original).hexdigest(),
            "snapshot_byte_identical": True,
            "snapshot_codesign_verify_returncode": codesign.returncode,
            "cases": cases, "synthetic_only": True,
        }, indent=2))


if __name__ == "__main__":
    main()
