//! Local feedback on the exact saved answer. No network call, model training,
//! memory promotion, or automatic agent action follows a rating.
use crate::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    chat_version: i64,
    expected_revision: i64,
    kind: String,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route(
        "/api/chats/{chat}/messages/{message}/feedback",
        axum::routing::patch(write),
    )
}

pub fn current(db: &Connection, message: &str) -> ApiResult<Value> {
    let saved: Option<(String, i64, i64)> = db
        .query_row(
            "SELECT kind,revision,updated_at FROM message_feedback WHERE message_id=?1",
            [message],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match saved {
        Some((kind, revision, updated_at)) => {
            json!({"message_id":message,"kind":kind,"revision":revision,"updated_at":updated_at})
        }
        None => json!({"message_id":message,"kind":"","revision":0,"updated_at":Value::Null}),
    })
}

fn mutate(db: &mut Connection, chat: &str, message: &str, input: Input) -> ApiResult<Value> {
    if input.chat_version < 0
        || input.expected_revision < 0
        || !matches!(input.kind.as_str(), "" | "thumbs_up" | "thumbs_down")
    {
        return Err(bad(
            "Choose thumbs up, thumbs down, or clear for the saved answer.",
        ));
    }
    let tx = db.transaction()?;
    chat_management::check_version(&tx, chat, input.chat_version)?;
    let saved: Option<(String, String, String)> = tx
        .query_row(
            "SELECT role,status,content FROM messages WHERE id=?1 AND chat_id=?2",
            params![message, chat],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((role, status, content)) = saved else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Saved answer not found in this conversation.".into(),
        ));
    };
    if role != "assistant"
        || content.trim().is_empty()
        || !matches!(
            status.as_str(),
            "completed" | "limited" | "cancelled" | "interrupted" | "failed"
        )
    {
        return Err(bad(
            "Feedback is available after an assistant answer has finished.",
        ));
    }
    // UNION de-duplicates nodes, so a damaged cycle cannot make this lookup
    // loop forever. The normal profile validation independently rejects cycles.
    let active: bool=tx.query_row(
        "WITH RECURSIVE branch(id) AS (SELECT COALESCE((SELECT leaf_id FROM conversation_heads WHERE chat_id=?1),(SELECT id FROM messages WHERE chat_id=?1 ORDER BY rowid DESC LIMIT 1)) UNION SELECT b.parent_id FROM message_branches b JOIN branch p ON b.message_id=p.id WHERE b.parent_id IS NOT NULL) SELECT EXISTS(SELECT 1 FROM branch WHERE id=?2)",
        params![chat,message], |r| r.get(0),
    )?;
    if !active {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Open this answer's branch before changing its feedback.".into(),
        ));
    }
    let revision = current(&tx, message)?["revision"].as_i64().unwrap();
    if revision != input.expected_revision {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "This feedback changed. Reload the saved answer before choosing again.".into(),
        ));
    }
    let next = revision
        .checked_add(1)
        .ok_or_else(|| bad("Feedback revision limit reached."))?;
    tx.execute("INSERT INTO message_feedback(message_id,kind,revision,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(message_id) DO UPDATE SET kind=excluded.kind,revision=excluded.revision,updated_at=excluded.updated_at",params![message,input.kind,next,now()])?;
    // Empty kind is a durable tombstone: clear must not make an old version-zero
    // view valid again. Feedback does not alter the selected branch or text.
    tx.commit()?;
    current(db, message)
}

async fn write(
    State(a): State<Arc<App>>,
    Path((chat, message)): Path<(String, String)>,
    Json(input): Json<Input>,
) -> ApiResult<Json<Value>> {
    Ok(Json(mutate(
        &mut a.db.lock().unwrap(),
        &chat,
        &message,
        input,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "foreign_keys", true).unwrap();
        smith_profile_storage::initialize_schema(&db).unwrap();
        chat_management::create(&db, "chat", "Synthetic feedback", 1).unwrap();
        chat_management::create(&db, "other", "Other", 1).unwrap();
        db.execute_batch("INSERT INTO messages(id,chat_id,role,content,status,created_at) VALUES('user','chat','user','Synthetic question','completed',1),('answer','chat','assistant','Synthetic answer','completed',2),('alternative','chat','assistant','Alternative','completed',3); INSERT INTO message_branches VALUES('user',NULL),('answer','user'),('alternative','user'); INSERT INTO conversation_heads VALUES('chat','answer');").unwrap();
        db
    }
    fn input(revision: i64, kind: &str) -> Input {
        Input {
            chat_version: 0,
            expected_revision: revision,
            kind: kind.into(),
        }
    }
    #[test]
    fn ratings_clear_with_cas_without_changing_answer_or_branch() {
        let mut db = fixture();
        assert_eq!(
            mutate(&mut db, "chat", "answer", input(0, "thumbs_up")).unwrap()["revision"],
            1
        );
        assert_eq!(
            mutate(&mut db, "chat", "answer", input(0, "thumbs_down"))
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            mutate(&mut db, "chat", "answer", input(1, "")).unwrap()["revision"],
            2
        );
        assert!(mutate(&mut db, "chat", "answer", input(0, "thumbs_up")).is_err());
        assert_eq!(
            db.query_row("SELECT content FROM messages WHERE id='answer'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "Synthetic answer"
        );
        assert_eq!(
            db.query_row(
                "SELECT leaf_id FROM conversation_heads WHERE chat_id='chat'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "answer"
        );
    }
    #[test]
    fn wrong_owner_branch_status_and_stale_chat_refuse_without_write() {
        let mut db = fixture();
        for (chat, message, kind) in [
            ("other", "answer", "thumbs_up"),
            ("chat", "user", "thumbs_up"),
            ("chat", "alternative", "thumbs_down"),
            ("chat", "answer", "arbitrary"),
        ] {
            assert!(mutate(&mut db, chat, message, input(0, kind)).is_err());
        }
        db.execute(
            "UPDATE messages SET status='streaming' WHERE id='answer'",
            [],
        )
        .unwrap();
        assert!(mutate(&mut db, "chat", "answer", input(0, "thumbs_up")).is_err());
        db.execute(
            "UPDATE messages SET status='completed' WHERE id='answer'",
            [],
        )
        .unwrap();
        db.execute("UPDATE chats SET version=1 WHERE id='chat'", [])
            .unwrap();
        assert_eq!(
            mutate(&mut db, "chat", "answer", input(0, "thumbs_up"))
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM message_feedback", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn encrypted_restart_retains_feedback_and_cleared_revision() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .canonicalize()
            .unwrap()
            .join(smith_profile_storage::DB_FILE);
        let key =
            smith_profile_storage::Secret::new("synthetic feedback fixture key".into()).unwrap();
        let mut db = smith_profile_storage::create(&path, &key).unwrap();
        chat_management::create(&db, "chat", "Synthetic", 1).unwrap();
        db.execute_batch("INSERT INTO messages(id,chat_id,role,content,status,created_at) VALUES('answer','chat','assistant','Synthetic','completed',1); INSERT INTO message_branches VALUES('answer',NULL); INSERT INTO conversation_heads VALUES('chat','answer');").unwrap();
        mutate(&mut db, "chat", "answer", input(0, "thumbs_down")).unwrap();
        smith_profile_storage::close(db).unwrap();
        let mut db = smith_profile_storage::unlock(&path, &key).unwrap();
        assert_eq!(current(&db, "answer").unwrap()["kind"], "thumbs_down");
        mutate(&mut db, "chat", "answer", input(1, "")).unwrap();
        smith_profile_storage::close(db).unwrap();
        let db = smith_profile_storage::unlock(&path, &key).unwrap();
        assert_eq!(current(&db, "answer").unwrap()["revision"], 2);
        assert_eq!(current(&db, "answer").unwrap()["kind"], "");
        smith_profile_storage::close(db).unwrap();
    }
}
