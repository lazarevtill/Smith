use smith_acceleration::*;
#[path = "../examples/support/fixture.rs"]
mod fixture;
use fixture::{fixture, GIB};

fn selected(out: PlanningOutcome) -> EffectivePlan {
    match out {
        PlanningOutcome::Candidate { plan, .. } => plan,
        other => panic!("{other:?}"),
    }
}
fn blocked(out: PlanningOutcome, code: ReasonCode) {
    match out {
        PlanningOutcome::Blocked { reasons, .. } => {
            assert!(reasons.iter().any(|r| r.code == code), "{reasons:?}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unified_memory_counts_each_allocation_and_headroom_once() {
    let (i, a, m, intent) = fixture();
    let p = selected(plan(&i, &a, &m, &intent, 7).unwrap());
    assert_eq!(p.backend(), Backend::LlamaMetal);
    assert_eq!(p.memory().len(), 1);
    let expected = 5 * GIB + 2048 * 65536 + 128 * 65536 + 4 * 1024 * 1024;
    assert_eq!(p.memory()[0].required_bytes, expected);
    assert_eq!(p.memory()[0].headroom_bytes, GIB);
    assert_eq!(p.intent_sha256(), intent.fingerprint().unwrap());
    let json = serde_json::to_value(&p).unwrap();
    assert!(json.get("resident").is_none() && json.get("observed_backend").is_none());
}
#[test]
fn discrete_memory_must_fit_host_and_device_independently() {
    let (mut i, mut a, m, mut intent) = fixture();
    i.os = OperatingSystem::Linux;
    i.devices[1].vendor = Vendor::Nvidia;
    i.devices[1].memory_pool_id = "vram".into();
    i.memory_pools.push(MemoryPool {
        id: "vram".into(),
        total_bytes: Some(8 * GIB),
        available_bytes: Some(7 * GIB),
    });
    a[1].os = i.os;
    a[1].backend = Backend::LlamaCuda;
    intent.overrides.backend = Some(Backend::LlamaCuda);
    let p = selected(plan(&i, &a, &m, &intent, 0).unwrap());
    assert_eq!(p.memory().len(), 2);
    assert_eq!(
        p.memory()
            .iter()
            .find(|p| p.pool_id == "unified")
            .unwrap()
            .required_bytes,
        4 * GIB
    );
    i.memory_pools[0].available_bytes = Some(4 * GIB);
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::MemoryInsufficient,
    );
}
#[test]
fn unknown_and_absent_facts_never_invent_fit_or_installation() {
    let (mut i, mut a, m, intent) = fixture();
    blocked(
        plan(&i, &[], &m, &intent, 0).unwrap(),
        ReasonCode::NoAdapters,
    );
    for adapter in &mut a {
        adapter.availability = Availability::InstalledUnverified;
    }
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::Unavailable,
    );
    for adapter in &mut a {
        adapter.availability = Availability::Qualified;
    }
    i.memory_pools[0].available_bytes = None;
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::MemoryUnknown,
    );
    i.memory_pools[0].available_bytes = Some(12 * GIB);
    i.memory_pools[0].total_bytes = None;
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::MemoryUnknown,
    );
    i.memory_pools[0].total_bytes = Some(16 * GIB);
    for adapter in &mut a {
        adapter.models[0].memory = None;
    }
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::MemoryUnknown,
    );
}
#[test]
fn arithmetic_overflow_refuses_candidate_instead_of_wrapping() {
    let (i, mut a, m, intent) = fixture();
    for adapter in &mut a {
        adapter.models[0].memory.as_mut().unwrap().allocations[0].bytes_per_context_token =
            u64::MAX;
    }
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::ArithmeticOverflow,
    );
    for adapter in &mut a {
        let allocation = &mut adapter.models[0].memory.as_mut().unwrap().allocations[0];
        allocation.bytes_per_context_token = 0;
        allocation.base_bytes = u64::MAX;
    }
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::ArithmeticOverflow,
    );
}
#[test]
fn exact_overrides_survive_insufficient_memory_and_all_fallback_policies() {
    let (mut i, a, m, mut intent) = fixture();
    intent.mode = Mode::Manual;
    intent.overrides.backend = Some(Backend::LlamaMetal);
    intent.overrides.device_id = Some("metal".into());
    intent.overrides.context_tokens = Some(8192);
    intent.overrides.kv_type = Some(KvType::F16);
    i.memory_pools[0].available_bytes = Some(GIB);
    for policy in [
        LocalFallback::Never,
        LocalFallback::Ask,
        LocalFallback::AllowVerifiedLocal,
    ] {
        intent.local_fallback = policy;
        match plan(&i, &a, &m, &intent, 0).unwrap() {
            PlanningOutcome::Blocked {
                intent: retained, ..
            } => assert_eq!(retained, intent),
            other => panic!("{other:?}"),
        }
    }
    i.memory_pools[0].available_bytes = Some(12 * GIB);
    let p = selected(plan(&i, &a, &m, &intent, 0).unwrap());
    assert_eq!(p.settings().context_tokens, 8192);
    assert_eq!(p.backend(), Backend::LlamaMetal);
    assert_eq!(p.device_id(), "metal");
}
#[test]
fn strict_ranges_unknown_fields_and_cross_setting_limits_reject() {
    let (i, a, m, mut intent) = fixture();
    intent.overrides.context_tokens = Some(0);
    assert!(plan(&i, &a, &m, &intent, 0).is_err());
    intent.overrides.context_tokens = Some(16384);
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::SettingsUnsupported,
    );
    intent.overrides.context_tokens = Some(128);
    intent.overrides.output_tokens = Some(256);
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::SettingsUnsupported,
    );
    intent.overrides.output_tokens = Some(64);
    intent.overrides.flash_attention = Some(true);
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::SettingsUnsupported,
    );
    let mut json = serde_json::to_value(&intent).unwrap();
    json["shell_flags"] = serde_json::json!("--unsafe");
    assert!(serde_json::from_value::<OptimizationIntent>(json).is_err());
    let mut json = serde_json::to_value(&i).unwrap();
    json["devices"][0]["endpoint"] = serde_json::json!("https://example.invalid");
    assert!(serde_json::from_value::<HardwareInventory>(json).is_err());
}

#[test]
fn mmap_defaults_to_declared_support_but_an_explicit_false_pin_is_preserved() {
    let (i, mut a, m, mut intent) = fixture();
    assert!(
        selected(plan(&i, &a, &m, &intent, 0).unwrap())
            .settings()
            .mmap
    );
    intent.overrides.mmap = Some(false);
    assert!(
        !selected(plan(&i, &a, &m, &intent, 0).unwrap())
            .settings()
            .mmap
    );
    intent.overrides.mmap = None;
    for adapter in &mut a {
        adapter.settings.supports_mmap = false;
    }
    assert!(
        !selected(plan(&i, &a, &m, &intent, 0).unwrap())
            .settings()
            .mmap
    );
    intent.overrides.mmap = Some(true);
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::SettingsUnsupported,
    );
}
#[test]
fn model_format_capability_and_backend_vendor_are_not_interchangeable() {
    let (i, mut a, mut m, mut intent) = fixture();
    a.remove(0);
    a[0].backend = Backend::MlxMetal;
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::DeviceMismatch,
    );
    a[0].backend = Backend::LlamaCuda;
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::DeviceMismatch,
    );
    a[0].backend = Backend::LlamaMetal;
    intent.capability = Capability::Vision;
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::CapabilityUnsupported,
    );
    intent.capability = Capability::TextChat;
    m.bundle_sha256 = "d".repeat(64);
    blocked(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        ReasonCode::ModelUnsupported,
    );
}
#[test]
fn every_supported_backend_requires_a_matching_explicit_tuple() {
    for (backend, os, vendor, format, layers) in [
        (
            Backend::LlamaCpu,
            OperatingSystem::Macos,
            Vendor::Apple,
            ModelFormat::GgufBundle,
            Some(0),
        ),
        (
            Backend::LlamaMetal,
            OperatingSystem::Macos,
            Vendor::Apple,
            ModelFormat::GgufBundle,
            Some(32),
        ),
        (
            Backend::MlxMetal,
            OperatingSystem::Macos,
            Vendor::Apple,
            ModelFormat::MlxBundle,
            None,
        ),
        (
            Backend::LlamaCuda,
            OperatingSystem::Windows,
            Vendor::Nvidia,
            ModelFormat::GgufBundle,
            Some(32),
        ),
        (
            Backend::LlamaHip,
            OperatingSystem::Linux,
            Vendor::Amd,
            ModelFormat::GgufBundle,
            Some(32),
        ),
        (
            Backend::LlamaSycl,
            OperatingSystem::Windows,
            Vendor::Intel,
            ModelFormat::GgufBundle,
            Some(32),
        ),
        (
            Backend::LlamaVulkan,
            OperatingSystem::Linux,
            Vendor::Other,
            ModelFormat::GgufBundle,
            Some(32),
        ),
    ] {
        let (mut i, mut a, mut m, mut intent) = fixture();
        let index = usize::from(backend != Backend::LlamaCpu);
        let mut adapter = a.remove(index);
        i.os = os;
        i.devices[index].vendor = vendor;
        adapter.os = os;
        adapter.backend = backend;
        m.format = format;
        adapter.models[0].format = format;
        adapter.models[0].gpu_layers = layers;
        intent.overrides.backend = Some(backend);
        assert_eq!(
            selected(plan(&i, &[adapter], &m, &intent, 0).unwrap()).backend(),
            backend
        );
    }
}
#[test]
fn generation_driver_model_device_config_and_intent_changes_invalidate_plan() {
    let (i, a, m, intent) = fixture();
    let p = selected(plan(&i, &a, &m, &intent, 7).unwrap());
    assert!(revalidate(&p, &i, &a, &m, &intent, 7).unwrap());
    assert!(!revalidate(&p, &i, &a, &m, &intent, 8).unwrap());
    let mut changed = i.clone();
    changed.generation = "new-generation".into();
    assert!(!revalidate(&p, &changed, &a, &m, &intent, 7).unwrap());
    changed = i.clone();
    changed.devices[1].driver_version = Some("different".into());
    assert!(!revalidate(&p, &changed, &a, &m, &intent, 7).unwrap());
    changed = i.clone();
    changed.devices[1].id = "replacement".into();
    assert!(!revalidate(&p, &changed, &a, &m, &intent, 7).unwrap());
    let mut model = m.clone();
    model.bundle_sha256 = "d".repeat(64);
    assert!(!revalidate(&p, &i, &a, &model, &intent, 7).unwrap());
    let mut adapters = a.clone();
    adapters[1].bundle_sha256 = Some("e".repeat(64));
    assert!(!revalidate(&p, &i, &adapters, &m, &intent, 7).unwrap());
    let mut choice = intent.clone();
    choice.overrides.threads = Some(2);
    assert!(!revalidate(&p, &i, &a, &m, &choice, 7).unwrap());
}
#[test]
fn selection_is_deterministic_and_preferences_are_explicit_not_measurements() {
    let (mut i, mut a, m, mut intent) = fixture();
    let first = selected(plan(&i, &a, &m, &intent, 0).unwrap());
    i.devices.reverse();
    a.reverse();
    assert_eq!(first, selected(plan(&i, &a, &m, &intent, 0).unwrap()));
    intent.preference = Preference::Quiet;
    let cpu = selected(plan(&i, &a, &m, &intent, 0).unwrap());
    assert_eq!(cpu.backend(), Backend::LlamaCpu);
    assert_eq!(cpu.settings().threads, 2);
    assert_eq!(cpu.id().len(), 64);
}
#[test]
fn preview_is_unvalidated_even_if_structural_defaults_are_available() {
    let (mut i, mut a, m, intent) = fixture();
    a[0].availability = Availability::InstalledUnverified;
    a[0].bundle_sha256 = None;
    a[0].selected_executable_sha256 = Some("f".repeat(64));
    a[0].qualified_devices.clear();
    a[0].models[0].memory = None;
    i.memory_pools[0].available_bytes = None;
    let outcome = preview(&i, &a[0], &m, &intent, 5).unwrap();
    match outcome {
        PreviewOutcome::Preview { preview, .. } => {
            assert_eq!(preview.qualification, PreviewQualification::Unvalidated);
            assert_eq!(preview.fit, PreviewFit::Unknown);
            assert_eq!(
                preview.identity.scope,
                ArtifactIdentityScope::SelectedExecutable
            );
            assert_eq!(preview.identity.sha256, "f".repeat(64));
            assert_eq!(preview.adapter_bundle_sha256, None);
            assert_eq!(preview.settings.threads, 4);
            assert_eq!(preview.settings.context_tokens, 2048);
            assert!(serde_json::to_value(preview).unwrap().get("id").is_none());
        }
        other => panic!("{other:?}"),
    }
    blocked(
        plan(&i, &a[..1], &m, &intent, 5).unwrap(),
        ReasonCode::Unavailable,
    );
    a[0].availability = Availability::Qualified;
    assert!(plan(&i, &a[..1], &m, &intent, 5).is_err());
    a[0].availability = Availability::NotInstalled;
    assert!(matches!(
        preview(&i, &a[0], &m, &intent, 5).unwrap(),
        PreviewOutcome::Blocked { .. }
    ));
}
#[test]
fn duplicate_devices_invalid_pools_and_unqualified_manual_overrides_fail_closed() {
    let (mut i, a, m, mut intent) = fixture();
    i.devices.push(i.devices[0].clone());
    assert!(i.validate().is_err());
    i.devices.pop();
    i.memory_pools[0].available_bytes = Some(17 * GIB);
    assert!(i.validate().is_err());
    i.memory_pools[0].available_bytes = Some(12 * GIB);
    intent.mode = Mode::Manual;
    assert!(intent.validate().is_err());
    intent.overrides.backend = Some(Backend::LlamaCuda);
    intent.overrides.device_id = Some("metal".into());
    assert!(matches!(
        plan(&i, &a, &m, &intent, 0).unwrap(),
        PlanningOutcome::Blocked { .. }
    ));
}
