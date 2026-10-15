//! Include as a cfg(test) child of acceleration_api. Hardware is synthetic;
//! only the repository's fixed --help/--list-devices fixture is executed.
//! No hardware observation, inference, network peer or user profile is used.
#![cfg(any(target_os = "macos", target_os = "linux"))]
use super::*;
use std::path::Path;

struct ProbeFixture {
    _directory: tempfile::TempDir,
    selected: runtime_catalog::CatalogSelection,
    report: runtime::RuntimeProbeReport,
}

static FULL: tokio::sync::OnceCell<ProbeFixture> = tokio::sync::OnceCell::const_new();
static LIMITED: tokio::sync::OnceCell<ProbeFixture> = tokio::sync::OnceCell::const_new();

async fn fixture(limited: bool) -> &'static ProbeFixture {
    let cell = if limited { &LIMITED } else { &FULL };
    cell.get_or_init(|| async move {
        let directory = tokio::task::spawn_blocking(move || {
            let directory = tempfile::tempdir().unwrap();
            let mut source =
                include_str!("../../crates/smith-managed-runtime/tests/fixtures/runtime_probe.rs")
                    .to_owned();
            if limited {
                // Retain --ubatch-size without --batch-size: a microbatch pin
                // must be refused when its companion flag cannot be applied.
                for flag in [
                    "--threads-batch N\\n",
                    "--batch-size N\\n",
                    "--flash-attn [on|off|auto]\\n",
                    "--load-mode MODE none mmap mlock mmap+mlock",
                ] {
                    assert_eq!(source.matches(flag).count(), 1);
                    source = source.replace(flag, "");
                }
            }
            let path = directory.path().join("probe.rs");
            std::fs::write(&path, source).unwrap();
            let stderr_path = directory.path().join("compiler.stderr");
            let mut compiler = std::process::Command::new(
                std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
            )
            .arg(&path)
            .args(["--edition=2021", "-o"])
            .arg(directory.path().join("synthetic-advice-probe"))
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .spawn()
            .expect("build the owned fixed probe fixture with the test toolchain");
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            loop {
                if let Some(status) = compiler.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "fixture compilation: {}",
                        std::fs::read_to_string(stderr_path).unwrap()
                    );
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    compiler.kill().unwrap();
                    compiler.wait().unwrap();
                    panic!("fixed fixture compiler exceeded 60 seconds and was reaped");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            std::fs::write(directory.path().join("metadata-only.gguf"), metadata()).unwrap();
            directory
        })
        .await
        .unwrap();
        let selected = runtime_catalog::inspect_selection(
            selected_file(directory.path().join("synthetic-advice-probe")),
            selected_file(directory.path().join("metadata-only.gguf")),
            &runtime::Cancellation::new(),
        )
        .await
        .unwrap();
        let plan = runtime::RuntimeProbePlan::prepare(
            selected.executable.clone(),
            &runtime::Cancellation::new(),
        )
        .await
        .unwrap();
        assert_eq!(plan.summary().commands, ["--help", "--list-devices"]);
        let id = plan.summary().id.clone();
        let mut probe =
            runtime::RuntimeProbe::start(plan.approve(&id).unwrap(), runtime::Cancellation::new());
        let report = probe.wait().await.unwrap();
        assert!(report.help.reaped && report.inventory.reaped);
        assert!(report.compatibility.protocol_candidate());
        assert!(!report.inference_verified);
        ProbeFixture {
            _directory: directory,
            selected,
            report,
        }
    })
    .await
}

fn selected_file(path: PathBuf) -> runtime_catalog::SelectedFileInput {
    runtime_catalog::SelectedFileInput {
        path: path.to_str().unwrap().into(),
        source: "owned synthetic review fixture".into(),
        source_revision: "fixture-v1".into(),
    }
}

fn metadata() -> Vec<u8> {
    fn string(bytes: &mut Vec<u8>, value: &str) {
        bytes.extend((value.len() as u64).to_le_bytes());
        bytes.extend(value.as_bytes());
    }
    let mut bytes = b"GGUF".to_vec();
    bytes.extend(3u32.to_le_bytes());
    bytes.extend(0u64.to_le_bytes()); // No tensors and never loaded for inference.
    bytes.extend(3u64.to_le_bytes());
    string(&mut bytes, "general.architecture");
    bytes.extend(8u32.to_le_bytes());
    string(&mut bytes, "llama");
    for (key, value) in [("llama.context_length", 4096u32), ("llama.block_count", 2)] {
        string(&mut bytes, key);
        bytes.extend(4u32.to_le_bytes());
        bytes.extend(value.to_le_bytes());
    }
    bytes
}

fn observation() -> hardware_inventory::Observation {
    use planning::*;
    hardware_inventory::Observation {
        platform_evidence: None,
        schema_version: 1,
        source: "synthetic_review_no_os_query",
        observed_unix_ms: 1,
        inventory: HardwareInventory {
            generation: "synthetic-generation".into(),
            os: OperatingSystem::Macos,
            architecture: Architecture::Aarch64,
            os_version: "synthetic-os".into(),
            logical_cpu_count: 8,
            host_memory_pool_id: "unified-pool".into(),
            memory_pools: vec![MemoryPool {
                id: "unified-pool".into(),
                total_bytes: Some(16 << 30),
                available_bytes: None,
            }],
            devices: vec![
                Device {
                    id: "synthetic-cpu".into(),
                    class: DeviceClass::Cpu,
                    vendor: Vendor::Apple,
                    memory_pool_id: "unified-pool".into(),
                    driver_version: None,
                },
                Device {
                    id: "synthetic-metal".into(),
                    class: DeviceClass::Gpu,
                    vendor: Vendor::Apple,
                    memory_pool_id: "unified-pool".into(),
                    driver_version: None,
                },
            ],
        },
        process_architecture: "aarch64",
        physical_cpu_count: Some(8),
        active_cpu_count: Some(8),
        identity_scope: hardware_inventory::IdentityScope::ProcessLifetime,
        metal_devices: vec![hardware_inventory::MetalEvidence {
            device_id: "synthetic-metal".into(),
            name: Some("Synthetic Metal".into()),
            unified_memory: Some(true),
            low_power: Some(true),
            removable: Some(false),
            recommended_working_set_bytes: Some(12 << 30),
            maximum_single_buffer_bytes: Some(8 << 30),
            apple_gpu_family: Some(true),
            mac_gpu_family_2: Some(true),
        }],
        memory_sample: Some(hardware_inventory::VmMemorySample {
            observed_unix_ms: 1,
            source: "synthetic_free_pages",
            page_size_bytes: 16384,
            free_including_speculative_bytes: 4 << 30,
            speculative_bytes: 1 << 30,
            inactive_bytes: 2 << 30,
            purgeable_bytes: 1 << 30,
            compressed_bytes: 1 << 30,
            conservative_available_bytes: 3 << 30,
        }),
        limitations: vec!["Synthetic inventory; no runtime placement evidence"],
        backend_runtime_tested: false,
        model_placement_observed: false,
    }
}

fn preferences_for(
    preference: planning::Preference,
    overrides: Value,
) -> runtime_preferences::Preferences {
    let mut result = runtime_preferences::Preferences {
        revision: 7,
        ..Default::default()
    };
    result.intent.preference = preference;
    result.intent.overrides = serde_json::from_value(overrides).unwrap();
    result.intent.validate().unwrap();
    result
}

fn advice(
    fixture: &ProbeFixture,
    preferences: &runtime_preferences::Preferences,
) -> ApiResult<SetupAdvice> {
    setup_advice(
        &observation(),
        &fixture.selected,
        "synthetic-model",
        &fixture.report,
        preferences,
    )
}

fn refused<T>(result: ApiResult<T>, message: &str) {
    let error = match result {
        Ok(_) => panic!("expected refusal: {message}"),
        Err(error) => error,
    };
    assert!(error.1.contains(message), "unexpected error: {}", error.1);
}

#[tokio::test]
async fn advice_device_pins_precede_balanced_and_quiet_defaults() {
    let f = fixture(false).await;
    let cpu = preferences_for(
        planning::Preference::Balanced,
        json!({"device_id":"synthetic-cpu"}),
    );
    let gpu = preferences_for(
        planning::Preference::Quiet,
        json!({"device_id":"synthetic-metal"}),
    );
    let cpu_advice = advice(f, &cpu).unwrap();
    assert_eq!(cpu_advice.device_id, "synthetic-cpu");
    assert_eq!(cpu_advice.config.backend, runtime::BackendRequest::Cpu);
    assert_eq!(cpu_advice.record.gpu_layers, Some(0));
    let gpu_advice = advice(f, &gpu).unwrap();
    assert_eq!(gpu_advice.device_id, "synthetic-metal");
    assert_eq!(
        gpu_advice.config.backend,
        runtime::BackendRequest::Device {
            name: "Metal0".into(),
            gpu_layers: 2
        }
    );
    assert_eq!(
        gpu_advice.config.threads, 2,
        "Quiet still controls unpinned thread defaults"
    );
    for (preferences, result) in [(&cpu, cpu_advice), (&gpu, gpu_advice)] {
        assert_eq!(
            result.review["stored_intent"],
            serde_json::to_value(&preferences.intent).unwrap()
        );
        assert!(
            preferences.intent.overrides.backend.is_none(),
            "derived choice must not rewrite Auto intent"
        );
    }
    assert!(matches!(
        advice(
            f,
            &preferences_for(planning::Preference::Balanced, json!({}))
        )
        .unwrap()
        .config
        .backend,
        runtime::BackendRequest::Device { .. }
    ));
    assert_eq!(
        advice(f, &preferences_for(planning::Preference::Quiet, json!({})))
            .unwrap()
            .config
            .backend,
        runtime::BackendRequest::Cpu
    );
}

#[tokio::test]
async fn advice_absent_incompatible_or_ambiguous_pins_do_not_fall_back() {
    let f = fixture(false).await;
    for overrides in [
        json!({"device_id":"absent-device"}),
        json!({"backend":"llama_cpu","device_id":"synthetic-metal"}),
        json!({"backend":"llama_metal","device_id":"synthetic-cpu"}),
        json!({"backend":"mlx_metal","device_id":"synthetic-metal"}),
        json!({"backend":"llama_cuda"}),
        json!({"backend":"llama_hip"}),
        json!({"backend":"llama_sycl"}),
        json!({"backend":"llama_vulkan"}),
    ] {
        let preferences = preferences_for(planning::Preference::Balanced, overrides);
        assert!(
            advice(f, &preferences).is_err(),
            "unsupported pin must not be replaced: {:?}",
            preferences.intent.overrides
        );
    }
    let preferences = preferences_for(
        planning::Preference::Balanced,
        json!({"device_id":"synthetic-metal"}),
    );
    let mut ambiguous = observation();
    ambiguous
        .metal_devices
        .push(ambiguous.metal_devices[0].clone());
    refused(
        setup_advice(
            &ambiguous,
            &f.selected,
            "synthetic-model",
            &f.report,
            &preferences,
        ),
        "pinned device",
    );
    let mut absent = observation();
    absent.metal_devices.clear();
    refused(
        setup_advice(
            &absent,
            &f.selected,
            "synthetic-model",
            &f.report,
            &preferences,
        ),
        "pinned device",
    );
}

#[tokio::test]
async fn advice_supported_advanced_pins_are_exact_and_unsupported_pins_are_refused() {
    let full = fixture(false).await;
    let preferences = preferences_for(
        planning::Preference::Balanced,
        json!({
            "device_id":"synthetic-cpu", "context_tokens":3072, "output_tokens":333,
            "threads":3, "prefill_threads":5, "batch_tokens":96, "microbatch_tokens":48,
            "kv_type":"q8", "flash_attention":true, "mmap":false, "mlock":true, "gpu_layers":0
        }),
    );
    let result = advice(full, &preferences).unwrap();
    assert_eq!(result.config.context_tokens, 3072);
    assert_eq!(result.config.threads, 3);
    assert_eq!(result.record.output_tokens, 333);
    let advanced = &result.advanced;
    assert_eq!(advanced.prefill_threads, Some(5));
    assert_eq!(advanced.batch_size, Some(96));
    assert_eq!(advanced.micro_batch_size, Some(48));
    assert_eq!(advanced.flash_attention, Some(runtime::FlashAttention::On));
    assert_eq!(advanced.cache_type_k, Some(runtime::KvType::Q8_0));
    assert_eq!(advanced.cache_type_v, Some(runtime::KvType::Q8_0));
    assert_eq!(advanced.mmap, Some(false));
    assert_eq!(advanced.mlock, Some(true));
    assert_eq!(
        result.review["applied_advanced"],
        serde_json::to_value(advanced).unwrap()
    );
    let plan = runtime::LoadPlan::prepare_with_options(
        runtime::Manifest {
            model_id: "synthetic-model".into(),
            executable: full.selected.executable.clone(),
            model: full.selected.model.clone(),
            protocol: runtime::Protocol::LlamaCppUnixHttpV1,
            execution_kind: runtime::ExecutionKind::CompatibleFixture,
            declares_text_chat: true,
        },
        result.config.clone(),
        advanced.clone(),
        &full.report.compatibility,
        &runtime::Cancellation::new(),
    )
    .await
    .unwrap();
    for pair in [
        ["--threads-batch", "5"],
        ["--batch-size", "96"],
        ["--ubatch-size", "48"],
        ["--flash-attn", "on"],
        ["--cache-type-k", "q8_0"],
        ["--cache-type-v", "q8_0"],
        ["--load-mode", "mlock"],
    ] {
        assert!(
            plan.summary()
                .advanced_argv
                .windows(2)
                .any(|actual| actual[0] == pair[0] && actual[1] == pair[1]),
            "missing exact reviewed argument: {pair:?}"
        );
    }
    let limited = fixture(true).await;
    assert!(limited.report.compatibility.supports_flag("--ubatch-size"));
    assert!(!limited.report.compatibility.supports_flag("--batch-size"));
    for overrides in [
        json!({"prefill_threads":2}),
        json!({"batch_tokens":64}),
        json!({"microbatch_tokens":32}),
        json!({"flash_attention":false}),
        json!({"mmap":false}),
        json!({"mlock":false}),
    ] {
        refused(
            advice(
                limited,
                &preferences_for(planning::Preference::Balanced, overrides),
            ),
            "override was not ignored",
        );
    }
    let defaults = advice(
        limited,
        &preferences_for(planning::Preference::Quiet, json!({})),
    )
    .unwrap();
    assert_eq!(defaults.advanced.prefill_threads, None);
    assert_eq!(defaults.advanced.batch_size, None);
    assert_eq!(defaults.advanced.micro_batch_size, None);
    assert_eq!(defaults.advanced.flash_attention, None);
    assert_eq!(defaults.advanced.mmap, None);
    assert_eq!(defaults.advanced.mlock, None);
    for overrides in [
        json!({"kv_type":"f32"}),
        json!({"kv_type":"bf16"}),
        json!({"device_id":"synthetic-cpu","gpu_layers":1}),
    ] {
        assert!(advice(
            full,
            &preferences_for(planning::Preference::Balanced, overrides)
        )
        .is_err());
    }
}

#[tokio::test]
async fn advice_never_promotes_probe_or_free_page_evidence_to_qualified_memory_fit() {
    let f = fixture(false).await;
    let preferences = preferences_for(planning::Preference::Balanced, json!({}));
    let result = advice(f, &preferences).unwrap();
    let review = result.review;
    let preview = &review["settings_preview"]["preview"];
    assert_eq!(preview["qualification"], "unvalidated");
    assert_eq!(preview["fit"], "unknown");
    assert_eq!(preview["inventory_generation"], "synthetic-generation");
    assert_eq!(preview["config_revision"], 7);
    assert_eq!(preview["identity"]["scope"], "selected_executable");
    assert_eq!(preview["identity"]["sha256"], f.selected.executable.sha256);
    assert!(preview["adapter_bundle_sha256"].is_null());
    assert_eq!(review["memory_fit"], "unknown");
    assert_eq!(review["placement"], "unverified");
    assert_eq!(review["runtime_check"]["inference_verified"], false);
    assert_eq!(
        observation().inventory.memory_pools.len(),
        1,
        "CPU/GPU share physical memory"
    );
    assert!(observation().inventory.memory_pools[0]
        .available_bytes
        .is_none());
    for overrides in [
        json!({"host_budget_bytes":1 << 30}),
        json!({"device_budget_bytes":1 << 30}),
        json!({"headroom_bytes":1 << 30}),
    ] {
        refused(
            advice(
                f,
                &preferences_for(planning::Preference::Balanced, overrides),
            ),
            "cannot enforce a memory budget",
        );
    }
}

fn test_app() -> Arc<App> {
    crate::app(Path::new(":memory:")).unwrap()
}
fn expected(a: &App) -> Expected {
    let consent = a.consent.lock().unwrap();
    let config = a.config.lock().unwrap();
    Expected {
        revision: consent.token(),
        provider: config.provider.clone(),
        endpoint: config.endpoint.clone(),
    }
}
fn with_expected(a: &App, mut input: Value) -> Value {
    let target = expected(a);
    input["revision"] = json!(target.revision);
    input["provider"] = json!(target.provider);
    input["endpoint"] = json!(target.endpoint);
    input
}

async fn manual_operations_are_blocked(a: &Arc<App>) {
    let file = json!({"path":"/synthetic/not-read","sha256":"a".repeat(64),"source":"synthetic","source_revision":"fixture-v1"});
    let prepare_input = serde_json::from_value(with_expected(a, json!({
        "executable":file,"model":file,"model_id":"synthetic-model","execution_kind":"compatible_fixture",
        "context_tokens":2048,"threads":2,"startup_seconds":3,"generation_seconds":3,
        "device":null,"gpu_layers":null
    }))).unwrap();
    let load_input = serde_json::from_value(with_expected(
        a,
        json!({"plan_id":"never-admitted","runtime_revision":0,"confirm":true}),
    ))
    .unwrap();
    let unload_input = serde_json::from_value(with_expected(a, json!({"owner":"never-admitted","generation":0,"runtime_revision":0,"model_id":"synthetic-model"}))).unwrap();
    for result in [
        native_api::prepare(State(a.clone()), Json(prepare_input)).await,
        native_api::load(State(a.clone()), Json(load_input)).await,
        native_api::unload(State(a.clone()), Json(unload_input)).await,
    ] {
        assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT);
    }
    assert!(
        a.native.snapshot().is_none(),
        "blocked entries must not create a runtime actor"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn apply_reservation_cancellation_and_shutdown_block_manual_transitions() {
    let a = test_app();
    a.config.lock().unwrap().provider = "managed".into();
    let (operation, token) = a.acceleration.begin("apply").unwrap();
    manual_operations_are_blocked(&a).await;
    refused(
        cancel(
            State(a.clone()),
            Json(CancelInput {
                operation_id: "wrong-operation".into(),
            }),
        )
        .await,
        "no longer running",
    );
    assert!(!token.is_cancelled());
    let _ = cancel(
        State(a.clone()),
        Json(CancelInput {
            operation_id: operation.clone(),
        }),
    )
    .await
    .unwrap();
    assert!(token.is_cancelled());
    assert!(a.acceleration.busy(), "cancel is not proof of cleanup");
    manual_operations_are_blocked(&a).await;
    a.acceleration.finish("wrong-operation", Ok(()));
    assert!(a.acceleration.busy());
    a.acceleration.finish(&operation, Ok(()));
    assert_eq!(
        a.acceleration
            .inner
            .lock()
            .unwrap()
            .operation
            .as_ref()
            .unwrap()
            .status,
        "cancelled"
    );
    assert!(!a.acceleration.busy());
    let (next, _) = a.acceleration.begin("inspect").unwrap();
    a.acceleration
        .finish(&operation, Err(bad("late old completion")));
    assert_eq!(
        a.acceleration
            .inner
            .lock()
            .unwrap()
            .operation
            .as_ref()
            .unwrap()
            .id,
        next
    );
    assert!(a.acceleration.busy());
    a.acceleration.begin_shutdown();
    a.acceleration.finish(&next, Ok(()));
    assert!(
        a.acceleration.busy(),
        "shutdown remains closed even after worker completion"
    );
    manual_operations_are_blocked(&a).await;
    assert!(a.acceleration.begin("plan").is_err());
    a.native.shutdown().await.unwrap();
}

async fn fixture_plan(f: &ProbeFixture) -> runtime::LoadPlan {
    let settings = advice(f, &runtime_preferences::Preferences::default()).unwrap();
    runtime::LoadPlan::prepare(
        runtime::Manifest {
            model_id: "synthetic-model".into(),
            executable: f.selected.executable.clone(),
            model: f.selected.model.clone(),
            protocol: runtime::Protocol::LlamaCppUnixHttpV1,
            execution_kind: runtime::ExecutionKind::CompatibleFixture,
            declares_text_chat: true,
        },
        settings.config,
        &runtime::Cancellation::new(),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn selected_plan_admission_rejects_stale_runtime_target_and_preferences() {
    let f = fixture(false).await;
    let a = test_app();
    let saved = runtime_preferences::read(&a.db.lock().unwrap()).unwrap();
    let prepared = native_api::PreparedPreferences {
        revision: saved.revision,
        intent: saved.intent.clone(),
        record: advice(f, &saved).unwrap().record,
    };
    let current_revision = a.native.revision();
    let target = expected(&a);
    let plan = fixture_plan(f).await;
    {
        let consent = a.consent.lock().unwrap();
        let error = native_api::admit_selected_plan(
            &a,
            &consent,
            current_revision + 1,
            target.clone(),
            plan.clone(),
            prepared.clone(),
            None,
        )
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(error.1.contains("StaleRevision"));
    }
    a.consent.lock().unwrap().advance();
    {
        let consent = a.consent.lock().unwrap();
        let error = native_api::admit_selected_plan(
            &a,
            &consent,
            current_revision,
            target,
            plan.clone(),
            prepared.clone(),
            None,
        )
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(error.1.contains("connection has changed"));
    }
    runtime_preferences::update(&a.db.lock().unwrap(), saved.revision, saved.intent).unwrap();
    let target = expected(&a);
    {
        let consent = a.consent.lock().unwrap();
        let error = native_api::admit_selected_plan(
            &a,
            &consent,
            current_revision,
            target,
            plan,
            prepared,
            None,
        )
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(error.1.contains("settings changed"));
    }
    let Json(status) = native_api::status(State(a.clone())).await;
    assert!(status["operation"].is_null());
    assert!(status["prepared"].is_null());
    assert_eq!(a.native.revision(), current_revision);
    assert!(!a.native.busy());
    a.native.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_apply_request_stays_tracked_and_cancel_selection_shutdown_prevent_dispatch() {
    use futures_util::FutureExt;
    let f = fixture(false).await;
    for transition in ["cancel_after_request_drop", "replace_selection", "shutdown"] {
        let a = test_app();
        let saved = runtime_preferences::read(&a.db.lock().unwrap()).unwrap();
        let target = expected(&a);
        let settings = advice(f, &saved).unwrap();
        let prepared = PreparedSetup {
            selection_id: "reviewed-selection".into(),
            runtime_revision: 0,
            plan: fixture_plan(f).await,
            expected: target.clone(),
            preferences: native_api::PreparedPreferences {
                revision: saved.revision,
                intent: saved.intent,
                record: settings.record,
            },
            inventory: observation(),
            device_id: settings.device_id,
            catalog: None,
        };
        a.acceleration.inner.lock().unwrap().selection = Some(Selection {
            id: "reviewed-selection".into(),
            model_id: "synthetic-model".into(),
            files: f.selected.clone(),
            expected: target,
            probe: None,
            catalog: None,
        });
        let (operation, token) = a.acceleration.begin("apply").unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let request = tokio::spawn(apply_selected(
            a.clone(),
            prepared,
            operation.clone(),
            token,
            async move {
                entered_tx.send(()).unwrap();
                release_rx.await.unwrap();
                Ok(observation())
            },
        ));
        tokio::time::timeout(Duration::from_secs(3), entered_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            a.tasks.drain().now_or_never().is_none(),
            "observation must be registered before request loss"
        );
        let request = if transition == "cancel_after_request_drop" {
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            assert!(a.acceleration.busy());
            assert!(
                a.tasks.drain().now_or_never().is_none(),
                "losing the reply cannot release worker custody"
            );
            let _ = cancel(
                State(a.clone()),
                Json(CancelInput {
                    operation_id: operation.clone(),
                }),
            )
            .await
            .unwrap();
            None
        } else {
            if transition == "replace_selection" {
                a.acceleration
                    .inner
                    .lock()
                    .unwrap()
                    .selection
                    .as_mut()
                    .unwrap()
                    .id = "newer-selection".into();
            } else {
                a.acceleration.begin_shutdown();
            }
            Some(request)
        };
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), a.tasks.drain())
            .await
            .unwrap();
        if let Some(request) = request {
            refused(
                request.await.unwrap(),
                "cancelled or replaced before loading",
            );
        }
        {
            let inner = a.acceleration.inner.lock().unwrap();
            let finished = inner.operation.as_ref().unwrap();
            assert_eq!(finished.id, operation);
            assert_eq!(
                finished.status,
                if transition == "replace_selection" {
                    "failed"
                } else {
                    "cancelled"
                }
            );
        }
        assert_eq!(a.acceleration.busy(), transition == "shutdown");
        assert!(
            a.native.snapshot().is_none(),
            "no rejected transfer may create a native actor"
        );
        assert!(!a.native.busy());
        a.native.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn uncertain_cleanup_is_failed_and_never_reopened_by_cancellation() {
    let a = test_app();
    a.config.lock().unwrap().provider = "managed".into();
    let (operation, token) = a.acceleration.begin("plan").unwrap();
    token.cancel();
    // Inject the retained cleanup-failure state; no unreaped process is created.
    a.acceleration.inner.lock().unwrap().cleanup_unconfirmed = true;
    a.acceleration
        .finish(&operation, Err(bad("synthetic cleanup uncertainty")));
    assert_eq!(
        a.acceleration
            .inner
            .lock()
            .unwrap()
            .operation
            .as_ref()
            .unwrap()
            .status,
        "failed"
    );
    assert!(a.acceleration.busy());
    assert!(a.acceleration.shutdown_result().is_err());
    assert!(a.acceleration.begin("inspect").is_err());
    manual_operations_are_blocked(&a).await;
    a.acceleration.begin_shutdown();
    assert!(a.acceleration.shutdown_result().is_err());
    a.native.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn preference_changes_clear_the_displayed_preview_before_new_advice() {
    let a = test_app();
    let original = runtime_preferences::Preferences::default().intent;
    // Seed only passive historical data to exercise rollback; no success of a
    // real runtime is claimed by this synthetic database record.
    let f = fixture(false).await;
    let saved = runtime_preferences::read(&a.db.lock().unwrap()).unwrap();
    let record = advice(f, &saved).unwrap().record;
    {
        let mut db = a.db.lock().unwrap();
        let tx = db.transaction().unwrap();
        runtime_preferences::commit_selected_success(&tx, 0, &original, &record).unwrap();
        tx.commit().unwrap();
    }
    let mut changed = original.clone();
    changed.preference = planning::Preference::Quiet;
    a.acceleration.inner.lock().unwrap().preview = Some(json!({"obsolete":1}));
    let _ = preferences(
        State(a.clone()),
        Json(PreferencesInput {
            expected_revision: 1,
            intent: changed,
        }),
    )
    .await
    .unwrap();
    assert!(a.acceleration.inner.lock().unwrap().preview.is_none());
    a.acceleration.inner.lock().unwrap().preview = Some(json!({"obsolete":2}));
    let _ = reset(
        State(a.clone()),
        Json(RevisionInput {
            expected_revision: 2,
        }),
    )
    .await
    .unwrap();
    assert!(a.acceleration.inner.lock().unwrap().preview.is_none());
    a.acceleration.inner.lock().unwrap().preview = Some(json!({"obsolete":3}));
    let _ = rollback(
        State(a.clone()),
        Json(RevisionInput {
            expected_revision: 3,
        }),
    )
    .await
    .unwrap();
    assert!(a.acceleration.inner.lock().unwrap().preview.is_none());
    assert_eq!(
        runtime_preferences::read(&a.db.lock().unwrap())
            .unwrap()
            .intent,
        original
    );
    assert!(a.native.snapshot().is_none());
}
