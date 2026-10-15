# Smith acceleration planning

Pure Rust 1.89 library with no filesystem, process, network, driver, GPU or model
access. It validates explicit local facts and returns settings advice or a
conservative candidate. It does not install adapters, infer that a model is loaded,
authorize execution or certify hardware from a backend name.

## Integration API

```rust,ignore
inventory.validate()?;
intent.validate()?;
let advice = preview(&inventory, &selected_adapter, &model, &intent, revision)?;
let outcome = plan(&inventory, &qualified_catalog, &model, &intent, revision)?;
if let PlanningOutcome::Candidate { plan, .. } = outcome {
    // Immediately before the separate owned-runtime/consent admission:
    assert!(revalidate(&plan, &fresh_inventory, &qualified_catalog,
                       &model, &intent, revision)?);
}
```

`HardwareInventory`, `MemoryPool`, `Device`, `AdapterDescriptor`, `ModelDescriptor`,
`ModelProfile`, `MemoryModel`, `OptimizationIntent`, and `Overrides` are typed,
serializable inputs with `deny_unknown_fields`. `validate()` rejects unsupported
ranges, duplicate identities, invalid digests, malformed references and ambiguous
profiles. Bound JSON request bytes before deserialization in the host; this crate's
collection/string limits are semantic checks after deserialization.

`plan()` returns `Blocked { intent, reasons }` or
`Candidate { plan: EffectivePlan, intent, reasons }`. `EffectivePlan` has private
fields, read-only getters, serialization for review, and no deserializer. Its
SHA-256 binds inventory generation and observations, adapter/dependency identity,
exact model identity, all intent/overrides, settings, memory profile and configuration
revision. `revalidate()` recomputes the decision against fresh facts. Persist
intent and passive metadata; never treat stored JSON as an execution grant.
`intent.fingerprint()` and `plan.intent_sha256()` support persistence checks.

`preview()` is intentionally separate. It requires one inspected installed adapter
identity and declared structural model/settings facts, but does not require complete
qualification or a memory upper bound. `PreviewOutcome::Preview` contains a
`SettingsPreview` whose qualification is always `unvalidated` and fit always
`unknown`; it has no effective-plan ID and cannot be passed to `revalidate()`.
This supports honest recommended settings for a local llama executable inspected
with bounded help/device probes. A host can offer its existing explicit manual-load
path separately. An inspected main executable uses
`selected_executable_sha256` and preview identity scope `selected_executable`;
its `adapter_bundle_sha256` remains null. It must never be relabelled as a complete
runtime/dependency package hash. Strict planning still requires a real complete
qualified bundle identity. A successful preview is never automatic qualification, optimized
execution or proof that memory fits.

## Trusted facts and memory accounting

An adapter is considered only if the host declares it `Qualified`, its installed
bundle hash is present, OS/architecture agree, and its device qualification matches
the observed device ID, OS version and driver version. Backend/vendor/OS/model-format
compatibility is independently checked. CPU, Metal, MLX Metal, CUDA, HIP, SYCL and
Vulkan are distinct identities. Capabilities must appear in both the exact model
and adapter inputs. No installed or qualified adapter is fabricated from inventory.

The trusted host must have validated the complete runtime dependency closure and
the actual adapter/model/device tuple before claiming qualification. Structs alone
cannot establish that evidence. This crate supplies no real qualified catalog.

Each model profile binds an exact model bundle, architecture, quantization, KV
type and layer-offload setting. `memory: None` means unknown and blocks a strict
candidate. Coefficients must be conservative engine/model-specific upper bounds
covering weights, KV/state, scratch/activation buffers, staging, thread resources
and runtime overhead for all settings permitted by that profile. A GGUF file size
or KV formula alone is insufficient. Models with nonlinear or unbounded workspace
needs must provide a proven upper bound for the allowed range or remain unknown.
Mmap/flash-attention flags never automatically subtract memory.

An allocation has stable identity, Host/Device locations and these terms:

`base + context_tokens * per_context + batch_tokens * per_batch + max(generation_threads, prefill_threads) * per_thread`

Every operation is checked for overflow. An allocation shared by Host and Device
is charged once for each distinct physical pool. Unified CPU/GPU RAM uses one pool
ID, so neither capacity nor the same allocation is counted twice. Discrete host/GPU
pools each need their own copy and budget. Distinct simultaneous buffers have
distinct allocation IDs even on unified RAM.

Every used pool must have known current total and available bytes. The allowed
budget is the minimum of these observations and applicable explicit budgets;
unified host/device caps intersect. Headroom is charged once per physical pool
(default1GiB), and resident/scratch estimates plus headroom must fit. Availability
is not reserved by this computation and can change before load. OS recommended
working-set size, physical memory and free/inactive counters must not be labelled
current available memory without a justified platform measurement.

## Settings and selection semantics

Implemented settings are context/output limits, generation/prefill threads,
batch/microbatch, exact profiled GPU layers/KV type, supported flash-attention,
mmap/mlock booleans, physical-pool budgets and headroom. Unsupported controls,
multi-GPU, arbitrary flags/env/paths and other settings have no mutation field.
Changing KV type or offload needs a separate exact memory profile; no guessed
scaling is used. Thread/context/batch defaults are bounded by actual declared
limits. Mmap defaults on only when declared supported; an explicit false override
is retained. The runtime adapter must apply either value through its qualified
interface. Explicit values are preserved or refused, never silently reduced.

Auto chooses only qualified estimated-fit candidates. Fast first prefers a GPU
class, Quiet a CPU class with a smaller default thread budget, and Balanced has no
class bias. Ties use estimated physical memory, adapter ID, device ID and profile
hash. This is a disclosed deterministic unmeasured policy, not a fastest/quietest
claim. Calibration ranking is a later integration gate. Manual mode requires an
exact backend/device and never substitutes another route. The local-fallback
preference is retained and hashed; this library performs no runtime fallback at
all. In particular there is no cloud endpoint/provider type or remote fallback.

## Verification and runnable fixture

```sh
cargo test --manifest-path crates/smith-acceleration/Cargo.toml --locked --offline
cargo clippy --manifest-path crates/smith-acceleration/Cargo.toml --locked --offline --all-targets -- -D warnings
cargo run --manifest-path crates/smith-acceleration/Cargo.toml --locked --offline --example synthetic_plan
```

The example emits complete synthetic inputs and output for integration. Its
hashes, qualified tuples and coefficients are fixtures, not real installed
adapter or model evidence. Tests cover unified/discrete accounting, unknown data,
overflow, unchanged manual overrides/fallback preferences, strict settings,
format/capability/vendor combinations, all seven backend identity routes,
generation/driver/model/device/config changes, deterministic selection and the
unvalidated preview boundary. No hardware, subprocess, model or network is used.
