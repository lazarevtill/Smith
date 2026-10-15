#![cfg(unix)]
use serde_json::{json, Value};
use smith_agent_core::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const CAPS: [Capability; 6] = [
    Capability::ReadWorkspace,
    Capability::ListWorkspace,
    Capability::WriteWorkspace,
    Capability::SearchWorkspace,
    Capability::EditWorkspace,
    Capability::TaskControl,
];
const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

struct Fixture {
    root: PathBuf,
    grant: ExecutionGrant,
    engine: TaskEngine<MemoryTraceStore>,
    id: String,
}
impl Fixture {
    fn new() -> Self {
        let workspace = Arc::new(Workspace::create_temporary().unwrap());
        let root = workspace.host_path().to_path_buf();
        let grant = ExecutionGrant::new(
            Principal::new("search-edit-fixture").unwrap(),
            workspace,
            "editor",
            CAPS,
        )
        .unwrap();
        let engine = TaskEngine::new(MemoryTraceStore::default(), ToolRegistry::default());
        let task = engine.create(&grant, "Synthetic search/edit data").unwrap();
        engine.start(&task.id, &grant).unwrap();
        Self {
            root,
            grant,
            engine,
            id: task.id,
        }
    }
    fn call(&self, name: &str, args: Value) -> Result<ToolOutcome> {
        self.engine
            .call(&self.id, &self.grant, ToolCall::new(name, args))
    }
    fn approve(&self) -> Result<ToolOutcome> {
        let task = self.engine.get(&self.id, &self.grant)?;
        self.engine.decide_approval(
            &self.id,
            task.revision,
            &self.grant,
            ApprovalDecision::Approve,
        )
    }
    fn write(&self, path: &str, contents: &str) {
        assert_eq!(
            self.call("write_file", json!({"path":path,"contents":contents}))
                .unwrap(),
            ToolOutcome::NeedsApproval
        );
        assert!(matches!(self.approve().unwrap(), ToolOutcome::Returned(_)));
    }
    fn edit(&self, old: &str, new: &str, expected: &str) -> Result<ToolOutcome> {
        assert_eq!(self.call("edit_file", json!({"path":"note","old_string":old,"new_string":new,"expected_contents":expected}))?, ToolOutcome::NeedsApproval);
        self.approve()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn returned(outcome: ToolOutcome) -> Value {
    match outcome {
        ToolOutcome::Returned(value) => value,
        other => panic!("expected value, got {other:?}"),
    }
}

#[test]
fn literal_search_is_case_sensitive_sorted_and_does_not_interpret_regex() {
    let f = Fixture::new();
    f.write("z.txt", "literal (a+)+$ and needle");
    f.write("a.txt", "needle twice: needle");
    f.write("b.txt", "NEEDLE and aaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(
        returned(f.call("search", json!({"query":"needle"})).unwrap())["matches"],
        json!(["a.txt", "z.txt"])
    );
    let literal = returned(f.call("search", json!({"query":"(a+)+$"})).unwrap());
    assert_eq!(literal["matches"], json!(["z.txt"]));
    assert_eq!(literal["truncated"], false);
    assert_eq!(literal["scanned_files"], 3);
    assert_eq!(
        returned(f.call("search", json!({"query":"absent"})).unwrap())["matches"],
        json!([])
    );
}

#[test]
fn search_has_result_and_aggregate_byte_limits_with_honest_truncation() {
    let f = Fixture::new();
    for name in ["a", "b", "c"] {
        f.write(name, "abcd");
    }
    let limited = returned(
        f.call("search", json!({"query":"a","max_results":1}))
            .unwrap(),
    );
    assert_eq!(limited["matches"], json!(["a"]));
    assert_eq!(limited["truncated"], true);
    let bytes = returned(
        f.call("search", json!({"query":"a","max_bytes":5}))
            .unwrap(),
    );
    assert_eq!(bytes["matches"], json!(["a"]));
    assert_eq!(bytes["bytes_read"], 5);
    assert_eq!(bytes["truncated"], true);
    for arguments in [
        json!({"query":""}),
        json!({"query":"a","max_results":0}),
        json!({"query":"a","max_results":129}),
        json!({"query":"a","max_bytes":0}),
        json!({"query":"a","max_bytes":4194305}),
        json!({"query":"a","regex":true}),
    ] {
        assert!(f.call("search", arguments).is_err());
    }
}

#[test]
fn search_skips_links_binary_and_nested_content_without_leaking_matches() {
    let f = Fixture::new();
    f.write("safe", "search target");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "search target").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), f.root.join("symlink")).unwrap();
    std::fs::hard_link(outside.path().join("secret"), f.root.join("hardlink")).unwrap();
    std::fs::write(f.root.join("binary"), [0xff, 0xfe]).unwrap();
    std::fs::create_dir(f.root.join("nested")).unwrap();
    std::fs::write(f.root.join("nested").join("inside"), "search target").unwrap();
    let result = returned(f.call("search", json!({"query":"search target"})).unwrap());
    assert_eq!(result["matches"], json!(["safe"]));
    assert_eq!(result["skipped"].as_array().unwrap().len(), 4);
    assert!(result["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["path"] == "hardlink" && item["reason"]["kind"] == "unsafe_path"));
}

#[test]
fn old_grants_and_delegation_cannot_gain_search_or_edit_permissions() {
    let f = Fixture::new();
    let old = f
        .grant
        .delegate(
            "old-grants",
            [
                Capability::ReadWorkspace,
                Capability::WriteWorkspace,
                Capability::ListWorkspace,
            ],
        )
        .unwrap();
    let requested = old.delegate("privileged-editor", CAPS).unwrap();
    assert!(!requested.permits(Capability::SearchWorkspace));
    assert!(!requested.permits(Capability::EditWorkspace));
    for (tool, args) in [
        ("search", json!({"query":"target"})),
        (
            "edit_file",
            json!({"path":"note","old_string":"a","new_string":"b","expected_contents":"a"}),
        ),
    ] {
        assert!(matches!(
            f.engine.call(&f.id, &requested, ToolCall::new(tool, args)),
            Err(Error::Denied(_))
        ));
    }
}

#[test]
fn exact_edit_preserves_unaffected_utf8_and_line_endings_and_is_atomic() {
    use std::io::Read;
    let f = Fixture::new();
    let prior = "\u{feff}header\r\nα target β\r\nlast line";
    f.write("note", prior);
    let mut opened_before = std::fs::File::open(f.root.join("note")).unwrap();
    let result = returned(f.edit("target", "replacement 🦀", prior).unwrap());
    assert_eq!(result["replaced"], 1);
    assert_ne!(result["before_sha256"], result["after_sha256"]);
    let mut old_view = String::new();
    opened_before.read_to_string(&mut old_view).unwrap();
    assert_eq!(
        old_view, prior,
        "old open descriptor must retain the whole prior file"
    );
    assert_eq!(
        std::fs::read(f.root.join("note")).unwrap(),
        "\u{feff}header\r\nα replacement 🦀 β\r\nlast line".as_bytes()
    );
    assert_eq!(std::fs::read_dir(&f.root).unwrap().count(), 1);
}

#[test]
fn sha256_guard_uses_standard_digest_and_trace_records_both_versions() {
    let f = Fixture::new();
    f.write("note", "abc");
    assert_eq!(
        returned(f.call("read_file", json!({"path":"note"})).unwrap())["sha256"],
        ABC_SHA256
    );
    assert_eq!(f.call("edit_file", json!({"path":"note","old_string":"b","new_string":"β","expected_sha256":ABC_SHA256.to_uppercase()})).unwrap(), ToolOutcome::NeedsApproval);
    assert_eq!(std::fs::read_to_string(f.root.join("note")).unwrap(), "abc");
    let result = returned(f.approve().unwrap());
    assert_eq!(result["before_sha256"], ABC_SHA256);
    let after = returned(f.call("read_file", json!({"path":"note"})).unwrap());
    assert_eq!(after["contents"], "aβc");
    assert_eq!(after["sha256"], result["after_sha256"]);
    assert!(f.engine.store().events(&f.id).unwrap().iter().any(|event| matches!(&event.event, TraceEvent::ToolFinished { result: Ok(value), .. } if value["before_sha256"] == ABC_SHA256 && value["after_sha256"] == result["after_sha256"])));
}

#[test]
fn edit_rejects_missing_ambiguous_or_stale_input_without_changing_prior_bytes() {
    let f = Fixture::new();
    for (prior, old) in [
        ("abc", "missing"),
        ("repeat repeat", "repeat"),
        ("aaa", "aa"),
        ("🦀🦀🦀", "🦀🦀"),
    ] {
        f.write("note", prior);
        assert!(matches!(
            f.edit(old, "replacement", prior).unwrap(),
            ToolOutcome::Failed(Error::InvalidInput(_))
        ));
        assert_eq!(std::fs::read_to_string(f.root.join("note")).unwrap(), prior);
    }
    f.write("note", "abc");
    assert!(matches!(
        f.edit("a", "replacement", "stale").unwrap(),
        ToolOutcome::Failed(Error::Conflict)
    ));
    assert_eq!(std::fs::read_to_string(f.root.join("note")).unwrap(), "abc");
    for args in [
        json!({"path":"note","old_string":"a","new_string":"b"}),
        json!({"path":"note","old_string":"","new_string":"b","expected_contents":"abc"}),
        json!({"path":"note","old_string":"a","new_string":"b","expected_sha256":"wrong"}),
        json!({"path":"note","old_string":"a","new_string":"b","expected_contents":"abc","expected_sha256":ABC_SHA256}),
    ] {
        assert!(matches!(
            f.call("edit_file", args),
            Err(Error::InvalidInput(_))
        ));
    }
    assert_eq!(std::fs::read_to_string(f.root.join("note")).unwrap(), "abc");
}

#[test]
fn two_approved_edits_from_one_version_cannot_overwrite_each_other() {
    let f = Fixture::new();
    f.write("note", "abc");
    let second = f.engine.create(&f.grant, "Second editor").unwrap();
    f.engine.start(&second.id, &f.grant).unwrap();
    for (id, replacement) in [(&f.id, "first"), (&second.id, "second")] {
        f.engine.call(id, &f.grant, ToolCall::new("edit_file",json!({"path":"note","old_string":"b","new_string":replacement,"expected_sha256":ABC_SHA256}))).unwrap();
    }
    assert!(matches!(f.approve().unwrap(), ToolOutcome::Returned(_)));
    let task = f.engine.get(&second.id, &f.grant).unwrap();
    assert_eq!(
        f.engine
            .decide_approval(
                &second.id,
                task.revision,
                &f.grant,
                ApprovalDecision::Approve
            )
            .unwrap(),
        ToolOutcome::Failed(Error::Conflict)
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("note")).unwrap(),
        "afirstc"
    );
}

enum StagedAction {
    Cancel,
    ExternalChange,
    FailRename,
    FailRead,
}
struct DuringStaging {
    inner: MemoryTraceStore,
    grant: ExecutionGrant,
    root: PathBuf,
    fired: AtomicBool,
    action: StagedAction,
}
impl TraceStore for DuringStaging {
    fn load(&self, id: &str) -> Result<Task> {
        let staged = std::fs::read_dir(&self.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".smith-write-")
        });
        if staged && !self.fired.swap(true, Ordering::SeqCst) {
            match self.action {
                StagedAction::Cancel => {
                    TaskEngine::new(self.inner.clone(), ToolRegistry::default()).cancel(
                        id,
                        &self.grant,
                        "cancel before commit",
                    )?;
                }
                StagedAction::ExternalChange => {
                    std::fs::write(self.root.join("note"), "independent replacement")?;
                }
                StagedAction::FailRename => {
                    std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o500))?;
                }
                StagedAction::FailRead => {
                    return Err(Error::Persistence(
                        "precommit task checkpoint unavailable".into(),
                    ))
                }
            }
        }
        self.inner.load(id)
    }
    fn commit(&self, expected: Option<u64>, task: &Task, event: &TraceEvent) -> Result<()> {
        self.inner.commit(expected, task, event)
    }
    fn events(&self, id: &str) -> Result<Vec<TraceRecord>> {
        self.inner.events(id)
    }
}

#[test]
fn cancellation_after_staging_preserves_the_original_file_and_cleans_staging() {
    let f = Fixture::new();
    f.write("note", "abc");
    let inode = std::fs::metadata(f.root.join("note")).unwrap().ino();
    let engine = TaskEngine::new(
        DuringStaging {
            inner: f.engine.store().clone(),
            grant: f.grant.clone(),
            root: f.root.clone(),
            fired: AtomicBool::new(false),
            action: StagedAction::Cancel,
        },
        ToolRegistry::default(),
    );
    engine.call(&f.id,&f.grant,ToolCall::new("edit_file",json!({"path":"note","old_string":"b","new_string":"CHANGED","expected_sha256":ABC_SHA256}))).unwrap();
    let task = engine.get(&f.id, &f.grant).unwrap();
    assert_eq!(
        engine.decide_approval(&f.id, task.revision, &f.grant, ApprovalDecision::Approve),
        Err(Error::Conflict)
    );
    assert_eq!(std::fs::read(f.root.join("note")).unwrap(), b"abc");
    assert_eq!(std::fs::metadata(f.root.join("note")).unwrap().ino(), inode);
    assert_eq!(std::fs::read_dir(&f.root).unwrap().count(), 1);
    assert!(matches!(
        engine.get(&f.id, &f.grant).unwrap().state,
        TaskState::Cancelled { .. }
    ));
}

#[test]
fn precommit_conflict_or_write_failure_never_truncates_the_prior_file() {
    for action in [
        StagedAction::ExternalChange,
        StagedAction::FailRename,
        StagedAction::FailRead,
    ] {
        let f = Fixture::new();
        f.write("note", "abc");
        let expected = if matches!(action, StagedAction::ExternalChange) {
            "independent replacement"
        } else {
            "abc"
        };
        let engine = TaskEngine::new(
            DuringStaging {
                inner: f.engine.store().clone(),
                grant: f.grant.clone(),
                root: f.root.clone(),
                fired: AtomicBool::new(false),
                action,
            },
            ToolRegistry::default(),
        );
        engine.call(&f.id,&f.grant,ToolCall::new("edit_file",json!({"path":"note","old_string":"b","new_string":"CHANGED","expected_sha256":ABC_SHA256}))).unwrap();
        let task = engine.get(&f.id, &f.grant).unwrap();
        assert!(matches!(
            engine
                .decide_approval(&f.id, task.revision, &f.grant, ApprovalDecision::Approve)
                .unwrap(),
            ToolOutcome::Failed(_)
        ));
        std::fs::set_permissions(&f.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            std::fs::read_to_string(f.root.join("note")).unwrap(),
            expected
        );
        assert!(engine
            .store()
            .events(&f.id)
            .unwrap()
            .iter()
            .any(|r| matches!(r.event, TraceEvent::ToolFinished { result: Err(_), .. })));
    }
}

#[test]
fn edit_cannot_follow_links_escape_the_workspace_or_exceed_result_size() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("file"), "abc").unwrap();
    std::os::unix::fs::symlink(outside.path().join("file"), f.root.join("note")).unwrap();
    assert!(matches!(
        f.edit("b", "X", "abc").unwrap(),
        ToolOutcome::Failed(Error::UnsafePath(_))
    ));
    assert_eq!(
        std::fs::read_to_string(outside.path().join("file")).unwrap(),
        "abc"
    );
    std::fs::remove_file(f.root.join("note")).unwrap();
    std::fs::hard_link(outside.path().join("file"), f.root.join("note")).unwrap();
    assert!(matches!(
        f.edit("b", "X", "abc").unwrap(),
        ToolOutcome::Failed(Error::UnsafePath(_))
    ));
    for path in ["../note", "/etc/passwd", "nested/note"] {
        assert!(matches!(
            f.call(
                "edit_file",
                json!({"path":path,"old_string":"a","new_string":"b","expected_contents":"a"})
            ),
            Err(Error::UnsafePath(_))
        ));
    }
    std::fs::remove_file(f.root.join("note")).unwrap();
    f.write("note", "abc");
    assert!(matches!(
        f.edit("b", &"x".repeat(64 * 1024), "abc").unwrap(),
        ToolOutcome::Failed(Error::LimitExceeded(_))
    ));
    assert_eq!(std::fs::read_to_string(f.root.join("note")).unwrap(), "abc");
}

struct LoseOneCompletion {
    inner: MemoryTraceStore,
    enabled: AtomicBool,
}
impl TraceStore for LoseOneCompletion {
    fn load(&self, id: &str) -> Result<Task> {
        self.inner.load(id)
    }
    fn commit(&self, expected: Option<u64>, task: &Task, event: &TraceEvent) -> Result<()> {
        if matches!(event, TraceEvent::ToolFinished { .. })
            && self.enabled.swap(false, Ordering::SeqCst)
        {
            return Err(Error::Persistence(
                "lost completion after filesystem effect".into(),
            ));
        }
        self.inner.commit(expected, task, event)
    }
    fn events(&self, id: &str) -> Result<Vec<TraceRecord>> {
        self.inner.events(id)
    }
}

#[test]
fn uncertain_edit_requires_reapproval_and_original_guard_prevents_double_application() {
    let f = Fixture::new();
    f.write("note", "abc");
    let engine = TaskEngine::new(
        LoseOneCompletion {
            inner: f.engine.store().clone(),
            enabled: AtomicBool::new(true),
        },
        ToolRegistry::default(),
    );
    // The original string remains in the result. An unguarded replay would
    // append a second X; the original whole-file version must prevent that.
    let call = ToolCall::new(
        "edit_file",
        json!({"path":"note","old_string":"b","new_string":"bX","expected_sha256":ABC_SHA256}),
    );
    assert_eq!(
        engine.call(&f.id, &f.grant, call.clone()).unwrap(),
        ToolOutcome::NeedsApproval
    );
    let task = engine.get(&f.id, &f.grant).unwrap();
    assert!(matches!(
        engine.decide_approval(&f.id, task.revision, &f.grant, ApprovalDecision::Approve),
        Err(Error::Persistence(_))
    ));
    assert_eq!(std::fs::read(f.root.join("note")).unwrap(), b"abXc");
    let task = engine.get(&f.id, &f.grant).unwrap();
    assert_eq!(
        task.state,
        TaskState::Running {
            in_flight: Some(call.clone())
        }
    );
    assert!(!engine.store().events(&f.id).unwrap().iter().any(
        |r| matches!(&r.event, TraceEvent::ToolFinished { call_id, .. } if call_id == &call.id)
    ));
    let interrupted = engine
        .mark_interrupted(&f.id, task.revision, &f.grant)
        .unwrap();
    assert_eq!(
        engine
            .resume_interrupted(
                &f.id,
                interrupted.revision,
                &f.grant,
                RecoveryDecision::RetryUncertainCall
            )
            .unwrap(),
        ToolOutcome::NeedsApproval
    );
    assert_eq!(std::fs::read(f.root.join("note")).unwrap(), b"abXc");
    let pending = engine.get(&f.id, &f.grant).unwrap();
    assert_eq!(
        engine
            .decide_approval(&f.id, pending.revision, &f.grant, ApprovalDecision::Approve)
            .unwrap(),
        ToolOutcome::Failed(Error::Conflict)
    );
    assert_eq!(std::fs::read(f.root.join("note")).unwrap(), b"abXc");
    assert!(matches!(
        engine.store().events(&f.id).unwrap().last().unwrap().event,
        TraceEvent::ToolFinished {
            result: Err(Error::Conflict),
            ..
        }
    ));
}
