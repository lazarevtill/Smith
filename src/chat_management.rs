//! Local chat management ported from the original Smith session workflow.
//! Reference: smith-storage/src/chat.rs list_sessions / rename_session /
//! delete_session; smith-ui/ui/app-window.slint session title/subtitle search.
//! No memory, interaction-signal, or lesson tables exist in this pilot.
//!
//! Bounded parity: trims explicit human renames, bumps recent activity, derives
//! the latest-message 48-character preview, and searches title + that preview.
//! This is not full transcript search. The selected branch supplies the preview.
//! Routine deletion is durable Trash in action_history, preserving every owner
//! row and payload. This module retains the explicit hard-purge primitive for
//! separately confirmed destructive requests. App data lives in keyed SQLCipher.
use crate::{bad, ApiError, ApiResult, StatusCode};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Summary {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub version: i64,
}
fn preview(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Summary> {
    let stored: String = r.get(1)?;
    let first: Option<String> = r.get(5)?;
    let last: Option<String> = r.get(6)?;
    Ok(Summary {
        id: r.get(0)?,
        title: if stored.trim().is_empty() {
            first
                .map(|s| preview(&s, 40))
                .unwrap_or("New conversation".into())
        } else {
            stored
        },
        created_at: r.get(2)?,
        updated_at: r.get(3)?,
        version: r.get(4)?,
        subtitle: last
            .map(|s| preview(&s, 48))
            .unwrap_or("Start a conversation".into()),
    })
}
const SELECT:&str="SELECT c.id,c.title,c.created_at,c.updated_at,c.version,(SELECT content FROM messages WHERE chat_id=c.id AND role='user' ORDER BY rowid LIMIT 1),COALESCE((SELECT content FROM messages WHERE id=(SELECT leaf_id FROM conversation_heads WHERE chat_id=c.id)),(SELECT content FROM messages WHERE chat_id=c.id ORDER BY rowid DESC LIMIT 1)) FROM chats c";
pub fn summary(db: &Connection, id: &str) -> ApiResult<Summary> {
    db.query_row(&format!("{SELECT} WHERE c.id=?1 AND NOT EXISTS(SELECT 1 FROM chat_trash t WHERE t.chat_id=c.id)"), [id], row)
        .optional()?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "Conversation not found.".into()))
}
pub fn list(db: &Connection, query: &str) -> ApiResult<Vec<Summary>> {
    if query.chars().count() > 256 {
        return Err(bad("Search supports up to 256 characters."));
    }
    let query = query.trim().to_lowercase();
    let mut stmt = db.prepare(&format!(
        "{SELECT} WHERE NOT EXISTS(SELECT 1 FROM chat_trash t WHERE t.chat_id=c.id) ORDER BY c.activity DESC,c.updated_at DESC,c.rowid DESC"
    ))?;
    let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|c| {
            query.is_empty()
                || c.title.to_lowercase().contains(&query)
                || c.subtitle.to_lowercase().contains(&query)
        })
        .collect())
}
pub fn create(db: &Connection, id: &str, title: &str, now: u64) -> ApiResult<Summary> {
    let tx = db.unchecked_transaction()?;
    tx.execute("INSERT INTO chats(id,title,created_at,updated_at,activity) VALUES(?1,?2,?3,?3,(SELECT COALESCE(MAX(activity),0)+1 FROM chats))",params![id,title,now])?;
    crate::action_history::invalidate_redo(&tx)?;
    tx.commit()?;
    summary(db, id)
}
pub fn touch(tx: &Transaction<'_>, id: &str, now: u64, bump_version: bool) -> rusqlite::Result<()> {
    if bump_version {
        crate::action_history::invalidate_redo(tx)?;
    }
    tx.execute("UPDATE chats SET updated_at=?2,activity=(SELECT COALESCE(MAX(activity),0)+1 FROM chats),version=version+?3 WHERE id=?1",params![id,now,bump_version as i64])?;
    Ok(())
}
pub fn check_version(db: &Connection, id: &str, version: i64) -> ApiResult<()> {
    require_active(db, id)?;
    check_version_any(db, id, version)
}
pub fn require_active(db: &Connection, id: &str) -> ApiResult<()> {
    let active:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM chat_trash WHERE chat_id=?1))",[id],|r|r.get(0))?;
    if active {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            "Conversation is missing or in Trash. Restore it before editing.".into(),
        ))
    }
}
fn check_version_any(db: &Connection, id: &str, version: i64) -> ApiResult<()> {
    let actual: Option<i64> = db
        .query_row("SELECT version FROM chats WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    match actual {
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            "Conversation not found.".into(),
        )),
        Some(v) if v != version => Err(ApiError(
            StatusCode::CONFLICT,
            "This conversation changed. Reload it before changing this conversation.".into(),
        )),
        _ => Ok(()),
    }
}
pub fn rename(
    db: &mut Connection,
    id: &str,
    title: &str,
    version: i64,
    now: u64,
) -> ApiResult<Summary> {
    crate::action_history::rename_metadata(db, id, title, version, now)?;
    summary(db, id)
}

pub fn list_trash(db: &Connection) -> ApiResult<Vec<Value>> {
    let mut query=db.prepare(&format!("{SELECT} WHERE EXISTS(SELECT 1 FROM chat_trash t WHERE t.chat_id=c.id) ORDER BY c.updated_at DESC,c.id"))?;
    let rows = query.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|item| {
            let trashed_at: i64 = db.query_row(
                "SELECT trashed_at FROM chat_trash WHERE chat_id=?1",
                [&item.id],
                |r| r.get(0),
            )?;
            let mut value = serde_json::to_value(item)
                .map_err(|_| bad("Conversation summary encoding failed."))?;
            value["trashed_at"] = json!(trashed_at);
            Ok(value)
        })
        .collect()
}
#[derive(Clone, Copy, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactPolicy {
    Keep,
    Delete,
}
pub fn delete(
    db: &mut Connection,
    id: &str,
    version: i64,
    policy: ArtifactPolicy,
) -> ApiResult<Value> {
    let tx = db.transaction()?;
    check_version_any(&tx, id, version)?;
    crate::action_history::forget_chat(&tx, id)?;
    let artifacts: i64 = tx.query_row(
        "SELECT count(*) FROM artifacts WHERE chat_id=?1",
        [id],
        |r| r.get(0),
    )?;
    match policy {
        ArtifactPolicy::Keep => {
            tx.execute("UPDATE artifacts SET chat_id=NULL WHERE chat_id=?1", [id])?;
        }
        ArtifactPolicy::Delete => {
            tx.execute("DELETE FROM artifacts WHERE chat_id=?1", [id])?;
        }
    }
    tx.execute(
        "DELETE FROM run_configs WHERE run_id IN (SELECT run_id FROM messages WHERE chat_id=?1)",
        [id],
    )?;
    tx.execute("DELETE FROM conversation_heads WHERE chat_id=?1", [id])?;
    tx.execute("DELETE FROM messages WHERE chat_id=?1", [id])?;
    crate::drafts::clear_for_deleted_chat(&tx, id)?;
    tx.execute("DELETE FROM chats WHERE id=?1", [id])?;
    tx.commit()?;
    Ok(
        json!({"ok":true,"id":id,"artifacts_kept":if policy==ArtifactPolicy::Keep{artifacts}else{0},"artifacts_deleted":if policy==ArtifactPolicy::Delete{artifacts}else{0}}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_preview_and_search_are_literal() {
        assert_eq!(preview("  Привет\n  мир  🦀", 9), "Привет ми…");
    }
}
