#![cfg(any(target_os = "macos", target_os = "linux"))]
use smith_managed_runtime::*;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};
#[path = "../../../src/runtime_catalog.rs"]
mod runtime_catalog;
static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
fn fixture() -> &'static Path {
    &FIXTURE
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("probe-fixture");
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            assert!(std::process::Command::new(rustc)
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime_probe.rs"))
                .args(["--edition=2021", "-o"])
                .arg(&bin)
                .status()
                .unwrap()
                .success());
            (dir, bin)
        })
        .1
}
async fn prepare(mode: &str) -> (tempfile::TempDir, RuntimeProbePlan) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(mode);
    std::fs::copy(fixture(), &file).unwrap();
    let artifact = inspect_selected_artifact(
        &file,
        ArtifactKind::Executable,
        "synthetic test binary".into(),
        "fixture-v1".into(),
        &Cancellation::new(),
    )
    .await
    .unwrap();
    assert_eq!(artifact.sha256.len(), 64);
    let plan = RuntimeProbePlan::prepare(artifact, &Cancellation::new())
        .await
        .unwrap();
    (dir, plan)
}
fn approve(plan: RuntimeProbePlan) -> ApprovedRuntimeProbe {
    let id = plan.summary().id.clone();
    plan.approve(&id).unwrap()
}
async fn pid(dir: &Path, name: &str) -> i32 {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(dir.join(name).with_extension("pid")) {
                break pid.parse().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
fn gone(pid: i32) {
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "owned child must be reaped"
    );
}

#[tokio::test]
async fn explicit_probe_observes_devices_and_fixed_arguments_only() {
    let (dir, plan) = prepare("normal").await;
    assert_eq!(plan.summary().commands, ["--help", "--list-devices"]);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "prepare must not execute"
    );
    let mut probe = RuntimeProbe::start(approve(plan), Cancellation::new());
    let report = probe.wait().await.unwrap();
    assert!(report.compatibility.protocol_candidate());
    assert!(report.compatibility.supports_flag("--threads-batch"));
    assert!(report.help.reaped && report.inventory.reaped);
    assert!(!report.inference_verified);
    assert_eq!(report.devices.len(), 5);
    assert_eq!(report.devices[1].backend, DeviceBackend::Metal);
    assert_eq!(report.devices[4].backend, DeviceBackend::Sycl);
    gone(pid(dir.path(), "normal").await);
}
#[tokio::test]
async fn terminal_probe_drop_does_not_cancel_the_callers_remaining_setup() {
    for mode in ["normal", "invalid"] {
        let (_dir, plan) = prepare(mode).await;
        let cancellation = Cancellation::new();
        let mut probe = RuntimeProbe::start(approve(plan), cancellation.clone());
        let result = probe.wait().await;
        assert_eq!(result.is_ok(), mode == "normal");
        drop(probe);
        assert!(
            !cancellation.is_cancelled(),
            "terminal {mode} probe must not impersonate user cancellation"
        );
    }
}
#[tokio::test]
async fn exact_approval_and_selected_identity_are_required() {
    let (_dir, plan) = prepare("normal").await;
    assert!(matches!(
        plan.approve("wrong"),
        Err(Failure::ConsentMismatch)
    ));
    let (dir, plan) = prepare("normal").await;
    std::fs::write(dir.path().join("normal"), b"changed").unwrap();
    let mut probe = RuntimeProbe::start(approve(plan), Cancellation::new());
    assert_eq!(probe.wait().await.unwrap_err(), Failure::InvalidManifest);
    assert!(!dir.path().join("normal.pid").exists());
}
#[tokio::test]
async fn cancel_drop_flood_and_deadline_reap_owned_processes() {
    let (dir, plan) = prepare("stall").await;
    let cancel = Cancellation::new();
    let mut probe = RuntimeProbe::start(approve(plan), cancel.clone());
    let child = pid(dir.path(), "stall").await;
    cancel.cancel();
    assert_eq!(probe.wait().await.unwrap_err(), Failure::Cancelled);
    gone(child);
    let (dir, plan) = prepare("stall").await;
    let probe = RuntimeProbe::start(approve(plan), Cancellation::new());
    let child = pid(dir.path(), "stall").await;
    drop(probe);
    tokio::time::timeout(Duration::from_secs(3), async {
        while unsafe { libc::kill(child, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (dir, plan) = prepare("flood").await;
    let mut probe = RuntimeProbe::start(approve(plan), Cancellation::new());
    assert_eq!(probe.wait().await.unwrap_err(), Failure::ResponseTooLarge);
    gone(pid(dir.path(), "flood").await);
    let (dir, plan) = prepare("stall").await;
    let mut probe = RuntimeProbe::start(approve(plan), Cancellation::new());
    assert_eq!(probe.wait().await.unwrap_err(), Failure::StartupTimeout);
    gone(pid(dir.path(), "stall").await);
}
#[tokio::test]
async fn unsuccessful_and_malformed_output_cannot_qualify_flags() {
    let (_dir, plan) = prepare("nonzero").await;
    let mut probe = RuntimeProbe::start(approve(plan), Cancellation::new());
    let report = probe.wait().await.unwrap();
    assert_eq!(report.help.exit_code, Some(3));
    assert!(report.devices.is_empty());
    let (_dir, plan) = prepare("invalid").await;
    let mut probe = RuntimeProbe::start(approve(plan), Cancellation::new());
    assert_eq!(probe.wait().await.unwrap_err(), Failure::Protocol);
}

#[tokio::test]
async fn selected_catalog_hashes_and_inspects_without_execution_or_typed_hashes() {
    use runtime_catalog::{inspect_selection, SelectedFileInput};
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("normal");
    std::fs::copy(fixture(), &executable).unwrap();
    let model = dir.path().join("test.gguf");
    let mut bytes = b"GGUF".to_vec();
    bytes.extend(3u32.to_le_bytes());
    bytes.extend(1u64.to_le_bytes());
    bytes.extend(0u64.to_le_bytes());
    std::fs::write(&model, bytes).unwrap();
    let input = |path: &Path| SelectedFileInput {
        path: path.to_str().unwrap().into(),
        source: "synthetic explicit selection".into(),
        source_revision: "fixture-v1".into(),
    };
    let result = inspect_selection(input(&executable), input(&model), &Cancellation::new())
        .await
        .unwrap();
    assert_eq!(result.executable.sha256.len(), 64);
    assert_eq!(result.model.sha256.len(), 64);
    assert!(!result.inference_verified);
    assert!(result.metadata.kv.is_none());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    assert!(serde_json::from_value::<SelectedFileInput>(serde_json::json!({"path":executable,"source":"x","source_revision":"v1","sha256":"client hash"})).is_err());
    let cancel = Cancellation::new();
    cancel.cancel();
    assert!(
        inspect_selection(input(&executable), input(&model), &cancel)
            .await
            .is_err()
    );
}

#[test]
fn advanced_settings_reject_arbitrary_flags_and_unknown_values() {
    for value in [
        serde_json::json!({"argv":["--rpc","remote"]}),
        serde_json::json!({"flash_attention":"true"}),
        serde_json::json!({"cache_type_k":"malicious"}),
    ] {
        assert!(serde_json::from_value::<AdvancedSettings>(value).is_err());
    }
}

#[tokio::test]
async fn native_prepare_cannot_bypass_split_model_metadata_refusal() {
    let (dir, probe) = prepare("normal").await;
    let model = dir.path().join("single-name.gguf");
    let mut b = b"GGUF".to_vec();
    b.extend(3u32.to_le_bytes());
    b.extend(1u64.to_le_bytes());
    b.extend(1u64.to_le_bytes());
    b.extend(11u64.to_le_bytes());
    b.extend(b"split.count");
    b.extend(4u32.to_le_bytes());
    b.extend(2u32.to_le_bytes());
    std::fs::write(&model, b).unwrap();
    let model = inspect_selected_artifact(
        &model,
        ArtifactKind::Model,
        "synthetic".into(),
        "fixture-v1".into(),
        &Cancellation::new(),
    )
    .await
    .unwrap();
    let manifest = Manifest {
        model_id: "test".into(),
        executable: probe.summary().executable.clone(),
        model,
        protocol: Protocol::LlamaCppUnixHttpV1,
        execution_kind: ExecutionKind::Native,
        declares_text_chat: true,
    };
    assert!(matches!(
        LoadPlan::prepare(manifest, LoadConfig::default(), &Cancellation::new()).await,
        Err(Failure::InvalidManifest)
    ));
    assert!(!dir.path().join("normal.pid").exists());
}
