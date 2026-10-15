//! Profile-encrypted composer drafts with optimistic versions and durable tombstones.
use crate::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteDraft {
    version: i64,
    text: String,
    #[serde(default)]
    attachments: Vec<conversations::Attachment>,
    #[serde(default)]
    media: Vec<media_api::MediaReference>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteDraft {
    version: i64,
}
pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/api/drafts/{owner}", get(read).put(write).delete(delete))
}
fn owner(db: &Connection, value: &str, writing: bool) -> ApiResult<()> {
    if value == "new" {
        return Ok(());
    }
    // Historical IDs are opaque database keys, not necessarily UUIDs.
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(bad(
            "Choose the new workspace or an existing conversation draft.",
        ));
    }
    if writing
        && !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1)",
            [value],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Conversation no longer exists; your draft was not overwritten.".into(),
        ));
    }
    if writing {
        chat_management::require_active(db, value)?;
    }
    Ok(())
}
fn current(db: &Connection, owner: &str) -> ApiResult<Value> {
    let row = db
        .query_row(
            "SELECT version,text,attachments,media FROM drafts WHERE owner=?1",
            [owner],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    match row {
        None => Ok(json!({"owner":owner,"version":0,"text":"","attachments":[],"media":[]})),
        Some((version, text, attachments, media)) => {
            let attachments: Value = serde_json::from_str(&attachments).map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Saved draft metadata could not be read. It was not replaced.".into(),
                )
            })?;
            let media: Value = serde_json::from_str(&media)
                .map_err(|_| bad("Saved media draft metadata is invalid; it was not replaced."))?;
            Ok(
                json!({"owner":owner,"version":version,"text":text,"attachments":attachments,"media":media}),
            )
        }
    }
}
fn mutate(
    db: &mut Connection,
    owner: &str,
    expected: i64,
    text: &str,
    attachments: &[conversations::Attachment],
    media: &[media_api::MediaReference],
    writing: bool,
) -> ApiResult<Value> {
    if expected < 0 {
        return Err(bad("Include the draft version you loaded."));
    }
    let tx = db.transaction()?;
    self::owner(&tx, owner, writing)?;
    let version: i64 = tx
        .query_row("SELECT version FROM drafts WHERE owner=?1", [owner], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(0);
    if version != expected {
        return Err(ApiError(StatusCode::CONFLICT,"This draft changed in another tab. Keep your visible text and reload the saved draft before choosing which version to retain.".into()));
    }
    let next = version
        .checked_add(1)
        .ok_or_else(|| bad("Draft version limit reached."))?;
    if !media.is_empty() {
        let prepared = media_api::validate_references(&tx, media)?;
        let bytes = prepared
            .iter()
            .map(|p| match &p.manifest.content {
                smith_media::Content::Document { context, .. } => context.len(),
                _ => 0,
            })
            .sum::<usize>();
        if bytes + attachments.iter().map(|a| a.content.len()).sum::<usize>() > 100_000 {
            return Err(bad("Draft excerpts must total at most 100,000 bytes."));
        }
    }
    let media =
        serde_json::to_string(media).map_err(|_| bad("Draft media could not be encoded."))?;
    let payload = serde_json::to_string(attachments)
        .map_err(|_| bad("Draft attachments could not be encoded."))?;
    let prior: Option<(String, String, String)> = tx
        .query_row(
            "SELECT text,attachments,media FROM drafts WHERE owner=?1",
            [owner],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if prior
        .as_ref()
        .is_none_or(|(old_text, old_attachments, old_media)| {
            old_text != text || old_attachments != &payload || old_media != &media
        })
    {
        action_history::invalidate_redo(&tx)?;
    }
    tx.execute("INSERT INTO drafts(owner,version,text,attachments,media,updated_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(owner) DO UPDATE SET version=excluded.version,text=excluded.text,attachments=excluded.attachments,media=excluded.media,updated_at=excluded.updated_at", params![owner,next,text,payload,media,now()])?;
    tx.commit()?;
    current(db, owner)
}
async fn read(State(a): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let db = a.db.lock().unwrap();
    owner(&db, &id, false)?;
    Ok(Json(current(&db, &id)?))
}
async fn write(
    State(a): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<WriteDraft>,
) -> ApiResult<Json<Value>> {
    if input.text.len() > 100_000 {
        return Err(bad("Draft text is limited to 100,000 bytes."));
    }
    conversations::validate_attachments(&input.attachments)?;
    if input.attachments.len() + input.media.len() > 3 {
        return Err(bad("Attach at most three files to a draft."));
    }
    Ok(Json(mutate(
        &mut a.db.lock().unwrap(),
        &id,
        input.version,
        &input.text,
        &input.attachments,
        &input.media,
        true,
    )?))
}
async fn delete(
    State(a): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<DeleteDraft>,
) -> ApiResult<Json<Value>> {
    Ok(Json(mutate(
        &mut a.db.lock().unwrap(),
        &id,
        input.version,
        "",
        &[],
        &[],
        false,
    )?))
}
// Called inside the chat deletion transaction. Keep the version to prevent ABA.
pub fn clear_for_deleted_chat(db: &Connection, id: &str) -> ApiResult<()> {
    db.execute(
        "UPDATE drafts SET version=version+1,text='',attachments='[]',media='[]',updated_at=?2 WHERE owner=?1",
        params![id, now()],
    )?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn imported_opaque_chat_ids_can_keep_encrypted_drafts() {
        let mut db = Connection::open_in_memory().unwrap();
        smith_profile_storage::initialize_schema(&db).unwrap();
        chat_management::create(&db, "migration-chat", "Imported", 1).unwrap();
        let saved = mutate(
            &mut db,
            "migration-chat",
            0,
            "retained imported draft",
            &[],
            &[],
            true,
        )
        .unwrap();
        assert_eq!(saved["text"], "retained imported draft");
        assert_eq!(
            mutate(&mut db, "missing-chat", 0, "not accepted", &[], &[], true)
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
    }
    #[test]
    fn draft_versions_survive_delete_and_stale_writes() {
        let mut db = Connection::open_in_memory().unwrap();
        smith_profile_storage::initialize_schema(&db).unwrap();
        let first = mutate(&mut db, "new", 0, "kept draft", &[], &[], true).unwrap();
        assert_eq!(first["version"], 1);
        assert_eq!(
            mutate(&mut db, "new", 0, "stale", &[], &[], true)
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(current(&db, "new").unwrap()["text"], "kept draft");
        let cleared = mutate(&mut db, "new", 1, "", &[], &[], false).unwrap();
        assert_eq!(cleared["version"], 2);
        assert_eq!(
            mutate(&mut db, "new", 0, "ABA", &[], &[], true)
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            mutate(&mut db, "new", 2, "after delete", &[], &[], true).unwrap()["version"],
            3
        );
    }
}
