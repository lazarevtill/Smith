//! Read-only snapshot adapter for the encrypted imported archive. Not registered
//! in any router. Hold the application's profile lease across capture and reply,
//! release its DB mutex after capture, then project/decode in bounded workers.
#[path = "historical_message_projection.rs"]
pub mod projection;
pub use projection::{contract, Error, Result};
use projection::{Ancestry, ArchivedMessage, MessageCards, MessageSource, Reason, ValidatedImage};
use rusqlite::{params, Connection};
use std::collections::BTreeSet;

fn storage(_: rusqlite::Error) -> Error {
    Error("The retained original archive could not be read; no data was changed.")
}
fn corrupt() -> Error {
    Error("The retained original archive identity is invalid; no data was changed.")
}

/// Captured in one read transaction, with no public constructor or mutable source.
/// Retain only for the admitted profile session; this is not a session credential.
pub struct Snapshot {
    message: ArchivedMessage,
    ancestry: Ancestry,
}
impl Snapshot {
    #[cfg(test)]
    pub fn source(&self) -> &MessageSource {
        self.message.source()
    }
    #[cfg(test)]
    pub fn original(&self) -> &contract::ExactRow {
        self.message.original()
    }
    pub fn cards(&self) -> Result<MessageCards> {
        self.message.cards(&self.ancestry)
    }
    pub fn embedded_image(&self, expected: &MessageSource, index: usize) -> Result<ValidatedImage> {
        self.message.embedded_image(expected, index)
    }
}

/// Capture a single original row and bounded ancestor path. Every image lookup
/// must call this again with `Some(reviewed_source)` before decoding. Never reuse
/// an earlier snapshot after a profile/session or navigation change.
///
/// Requires an autocommit connection so it cannot accidentally join a caller's
/// write transaction. All SQL below is SELECT; the transaction only pins reads.
pub fn capture(
    db: &Connection,
    row_number: i64,
    expected: Option<&MessageSource>,
) -> Result<Snapshot> {
    if row_number < 0 || !db.is_autocommit() {
        return Err(Error(
            "Original message capture requires a valid row and an idle read connection.",
        ));
    }
    if expected.is_some_and(|s| !s.valid() || s.row_number != row_number) {
        return Err(corrupt());
    }
    let tx = db.unchecked_transaction().map_err(storage)?;
    let import = import_id(&tx)?;
    let message = read_row(&tx, &import, row_number, contract::MAX_ARCHIVE_ROW)?;
    if expected.is_some_and(|s| message.source() != s) {
        return Err(Error(
            "Original message source changed; refresh before opening it.",
        ));
    }
    let ancestry = if message.text("role") == Some("tool") {
        ancestry(&tx, &import, &message)?
    } else {
        Ancestry {
            nodes: Vec::new(),
            end: Ok(()),
        }
    };
    tx.commit().map_err(storage)?;
    Ok(Snapshot { message, ancestry })
}

fn import_id(db: &Connection) -> Result<String> {
    // CASE applies byte/type limits before rusqlite allocates either string.
    let mut query = db.prepare("SELECT CASE WHEN typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND 256 THEN id END, CASE WHEN typeof(report_json)='text' AND length(CAST(report_json AS BLOB)) BETWEEN 1 AND 65536 THEN report_json END FROM legacy_imports LIMIT 2").map_err(storage)?;
    let mut rows = query.query([]).map_err(storage)?;
    let row = rows
        .next()
        .map_err(storage)?
        .ok_or(Error("This workspace has no retained original import."))?;
    let id: Option<String> = row.get(0).map_err(storage)?;
    let raw: Option<String> = row.get(1).map_err(storage)?;
    let id = id.ok_or_else(corrupt)?;
    let raw = raw.ok_or_else(corrupt)?;
    if id.chars().any(char::is_control) || rows.next().map_err(storage)?.is_some() {
        return Err(corrupt());
    }
    let report: serde_json::Value = serde_json::from_str(&raw).map_err(|_| corrupt())?;
    if report["import_id"].as_str() != Some(&id) || report["source_schema"] != 25 {
        return Err(corrupt());
    }
    Ok(id)
}

fn read_row(db: &Connection, import: &str, number: i64, limit: usize) -> Result<ArchivedMessage> {
    let mut query = db.prepare("SELECT length(CAST(payload_json AS BLOB)), CASE WHEN typeof(payload_json)='text' AND length(CAST(payload_json AS BLOB))<=?2 THEN payload_json END FROM legacy_records WHERE table_name='messages' AND row_number=?1 LIMIT 2").map_err(storage)?;
    let mut rows = query
        .query(params![number, limit as i64])
        .map_err(storage)?;
    let row = rows
        .next()
        .map_err(storage)?
        .ok_or(Error("The requested original message is absent."))?;
    let size: i64 = row.get(0).map_err(storage)?;
    let raw: Option<String> = row.get(1).map_err(storage)?;
    if size < 0 || size as u64 > limit as u64 {
        return Err(Error(
            "The original message exceeds its bounded view; exact archive bytes remain retained.",
        ));
    }
    let raw = raw.ok_or_else(corrupt)?;
    if rows.next().map_err(storage)?.is_some() {
        return Err(corrupt());
    }
    ArchivedMessage::parse(import, number, &raw)
}

fn ancestry(db: &Connection, import: &str, message: &ArchivedMessage) -> Result<Ancestry> {
    let mut nodes = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(id) = message.id() {
        seen.insert(id.to_owned());
    }
    let mut next = message.parent().map(|v| v.map(str::to_owned));
    let mut remaining = contract::MAX_ANCESTOR_BYTES;
    let end = loop {
        let id = match next {
            Ok(None) => break Ok(()),
            Ok(Some(ref id)) => id.clone(),
            Err(reason) => break Err(reason),
        };
        if !seen.insert(id.clone()) {
            break Err(Reason::Cycle);
        }
        if nodes.len() >= contract::MAX_ANCESTORS {
            break Err(Reason::AncestryLimit);
        }
        // Resolve at most two identities, and inspect size before allocating raw
        // payload. Charge the complete retained row, including unknown columns.
        let mut query = db.prepare("SELECT row_number,length(CAST(payload_json AS BLOB)),typeof(payload_json) FROM legacy_records WHERE table_name='messages' AND record_id=?1 LIMIT 2").map_err(storage)?;
        let mut rows = query.query([&id]).map_err(storage)?;
        let Some(row) = rows.next().map_err(storage)? else {
            break Err(Reason::MissingAncestor);
        };
        let number: i64 = row.get(0).map_err(storage)?;
        let bytes: i64 = row.get(1).map_err(storage)?;
        let kind: String = row.get(2).map_err(storage)?;
        if rows.next().map_err(storage)?.is_some() {
            break Err(Reason::DuplicateIdentity);
        }
        if number < 0 || kind != "text" || bytes < 0 {
            break Err(Reason::InvalidIdentity);
        }
        if bytes as u64 > remaining as u64 {
            break Err(Reason::AncestryLimit);
        }
        let node = match read_row(db, import, number, remaining) {
            Ok(node) => node,
            Err(_) => break Err(Reason::InvalidIdentity),
        };
        if node.id() != Some(id.as_str()) {
            break Err(Reason::InvalidIdentity);
        }
        remaining -= node.payload_bytes();
        next = node.parent().map(|v| v.map(str::to_owned));
        nodes.push(node);
    };
    Ok(Ancestry { nodes, end })
}

#[cfg(test)]
#[path = "../tests/support/historical_message_store.rs"]
mod tests;
