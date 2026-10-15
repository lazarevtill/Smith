# Native MLX child protocol v1

Private application component; no application-code license has been selected.

Launch `smith-mlx-worker --scratch-root /absolute/private/0700/directory` as an owned child with pipes. No HTTP server, Python, remote model identifier, downloader, model install, custom model code, or implicit provider fallback exists. The worker requires an explicitly selected local **MLX safetensors** package; GGUF is not this backend's format.

stdin and stdout are UTF-8 JSON Lines, one object per LF, at most 131072 bytes before LF. stdout contains only protocol events; native diagnostics are redirected to stderr. The supervisor must bound stderr too. No credentials or model input should be logged. A malformed/oversized/unfinished frame emits a null-ID error and closes the session. Unknown fields, duplicate JSON keys, unknown operations and invalid IDs are rejected. IDs contain 1–64 ASCII letters, digits, `_`, `-`, and cannot repeat in one child lifetime. Maximum 4096 requests, one active operation, one loaded model. Start a fresh child when the request budget is exhausted.

All commands contain `v:1,id:string,op:string`. Commands:

| op | Additional fields | Result |
|---|---|---|
| `hello` | none | `done` with backend/protocol/limits, no GPU access |
| `inspect` | `model_root:string,expected_package_sha256?:string` | `started`, then `done` with manifest/capabilities; validates and hashes a private snapshot, no GPU access |
| `load` | `model_root:string,expected_package_sha256:string` | `started`, then `done` with manifest/capabilities; genuine MLX Metal loader |
| `generate` | `mode:"raw",prompt:string,max_tokens:int,temperature:number,top_p:number,seed?:uint64` | `started`, zero or more `delta`, then `done` or `error` |
| `generate` | `mode:"chat",messages:[{role,content}],max_tokens,temperature,top_p,seed?,thinking_mode?:"model_default"\|"disabled"` | same; requires admitted local template; disabled requires exact Qwen3 template |
| `cancel` | `target_id:string` | own-ID `done` acknowledging request or `not_active` error; target gets its own terminal event after drain |
| `unload` | none | `started`, then `done`; only when idle |
| `shutdown` | none | cancels/drains active work, releases model/snapshot, own-ID `done`, exits |

Examples:

```json
{"v":1,"id":"hello-1","op":"hello"}
{"v":1,"id":"load-1","op":"load","model_root":"/approved/canonical/model","expected_package_sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}
{"v":1,"id":"gen-1","op":"generate","mode":"raw","prompt":"Explicitly formatted prompt","max_tokens":128,"temperature":0.6,"top_p":0.9,"seed":7}
{"v":1,"id":"cancel-1","op":"cancel","target_id":"gen-1"}
```

Every event has `v:1,id,event`. `started` has no other fields. `error` contains a bounded stable `code` and redacted `message`; no path, original parser exception, prompt, or token is included. `done` for inspect/load contains `manifest:{model_type,files:[{name,bytes,sha256}],package_sha256,template?,context_limit}` plus `capabilities`. `template` absent/null means chat is unsupported, not a default template.

Current source hello capabilities are exactly `["inspect","local_text","qwen3_thinking_control_v1"]`. Previously qualified frozen workers advertise exactly `["inspect","local_text"]`; they do not implement the new thinking option. A compatible supervisor must admit those legacy workers only for `model_default`, and must require the new capability plus the exact Qwen3 template before admitting `disabled`. This source change requires a newly qualified worker/supervisor; it does not change any previously frozen executable.

Generation `delta` is **a text revision**, not necessarily an append:

```json
{"v":1,"id":"gen-1","event":"delta","replace_from_utf8":3,"text":"replacement suffix"}
```

Keep exactly the first `replace_from_utf8` bytes of the previous output (always a UTF-8 scalar boundary) and append `text`. This preserves tokenizer whitespace/Unicode corrections. The root app must not merely concatenate these events. A final generation `done` includes `input_tokens`, `output_tokens`, `finish_reason` (`stop`, `length`, `cancelled`). Text is delivered only by deltas; done has no unbounded copy. A cancelled stream's partial text must be shown as incomplete.

If a chat request explicitly supplied `thinking_mode`, its `done` additionally echoes that exact value. The native backend must confirm the value; the worker refuses an unconfirmed completion. For a request omitting the field, the original terminal shape stays unchanged. The supervisor must require a matching echo for an explicit option, rather than infer application from generated text. A terminal with `finish_reason:"cancelled"` remains incomplete even if it carries the matching echo: the public completion must not report a successful thinking acknowledgement. This includes worker-initiated cancellation without a host cancel command. Requested intent stays in provenance; cancellation or errors do not acknowledge successful completion.

Bounds: raw/chat input 65536 UTF-8 bytes, at most 64 system/user/assistant messages, 8192 input tokens, 1–2048 output tokens, output524288 UTF-8 bytes, one decoded patch at most16384 bytes, temperature0–2, top_p(0,1]. Complete prompt plus requested output must fit `context_limit` (maximum10240); no silent truncation. No tools, media, multimodal input, embeddings, audio, image synthesis or generic reasoning controls are advertised.

Model root and scratch must be absolute canonical paths without dot/empty components or symlink ancestors. Source files are opened with descriptor-relative `O_NOFOLLOW`; regular files with one hard link only. Top-level safetensors only, at most128 directory entries and16GiB aggregate package bytes; JSON/tokenizer/header/dimension/tensor/index bounds are checked before MLX. All shard references must exactly match files/tensors in the snapshot. Custom code and arbitrary nested package directories are rejected. Only passive README/license metadata is ignored. Snapshot data remains inside the caller's private scratch until unload; it is not copied to any global model cache. The supervisor must remove its own scratch tree after an abnormal child exit.

Supported architecture admission is dense `llama`, `mistral`, `qwen2`, `qwen3`, `granite`, bounded dimensions and affine4/8-bit quantization. Admission does not prove checkpoint compatibility; MLX still verifies tensor shapes/required parameters. Unsupported architecture/tokenizer/quantization produces a typed error.

Chat recognizes the two exact simple local template strings and one exact published Qwen3 template in `Sources/WorkerCore/TemplatePolicy.swift`. These are rendered by bounded Swift string construction; arbitrary Jinja is never evaluated. The Qwen3 source SHA-256 is `87a2728cb8dc9fe424d624542f6060ec05a1d285ebbec578bb078900e33396b5`, from `mlx-community/Qwen3-0.6B-4bit` revision `73e3e38d981303bc594367cd910ea6eb48349da8`. Only its no-tools, string-content branch is supported. Other complex templates expose only `raw_completion`. Raw mode tokenizes the exact caller-provided string with `add_special_tokens=false`; it does not insert a template or change roles, and any supplied thinking field is refused.

Omitted thinking mode and `model_default` preserve the existing prompt bytes. `disabled` selects only the exact Qwen3 template's `enable_thinking=false` branch: append `<think>\n\n</think>\n\n` after `<|im_start|>assistant\n`. Historical/current assistant reasoning handling is identical in both modes. Other templates refuse this mode with `unsupported_thinking_mode`; unknown values, null and wrong JSON types are invalid requests. Literal `/no_think` or `/think` content never selects a mode. No generated token, reasoning envelope, whitespace or response text is stripped. The option does not guarantee model compliance or make malformed structured output valid.

EOF cancels active work, waits for native generation's owned producer task, releases memory/snapshot, and exits. Load/inspect watchdog180s; generation120s; a non-interruptible native operation receives cancellation then process exit124 after5s. EOF/shutdown also allow5s for drain. stdout backpressure is bounded to5s then exit74. Supervisor must independently enforce time/output/process limits and terminate/reap the owned child; cancellation cannot preempt an already submitted Metal kernel. No detached native producer survives worker process exit. Normal unload does not affect any other application/process.

The supervisor reviews bounded source file records and the exact package hash first, then passes that `expected_package_sha256` on load (required64 lowercase hexadecimal characters). The worker compares that hash against the newly completed private snapshot **before** any GPU/model-loader call; a changed package returns `package_changed`. The `inspect` command is optional metadata validation; the host can review file hashes without launching a worker. Optional inspect hashes use the same check. This prevents source replacement between review and loading from changing approved model bytes.

Runtime authority is separate from the model package: the supervisor must review and privately snapshot the executable and its exact six colocated resources listed in `Sources/WorkerCore/RuntimeResources.swift`. The worker requires all six regular, single-link files without symlink components, limits each Metal file to64MiB and each metadata file to1MiB, and requires the two Metal copies to have identical SHA-256 hashes. Missing resources return `invalid_runtime_resources` before GPU access. It then asks Metal to load the exact colocated `mlx.metallib`; invalid bytes return `invalid_metallib` before MLX initialization, preventing upstream bundle/build-directory fallback. The supervisor is responsible for binding those bytes to reviewed hashes and keeping the copied runtime immutable. Public operating-system dylibs remain an external platform dependency boundary.
