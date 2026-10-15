use crate::*;
use std::collections::{BTreeMap, BTreeSet};

type CandidateKey = (u8, u64, String, String, String);
type Failure = (ReasonCode, &'static str);
const GIB: u64 = 1024 * 1024 * 1024;

/// Structural advice for one explicitly selected, inspected local adapter.
/// The host must have populated supported settings/model facts from actual
/// metadata/CLI admission. This never promotes those facts to qualified fit.
pub fn preview(
    inventory: &HardwareInventory,
    adapter: &AdapterDescriptor,
    model: &ModelDescriptor,
    intent: &OptimizationIntent,
    config_revision: u64,
) -> Result<PreviewOutcome, ValidationError> {
    inventory.validate()?;
    adapter.validate()?;
    model.validate()?;
    intent.validate()?;
    let mut reasons = Vec::new();
    let mut omitted = 0;
    let blocked = |reasons| PreviewOutcome::Blocked {
        intent: intent.clone(),
        reasons,
    };
    let identity = adapter
        .bundle_sha256
        .as_ref()
        .map(|sha256| PreviewArtifactIdentity {
            scope: ArtifactIdentityScope::CompleteBundle,
            sha256: sha256.clone(),
        })
        .or_else(|| {
            adapter
                .selected_executable_sha256
                .as_ref()
                .map(|sha256| PreviewArtifactIdentity {
                    scope: ArtifactIdentityScope::SelectedExecutable,
                    sha256: sha256.clone(),
                })
        });
    if adapter.availability == Availability::NotInstalled || identity.is_none() {
        reason(
            &mut reasons,
            &mut omitted,
            Some(adapter),
            None,
            (
                ReasonCode::Unavailable,
                "Preview requires an explicitly inspected installed adapter identity.",
            ),
        );
        return Ok(blocked(reasons));
    }
    if adapter.os != inventory.os || adapter.architecture != inventory.architecture {
        reason(
            &mut reasons,
            &mut omitted,
            Some(adapter),
            None,
            (
                ReasonCode::PlatformMismatch,
                "Adapter OS/architecture does not match.",
            ),
        );
        return Ok(blocked(reasons));
    }
    if !adapter.capabilities.contains(&intent.capability)
        || !model.capabilities.contains(&intent.capability)
    {
        reason(
            &mut reasons,
            &mut omitted,
            Some(adapter),
            None,
            (
                ReasonCode::CapabilityUnsupported,
                "Requested capability is not declared by both adapter and model.",
            ),
        );
        return Ok(blocked(reasons));
    }
    if intent
        .overrides
        .backend
        .is_some_and(|b| b != adapter.backend)
    {
        reason(
            &mut reasons,
            &mut omitted,
            Some(adapter),
            None,
            (
                ReasonCode::OverrideUnsatisfied,
                "Pinned backend was not changed for preview.",
            ),
        );
        return Ok(blocked(reasons));
    }
    let mut devices: Vec<_> = inventory
        .devices
        .iter()
        .filter(|d| {
            intent
                .overrides
                .device_id
                .as_deref()
                .is_none_or(|id| id == d.id)
                && backend_matches(adapter.backend, inventory, d, model.format)
        })
        .collect();
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    let mut profiles: Vec<_> = adapter
        .models
        .iter()
        .filter(|p| {
            p.model_bundle_sha256 == model.bundle_sha256
                && p.format == model.format
                && p.architecture == model.architecture
                && p.quantization == model.quantization
        })
        .collect();
    profiles.sort_by_key(|p| (p.kv_type, p.gpu_layers));
    for d in devices {
        for p in &profiles {
            match resolve_settings(inventory, adapter, model, p, intent) {
                Ok(settings) => {
                    reason(&mut reasons,&mut omitted,Some(adapter),Some(d),(ReasonCode::Unavailable,"Structural settings preview only: execution qualification is unvalidated. Normal explicit runtime admission remains separate."));
                    reason(&mut reasons,&mut omitted,Some(adapter),Some(d),(ReasonCode::MemoryUnknown,"Memory fit is unknown; no scratch/staging upper bound or successful calibration is inferred."));
                    return Ok(PreviewOutcome::Preview {
                        preview: SettingsPreview {
                            inventory_generation: inventory.generation.clone(),
                            config_revision,
                            adapter_id: adapter.id.clone(),
                            adapter_bundle_sha256: adapter.bundle_sha256.clone(),
                            identity: identity.clone().expect("checked inspected identity"),
                            model_bundle_sha256: model.bundle_sha256.clone(),
                            backend: adapter.backend,
                            device_id: d.id.clone(),
                            settings,
                            qualification: PreviewQualification::Unvalidated,
                            fit: PreviewFit::Unknown,
                        },
                        intent: intent.clone(),
                        reasons,
                    });
                }
                Err(f) => reason(&mut reasons, &mut omitted, Some(adapter), Some(d), f),
            }
        }
    }
    reason(&mut reasons,&mut omitted,Some(adapter),None,(ReasonCode::ModelUnsupported,"No structurally compatible device/model/settings preview; pinned overrides remain unchanged."));
    Ok(blocked(reasons))
}

/// Compute a candidate from explicit qualified local inputs. No source probing,
/// downloads, execution, fallback to cloud, or observed-residency claims occur.
pub fn plan(
    inventory: &HardwareInventory,
    adapters: &[AdapterDescriptor],
    model: &ModelDescriptor,
    intent: &OptimizationIntent,
    config_revision: u64,
) -> Result<PlanningOutcome, ValidationError> {
    inventory.validate()?;
    model.validate()?;
    intent.validate()?;
    if adapters.len() > 32 {
        return Err(ValidationError::new("adapters.count"));
    }
    let mut ids = BTreeSet::new();
    for a in adapters {
        a.validate()?;
        if !ids.insert(&a.id) {
            return Err(ValidationError::new("adapters.duplicate_id"));
        }
    }
    let inventory_hash = inventory.fingerprint()?;
    let model_hash = hash(model)?;
    let intent_hash = hash(intent)?;
    let mut reasons = Vec::new();
    let mut omitted = 0usize;
    let mut best: Option<(CandidateKey, EffectivePlan)> = None;
    let mut ordered: Vec<_> = adapters.iter().collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));
    let mut devices: Vec<_> = inventory.devices.iter().collect();
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    if adapters.is_empty() {
        reason(
            &mut reasons,
            &mut omitted,
            None,
            None,
            (
                ReasonCode::NoAdapters,
                "No installed adapter facts were supplied.",
            ),
        );
    }
    for a in ordered {
        let failure = if a.availability != Availability::Qualified {
            Some((
                ReasonCode::Unavailable,
                "Adapter is absent or has not been qualified by the host.",
            ))
        } else if a.os != inventory.os || a.architecture != inventory.architecture {
            Some((
                ReasonCode::PlatformMismatch,
                "Adapter OS or architecture differs from this inventory.",
            ))
        } else if intent.overrides.backend.is_some_and(|b| b != a.backend) {
            Some((
                ReasonCode::OverrideUnsatisfied,
                "Pinned backend is preserved; this adapter is not selected.",
            ))
        } else if !a.capabilities.contains(&intent.capability)
            || !model.capabilities.contains(&intent.capability)
        {
            Some((
                ReasonCode::CapabilityUnsupported,
                "Both the exact model and adapter must declare the requested capability.",
            ))
        } else {
            None
        };
        if let Some(f) = failure {
            reason(&mut reasons, &mut omitted, Some(a), None, f);
            continue;
        }
        let adapter_hash = hash(a)?;
        let mut profiles: Vec<_> = a
            .models
            .iter()
            .filter(|p| {
                p.model_bundle_sha256 == model.bundle_sha256
                    && p.format == model.format
                    && p.architecture == model.architecture
                    && p.quantization == model.quantization
            })
            .collect();
        profiles.sort_by_key(|p| (p.kv_type, p.gpu_layers));
        if profiles.is_empty() {
            reason(&mut reasons,&mut omitted,Some(a),None,(ReasonCode::ModelUnsupported,"No qualified memory/settings profile exists for this exact model bundle and format."));
            continue;
        }
        if devices.is_empty() {
            reason(
                &mut reasons,
                &mut omitted,
                Some(a),
                None,
                (
                    ReasonCode::DeviceMismatch,
                    "No device observations were supplied.",
                ),
            );
        }
        for d in &devices {
            if intent
                .overrides
                .device_id
                .as_deref()
                .is_some_and(|id| id != d.id)
            {
                reason(
                    &mut reasons,
                    &mut omitted,
                    Some(a),
                    Some(d),
                    (
                        ReasonCode::OverrideUnsatisfied,
                        "Pinned device is preserved; another device cannot substitute.",
                    ),
                );
                continue;
            }
            if !backend_matches(a.backend, inventory, d, model.format) {
                reason(
                    &mut reasons,
                    &mut omitted,
                    Some(a),
                    Some(d),
                    (
                        ReasonCode::DeviceMismatch,
                        "Backend/device/OS/model-format combination is unsupported.",
                    ),
                );
                continue;
            }
            if !a.qualified_devices.iter().any(|q| {
                q.device_id == d.id
                    && q.os_version == inventory.os_version
                    && q.driver_version == d.driver_version
            }) {
                reason(
                    &mut reasons,
                    &mut omitted,
                    Some(a),
                    Some(d),
                    (
                        ReasonCode::QualificationChanged,
                        "Exact device, OS version and driver qualification is absent or changed.",
                    ),
                );
                continue;
            }
            for p in &profiles {
                let result =
                    resolve_settings(inventory, a, model, p, intent).and_then(|settings| {
                        estimate(inventory, d, p, &settings, intent)
                            .map(|memory| (settings, memory))
                    });
                let (settings, memory) = match result {
                    Ok(value) => value,
                    Err(f) => {
                        reason(&mut reasons, &mut omitted, Some(a), Some(d), f);
                        continue;
                    }
                };
                let profile_hash = hash(p)?;
                let mut candidate = EffectivePlan {
                    id: String::new(),
                    inventory_generation: inventory.generation.clone(),
                    inventory_sha256: inventory_hash.clone(),
                    adapter_sha256: adapter_hash.clone(),
                    model_sha256: model_hash.clone(),
                    intent_sha256: intent_hash.clone(),
                    config_revision,
                    adapter_id: a.id.clone(),
                    adapter_bundle_sha256: a
                        .bundle_sha256
                        .clone()
                        .expect("validated qualified bundle"),
                    model_bundle_sha256: model.bundle_sha256.clone(),
                    backend: a.backend,
                    device_id: d.id.clone(),
                    settings,
                    memory,
                    profile_sha256: profile_hash.clone(),
                };
                candidate.id = hash(&candidate)?;
                let class_rank = match intent.preference {
                    Preference::Balanced => 0,
                    Preference::Fast => u8::from(d.class != DeviceClass::Gpu),
                    Preference::Quiet => u8::from(d.class != DeviceClass::Cpu),
                };
                // Inventory pool limits are <=2^53 and there are <=32 pools.
                let peak = candidate.memory.iter().map(|m| m.required_bytes).sum();
                let key = (class_rank, peak, a.id.clone(), d.id.clone(), profile_hash);
                if best.as_ref().is_none_or(|(old, _)| &key < old) {
                    best = Some((key, candidate));
                }
            }
        }
    }
    if omitted > 0 {
        reasons.push(Reason {
            code: ReasonCode::AdditionalCandidatesRejected,
            adapter_id: None,
            device_id: None,
            message: format!(
                "{omitted} additional rejection diagnostics exceeded the 256-record output bound."
            ),
        });
    }
    if let Some((_, plan)) = best {
        reasons.push(Reason { code:ReasonCode::DeterministicUnmeasuredChoice,adapter_id:Some(plan.adapter_id.clone()),device_id:Some(plan.device_id.clone()),
            message:"Selected a qualified estimated-fit local candidate by declared preference, memory, then stable IDs. No latency, energy, loaded-state or actual device execution is inferred. Pinned overrides were retained.".into() });
        Ok(PlanningOutcome::Candidate {
            plan,
            intent: intent.clone(),
            reasons,
        })
    } else {
        Ok(PlanningOutcome::Blocked {
            intent: intent.clone(),
            reasons,
        })
    }
}

/// Recompute against fresh facts and the exact settings revision. A true result
/// only says planning inputs still agree; separate process/consent admission is
/// required, and current free memory can change after this check.
pub fn revalidate(
    previous: &EffectivePlan,
    inventory: &HardwareInventory,
    adapters: &[AdapterDescriptor],
    model: &ModelDescriptor,
    intent: &OptimizationIntent,
    config_revision: u64,
) -> Result<bool, ValidationError> {
    Ok(
        matches!(plan(inventory,adapters,model,intent,config_revision)?, PlanningOutcome::Candidate { plan,.. } if plan == *previous),
    )
}

fn reason(
    reasons: &mut Vec<Reason>,
    omitted: &mut usize,
    a: Option<&AdapterDescriptor>,
    d: Option<&Device>,
    f: Failure,
) {
    let adapter_id = a.map(|v| v.id.clone());
    let device_id = d.map(|v| v.id.clone());
    if reasons
        .iter()
        .any(|r| r.code == f.0 && r.adapter_id == adapter_id && r.device_id == device_id)
    {
        return;
    }
    if reasons.len() >= 256 {
        *omitted += 1;
        return;
    }
    reasons.push(Reason {
        code: f.0,
        adapter_id,
        device_id,
        message: f.1.into(),
    });
}

fn backend_matches(b: Backend, i: &HardwareInventory, d: &Device, format: ModelFormat) -> bool {
    use Backend::*;
    if b == MlxMetal {
        return format == ModelFormat::MlxBundle
            && i.os == OperatingSystem::Macos
            && i.architecture == Architecture::Aarch64
            && d.vendor == Vendor::Apple
            && d.class == DeviceClass::Gpu;
    }
    if format != ModelFormat::GgufBundle {
        return false;
    }
    match b {
        LlamaCpu => d.class == DeviceClass::Cpu,
        LlamaMetal => d.class == DeviceClass::Gpu && i.os == OperatingSystem::Macos,
        LlamaCuda => {
            d.class == DeviceClass::Gpu
                && d.vendor == Vendor::Nvidia
                && i.os != OperatingSystem::Macos
        }
        LlamaHip => {
            d.class == DeviceClass::Gpu && d.vendor == Vendor::Amd && i.os != OperatingSystem::Macos
        }
        LlamaSycl => {
            d.class == DeviceClass::Gpu
                && d.vendor == Vendor::Intel
                && i.os != OperatingSystem::Macos
        }
        LlamaVulkan => d.class == DeviceClass::Gpu && i.os != OperatingSystem::Macos,
        MlxMetal => false,
    }
}

fn resolve_settings(
    i: &HardwareInventory,
    a: &AdapterDescriptor,
    m: &ModelDescriptor,
    p: &ModelProfile,
    intent: &OptimizationIntent,
) -> Result<EffectiveSettings, Failure> {
    let o = &intent.overrides;
    let l = &a.settings;
    let unsupported=(ReasonCode::SettingsUnsupported,"Requested settings exceed qualified limits or require an unavailable implementation; overrides were not changed.");
    if o.kv_type.is_some_and(|v| v != p.kv_type)
        || o.gpu_layers.is_some_and(|v| Some(v) != p.gpu_layers)
    {
        return Err(unsupported);
    }
    if (a.backend == Backend::LlamaCpu && p.gpu_layers != Some(0))
        || (a.backend == Backend::MlxMetal && p.gpu_layers.is_some())
        || (!matches!(a.backend, Backend::LlamaCpu | Backend::MlxMetal)
            && p.gpu_layers.is_none_or(|n| n == 0))
    {
        return Err(unsupported);
    }
    let context = o
        .context_tokens
        .unwrap_or(2048.min(l.max_context_tokens).min(m.native_context_limit));
    let threads_default = if intent.preference == Preference::Quiet {
        2
    } else {
        4
    };
    let threads = o.threads.unwrap_or(
        threads_default
            .min(i.logical_cpu_count.saturating_sub(1).max(1))
            .min(l.max_threads),
    );
    let prefill_threads = o.prefill_threads.unwrap_or(threads);
    let batch = o
        .batch_tokens
        .unwrap_or(128.min(l.max_batch_tokens).min(context));
    let microbatch = o
        .microbatch_tokens
        .unwrap_or(64.min(l.max_microbatch_tokens).min(batch));
    let output = o
        .output_tokens
        .unwrap_or(256.min(l.max_output_tokens).min(context));
    let flash = o.flash_attention.unwrap_or(false);
    let mmap = o.mmap.unwrap_or(l.supports_mmap);
    let mlock = o.mlock.unwrap_or(false);
    if context > l.max_context_tokens
        || context > m.native_context_limit
        || output > context
        || output > l.max_output_tokens
        || threads > l.max_threads
        || threads > i.logical_cpu_count
        || prefill_threads > l.max_threads
        || prefill_threads > i.logical_cpu_count
        || batch > context
        || batch > l.max_batch_tokens
        || microbatch > batch
        || microbatch > l.max_microbatch_tokens
        || (flash && !l.supports_flash_attention)
        || (mmap && !l.supports_mmap)
        || (mlock && !l.supports_mlock)
    {
        return Err(unsupported);
    }
    Ok(EffectiveSettings {
        context_tokens: context,
        output_tokens: output,
        threads,
        prefill_threads,
        batch_tokens: batch,
        microbatch_tokens: microbatch,
        gpu_layers: p.gpu_layers,
        kv_type: p.kv_type,
        flash_attention: flash,
        mmap,
        mlock,
        headroom_bytes: o.headroom_bytes.unwrap_or(GIB),
    })
}

fn estimate(
    i: &HardwareInventory,
    d: &Device,
    p: &ModelProfile,
    s: &EffectiveSettings,
    intent: &OptimizationIntent,
) -> Result<Vec<PoolEstimate>, Failure> {
    let unknown=(ReasonCode::MemoryUnknown,"Exact model memory coefficients or current physical-pool availability are unknown; fit is not claimed.");
    let overflow = (
        ReasonCode::ArithmeticOverflow,
        "Memory arithmetic overflowed; this candidate is refused.",
    );
    let model = p.memory.as_ref().ok_or(unknown)?;
    if d.class == DeviceClass::Gpu
        && !model
            .allocations
            .iter()
            .any(|a| a.locations.contains(&MemoryLocation::Device))
    {
        return Err(unknown);
    }
    let mut required: BTreeMap<String, u64> = BTreeMap::new();
    for a in &model.allocations {
        let bytes = a
            .bytes_per_context_token
            .checked_mul(u64::from(s.context_tokens))
            .and_then(|v| {
                a.bytes_per_batch_token
                    .checked_mul(u64::from(s.batch_tokens))
                    .and_then(|b| v.checked_add(b))
            })
            .and_then(|v| {
                a.bytes_per_thread
                    .checked_mul(u64::from(s.threads.max(s.prefill_threads)))
                    .and_then(|t| v.checked_add(t))
            })
            .and_then(|v| v.checked_add(a.base_bytes))
            .ok_or(overflow)?;
        let pools: BTreeSet<_> = a
            .locations
            .iter()
            .map(|location| match location {
                MemoryLocation::Host => &i.host_memory_pool_id,
                MemoryLocation::Device => &d.memory_pool_id,
            })
            .collect();
        for pool in pools {
            let current = required.entry(pool.clone()).or_default();
            *current = current.checked_add(bytes).ok_or(overflow)?;
        }
    }
    let mut result = Vec::new();
    for (id, bytes) in required {
        let pool = i
            .memory_pools
            .iter()
            .find(|v| v.id == id)
            .expect("validated pool reference");
        let total = pool.total_bytes.ok_or(unknown)?;
        let available = pool.available_bytes.ok_or(unknown)?;
        let mut permitted = available.min(total);
        if id == i.host_memory_pool_id {
            permitted = permitted.min(intent.overrides.host_budget_bytes.unwrap_or(permitted));
        }
        if id == d.memory_pool_id {
            permitted = permitted.min(intent.overrides.device_budget_bytes.unwrap_or(permitted));
        }
        if bytes.checked_add(s.headroom_bytes).ok_or(overflow)? > permitted {
            return Err((ReasonCode::MemoryInsufficient,"Resident, scratch/staging and headroom estimates exceed a current physical-pool budget; context and precision were not reduced."));
        }
        result.push(PoolEstimate {
            pool_id: id,
            required_bytes: bytes,
            headroom_bytes: s.headroom_bytes,
            available_bytes: available,
            permitted_bytes: permitted,
        });
    }
    Ok(result)
}
