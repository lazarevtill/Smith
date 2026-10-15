# Smith MLX runtime supervisor

Private, unpublished application component. No application-code license has been selected. This crate implements an owned Rust supervisor for the [native Swift worker protocol](../../desktop/mlx/PROTOCOL.md); its implementation contains no Python bridge, downloader, HTTP endpoint, or provider fallback. Native mode is macOS Metal only. Linux is supported only for explicitly labelled synthetic protocol fixtures.

## Host integration

```rust,ignore
let plan = LaunchPlan::prepare(
    SelectedWorker {
        path: selected_worker,
        source: selected_provenance,
        source_revision: selected_revision,
        execution_kind: ExecutionKind::Native,
    },
    &canonical_selected_model_directory,
    Budgets::default(),
    &cancellation,
).await?;
// Retain this opaque plan on the host and review plan.summary().
let approved = plan.approve(&exact_reviewed_plan_id)?;
let runtime = match Runtime::launch_owned(approved, cancellation).await {
    Ok(runtime) => runtime,
    Err(failure) => {
        retain_runtime_for_cleanup(failure.runtime); // Host retains this receipt.
        return Err(failure.error);
    }
};
let mut inspection = runtime.begin(runtime.status().revision, Request::Inspect)?;
let Outcome::Manifest(manifest) = inspection.wait().await? else { unreachable!() };
// The host reviews the actual supported capabilities and context limit.
let mut load = runtime.begin(runtime.status().revision, Request::Load)?;
load.wait().await?;
```

`Runtime` is cloneable. `status()` and `subscribe()` expose a revisioned `Snapshot`; `begin(expected_revision, Request)` accepts at most one operation and rejects stale revisions before dispatch. A successful launch proves the exact protocol handshake, not model readiness. Only `Phase::Ready` with a loaded manifest means the worker accepted the load. `Request::Unload` releases the loaded model while retaining the child. Inspect/load always refer to the same approved model selection; another model requires another reviewed plan/session.

Application hosts use `Runtime::launch_owned`, which returns either the runtime or `LaunchFailure { error, runtime }`. On failed startup, retain that runtime until `shutdown()` confirms cleanup. This includes malformed handshakes and pre-spawn copied-bundle removal failures; eventual recovery remains observable through the same receipt. The `launch` convenience method returns only `Failure` and is appropriate for controlled tests, not host cleanup recovery.

`GenerationRequest::Raw { prompt, sampling }` sends the caller's exact raw prompt. `GenerationRequest::Chat { messages, sampling }` is admitted only when the actual worker manifest names a supported exact template (simple ChatML, simple Granite, or the pinned Qwen3 string-content template). `Manifest::capabilities()` exposes `RawCompletion` and, only when admitted, `Chat`. The host must keep raw completion separate from ordinary chat; complex or unknown Jinja templates are not silently substituted. Actual context/token limits are checked by the worker, with bounded output and terminal token-count validation in Rust.

`GenerationRequest::ChatWithThinking { messages, sampling, thinking_mode }` is an additive API; old `Chat`/`Raw` callers are unchanged. `ThinkingMode` has exactly `ModelDefault` and `Disabled`. Default mode emits the original wire request with no thinking field. Disabled mode requires both the exact `Qwen3DefaultThinking` template and the negotiated `qwen3_thinking_control_v1` worker feature. Model names alone never qualify. The hello parser accepts only the exact legacy capability list or the exact new list; unknown/duplicate capabilities are refused. A legacy worker remains usable for default chat but is never sent an explicit disabled request.

Before saving a selected mode as ready, the host calls `runtime.validate_chat_thinking(expected_revision, mode)` after load. It checks live readiness, current revision, exact template and worker feature; failure must leave settings/config uncommitted and use normal owned cleanup. `Snapshot.worker_features` also permits refusal of a legacy worker immediately after handshake, before load. Every generation independently repeats the feature/template check before dispatch. A disabled request requires an exact terminal `thinking_mode:"disabled"` acknowledgement; missing, wrong, null, unknown, or unsolicited default acknowledgements are protocol failures and retire the child. `Completion.thinking_mode` records a confirmed explicit terminal only; host-driven cancellation leaves it absent. Generated text, including literal reasoning tags, is never removed or transformed to make structured output pass.

`runtime.generation_provenance(expected_revision, &request)` validates and captures descriptive request intent without dispatch: exact runtime plan/worker/resource/package identities, session/revision, capability/template, requested mode, sampling, and negotiated feature. It contains no prompt text or filesystem paths. The host may insert that value (with its own reviewed setup/settings identity) in the existing accepted-run transaction, while retaining the same request/revision for dispatch. It is not an execution grant. `operation.provenance()` returns an immutable reference set once after actor admission, before dispatch, and retained across completion, error, cancellation, and runtime shutdown. Failed pre-admission requests have no operation provenance. A captured request is not proof that the model completed or complied; outcome and incomplete flags remain separate.

`Operation::subscribe()` is a bounded latest-value watch channel. Each `Progress` contains the **entire revised text**, a monotonically increasing output revision, and `incomplete`/`finished` flags. Consumers replace their visible output with `Progress.text`; they must not concatenate updates. The supervisor validates every byte offset as a UTF-8 scalar boundary before modifying text. `Operation::wait(&mut self)` returns `Outcome`; retain the handle or a progress receiver to recover the last full text after an error.

Call `Operation::cancel()` for a single active operation. Generation cancellation returns a partial `Completion` marked incomplete with unknown token counts once child reap and scratch removal are confirmed. If either remains unconfirmed, the operation returns `Failure::Cleanup`, including when the child has exited but scratch removal failed; the last full text remains available in incomplete progress. Deadlines and protocol/worker errors return typed `Failure` while preserving the last valid full text in progress. Cancellation, timeout, protocol failure, and worker error retire the whole child session; a further generation requires an explicitly launched/loaded session. Dropping a pending operation also cancels it.

Use `Runtime::begin_shutdown()` to synchronously close admission, then await `shutdown(&self)`. Every runtime clone shares one retained join handle. Concurrent shutdown calls, including a later caller after an abandoned shutdown waiter, await the same bounded cleanup attempt. Dropping a nonfinal runtime clone does not cancel; dropping the last owner latches cancellation. Hosts must register and await their launch/operation/shutdown futures during application teardown. They must retain an occupied fault and refuse replacement engines when `Failure::Cleanup` occurs or `spawned != reaped`; an unconfirmed child is never reported as successfully reaped. A retained cleanup owner keeps the exact child/readers/scratch through repeated OS wait or removal errors, retries, and updates the same status watch after confirmed recovery. A later `shutdown()` can then confirm resolution. Shutdown can return a fully reaped snapshot whose phase is failed, for example after malformed shutdown acknowledgement; inspect the snapshot's failure separately from cleanup ownership. Recovery preserves the original protocol failure.

## Selection and process boundary

Preparation hashes the selected native executable, its six required colocated runtime resources, and each admitted model package file with a bounded streaming buffer. It does not execute the worker or read model tensors into memory. Plans expire after five minutes, cannot be deserialized into authority, and require the exact retained review ID. Expiry and cancellation are checked again after hashing/copying directly before launch. `PlanSummary.runtime_files` contains the executable plus resource hashes/sizes; `runtime_bundle_sha256` binds their sorted names and contents.

Native mode requires `mlx.metallib`, `mlx-swift_Cmlx.bundle/Contents/Info.plist`, `mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib`, `swift-transformers_Hub.bundle/gpt2_tokenizer_config.json`, `swift-transformers_Hub.bundle/t5_tokenizer_config.json`, and `swift-crypto_Crypto.bundle/PrivacyInfo.xcprivacy`. Resource files and their ancestors must not be symlinks; hardlinks, missing files, excess sizes, and inconsistent Metal library copies are refused. Metal libraries are capped at64MiB each, tokenizer configs at1MiB each, plist/privacy resources at64KiB each. Native launch copies only these admitted resources and the selected Swift worker into the owned scratch tree, verifies the copied hashes, makes resources read-only and the worker executable read-only, and executes that private copy. The worker independently refuses missing/corrupt colocated resources before MLX entry, preventing fallback to absolute build resources. Compatible-fixture mode deliberately executes its explicit fixture binary in place and is labelled separately. Apple platform executables are not a supported worker catalog.

Model roots must be canonical absolute UTF-8 paths with no empty, dot, parent, or symlink components. Files are opened relative to a retained directory descriptor with `O_NOFOLLOW`; symlinks, nonregular files, hardlinks, subdirectories, and executable/custom model code are rejected. Bounds match the worker: at most 128 entries, 16 GiB package data, 32 MiB tokenizer JSON, and 1 MiB other small files. Only the worker's explicit top-level safetensors/config/tokenizer allowlist is hashed; passive README/license metadata is ignored after its file type is checked. File/root identities are checked around hashing.

Every inspect/load request sends the approved package hash. The Swift worker checks the hash of its newly created private snapshot **before** GPU/model-loader entry. Rust then requires the returned manifest's exact file records and package hash before publishing loaded readiness. Thus a changed source cannot become an accepted load through a review/snapshot race. Worker-side safetensors, tokenizer, template, tensor-shape, and shard validation remain necessary; Rust hashing is not a substitute for model-format validation.

Launch uses only the selected worker bundle, fixed `--scratch-root` argument, a fresh private scratch working directory, cleared environment plus `LANG=C`, and private stdin/stdout/stderr pipes. On Unix, scratch is created atomically with mode0700, independent of a permissive process umask; the child fixture verifies its mode and owner. Scratch, including the copied bundle, is removed after confirmed child reap. No shell, arbitrary flags, credentials, inherited provider environment, device fallback, or network configuration is supplied. The supervisor is an ownership/protocol boundary, **not an OS sandbox for an untrusted native executable**. Local hashes identify bytes, not publisher trust or full OS dynamic-library certification; the host is responsible for admitting the selected worker. No automatic scanning or adoption occurs.

Frames are limited to 131072 bytes, stdout has a bounded eight-frame queue, stderr is discarded with a 131072-byte lifetime cap, patches are at most 16384 bytes, and total output is at most 524288 bytes. Duplicate JSON keys, invalid UTF-8, null/stale IDs, unknown fields, excess nesting, unsolicited/duplicate terminal frames, invalid revisions, and mismatched capabilities are errors. Request IDs are unique and session requests are bounded below the worker's 4096-request ceiling. Independent hello/load/generation/drain/reap deadlines apply. Cancellation sends an exact target request, accepts both legal acknowledgement orderings, then kills and reaps if drain fails. No later model request is admitted to a retired session.

## Verification and current limits

The tests compile a small std-only Rust protocol child into an owned temporary directory. Synthetic manifests/weights are arbitrary fixture bytes; no MLX loader, real model, GPU inference, network, credentials, user profile, or user files are used. The fixture verifies fixed arguments, cleared environment and private working directory. Tests cover selection changes, symlink/hardlink/code refusal, revision admission, raw/chat distinction, UTF-8 replacements, partial errors, cancellation ordering and ignored cancellation, dropped handles, shared shutdown, malformed/stale/oversized stdout, stderr overflow, private resource copying, approval expiry, process reap, and scratch removal. Real temporary-directory permission faults exercise retained cleanup and failed-launch recovery without manufacturing successful OS cleanup.

Run `cargo test --offline --manifest-path crates/smith-mlx-runtime/Cargo.toml` and `cargo clippy --offline --manifest-path crates/smith-mlx-runtime/Cargo.toml --all-targets -- -D warnings` with the installed Rust 1.89 toolchain. Native worker qualification and application/provider wiring are separate work; these fixture tests do not establish real-model compatibility or cross-platform acceleration.
