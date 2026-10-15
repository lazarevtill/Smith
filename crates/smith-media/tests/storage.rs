#![cfg(any(target_os = "macos", target_os = "linux"))]
use smith_media::{AttachmentRequest, ErrorKind, Limits, MediaStore};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{symlink, PermissionsExt},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

fn req() -> AttachmentRequest {
    AttachmentRequest::new("notes.txt", Some("text/plain"))
}
fn store(dir: &tempfile::TempDir) -> MediaStore {
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    MediaStore::open(dir.path(), Limits::default()).unwrap()
}

#[test]
fn managed_round_trip_preserves_original_image_and_document_bytes() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    for (name, bytes) in [
        ("x.png", include_bytes!("fixtures/pixels.png").as_slice()),
        ("x.txt", include_bytes!("fixtures/unicode.txt").as_slice()),
    ] {
        let mut request = AttachmentRequest::new(name, None);
        request
            .metadata
            .insert("author".into(), "Test Fixture".into());
        let first = store.ingest_bytes(request.clone(), bytes).unwrap();
        let second = store.ingest_bytes(request.clone(), bytes).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first.manifest, second.manifest);
        assert_eq!(store.read(&first.id).unwrap(), first);
        assert_eq!(store.read_bytes(&first.id).unwrap(), bytes);
        assert_eq!(first.manifest.request, request);
        assert_eq!(
            fs::metadata(
                root.path()
                    .join("attachments")
                    .join(first.id)
                    .join("original")
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::read_dir(root.path().join(".staging")).unwrap().count(),
        0
    );
}

#[test]
fn validation_failures_never_publish_or_leave_staging_files() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    assert!(store.ingest_bytes(req(), b"bad\0text").is_err());
    assert!(store
        .ingest_bytes(AttachmentRequest::new("bad.png", None), b"broken")
        .is_err());
    assert_eq!(
        fs::read_dir(root.path().join("attachments"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(root.path().join(".staging")).unwrap().count(),
        0
    );
}

#[test]
fn source_paths_are_descriptor_confined_and_links_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let store = store(&root);
    fs::create_dir(source.path().join("nested")).unwrap();
    fs::write(source.path().join("nested/ok.txt"), "inside").unwrap();
    fs::write(outside.path().join("outside.txt"), "outside").unwrap();
    symlink(
        outside.path().join("outside.txt"),
        source.path().join("link.txt"),
    )
    .unwrap();
    symlink(outside.path(), source.path().join("linked-dir")).unwrap();
    fs::hard_link(
        outside.path().join("outside.txt"),
        source.path().join("hard.txt"),
    )
    .unwrap();
    assert!(store
        .ingest_file(source.path(), "nested/ok.txt", req())
        .is_ok());
    for path in [
        "../outside.txt",
        "/tmp/outside.txt",
        "./nested/ok.txt",
        "nested/../nested/ok.txt",
        "nested//ok.txt",
        "C:\\file.txt",
        "link.txt",
        "linked-dir/outside.txt",
        "hard.txt",
        "nested",
        "",
    ] {
        assert_eq!(
            store
                .ingest_file(source.path(), path, req())
                .unwrap_err()
                .kind,
            ErrorKind::UnsafePath,
            "path: {path}"
        );
    }
    assert_eq!(
        fs::read(outside.path().join("outside.txt")).unwrap(),
        b"outside"
    );
}

#[test]
fn fifo_is_rejected_without_waiting_for_a_writer() {
    let root = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let store = store(&root);
    let fifo = source.path().join("fifo.txt");
    #[cfg(target_os = "linux")]
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    // rustix does not expose mkfifo on macOS. This fixed system binary only creates
    // a synthetic test fixture; the ingestion library never executes processes.
    #[cfg(target_os = "macos")]
    assert!(std::process::Command::new("/usr/bin/mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    assert_eq!(
        store
            .ingest_file(source.path(), "fifo.txt", req())
            .unwrap_err()
            .kind,
        ErrorKind::UnsafePath
    );
}

#[test]
fn managed_symlinks_and_nonprivate_roots_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(MediaStore::open(root.path(), Limits::default()), Err(e) if e.kind == ErrorKind::UnsafePath)
    );
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root.path().join(".staging")).unwrap();
    assert!(
        matches!(MediaStore::open(root.path(), Limits::default()), Err(e) if e.kind == ErrorKind::UnsafePath)
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    let alias = root.path().join("alias");
    symlink(outside.path(), &alias).unwrap();
    assert!(
        matches!(MediaStore::open(&alias, Limits::default()), Err(e) if e.kind == ErrorKind::UnsafePath)
    );
}

#[test]
fn root_spellings_cannot_hide_symlinks_for_store_or_source() {
    let root = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let store = store(&root);
    fs::set_permissions(source.path(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(source.path().join("notes.txt"), "authorized fixture").unwrap();
    let alias = aliases.path().join("alias");
    symlink(source.path(), &alias).unwrap();

    // Use raw OsString appends: Path construction/iteration may normalize dots.
    for base in [alias.as_path(), source.path()] {
        for suffix in ["/", "/.", "//", "/./", "/child/.."] {
            let mut spelling = base.as_os_str().to_owned();
            spelling.push(suffix);
            let path = std::path::Path::new(&spelling);
            assert!(
                matches!(MediaStore::open(path, Limits::default()),
                Err(e) if e.kind == ErrorKind::UnsafePath),
                "store root: {path:?}"
            );
            assert_eq!(
                store
                    .ingest_file(path, "notes.txt", req())
                    .unwrap_err()
                    .kind,
                ErrorKind::UnsafePath,
                "source root: {path:?}"
            );
        }
    }
    assert_eq!(
        store
            .ingest_file(&alias, "notes.txt", req())
            .unwrap_err()
            .kind,
        ErrorKind::UnsafePath
    );
    assert_eq!(
        fs::read_dir(source.path()).unwrap().count(),
        1,
        "rejected store roots must not create managed directories"
    );
    assert_eq!(
        fs::read_dir(root.path().join("attachments"))
            .unwrap()
            .count(),
        0
    );
    let record = store
        .ingest_file(source.path(), "notes.txt", req())
        .unwrap();
    assert_eq!(store.read_bytes(&record.id).unwrap(), b"authorized fixture");
    assert!(MediaStore::open(source.path(), Limits::default()).is_ok());
}

#[test]
fn bounded_reader_reads_at_most_limit_plus_one() {
    struct CountingReader {
        consumed: usize,
    }
    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            buf.fill(b'x');
            self.consumed += buf.len();
            Ok(buf.len())
        }
    }
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = MediaStore::open(
        root.path(),
        Limits {
            max_input_bytes: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    let mut reader = CountingReader { consumed: 0 };
    assert_eq!(
        store.ingest_reader(req(), &mut reader).unwrap_err().kind,
        ErrorKind::LimitExceeded
    );
    assert_eq!(reader.consumed, 5);
    assert_eq!(
        fs::read_dir(root.path().join("attachments"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn readers_reject_corrupted_payload_and_manifest_context() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root);
    let record = store.ingest_bytes(req(), b"original").unwrap();
    let dir = root.path().join("attachments").join(&record.id);
    fs::write(dir.join("original"), b"modified").unwrap();
    assert_eq!(
        store.read(&record.id).unwrap_err().kind,
        ErrorKind::Integrity
    );
    fs::write(dir.join("original"), b"original").unwrap();
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    json["content"]["context"] = "injected context".into();
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&json).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store.read_bytes(&record.id).unwrap_err().kind,
        ErrorKind::Integrity
    );
}

#[test]
fn readers_reject_path_ids_and_replaced_payload_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let store = store(&root);
    for id in [
        "",
        "../secret",
        "/tmp/secret",
        "not-an-id",
        "../../attachments",
    ] {
        assert_eq!(store.read(id).unwrap_err().kind, ErrorKind::UnsafePath);
    }
    let record = store.ingest_bytes(req(), b"original").unwrap();
    let payload = root
        .path()
        .join("attachments")
        .join(&record.id)
        .join("original");
    fs::write(outside.path().join("secret.txt"), "outside").unwrap();
    fs::remove_file(&payload).unwrap();
    symlink(outside.path().join("secret.txt"), &payload).unwrap();
    assert_eq!(
        store.read_bytes(&record.id).unwrap_err().kind,
        ErrorKind::UnsafePath
    );
}

#[test]
fn concurrent_publication_never_exposes_half_an_attachment() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(store(&root));
    let finished = Arc::new(AtomicBool::new(false));
    let observed = {
        let finished = Arc::clone(&finished);
        let path = root.path().join("attachments");
        std::thread::spawn(move || {
            let mut seen = 0;
            while !finished.load(Ordering::Acquire) {
                for entry in fs::read_dir(&path).unwrap() {
                    let path = entry.unwrap().path();
                    assert!(path.join("original").is_file());
                    assert!(path.join("manifest.json").is_file());
                    seen += 1;
                }
                std::thread::yield_now();
            }
            seen
        })
    };
    let handles: Vec<_> = (0..12)
        .map(|i| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .ingest_bytes(req(), format!("attachment {i}").as_bytes())
                    .unwrap()
            })
        })
        .collect();
    for handle in handles {
        let record = handle.join().unwrap();
        assert_eq!(store.read(&record.id).unwrap(), record);
    }
    finished.store(true, Ordering::Release);
    let _observations = observed.join().unwrap();
    assert_eq!(
        fs::read_dir(root.path().join("attachments"))
            .unwrap()
            .count(),
        12
    );
    assert_eq!(
        fs::read_dir(root.path().join(".staging")).unwrap().count(),
        0
    );
}
