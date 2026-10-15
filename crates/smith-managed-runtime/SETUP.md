# Explicit local setup contract

`inspect_selected_artifact(&Path, ArtifactKind, source, source_revision, &Cancellation)` computes SHA-256 for one selected existing file. It never runs code. Hashing streams the complete file with a 1 MiB buffer; limits are 512 MiB for native executables and 1 TiB for models. Source labels are declarations, not publisher verification. `src/runtime_catalog.rs::inspect_selection` joins that custody with bounded GGUF metadata and rejects identity/modification changes between inspection and hashing. Its strict input has `path`, `source`, and `source_revision`; no user-entered digest is required.

`inspect_gguf` accepts little-endian GGUF v2/v3 and reads only the header/metadata. It caps metadata at 64 MiB, keys at 8,192, key lengths at 1,024 bytes, individual strings at 1 MiB, aggregate values at one million, and nesting at four levels. It checks UTF-8, value types, booleans, duplicate keys, arithmetic/length bounds and truncation. Split metadata or a shard filename rejects without discovering/opening sibling files. Native `LoadPlan` preparation also applies this check; explicitly synthetic protocol fixtures retain their separate execution kind.

The result reports file size, declared architecture/name/size label, context, layers and attention dimensions where available. Conventional scalar llama/qwen2/qwen3 metadata can produce an explicitly labeled full-attention KV element estimate; arrays, hybrid/recurrent/sliding-window layouts and absent dimensions remain unknown. Tensor layout/data, actual memory requirements and successful inference are not validated. Format evidence: [GGUF specification](https://github.com/ggml-org/ggml/blob/master/docs/gguf.md).

## Runtime inspection approval

Prepare `RuntimeProbePlan` from the captured executable artifact, display its `summary()`, and approve its exact ID. Preparation rehashes but executes nothing. Approval is opaque, non-deserializable, single-use and expires after five minutes. `RuntimeProbe::start(approved, cancellation)` owns the worker; the host should await `wait()` inside a registered task and cancel/join it on workspace shutdown. Dropping its handle cancels the worker.

Execution uses the installed canonical executable **in place**. It does not copy an Apple platform binary, adopt another process, search PATH, scan directories, or supply a model path. The only two commands are `--help` and `--list-devices`, each with an empty inherited environment except `LANG=C`, private temporary working directory, closed stdin, 10-second deadline, and 128 KiB per output stream. The shared probe/runtime directory constructor explicitly creates Unix directories with mode0700, including under a permissive process umask. Cancellation/output overflow/deadline kills and awaits the owned child, with a two-second reap deadline. Cleanup failure is an error; the host must not portray it as a successful stop. Only the direct child is owned; escaped descendants and parent-crash cleanup are not certified.

This action executes selected native code. A hash identifies that file; it does not prove publisher authenticity or constrain malicious code to the documented flags. Installed dynamic dependencies are not hashed. In-place pathname execution also retains the platform's final metadata-check-to-exec race; approved installation files must not be concurrently replaced. No network/model behavior is claimed on behalf of arbitrary selected code. Smith itself supplies no model, credentials, network flags, inherited configuration, or free-form arguments.

The report includes exact bounded output, exit status/signal, confirmed direct-child reap, and named CPU/Metal/CUDA/HIP/SYCL/unknown observations from a successful device listing. Missing entries remain unknown. These observations do not prove an inference backend, placement, memory availability or successful model load.

## Typed settings

`RuntimeCompatibility` comes only from the same approved probe's successful help output. It serializes for display but cannot deserialize into authority. Host getters expose supported flags, flash attention, KV types, memory settings, UNIX socket syntax, missing core flags and `protocol_candidate()`. A candidate requires every fixed launch flag, the UNIX socket description, and `fit off`/`device none` syntax. Actual load still separately validates readiness, peer PID, model identity and HTTP protocol.

Use `LoadPlan::prepare_with_options(manifest, config, settings, &compatibility, cancel)` for the new setup flow, including when all optional settings are absent. It rejects a missing protocol candidate. Existing `LoadConfig` and the original explicit `prepare` signature are unchanged.

| Typed optional field | Fixed argv |
| --- | --- |
| `prefill_threads`, 1–512 | `--threads-batch N` |
| `batch_size`, 1–32,768 | `--batch-size N` |
| `micro_batch_size`, 1–batch size, explicit batch required | `--ubatch-size N` |
| `flash_attention`: on/off/auto | `--flash-attn VALUE` |
| `cache_type_k`, `cache_type_v`: f16/q8_0/q4_0 | `--cache-type-k TYPE`, `--cache-type-v TYPE` |
| `mmap`, `mlock` | Supply both resolved booleans; reported `--load-mode` values, or reported legacy `--mmap`/`--no-mmap` and `--mlock`/`--no-mlock` flags |

Quantized V requires explicit flash attention on. Every supplied setting requires matching help evidence for the captured executable. Settings and resulting argv are included in the reviewed plan hash and passed to the actual supervised child. No arbitrary flags or environment fields are accepted. This is observed CLI syntax compatibility plus fixture-tested translation, not hardware qualification. Argument evidence: [llama.cpp argument definitions](https://github.com/ggml-org/llama.cpp/blob/master/common/arg.cpp).

Synthetic tests cover bounded/malformed metadata, sparse files without tensor reads, shard refusal through both entrypoints, automatic hashes with no execution, exact approval, stale file identity, bounded output, timeout/cancel/drop reaping, protocol/flag gates, and actual owned-child argv. They never load a real model, contact a provider, or change drivers. Run `cargo test --offline --manifest-path crates/smith-managed-runtime/Cargo.toml`; local Unix-socket fixture tests require permission to bind their private temporary sockets.
