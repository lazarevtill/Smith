#!/usr/bin/env python3
"""Isolated pure/native-task bridge tests; optional exact owned backend journey.

Uses cached Rust dependencies, a separate target and disposable synthetic data.
Never starts the shared Slint compiler or discovers an existing backend/model.
"""
from pathlib import Path
import argparse
import hashlib
import json
import os
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
NATIVE = ROOT / "desktop/slint"
EXPECTED = "0b953b04b63940aef24e03ce89ed369eabc629ac5c7277780e0858b5205366f5"

def run(*args, env=None):
    subprocess.run(args, check=True, timeout=180, env=env, cwd=ROOT)

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--backend", type=Path)
    parser.add_argument("--async-backend", type=Path)
    parser.add_argument("--async-sha256")
    args=parser.parse_args()
    if args.backend:
        args.backend=args.backend.resolve(strict=True)
        assert hashlib.sha256(args.backend.read_bytes()).hexdigest()==EXPECTED, "unreviewed backend identity"
    if args.async_backend:
        args.async_backend=args.async_backend.resolve(strict=True)
        assert args.async_sha256 and len(args.async_sha256)==64 and all(c in "0123456789abcdef" for c in args.async_sha256), "explicit reviewed async backend SHA256 required"
        assert hashlib.sha256(args.async_backend.read_bytes()).hexdigest()==args.async_sha256, "unreviewed async backend identity"
    with tempfile.TemporaryDirectory(prefix="smith-task-bridge-review-") as name:
        base=Path(name).resolve();(base/"src").mkdir()
        manifest=base/"Cargo.toml"
        text='[package]\nname="smith-native-bridge"\nversion="0.0.0"\nedition="2021"\n[dependencies]\nserde={version="1",features=["derive"]}\nserde_json="1"\nlibc="0.2"\n[workspace]\n'
        for target in ["task_bridge", "task_bridge_owned", "task_model_async_owned"]:
            text+='[[test]]\nname='+json.dumps(target)+'\npath='+json.dumps(str(NATIVE/"tests"/(target+".rs")))+'\n'
        manifest.write_text(text)
        (base/"src/lib.rs").write_text("\n".join('#[path='+json.dumps(str(NATIVE/"src"/(module+".rs")))+']\npub mod '+module+';' for module in ["bridge","settings","http","owned_backend","documents","mlx","task_bridge"]))
        run("cargo","generate-lockfile","--manifest-path",str(manifest),"--offline")
        run("cargo","test","--manifest-path",str(manifest),"--locked","--offline","--test","task_bridge")
        run("cargo","clippy","--manifest-path",str(manifest),"--locked","--offline","--all-targets","--","-D","warnings")
        if args.backend:
            env=dict(os.environ, SMITH_NATIVE_TEST_BACKEND=str(args.backend))
            run("cargo","test","--manifest-path",str(manifest),"--locked","--offline","--test","task_bridge_owned","--","--ignored","--test-threads=1",env=env)
        if args.async_backend:
            env=dict(os.environ, SMITH_NATIVE_TEST_BACKEND=str(args.async_backend))
            run("cargo","test","--manifest-path",str(manifest),"--locked","--offline","--test","task_model_async_owned","--","--ignored","--test-threads=1",env=env)

if __name__=="__main__": main()
