#!/usr/bin/env python3
"""Qualify the packaged thinking protocol using synthetic metadata; never load a model."""
import contextlib
import hashlib
import json
import os
import pathlib
import select
import struct
import subprocess
import sys
import tempfile
import time


binary = pathlib.Path(sys.argv[1]).resolve(strict=True)
fixture = pathlib.Path(__file__).resolve().parents[1] / "Tests/WorkerCoreTests/Fixtures/qwen3-template.jinja"
template = fixture.read_bytes()
assert hashlib.sha256(template).hexdigest() == "87a2728cb8dc9fe424d624542f6060ec05a1d285ebbec578bb078900e33396b5"
checks = []


def event(child):
    line = bytearray()
    deadline = time.monotonic() + 20
    while not line.endswith(b"\n"):
        assert time.monotonic() < deadline, "native child response timeout"
        ready, _, _ = select.select([child.stdout], [], [], 0.1)
        if ready:
            chunk = os.read(child.stdout.fileno(), 1)
            assert chunk, "unexpected child EOF"
            line.extend(chunk)
            assert len(line) <= 131073, "response exceeded protocol limit"
    return json.loads(line)


def request(child, request_id, operation, **fields):
    payload = json.dumps({"v": 1, "id": request_id, "op": operation, **fields}).encode() + b"\n"
    assert len(payload) <= 131073
    child.stdin.write(payload)
    child.stdin.flush()
    while True:
        result = event(child)
        assert result["id"] in (request_id, None), result
        if result["event"] in ("done", "error"):
            return result
        assert result["event"] == "started", result


with tempfile.TemporaryDirectory(prefix="smith-mlx-thinking-protocol-") as temporary:
    base = pathlib.Path(temporary).resolve()
    scratch = base / "scratch"
    scratch.mkdir(mode=0o700)

    @contextlib.contextmanager
    def process():
        # Stderr goes to an owned file to avoid a pipe deadlock; output remains capped below.
        with tempfile.TemporaryFile(dir=base) as stderr:
            child = subprocess.Popen(
                [str(binary), "--scratch-root", str(scratch)],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                env={"LANG": "C", "PATH": "/usr/bin:/bin", "TMPDIR": str(base)}, bufsize=0,
            )
            try:
                yield child
            finally:
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=10)
                child.stdin.close()
                child.stdout.close()
                assert stderr.tell() <= 262144, "stderr exceeded qualification limit"
                assert list(scratch.iterdir()) == [], "worker left a snapshot behind"

    def shutdown(child):
        assert request(child, "shutdown", "shutdown")["event"] == "done"
        child.stdin.close()
        assert child.wait(timeout=10) == 0

    generation = {"mode": "chat", "messages": [{"role": "user", "content": "/no_think"}],
                  "max_tokens": 8, "temperature": 0, "top_p": 1}
    with process() as child:
        hello = request(child, "hello", "hello")
        assert hello["capabilities"] == ["inspect", "local_text", "qwen3_thinking_control_v1"], hello
        checks.append("finite-thinking-feature-on-actual-native-hello")
        for index, option in enumerate(({}, {"thinking_mode": "model_default"}, {"thinking_mode": "disabled"})):
            result = request(child, f"unloaded-{index}", "generate", **generation, **option)
            assert result["id"] == f"unloaded-{index}" and result["code"] == "no_model", result
        checks.append("valid-default-and-disabled-chat-reach-no-model-refusal-without-fallback")
        shutdown(child)

    invalid = [("unknown", "automatic"), ("null", None), ("boolean", False), ("number", 0)]
    for label, mode in invalid:
        with process() as child:
            result = request(child, "invalid", "generate", **generation, thinking_mode=mode)
            assert result["id"] is None and result["code"] == "invalid_request", result
            assert child.wait(timeout=10) == 0
            checks.append(f"reject-{label}-thinking-mode-before-admission")
    for mode in ("model_default", "disabled"):
        with process() as child:
            result = request(child, "raw-option", "generate", mode="raw", prompt="literal",
                             max_tokens=8, temperature=0, top_p=1, thinking_mode=mode)
            assert result["id"] is None and result["code"] == "unsupported_thinking_mode", result
            assert child.wait(timeout=10) == 0
            checks.append(f"reject-raw-{mode}-before-admission")

    model = base / "synthetic-metadata"
    model.mkdir()
    config = {"model_type": "qwen3", "hidden_size": 64, "num_attention_heads": 4,
              "num_hidden_layers": 1, "intermediate_size": 128, "vocab_size": 256,
              "max_position_embeddings": 8192}
    for name, value in [("config.json", config),
                        ("tokenizer_config.json", {"tokenizer_class": "PreTrainedTokenizerFast"}),
                        ("tokenizer.json", {"model": {"type": "BPE", "vocab": {"a": 0}, "merges": []}})]:
        (model / name).write_text(json.dumps(value))
    header = json.dumps({"synthetic.weight": {"dtype": "F32", "shape": [1], "data_offsets": [0, 4]}}, separators=(",", ":")).encode()
    (model / "model.safetensors").write_bytes(struct.pack("<Q", len(header)) + header + b"\0" * 4)
    (model / "chat_template.jinja").write_bytes(template)
    with process() as child:
        exact = request(child, "exact-template", "inspect", model_root=str(model))
        assert exact["manifest"]["template"] == "qwen3-default-thinking-v1", exact
        assert exact["capabilities"] == ["raw_completion", "chat"], exact
        checks.append("inspect-exact-pinned-template-as-chat-without-gpu-loader")
        (model / "chat_template.jinja").write_bytes(template + b" ")
        changed = request(child, "changed-template", "inspect", model_root=str(model))
        assert changed["manifest"].get("template") is None, changed
        assert changed["capabilities"] == ["raw_completion"], changed
        checks.append("one-byte-template-change-remains-raw-only")
        shutdown(child)

print(json.dumps({"schema": "smith.mlx-thinking-native-process.v1",
                  "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                  "native_process": True, "gpu_inference": False, "model_load_requested": False,
                  "downloads": False, "owned_children_reaped": True, "scratch_cleanup_confirmed": True,
                  "passed": len(checks), "checks": checks}, indent=2))
