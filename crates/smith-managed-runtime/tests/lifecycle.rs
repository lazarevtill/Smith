#![cfg(any(target_os = "macos", target_os = "linux"))]
use sha2::{Digest, Sha256};
use smith_managed_runtime::*;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    net::UnixListener,
    time::{sleep, timeout, Instant},
};

static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
fn fixture() -> &'static Path {
    &FIXTURE
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let output = directory.path().join("llama-compatible");
            let status = std::process::Command::new(
                std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
            )
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/llama_compatible.rs"))
            .args(["--edition=2021", "-o"])
            .arg(&output)
            .status()
            .unwrap();
            assert!(status.success());
            (directory, output)
        })
        .1
}
fn artifact(path: &Path) -> Artifact {
    let digest = Sha256::digest(std::fs::read(path).unwrap());
    Artifact {
        path: path.to_path_buf(),
        sha256: digest.iter().map(|b| format!("{b:02x}")).collect(),
        source: "controlled integration fixture".into(),
        source_revision: "fixture-v1".into(),
    }
}
async fn plan(mode: &str) -> (tempfile::TempDir, ApprovedPlan) {
    plan_budgets(mode, 3_000, 3_000).await
}
async fn plan_budgets(
    mode: &str,
    startup_ms: u64,
    generation_ms: u64,
) -> (tempfile::TempDir, ApprovedPlan) {
    let directory = tempfile::tempdir().unwrap();
    let model = directory.path().join("model.gguf");
    std::fs::write(&model, format!("GGUF{mode}")).unwrap();
    let manifest = Manifest {
        model_id: "test/model".into(),
        executable: artifact(fixture()),
        model: artifact(&model),
        protocol: Protocol::LlamaCppUnixHttpV1,
        execution_kind: ExecutionKind::CompatibleFixture,
        declares_text_chat: true,
    };
    let config = LoadConfig {
        budgets: Budgets {
            startup: Duration::from_millis(startup_ms),
            generation: Duration::from_millis(generation_ms),
            reap: Duration::from_secs(2),
        },
        ..LoadConfig::default()
    };
    let plan = LoadPlan::prepare(manifest, config, &Cancellation::new())
        .await
        .unwrap();
    let id = plan.summary().id.clone();
    (directory, plan.approve(&id).unwrap())
}
async fn load(runtime: &Runtime, plan: ApprovedPlan) -> Snapshot {
    runtime
        .load(runtime.status().revision, plan, Cancellation::new())
        .await
        .unwrap()
}
fn request() -> TextRequest {
    TextRequest {
        messages: vec![Message {
            role: Role::User,
            content: "private fixture prompt".into(),
        }],
        max_tokens: 8,
    }
}
async fn run(runtime: &Runtime, target: Target) -> Generation {
    runtime
        .start_generation(target, Capability::TextChat, request(), Cancellation::new())
        .await
        .unwrap()
}
async fn settled(runtime: &Runtime) -> Snapshot {
    timeout(Duration::from_secs(3), async {
        loop {
            let state = runtime.status();
            if state.pid.is_none() {
                return state;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
fn reaped(state: &Snapshot) {
    assert_eq!(state.spawned, state.reaped);
    assert_eq!(state.pid, None);
    assert_eq!(state.active_leases, 0);
}

#[tokio::test]
async fn explicit_plan_normal_load_generation_and_unload() {
    let (_dir, plan) = plan("normal").await;
    let runtime = Runtime::new();
    assert_eq!(runtime.status().spawned, 0);
    let state = load(&runtime, plan.clone()).await;
    assert_eq!(state.phase, Phase::Ready);
    assert_eq!(state.execution_kind, Some(ExecutionKind::CompatibleFixture));
    assert_eq!(state.observed_backend, Evidence::Unknown);
    let same = load(&runtime, plan.clone()).await;
    assert_eq!(state.target, same.target);
    assert_eq!(same.spawned, 1);
    let mut generation = run(&runtime, state.target).await;
    let result = generation.wait().await.unwrap();
    assert_eq!(result.text, "fixture output");
    assert_eq!(result.usage.unwrap().total_tokens, 6);
    assert_eq!(runtime.status().text_chat, Evidence::ProtocolObserved);
    let state = runtime
        .unload(state.target, "test/model".into())
        .await
        .unwrap();
    reaped(&state);
    let closed = runtime.shutdown().await.unwrap();
    reaped(&closed);
    assert_eq!(runtime.shutdown().await.unwrap().target, closed.target);
    let observed = std::fs::read_to_string(
        plan.summary()
            .manifest
            .model
            .path
            .with_extension("observed"),
    )
    .unwrap();
    for flag in [
        "--offline",
        "--no-agent",
        "--no-webui",
        "--no-mmproj",
        "--fit",
    ] {
        assert!(observed.contains(flag));
    }
    assert!(observed.contains("LLAMA_API_KEY"));
    assert!(!observed.contains("HTTP_PROXY"));
    assert!(!observed.contains("PATH\""));
    let logging = &plan.summary().startup_logging_argv;
    assert_eq!(
        logging,
        &[
            "--verbosity",
            "3",
            "--log-colors",
            "off",
            "--no-log-prefix",
            "--no-log-timestamps",
            "--no-log-jsonl"
        ]
    );
    for arg in logging {
        assert!(observed.contains(&format!("{arg:?}")));
    }
}

fn captured_requests(plan: &ApprovedPlan) -> Vec<serde_json::Value> {
    std::fs::read_to_string(
        plan.summary()
            .manifest
            .model
            .path
            .with_extension("requests"),
    )
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect()
}

#[tokio::test]
async fn task_schema_is_sent_only_on_explicit_task_generation() {
    let (_dir, plan) = plan("request_capture").await;
    let runtime = Runtime::new();
    let state = load(&runtime, plan.clone()).await;
    let mut plain = run(&runtime, state.target).await;
    assert_eq!(plain.wait().await.unwrap().text, "fixture output");
    let mut task = runtime
        .start_task_generation(state.target, request(), Cancellation::new())
        .await
        .unwrap();
    assert_eq!(task.wait().await.unwrap().text, "fixture output");
    let requests = captured_requests(&plan);
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("response_format").is_none());
    assert_eq!(requests[1]["response_format"]["type"], "json_schema");
    assert_eq!(
        requests[1]["response_format"]["json_schema"]["schema"]["required"],
        serde_json::json!(["content", "tool_call"])
    );
    assert_eq!(requests[0]["messages"], requests[1]["messages"]);
    assert!(requests[1].get("grammar").is_none());
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn task_schema_rejection_never_falls_back_or_replays() {
    let (_dir, plan) = plan("task_schema_reject").await;
    let runtime = Runtime::new();
    let state = load(&runtime, plan.clone()).await;
    let mut task = runtime
        .start_task_generation(state.target, request(), Cancellation::new())
        .await
        .unwrap();
    assert_eq!(task.wait().await.unwrap_err(), Failure::Protocol);
    assert_eq!(captured_requests(&plan).len(), 1);
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn ignored_task_schema_preserves_exact_response_for_host_rejection() {
    let (_dir, plan) = plan("task_schema_ignored").await;
    let runtime = Runtime::new();
    let state = load(&runtime, plan.clone()).await;
    let mut task = runtime
        .start_task_generation(state.target, request(), Cancellation::new())
        .await
        .unwrap();
    assert_eq!(
        task.wait().await.unwrap().text,
        "```json\n{\"content\":\"synthetic\",\"tool_call\":null}\n```"
    );
    assert_eq!(captured_requests(&plan).len(), 1);
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn task_schema_uses_same_exact_owner_and_cancellation_lease() {
    let (_dir, plan) = plan("task_schema_stall").await;
    let runtime = Runtime::new();
    let state = load(&runtime, plan.clone()).await;
    let stale = Target {
        generation: state.target.generation + 1,
        ..state.target
    };
    assert!(matches!(
        runtime
            .start_task_generation(stale, request(), Cancellation::new())
            .await,
        Err(Failure::StaleGeneration)
    ));
    assert!(!plan
        .summary()
        .manifest
        .model
        .path
        .with_extension("requests")
        .exists());
    let cancel = Cancellation::new();
    let mut task = runtime
        .start_task_generation(state.target, request(), cancel.clone())
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while !plan
            .summary()
            .manifest
            .model
            .path
            .with_extension("requests")
            .exists()
        {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert_eq!(task.wait().await.unwrap_err(), Failure::Cancelled);
    reaped(&runtime.status());
    assert_eq!(captured_requests(&plan).len(), 1);
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn reviewed_advanced_settings_reach_actual_owned_child_and_plan_identity() {
    let (_dir, original) = plan("normal").await;
    let manifest = original.summary().manifest.clone();
    let probe = RuntimeProbePlan::prepare(manifest.executable.clone(), &Cancellation::new())
        .await
        .unwrap();
    let probe_id = probe.summary().id.clone();
    let mut handle = RuntimeProbe::start(probe.approve(&probe_id).unwrap(), Cancellation::new());
    let report = handle.wait().await.unwrap();
    let settings = AdvancedSettings {
        prefill_threads: Some(6),
        batch_size: Some(128),
        micro_batch_size: Some(32),
        flash_attention: Some(FlashAttention::On),
        cache_type_k: Some(KvType::Q8_0),
        cache_type_v: Some(KvType::F16),
        mmap: Some(false),
        mlock: Some(false),
    };
    let plan = LoadPlan::prepare_with_options(
        manifest.clone(),
        original.summary().config.clone(),
        settings.clone(),
        &report.compatibility,
        &Cancellation::new(),
    )
    .await
    .unwrap();
    assert_ne!(plan.summary().id, original.summary().id);
    assert_eq!(plan.summary().advanced.as_ref(), Some(&settings));
    let expected = vec![
        "--threads-batch",
        "6",
        "--batch-size",
        "128",
        "--ubatch-size",
        "32",
        "--flash-attn",
        "on",
        "--cache-type-k",
        "q8_0",
        "--cache-type-v",
        "f16",
        "--load-mode",
        "none",
    ];
    assert_eq!(plan.summary().advanced_argv, expected);
    let id = plan.summary().id.clone();
    let runtime = Runtime::new();
    let loaded = load(&runtime, plan.approve(&id).unwrap()).await;
    assert_eq!(loaded.observed_backend, Evidence::Unknown);
    let observed = std::fs::read_to_string(manifest.model.path.with_extension("observed")).unwrap();
    for pair in expected.chunks(2) {
        assert!(observed.contains(&format!("\"{}\", \"{}\"", pair[0], pair[1])));
    }
    reaped(&runtime.shutdown().await.unwrap());
    let mut changed = manifest.clone();
    changed.executable.sha256 = "0".repeat(64);
    assert!(matches!(
        LoadPlan::prepare_with_options(
            changed,
            LoadConfig::default(),
            settings.clone(),
            &report.compatibility,
            &Cancellation::new()
        )
        .await,
        Err(Failure::ArtifactChanged)
    ));
    let invalid = AdvancedSettings {
        batch_size: Some(1),
        micro_batch_size: Some(2),
        ..settings
    };
    assert!(matches!(
        LoadPlan::prepare_with_options(
            manifest,
            LoadConfig::default(),
            invalid,
            &report.compatibility,
            &Cancellation::new()
        )
        .await,
        Err(Failure::InvalidConfig)
    ));
}

#[tokio::test]
async fn manifest_hash_source_consent_and_revision_are_required() {
    let (_dir, approved) = plan("normal").await;
    let p = LoadPlan::prepare(
        approved.summary().manifest.clone(),
        approved.summary().config.clone(),
        &Cancellation::new(),
    )
    .await
    .unwrap();
    assert!(matches!(
        p.approve("another plan"),
        Err(Failure::ConsentMismatch)
    ));
    let mut bad = approved.summary().manifest.clone();
    bad.model.source.clear();
    assert!(matches!(
        LoadPlan::prepare(bad, LoadConfig::default(), &Cancellation::new()).await,
        Err(Failure::InvalidManifest)
    ));
    let runtime = Runtime::new();
    assert_eq!(
        runtime
            .load(999, approved.clone(), Cancellation::new())
            .await
            .unwrap_err(),
        Failure::StaleRevision
    );
    assert_eq!(runtime.status().spawned, 0);
    let cancel = Cancellation::new();
    cancel.cancel();
    assert_eq!(
        runtime.load(0, approved.clone(), cancel).await.unwrap_err(),
        Failure::Cancelled
    );
    std::fs::write(&approved.summary().manifest.model.path, b"GGUFchanged").unwrap();
    assert_eq!(
        runtime
            .load(0, approved, Cancellation::new())
            .await
            .unwrap_err(),
        Failure::ArtifactChanged
    );
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn unsupported_protocol_and_capability_fail_without_fallback() {
    let (_dir, approved) = plan("normal").await;
    let mut manifest = approved.summary().manifest.clone();
    manifest.protocol = Protocol::LegacyTcp;
    let p = LoadPlan::prepare(manifest, LoadConfig::default(), &Cancellation::new())
        .await
        .unwrap();
    let id = p.summary().id.clone();
    let runtime = Runtime::new();
    assert_eq!(
        runtime
            .load(0, p.approve(&id).unwrap(), Cancellation::new())
            .await
            .unwrap_err(),
        Failure::UnsupportedProtocol
    );
    assert_eq!(runtime.status().spawned, 0);
    let state = load(&runtime, approved).await;
    for capability in [
        Capability::Tools,
        Capability::Vision,
        Capability::Audio,
        Capability::Embeddings,
        Capability::Reasoning,
    ] {
        assert!(matches!(
            runtime
                .start_generation(state.target, capability, request(), Cancellation::new())
                .await,
            Err(Failure::UnsupportedCapability)
        ));
    }
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn cancellation_load_timeout_and_child_exit_reap() {
    for (mode, expected) in [
        ("health_stall", Failure::StartupTimeout),
        ("exit", Failure::ChildExited),
        ("wrong_model", Failure::Protocol),
    ] {
        let (_dir, plan) = plan_budgets(mode, 160, 1000).await;
        let runtime = Runtime::new();
        let result = runtime.load(0, plan, Cancellation::new()).await;
        assert_eq!(result.unwrap_err(), expected);
        reaped(&runtime.status());
        reaped(&runtime.shutdown().await.unwrap());
    }
    let (_dir, plan) = plan("health_stall").await;
    let runtime = Runtime::new();
    let cancel = Cancellation::new();
    let r = runtime.clone();
    let c = cancel.clone();
    let load = tokio::spawn(async move { r.load(0, plan, c).await });
    while runtime.status().pid.is_none() {
        sleep(Duration::from_millis(5)).await;
    }
    let began = Instant::now();
    cancel.cancel();
    assert_eq!(load.await.unwrap().unwrap_err(), Failure::Cancelled);
    assert!(began.elapsed() < Duration::from_millis(500));
    reaped(&runtime.status());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn active_lease_serializes_eviction_switch_and_cancel() {
    let (_dir, plan) = plan("generation_stall").await;
    let runtime = Runtime::new();
    let state = load(&runtime, plan.clone()).await;
    let mut generation = run(&runtime, state.target).await;
    assert_eq!(
        runtime
            .unload(state.target, "test/model".into())
            .await
            .unwrap_err(),
        Failure::Busy
    );
    assert_eq!(
        runtime
            .unload_if_idle(state.target, "test/model".into(), Duration::ZERO)
            .await
            .unwrap_err(),
        Failure::Busy
    );
    assert_eq!(
        runtime
            .load(runtime.status().revision, plan, Cancellation::new())
            .await
            .unwrap_err(),
        Failure::Busy
    );
    let began = Instant::now();
    generation.cancel();
    assert_eq!(generation.wait().await.unwrap_err(), Failure::Cancelled);
    assert!(began.elapsed() < Duration::from_millis(500));
    reaped(&runtime.status());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn generation_timeouts_stalled_body_and_invalid_responses_reap() {
    for (mode, error) in [
        ("generation_stall", Failure::GenerationTimeout),
        ("body_stall", Failure::GenerationTimeout),
        ("oversize", Failure::ResponseTooLarge),
        ("redirect", Failure::Protocol),
        ("truncated", Failure::Transport),
        ("malformed", Failure::Protocol),
    ] {
        let (_dir, plan) = plan_budgets(mode, 3000, 120).await;
        let runtime = Runtime::new();
        let state = load(&runtime, plan).await;
        let mut generation = run(&runtime, state.target).await;
        assert_eq!(generation.wait().await.unwrap_err(), error);
        reaped(&runtime.status());
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn caller_drop_shutdown_and_sibling_ownership() {
    let (_dir, plan) = plan("generation_stall").await;
    let runtime = Runtime::new();
    let sibling = Runtime::new();
    let state = load(&runtime, plan.clone()).await;
    let other = load(&sibling, plan.clone()).await;
    let generation = run(&runtime, state.target).await;
    drop(generation);
    reaped(&settled(&runtime).await);
    assert_eq!(sibling.refresh_status().await.unwrap().pid, other.pid);
    let state = load(&runtime, plan.clone()).await;
    let mut generation = run(&runtime, state.target).await;
    reaped(&runtime.shutdown().await.unwrap());
    assert_eq!(generation.wait().await.unwrap_err(), Failure::ShuttingDown);
    reaped(&sibling.shutdown().await.unwrap());
    let runtime = Runtime::new();
    let r = runtime.clone();
    let (_dir, stalled) = plan_budgets("health_stall", 3000, 3000).await;
    let handle = tokio::spawn(async move { r.load(0, stalled, Cancellation::new()).await });
    while runtime.status().pid.is_none() {
        sleep(Duration::from_millis(5)).await;
    }
    handle.abort();
    let _ = handle.await;
    reaped(&settled(&runtime).await);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_replacement_preserves_old_and_stale_target_cannot_stop_new() {
    let (_dir, plan) = plan("normal").await;
    let runtime = Runtime::new();
    let old = load(&runtime, plan.clone()).await;
    let (_bad_dir, bad) = plan_budgets("normal", 500, 500).await;
    std::fs::write(&bad.summary().manifest.model.path, b"GGUFmodified").unwrap();
    assert_eq!(
        runtime
            .load(old.revision, bad, Cancellation::new())
            .await
            .unwrap_err(),
        Failure::ArtifactChanged
    );
    assert_eq!(runtime.status().pid, old.pid);
    runtime
        .unload(old.target, "test/model".into())
        .await
        .unwrap();
    let new = load(&runtime, plan).await;
    assert_ne!(new.target, old.target);
    assert_eq!(
        runtime
            .unload(old.target, "test/model".into())
            .await
            .unwrap_err(),
        Failure::StaleGeneration
    );
    assert_eq!(
        runtime
            .unload(new.target, "wrong/model".into())
            .await
            .unwrap_err(),
        Failure::WrongModel
    );
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn ready_child_exit_is_reflected_and_never_restarted() {
    let (_dir, plan) = plan("exit_after_ready").await;
    let runtime = Runtime::new();
    load(&runtime, plan).await;
    let state = settled(&runtime).await;
    reaped(&state);
    assert_eq!(state.failure, Some(Failure::ChildExited));
    assert_eq!(state.spawned, 1);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn occupied_socket_receives_zero_http_or_credentials() {
    let (_dir, plan) = plan("no_bind").await;
    let marker = plan.summary().manifest.model.path.with_extension("socket");
    let runtime = Runtime::new();
    let r = runtime.clone();
    let loading = tokio::spawn(async move { r.load(0, plan, Cancellation::new()).await });
    let path = timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(path) = std::fs::read_to_string(&marker) {
                break path;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let foreign = UnixListener::bind(&path).unwrap();
    let (mut stream, _) = timeout(Duration::from_secs(3), foreign.accept())
        .await
        .unwrap()
        .unwrap();
    let mut data = [0; 1024];
    let n = timeout(Duration::from_secs(1), stream.read(&mut data))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        n, 0,
        "no health, credential, or prompt may reach a foreign peer"
    );
    assert_eq!(
        loading.await.unwrap().unwrap_err(),
        Failure::EndpointOwnership
    );
    reaped(&runtime.status());
    assert!(foreign.local_addr().is_ok());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn stolen_socket_is_rechecked_on_generation_before_prompt_bytes() {
    let (_dir, plan) = plan("normal").await;
    let marker = plan.summary().manifest.model.path.with_extension("socket");
    let runtime = Runtime::new();
    let state = load(&runtime, plan).await;
    let path = std::fs::read_to_string(marker).unwrap();
    std::fs::remove_file(&path).unwrap();
    let foreign = UnixListener::bind(path).unwrap();
    let mut generation = run(&runtime, state.target).await;
    let (mut stream, _) = timeout(Duration::from_secs(2), foreign.accept())
        .await
        .unwrap()
        .unwrap();
    let mut data = [0; 1024];
    assert_eq!(stream.read(&mut data).await.unwrap(), 0);
    assert_eq!(
        generation.wait().await.unwrap_err(),
        Failure::EndpointOwnership
    );
    reaped(&runtime.status());
    assert!(foreign.local_addr().is_ok());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn repeated_cycles_and_idle_admission_have_one_winner() {
    let (_dir, plan) = plan("normal").await;
    let runtime = Runtime::new();
    for _ in 0..12 {
        let state = load(&runtime, plan.clone()).await;
        let (run, eviction) = tokio::join!(
            runtime.start_generation(
                state.target,
                Capability::TextChat,
                request(),
                Cancellation::new()
            ),
            runtime.unload_if_idle(state.target, "test/model".into(), Duration::ZERO)
        );
        match (run, eviction) {
            (Ok(mut generation), Err(Failure::Busy)) => {
                generation.wait().await.unwrap();
                runtime
                    .unload(state.target, "test/model".into())
                    .await
                    .unwrap();
            }
            (Err(Failure::NotReady), Ok(_)) => {}
            _ => panic!("admission and eviction must have exactly one winner"),
        }
        reaped(&runtime.status());
    }
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn last_runtime_handle_drop_reaps_while_executor_is_alive() {
    let (_dir, plan) = plan("normal").await;
    let runtime = Runtime::new();
    load(&runtime, plan).await;
    let mut status = runtime.subscribe();
    drop(runtime);
    let terminal = timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = status.borrow_and_update().clone();
            if !snapshot.accepting && snapshot.phase == Phase::Stopped {
                break snapshot;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    reaped(&terminal);
}

#[tokio::test]
async fn stderr_flood_cannot_block_supervisor_or_leak_into_status() {
    let (_dir, plan) = plan("stderr_flood").await;
    let runtime = Runtime::new();
    let snapshot = load(&runtime, plan).await;
    assert_eq!(snapshot.phase, Phase::Ready);
    assert_eq!(snapshot.process_tree_cleanup, Evidence::Unsupported);
    assert_eq!(snapshot.parent_crash_cleanup, Evidence::Unsupported);
    let observation = runtime
        .startup_observation(snapshot.target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observation.stderr.inspected_bytes, 64 * 1024);
    assert!(observation.stderr.truncated);
    assert!(matches!(
        observation.placement,
        StartupPlacement::Unknown {
            reason: StartupUnknownReason::IncompleteOutput
        }
    ));
    let encoded = serde_json::to_string(&snapshot).unwrap();
    assert!(!encoded.contains("Bearer"));
    assert!(!encoded.contains("private fixture prompt"));
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn fifo_artifact_is_rejected_before_open_can_block() {
    use std::os::unix::ffi::OsStrExt;
    let (_dir, approved) = plan("normal").await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("not-a-model.gguf");
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated path points inside this test's owned TempDir.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let mut manifest = approved.summary().manifest.clone();
    manifest.model.path = path;
    let result = timeout(
        Duration::from_millis(500),
        LoadPlan::prepare(
            manifest,
            approved.summary().config.clone(),
            &Cancellation::new(),
        ),
    )
    .await
    .expect("special-file rejection must not wait for a FIFO writer");
    assert!(matches!(result, Err(Failure::InvalidManifest)));
}

#[tokio::test]
async fn startup_output_is_exact_target_bounded_and_frozen_before_prompt_admission() {
    let (_dir, approved) = plan("startup_output").await;
    let summary = approved.summary().clone();
    let runtime = Runtime::new();
    assert!(runtime
        .startup_observation(runtime.status().target)
        .await
        .unwrap()
        .is_none());
    let ready = load(&runtime, approved).await;
    let before = runtime
        .startup_diagnostics(ready.target)
        .await
        .unwrap()
        .unwrap();
    let observation = &before.observation;
    assert_eq!(observation.identity.target, ready.target);
    assert_eq!(observation.identity.pid, ready.pid.unwrap());
    assert_eq!(observation.identity.plan_id, summary.id);
    assert_eq!(
        observation.identity.executable_sha256,
        summary.manifest.executable.sha256
    );
    assert_eq!(
        observation.identity.execution_kind,
        ExecutionKind::CompatibleFixture
    );
    assert!(matches!(
        observation.placement,
        StartupPlacement::Unknown { .. }
    ));
    assert!(observation.retention_closed);
    assert!(before
        .lines()
        .iter()
        .any(|line| line.text() == "startup-only stdout marker"));
    assert!(before
        .lines()
        .iter()
        .any(|line| line.text() == "startup key=[redacted]"));
    assert!(before
        .lines()
        .iter()
        .any(|line| line.text().contains("untrusted synthetic Metal")));
    let public = serde_json::to_string(observation).unwrap();
    assert!(!public.contains("fixture model path"));
    assert!(!public.contains(&summary.manifest.model.path.to_string_lossy().to_string()));
    assert!(!format!("{observation:?}").contains("startup key="));
    let wrong_owner = Target {
        owner: uuid::Uuid::new_v4(),
        ..ready.target
    };
    assert!(matches!(
        runtime.startup_diagnostics(wrong_owner).await,
        Err(Failure::StaleGeneration)
    ));
    let mut generation = run(&runtime, ready.target).await;
    assert_eq!(generation.wait().await.unwrap().text, "fixture output");
    let after = runtime
        .startup_diagnostics(ready.target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.observation.stdout, before.observation.stdout);
    assert_eq!(after.observation.stderr, before.observation.stderr);
    assert_eq!(
        before
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>(),
        after
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
    );
    assert!(!after
        .lines()
        .iter()
        .any(|line| line.text().contains("private prompt")));
    assert_eq!(runtime.status().observed_backend, Evidence::Unknown);
    reaped(
        &runtime
            .unload(ready.target, "test/model".into())
            .await
            .unwrap(),
    );
    assert!(runtime
        .startup_observation(ready.target)
        .await
        .unwrap()
        .is_some());
    let (_next_dir, next) = plan("normal").await;
    let next_ready = load(&runtime, next).await;
    assert_ne!(next_ready.target, ready.target);
    assert!(matches!(
        runtime.startup_diagnostics(ready.target).await,
        Err(Failure::StaleGeneration)
    ));
    let current = runtime
        .startup_diagnostics(next_ready.target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.observation.identity.target, next_ready.target);
    assert!(current.lines().is_empty());
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn cancelled_startup_freezes_diagnostics_and_reaps_output_owners() {
    let (_dir, approved) = plan("startup_stall").await;
    let runtime = Runtime::new();
    let cancel = Cancellation::new();
    let loading = {
        let runtime = runtime.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { runtime.load(0, approved, cancel).await })
    };
    let target = timeout(Duration::from_secs(2), async {
        loop {
            let state = runtime.status();
            if state.phase == Phase::Loading {
                break state.target;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(runtime.startup_diagnostics(target).await.unwrap().is_none());
    cancel.cancel();
    assert_eq!(loading.await.unwrap().unwrap_err(), Failure::Cancelled);
    let diagnostic = runtime.startup_diagnostics(target).await.unwrap().unwrap();
    assert!(diagnostic.observation.retention_closed);
    assert_eq!(diagnostic.observation.identity.target, target);
    reaped(&runtime.status());
    reaped(&runtime.shutdown().await.unwrap());
}

#[tokio::test]
async fn reported_startup_allocations_keep_fixture_identity_and_do_not_become_profiling() {
    let (_dir, approved) = plan("startup_report").await;
    let runtime = Runtime::new();
    let ready = load(&runtime, approved).await;
    let observation = runtime
        .startup_observation(ready.target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observation.identity.target, ready.target);
    assert_eq!(observation.identity.pid, ready.pid.unwrap());
    assert_eq!(
        observation.identity.execution_kind,
        ExecutionKind::CompatibleFixture
    );
    assert!(!observation.kernel_execution_verified);
    let StartupPlacement::RuntimeReported {
        offload,
        model_buffers,
        ..
    } = observation.placement
    else {
        panic!("expected exact synthetic source-format records");
    };
    assert_eq!(offload.offloaded_layers, 40);
    assert_eq!(offload.total_layers, 41);
    assert_eq!(model_buffers[0].kind, ReportedBufferKind::MetalMapped);
    assert_eq!(model_buffers[0].size_mib_hundredths, 509_918);
    assert_eq!(runtime.status().observed_backend, Evidence::Unknown);
    reaped(&runtime.shutdown().await.unwrap());
}
