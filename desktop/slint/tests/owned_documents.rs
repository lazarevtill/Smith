#![cfg(unix)]
use serde_json::json;
use smith_native_bridge::{
    bridge::Bridge,
    documents::{read_selected, write_new, MAX_FILE},
    owned_backend::OwnedBackend,
};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn temp() -> Temp {
    let path = std::env::temp_dir().join(format!(
        "smith-native-documents-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&path).unwrap();
    Temp(path)
}
#[test]
fn selected_files_are_bounded_regular_and_exports_never_replace() {
    let t = temp();
    let p = t.0.join("selected.txt");
    std::fs::write(&p, b"reviewed").unwrap();
    assert_eq!(read_selected(&p).unwrap().1, b"reviewed");
    let link = t.0.join("link.txt");
    std::os::unix::fs::symlink(&p, &link).unwrap();
    assert!(read_selected(&link).is_err());
    let fifo = t.0.join("pipe.txt");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let start = std::time::Instant::now();
    assert!(read_selected(&fifo).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    let large = t.0.join("large.txt");
    std::fs::File::create(&large)
        .unwrap()
        .set_len(MAX_FILE as u64 + 1)
        .unwrap();
    assert!(read_selected(&large).is_err());
    assert!(write_new(&p, b"replace").is_err());
    assert_eq!(std::fs::read(&p).unwrap(), b"reviewed");
    assert!(write_new(&link, b"replace").is_err());
    let out = t.0.join("new.txt");
    write_new(&out, b"export").unwrap();
    assert_eq!(std::fs::read(out).unwrap(), b"export");
}
#[test]
#[ignore = "requires explicit frozen normal backend and own loopback execution"]
fn uploaded_context_draft_reopen_preview_and_exact_original_export() {
    let executable = PathBuf::from(
        std::env::var_os("SMITH_NATIVE_TEST_BACKEND").expect("explicit frozen backend"),
    );
    let t = temp();
    let profile = t.0.join("profile");
    std::fs::create_dir(&profile).unwrap();
    let backend = OwnedBackend::launch(&executable, &profile).unwrap();
    let mut b = Bridge::new(backend.transport());
    b.initialize().unwrap();
    b.new_chat().unwrap();
    let id = b.view.conversation.as_ref().unwrap().chat.id.clone();
    let selected = t.0.join("quoted \" name.txt");
    let bytes = b"Synthetic document context\nNo external content";
    std::fs::write(&selected, bytes).unwrap();
    b.inspect_document(b.view.draft.identity, &selected)
        .unwrap();
    let review = b.document_review().unwrap();
    let object = b.view.documents.pending["id"].clone();
    assert_eq!(
        b.view.documents.pending["content"]["context"],
        String::from_utf8_lossy(bytes).as_ref()
    );
    assert!(b.view.draft.media.is_empty());
    b.attach_document(&review, "context").unwrap();
    assert!(b.attach_document(&review, "context").is_err());
    assert_eq!(b.view.draft.media[0]["id"], object);
    b.home().unwrap();
    assert!(b.attach_document(&review, "context").is_err());
    b.open(&id).unwrap();
    assert_eq!(b.view.draft.media[0]["id"], object);
    b.change_draft(b.view.draft.identity, "Use the reviewed document".into())
        .unwrap();
    b.save_draft().unwrap();
    let intent = b.intent().unwrap();
    b.send(&intent, "Use the reviewed document").unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while b
        .view
        .conversation
        .as_ref()
        .unwrap()
        .active_messages
        .iter()
        .any(|m| m.status == "streaming")
    {
        b.poll().unwrap();
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        b.view.conversation.as_ref().unwrap().active_messages[0].media[0]["id"],
        object
    );
    b.save_answer(&b.answer_review().unwrap()).unwrap();
    let artifact = b
        .view
        .artifacts
        .iter()
        .find(|v| v["kind"] == "legacy_text")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    b.open_artifact(&artifact).unwrap();
    assert!(!b.view.documents.preview_text.is_empty());
    let export = t.0.join("saved-answer.txt");
    b.download_document(&b.download_review().unwrap(), &export)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(export).unwrap(),
        b.view.documents.preview_text
    );
    b.generate_table(
        "native-table.csv",
        "csv",
        json!({"headers":["Name","Count"],"rows":[[{"type":"text","value":"owned"},{"type":"number","value":"2"}]]}),
    )
    .unwrap();
    assert!(b.view.documents.preview_text.contains("owned"));
    let output = t.0.join("table.csv");
    b.download_document(&b.download_review().unwrap(), &output)
        .unwrap();
    assert!(std::fs::read_to_string(output).unwrap().contains("owned,2"));
    b.review_saved_document(b.view.draft.identity, object.as_str().unwrap())
        .unwrap();
    let import = b.document_review().unwrap();
    b.promote_document(&import).unwrap();
    assert!(b
        .view
        .artifacts
        .iter()
        .any(|v| v["document"]["id"] == object));
    assert!(b.promote_document(&import).is_err());
    b.review_saved_document(b.view.draft.identity, object.as_str().unwrap())
        .unwrap();
    b.attach_document(&b.document_review().unwrap(), "context")
        .unwrap();
    drop(b);
    backend.stop().unwrap();
    drop(backend);
    let backend = OwnedBackend::launch(&executable, &profile).unwrap();
    let mut b = Bridge::new(backend.transport());
    b.initialize().unwrap();
    b.open(&id).unwrap();
    assert_eq!(b.view.draft.media[0]["id"], object);
    assert!(b
        .view
        .artifacts
        .iter()
        .any(|v| v["name"] == "native-table.csv"));
    drop(b);
    backend.stop().unwrap();
}
