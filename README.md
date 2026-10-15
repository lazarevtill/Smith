# Smith

Smith is a local native desktop AI workspace built with Rust and Slint. This
repository contains the ongoing refactor: it preserves the original interface
and assets while moving behavior into explicit domain, storage, runtime and
platform boundaries.

Ordinary startup creates or reopens an encrypted local workspace without an
account, sign-in page or password screen. Selected local models run through
owned native workers. Remote provider connections are optional and separate
from local access.

**Development status:** macOS is the currently exercised desktop platform.
Windows and Linux have build CI; several platform adapters still require
implementation and runtime testing. This is not a complete replacement for
every original Smith feature or a qualified cross-platform release.

## Current work

- Original native layout, fonts and icons, with focused usability fixes.
- Local conversations, branches, explicit export and recoverable removal.
- Encrypted storage, copy-based legacy import and preserved original records.
- Saved local model catalog with file revalidation and restart-safe operations.
- Reviewed native MLX setup, explicit thinking controls for supported models,
  immutable inference provenance, cancellation and owned-process cleanup.
- Native GGUF runtime integration and bounded local agent/tool workflows.

Imported historical image/tool-card projection is implemented in the backend;
native card adoption is still in progress. Broader voice/media, model
provisioning, integrations, memory learning and remaining platform workflows
have separate work outstanding. Hardware inventory and a successful build do
not prove GPU execution or optimized performance.

## Build

The backend uses Rust 1.89.0 for its current build baseline. The native UI uses
Rust 1.92.0 and Slint 1.17.1. Both dependency lockfiles are committed.

```sh
rustup toolchain install 1.89.0 --profile minimal
rustup toolchain install 1.92.0 --profile minimal
cargo +1.89.0 build --locked --bin smith-next
cargo +1.92.0 build --locked --manifest-path desktop/slint/Cargo.toml \
  --features native-ui --bin smith-native-desktop
```

Native C/C++ development tools and SQLCipher/OpenSSL dependencies are required.
The original UI build input is included under `vendor/smith-ui`; CI verifies its
exact file manifest. No private source checkout or additional GitHub token is
needed. See [CI and platform status](docs/CI-NATIVE-BUILDS.md) for target-specific
dependencies and current runtime limitations.

On macOS, an explicit development launch selects the backend executable and a
new local profile directory:

```sh
desktop/slint/target/debug/smith-native-desktop \
  --backend "$PWD/target/debug/smith-next" \
  --profile /absolute/path/to/new-smith-profile
```

This creates persistent local data in the selected directory. Keep existing
workspaces and their keys backed up before experimenting with development
builds. This repository contains no user profile, model weight or prebuilt
runtime. Local model execution requires explicitly selected, compatible files;
there is no automatic download or startup model load.

## Architecture and checks

See [architecture](docs/ARCHITECTURE.md), the
[MLX protocol](desktop/mlx/PROTOCOL.md), and
[publication scope](PUBLICATION-SCOPE.json).

CI compiles the normal backend and native UI independently on Windows and Linux.
Hardware and packaged desktop tests follow separately. Synthetic tests are
included; tests that require an owned desktop, native worker or real model are
explicitly gated. Passing a synthetic fixture is not a language-quality or
hardware qualification result.

## Licensing

An application-wide license has not been selected. Public availability alone
does not grant a new license. Existing dependency, font and asset notices remain
applicable. See [license status](LICENSE-STATUS.md) and
[third-party notices](THIRD-PARTY-NOTICES.md).
