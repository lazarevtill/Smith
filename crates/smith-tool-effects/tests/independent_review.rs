#![cfg(all(feature = "fixtures", target_os = "macos"))]
//! Independent reviewer checks. Owned temporary files and local child processes only.
use serde_json::json;
use smith_tool_effects::{EffectHost, Error, ExecutableGrant, WorkspaceGrant};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn git_workspace(host: &EffectHost) -> (String, PathBuf) {
    let work = WorkspaceGrant::temporary().unwrap();
    let path = work.host_path().to_owned();
    let git = ExecutableGrant::inspect(
        "git",
        Path::new("/Applications/Xcode.app/Contents/Developer/usr/bin/git"),
    )
    .unwrap();
    let binding = host.bind_workspace(work, vec![], Some(git)).unwrap();
    (binding.id, path)
}
fn run(host: &EffectHost, id: &str, tool: &str, arguments: serde_json::Value) {
    let revision = host.binding(id).unwrap().revision;
    let plan = host.prepare(id, revision, tool, arguments).unwrap();
    let approval = host.approve(&plan.summary().id, revision).unwrap();
    let result = host.execute(approval, &mut || Ok(())).unwrap();
    assert_eq!(result["exit_code"], 0, "{result}");
}

#[test]
fn clone_source_mutation_invalidates_review_before_destination_is_consumed() {
    let host = EffectHost::new();
    let (source, source_path) = git_workspace(&host);
    let (destination, destination_path) = git_workspace(&host);
    run(&host, &source, "git_init", json!({}));
    fs::write(source_path.join("reviewed.txt"), "reviewed content").unwrap();
    run(
        &host,
        &source,
        "git_commit",
        json!({"message":"Synthetic initial content"}),
    );
    let plan = host
        .prepare(
            &destination,
            0,
            "git_clone",
            json!({"url":format!("owned://{source}"),"directory":"."}),
        )
        .unwrap();
    assert_eq!(plan.summary().policy["clone_source"]["binding_id"], source);
    assert!(plan.summary().policy["clone_source"]["sha256"]
        .as_str()
        .is_some_and(|s| s.len() == 64));
    let approved = host.approve(&plan.summary().id, 0).unwrap();
    fs::write(source_path.join("reviewed.txt"), "changed after approval").unwrap();
    assert_eq!(
        host.execute(approved, &mut || Ok(())).unwrap_err(),
        Error::Stale
    );
    assert_eq!(host.binding(&destination).unwrap().revision, 0);
    assert_eq!(fs::read_dir(destination_path).unwrap().count(), 0);
    assert!(host.approve(&plan.summary().id, 0).is_err());
}

#[test]
fn source_executable_replaced_with_fifo_is_rejected_without_blocking() {
    let temp = tempfile::tempdir().unwrap();
    let file = fs::canonicalize(temp.path())
        .unwrap()
        .join("selected-native");
    fs::copy(env!("CARGO_BIN_EXE_smith-effects-fixture"), &file).unwrap();
    let grant = ExecutableGrant::inspect("selected", &file).unwrap();
    fs::remove_file(&file).unwrap();
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let start = Instant::now();
    assert!(EffectHost::new()
        .bind_workspace(WorkspaceGrant::temporary().unwrap(), vec![grant], None)
        .is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn bound_executable_snapshot_survives_source_replacement_without_running_new_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let file = fs::canonicalize(temp.path())
        .unwrap()
        .join("selected-native");
    fs::copy(env!("CARGO_BIN_EXE_smith-effects-fixture"), &file).unwrap();
    let grant = ExecutableGrant::inspect("selected", &file).unwrap();
    let host = EffectHost::new();
    let binding = host
        .bind_workspace(WorkspaceGrant::temporary().unwrap(), vec![grant], None)
        .unwrap();
    fs::write(&file, "replacement is not an executable").unwrap();
    let plan = host
        .prepare(
            &binding.id,
            0,
            "run_shell",
            json!({"command":"selected echo reviewed-snapshot"}),
        )
        .unwrap();
    let approved = host.approve(&plan.summary().id, 0).unwrap();
    let result = host.execute(approved, &mut || Ok(())).unwrap();
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["stdout"], "reviewed-snapshot\n");
    assert_eq!(result["reaped"], true);
}
