//! Semantic reversal tests against a genuine keyed profile, not a copied schema.
use super::*;
use smith_profile_storage as storage;
use std::path::PathBuf;

const SECRET: &str = "synthetic history profile passphrase";
const PAYLOAD: &str = "PRIVATE_HISTORY_CONTENT_723efc7d";
struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    db: Option<Connection>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join(storage::DB_FILE);
        let db = storage::create(&path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap();
        chat_management::create(&db, "a", "Original", 1).unwrap();
        chat_management::create(&db, "b", "Second", 1).unwrap();
        db.execute("INSERT INTO messages(id,chat_id,role,content,status,created_at) VALUES('u','a','user',?1,'complete',1)", [PAYLOAD]).unwrap();
        db.execute("INSERT INTO messages(id,chat_id,role,content,status,created_at,run_id) VALUES('one','a','assistant',?1,'complete',2,'r1'),('two','a','assistant',?1,'complete',3,'r2')", [PAYLOAD]).unwrap();
        db.execute_batch("INSERT INTO message_branches VALUES('u',NULL),('one','u'),('two','u'); INSERT INTO conversation_heads VALUES('a','two'); INSERT INTO run_configs(run_id,provider,endpoint,model) VALUES('r1','mock','http://localhost','model'),('r2','mock','http://localhost','model');").unwrap();
        db.execute(
            "INSERT INTO drafts(owner,version,text,attachments,updated_at) VALUES('a',1,?1,'[]',3)",
            [PAYLOAD],
        )
        .unwrap();
        db.execute("INSERT INTO text_attachments(id,message_id,name,media_type,content) VALUES('text','u','text.txt','text/plain',?1)", [PAYLOAD]).unwrap();
        db.execute("INSERT INTO artifacts(id,chat_id,name,content,created_at) VALUES('artifact','a','note',?1,3)", [PAYLOAD]).unwrap();
        let media_id = "76aa91d0-58c0-49a0-803d-413432c8836c";
        let manifest = smith_media::inspect(
            smith_media::AttachmentRequest::new("note.txt", Some("text/plain")),
            PAYLOAD.as_bytes(),
            &smith_media::Limits::default(),
        )
        .unwrap();
        db.execute("INSERT INTO media_objects(id,request_id,origin,name,media_type,format,sha256,byte_len,manifest_json,bytes,created_at) VALUES(?1,'96a92a16-3e7e-4ae7-a4c5-5e54e7a2fc76','upload','note.txt','text/plain','text',?2,?3,?4,?5,3)", params![media_id,manifest.sha256,manifest.byte_len,serde_json::to_string(&manifest).unwrap(),PAYLOAD.as_bytes()]).unwrap();
        db.execute(
            "INSERT INTO message_media VALUES('link','u',?1,0,'context')",
            [media_id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO artifact_media VALUES('artifact',?1)",
            [media_id],
        )
        .unwrap();
        Self {
            dir,
            path,
            db: Some(db),
        }
    }
    fn db(&self) -> &Connection {
        self.db.as_ref().unwrap()
    }
    fn db_mut(&mut self) -> &mut Connection {
        self.db.as_mut().unwrap()
    }
    fn restart(&mut self) {
        storage::close(self.db.take().unwrap()).unwrap();
        self.db = Some(
            storage::unlock(&self.path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap(),
        );
    }
    fn reverse(&mut self, redo: bool) -> Value {
        let history = status(self.db()).unwrap();
        let id = history[if redo { "redo" } else { "undo" }]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        reverse(
            self.db_mut(),
            history["revision"].as_i64().unwrap(),
            &id,
            redo,
            9,
        )
        .unwrap()
    }
    fn retained(&self) -> Vec<(String, Vec<String>)> {
        [
            "messages",
            "message_branches",
            "run_configs",
            "drafts",
            "text_attachments",
            "artifacts",
            "media_objects",
            "message_media",
            "artifact_media",
        ]
        .into_iter()
        .map(|table| {
            let mut query = self
                .db()
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let columns = query.column_count();
            let rows = query
                .query_map([], |row| {
                    Ok((0..columns)
                        .map(|column| format!("{:?}", row.get_ref(column).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            (table.into(), rows)
        })
        .collect()
    }
    fn encrypted(&self) {
        for entry in std::fs::read_dir(self.dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(path).unwrap();
            for sentinel in [
                PAYLOAD.as_bytes(),
                SECRET.as_bytes(),
                b"before_json".as_slice(),
            ] {
                assert!(!bytes.windows(sentinel.len()).any(|v| v == sentinel));
            }
            assert!(!bytes.starts_with(b"SQLite format 3\0"));
        }
    }
}
fn target(id: &str, version: i64) -> Target {
    Target {
        id: id.into(),
        version,
    }
}

#[test]
fn batch_trash_restart_undo_redo_retains_every_owned_payload() {
    let mut f = Fixture::new();
    let before = f.retained();
    let expected = revision(f.db()).unwrap();
    trash(
        f.db_mut(),
        &[target("a", 0), target("b", 0)],
        Some(expected),
        4,
    )
    .unwrap();
    assert!(chat_management::list(f.db(), "").unwrap().is_empty());
    assert_eq!(chat_management::list_trash(f.db()).unwrap().len(), 2);
    assert!(chat_management::require_active(f.db(), "a").is_err());
    assert_eq!(f.retained(), before);
    f.encrypted();
    f.restart();
    f.reverse(false);
    assert_eq!(chat_management::list(f.db(), "").unwrap().len(), 2);
    assert!(chat_management::list_trash(f.db()).unwrap().is_empty());
    f.reverse(true);
    assert!(chat_management::list(f.db(), "").unwrap().is_empty());
    restore(f.db_mut(), &[target("a", 3), target("b", 3)], None, 10).unwrap();
    assert_eq!(chat_management::list(f.db(), "").unwrap().len(), 2);
    assert_eq!(f.retained(), before);
    f.encrypted();
}

#[test]
fn rename_branch_and_trash_reverse_in_order_with_monotonic_versions() {
    let mut f = Fixture::new();
    let before = f.retained();
    rename_metadata(f.db_mut(), "a", "Renamed", 0, 2).unwrap();
    select_branch(f.db_mut(), "a", "one", 1, 3).unwrap();
    trash(f.db_mut(), &[target("a", 2)], None, 4).unwrap();
    for expected in 4..=6 {
        f.reverse(false);
        assert_eq!(capture(f.db(), "a").unwrap().1, expected);
    }
    let (view, _) = capture(f.db(), "a").unwrap();
    assert_eq!(view.title, "Original");
    assert_eq!(view.title_locked, 0);
    assert_eq!(view.leaf.as_deref(), Some("two"));
    assert_eq!(view.trashed_at, None);
    f.restart();
    for expected in 7..=9 {
        f.reverse(true);
        assert_eq!(capture(f.db(), "a").unwrap().1, expected);
    }
    let (view, _) = capture(f.db(), "a").unwrap();
    assert_eq!(view.title, "Renamed");
    assert_eq!(view.title_locked, 1);
    assert_eq!(view.leaf.as_deref(), Some("one"));
    assert_eq!(view.trashed_at, Some(4));
    assert_eq!(f.retained(), before);
}

#[test]
fn batch_is_atomic_for_stale_missing_duplicate_and_streaming_targets() {
    let mut f = Fixture::new();
    let before = status(f.db()).unwrap();
    for selected in [
        vec![target("a", 0), target("b", 7)],
        vec![target("a", 0), target("missing", 0)],
        vec![target("a", 0), target("a", 0)],
    ] {
        assert!(trash(f.db_mut(), &selected, None, 4).is_err());
        assert_eq!(status(f.db()).unwrap(), before);
        assert_eq!(capture(f.db(), "a").unwrap().1, 0);
        assert!(chat_management::list_trash(f.db()).unwrap().is_empty());
    }
    f.db()
        .execute("UPDATE messages SET status='streaming' WHERE id='two'", [])
        .unwrap();
    assert_eq!(
        trash(f.db_mut(), &[target("b", 0), target("a", 0)], None, 4)
            .unwrap_err()
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(status(f.db()).unwrap(), before);
    assert_eq!(capture(f.db(), "b").unwrap().1, 0);
    assert!(chat_management::list_trash(f.db()).unwrap().is_empty());
}

#[test]
fn stale_history_and_external_edit_cannot_be_overwritten_and_new_edit_invalidates_redo() {
    let mut f = Fixture::new();
    rename_metadata(f.db_mut(), "a", "Renamed", 0, 2).unwrap();
    let current = status(f.db()).unwrap();
    let id = current["undo"]["id"].as_str().unwrap().to_owned();
    let rev = current["revision"].as_i64().unwrap();
    assert_eq!(
        reverse(f.db_mut(), rev - 1, &id, false, 3).unwrap_err().0,
        StatusCode::CONFLICT
    );
    f.reverse(false);
    let tx = f.db_mut().transaction().unwrap();
    tx.execute("INSERT INTO artifacts(id,chat_id,name,content,created_at) VALUES('new','a','new','new local edit',3)", []).unwrap();
    chat_management::touch(&tx, "a", 3, true).unwrap();
    tx.commit().unwrap();
    assert!(status(f.db()).unwrap()["redo"].is_null());
    rename_metadata(f.db_mut(), "a", "Second rename", 3, 4).unwrap();
    f.db()
        .execute(
            "UPDATE chats SET title='Independent edit',version=version+1 WHERE id='a'",
            [],
        )
        .unwrap();
    let current = status(f.db()).unwrap();
    assert_eq!(
        reverse(
            f.db_mut(),
            current["revision"].as_i64().unwrap(),
            current["undo"]["id"].as_str().unwrap(),
            false,
            5
        )
        .unwrap_err()
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(capture(f.db(), "a").unwrap().0.title, "Independent edit");
    assert_eq!(
        f.db()
            .query_row("SELECT content FROM artifacts WHERE id='new'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        "new local edit"
    );
}

#[test]
fn restore_is_reversible_but_explicit_purge_cannot_resurrect_deleted_data() {
    let mut f = Fixture::new();
    let before = f.retained();
    trash(f.db_mut(), &[target("a", 0)], None, 2).unwrap();
    restore(f.db_mut(), &[target("a", 1)], None, 3).unwrap();
    f.reverse(false);
    assert!(capture(f.db(), "a").unwrap().0.trashed_at.is_some());
    f.reverse(true);
    assert!(capture(f.db(), "a").unwrap().0.trashed_at.is_none());
    assert_eq!(f.retained(), before);
    chat_management::delete(f.db_mut(), "a", 4, chat_management::ArtifactPolicy::Keep).unwrap();
    assert!(status(f.db()).unwrap()["undo"].is_null());
    assert!(status(f.db()).unwrap()["redo"].is_null());
    assert!(capture(f.db(), "a").is_err());
    let tombstone = f
        .db()
        .query_row(
            "SELECT version,text,attachments,media FROM drafts WHERE owner='a'",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(tombstone, (2, String::new(), "[]".into(), "[]".into()));
    assert_eq!(
        f.db()
            .query_row(
                "SELECT content FROM artifacts WHERE id='artifact'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        PAYLOAD
    );
}

#[test]
fn journal_failure_rolls_back_trash_versions_and_redo_invalidation() {
    let mut f = Fixture::new();
    rename_metadata(f.db_mut(), "a", "Renamed", 0, 2).unwrap();
    f.reverse(false);
    let before = status(f.db()).unwrap();
    let retained = f.retained();
    f.db().execute_batch("CREATE TEMP TRIGGER reject_history BEFORE INSERT ON action_history BEGIN SELECT RAISE(ABORT,'injected journal failure'); END;").unwrap();
    assert!(trash(f.db_mut(), &[target("a", 2), target("b", 0)], None, 4).is_err());
    assert_eq!(status(f.db()).unwrap(), before);
    assert_eq!(capture(f.db(), "a").unwrap().1, 2);
    assert_eq!(capture(f.db(), "b").unwrap().1, 0);
    assert!(chat_management::list_trash(f.db()).unwrap().is_empty());
    assert_eq!(f.retained(), retained);
    f.db().execute_batch("DROP TRIGGER reject_history").unwrap();
    f.reverse(true);
    assert_eq!(capture(f.db(), "a").unwrap().0.title, "Renamed");
}

#[test]
fn oversized_legacy_metadata_is_retained_without_publishing_unreadable_history() {
    let mut f = Fixture::new();
    let title = "legacy".repeat(25_000);
    f.db()
        .execute("UPDATE chats SET title=?1 WHERE id='a'", [&title])
        .unwrap();
    let before = status(f.db()).unwrap();
    assert_eq!(
        trash(f.db_mut(), &[target("a", 0)], None, 2).unwrap_err().0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(status(f.db()).unwrap(), before);
    assert_eq!(capture(f.db(), "a").unwrap().0.title, title);
    assert_eq!(capture(f.db(), "a").unwrap().1, 0);
    assert!(chat_management::list_trash(f.db()).unwrap().is_empty());
}

#[test]
fn unreadable_attachment_does_not_prevent_metadata_trash_or_restore() {
    let mut f = Fixture::new();
    f.db()
        .execute(
            "UPDATE media_objects SET manifest_json='unreadable preserved metadata'",
            [],
        )
        .unwrap();
    let before = f.retained();
    trash(f.db_mut(), &[target("a", 0)], None, 2).unwrap();
    f.reverse(false);
    assert_eq!(f.retained(), before);
    assert_eq!(
        chat_management::summary(f.db(), "a").unwrap().title,
        "Original"
    );
}

async fn request(app: Arc<App>, path: &str, input: Value) -> (StatusCode, Value) {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(input.to_string()))
        .unwrap();
    let response = routes().with_state(app).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"non_json":String::from_utf8_lossy(&bytes)})),
    )
}

#[tokio::test]
async fn http_batch_history_and_runtime_generation_guards_match_contract() {
    let mut f = Fixture::new();
    let app = crate::app_from_connection(f.db.take().unwrap()).unwrap();
    let initial = status(&app.db.lock().unwrap()).unwrap();
    let (tx, _) = watch::channel(RunState {
        message_id: "two".into(),
        content: String::new(),
        status: "streaming".into(),
        error: None,
        snapshot: Config::default(),
        warning: None,
        finish_reason: None,
        transport_incomplete: false,
        persisted: true,
    });
    app.runs.lock().unwrap().insert(
        "test-run".into(),
        Arc::new(Run {
            tx,
            cancel: Arc::new(Notify::new()),
            chat_id: "a".into(),
        }),
    );
    let batch = json!({"chats":[{"id":"a","version":0},{"id":"b","version":0}],"history_revision":initial["revision"]});
    assert_eq!(
        request(app.clone(), "/api/chats/trash", batch.clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(status(&app.db.lock().unwrap()).unwrap(), initial);
    app.runs.lock().unwrap().clear();
    let (code, trashed) = request(app.clone(), "/api/chats/trash", batch).await;
    assert_eq!(code, StatusCode::OK, "{trashed}");
    assert_eq!(trashed["chats"].as_array().unwrap().len(), 2);
    assert_eq!(trashed["history"]["undo"]["kind"], "trash");
    assert_eq!(
        request(
            app.clone(),
            "/api/actions/undo",
            json!({"revision":initial["revision"],"action_id":trashed["action"]["id"]})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (code, undone) = request(
        app.clone(),
        "/api/actions/undo",
        json!({"revision":trashed["history"]["revision"],"action_id":trashed["action"]["id"]}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{undone}");
    assert_eq!(undone["chats"][0]["trashed"], false);
    assert_eq!(undone["history"]["redo"]["kind"], "trash");
    assert_eq!(request(app.clone(), "/api/actions/redo",json!({"revision":undone["history"]["revision"],"action_id":trashed["action"]["id"],"replay_provider":true})).await.0,StatusCode::UNPROCESSABLE_ENTITY);
    let (code, redone) = request(
        app.clone(),
        "/api/actions/redo",
        json!({"revision":undone["history"]["revision"],"action_id":trashed["action"]["id"]}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{redone}");
    assert_eq!(redone["chats"][0]["trashed"], true);
    f.db = Some(Arc::try_unwrap(app).ok().unwrap().db.into_inner().unwrap());
    f.encrypted();
}
