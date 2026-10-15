//! Durable, profile-encrypted reversal of local conversation metadata only.
//! Content, drafts, media and provider calls are never deleted or replayed here.
use crate::*;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub id: String,
    pub version: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct View {
    id: String,
    title: String,
    title_locked: i64,
    leaf: Option<String>,
    trashed_at: Option<i64>,
}
struct Entry {
    id: String,
    kind: String,
    before: Vec<View>,
    after: Vec<View>,
}

fn conflict() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "History or a conversation changed. Refresh before applying this action.".into(),
    )
}
fn corrupt() -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Saved action history is invalid. Existing conversations were retained.".into(),
    )
}
fn revision(db: &Connection) -> ApiResult<i64> {
    Ok(db.query_row(
        "SELECT revision FROM action_history_state WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}
fn check_revision(db: &Connection, expected: Option<i64>) -> ApiResult<()> {
    if let Some(expected) = expected {
        if revision(db)? != expected {
            return Err(conflict());
        }
    }
    Ok(())
}

/// Call in the same transaction as any new persisted user edit that is not
/// itself journaled. It cannot replay or discard content.
pub fn invalidate_redo(db: &Connection) -> rusqlite::Result<()> {
    db.execute(
        "UPDATE action_history SET state='invalidated' WHERE state='undone'",
        [],
    )?;
    db.execute(
        "UPDATE action_history_state SET revision=revision+1 WHERE id=1",
        [],
    )?;
    Ok(())
}

fn capture(db: &Connection, id: &str) -> ApiResult<(View, i64)> {
    let row: Option<(String, i64, i64)> = db
        .query_row(
            "SELECT title,title_locked,version FROM chats WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (title, title_locked, version) =
        row.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "Conversation not found.".into()))?;
    let leaf = db
        .query_row(
            "SELECT leaf_id FROM conversation_heads WHERE chat_id=?1",
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let trashed_at = db
        .query_row(
            "SELECT trashed_at FROM chat_trash WHERE chat_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok((
        View {
            id: id.into(),
            title,
            title_locked,
            leaf,
            trashed_at,
        },
        version,
    ))
}
fn inactive(db: &Connection, id: &str) -> ApiResult<()> {
    if db.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE chat_id=?1 AND status='streaming')",
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Stop the active response and wait for it to finish before changing this conversation."
                .into(),
        ));
    }
    Ok(())
}
fn targets(db: &Connection, targets: &[Target]) -> ApiResult<Vec<View>> {
    if targets.is_empty() || targets.len() > 100 {
        return Err(bad("Select between 1 and 100 conversations."));
    }
    let mut ids = BTreeSet::new();
    let mut before = Vec::with_capacity(targets.len());
    for target in targets {
        if !ids.insert(&target.id) {
            return Err(bad("A conversation was selected more than once."));
        }
        let (view, version) = capture(db, &target.id)?;
        if version != target.version {
            return Err(conflict());
        }
        inactive(db, &target.id)?;
        before.push(view);
    }
    before.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(before)
}
fn encode(views: &[View]) -> ApiResult<String> {
    let encoded = serde_json::to_string(views).map_err(|_| corrupt())?;
    if encoded.len() > 128 * 1024 {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE,"This selection contains too much saved conversation metadata for one reversible action. Select fewer conversations.".into()));
    }
    Ok(encoded)
}
fn decode(value: &str) -> ApiResult<Vec<View>> {
    if value.len() > 128 * 1024 {
        return Err(corrupt());
    }
    let views: Vec<View> = serde_json::from_str(value).map_err(|_| corrupt())?;
    let ids: BTreeSet<_> = views.iter().map(|v| &v.id).collect();
    if views.is_empty() || views.len() > 100 || ids.len() != views.len() {
        return Err(corrupt());
    }
    Ok(views)
}
fn apply(tx: &Transaction<'_>, views: &[View], now: u64) -> ApiResult<()> {
    for view in views {
        if let Some(leaf) = &view.leaf {
            let owned: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND chat_id=?2)",
                params![leaf, view.id],
                |row| row.get(0),
            )?;
            if !owned {
                return Err(corrupt());
            }
        }
        tx.execute("UPDATE chats SET title=?2,title_locked=?3,version=version+1,updated_at=?4,activity=(SELECT COALESCE(MAX(activity),0)+1 FROM chats) WHERE id=?1",params![view.id,view.title,view.title_locked,now])?;
        match &view.leaf {
            Some(leaf) => {
                conversations::set_leaf(tx, &view.id, leaf)?;
            }
            None => {
                tx.execute(
                    "DELETE FROM conversation_heads WHERE chat_id=?1",
                    [&view.id],
                )?;
            }
        }
        match view.trashed_at {
            Some(when) => {
                tx.execute("INSERT INTO chat_trash(chat_id,trashed_at) VALUES(?1,?2) ON CONFLICT(chat_id) DO UPDATE SET trashed_at=excluded.trashed_at",params![view.id,when])?;
            }
            None => {
                tx.execute("DELETE FROM chat_trash WHERE chat_id=?1", [&view.id])?;
            }
        }
        tx.execute("INSERT INTO action_history_versions(chat_id,version) SELECT id,version FROM chats WHERE id=?1 ON CONFLICT(chat_id) DO UPDATE SET version=excluded.version",[&view.id])?;
    }
    Ok(())
}
fn record(
    tx: &Transaction<'_>,
    kind: &str,
    before: &[View],
    after: &[View],
    now: u64,
) -> ApiResult<String> {
    invalidate_redo(tx)?;
    let id = crate::id();
    tx.execute("INSERT INTO action_history(id,kind,before_json,after_json,state,created_at) VALUES(?1,?2,?3,?4,'applied',?5)",params![id,kind,encode(before)?,encode(after)?,now])?;
    Ok(id)
}
fn top(db: &Connection, redo: bool) -> ApiResult<Option<Entry>> {
    let query = if redo {
        "SELECT id,kind,before_json,after_json FROM action_history WHERE state='undone' ORDER BY sequence ASC LIMIT 1"
    } else {
        "SELECT id,kind,before_json,after_json FROM action_history WHERE state='applied' ORDER BY sequence DESC LIMIT 1"
    };
    let row: Option<(String, String, String, String)> = db
        .query_row(query, [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    row.map(|(id, kind, before, after)| {
        Ok(Entry {
            id,
            kind,
            before: decode(&before)?,
            after: decode(&after)?,
        })
    })
    .transpose()
}
fn entry_view(entry: Entry) -> Value {
    let label = match entry.kind.as_str() {
        "rename" => "Rename conversation".into(),
        "branch" => "Switch conversation branch".into(),
        "trash" => format!("Move {} conversation(s) to Trash", entry.after.len()),
        "restore" => format!("Restore {} conversation(s)", entry.after.len()),
        _ => "Conversation change".into(),
    };
    json!({"id":entry.id,"kind":entry.kind,"label":label,"chat_ids":entry.after.iter().map(|v|&v.id).collect::<Vec<_>>()})
}
pub fn status(db: &Connection) -> ApiResult<Value> {
    Ok(
        json!({"revision":revision(db)?,"undo":top(db,false)?.map(entry_view),"redo":top(db,true)?.map(entry_view)}),
    )
}
fn response(db: &Connection, views: &[View], action_id: &str, kind: &str) -> ApiResult<Value> {
    let chats = views
        .iter()
        .map(|v| {
            let (current, version) = capture(db, &v.id)?;
            Ok(json!({"id":v.id,"version":version,"trashed":current.trashed_at.is_some()}))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(json!({"ok":true,"history":status(db)?,"chats":chats,"action":{"id":action_id,"kind":kind}}))
}

pub fn rename_metadata(
    db: &mut Connection,
    id: &str,
    title: &str,
    version: i64,
    now: u64,
) -> ApiResult<()> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 100 || title.chars().any(char::is_control) {
        return Err(bad(
            "Use a title of 1–100 characters without line breaks or control characters.",
        ));
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = targets(
        &tx,
        &[Target {
            id: id.into(),
            version,
        }],
    )?;
    if before[0].trashed_at.is_some() {
        return Err(conflict());
    }
    let mut after = before.clone();
    after[0].title = title.into();
    after[0].title_locked = 1;
    if before != after {
        apply(&tx, &after, now)?;
        record(&tx, "rename", &before, &after, now)?;
    }
    tx.commit()?;
    Ok(())
}
pub fn select_branch(
    db: &mut Connection,
    id: &str,
    message_id: &str,
    version: i64,
    now: u64,
) -> ApiResult<Value> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = targets(
        &tx,
        &[Target {
            id: id.into(),
            version,
        }],
    )?;
    if before[0].trashed_at.is_some() {
        return Err(conflict());
    }
    let all = conversations::all(&tx, id)?;
    let tip = conversations::tip(&all, message_id)?;
    conversations::path(&all, Some(&tip))?;
    let mut after = before.clone();
    after[0].leaf = Some(tip);
    if before != after {
        apply(&tx, &after, now)?;
        record(&tx, "branch", &before, &after, now)?;
    }
    tx.commit()?;
    conversations::payload(db, id)
}
pub fn trash(
    db: &mut Connection,
    selected: &[Target],
    expected_history: Option<i64>,
    now: u64,
) -> ApiResult<Value> {
    move_trash(db, selected, expected_history, now, false)
}
pub fn restore(
    db: &mut Connection,
    selected: &[Target],
    expected_history: Option<i64>,
    now: u64,
) -> ApiResult<Value> {
    move_trash(db, selected, expected_history, now, true)
}
fn move_trash(
    db: &mut Connection,
    selected: &[Target],
    expected_history: Option<i64>,
    now: u64,
    restoring: bool,
) -> ApiResult<Value> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_revision(&tx, expected_history)?;
    let before = targets(&tx, selected)?;
    if before.iter().any(|v| v.trashed_at.is_some() != restoring) {
        return Err(conflict());
    }
    let mut after = before.clone();
    for view in &mut after {
        view.trashed_at = if restoring { None } else { Some(now as i64) };
    }
    apply(&tx, &after, now)?;
    let kind = if restoring { "restore" } else { "trash" };
    let id = record(&tx, kind, &before, &after, now)?;
    let value = response(&tx, &after, &id, kind)?;
    tx.commit()?;
    Ok(value)
}
pub fn reverse(
    db: &mut Connection,
    expected_history: i64,
    action_id: &str,
    redo: bool,
    now: u64,
) -> ApiResult<Value> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_revision(&tx, Some(expected_history))?;
    let entry = top(&tx, redo)?.ok_or_else(conflict)?;
    if entry.id != action_id {
        return Err(conflict());
    }
    if entry.before.iter().map(|v| &v.id).collect::<Vec<_>>()
        != entry.after.iter().map(|v| &v.id).collect::<Vec<_>>()
    {
        return Err(corrupt());
    }
    let expected = if redo { &entry.before } else { &entry.after };
    let desired = if redo { &entry.after } else { &entry.before };
    for view in expected {
        let (current, version) = capture(&tx, &view.id)?;
        inactive(&tx, &view.id)?;
        let journal_version: Option<i64> = tx
            .query_row(
                "SELECT version FROM action_history_versions WHERE chat_id=?1",
                [&view.id],
                |r| r.get(0),
            )
            .optional()?;
        if current != *view || journal_version != Some(version) {
            return Err(conflict());
        }
    }
    apply(&tx, desired, now)?;
    tx.execute(
        "UPDATE action_history SET state=?2 WHERE id=?1",
        params![entry.id, if redo { "applied" } else { "undone" }],
    )?;
    tx.execute(
        "UPDATE action_history_state SET revision=revision+1 WHERE id=1",
        [],
    )?;
    let value = response(&tx, desired, &entry.id, &entry.kind)?;
    tx.commit()?;
    Ok(value)
}

/// An explicitly requested irreversible purge invalidates entries touching its
/// deleted owner. Routine Trash never calls this function or deletes payloads.
pub fn forget_chat(tx: &Transaction<'_>, id: &str) -> ApiResult<()> {
    let rows = tx
        .prepare("SELECT id,before_json FROM action_history WHERE state!='invalidated'")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (action, before) in rows {
        if decode(&before)?.iter().any(|v| v.id == id) {
            tx.execute(
                "UPDATE action_history SET state='invalidated' WHERE id=?1",
                [action],
            )?;
        }
    }
    invalidate_redo(tx)?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    chats: Vec<Target>,
    history_revision: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reverse {
    revision: i64,
    action_id: String,
}
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/actions", get(history))
        .route("/api/actions/undo", post(undo))
        .route("/api/actions/redo", post(redo))
        .route("/api/trash", get(trash_list))
        .route("/api/chats/trash", post(trash_batch))
        .route("/api/chats/restore", post(restore_batch))
}
async fn history(State(app): State<Arc<App>>) -> ApiResult<Json<Value>> {
    Ok(Json(status(&app.db.lock().unwrap())?))
}
async fn trash_list(State(app): State<Arc<App>>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"chats":chat_management::list_trash(&app.db.lock().unwrap())?}),
    ))
}
async fn trash_batch(
    State(app): State<Arc<App>>,
    Json(input): Json<Batch>,
) -> ApiResult<Json<Value>> {
    let runs = app.runs.lock().unwrap();
    guard_runs(&runs, input.chats.iter().map(|v| v.id.as_str()))?;
    Ok(Json(trash(
        &mut app.db.lock().unwrap(),
        &input.chats,
        Some(input.history_revision),
        now(),
    )?))
}
async fn restore_batch(
    State(app): State<Arc<App>>,
    Json(input): Json<Batch>,
) -> ApiResult<Json<Value>> {
    let runs = app.runs.lock().unwrap();
    guard_runs(&runs, input.chats.iter().map(|v| v.id.as_str()))?;
    Ok(Json(restore(
        &mut app.db.lock().unwrap(),
        &input.chats,
        Some(input.history_revision),
        now(),
    )?))
}
async fn undo(State(app): State<Arc<App>>, Json(input): Json<Reverse>) -> ApiResult<Json<Value>> {
    let runs = app.runs.lock().unwrap();
    let mut db = app.db.lock().unwrap();
    if let Some(entry) = top(&db, false)? {
        guard_runs(&runs, entry.after.iter().map(|v| v.id.as_str()))?;
    }
    Ok(Json(reverse(
        &mut db,
        input.revision,
        &input.action_id,
        false,
        now(),
    )?))
}
async fn redo(State(app): State<Arc<App>>, Json(input): Json<Reverse>) -> ApiResult<Json<Value>> {
    let runs = app.runs.lock().unwrap();
    let mut db = app.db.lock().unwrap();
    if let Some(entry) = top(&db, true)? {
        guard_runs(&runs, entry.after.iter().map(|v| v.id.as_str()))?;
    }
    Ok(Json(reverse(
        &mut db,
        input.revision,
        &input.action_id,
        true,
        now(),
    )?))
}

fn guard_runs<'a>(
    runs: &HashMap<String, Arc<Run>>,
    ids: impl Iterator<Item = &'a str>,
) -> ApiResult<()> {
    for id in ids {
        if runs
            .values()
            .any(|run| run.chat_id == id && run.tx.borrow().status == "streaming")
        {
            return Err(ApiError(StatusCode::CONFLICT, "Stop the active response and wait for it to finish before changing this conversation.".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/support/action_history.rs"]
mod tests;
