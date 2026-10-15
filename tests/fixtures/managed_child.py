#!/usr/bin/env python3
"""Private-pipe child fixture. Synthetic text only; no sockets, models or inference."""
import json
import os
import select
import sys
import threading
import time

lock = threading.Lock()
stop_run = threading.Event()
worker = None
ignore_cancel = False


def emit(value):
    with lock:
        sys.stdout.write(json.dumps(value) + "\n")
        sys.stdout.flush()


line = sys.stdin.readline()
if not line:
    sys.exit(0)
config = json.loads(line)
startup = config["startup"]
identity = {key: config[key] for key in ("nonce", "generation", "model")}

if startup == "exit_early":
    sys.exit(23)
if startup == "malformed":
    print("not-json", flush=True)
elif startup == "oversized":
    print("x" * (32 * 1024), flush=True)
elif startup == "wrong_nonce":
    emit({"event": "ready", **identity, "nonce": "wrong", "protocol": 1})
elif startup == "wrong_model":
    emit({"event": "ready", **identity, "model": "wrong", "protocol": 1})
elif startup == "wrong_version":
    emit({"event": "ready", **identity, "protocol": 99})
else:
    emit({"event": "loading", **identity, "protocol": 1})
    if startup == "flood_stderr":
        for _ in range(128):
            sys.stderr.write("synthetic-secret-must-not-be-retained " * 256)
        sys.stderr.flush()
    if startup != "stall_ready":
        if startup == "delayed":
            time.sleep(0.2)
        emit({"event": "ready", **identity, "protocol": 1})


def generate(run_id, mode):
    try:
        count = 10000 if mode == "flood" else 3
        for index in range(count):
            if stop_run.is_set() and mode != "ignore_cancel":
                emit({"event": "cancelled", "run_id": run_id})
                return
            emit({"event": "token", "run_id": run_id, "text": "fixture-%d " % index})
            if mode in ("stall", "ignore_cancel"):
                while True:
                    time.sleep(0.005)
                    if stop_run.is_set() and mode != "ignore_cancel":
                        emit({"event": "cancelled", "run_id": run_id})
                        return
            time.sleep(0.005)
        emit({"event": "done", "run_id": run_id})
    except (BrokenPipeError, OSError):
        os._exit(0)


if startup == "exit_after_ready":
    time.sleep(0.15)
    sys.exit(24)

for line in sys.stdin:
    command = json.loads(line)
    if command["op"] == "shutdown":
        if startup == "ignore_shutdown":
            continue
        os._exit(0)
    if command["op"] == "cancel":
        if not ignore_cancel:
            stop_run.set()
    if command["op"] == "run":
        if worker is not None:
            worker.join(timeout=0.2)
        stop_run.clear()
        ignore_cancel = command["mode"] == "ignore_cancel"
        worker = threading.Thread(target=generate, args=(command["run_id"], command["mode"]), daemon=True)
        worker.start()

# Parent process exit closes this private input pipe, including on SIGKILL.
# No descendants are created by this fixture.
os._exit(0)
