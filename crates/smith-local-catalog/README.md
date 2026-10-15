# Saved local selections

This core saves explicit external runtime/model selections. It does not discover,
download, install, probe, execute, optimize or load anything. Saved selections are
not native runtime plans, approvals or credentials.

`Catalog::decode`, `list`, `list_trash`, `details`, `select_for_review`, `trash`
and `restore` operate on saved data only. They work when selected files are
missing. Selection-for-review is separate from the application's loaded model.
Every mutation returns a candidate catalog; this library performs no persistence
writes. The host owns encrypted storage, its transaction and request authority.

## Host integration

1. Read one bounded value from the already-keyed database. Use `Catalog::empty()`
   only when its row is absent; use `decode` for every existing value. A malformed,
   oversized or unknown-version record is an error, never a reset.
2. Browse `EntrySummary` values without reading paths. Explicit Details returns
   the passive manifest and declared adapter-scoped hints. Paths should stay in
   authenticated profile responses, not logs or telemetry.
3. For an explicit registration/replacement, capture the current profile/session,
   catalog revision and host authority before scheduling a tracked blocking worker.
   Give `capture(CaptureRequest, &ReadControl)` exactly the selected runtime and
   model roots plus their explicit file names. No directory listing occurs.
4. Serialize cancellation and publication under the same host lock. After the
   blocking worker returns, check its `ReadControl`, live profile/session and
   expected catalog again. `insert` or `replace` consumes the successful
   `CapturedAssets`; the host's transaction compares the exact prior stored bytes
   (or their digest) and `storage_base_revision`, then writes `encode()`.
   `storage_base_sha256=None` means the row must still be absent. A mutation
   retains its base fingerprint, so decode the committed new bytes before using
   the returned catalog as a new persisted snapshot.
5. `select_for_review` changes only the passive selection. It does **not** apply
   saved hints, alter manual pins, change active configuration, or load anything.
   The existing runtime-preferences record stays separate.
6. Construct `PreparationContext` from current host facts: opaque profile session,
   opaque connection/configuration revision string and inventory generation.
   `review(id, entry_revision, context)` returns a getter-only `SelectionReview`.
   `prepare(&review, &current, &control)` rehashes every selected file. After the
   worker returns, reload authoritative catalog/context and `check_review` again.
7. The resulting `PreparedSelection` contains immutable captured data, **not**
   permission to execute. Reuse the native adapter's actual model inspection,
   capability/CLI or worker checks, prepare, explicit approval and final load
   revalidation. Never deserialize a saved record into those authority types.

This crate does not prove that a host-supplied context is current. The host must
hold/recheck its own consent/session/configuration state across admission and
publication. Cancelling a blocking read does not make its worker disappear:
retain and drain it before claiming cleanup or closing profile storage.

## Identity and storage

Format version 1 has a monotonic JavaScript-safe catalog revision. Newly created
entry IDs derive from their creation revision, and entry revisions change on
replacement, Trash and restore. All mutations check both catalog and entry
revisions where applicable. Trash clears selection and preserves the entry and
external files. Restore retains its ID but never selects it. There is no purge
API; the 128-entry cap includes Trash and fails without discarding data.

Selected manifests bind format, declared metadata/hints, provenance labels,
source revision, exact root/name/role, byte count and SHA-256. A review additionally
binds the entire entry, catalog revision and current host context. Hashes identify
selected bytes, not a publisher, installed dependency closure or optimization.
`CapturedAssets`, `SelectionReview` and `PreparedSelection` have no deserializer;
the latter two have no public constructor or mutable field access.

Each runtime group has exactly one executable plus optional explicit resources.
GGUF model groups contain one weight file. MLX groups contain explicit
`config.json`, `tokenizer.json`, `tokenizer_config.json` and at least one named
`.safetensors` weight file. This is a structural catalog contract, not a GGUF,
SafeTensors, tokenizer or architecture parser. The native adapter must establish
the model's actual support later. Unlisted files are neither read nor covered by
this identity. Every package is labelled `external_user_selected` and
`selected_files_only`; version 1 cannot manufacture installer or complete-package
verification. A future managed-package path needs a distinct, verified contract.

Declared capability and settings hints are typed by `llama_gguf` versus
`mlx_swift`, range-checked, and never interpreted as supported/effective values.
Selecting a saved entry cannot erase a user's manual overrides. No cloud/provider
fallback, credential, endpoint, arbitrary command, environment or grammar is
represented by this catalog.

## Bounded file validation

Unix capture/prepare opens each selected path component with `openat`, no-follow,
close-on-exec and nonblocking flags. Only single-link regular files are admitted;
the selected executable must have an executable permission bit. Symlinks,
special files, ambiguous path spellings and duplicate opened file identities are
rejected. Explicit inputs must use resolved roots; the library never substitutes
a canonicalized or discovered alternative.

Every byte is hashed on every capture and prepare. Open-file identity, size,
mtime/ctime, mode and link count are checked around hashing, then all held file
descriptors and fresh no-follow path opens are checked again after the last file.
There is no inode/mtime hash cache. Byte-identical replacement between separate
preparations is accepted only after full hashing; the native runtime owns its
separate final race checks and process custody.

Limits: 2 MiB serialized catalog; 128 entries including Trash; 128 explicit files
per entry; 64 GiB combined selected bytes; executable 512 MiB; runtime resource
1 GiB; model metadata 64 MiB. Read buffers are 1 MiB. The caller chooses a deadline
up to 300 seconds and can latch cancellation. Checks occur around each read;
the OS may block inside an individual regular-file read, so this is a cooperative
deadline, not a hard kernel-I/O cancellation guarantee. The host must not announce
completion while a worker remains undrained.

Persistence and lexical foreign-platform browsing are portable. Windows secure
capture/prepare currently returns `UnsupportedPlatform`; no Windows file-race or
runtime qualification is claimed. Unix implementation qualification is limited
to synthetic owned files on the tested host, recorded with test results.

## Synthetic fixture and qualification

`tests/catalog.rs` constructs disposable synthetic GGUF/MLX-shaped file sets and
never executes a selected file. It covers byte replacement, tokenizer changes,
revision/session/inventory invalidation, passive missing-file browsing,
recoverable Trash/restart, cancellation/deadline, malformed persistence, adapter
hint mismatch, symlink/hardlink/FIFO rejection and sparse-file size bounds.

The crate is isolated from root Cargo/schema/API wiring. On 2026-10-15, all 13
synthetic integration tests passed on the macOS host, followed by strict
all-target Clippy and formatting checks. Execution used one build job, offline
dependencies and a separate temporary target; no model, selected executable or
existing profile was executed/read. The sparse-file cap test creates only an
owned sparse file and rejects it before hashing. No Windows/Linux runtime test
or complete app integration is claimed.

Reproduce with `CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=target/catalog-tests
cargo test --manifest-path crates/smith-local-catalog/Cargo.toml --locked --offline`
from the repository root, and the same variables for
`cargo clippy --manifest-path crates/smith-local-catalog/Cargo.toml --locked
--offline --all-targets -- -D warnings`.
