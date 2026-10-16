# Building Smith

## Supported build hosts

| Host | Platform argument | Backend compiler | Desktop compiler |
| --- | --- | --- | --- |
| Linux x86_64 | `linux` | Rust 1.89.0 | Rust 1.92.0 |
| Windows x86_64 / MSVC | `windows` | Rust 1.89.0 | Rust 1.92.0 |
| macOS Apple silicon | `macos-arm64` | Rust 1.89.0 | Rust 1.92.0 |
| macOS Intel | `macos-intel` | Rust 1.89.0 | Rust 1.92.0 |

Use the matching OS and architecture. ARM Windows/Linux are not current CI lanes.
Install Git, Python 3, rustup and a native C/C++ toolchain first. Install both
Rust versions with `rustup toolchain install VERSION --profile minimal`.

- Linux: use the apt development-package list in the checked-in workflow.
- Windows: install Visual Studio C++ build tools/Windows SDK and compatible x64
  MSVC OpenSSL. Set `OPENSSL_INCLUDE_DIR` and `OPENSSL_LIB_DIR` to actual headers
  and import libraries. The CI binding is runner-specific, not a portable installer.
- macOS: install Xcode command-line tools, OpenSSL 3 and pkg-config. With Homebrew,
  use `brew install openssl@3 pkg-config` and set
  `OPENSSL_DIR="$(brew --prefix openssl@3)"` when building the backend.

## Verified release-mode commands

From the repository root, replace PLATFORM with one value from the table:

```sh
python ci/verify_ui_snapshot.py
python ci/build_native.py --component backend --platform PLATFORM
python ci/build_native.py --component native --platform PLATFORM --ui-dir vendor/smith-ui
```

The helper checks compiler/host identity and builds with committed lockfiles,
`--release` and one compiler job. Reports, logs and executables are written under
`target/ci/PLATFORM/COMPONENT/`; the executable is inside the target triple's
`release` directory. `--dry-run` prints the plan without building; `--offline`
requires dependencies already cached. Neither command downloads models or
launches the app. Compiler dependencies may be fetched by Cargo.

## Checks

```sh
python ci/test_build_native.py
python ci/check_platform_ui.py --toolchain 1.92.0 --ui-dir vendor/smith-ui
```

The workflow is the executable specification for hosted build prerequisites.
Both components passed on all four hosts for the first alpha tag. See
[CI evidence](CI-NATIVE-BUILDS.md) and [runtime limitations](PLATFORM-READINESS.md).

## Packaging

Compiling an executable does not produce a complete distributable. The existing
`desktop/slint/package-macos.sh --help` describes explicit, hash-qualified local
Mac packaging inputs, including dependency notices and optional MLX resources.
It packages selected existing binaries and refuses overwriting its output.
Windows and Linux distributables are not qualified yet. Model weights, profiles,
workspace keys and local configuration must never enter release assets.
