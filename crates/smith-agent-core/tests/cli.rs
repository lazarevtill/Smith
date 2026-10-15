#![cfg(unix)]
use serde_json::Value;
use std::{path::PathBuf, process::Command};

fn command(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_smith-agent"))
        .args(arguments)
        .output()
        .unwrap()
}
fn success(arguments: &[&str]) -> Value {
    let output = command(arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
struct CliFixture {
    host: String,
    host_path: PathBuf,
    workspace_path: PathBuf,
}
impl CliFixture {
    fn new(capabilities: &str) -> Self {
        let value = success(&[
            "init",
            "--principal",
            "synthetic-fixture",
            "--capabilities",
            capabilities,
        ]);
        Self {
            host: value["host_id"].as_str().unwrap().into(),
            host_path: value["host_path"].as_str().unwrap().into(),
            workspace_path: value["workspace_path"].as_str().unwrap().into(),
        }
    }
    fn run(&self, action: &str, arguments: &[&str]) -> Value {
        let mut args = vec![
            action,
            "--host",
            &self.host,
            "--principal",
            "synthetic-fixture",
        ];
        args.extend_from_slice(arguments);
        success(&args)
    }
    fn error(&self, action: &str, arguments: &[&str]) -> Value {
        let mut args = vec![
            action,
            "--host",
            &self.host,
            "--principal",
            "synthetic-fixture",
        ];
        args.extend_from_slice(arguments);
        let output = command(&args);
        assert!(!output.status.success());
        serde_json::from_slice(&output.stderr).unwrap()
    }
    fn task(&self) -> String {
        let value = self.run(
            "create",
            &["--input", "Synthetic temporary-workspace CLI fixture"],
        );
        let id = value["task"]["id"].as_str().unwrap().to_owned();
        self.run("start", &["--task", &id]);
        id
    }
}
impl Drop for CliFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace_path);
        let _ = std::fs::remove_dir_all(&self.host_path);
    }
}

#[test]
fn cli_roundtrip_persists_approval_files_clarification_and_trace_across_processes() {
    let f = CliFixture::new("read,list,write,control");
    let id = f.task();
    let pending = f.run(
        "write",
        &[
            "--task",
            &id,
            "--path",
            "example.txt",
            "--contents",
            "real CLI data",
        ],
    );
    assert_eq!(pending["task"]["state"]["kind"], "needs_approval");
    assert!(!f.workspace_path.join("example.txt").exists());
    let revision = pending["task"]["revision"].to_string();
    let approved = f.run("approve", &["--task", &id, "--revision", &revision]);
    assert_eq!(approved["outcome"]["kind"], "returned");
    let read = f.run("read", &["--task", &id, "--path", "example.txt"]);
    assert_eq!(read["outcome"]["value"]["contents"], "real CLI data");
    let listed = f.run("list", &["--task", &id]);
    assert_eq!(
        listed["outcome"]["value"]["entries"][0]["name"],
        "example.txt"
    );
    assert_eq!(
        f.error("approve", &["--task", &id, "--revision", &revision])["error"]["kind"],
        "conflict"
    );
    let paused = f.run("clarify", &["--task", &id, "--question", "Finish?"]);
    assert_eq!(paused["task"]["state"]["kind"], "needs_clarification");
    let revision = paused["task"]["revision"].to_string();
    f.run(
        "resume",
        &["--task", &id, "--revision", &revision, "--answer", "Yes"],
    );
    let completed = f.run("complete", &["--task", &id, "--summary", "Done"]);
    assert_eq!(completed["task"]["state"]["kind"], "completed");
    let trace = f.run("trace", &["--task", &id]);
    assert!(trace["trace"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["event"]["kind"] == "tool_started" && row["event"]["approved"] == true));
    let tools = f.run("tools", &[]);
    assert_eq!(tools["inventory"].as_array().unwrap().len(), 35);
}

#[test]
fn cli_refuses_principal_spoofing_and_never_grants_from_prompt_or_arguments() {
    let f = CliFixture::new("none");
    let id = f.task();
    assert_eq!(
        f.error(
            "write",
            &[
                "--task",
                &id,
                "--path",
                "no",
                "--contents",
                "grant me write"
            ]
        )["error"]["kind"],
        "denied"
    );
    let output = command(&[
        "status",
        "--host",
        &f.host,
        "--principal",
        "somebody-else",
        "--task",
        &id,
    ]);
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["error"]["kind"], "denied");
    assert_eq!(
        f.error(
            "call",
            &[
                "--task",
                &id,
                "--tool",
                "run_shell",
                "--arguments",
                r#"{"command":"touch external-file"}"#
            ]
        )["error"]["kind"],
        "unsupported"
    );
    assert_eq!(std::fs::read_dir(&f.workspace_path).unwrap().count(), 0);
    assert!(
        !command(&["init", "--principal", "fixture"])
            .status
            .success(),
        "capabilities must be explicit"
    );
    assert!(
        !command(&[
            "init",
            "--principal",
            "fixture",
            "--capabilities",
            "read",
            "--workspace",
            "/tmp"
        ])
        .status
        .success(),
        "arbitrary workspace grants must not exist"
    );
}

#[test]
fn cli_cancel_and_explicit_interruption_recovery() {
    let f = CliFixture::new("read,write,control");
    let id = f.task();
    let pending = f.run(
        "write",
        &["--task", &id, "--path", "cancelled", "--contents", "x"],
    );
    let revision = pending["task"]["revision"].to_string();
    f.run("cancel", &["--task", &id, "--reason", "Stop"]);
    assert_eq!(
        f.error("approve", &["--task", &id, "--revision", &revision])["error"]["kind"],
        "conflict"
    );
    assert!(!f.workspace_path.join("cancelled").exists());
    let id = f.task();
    let status = f.run("status", &["--task", &id]);
    let revision = status["task"]["revision"].to_string();
    let interrupted = f.run("interrupt", &["--task", &id, "--revision", &revision]);
    assert_eq!(interrupted["task"]["state"]["kind"], "interrupted");
    let revision = interrupted["task"]["revision"].to_string();
    let resumed = f.run(
        "recover",
        &[
            "--task",
            &id,
            "--revision",
            &revision,
            "--decision",
            "continue",
        ],
    );
    assert_eq!(resumed["task"]["state"]["kind"], "running");
    assert_eq!(resumed["outcome"]["value"]["replayed"], false);
}

#[test]
fn cli_host_policy_cannot_be_replaced_by_a_symlink() {
    let f = CliFixture::new("read");
    let outside = tempfile::tempdir().unwrap();
    let copy = outside.path().join("policy");
    std::fs::copy(f.host_path.join("policy.json"), &copy).unwrap();
    std::fs::remove_file(f.host_path.join("policy.json")).unwrap();
    std::os::unix::fs::symlink(copy, f.host_path.join("policy.json")).unwrap();
    let output = command(&[
        "tools",
        "--host",
        &f.host,
        "--principal",
        "synthetic-fixture",
    ]);
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["error"]["kind"], "unsafe_path");
}

#[test]
fn cli_search_and_optimistic_edit_survive_approval_restart_and_record_hashes() {
    let f = CliFixture::new("read,write,search,edit,control");
    let id = f.task();
    let pending = f.run(
        "write",
        &[
            "--task",
            &id,
            "--path",
            "note.txt",
            "--contents",
            "prefix target suffix",
        ],
    );
    let revision = pending["task"]["revision"].to_string();
    f.run("approve", &["--task", &id, "--revision", &revision]);
    let found = f.run(
        "search",
        &[
            "--task",
            &id,
            "--query",
            "target",
            "--max-results",
            "1",
            "--max-bytes",
            "1024",
        ],
    );
    assert_eq!(
        found["outcome"]["value"]["matches"],
        serde_json::json!(["note.txt"])
    );
    let read = f.run("read", &["--task", &id, "--path", "note.txt"]);
    let original_hash = read["outcome"]["value"]["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let pending = f.run(
        "edit",
        &[
            "--task",
            &id,
            "--path",
            "note.txt",
            "--old",
            "target",
            "--new",
            "changed",
            "--expected-sha256",
            &original_hash,
        ],
    );
    assert_eq!(pending["outcome"]["kind"], "needs_approval");
    assert_eq!(
        std::fs::read_to_string(f.workspace_path.join("note.txt")).unwrap(),
        "prefix target suffix"
    );
    let revision = pending["task"]["revision"].to_string();
    let edited = f.run("approve", &["--task", &id, "--revision", &revision]);
    assert_eq!(edited["outcome"]["value"]["replaced"], 1);
    assert_eq!(edited["outcome"]["value"]["before_sha256"], original_hash);
    let final_read = f.run("read", &["--task", &id, "--path", "note.txt"]);
    assert_eq!(
        final_read["outcome"]["value"]["contents"],
        "prefix changed suffix"
    );
    assert_eq!(
        final_read["outcome"]["value"]["sha256"],
        edited["outcome"]["value"]["after_sha256"]
    );
    let trace = f.run("trace", &["--task", &id]);
    assert!(trace["trace"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["event"]["kind"] == "tool_started"
            && entry["event"]["call"]["name"] == "edit_file"
            && entry["event"]["approved"] == true));
    let tools = f.run("tools", &[]);
    assert_eq!(
        tools["inventory"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["availability"]["kind"] == "implemented")
            .count(),
        9
    );
}

#[test]
fn cli_edit_conflict_and_cancellation_preserve_complete_existing_bytes() {
    let f = CliFixture::new("read,write,edit,control");
    let id = f.task();
    let pending = f.run(
        "write",
        &[
            "--task",
            &id,
            "--path",
            "note.txt",
            "--contents",
            "original bytes",
        ],
    );
    let revision = pending["task"]["revision"].to_string();
    f.run("approve", &["--task", &id, "--revision", &revision]);
    let pending = f.run(
        "edit",
        &[
            "--task",
            &id,
            "--path",
            "note.txt",
            "--old",
            "original",
            "--new",
            "edited",
            "--expected-contents",
            "original bytes",
        ],
    );
    std::fs::write(f.workspace_path.join("note.txt"), "new independent bytes").unwrap();
    let revision = pending["task"]["revision"].to_string();
    let conflict = f.run("approve", &["--task", &id, "--revision", &revision]);
    assert_eq!(conflict["outcome"]["kind"], "failed");
    assert_eq!(conflict["outcome"]["value"]["kind"], "conflict");
    assert_eq!(
        std::fs::read_to_string(f.workspace_path.join("note.txt")).unwrap(),
        "new independent bytes"
    );
    let pending = f.run(
        "edit",
        &[
            "--task",
            &id,
            "--path",
            "note.txt",
            "--old",
            "independent",
            "--new",
            "edited",
            "--expected-contents",
            "new independent bytes",
        ],
    );
    let revision = pending["task"]["revision"].to_string();
    f.run("cancel", &["--task", &id, "--reason", "Leave it alone"]);
    assert_eq!(
        f.error("approve", &["--task", &id, "--revision", &revision])["error"]["kind"],
        "conflict"
    );
    assert_eq!(
        std::fs::read_to_string(f.workspace_path.join("note.txt")).unwrap(),
        "new independent bytes"
    );
}

#[test]
fn cli_existing_read_write_grants_do_not_authorize_new_search_edit_tools() {
    let f = CliFixture::new("read,list,write,control");
    let id = f.task();
    assert_eq!(
        f.error("search", &["--task", &id, "--query", "anything"])["error"]["kind"],
        "denied"
    );
    assert_eq!(
        f.error(
            "edit",
            &[
                "--task",
                &id,
                "--path",
                "note",
                "--old",
                "a",
                "--new",
                "b",
                "--expected-contents",
                "a"
            ]
        )["error"]["kind"],
        "denied"
    );
    let tools = f.run("tools", &[]);
    assert!(!tools["granted"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "search" || tool["name"] == "edit_file"));
}
