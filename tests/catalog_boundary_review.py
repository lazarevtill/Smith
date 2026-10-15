#!/usr/bin/env python3
"""Independent actual-binary catalog checks; disposable storage, no model/provider."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import uuid
from unittest.mock import patch

from desktop_boundary_review import launch, stop

MODE = "--smith-catalog-echo"


def request(listener, owner, path, *, session=None, data=None, method="GET"):
    port = listener.getsockname()[1]
    headers = {"Host": f"127.0.0.1:{port}", "X-Smith-Desktop-Owner": owner}
    if session is not None:
        headers["X-Smith-Profile-Session"] = session
    if data is not None:
        headers.update({"Content-Type": "application/json", "Origin": f"http://127.0.0.1:{port}"})
    # This boundary hashes/snapshots a 56 MiB debug binary, unlike the small
    # read-only requests in desktop_boundary_review's three-second helper.
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    try:
        connection.request(method, path, body=json.dumps(data) if data is not None else None,
                           headers=headers)
        response = connection.getresponse()
        payload = response.read(1024 * 1024 + 1)
        assert len(payload) <= 1024 * 1024
        return response.status, payload
    finally:
        connection.close()


def direct(binary, root):
    env = {"PATH": "/usr/bin:/bin", "HOME": str(root / "unused-home"),
           "SMITH_NEXT_DATA_DIR": str(root / "must-not-exist"),
           "SMITH_NEXT_DESKTOP_OWNER": "invalid-owner-must-never-be-read",
           "SMITH_NEXT_DESKTOP_LISTENER_FD": "invalid-fd"}

    def run(args, code, expected=None):
        result = subprocess.run([str(binary), *args], env=env, cwd=root,
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
        assert result.returncode == code, (args[:2], result.returncode, result.stderr)
        assert len(result.stdout) <= 8192 and len(result.stderr) < 1024
        if expected is not None:
            assert result.stdout == expected
        assert not (root / "must-not-exist").exists()
        assert not (root / "unused-home").exists()
        return result

    payload = ["--profile", "should-not-exist", "--smith-catalog-echo", "-n",
               "$(touch should-not-exist)", "é", ""]
    run([MODE, *payload], 0, (" ".join(payload) + "\n").encode())
    assert not (root / "should-not-exist").exists()
    run([MODE], 0, b"\n")
    run([MODE, "x" * 8191], 0, b"x" * 8191 + b"\n")
    run([MODE, "x" * 8192], 64, b"")
    run([MODE, "é" * 4096], 64, b"")
    run([MODE, *([""] * 64)], 0, b" " * 63 + b"\n")
    run([MODE, *([""] * 65)], 64, b"")
    for malformed in ("bad\nline", "bad\x7fvalue", "bad\u0085value", b"bad\xff"):
        run([MODE, "safe", malformed], 64, b"")
    # Near matches or a flag in a later position cannot enter the special mode;
    # invalid desktop ownership then refuses normal startup before any profile.
    run([MODE + "-other"], 1, b"")
    run(["--ordinary-argument", MODE, "must-not-echo"], 1, b"")
    snapshot = root / "snapshot"
    shutil.copyfile(binary, snapshot)
    snapshot.chmod(0o500)
    original_binary = binary
    binary = snapshot
    run([MODE, "copied-native-entrypoint"], 0, b"copied-native-entrypoint\n")
    assert hashlib.sha256(snapshot.read_bytes()).digest() == hashlib.sha256(original_binary.read_bytes()).digest()


def app_boundary(binary, root):
    temp = root / "owned-temp"
    temp.mkdir(mode=0o700)
    wrong = root / "not-an-executable"
    wrong.write_text("test-only override must be absent from normal builds")
    minimal_env = {"PATH": "/usr/bin:/bin", "HOME": str(root / "home"),
                   "TMPDIR": str(temp), "SMITH_TEST_CATALOG_BINARY": str(wrong)}
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(16)
        owner = secrets.token_hex(32)
        with patch.dict(os.environ, minimal_env, clear=True):
            child = launch(binary, root / "workspace", listener, owner)
        try:
            code, body = request(listener, owner, "/api/profile/status")
            assert code == 200
            session = json.loads(body)["session"]

            def call(path, data=None, expected=200):
                code, body = request(listener, owner, path, session=session, data=data,
                                     method="GET" if data is None else "POST")
                assert code == expected, (path, code, body[:1000])
                return json.loads(body)

            catalog = call("/api/agent/effects/catalog")
            assert Path(catalog["executables"][0]["path"]).resolve() == binary
            assert catalog["executables"][0]["argv_prefix"] == [MODE]
            workspace = call("/api/agent/workspaces", {
                "label": "Independent catalog boundary", "capabilities": ["shell"],
                "resources": [], "confirmed": True})["workspace"]["id"]
            bind = {"workspace_id": workspace, "executables": ["echo"],
                    "git": False, "confirmed": True}
            code, _ = request(listener, owner, "/api/agent/effects/bind-workspace",
                              session=session, data={**bind, "argv_prefix": []}, method="POST")
            assert 400 <= code < 500
            bound = call("/api/agent/effects/bind-workspace", bind)
            binding = bound["binding"]
            assert binding["policy"]["executables"][0]["argv_prefix"] == [MODE]
            grant = call(f"/api/agent/workspaces/{workspace}/grants", {
                "expected_policy_revision": 1, "capabilities": ["shell"],
                "resources": [bound["resource"]], "confirmed": True})
            task = call("/api/agent/tasks", {"workspace_id": workspace,
                "policy_revision": grant["workspace"]["policy_revision"],
                "input": "Synthetic fixed-prefix boundary"})["task"]
            tid = task["id"]
            task = call(f"/api/agent/tasks/{tid}/start", {"expected_revision": 0})["task"]
            base = {"binding_id": binding["id"], "revision": binding["revision"],
                    "tool": "run_shell"}
            code, _ = request(listener, owner, "/api/agent/effects/prepare", session=session,
                              data={**base, "arguments": {"command": "echo safe", "argv_prefix": []}}, method="POST")
            assert 400 <= code < 500
            command = "echo --profile synthetic-escape --smith-catalog-echo -n"
            prepared = call("/api/agent/effects/prepare", {**base, "arguments": {"command": command}})
            assert prepared["plan"]["policy"]["executables"][0]["argv_prefix"] == [MODE]
            task = call(f"/api/agent/tasks/{tid}/actions", {"expected_revision": task["revision"],
                "request_id": str(uuid.uuid4()), "action": {"kind": "tool", "name": "run_shell",
                "arguments": prepared["call"]["arguments"]}})["task"]
            assert task["state"]["kind"] == "needs_approval" and task["last_result"] is None
            approval = {"task_id": tid, "expected_revision": task["revision"],
                "call_id": task["state"]["call"]["id"], "plan_id": prepared["plan"]["id"],
                "confirmed": True}
            result = call("/api/agent/effects/approve", approval)["task"]["last_result"]
            assert result["exit_code"] == 0 and result["signal"] is None, result
            assert result["stdout"] == "--profile synthetic-escape --smith-catalog-echo -n\n"
            assert result["stderr"] == "" and result["reaped"] is True
            assert result["sandbox"] == "macos-seatbelt"
            try:
                os.kill(result["pid"], 0)
            except ProcessLookupError:
                pass
            else:
                raise AssertionError("Reported catalog child is still alive")
            call("/api/agent/effects/approve", approval, expected=409)
            assert not list(temp.rglob("synthetic-escape"))
        finally:
            stop(child)


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert digest == sys.argv[2], "Expected immutable backend SHA-256"
    with tempfile.TemporaryDirectory(prefix="smith-catalog-review-") as temporary:
        root = Path(temporary).resolve()
        direct(binary, root)
        app_boundary(binary, root)
    print(json.dumps({"passed": True, "groups": 2, "binary_sha256": digest,
                      "synthetic_only": True, "model_calls": 0, "desktop_window_opened": False}))


if __name__ == "__main__":
    main()
