# Native compile gates; hardware tests follow separately

The public source tree contains the active build-only workflow [`.github/workflows/native-build.yml`](../.github/workflows/native-build.yml), triggered on push or manual dispatch. It uses one checkout of this repository and the self-contained reviewed UI snapshot under `vendor/smith-ui`. No separate source repository, original Git history, private token or model package is needed. The original GitLab upstream and anyLC are not CI inputs or publication destinations. Configuration validation is complete; **Windows/Linux build success must be established by the actual job results, not this document.**

The [first hosted run](https://github.com/lazarevtill/Smith/actions/runs/38823025776), at source commit `98b28d4eb02d60776d22bdf22d2b113985a64012`, passed the Linux backend and both Linux/Windows native UI builds. The Windows backend failed because a globally compiled transcription function signature referenced Unix-only `libc::rlim_t`. The follow-up keeps that implementation under `cfg(unix)` and explicitly refuses non-Unix execution before spawning; it does not add Windows transcription support. The corrected source subsequently passed all four Linux/Windows lanes at `6db20f7`. No runtime or hardware test ran in these jobs.

The expanded [eight-lane run](https://github.com/lazarevtill/Smith/actions/runs/38894937123) passed on `e9f0e4246c5944b154ff15529e2636bc2db036b3`, tagged `v0.1.0-alpha.1`. These are compilation results, not packaged runtime or GPU qualification.

## Build inputs

| Lane | Compiler | Normal build | Hosted runner |
| --- | --- | --- | --- |
| Backend Linux x64 | Rust 1.89.0 | `smith-next`, existing features and SQLCipher | `ubuntu-24.04` |
| Backend Windows x64 | Rust 1.89.0 | `smith-next.exe`, existing features and SQLCipher | `windows-2022` |
| Native UI Linux x64 | Rust 1.92.0 | `smith-native-desktop`, `native-ui` | `ubuntu-24.04` |
| Native UI Windows x64 | Rust 1.92.0 | `smith-native-desktop.exe`, `native-ui` | `windows-2022` |

The backend compiler is the explicit project build baseline, not a new MSRV claim. The UI manifest declares Rust 1.92 and pins Slint 1.17.1; that Slint release also declares [Rust 1.92](https://github.com/slint-ui/slint/blob/v1.17.1/Cargo.toml). The normal UI features retain accessibility, system tray, winit and software rendering; the build does not replace them with fixtures or a bridge-only target. Slint's pinned [winit feature includes X11 and Wayland](https://github.com/slint-ui/slint/blob/v1.17.1/api/rs/slint/Cargo.toml). The explicit [hosted runner labels](https://docs.github.com/en/actions/reference/runners/github-hosted-runners) avoid `latest`, but runner images/system package versions still change and must be recorded from each job.

Both Cargo lockfiles are required and builds use `--locked --release` with one compiler job. The helper checks the exact compiler version and native host target, refuses compiler/cfg override environment variables, records the lockfile digest, and hashes the resulting executable. It never launches that executable. Component target directories and exclusive build reports/logs live under ignored `target/ci/`.

Successful job stdout retains the compiler/Cargo versions, source commit and dirty-state flag, executable/manifest/lockfile hashes, OS system/release/version/architecture, and the explicit `ImageOS`, `ImageVersion`, `RUNNER_OS`, `RUNNER_ARCH` allowlist. Missing runner fields remain null. No arbitrary environment or secret values are collected. Exact apt/MSVC/Windows SDK/OpenSSL dependency versions are **not captured** by this initial helper: runner-image metadata and Cargo locks alone do not reproduce all native dependencies. Preserve package-install/compiler-link diagnostics in the job logs; add a separately reviewed package manifest before claiming a reproducible distributable.

The matrix additionally builds both components on `macos-15` (arm64) and
`macos-15-intel` (x86_64), with the same component compiler versions.

## Local use on the target OS

`ci/build_native.py` requires Python 3, Git, rustup with the named compiler already installed, and the platform's native C/C++ linker/development libraries. The helper itself installs nothing. These commands are compile operations; use them only in the assigned build lane:

```sh
python ci/build_native.py --component backend --platform linux
python ci/build_native.py --component native --platform linux --ui-dir vendor/smith-ui
```

On Windows use `--platform windows`; on Mac use `--platform macos-arm64` or
`--platform macos-intel`. Native builds require `--ui-dir vendor/smith-ui`. `--ui-dir` may explicitly select another complete manifested snapshot; the verifier still requires the reviewed source identity. `--offline` requires a populated dependency cache. `--dry-run` prints the exact command and performs no subprocess or writes, and is safe for inspecting any lane from another OS.

Linux dependencies in the workflow include the native compiler, pkg-config, OpenSSL development files, fontconfig/FreeType, X11, xkbcommon and Wayland development files. Windows requires the MSVC/Windows SDK toolchain and x64 MSVC OpenSSL headers/import library. The workflow binds the runner's existing `Program Files/OpenSSL/include` and `lib/VC/x64/MD/libcrypto.lib` explicitly and fails if that layout is absent. It does not silently switch crypto implementations. The [pinned rusqlite 0.37 build script](https://github.com/rusqlite/rusqlite/blob/v0.37.0/libsqlite3-sys/build.rs) compiles bundled SQLCipher and requires system crypto, with `OPENSSL_LIB_DIR` / `OPENSSL_INCLUDE_DIR` controlling Windows linkage. The [runner OpenSSL installer](https://github.com/actions/runner-images/blob/main/images/windows/scripts/build/Install-OpenSSL.ps1) documents its installation root. DLL redistribution and installer packaging remain separate work.

## Reviewed original UI source

The native build uses selected UI markup/assets from exact original commit `adfa145a7bd706f744d17c67bf017c24c1bef57f`, included as a snapshot rather than repository history. `vendor/smith-ui/SOURCE.json` records the original commit, each relative path, byte length and SHA256, plus provenance/license notes. The strict verifier checks every file, rejects missing/extra files, traversal, unknown fields, duplicate keys/paths, links/reparse points and declared-size/hash changes, and bounds manifest/file counts and bytes. This verifies consistency of the reviewed snapshot recorded in this source commit; it does not invent an application-wide license grant. See the root license-status document.

Run `python ci/verify_ui_snapshot.py` to check the snapshot without Git, a compiler or network access. The build helper requires that explicit directory and records the manifest hash/count/bytes in successful CI stdout. `desktop/slint/build.rs` also defaults to the snapshot, preserving an explicit `SMITH_ORIGINAL_UI_DIR` override for local development. CI always selects and verifies the snapshot; the direct Cargo override remains the caller's responsibility. Generated overlays are written only into Cargo build output, leaving the snapshot unchanged.

The workflow runs on push/manual dispatch, with only repository-content read permission and no persisted checkout credentials. It performs snapshot verification and native compilation. There are no artifact or cache uploads, release steps, model downloads, desktop launches, hardware queries or application tests. Each component/platform is a separate failing gate with `fail-fast: false`; there is no `continue-on-error`. Build failures must be fixed in actual platform code rather than concealed with mock feature flags. Successful build identity remains in job stdout; binaries/full local logs disappear with the ephemeral runner unless a separately reviewed retention/publishing step is added.

## Source-grounded platform gaps

These are implementation boundaries found by reading current source, not cross-platform execution results:

| Surface | Linux | Windows |
| --- | --- | --- |
| Native owned backend | Unix listener/process custody implementation exists; Linux lifecycle still needs tests | `desktop/slint/src/owned_backend.rs` and `src/desktop.rs` explicitly refuse unsupported non-Unix custody |
| Default native paths/package | `desktop/slint/src/main.rs` still assumes macOS bundle/Application Support layout; platform defaults and packaging required | Same layout gap, plus owned-process adapter above |
| Encrypted automatic profile publication | Exclusive `renameat2(RENAME_NOREPLACE)` path exists; durability/journey tests pending | `smith-profile-storage/src/files.rs::rename_exclusive` explicitly refuses; Windows ACL/key custody and publication need implementation/qualification |
| Selected GGUF supervision | Same-stream UDS peer PID/UID ownership code exists; runtime/build/device qualification pending | Typed `UnsupportedPlatform`; no TCP ownership fallback |
| Native MLX | Native worker preparation refused outside macOS | Native worker preparation refused outside macOS |
| Saved model capture | Unix descriptor-based capture exists; test on Linux needed | `smith-local-catalog/src/files.rs` explicitly refuses capture; passive catalog decode remains separate |
| Local file/tool effects | Unix workspace code exists; sandboxed process effects remain macOS-only | Workspace/effects adapters explicitly unsupported |
| Native file chooser/save dialogs | `desktop/slint/src/picker.rs` reports unavailable | Same explicit unavailability |
| GPU inventory | Fixed proc/sysfs observation implementation; native execution unqualified | Win32/DXGI observation implementation; native compile/runtime unqualified |

The Windows transcription signature failure above is an observed compiler result. Other source gates should not be mistaken for build failures: for example, the media crate's disk `MediaStore` is gated to macOS/Linux, but the current application media API does not import it. That gate alone does not establish a compile failure. Source-level unsupported returns can compile successfully while still preventing a usable product journey.

Passing these four builds would establish that the normal backend and UI compile on the specified targets. It would not establish application startup, secure storage, packaged launch, accessibility, task execution, CUDA/HIP/ROCm/SYCL/Intel Arc/Vulkan/Metal/MLX support, model placement, latency, memory fit or optimized defaults. Those require later owned synthetic lifecycle tests and explicit real-device/model qualification; CPU-vendor/GPU identifiers alone are insufficient.
