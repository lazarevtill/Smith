# Desktop platform audit — 2026-10-16

This audit distinguishes code coverage, compilation, synthetic tests, app lifecycle and actual GPU execution. The original UI is retained. The current development app is not a completed three-platform release.

## UI policy corrected locally

`desktop/slint/src/platform_ui.rs` supplies target-specific choices through `NativePlatform` and `NativeSettings` rather than a shared list of every backend. Stable persisted backend/provider IDs are resolved against the filtered choices. An unsupported saved ID produces an unselected field and a visible explanation; saving refuses until an explicit available choice is made. No saved configuration is reset merely by opening the UI.

| Target | GGUF candidate choices | MLX controls | File-picker buttons | Owned runtime |
|---|---|---|---|---|
| macOS Apple silicon | Auto, CPU, Metal | Available | Native macOS adapter | Implemented |
| macOS Intel | Auto, CPU, Metal | Hidden | Native macOS adapter | Implemented, execution unqualified |
| Linux | Auto, CPU, CUDA, HIP, SYCL, Vulkan | Hidden | Disabled; explicit path fields retained | Implemented, target execution unqualified |
| Windows | Platform policy knows CPU/CUDA/HIP/SYCL/Vulkan; setup panel is unavailable | Hidden | Disabled; explicit path fields retained | Not implemented |

These are candidates, not a claim that the selected GPU, driver, architecture, model or runtime supports them. Exact runtime inspection, device evidence and the existing planner still decide admission. MLX is a separate engine rather than an option in the GGUF backend selector. Unknown targets omit platform capabilities.

The original unconditional WSL2 provisioning card, WSL2 welcome row, `winget` welcome row and unimplemented Windows startup/tray settings are removed from generated UI on every target. Their adapters are absent even on Windows. Original recipe records remain preserved, inert data; recipe text no longer claims a working WSL2 sandbox. The original checkout is not modified.

## Source audit and remaining release blockers

| Surface | macOS | Linux | Windows | Evidence / next gate |
|---|---|---|---|---|
| Native Slint window | Actual arm64 lifecycle qualified | Prior public source compiled | Prior public source compiled | New local source requires new CI run before claiming target compilation |
| Owned backend process/listener | Implemented and actual fresh/reopen/quit/reaping tested | Unix implementation; native target lifecycle pending | Explicit refusal; missing Windows inherited listener / process identity / child custody | `desktop/slint/src/owned_backend.rs`, `src/desktop.rs` |
| Automatic encrypted local workspace | Actual synthetic create/reopen tested, no login | Unix private modes / exclusive rename implementation, target test pending | Missing private ACL/reparse/handle custody and exclusive publication; existing API refuses unsupported publication | `crates/smith-profile-storage/src/{local,files}.rs`; do not substitute permissive storage |
| GGUF worker lifecycle | Actual local model evidence exists separately | Unix transport implementation, target test pending | Transport/plan explicitly unsupported | `crates/smith-managed-runtime/src/{plan,transport,supervisor}.rs` |
| MLX | Native Apple silicon worker tested separately | Native MLX refused; Linux protocol fixtures are not MLX | Native MLX refused | `crates/smith-mlx-runtime/src/plan.rs`; UI only offers Apple silicon |
| GPU inventory/planning | Mac inventory and real MLX evidence | Linux parser/API and planner tests | Windows inventory adapters and planner tests | Inventory does not establish kernel execution, fit, speed or safe optimal settings |
| Open/save/folder dialogs | Native NSOpenPanel/NSSavePanel | Missing native adapter | Missing native adapter | `desktop/slint/src/picker.rs`; path input does not implement export dialogs |
| Clipboard | Native NSPasteboard write adapter | Explicit unsupported response | Explicit unsupported response | No silent shell clipboard fallback |
| Speech / media normalization | Reviewed native transcription; Apple converter | Unix child support requires execution verification; Apple normalization unavailable | Child launch refuses unsupported platform | `src/local_transcription.rs`; no inference from compile success |
| Computer-use effects | Scoped AX adapters compile, actual permission/input journey unqualified | Explicit unsupported | Explicit unsupported | `docs/PLATFORM-ADAPTERS.md`; no arbitrary host-shell fallback |
| Startup / tray / WSL2 provisioning | Removed unsupported controls | Removed unsupported controls | Removed unsupported controls | Implement each real OS adapter before enabling |
| Distribution / updates | Local arm64 `.app`; unsigned/unnotarized, no installation | No qualified distributable/update path | No qualified distributable/update path | Signing, packaging and recovery remain release work |

## Checks

Five pure policy tests exercise macOS arm64/x86_64, Linux and Windows x86_64/aarch64, unknown targets, every filtered index round-trip, invalid indices and foreign saved values. Three tests apply the real overlay to the pinned original UI and check absent unsupported controls, preserved neighboring cards and target-based inference routing. These tests run on a Mac with other targets represented as input data; they are not execution on those operating systems.

The local CI plan now includes eight normal build lanes: backend/native UI for Linux x64, Windows x64, macOS arm64 and macOS Intel, plus policy/markup checks on native lanes, using the already-reviewed vendored UI snapshot without a private checkout or new credentials. `ci/test_build_native.py` has seven passing tests covering all eight command plans and rejects OS/architecture mismatches. It is a review template outside `.github/workflows`; nothing new has been published or dispatched.

The last actually completed hosted run is [public 6db20f7, four passing Linux/Windows compilation jobs](https://github.com/lazarevtill/Smith/actions/runs/38826313123). It predates these local fixes. Runner labels for the proposed Mac lanes were checked against [GitHub's runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners): `macos-15` is arm64 and `macos-15-intel` is Intel. No GPU capability is inferred from runner labels.

## Gates before declaring all platforms ready

1. Run the exact reviewed updated source in the eight CI lanes after its separately approved publication. Record compiler, source, lockfile, binary hashes and real runner architecture.
2. Implement Windows owned-process/listener and encrypted-storage custody before a Windows startup smoke test can pass. Never work around these guards to make a green test.
3. Complete native Linux/Windows file dialogs, exports and clipboard, then validate chosen/cancelled paths and non-overwrite semantics on their actual desktops.
4. Run real fresh/reopen/quit/reaping and copied-data import/rollback on each OS. Preserve original data; use synthetic profiles first.
5. Execute a hardware matrix later as requested: Apple silicon MLX/Metal, supported NVIDIA CUDA, AMD HIP/ROCm, Intel SYCL/Arc and Vulkan/CPU cases. Include exact device/driver/runtime/model identity, overrides, cancellation, restart, memory-pressure behavior and explicit fallback. A failed or unavailable accelerator must not silently select another provider.

No new model/runtime download, real-profile access, deployment, key creation for an external service or publication occurs in this audit.

## Latest local Mac qualification

The final platform build passes actual Settings, compact chat/draft and imported historical-card UI regressions on macOS arm64. Settings preserve foreign stored backend values until an explicit supported replacement is saved, then clear the warning from confirmed saved state. Original UI asset files are unchanged. These checks do not qualify Windows/Linux runtime behavior.

The local Parity14 arm64 app also passed fresh workspace, reopen and LaunchServices bundle-open lifecycle checks with empty stderr and all owned backends reaped. It remains unsigned and unnotarized.
