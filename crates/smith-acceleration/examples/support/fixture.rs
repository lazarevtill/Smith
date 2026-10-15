//! Synthetic facts only. These hashes/device names identify no real runtime.
use smith_acceleration::*;
pub const GIB: u64 = 1024 * 1024 * 1024;
pub fn fixture() -> (
    HardwareInventory,
    Vec<AdapterDescriptor>,
    ModelDescriptor,
    OptimizationIntent,
) {
    let inventory = HardwareInventory {
        generation: "synthetic-1".into(),
        os: OperatingSystem::Macos,
        architecture: Architecture::Aarch64,
        os_version: "synthetic-os-1".into(),
        logical_cpu_count: 8,
        host_memory_pool_id: "unified".into(),
        memory_pools: vec![MemoryPool {
            id: "unified".into(),
            total_bytes: Some(16 * GIB),
            available_bytes: Some(12 * GIB),
        }],
        devices: vec![
            Device {
                id: "cpu".into(),
                class: DeviceClass::Cpu,
                vendor: Vendor::Apple,
                memory_pool_id: "unified".into(),
                driver_version: None,
            },
            Device {
                id: "metal".into(),
                class: DeviceClass::Gpu,
                vendor: Vendor::Apple,
                memory_pool_id: "unified".into(),
                driver_version: Some("synthetic-driver".into()),
            },
        ],
    };
    let model = ModelDescriptor {
        id: "synthetic-model".into(),
        bundle_sha256: "a".repeat(64),
        format: ModelFormat::GgufBundle,
        architecture: "synthetic-transformer".into(),
        quantization: "q4".into(),
        capabilities: vec![Capability::TextChat],
        native_context_limit: 8192,
    };
    let profile = ModelProfile {
        model_bundle_sha256: model.bundle_sha256.clone(),
        format: model.format,
        architecture: model.architecture.clone(),
        quantization: model.quantization.clone(),
        kv_type: KvType::F16,
        gpu_layers: Some(0),
        memory: Some(MemoryModel {
            provenance: "synthetic upper bounds; never a real model estimate".into(),
            allocations: vec![
                MemoryAllocation {
                    id: "weights".into(),
                    locations: vec![MemoryLocation::Host, MemoryLocation::Device],
                    base_bytes: 4 * GIB,
                    bytes_per_context_token: 0,
                    bytes_per_batch_token: 0,
                    bytes_per_thread: 0,
                },
                MemoryAllocation {
                    id: "kv-scratch-overhead".into(),
                    locations: vec![MemoryLocation::Device],
                    base_bytes: GIB,
                    bytes_per_context_token: 65536,
                    bytes_per_batch_token: 65536,
                    bytes_per_thread: 1024 * 1024,
                },
            ],
        }),
    };
    let mut cpu = AdapterDescriptor {
        id: "synthetic-cpu".into(),
        backend: Backend::LlamaCpu,
        os: inventory.os,
        architecture: inventory.architecture,
        version: "synthetic-v1".into(),
        bundle_sha256: Some("b".repeat(64)),
        selected_executable_sha256: None,
        availability: Availability::Qualified,
        capabilities: vec![Capability::TextChat],
        qualified_devices: vec![QualifiedDevice {
            device_id: "cpu".into(),
            os_version: inventory.os_version.clone(),
            driver_version: None,
        }],
        settings: SettingsLimits {
            max_context_tokens: 8192,
            max_output_tokens: 4096,
            max_threads: 8,
            max_batch_tokens: 512,
            max_microbatch_tokens: 128,
            supports_flash_attention: false,
            supports_mmap: true,
            supports_mlock: false,
        },
        models: vec![profile],
    };
    let mut metal = cpu.clone();
    metal.id = "synthetic-metal".into();
    metal.backend = Backend::LlamaMetal;
    metal.bundle_sha256 = Some("c".repeat(64));
    metal.qualified_devices = vec![QualifiedDevice {
        device_id: "metal".into(),
        os_version: inventory.os_version.clone(),
        driver_version: Some("synthetic-driver".into()),
    }];
    metal.models[0].gpu_layers = Some(32);
    cpu.models[0].memory.as_mut().unwrap().allocations[0].locations = vec![MemoryLocation::Host];
    let intent = OptimizationIntent {
        mode: Mode::Auto,
        preference: Preference::Fast,
        capability: Capability::TextChat,
        local_fallback: LocalFallback::Never,
        overrides: Overrides::default(),
    };
    (inventory, vec![cpu, metal], model, intent)
}
