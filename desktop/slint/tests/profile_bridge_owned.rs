#![cfg(unix)]
//! Explicit owned normal backend + independently generated schema25 encrypted
//! copies only. Environment variables select public executables, never keys.
use smith_native_bridge::{
    bridge::{Bridge, Transport},
    owned_backend::{OwnedBackend, OwnedTransport},
    profile_bridge::*,
    task_bridge::new_task_request_id,
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "smith-native-original-owned-{}",
            new_task_request_id().unwrap()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(path.join("host")).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
struct SyntheticKey(String);
impl SyntheticKey {
    fn new() -> Self {
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut bytes)
            .unwrap();
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
    fn entered(&self) -> OriginalKey {
        OriginalKey::master(self.0.clone()).unwrap()
    }
}
impl Drop for SyntheticKey {
    fn drop(&mut self) {
        unsafe {
            for b in self.0.as_mut_vec() {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}
fn executable(name: &str) -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os(name).expect("Explicit synthetic qualification executable is required"),
    );
    assert!(path.is_absolute() && path.is_file());
    path.canonicalize().unwrap()
}
fn fixture(root: &Path, key: &SyntheticKey, version: u32) {
    let mut child = Command::new(executable("SMITH_NATIVE_TEST_LEGACY_FIXTURE"))
        .arg(root)
        .arg(version.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(key.0.as_bytes())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "Synthetic fixture generation failed");
            break;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Synthetic fixture generator timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let p = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(base, &p, result)
            } else {
                assert!(entry.file_type().unwrap().is_file());
                result.insert(
                    p.strip_prefix(base).unwrap().to_owned(),
                    std::fs::read(p).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}
fn selection(source: &Path) -> SourceSelection {
    SourceSelection::new(source, &source.join("artifacts"), true).unwrap()
}
fn poll(
    w: &mut ProfileWorkflow,
    b: &mut Bridge<OwnedTransport>,
    ticket: &OperationTicket,
) -> ProfileStatus {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let status = w.status(b).unwrap();
        let operation = status
            .operation
            .as_ref()
            .expect("Owned operation must remain visible");
        assert_eq!(operation.id, ticket.id);
        assert_eq!(operation.kind, ticket.kind);
        if operation.terminal() {
            assert_eq!(status.state, ProfilePhase::Unlocked);
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "Owned original-copy operation deadline exceeded"
        );
        thread::sleep(Duration::from_millis(30));
    }
}
fn inspect(
    w: &mut ProfileWorkflow,
    b: &mut Bridge<OwnedTransport>,
    source: &Path,
    key: &SyntheticKey,
) -> ProfileReview {
    let ticket = w.inspect(b, selection(source), key.entered()).unwrap();
    let status = poll(w, b, &ticket);
    let report = status.operation.unwrap().import_report().unwrap();
    assert_eq!(report.source_schema, 25);
    let expected: i64 = std::env::var("SMITH_NATIVE_TEST_TARGET_SCHEMA")
        .expect("An exact target schema must accompany the frozen backend")
        .parse()
        .expect("Expected target schema must be an integer");
    assert_eq!(report.target_schema, expected);
    assert_eq!(report.mapped_chats, 1);
    assert_eq!(report.mapped_messages, 6);
    assert!(report.verified_blobs >= 2);
    assert!(report.retained_rows >= report.mapped_messages);
    assert!(report.dormant_domains.iter().any(|s| s == "tools"));
    w.review().unwrap()
}
fn import(
    w: &mut ProfileWorkflow,
    b: &mut Bridge<OwnedTransport>,
    source: &Path,
    key: &SyntheticKey,
) {
    let review = inspect(w, b, source, key);
    let ticket = w.execute(b, review, key.entered()).unwrap();
    let status = poll(w, b, &ticket);
    assert!(
        status.operation.unwrap().succeeded(),
        "Original import must complete"
    );
    assert_eq!(status.selection, Selection::Imported);
    w.open(b).unwrap();
    assert!(b.view.available);
}
fn session(transport: &mut OwnedTransport) -> String {
    transport
        .request("GET", "/api/profile/status", None, None)
        .unwrap()["session"]
        .as_str()
        .unwrap()
        .to_owned()
}
fn verify_history(owner: &OwnedBackend, b: &mut Bridge<OwnedTransport>) {
    assert_eq!(b.view.chats.len(), 1);
    assert_eq!(b.view.chats[0].id, "session");
    assert!(
        b.view.tasks.is_empty(),
        "Historical tasks must not gain runtime authority"
    );
    b.open("session").unwrap();
    let conversation = b.view.conversation.as_ref().unwrap();
    assert_eq!(conversation.messages.len(), 6);
    assert!(conversation
        .active_messages
        .iter()
        .any(|m| m.id == "latest" && m.content == "Original selected answer retained."));
    assert!(conversation
        .messages
        .iter()
        .any(|m| m.id == "branch1" && m.status == "imported_reference"));
    let mut transport = owner.transport();
    let token = session(&mut transport);
    let original = transport
        .request(
            "GET",
            "/api/legacy-import/messages/latest",
            Some(&token),
            None,
        )
        .unwrap();
    assert_eq!(
        original["reasoning"],
        "Original private reasoning retained."
    );
    assert_eq!(original["runtime_activation"], "dormant");
    let cancelled = transport
        .request(
            "GET",
            "/api/legacy-import/messages/branch1",
            Some(&token),
            None,
        )
        .unwrap();
    assert_eq!(cancelled["original_status"], "cancelled");
    assert!(cancelled["tool_calls_json"]
        .as_str()
        .unwrap()
        .contains("SENSITIVE_TOOL_SENTINEL"));
    let records = transport
        .request(
            "GET",
            "/api/legacy-import/records/original_unknown",
            Some(&token),
            None,
        )
        .unwrap();
    assert_eq!(records["total"], 1);
    assert_eq!(records["runtime_activation"], "dormant");
    let exact = &records["rows"][0]["record"];
    let columns = exact["columns"].as_array().unwrap();
    for (column, kind, value) in [
        ("rowid", "integer", "-51"),
        ("exact_number", "integer", "9223372036854775807"),
        ("bytes", "blob", "00ff0102"),
    ] {
        let index = columns.iter().position(|v| v == column).unwrap();
        assert_eq!(exact["values"][index]["type"], kind);
        assert_eq!(exact["values"][index]["value"], value);
    }
    let tasks = transport
        .request("GET", "/api/legacy-import/tasks", Some(&token), None)
        .unwrap();
    assert!(!tasks["tasks"].as_array().unwrap().is_empty());
    for task in tasks["tasks"].as_array().unwrap() {
        assert_eq!(task["runtime_activation"], "dormant");
        assert_eq!(task["resumable"], false);
    }
    let bytes = transport
        .download(
            "/api/legacy-import/artifacts/11223344-5566-7788-99aa-bbccddeeff00/bytes",
            &token,
        )
        .unwrap();
    assert_eq!(
        bytes,
        b"Synthetic original managed artifact. Exact retained bytes.\n"
    );
}
fn encrypted(destination: &Path, key: &SyntheticKey) {
    let snapshot = tree(destination);
    assert!(destination.join("profile.sqlite3").is_file());
    assert!(destination.join("import.db").is_file());
    for bytes in snapshot.values() {
        assert!(!bytes.starts_with(b"SQLite format 3\0"));
        for needle in [
            key.0.as_bytes(),
            b"Original private reasoning retained.",
            b"Original selected answer retained.",
            b"Synthetic original managed artifact. Exact retained bytes.",
        ] {
            assert!(
                !bytes.windows(needle.len()).any(|v| v == needle),
                "Encrypted import must not disclose synthetic plaintext"
            );
        }
    }
}
#[test]
#[ignore = "Requires explicitly selected normal backend and synthetic original fixture executable"]
fn native_original_import_reads_projection_and_exact_evidence_then_deactivates_without_deletion() {
    let temp = Temp::new();
    let key = SyntheticKey::new();
    let source = temp.0.join("copy");
    fixture(&source, &key, 25);
    let before = tree(&source);
    std::fs::write(
        temp.0.join("host/profile.sqlite3"),
        b"Retained historical manual slot sentinel",
    )
    .unwrap();
    let owner = OwnedBackend::launch(
        &executable("SMITH_NATIVE_TEST_BACKEND"),
        &temp.0.join("host"),
    )
    .unwrap();
    let mut b = Bridge::new(owner.transport());
    b.initialize().unwrap();
    b.change_draft(
        b.view.draft.identity,
        "Prior automatic workspace draft".into(),
    )
    .unwrap();
    b.save_draft().unwrap();
    let mut w = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    import(&mut w, &mut b, &source, &key);
    verify_history(&owner, &mut b);
    let destination = temp.0.join("host/local-workspace/imported");
    encrypted(&destination, &key);
    assert!(
        tree(&source) == before,
        "Original copy bytes changed during import/read"
    );
    let review = w.review_rollback(&mut b, selection(&source)).unwrap();
    let ticket = w.rollback(&mut b, review).unwrap();
    let status = poll(&mut w, &mut b, &ticket);
    let report = status.operation.unwrap().rollback_report().unwrap();
    assert!(
        report.eligible
            && report.deactivated
            && report.source_unchanged
            && report.destination_unchanged
    );
    assert_eq!(status.selection, Selection::Default);
    w.open(&mut b).unwrap();
    assert_eq!(b.view.draft.text, "Prior automatic workspace draft");
    assert!(b.view.chats.is_empty());
    assert!(destination.join("import.db").exists());
    assert!(tree(&source) == before);
    assert_eq!(
        std::fs::read(temp.0.join("host/profile.sqlite3")).unwrap(),
        b"Retained historical manual slot sentinel"
    );
    let retained_import = tree(&destination);
    let ticket = w
        .inspect(&mut b, selection(&source), key.entered())
        .unwrap();
    let status = poll(&mut w, &mut b, &ticket);
    assert!(!status.operation.unwrap().succeeded());
    assert!(
        w.review().is_err(),
        "An occupied import slot must not earn a replacement review"
    );
    w.open(&mut b).unwrap();
    assert_eq!(b.view.draft.text, "Prior automatic workspace draft");
    assert!(
        tree(&destination) == retained_import,
        "A repeated import overwrote retained encrypted evidence"
    );
    assert!(tree(&source) == before);
    owner.stop().unwrap();
}
#[test]
#[ignore = "Requires explicitly selected normal backend and synthetic original fixture executable"]
fn native_import_restart_preserves_data_and_later_write_blocks_deactivation() {
    let temp = Temp::new();
    let key = SyntheticKey::new();
    let source = temp.0.join("copy");
    fixture(&source, &key, 25);
    let before = tree(&source);
    let backend = executable("SMITH_NATIVE_TEST_BACKEND");
    let owner = OwnedBackend::launch(&backend, &temp.0.join("host")).unwrap();
    let mut b = Bridge::new(owner.transport());
    b.initialize().unwrap();
    let mut w = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    import(&mut w, &mut b, &source, &key);
    b.open("session").unwrap();
    b.change_draft(b.view.draft.identity, "Post-import retained write".into())
        .unwrap();
    b.save_draft().unwrap();
    owner.stop().unwrap();
    drop(b);
    drop(owner);
    let owner = OwnedBackend::launch(&backend, &temp.0.join("host")).unwrap();
    let mut b = Bridge::new(owner.transport());
    b.initialize().unwrap();
    verify_history(&owner, &mut b);
    assert_eq!(b.view.draft.text, "Post-import retained write");
    let mut w = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    let review = w.review_rollback(&mut b, selection(&source)).unwrap();
    let ticket = w.rollback(&mut b, review).unwrap();
    let status = poll(&mut w, &mut b, &ticket);
    let operation = status.operation.unwrap();
    assert!(!operation.succeeded());
    assert!(operation.error.unwrap().contains("not eligible"));
    assert_eq!(status.selection, Selection::Imported);
    w.open(&mut b).unwrap();
    b.open("session").unwrap();
    assert_eq!(b.view.draft.text, "Post-import retained write");
    assert!(tree(&source) == before);
    encrypted(&temp.0.join("host/local-workspace/imported"), &key);
    owner.stop().unwrap();
}
#[test]
#[ignore = "Requires explicitly selected normal backend and synthetic original fixture executable"]
fn native_wrong_key_and_future_schema_never_publish_or_reset_copies() {
    let temp = Temp::new();
    let key = SyntheticKey::new();
    let wrong = SyntheticKey::new();
    let source = temp.0.join("copy");
    let future = temp.0.join("future-copy");
    fixture(&source, &key, 25);
    fixture(&future, &key, 26);
    let before = tree(&source);
    let before_future = tree(&future);
    let owner = OwnedBackend::launch(
        &executable("SMITH_NATIVE_TEST_BACKEND"),
        &temp.0.join("host"),
    )
    .unwrap();
    let mut b = Bridge::new(owner.transport());
    b.initialize().unwrap();
    let mut w = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    for (selected, secret) in [(&source, &wrong), (&future, &key)] {
        let ticket = w
            .inspect(&mut b, selection(selected), secret.entered())
            .unwrap();
        let status = poll(&mut w, &mut b, &ticket);
        assert!(!status.operation.unwrap().succeeded());
        assert!(w.review().is_err());
        w.open(&mut b).unwrap();
        assert!(b.view.available);
        assert!(b.view.chats.is_empty());
        assert!(!temp.0.join("host/local-workspace/imported").exists());
    }
    assert!(tree(&source) == before);
    assert!(tree(&future) == before_future);
    owner.stop().unwrap();
}
