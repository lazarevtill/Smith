#!/usr/bin/env python3
"""Independent inherited-listener review; owned children and synthetic storage only.

No desktop window, installed model, real profile, keychain or external service is
used. This checks the actual supplied Rust backend, not a protocol replacement.
"""
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import selectors
import signal
import socket
import subprocess
import sys
import tempfile
import time


def clean_env(root, owner):
    env = dict(os.environ)
    for key in ("SMITH_NEXT_SYNTHETIC_PROFILE", "SMITH_NEXT_MANUAL_PROFILE"):
        env.pop(key, None)
    env.update(SMITH_NEXT_DATA_DIR=str(root), SMITH_NEXT_PORT="0",
               SMITH_NEXT_DESKTOP_OWNER=owner, SMITH_NEXT_DESKTOP_LISTENER_FD="0")
    return env


def stop(child):
    if child.poll() is None:
        child.send_signal(signal.SIGTERM)
        try:
            child.wait(15)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(5)
            raise AssertionError("Owned backend did not finish graceful shutdown")
    assert child.returncode == 0, "Owned backend did not report clean shutdown"


def launch(binary, root, listener, owner, *, on_spawn=None):
    child = subprocess.Popen([str(binary)], env=clean_env(root, owner),
                             stdin=listener, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE)
    deadline = time.monotonic() + 20
    pending = bytearray()
    try:
        # Retain caller custody before readiness or cleanup can fail.
        if on_spawn is not None:
            on_spawn(child)
        with selectors.DefaultSelector() as ready:
            ready.register(child.stdout, selectors.EVENT_READ)
            while time.monotonic() < deadline:
                if child.poll() is not None:
                    raise AssertionError("Owned backend exited before desktop readiness")
                if not ready.select(.05):
                    continue
                chunk = os.read(child.stdout.fileno(), 4096)
                pending.extend(chunk)
                assert len(pending) <= 65536, "Unbounded readiness output"
                while b"\n" in pending:
                    line, _, pending = pending.partition(b"\n")
                    value = json.loads(line)
                    assert value["smith_desktop"] == 1 and value["pid"] == child.pid
                    assert value["owner"] == owner
                    assert value["url"] == "http://127.0.0.1:" + str(listener.getsockname()[1])
                    return child
        raise AssertionError("Owned backend readiness deadline exceeded")
    except BaseException:
        if child.poll() is None:
            child.kill()
        child.wait(5)
        raise


def request(listener, owner, path, *, session=None, data=None, method="GET", host=None):
    port = listener.getsockname()[1]
    headers = {"Host": host or f"127.0.0.1:{port}"}
    if owner is not None:
        headers["X-Smith-Desktop-Owner"] = owner
    if session is not None:
        headers["X-Smith-Profile-Session"] = session
    body = None
    if data is not None:
        body = json.dumps(data)
        headers["Content-Type"] = "application/json"
        headers["Origin"] = f"http://127.0.0.1:{port}"
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        conn.request(method, path, body, headers)
        reply = conn.getresponse()
        payload = reply.read(1024 * 1024)
        return reply.status, payload
    finally:
        conn.close()


def refused(binary, root, listener, extra=None):
    owner = secrets.token_hex(32)
    env = clean_env(root, owner)
    env.update(extra or {})
    result = subprocess.run([str(binary)], env=env, stdin=listener,
                            capture_output=True, timeout=10)
    assert result.returncode != 0
    assert owner.encode() not in result.stdout + result.stderr
    assert not (root / "local-workspace").exists(), "Invalid desktop launch created a workspace"


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    checks = []
    with tempfile.TemporaryDirectory(prefix="smith-desktop-rust-review-") as temporary:
        root = Path(temporary).resolve()
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(16)
            owner = secrets.token_hex(32)
            profile = root / "valid"
            child = launch(binary, profile, listener, owner)
            try:
                assert request(listener, None, "/api/profile/status")[0] == 403
                assert request(listener, "wrong", "/api/profile/status")[0] == 403
                assert request(listener, owner, "/api/profile/status", host="localhost:" + str(listener.getsockname()[1]))[0] == 403
                status, payload = request(listener, owner, "/api/profile/status")
                state = json.loads(payload)
                assert status == 200 and state["automatic"] and state["state"] == "unlocked"
                session = state["session"]
                assert request(listener, owner, "/api/config")[0] == 409
                assert request(listener, owner, "/api/config", session=session)[0] == 200
                assert request(listener, owner, "/")[0] == 200
                draft = {"version": 0, "text": "Synthetic desktop restart draft", "attachments": [], "media": []}
                assert request(listener, owner, "/api/drafts/new", session=session, data=draft, method="PUT")[0] == 200
            finally:
                stop(child)
            with socket.socket() as competitor:
                competitor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                competitor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
                try:
                    competitor.bind(listener.getsockname())
                except OSError:
                    pass
                else:
                    raise AssertionError("Exact retained desktop address was taken over")
            owner = secrets.token_hex(32)
            child = launch(binary, profile, listener, owner)
            try:
                _, payload = request(listener, owner, "/api/profile/status")
                state = json.loads(payload)
                _, payload = request(listener, owner, "/api/drafts/new", session=state["session"])
                assert json.loads(payload)["text"] == draft["text"]
                assert request(listener, owner, "/api/config", session=session)[0] == 409
            finally:
                stop(child)
            local = profile / "local-workspace"
            assert (local / "managed.key").stat().st_mode & 0o777 == 0o600
            assert (local / "profile.sqlite3").read_bytes()[:16] != b"SQLite format 3\x00"
            checks.append("actual inherited listener, invisible ownership, automatic storage and restart")
            refused(binary, root / "plaintext", listener, {"SMITH_NEXT_SYNTHETIC_PROFILE": "plaintext"})
            checks.append("desktop plaintext fixture mode refused before workspace creation")
        for case in ("bound-only", "wildcard", "reuseport"):
            with socket.socket() as listener:
                if case == "reuseport":
                    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
                listener.bind(("0.0.0.0" if case == "wildcard" else "127.0.0.1", 0))
                if case != "bound-only":
                    listener.listen(4)
                refused(binary, root / case, listener)
        with open(os.devnull, "rb") as ordinary:
            refused(binary, root / "ordinary-stdin", ordinary)
        checks.append("nonlistener, wildcard, reusable and ordinary stdin rejected without workspace")
    print(json.dumps({"passed": True, "groups": len(checks), "checks": checks,
                      "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                      "synthetic_only": True, "desktop_window_opened": False}, indent=2))


if __name__ == "__main__":
    main()
