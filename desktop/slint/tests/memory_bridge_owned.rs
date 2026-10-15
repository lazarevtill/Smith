#![cfg(unix)]
//! Explicitly selected owned normal backend, disposable automatic encrypted
//! workspace, synthetic memories only. No model, provider, or real profile.
use smith_native_bridge::{
    bridge::Bridge,
    memory_bridge::{MemoryFilter, MemoryRecord, MemoryReview, PAGE_SIZE},
    owned_backend::{OwnedBackend, OwnedTransport},
    task_bridge::new_task_request_id,
};
use std::{
    collections::BTreeSet,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "smith-native-memory-owned-{}",
            new_task_request_id().unwrap()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn backend() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("SMITH_NATIVE_TEST_BACKEND")
            .expect("An explicitly selected frozen normal backend is required"),
    );
    assert!(path.is_absolute() && path.is_file());
    path.canonicalize().unwrap()
}
fn bridge(owner: &OwnedBackend) -> Bridge<OwnedTransport> {
    let mut bridge = Bridge::new(owner.transport());
    bridge.initialize().unwrap();
    assert!(bridge.view.available);
    bridge
}
fn filter(namespace: Option<&str>, deleted: bool) -> MemoryFilter {
    MemoryFilter {
        namespace: namespace.map(str::to_owned),
        query: String::new(),
        deleted,
    }
}
fn create(b: &mut Bridge<OwnedTransport>, namespace: &str, fact: &str) -> MemoryReview {
    let editor = b
        .memory_first(filter(Some(namespace), false))
        .unwrap()
        .editor();
    let intent = editor
        .create(namespace.into(), fact.into(), vec!["synthetic".into()])
        .unwrap();
    b.memory_create(&intent).unwrap()
}
fn find(b: &mut Bridge<OwnedTransport>, namespace: &str, deleted: bool, id: &str) -> MemoryReview {
    let page = b.memory_first(filter(Some(namespace), deleted)).unwrap();
    let record = page
        .memories
        .iter()
        .find(|r| r.id == id)
        .expect("Retained memory is missing");
    page.review(record).unwrap()
}
fn assert_encrypted(root: &Path, needles: &[&[u8]]) {
    fn walk(root: &Path, needles: &[&[u8]]) {
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), needles);
            } else {
                assert!(entry.file_type().unwrap().is_file());
                let bytes = std::fs::read(entry.path()).unwrap();
                assert!(!bytes.starts_with(b"SQLite format 3\0"));
                for needle in needles {
                    assert!(
                        !bytes.windows(needle.len()).any(|part| part == *needle),
                        "Synthetic memory plaintext leaked into published storage"
                    );
                }
            }
        }
    }
    assert!(root.join("local-workspace/profile.sqlite3").is_file());
    walk(root, needles);
}

#[test]
#[ignore = "Requires an explicitly selected frozen normal backend; synthetic local workspace only"]
fn native_memory_cas_trash_restore_and_encrypted_restart_preserve_exact_records() {
    let temp = Temp::new();
    let executable = backend();
    let owner = OwnedBackend::launch(&executable, &temp.0).unwrap();
    let mut a = bridge(&owner);
    let mut concurrent = bridge(&owner);
    assert!(a
        .memory_first(filter(None, false))
        .unwrap()
        .memories
        .is_empty());
    let initial = create(&mut a, "facts", "SYNTHETIC_MEMORY_RESTART_SENTINEL_9");
    let id = initial.record().id.clone();
    let stale = find(&mut concurrent, "facts", false, &id);
    let stale_clone = stale.clone();
    let pinned = a.memory_set_pinned(&initial, true).unwrap();
    assert_eq!(pinned.record().version, 2);
    assert!(concurrent.memory_trash(&stale).is_err());
    assert!(concurrent.memory_trash(&stale_clone).is_err());
    let unchanged = find(&mut concurrent, "facts", false, &id);
    assert_eq!(unchanged.record(), pinned.record());
    let no_op = a.memory_set_pinned(&pinned, true).unwrap();
    assert_eq!(no_op.record(), pinned.record());
    let edited = a
        .memory_edit(
            &no_op,
            "SYNTHETIC_MEMORY_EDITED_SENTINEL_9".into(),
            vec!["explicit".into(), "retained".into()],
        )
        .unwrap();
    assert_eq!(edited.record().version, 3);
    let trashed = a.memory_trash(&edited).unwrap();
    assert_eq!(trashed.record().version, 4);
    assert!(a
        .memory_first(filter(Some("facts"), false))
        .unwrap()
        .memories
        .is_empty());
    let trash_page = a.memory_first(filter(Some("facts"), true)).unwrap();
    assert_eq!(trash_page.memories, vec![trashed.record().clone()]);
    assert!(a.memory_set_pinned(&trashed, false).is_err());
    assert!(a
        .memory_edit(&trashed, "Refused while trashed".into(), vec![])
        .is_err());
    let restored = a.memory_restore(&trashed).unwrap();
    assert_eq!(restored.record().version, 5);
    assert!(restored.record().pinned && !restored.record().deleted);
    let other = create(&mut a, "other", "SYNTHETIC_OTHER_NAMESPACE_SENTINEL_9");
    let all = a.memory_first(filter(None, false)).unwrap();
    assert_eq!(all.memories.len(), 2);
    let old_editor = all.editor();
    let expected: Vec<MemoryRecord> = all.memories.clone();
    owner.stop().unwrap();
    drop(a);
    drop(concurrent);
    drop(owner);
    assert_encrypted(
        &temp.0,
        &[
            b"SYNTHETIC_MEMORY_EDITED_SENTINEL_9",
            b"SYNTHETIC_OTHER_NAMESPACE_SENTINEL_9",
        ],
    );
    let owner = OwnedBackend::launch(&executable, &temp.0).unwrap();
    let mut restarted = bridge(&owner);
    assert!(
        restarted.memory_trash(&restored).is_err(),
        "A pre-restart receipt must not bind to the new session"
    );
    let old_creation = old_editor
        .create("facts".into(), "Refused stale editor".into(), vec![])
        .unwrap();
    assert!(restarted.memory_create(&old_creation).is_err());
    let retained = restarted.memory_first(filter(None, false)).unwrap();
    assert_eq!(retained.memories, expected);
    let fresh = retained
        .review(retained.memories.iter().find(|r| r.id == id).unwrap())
        .unwrap();
    let new_trash = restarted.memory_trash(&fresh).unwrap();
    assert_eq!(new_trash.record().version, 6);
    assert_eq!(
        find(&mut restarted, "other", false, &other.record().id).record(),
        other.record()
    );
    owner.stop().unwrap();
}

#[test]
#[ignore = "Requires an explicitly selected frozen normal backend; synthetic local workspace only"]
fn native_memory_pages_are_bounded_scoped_and_literal_without_read_side_effects() {
    let temp = Temp::new();
    let owner = OwnedBackend::launch(&backend(), &temp.0).unwrap();
    let mut b = bridge(&owner);
    for index in 0..35 {
        create(&mut b, "facts", &format!("Synthetic paged fact {index:02}"));
    }
    create(&mut b, "other", "Synthetic paged fact outside scope");
    let first = b.memory_first(filter(Some("facts"), false)).unwrap();
    assert_eq!(first.memories.len(), PAGE_SIZE);
    let cursor = first.next.as_ref().unwrap().clone();
    let duplicate = cursor.clone();
    let second = b.memory_next(&cursor).unwrap();
    assert_eq!(second.memories.len(), 3);
    assert!(second.next.is_none());
    assert!(b.memory_next(&duplicate).is_err());
    let ids: BTreeSet<_> = first
        .memories
        .iter()
        .chain(&second.memories)
        .map(|r| &r.id)
        .collect();
    assert_eq!(ids.len(), 35);
    assert!(first
        .memories
        .iter()
        .chain(&second.memories)
        .all(|r| r.namespace == "facts" && r.version == 1));
    let literal = create(&mut b, "literal", "Synthetic exact 100%_ MATCH marker");
    create(&mut b, "literal", "Synthetic exact 100xy MATCH marker");
    let query = MemoryFilter {
        namespace: Some("literal".into()),
        query: "100%_ match".into(),
        deleted: false,
    };
    let matched = b.memory_first(query.clone()).unwrap();
    assert_eq!(matched.memories, vec![literal.record().clone()]);
    let again = b.memory_first(query).unwrap();
    assert_eq!(
        again.memories, matched.memories,
        "Reading memory must not ingest or revise it"
    );
    let no_match = b
        .memory_first(MemoryFilter {
            namespace: Some("other".into()),
            query: "100%_".into(),
            deleted: false,
        })
        .unwrap();
    assert!(no_match.memories.is_empty());
    let scoped = b.memory_first(filter(Some("facts"), false)).unwrap();
    assert_eq!(scoped.memories, first.memories);
    owner.stop().unwrap();
}
