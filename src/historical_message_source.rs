//! Lazy, SELECT-only current-message to exact archived-source mapping.
use super::*;
use contract::{MessageSelection, SelectedMessageSource, SourceMapping};

fn stale() -> Error {
    Error("The displayed conversation changed or its original source is unavailable. Refresh before opening historical cards.")
}

pub fn resolve(db: &Connection, selection: &MessageSelection) -> Result<SelectedMessageSource> {
    if !selection.valid() || !db.is_autocommit() {
        return Err(stale());
    }
    let tx = db.unchecked_transaction().map_err(storage)?;
    let import = import_id(&tx)?;
    let current: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM chats c JOIN conversation_heads h ON h.chat_id=c.id WHERE c.id=?1 AND typeof(c.version)='integer' AND c.version=?2 AND h.leaf_id=?3 AND NOT EXISTS(SELECT 1 FROM chat_trash t WHERE t.chat_id=c.id))",
        params![selection.chat_id, selection.chat_version, selection.active_leaf],
        |r| r.get(0),
    ).map_err(storage)?;
    if !current {
        return Err(stale());
    }
    active_member(&tx, selection)?;

    // Schema7 import preserves IDs exactly and creates this membership row.
    // Never derive an original ID from a prefix, sequence or displayed row index.
    let (status, role, original_status, seq) = {
        let mut q = tx.prepare("SELECT CASE WHEN typeof(m.status)='text' AND length(CAST(m.status AS BLOB)) BETWEEN 1 AND 1024 THEN m.status END, CASE WHEN typeof(m.role)='text' AND length(CAST(m.role AS BLOB)) BETWEEN 1 AND 1024 THEN m.role END, CASE WHEN typeof(d.original_status)='text' AND length(CAST(d.original_status AS BLOB)) BETWEEN 1 AND 1024 THEN d.original_status END, d.original_seq FROM legacy_message_details d JOIN messages m ON m.id=d.message_id WHERE m.id=?1 AND m.chat_id=?2 AND m.run_id IS NULL LIMIT 2").map_err(storage)?;
        let mut rows = q
            .query(params![selection.message_id, selection.chat_id])
            .map_err(storage)?;
        let r = rows.next().map_err(storage)?.ok_or_else(stale)?;
        let value = (
            r.get::<_, Option<String>>(0)
                .map_err(storage)?
                .ok_or_else(stale)?,
            r.get::<_, Option<String>>(1)
                .map_err(storage)?
                .ok_or_else(stale)?,
            r.get::<_, Option<String>>(2)
                .map_err(storage)?
                .ok_or_else(stale)?,
            r.get::<_, i64>(3).map_err(storage)?,
        );
        if rows.next().map_err(storage)?.is_some() {
            return Err(stale());
        }
        value
    };
    if !matches!(
        status.as_str(),
        "completed" | "cancelled" | "failed" | "interrupted" | "imported_reference"
    ) {
        return Err(stale());
    }
    let number = {
        let mut q = tx.prepare("SELECT row_number FROM legacy_records WHERE table_name='messages' AND record_id=?1 LIMIT 2").map_err(storage)?;
        let mut rows = q.query([&selection.message_id]).map_err(storage)?;
        let number: i64 = rows
            .next()
            .map_err(storage)?
            .ok_or_else(stale)?
            .get(0)
            .map_err(storage)?;
        if number < 0 || rows.next().map_err(storage)?.is_some() {
            return Err(stale());
        }
        number
    };
    let message = read_row(&tx, &import, number, contract::MAX_ARCHIVE_ROW)?;
    if message.id() != Some(selection.message_id.as_str())
        || message.text("session_id") != Some(selection.chat_id.as_str())
        || message.text("role") != Some(role.as_str())
        || message.text("status") != Some(original_status.as_str())
        || message.field("seq") != Some(&contract::ExactValue::Integer(seq.to_string()))
    {
        return Err(stale());
    }
    let receipt = SelectedMessageSource {
        selection: selection.clone(),
        message_status: status,
        original_source: SourceMapping {
            source: message.source().clone(),
            original_message_id: selection.message_id.clone(),
            original_session_id: selection.chat_id.clone(),
        },
    };
    tx.commit().map_err(storage)?;
    Ok(receipt)
}

fn active_member(db: &Connection, selection: &MessageSelection) -> Result<()> {
    let mut cursor = Some(selection.active_leaf.clone());
    let mut seen = BTreeSet::new();
    let mut remaining = contract::MAX_ANCESTOR_BYTES;
    let mut child_row = None;
    let mut found = false;
    // Validate the complete selected branch even after reaching the message.
    // Missing parents, cross-chat edges and cycles never get a partial receipt.
    while let Some(id) = cursor {
        if seen.len() >= contract::MAX_ANCESTORS || id.len() > remaining || !seen.insert(id.clone())
        {
            return Err(stale());
        }
        remaining -= id.len();
        let mut q = db.prepare("SELECT m.rowid, b.parent_id IS NULL OR (typeof(b.parent_id)='text' AND length(CAST(b.parent_id AS BLOB)) BETWEEN 1 AND 1024), CASE WHEN typeof(b.parent_id)='text' AND length(CAST(b.parent_id AS BLOB)) BETWEEN 1 AND 1024 THEN b.parent_id END FROM messages m JOIN message_branches b ON b.message_id=m.id WHERE m.id=?1 AND m.chat_id=?2 LIMIT 2").map_err(storage)?;
        let mut rows = q.query(params![id, selection.chat_id]).map_err(storage)?;
        let row = rows.next().map_err(storage)?.ok_or_else(stale)?;
        let ordinal: i64 = row.get(0).map_err(storage)?;
        let valid: bool = row.get(1).map_err(storage)?;
        let parent: Option<String> = row.get(2).map_err(storage)?;
        if !valid
            || child_row.is_some_and(|child| ordinal >= child)
            || parent
                .as_ref()
                .is_some_and(|s| s.chars().any(char::is_control))
            || rows.next().map_err(storage)?.is_some()
        {
            return Err(stale());
        }
        found |= id == selection.message_id;
        child_row = Some(ordinal);
        cursor = parent;
    }
    if !found {
        return Err(stale());
    }
    Ok(())
}
