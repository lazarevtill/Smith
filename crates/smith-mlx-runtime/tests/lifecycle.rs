#![cfg(unix)]
use smith_mlx_runtime::*;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};
static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
fn fixture() -> &'static Path {
    &FIXTURE
        .get_or_init(|| {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("fixture");
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            assert!(std::process::Command::new(rustc)
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/worker.rs"))
                .args(["--edition=2021", "-o"])
                .arg(&p)
                .status()
                .unwrap()
                .success());
            (d, p)
        })
        .1
}
struct Prepared {
    dir: tempfile::TempDir,
    worker: PathBuf,
    plan: LaunchPlan,
}
async fn prepare(mode: &str, chat: bool) -> Prepared {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let worker = root.join(mode);
    std::fs::copy(fixture(), &worker).unwrap();
    let model = root.join("model");
    std::fs::create_dir(&model).unwrap();
    for name in [
        "config.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "model.safetensors",
    ] {
        std::fs::write(model.join(name), format!("synthetic {name}")).unwrap();
    }
    let budgets = Budgets {
        hello: Duration::from_secs(10),
        operation: Duration::from_secs(2),
        generation: Duration::from_millis(300),
        drain: Duration::from_millis(200),
        reap: Duration::from_secs(1),
    };
    let plan = LaunchPlan::prepare(
        SelectedWorker {
            path: worker.clone(),
            source: "synthetic test binary".into(),
            source_revision: "fixture-v1".into(),
            execution_kind: ExecutionKind::CompatibleFixture,
        },
        &model,
        budgets,
        &Cancellation::new(),
    )
    .await
    .unwrap();
    let manifest = Manifest {
        model_type: "llama".into(),
        files: plan.summary().model.files.clone(),
        package_sha256: plan.summary().model.package_sha256.clone(),
        template: if chat {
            Some(Template::SimpleChatml)
        } else {
            None
        },
        context_limit: 4096,
    };
    std::fs::write(
        worker.with_extension("manifest"),
        serde_json::to_vec(
            &serde_json::json!({"manifest":manifest,"capabilities":manifest.capabilities()}),
        )
        .unwrap(),
    )
    .unwrap();
    Prepared { dir, worker, plan }
}
async fn launch(p: Prepared) -> (tempfile::TempDir, PathBuf, Runtime) {
    let id = p.plan.summary().id.clone();
    let runtime = Runtime::launch(p.plan.approve(&id).unwrap(), Cancellation::new())
        .await
        .unwrap();
    (p.dir, p.worker, runtime)
}
async fn op(runtime: &Runtime, request: Request) -> Result<Outcome> {
    runtime
        .begin(runtime.status().revision, request)?
        .wait()
        .await
}
fn raw() -> Request {
    Request::Generate(GenerationRequest::Raw {
        prompt: "exact caller raw prompt".into(),
        sampling: Sampling::default(),
    })
}
const THINKING_LITERAL: &str = "<think>\n\n</think>\n\nLiteral fixture response.";
fn thinking(mode: ThinkingMode) -> GenerationRequest {
    GenerationRequest::ChatWithThinking {
        messages: vec![Message {
            role: Role::User,
            content: "Synthetic literal /no_think input".into(),
        }],
        sampling: Sampling::default(),
        thinking_mode: mode,
    }
}
async fn prepare_thinking(mode: &str, template: Option<Template>) -> Prepared {
    let p = prepare(mode, true).await;
    let path = p.worker.with_extension("manifest");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["manifest"]["model_type"] = "qwen3".into();
    value["manifest"]["template"] = serde_json::to_value(template).unwrap();
    let manifest: Manifest = serde_json::from_value(value["manifest"].clone()).unwrap();
    value["capabilities"] = serde_json::to_value(manifest.capabilities()).unwrap();
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    p
}
fn reaped(s: Snapshot) {
    assert_eq!(s.spawned, s.reaped);
    assert_eq!(s.pid, None);
    assert!(!s.accepting);
}
fn scratch(worker: &Path) -> PathBuf {
    PathBuf::from(std::fs::read_to_string(worker.with_extension("scratch")).unwrap())
}
async fn progress(op: &Operation) {
    let mut rx = op.subscribe();
    tokio::time::timeout(Duration::from_secs(2), async {
        while rx.borrow().text.is_empty() {
            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inspect_load_utf8_revision_chat_unload_shutdown_are_real_protocol_operations() {
    let (_d, worker, r) = launch(prepare("normal", true).await).await;
    assert_eq!(r.status().execution_kind, ExecutionKind::CompatibleFixture);
    assert_eq!(r.status().phase, Phase::Idle);
    let inspected = op(&r, Request::Inspect).await.unwrap();
    assert!(matches!(inspected, Outcome::Manifest(_)));
    assert!(r.status().manifest.is_none());
    op(&r, Request::Load).await.unwrap();
    assert_eq!(r.status().manifest.as_ref().unwrap().context_limit, 4096);
    let mut generation = r.begin(r.status().revision, raw()).unwrap();
    let updates = generation.subscribe();
    let result = generation.wait().await.unwrap();
    let Outcome::Completion(result) = result else {
        panic!()
    };
    assert_eq!(result.text, "hello 🌍");
    assert!(!result.incomplete);
    assert_eq!(updates.borrow().revision, 2);
    assert_eq!(updates.borrow().text, "hello 🌍");
    assert!(updates.borrow().finished);
    let request = Request::Generate(GenerationRequest::Chat {
        messages: vec![Message {
            role: Role::User,
            content: "chat is explicitly admitted".into(),
        }],
        sampling: Sampling::default(),
    });
    op(&r, request).await.unwrap();
    op(&r, Request::Unload).await.unwrap();
    assert!(r.status().manifest.is_none());
    let owned = scratch(&worker);
    assert!(owned.exists());
    reaped(r.shutdown().await.unwrap());
    assert!(!owned.exists());
}

#[tokio::test]
async fn legacy_worker_preserves_default_and_refuses_disabled_before_any_dispatch() {
    let (_d, worker, runtime) =
        launch(prepare_thinking("thinking_legacy", Some(Template::Qwen3DefaultThinking)).await)
            .await;
    assert!(!runtime.status().worker_features.qwen3_thinking_control_v1);
    assert_eq!(
        runtime.validate_chat_thinking(runtime.status().revision, ThinkingMode::Disabled),
        Err(Failure::NotLoaded)
    );
    op(&runtime, Request::Load).await.unwrap();
    let revision = runtime.status().revision;
    assert_eq!(
        runtime.validate_chat_thinking(revision, ThinkingMode::Disabled),
        Err(Failure::UnsupportedCapability)
    );
    assert_eq!(
        runtime.generation_provenance(revision, &thinking(ThinkingMode::Disabled)),
        Err(Failure::UnsupportedCapability)
    );
    let mut rejected = runtime
        .begin(
            revision,
            Request::Generate(thinking(ThinkingMode::Disabled)),
        )
        .unwrap();
    assert_eq!(
        rejected.wait().await.unwrap_err(),
        Failure::UnsupportedCapability
    );
    assert!(rejected.provenance().is_none());
    assert!(!worker.with_extension("requests").exists());
    assert_eq!(runtime.status().phase, Phase::Ready);
    runtime
        .validate_chat_thinking(revision, ThinkingMode::ModelDefault)
        .unwrap();
    let Outcome::Completion(result) = op(
        &runtime,
        Request::Generate(thinking(ThinkingMode::ModelDefault)),
    )
    .await
    .unwrap() else {
        panic!()
    };
    assert_eq!(result.text, THINKING_LITERAL);
    assert_eq!(result.thinking_mode, None);
    let sent: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(worker.with_extension("requests"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert!(sent.get("thinking_mode").is_none());
    reaped(runtime.shutdown().await.unwrap());
    assert!(!scratch(&worker).exists());
}

#[tokio::test]
async fn disabled_requires_exact_template_even_when_worker_advertises_feature() {
    for template in [
        None,
        Some(Template::SimpleChatml),
        Some(Template::SimpleGranite),
    ] {
        let (_d, worker, runtime) =
            launch(prepare_thinking("thinking_other", template).await).await;
        assert!(runtime.status().worker_features.qwen3_thinking_control_v1);
        op(&runtime, Request::Load).await.unwrap();
        let revision = runtime.status().revision;
        assert_eq!(
            runtime.validate_chat_thinking(revision, ThinkingMode::Disabled),
            Err(Failure::UnsupportedCapability)
        );
        assert_eq!(
            op(
                &runtime,
                Request::Generate(thinking(ThinkingMode::Disabled))
            )
            .await
            .unwrap_err(),
            Failure::UnsupportedCapability
        );
        assert!(!worker.with_extension("requests").exists());
        reaped(runtime.shutdown().await.unwrap());
    }
}

#[tokio::test]
async fn explicit_mode_acknowledgement_preserves_literal_text_and_exact_immutable_provenance() {
    let prepared = prepare_thinking("thinking_normal", Some(Template::Qwen3DefaultThinking)).await;
    let summary = prepared.plan.summary().clone();
    let (_d, worker, runtime) = launch(prepared).await;
    op(&runtime, Request::Load).await.unwrap();
    let state = runtime.status();
    runtime
        .validate_chat_thinking(state.revision, ThinkingMode::Disabled)
        .unwrap();
    assert_eq!(
        runtime.validate_chat_thinking(state.revision - 1, ThinkingMode::Disabled),
        Err(Failure::StaleRevision)
    );
    let mut request = thinking(ThinkingMode::Disabled);
    let captured = runtime
        .generation_provenance(state.revision, &request)
        .unwrap();
    assert_eq!(captured.runtime_plan_id, summary.id);
    assert_eq!(captured.worker_sha256, summary.worker.sha256);
    assert_eq!(
        captured.runtime_bundle_sha256,
        summary.runtime_bundle_sha256
    );
    assert_eq!(captured.package_sha256, summary.model.package_sha256);
    assert_eq!(captured.session_id, state.session_id);
    assert_eq!(captured.runtime_revision, state.revision);
    assert_eq!(captured.template, Some(Template::Qwen3DefaultThinking));
    assert_eq!(captured.thinking_mode, Some(ThinkingMode::Disabled));
    assert_eq!(captured.capability, Capability::Chat);
    let encoded = serde_json::to_string(&captured).unwrap();
    assert!(!encoded.contains("Synthetic literal"));
    assert!(!encoded.contains(summary.model.root.to_str().unwrap()));
    let mut operation = runtime
        .begin(state.revision, Request::Generate(request.clone()))
        .unwrap();
    if let GenerationRequest::ChatWithThinking {
        sampling,
        thinking_mode,
        ..
    } = &mut request
    {
        sampling.max_tokens = 1;
        *thinking_mode = ThinkingMode::ModelDefault;
    }
    let Outcome::Completion(result) = operation.wait().await.unwrap() else {
        panic!()
    };
    assert_eq!(result.text, THINKING_LITERAL);
    assert_eq!(operation.snapshot().text, THINKING_LITERAL);
    assert_eq!(result.thinking_mode, Some(ThinkingMode::Disabled));
    assert!(!result.incomplete);
    assert_eq!(operation.provenance(), Some(&captured));
    let sent: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(worker.with_extension("requests"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(sent["thinking_mode"], "disabled");
    assert_eq!(sent["max_tokens"], 128);
    assert_eq!(
        sent["messages"][0]["content"],
        "Synthetic literal /no_think input"
    );
    reaped(runtime.shutdown().await.unwrap());
    assert_eq!(operation.provenance(), Some(&captured));
    assert_eq!(
        runtime.generation_provenance(runtime.status().revision, &request),
        Err(Failure::ChildExited)
    );
}

#[tokio::test]
async fn missing_mismatched_unknown_null_and_unsolicited_thinking_acknowledgements_retire_worker() {
    for mode in [
        "thinking_missing_ack",
        "thinking_wrong_ack",
        "thinking_unknown_ack",
        "thinking_null_ack",
        "thinking_extra_default_ack",
    ] {
        let (_d, worker, runtime) =
            launch(prepare_thinking(mode, Some(Template::Qwen3DefaultThinking)).await).await;
        op(&runtime, Request::Load).await.unwrap();
        let requested = if mode == "thinking_extra_default_ack" {
            ThinkingMode::ModelDefault
        } else {
            ThinkingMode::Disabled
        };
        let mut operation = runtime
            .begin(
                runtime.status().revision,
                Request::Generate(thinking(requested)),
            )
            .unwrap();
        assert_eq!(operation.wait().await.unwrap_err(), Failure::Protocol);
        assert_eq!(operation.snapshot().text, THINKING_LITERAL);
        assert!(operation.snapshot().incomplete);
        assert_eq!(
            operation.provenance().unwrap().thinking_mode,
            Some(requested)
        );
        reaped(runtime.shutdown().await.unwrap());
        assert!(!scratch(&worker).exists());
    }
}

#[tokio::test]
async fn cancelled_thinking_operation_keeps_requested_provenance_without_success_acknowledgement() {
    let (_d, worker, runtime) =
        launch(prepare_thinking("thinking_cancel", Some(Template::Qwen3DefaultThinking)).await)
            .await;
    op(&runtime, Request::Load).await.unwrap();
    let mut operation = runtime
        .begin(
            runtime.status().revision,
            Request::Generate(thinking(ThinkingMode::Disabled)),
        )
        .unwrap();
    progress(&operation).await;
    let captured = operation.provenance().unwrap().clone();
    operation.cancel();
    let Outcome::Completion(result) = operation.wait().await.unwrap() else {
        panic!()
    };
    assert!(result.incomplete);
    assert_eq!(result.text, THINKING_LITERAL);
    assert_eq!(result.finish_reason, FinishReason::Cancelled);
    assert_eq!(result.thinking_mode, None);
    assert_eq!(operation.provenance(), Some(&captured));
    assert_eq!(captured.thinking_mode, Some(ThinkingMode::Disabled));
    reaped(runtime.shutdown().await.unwrap());
    assert!(!scratch(&worker).exists());
}

#[tokio::test]
async fn spontaneous_cancelled_terminal_cannot_claim_successful_thinking_acknowledgement() {
    let (_d, worker, runtime) = launch(
        prepare_thinking(
            "thinking_spontaneous_cancel",
            Some(Template::Qwen3DefaultThinking),
        )
        .await,
    )
    .await;
    op(&runtime, Request::Load).await.unwrap();
    let mut operation = runtime
        .begin(
            runtime.status().revision,
            Request::Generate(thinking(ThinkingMode::Disabled)),
        )
        .unwrap();
    // No host cancel request: the worker itself emits done/cancelled with an exact option echo.
    let Outcome::Completion(result) = operation.wait().await.unwrap() else {
        panic!()
    };
    assert_eq!(result.finish_reason, FinishReason::Cancelled);
    assert!(result.incomplete);
    assert_eq!(result.thinking_mode, None);
    assert_eq!(result.text, THINKING_LITERAL);
    assert!(operation.snapshot().incomplete);
    assert_eq!(
        operation.provenance().unwrap().thinking_mode,
        Some(ThinkingMode::Disabled)
    );
    reaped(runtime.shutdown().await.unwrap());
    assert!(!scratch(&worker).exists());
}
#[tokio::test]
async fn stale_revision_busy_and_unsupported_chat_do_not_dispatch() {
    let (_d, _worker, r) = launch(prepare("stall", false).await).await;
    let mut stale = r.begin(0, Request::Load).unwrap();
    assert_eq!(stale.wait().await.unwrap_err(), Failure::StaleRevision);
    op(&r, Request::Load).await.unwrap();
    let chat = Request::Generate(GenerationRequest::Chat {
        messages: vec![Message {
            role: Role::User,
            content: "no template".into(),
        }],
        sampling: Sampling::default(),
    });
    assert_eq!(
        op(&r, chat).await.unwrap_err(),
        Failure::UnsupportedCapability
    );
    assert_eq!(r.status().phase, Phase::Ready);
    let mut generation = r.begin(r.status().revision, raw()).unwrap();
    assert!(matches!(
        r.begin(r.status().revision, Request::Unload),
        Err(Failure::Busy)
    ));
    progress(&generation).await;
    generation.cancel();
    let result = generation.wait().await.unwrap();
    assert!(matches!(
        result,
        Outcome::Completion(Completion {
            incomplete: true,
            finish_reason: FinishReason::Cancelled,
            ..
        })
    ));
    reaped(r.shutdown().await.unwrap());
}
#[tokio::test]
async fn cancellation_accepts_both_ack_orders_and_ignored_cancel_kills() {
    for mode in ["stall", "terminal_first", "ignore_cancel"] {
        let (_d, worker, r) = launch(prepare(mode, false).await).await;
        op(&r, Request::Load).await.unwrap();
        let mut g = r.begin(r.status().revision, raw()).unwrap();
        progress(&g).await;
        g.cancel();
        let out = g.wait().await.unwrap();
        let Outcome::Completion(c) = out else {
            panic!()
        };
        assert_eq!(c.text, "hél");
        assert!(c.incomplete);
        assert_eq!(c.finish_reason, FinishReason::Cancelled);
        reaped(r.status());
        assert!(!scratch(&worker).exists());
        r.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn dropped_operation_and_generation_deadline_reap_and_keep_partial_incomplete() {
    let (_d, worker, r) = launch(prepare("ignore_cancel", false).await).await;
    op(&r, Request::Load).await.unwrap();
    let g = r.begin(r.status().revision, raw()).unwrap();
    progress(&g).await;
    let mut updates = g.subscribe();
    drop(g);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !updates.borrow().finished {
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(updates.borrow().incomplete);
    assert_eq!(updates.borrow().text, "hél");
    reaped(r.shutdown().await.unwrap());
    assert!(!scratch(&worker).exists());
    let (_d, _worker, r) = launch(prepare("ignore_cancel", false).await).await;
    op(&r, Request::Load).await.unwrap();
    let mut g = r.begin(r.status().revision, raw()).unwrap();
    assert_eq!(g.wait().await.unwrap_err(), Failure::Deadline);
    reaped(r.shutdown().await.unwrap());
}
#[tokio::test]
async fn malformed_stale_and_unbounded_frames_retire_owned_session() {
    for mode in [
        "bad_boundary",
        "oversized",
        "stderr_flood",
        "duplicate_key",
        "unfinished",
        "invalid_utf8",
        "unknown_field",
        "done_early",
    ] {
        let (_d, worker, r) = launch(prepare(mode, false).await).await;
        op(&r, Request::Load).await.unwrap();
        let result = op(&r, raw()).await;
        assert!(
            matches!(result, Err(Failure::Protocol | Failure::OutputLimit)),
            "{mode}: {result:?}"
        );
        reaped(r.shutdown().await.unwrap());
        assert!(!scratch(&worker).exists());
    }
    for mode in ["stale", "mismatch", "worker_error"] {
        let (_d, worker, r) = launch(prepare(mode, false).await).await;
        assert!(op(&r, Request::Load).await.is_err());
        reaped(r.shutdown().await.unwrap());
        assert!(!scratch(&worker).exists());
    }
}
#[tokio::test]
async fn exact_plan_and_revalidated_selection_are_required_before_spawn() {
    let p = prepare("normal", false).await;
    assert!(matches!(
        p.plan.approve("wrong"),
        Err(Failure::ConsentMismatch)
    ));
    let p = prepare("normal", false).await;
    std::fs::write(p.plan.summary().model.root.join("config.json"), b"changed").unwrap();
    let id = p.plan.summary().id.clone();
    assert!(matches!(
        Runtime::launch(p.plan.approve(&id).unwrap(), Cancellation::new()).await,
        Err(Failure::SelectionChanged)
    ));
    assert!(!p.worker.with_extension("pid").exists());
    let p = prepare("normal", false).await;
    let cancel = Cancellation::new();
    cancel.cancel();
    let id = p.plan.summary().id.clone();
    assert!(matches!(
        Runtime::launch(p.plan.approve(&id).unwrap(), cancel).await,
        Err(Failure::Cancelled)
    ));
    assert!(!p.worker.with_extension("pid").exists());
}
#[tokio::test]
async fn runtime_drop_and_bad_hello_reap_and_remove_scratch() {
    let (_d, worker, r) = launch(prepare("normal", false).await).await;
    let path = scratch(&worker);
    let mut status = r.subscribe();
    drop(r);
    tokio::time::timeout(Duration::from_secs(3), async {
        while status.borrow().pid.is_some() {
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(!path.exists());
    reaped(status.borrow().clone());
    let p = prepare("bad_hello", false).await;
    let id = p.plan.summary().id.clone();
    assert!(matches!(
        Runtime::launch(p.plan.approve(&id).unwrap(), Cancellation::new()).await,
        Err(Failure::Protocol)
    ));
    assert!(!scratch(&p.worker).exists());
}
#[tokio::test]
async fn symlinks_hardlinks_and_extra_code_are_rejected_without_execution() {
    use std::os::unix::fs::symlink;
    for mode in ["symlink", "hardlink", "code"] {
        let p = prepare("normal", false).await;
        let root = &p.plan.summary().model.root;
        match mode {
            "symlink" => symlink(root.join("config.json"), root.join("extra.md")).unwrap(),
            "hardlink" => {
                std::fs::hard_link(root.join("config.json"), root.join("extra.md")).unwrap()
            }
            _ => std::fs::write(root.join("model.py"), b"never execute").unwrap(),
        };
        let id = p.plan.summary().id.clone();
        assert!(
            Runtime::launch(p.plan.approve(&id).unwrap(), Cancellation::new())
                .await
                .is_err()
        );
        assert!(!p.worker.with_extension("pid").exists());
    }
}

#[tokio::test]
async fn runtime_clones_and_cancelled_shutdown_waiters_share_confirmed_cleanup() {
    let (_dir, worker, runtime) = launch(prepare("ignore_cancel", false).await).await;
    let r = runtime.clone();
    drop(runtime);
    op(&r, Request::Load).await.unwrap();
    let mut generation = r.begin(r.status().revision, raw()).unwrap();
    progress(&generation).await;
    let pid = r.status().pid.unwrap() as i32;
    let waiter = r.clone();
    let first = tokio::spawn(async move { waiter.shutdown().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    first.abort();
    let _ = first.await;
    let second = r.clone();
    let third = r.clone();
    let (a, b) = tokio::join!(second.shutdown(), third.shutdown());
    reaped(a.unwrap());
    reaped(b.unwrap());
    reaped(r.shutdown().await.unwrap());
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert!(!scratch(&worker).exists());
    let Outcome::Completion(result) = generation.wait().await.unwrap() else {
        panic!()
    };
    assert_eq!(result.text, "hél");
    assert!(result.incomplete);
}

#[tokio::test]
async fn failed_generation_preserves_the_last_full_revision_and_rejects_followup() {
    let (_dir, _worker, r) = launch(prepare("bad_boundary", false).await).await;
    op(&r, Request::Load).await.unwrap();
    let mut generation = r.begin(r.status().revision, raw()).unwrap();
    assert_eq!(generation.wait().await.unwrap_err(), Failure::Protocol);
    let p = generation.snapshot();
    assert_eq!(p.text, "hél");
    assert_eq!(p.revision, 1);
    assert!(p.incomplete && p.finished);
    assert!(matches!(
        r.begin(r.status().revision, raw()),
        Err(Failure::ChildExited)
    ));
    reaped(r.shutdown().await.unwrap());
}

#[tokio::test]
async fn normalized_aliases_and_replaced_executables_cannot_reuse_approval() {
    let p = prepare("normal", false).await;
    let path = PathBuf::from(format!("{}/.", p.plan.summary().model.root.display()));
    let selected = SelectedWorker {
        path: p.worker.clone(),
        source: "fixture".into(),
        source_revision: "v1".into(),
        execution_kind: ExecutionKind::CompatibleFixture,
    };
    assert!(matches!(
        LaunchPlan::prepare(selected, &path, Budgets::default(), &Cancellation::new()).await,
        Err(Failure::InvalidSelection)
    ));
    let p = prepare("normal", false).await;
    std::fs::write(&p.worker, b"not the selected executable").unwrap();
    let id = p.plan.summary().id.clone();
    assert!(
        Runtime::launch(p.plan.approve(&id).unwrap(), Cancellation::new())
            .await
            .is_err()
    );
    assert!(!p.worker.with_extension("pid").exists());
}

#[tokio::test]
async fn malformed_shutdown_and_idle_replay_are_faults_even_after_confirmed_reap() {
    let (_dir, worker, r) = launch(prepare("bad_shutdown", false).await).await;
    let s = r.shutdown().await.unwrap();
    assert_eq!(s.phase, Phase::Failed);
    assert_eq!(s.failure, Some(Failure::Protocol));
    reaped(s);
    assert!(!scratch(&worker).exists());
    let (_dir, worker, r) = launch(prepare("late_terminal", false).await).await;
    op(&r, Request::Load).await.unwrap();
    let _ = op(&r, raw()).await;
    let mut status = r.subscribe();
    tokio::time::timeout(Duration::from_secs(3), async {
        while status.borrow().pid.is_some() {
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(r.status().failure, Some(Failure::Protocol));
    reaped(r.shutdown().await.unwrap());
    assert!(!scratch(&worker).exists());
}

#[tokio::test]
async fn shutdown_answers_a_queued_request_as_cancelled_after_cleanup() {
    let (_dir, _worker, r) = launch(prepare("normal", false).await).await;
    let mut pending = r.begin(r.status().revision, Request::Load).unwrap();
    // No await between enqueue and shutdown: this operation has not dispatched.
    r.begin_shutdown();
    reaped(r.shutdown().await.unwrap());
    assert_eq!(pending.wait().await.unwrap_err(), Failure::Cancelled);
    assert!(pending.snapshot().finished);
    assert_eq!(pending.snapshot().revision, 0);
}

#[tokio::test]
async fn cancelled_generation_reports_post_reap_cleanup_uncertainty_and_retains_progress() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, worker, runtime) = launch(prepare("cancel_cleanup", false).await).await;
    op(&runtime, Request::Load).await.unwrap();
    let owned = scratch(&worker);
    let mut generation = runtime.begin(runtime.status().revision, raw()).unwrap();
    progress(&generation).await;
    generation.cancel();
    let result = generation.wait().await;
    let state = runtime.status();
    let progress = generation.snapshot();
    let cleanup_result = runtime.shutdown().await;
    let was_retained = owned.exists();
    let reuse = runtime.begin(runtime.status().revision, Request::Load);

    // Restore the real removal failure before asserting the operation result,
    // so even a regression failure leaves the retained cleanup owner able to finish.
    std::fs::set_permissions(owned.join("locked"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut status = runtime.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        while status.borrow().failure == Some(Failure::Cleanup) {
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let recovered = runtime.shutdown().await.unwrap();

    assert_eq!(result.unwrap_err(), Failure::Cleanup);
    reaped(state.clone());
    assert_eq!(state.failure, Some(Failure::Cleanup));
    assert_eq!(cleanup_result.unwrap_err(), Failure::Cleanup);
    assert!(was_retained);
    assert!(matches!(reuse, Err(Failure::ChildExited)));
    assert_eq!(progress.text, "hél");
    assert!(progress.incomplete && progress.finished);
    reaped(recovered.clone());
    assert_eq!(recovered.failure, Some(Failure::Cancelled));
    assert!(!owned.exists());
}

#[tokio::test]
async fn cleanup_fault_retains_scratch_and_later_reports_confirmed_recovery() {
    use std::os::unix::fs::PermissionsExt;
    for mode in ["cleanup_fault", "bad_shutdown_cleanup"] {
        let (_dir, worker, r) = launch(prepare(mode, false).await).await;
        let owned = scratch(&worker);
        assert_eq!(r.shutdown().await.unwrap_err(), Failure::Cleanup);
        let state = r.status();
        assert_eq!(state.spawned, state.reaped);
        assert_eq!(state.failure, Some(Failure::Cleanup));
        assert!(owned.exists());
        std::fs::set_permissions(owned.join("locked"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let mut status = r.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            while status.borrow().failure == Some(Failure::Cleanup) {
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let state = r.shutdown().await.unwrap();
        reaped(state.clone());
        assert!(!owned.exists());
        assert_eq!(
            state.failure,
            if mode == "bad_shutdown_cleanup" {
                Some(Failure::Protocol)
            } else {
                None
            }
        );
    }
}

#[tokio::test]
async fn failed_launch_returns_the_retained_cleanup_receipt_until_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let p = prepare("bad_hello_cleanup", false).await;
    let id = p.plan.summary().id.clone();
    let failed =
        match Runtime::launch_owned(p.plan.approve(&id).unwrap(), Cancellation::new()).await {
            Ok(_) => panic!("malformed hello must fail"),
            Err(failure) => failure,
        };
    assert_eq!(failed.error, Failure::Cleanup);
    assert_eq!(failed.runtime.status().failure, Some(Failure::Cleanup));
    let owned = scratch(&p.worker);
    assert!(owned.exists());
    std::fs::set_permissions(owned.join("locked"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut status = failed.runtime.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        while status.borrow().failure == Some(Failure::Cleanup) {
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let result = failed.runtime.shutdown().await.unwrap();
    assert_eq!(result.failure, Some(Failure::Protocol));
    reaped(result);
    assert!(!owned.exists());
}

#[cfg(target_os = "macos")]
fn resource_fixture(parent: &Path) {
    for name in [
        "mlx.metallib",
        "mlx-swift_Cmlx.bundle/Contents/Info.plist",
        "mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib",
        "swift-transformers_Hub.bundle/gpt2_tokenizer_config.json",
        "swift-transformers_Hub.bundle/t5_tokenizer_config.json",
        "swift-crypto_Crypto.bundle/PrivacyInfo.xcprivacy",
    ] {
        let path = parent.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"synthetic resource bytes").unwrap();
    }
}
#[cfg(target_os = "macos")]
async fn native_plan(p: &Prepared) -> Result<LaunchPlan> {
    LaunchPlan::prepare(
        SelectedWorker {
            path: p.worker.clone(),
            source: "synthetic native-transport fixture, no MLX".into(),
            source_revision: "fixture-v1".into(),
            execution_kind: ExecutionKind::Native,
        },
        &p.plan.summary().model.root,
        p.plan.summary().budgets,
        &Cancellation::new(),
    )
    .await
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_bundle_requires_exact_resources_and_runs_only_private_verified_copy() {
    let mut p = prepare("normal", false).await;
    assert!(matches!(
        native_plan(&p).await,
        Err(Failure::InvalidSelection)
    ));
    resource_fixture(p.worker.parent().unwrap());
    p.plan = native_plan(&p).await.unwrap();
    assert_eq!(p.plan.summary().runtime_files.len(), 7);
    assert_eq!(p.plan.summary().runtime_bundle_sha256.len(), 64);
    let (_dir, worker, r) = launch(p).await;
    assert!(
        !worker.with_extension("pid").exists(),
        "source worker must not execute in Native mode"
    );
    reaped(r.shutdown().await.unwrap());
    for mutation in ["changed", "symlink", "missing"] {
        let mut p = prepare("normal", false).await;
        resource_fixture(p.worker.parent().unwrap());
        p.plan = native_plan(&p).await.unwrap();
        let path = p
            .worker
            .parent()
            .unwrap()
            .join("swift-transformers_Hub.bundle/t5_tokenizer_config.json");
        match mutation {
            "changed" => std::fs::write(&path, b"changed resource").unwrap(),
            "missing" => std::fs::remove_file(&path).unwrap(),
            _ => {
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(p.worker.parent().unwrap().join("mlx.metallib"), &path)
                    .unwrap();
            }
        }
        let id = p.plan.summary().id.clone();
        assert!(
            Runtime::launch(p.plan.approve(&id).unwrap(), Cancellation::new())
                .await
                .is_err()
        );
        assert!(!p.worker.with_extension("pid").exists());
    }
}
