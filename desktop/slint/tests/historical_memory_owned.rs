#![cfg(unix)]
//! Explicit owned normal backend + generated SQLCipher25 copy. No real profile,
//! model, provider, automatic recall, or external path is accessed.
use smith_native_bridge::{
    bridge::{Bridge, Transport},
    historical_memory_bridge::*,
    memory_bridge::MemoryFilter,
    owned_backend::{OwnedBackend, OwnedTransport},
    profile_bridge::*,
    task_bridge::new_task_request_id,
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "smith-historical-memory-owned-{}",
            new_task_request_id().unwrap()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::create_dir(path.join("host")).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
struct Key(String);
impl Key {
    fn new() -> Self {
        let mut bytes = [0; 32];
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
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            for b in self.0.as_mut_vec() {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}
fn executable(name: &str) -> PathBuf {
    let path =
        PathBuf::from(std::env::var_os(name).expect("Explicit synthetic executable required"));
    assert!(path.is_absolute() && path.is_file());
    path.canonicalize().unwrap()
}
fn fixture(root: &Path, key: &Key) {
    fixture_args(root, key, &[]);
}
fn fixture_args(root: &Path, key: &Key, args: &[String]) {
    let mut child = Command::new(executable("SMITH_NATIVE_TEST_HISTORICAL_FIXTURE"))
        .arg(root)
        .args(args)
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
            assert!(
                status.success(),
                "Synthetic historical fixture generation failed"
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Synthetic fixture deadline exceeded");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(base, &entry.path(), output);
            } else {
                assert!(entry.file_type().unwrap().is_file());
                output.insert(
                    entry.path().strip_prefix(base).unwrap().to_owned(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    walk(root, root, &mut output);
    output
}
fn selection(root: &Path) -> SourceSelection {
    SourceSelection::new(root, &root.join("artifacts"), true).unwrap()
}
fn bridge(owner: &OwnedBackend) -> Bridge<OwnedTransport> {
    let mut b = Bridge::new(owner.transport());
    b.initialize().unwrap();
    assert!(b.view.available);
    b
}
fn poll(
    w: &mut ProfileWorkflow,
    b: &mut Bridge<OwnedTransport>,
    ticket: &OperationTicket,
) -> ProfileStatus {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let status = w.status(b).unwrap();
        let operation = status.operation.as_ref().unwrap();
        assert_eq!(operation.id, ticket.id);
        assert_eq!(operation.kind, ticket.kind);
        if operation.terminal() {
            assert_eq!(status.state, ProfilePhase::Unlocked);
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "Owned import operation deadline exceeded"
        );
        thread::sleep(Duration::from_millis(30));
    }
}
fn import(w: &mut ProfileWorkflow, b: &mut Bridge<OwnedTransport>, source: &Path, key: &Key) {
    let expected: i64 = std::env::var("SMITH_NATIVE_TEST_TARGET_SCHEMA")
        .expect("Exact backend target schema required")
        .parse()
        .unwrap();
    let ticket = w.inspect(b, selection(source), key.entered()).unwrap();
    let status = poll(w, b, &ticket);
    let operation = status.operation.unwrap();
    assert!(
        operation.succeeded(),
        "Synthetic preflight failed: {}",
        operation
            .error
            .as_deref()
            .unwrap_or("No sanitized diagnostic")
    );
    let inspected = operation.import_report().unwrap();
    assert_eq!(inspected.source_schema, 25);
    assert_eq!(inspected.target_schema, expected);
    let review = w.review().unwrap();
    let ticket = w.execute(b, review, key.entered()).unwrap();
    let status = poll(w, b, &ticket);
    let operation = status.operation.unwrap();
    assert!(
        operation.succeeded(),
        "Synthetic import failed: {}",
        operation
            .error
            .as_deref()
            .unwrap_or("No sanitized diagnostic")
    );
    let report = operation.import_report().unwrap();
    assert_eq!(report.target_schema, expected);
    assert_eq!(status.selection, Selection::Imported);
    w.open(b).unwrap();
}
fn filter(
    kind: HistoricalKind,
    scope: AgentScope,
    session: Option<&str>,
    trash: bool,
) -> HistoricalFilter {
    HistoricalFilter {
        kind,
        scope,
        session_id: session.map(str::to_owned),
        query: String::new(),
        trash,
    }
}
fn all(kind: HistoricalKind) -> HistoricalFilter {
    filter(kind, AgentScope::All, None, false)
}
fn find(page: &HistoricalPage, id: &str) -> HistoricalReview {
    page.review(
        page.records
            .iter()
            .find(|r| r.original_id.as_ref().and_then(ExactValue::text) == Some(id))
            .unwrap(),
    )
    .unwrap()
}
fn exact<'a>(detail: &'a HistoricalDetails, name: &str) -> &'a ExactValue {
    let index = detail
        .original
        .columns
        .iter()
        .position(|v| v == name)
        .unwrap();
    &detail.original.values[index]
}
fn assert_dormant(owner: &OwnedBackend, b: &mut Bridge<OwnedTransport>) {
    assert!(b
        .memory_first(MemoryFilter {
            namespace: None,
            query: String::new(),
            deleted: false
        })
        .unwrap()
        .memories
        .is_empty());
    assert!(b.view.tasks.is_empty());
    let mut transport = owner.transport();
    let token = transport
        .request("GET", "/api/profile/status", None, None)
        .unwrap()["session"]
        .as_str()
        .unwrap()
        .to_owned();
    let agents = transport
        .request("GET", "/api/agent/definitions", Some(&token), None)
        .unwrap();
    assert!(agents["definitions"].as_array().unwrap().is_empty());
}
fn assert_encrypted(root: &Path, key: &Key) {
    assert!(root.join("profile.sqlite3").is_file());
    assert!(root.join("import.db").is_file());
    for bytes in tree(root).values() {
        assert!(!bytes.starts_with(b"SQLite format 3\0"));
        for needle in [
            key.0.as_bytes(),
            b"Synthetic agent episode",
            b"Original dormant advice",
            b"unknown-original-polarity",
            b"free text; no tool grant",
        ] {
            assert!(
                !bytes.windows(needle.len()).any(|v| v == needle),
                "Synthetic historical plaintext leaked into storage"
            );
        }
    }
}

#[test]
#[ignore = "Requires frozen normal backend with source mapping and synthetic encrypted historical fixture"]
fn historical_selected_source_owned_transport_restart_and_navigation() {
    use smith_native_bridge::bridge::NavigationEpoch;
    let temp = Temp::new();
    let key = Key::new();
    let source = temp.0.join("copy");
    fixture(&source, &key);
    let original = tree(&source);
    let backend = executable("SMITH_NATIVE_TEST_BACKEND");
    let owner = OwnedBackend::launch(&backend, &temp.0.join("host")).unwrap();
    let mut b = bridge(&owner);
    let mut workflow = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    import(&mut workflow, &mut b, &source, &key);
    b.open("session").unwrap();
    let mut epoch = NavigationEpoch::default();
    let lookup = b.review_message_source("root", &epoch).unwrap();
    let receipt = b.fetch_message_source(&lookup, &epoch).unwrap();
    let mapping = receipt.mapping(&b, &epoch).unwrap().clone();
    assert_eq!(mapping.original_message_id, "root");
    assert_eq!(mapping.original_session_id, "session");
    assert!(mapping.source.valid());
    assert!(b.fetch_message_source(&lookup, &epoch).is_err());
    epoch.advance();
    assert!(receipt.mapping(&b, &epoch).is_err());
    let old_lookup = b.review_message_source("root", &epoch).unwrap();
    let destination = temp.0.join("host/local-workspace/imported");
    let archive = std::fs::read(destination.join("import.db")).unwrap();
    assert_dormant(&owner, &mut b);
    owner.stop().unwrap();

    let owner = OwnedBackend::launch(&backend, &temp.0.join("host")).unwrap();
    b = bridge(&owner);
    b.open("session").unwrap();
    assert!(b.fetch_message_source(&old_lookup, &epoch).is_err());
    let lookup = b.review_message_source("root", &epoch).unwrap();
    let receipt = b.fetch_message_source(&lookup, &epoch).unwrap();
    assert_eq!(receipt.mapping(&b, &epoch).unwrap(), &mapping);
    assert_dormant(&owner, &mut b);
    let mut workflow = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    let review = workflow
        .review_rollback(&mut b, selection(&source))
        .unwrap();
    let ticket = workflow.rollback(&mut b, review).unwrap();
    let status = poll(&mut workflow, &mut b, &ticket);
    assert!(status.operation.unwrap().succeeded());
    assert_eq!(status.selection, Selection::Default);
    assert!(receipt.mapping(&b, &epoch).is_err());
    assert_eq!(
        std::fs::read(destination.join("import.db")).unwrap(),
        archive
    );
    assert!(tree(&source) == original);
    assert_encrypted(&destination, &key);
    owner.stop().unwrap();
}

#[test]
#[ignore = "Requires frozen normal backend and generated encrypted historical fixture; owned loopback only"]
fn historical_import_exact_dormant_reads_preserve_rollback_and_original_bytes() {
    let temp = Temp::new();
    let key = Key::new();
    let source = temp.0.join("copy");
    fixture(&source, &key);
    let original = tree(&source);
    let owner = OwnedBackend::launch(
        &executable("SMITH_NATIVE_TEST_BACKEND"),
        &temp.0.join("host"),
    )
    .unwrap();
    let mut b = bridge(&owner);
    let mut workflow = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    import(&mut workflow, &mut b, &source, &key);
    let destination = temp.0.join("host/local-workspace/imported");
    let archive = std::fs::read(destination.join("import.db")).unwrap();
    let episodes = b.historical_first(all(HistoricalKind::Episode)).unwrap();
    assert_eq!(episodes.total_archived_rows, 3);
    assert_eq!(episodes.records.len(), 3);
    assert!(episodes.next.is_none());
    let agent = b.historical_detail(&find(&episodes, "memory")).unwrap();
    assert_eq!(
        exact(&agent, "embedding"),
        &ExactValue::Blob("0000803f00000080".into())
    );
    assert_eq!(
        exact(&agent, "original_extra"),
        &ExactValue::Blob("00ff".into())
    );
    assert_eq!(
        exact(&agent, "salience"),
        &ExactValue::RealBits("000000000000c0bf".into())
    );
    assert_eq!(
        exact(&agent, "created_at"),
        &ExactValue::Text("2025-01-02T03:04:05.123+03:00".into())
    );
    assert_eq!(
        exact(&agent, "valid_to"),
        &ExactValue::Text("2099-01-01T00:00:00Z".into())
    );
    assert_eq!(exact(&agent, "pinned"), &ExactValue::Integer("1".into()));
    assert_eq!(agent.record().embedding.dimensions, Some(2));
    let global = b
        .historical_detail(&find(&episodes, "global-episode"))
        .unwrap();
    assert_eq!(exact(&global, "rowid"), &ExactValue::Integer("-19".into()));
    assert_eq!(exact(&global, "embedding"), &ExactValue::Blob("01".into()));
    assert_eq!(global.record().embedding.dimensions, None);
    let scoped = b
        .historical_first(filter(
            HistoricalKind::Episode,
            AgentScope::Agent { id: "agent".into() },
            Some("session"),
            false,
        ))
        .unwrap();
    assert_eq!(
        scoped.records.len(),
        2,
        "Superseded and future-valid originals remain browsable as history"
    );
    let global_scope = b
        .historical_first(filter(
            HistoricalKind::Episode,
            AgentScope::Global,
            None,
            false,
        ))
        .unwrap();
    assert_eq!(global_scope.records, vec![global.record().clone()]);
    let lessons = b.historical_first(all(HistoricalKind::Lesson)).unwrap();
    assert_eq!(lessons.records.len(), 3);
    assert_eq!(lessons.import_id, episodes.import_id);
    let caution = b.historical_detail(&find(&lessons, "lesson")).unwrap();
    assert_eq!(
        exact(&caution, "original_extra"),
        &ExactValue::TextBytes("ff00".into())
    );
    assert_eq!(
        exact(&caution, "embedding"),
        &ExactValue::Blob("0000c07f00000080".into())
    );
    assert_eq!(
        exact(&caution, "usage_count"),
        &ExactValue::Integer(i64::MAX.to_string())
    );
    assert_eq!(
        exact(&caution, "success_count"),
        &ExactValue::Integer("7".into())
    );
    assert_eq!(
        exact(&caution, "scope"),
        &ExactValue::Text("free text; no tool grant".into())
    );
    assert_eq!(
        exact(&caution, "created_at"),
        &ExactValue::Text("2025-01-02T03:04:05.789+03:00".into())
    );
    let unknown = b
        .historical_detail(&find(&lessons, "unknown-lesson"))
        .unwrap();
    assert_eq!(
        exact(&unknown, "polarity"),
        &ExactValue::Text("unknown-original-polarity".into())
    );
    assert!(unknown
        .record()
        .warnings
        .iter()
        .any(|s| s.contains("polarity")));
    assert_eq!(unknown.record().embedding.byte_len, Some(0));
    assert_eq!(unknown.record().embedding.dimensions, None);
    for row in episodes.records.iter().chain(&lessons.records) {
        assert_eq!(row.runtime_activation, "dormant");
        assert_eq!(row.embedding.model_provenance, "unknown");
        assert!(!row.embedding.reuse_allowed);
        assert_eq!(row.overlay, HistoricalOverlay::default());
    }
    assert_dormant(&owner, &mut b);
    assert!(tree(&source) == original);
    assert_eq!(
        std::fs::read(destination.join("import.db")).unwrap(),
        archive
    );
    let review = workflow
        .review_rollback(&mut b, selection(&source))
        .unwrap();
    let ticket = workflow.rollback(&mut b, review).unwrap();
    let status = poll(&mut workflow, &mut b, &ticket);
    let report = status.operation.unwrap().rollback_report().unwrap();
    assert!(
        report.eligible
            && report.deactivated
            && report.source_unchanged
            && report.destination_unchanged
    );
    assert_eq!(status.selection, Selection::Default);
    workflow.open(&mut b).unwrap();
    assert!(b.historical_first(all(HistoricalKind::Episode)).is_err());
    assert!(tree(&source) == original);
    assert_eq!(
        std::fs::read(destination.join("import.db")).unwrap(),
        archive
    );
    assert_encrypted(&destination, &key);
    owner.stop().unwrap();
}

#[test]
#[ignore = "Requires frozen normal backend and generated encrypted historical fixture; owned loopback only"]
fn historical_trash_cas_receipts_restart_restore_keep_archive_and_refuse_rollback() {
    let temp = Temp::new();
    let key = Key::new();
    let source = temp.0.join("copy");
    fixture(&source, &key);
    let original = tree(&source);
    let backend = executable("SMITH_NATIVE_TEST_BACKEND");
    let owner = OwnedBackend::launch(&backend, &temp.0.join("host")).unwrap();
    let mut b = bridge(&owner);
    let mut workflow = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    import(&mut workflow, &mut b, &source, &key);
    let destination = temp.0.join("host/local-workspace/imported");
    let archive = std::fs::read(destination.join("import.db")).unwrap();
    let page = b.historical_first(all(HistoricalKind::Episode)).unwrap();
    let review = find(&page, "memory");
    let exact_before = b.historical_detail(&review).unwrap().original;
    let mut other = bridge(&owner);
    let stale = find(
        &other
            .historical_first(all(HistoricalKind::Episode))
            .unwrap(),
        "memory",
    );
    let trashed = b.historical_trash(&review).unwrap();
    assert_eq!(
        trashed.record().overlay,
        HistoricalOverlay {
            revision: 1,
            trashed: true
        }
    );
    assert!(other.historical_trash(&stale).is_err());
    assert!(b.historical_trash(&review).is_err());
    assert_eq!(
        b.historical_first(all(HistoricalKind::Episode))
            .unwrap()
            .records
            .len(),
        2
    );
    let trash = b
        .historical_first(filter(HistoricalKind::Episode, AgentScope::All, None, true))
        .unwrap();
    assert_eq!(trash.records, vec![trashed.record().clone()]);
    assert_dormant(&owner, &mut b);
    owner.stop().unwrap();
    drop(other);
    drop(b);
    drop(owner);
    assert_encrypted(&destination, &key);
    let owner = OwnedBackend::launch(&backend, &temp.0.join("host")).unwrap();
    let mut b = bridge(&owner);
    assert!(
        b.historical_restore(&trashed).is_err(),
        "A receipt from the previous process must not grant current authority"
    );
    let trash = b
        .historical_first(filter(HistoricalKind::Episode, AgentScope::All, None, true))
        .unwrap();
    assert_eq!(trash.records, vec![trashed.record().clone()]);
    let retained = find(&trash, "memory");
    let details = b.historical_detail(&retained).unwrap();
    assert_eq!(details.original, exact_before);
    let restored = b.historical_restore(&retained).unwrap();
    assert_eq!(
        restored.record().overlay,
        HistoricalOverlay {
            revision: 2,
            trashed: false
        }
    );
    assert_eq!(
        b.historical_detail(&restored).unwrap().original,
        exact_before
    );
    assert_eq!(
        b.historical_first(all(HistoricalKind::Episode))
            .unwrap()
            .records
            .len(),
        3
    );
    let lessons = b.historical_first(all(HistoricalKind::Lesson)).unwrap();
    let original_lesson = find(&lessons, "lesson");
    let lesson_trash = b.historical_trash(&original_lesson).unwrap();
    let lesson_restore = b.historical_restore(&lesson_trash).unwrap();
    assert_eq!(lesson_restore.record().data, original_lesson.record().data);
    assert_dormant(&owner, &mut b);
    let mut workflow = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    let review = workflow
        .review_rollback(&mut b, selection(&source))
        .unwrap();
    let ticket = workflow.rollback(&mut b, review).unwrap();
    let status = poll(&mut workflow, &mut b, &ticket);
    assert!(
        !status.operation.unwrap().succeeded(),
        "Committed overlay writes, including Trash then Restore, must refuse rollback"
    );
    assert_eq!(status.selection, Selection::Imported);
    workflow.open(&mut b).unwrap();
    assert_eq!(
        find(
            &b.historical_first(all(HistoricalKind::Episode)).unwrap(),
            "memory"
        )
        .record(),
        restored.record()
    );
    assert_eq!(
        std::fs::read(destination.join("import.db")).unwrap(),
        archive
    );
    assert!(tree(&source) == original);
    assert_encrypted(&destination, &key);
    owner.stop().unwrap();
}

#[cfg(feature = "native-ui")]
#[test]
#[ignore = "Requires frozen source-mapping backend, card fixture and software native renderer"]
fn native_original_cards_images_and_navigation_owned() {
    use slint::{ComponentHandle, Model};
    use smith_native_bridge::{
        bridge::NavigationEpoch,
        native::{register_reviewed_fonts, Driver, MainWindow},
    };
    fn wait(w: &MainWindow, d: &Driver, predicate: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            d.pump(w);
            if predicate() {
                return;
            }
            assert!(
                Instant::now() < until,
                "Native cards deadline: {}",
                w.get_status_text()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn kinds(w: &MainWindow, id: &str) -> Vec<String> {
        w.get_messages()
            .iter()
            .filter(|m| m.id == id)
            .flat_map(|m| {
                m.segments
                    .iter()
                    .map(|s| s.kind.to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    fn click_rail(w: &MainWindow, y: f32) {
        let position = slint::LogicalPosition::new(32.0, y);
        let button = slint::platform::PointerEventButton::Left;
        w.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
        w.window()
            .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
        w.window()
            .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
    }
    fn capture(w: &MainWindow, name: &str) {
        if let Some(root) = std::env::var_os("SMITH_NATIVE_TEST_CAPTURES") {
            std::fs::create_dir_all(&root).unwrap();
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
            let pixels = w.window().take_snapshot().unwrap();
            image::save_buffer(
                std::path::Path::new(&root).join(format!("{name}.png")),
                pixels.as_bytes(),
                pixels.width(),
                pixels.height(),
                image::ColorType::Rgba8,
            )
            .unwrap();
        }
    }
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("software".into()),
            mock_time: true,
            threading: false,
            ..Default::default()
        },
    )))
    .unwrap();
    register_reviewed_fonts().unwrap();
    let temp = Temp::new();
    let key = Key::new();
    let source = temp.0.join("copy");
    let witness = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    witness.set_nonblocking(true).unwrap();
    fixture_args(
        &source,
        &key,
        &[witness.local_addr().unwrap().port().to_string()],
    );
    let original = tree(&source);
    let owner = OwnedBackend::launch(
        &executable("SMITH_NATIVE_TEST_BACKEND"),
        &temp.0.join("host"),
    )
    .unwrap();
    let mut b = bridge(&owner);
    let mut workflow = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    import(&mut workflow, &mut b, &source, &key);
    let destination = temp.0.join("host/local-workspace/imported");
    let archive = std::fs::read(destination.join("import.db")).unwrap();
    b.open("session").unwrap();
    let epoch = NavigationEpoch::default();
    let card = b.historical_message("root", &epoch, None).unwrap();
    assert_eq!(card.view(&b.view, &epoch).unwrap().0.images.len(), 9);
    for index in 0..3 {
        let image = b.historical_message("root", &epoch, Some(index)).unwrap();
        let (_, preview) = image.view(&b.view, &epoch).unwrap();
        let preview = preview.unwrap();
        assert_eq!(
            preview.rgba.len(),
            (preview.width * preview.height * 4) as usize
        );
        assert!(preview.width <= 1024 && preview.height <= 1024);
    }
    for index in 3..9 {
        assert!(b.historical_message("root", &epoch, Some(index)).is_err());
    }
    assert!(b.historical_message("branch1", &epoch, None).is_err()); // inactive sibling
    let rollback = workflow
        .review_rollback(&mut b, selection(&source))
        .unwrap();
    drop(rollback); // eligibility check only, before the later explicit branch change

    let w = MainWindow::new().unwrap();
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    let d = Driver::attach_transport(&w, owner.transport());
    w.show().unwrap();
    wait(&w, &d, || d.ready());
    let index = w
        .get_sessions()
        .iter()
        .position(|c| c.id == "session")
        .unwrap();
    w.invoke_select_session(index as i32);
    wait(&w, &d, || w.get_messages().iter().any(|m| m.id == "root"));
    assert!(
        w.get_messages()
            .iter()
            .find(|m| m.id == "root")
            .unwrap()
            .historical_available
    );
    assert!(
        !w.get_messages()
            .iter()
            .find(|m| m.id == "cards-final")
            .unwrap()
            .historical_available
    );
    w.set_pinned_to_bottom(false);
    w.invoke_native_original_details("root".into(), -1);
    wait(&w, &d, || {
        kinds(&w, "root")
            .iter()
            .any(|k| k == "historical-image-load")
    });
    capture(&w, "original-image-references");
    w.invoke_native_original_details("root".into(), 0);
    wait(&w, &d, || {
        kinds(&w, "root").iter().any(|k| k == "historical-image")
    });
    capture(&w, "original-image-preview");
    w.set_active_view(1);
    d.pump(&w);
    assert!(!kinds(&w, "root")
        .iter()
        .any(|k| k.starts_with("historical-")));
    w.set_active_view(0);
    d.pump(&w);
    assert!(!kinds(&w, "root")
        .iter()
        .any(|k| k.starts_with("historical-")));
    // A queued response cannot survive leaving and returning to the same screen.
    w.invoke_native_original_details("root".into(), 0);
    click_rail(&w, 144.0);
    assert_eq!(w.get_active_view(), 1);
    click_rail(&w, 92.0);
    assert_eq!(w.get_active_view(), 0);
    d.pump(&w);
    let until = Instant::now() + Duration::from_millis(600);
    while Instant::now() < until {
        d.pump(&w);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!kinds(&w, "root")
        .iter()
        .any(|k| k.starts_with("historical-")));

    // Explicit synthetic branch selection lets the original tool card surface
    // prove literal malformed arguments and historical clarification stay inert.
    b.branch(&b.intent().unwrap(), "tool").unwrap();
    w.invoke_select_session(index as i32);
    wait(&w, &d, || {
        w.get_messages().iter().any(|m| m.id == "branch1")
    });
    w.invoke_native_original_details("branch1".into(), -1);
    wait(&w, &d, || {
        kinds(&w, "branch1").iter().filter(|k| *k == "tool").count() == 2
    });
    let tool = w.get_messages().iter().find(|m| m.id == "branch1").unwrap();
    assert!(tool
        .segments
        .iter()
        .any(|s| s.text.contains("{literal invalid")));
    assert!(!tool.segments.iter().any(|s| s.kind == "plan"));
    capture(&w, "original-tool-cards");
    w.invoke_native_original_details("tool".into(), -1);
    wait(&w, &d, || {
        kinds(&w, "tool").iter().any(|k| k == "historical-note")
    });
    capture(&w, "original-tool-result");
    assert_dormant(&owner, &mut b);
    assert!(
        witness.accept().is_err(),
        "Historical remote reference was fetched"
    );
    assert!(tree(&source) == original);
    assert_eq!(
        std::fs::read(destination.join("import.db")).unwrap(),
        archive
    );
    assert_encrypted(&destination, &key);
    drop(d);
    w.hide().unwrap();
    owner.stop().unwrap();
}
