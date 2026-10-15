#!/usr/bin/env python3
"""Compile/run synthetic inventory checks in a disposable standalone harness.

No OS inventory entrypoint is called. The optional-OS FFI bodies receive a
host-only type check, not a Windows link/run qualification. No downloads.
"""
from pathlib import Path
import json
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(*args):
    subprocess.run(args, check=True, timeout=180, cwd=ROOT)


def main():
    with tempfile.TemporaryDirectory(prefix="smith-platform-inventory-review-") as name:
        base = Path(name).resolve()
        (base / "src").mkdir()
        manifest = base / "Cargo.toml"
        manifest.write_text(
            '[package]\nname="smith-platform-inventory-review"\nversion="0.1.0"\nedition="2021"\n'
            '[dependencies]\nserde={version="1",features=["derive"]}\nserde_json="1"\nsha2="0.11"\n'
            'uuid={version="1",features=["v4"]}\nlibc="0.2"\n'
            'smith-acceleration={path=' + json.dumps(str(ROOT / "crates/smith-acceleration")) + '}\n[workspace]\n'
        )
        common = '#![allow(dead_code)]\n#[path=' + json.dumps(str(ROOT / "src/hardware_inventory.rs")) + ']\nmod hardware_inventory;\n'
        normal = common + '#[path=' + json.dumps(str(ROOT / "src/hardware_inventory_platform.rs")) + ']\nmod hardware_inventory_platform;\n'
        (base / "src/lib.rs").write_text(normal)
        run("cargo", "generate-lockfile", "--manifest-path", str(manifest), "--offline")
        run("cargo", "test", "--manifest-path", str(manifest), "--locked", "--offline", "hardware_inventory_platform")
        run("cargo", "clippy", "--manifest-path", str(manifest), "--locked", "--offline", "--all-targets", "--", "-D", "warnings")

        # Enable the Windows FFI implementation in a disposable COPY for Rust
        # type checking only. cargo check does not link kernel32/ntdll or run it.
        # Do not describe this as a target-Windows build or ABI qualification.
        source = (ROOT / "src/hardware_inventory_platform.rs").read_text()
        marker = '#[cfg(target_os = "windows")]'
        assert source.count(marker) == 3, "review harness needs source cfg audit"
        copied = source.replace(marker, '#[cfg(any(target_os = "windows", test))]')
        (base / "src/windows_typecheck.rs").write_text(copied)
        (base / "src/lib.rs").write_text(common + 'mod windows_typecheck;\n')
        run("cargo", "check", "--manifest-path", str(manifest), "--locked", "--offline", "--tests")
        run("cargo", "clippy", "--manifest-path", str(manifest), "--locked", "--offline", "--tests", "--", "-D", "warnings")
        print(json.dumps({"synthetic_tests": "passed", "host_clippy": "passed", "windows_ffi_host_typecheck": "passed",
                          "linux_native_compile_and_run": "not_qualified", "windows_native_compile_link_and_run": "not_qualified"}))


if __name__ == "__main__":
    main()
