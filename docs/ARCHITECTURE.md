# Smith refactor boundaries

The desktop shell retains the original Slint interface. A local Rust host owns
the encrypted profile, provider selection, task state and subprocess lifecycle.
The shell connects only to its own backend through a private launch handshake;
it does not adopt an existing server. Local process/session ownership is an
internal boundary, not a user account system.

## Layers

| Layer | Responsibility |
| --- | --- |
| `desktop/slint` | Native controls, captured review state, stale-action refusal, explicit file selection and UI feedback |
| Root `src` | Local API orchestration, accepted-run transactions, bounded jobs and lifetime coordination |
| `smith-profile-storage` | SQLCipher schema, automatic local key/profile custody, staged copy import and migration |
| `smith-agent-core` / `smith-tool-effects` | Typed task authority, exact tool records, bounded effects and cancellation |
| `smith-local-catalog` | Passive model records, exact selected-file identities and versioned edits |
| `smith-managed-runtime` / `smith-mlx-runtime` | Reviewed runtime plans, native process custody, protocol checks and clean shutdown |
| `smith-acceleration` / `smith-platform` | Hardware observations, constrained setup advice and explicit OS adapters |
| `smith-media` | Bounded parsing, static raster validation and document handling |
| `desktop/mlx` | Native Swift/MLX worker for the Apple runtime; no Python fallback |

## Data and state

Normal startup opens a local encrypted workspace automatically. Legacy import
operates on an explicit copy and preserves source records. Schema changes are
versioned and transactional. The new model-provenance table records the exact
accepted model, worker, template, thinking mode and sampling settings; earlier
runs retain unknown provenance instead of invented metadata.

Saved-model selection is passive. Prepare revalidates selected files and binds
the review to settings, hardware observation, profile session and configuration
revision. Loading requires the exact reviewed plan. An edited form cannot reuse
the approval of an earlier plan. Unknown operation outcomes retain their request
identity so restart recovery cannot duplicate a committed edit.

Recoverable removal retains data until an explicit retention policy is chosen.
Imported images and tool calls are displayed as historical content, not executed
as new commands. File paths and remote URLs found in historical content do not
authorize reads or fetches.

## Native acceleration

Apple MLX and native GGUF runtime integration have separate adapters. CUDA,
ROCm/HIP, Intel and other device paths require their real runtime and platform
implementations. Unsupported operations return an explicit error; a device name
or suggested setting is never treated as proof of execution or memory fit.

Windows/Linux CI is configured to check compilation. Secure storage publication,
process custody, platform dialogs, packaging and accelerator execution require
further platform work and real-device tests. See the CI document for exact gaps.

## Migration and rollback

The refactor does not overwrite the original source or migrate a live database
in place. Copy import, exact digests, version gates and synthetic migration tests
precede user-data migration. Existing profiles and their key material must be
retained together. Application rollback must use a compatible profile copy;
opening a newer schema with an older executable is not a rollback mechanism.

Parity is tracked by behavior: input, accepted authority, persisted output,
cancellation, restart and recovery. Rendering the same screen or passing a mock
provider test alone does not establish original-feature or model-quality parity.
