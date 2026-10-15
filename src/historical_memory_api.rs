//! Dormant original episodes/lessons. Archive payloads are immutable; only an
//! explicitly requested local Trash overlay and its receipt may be written.
use crate::historical_memory_contract as contract;
use crate::{ApiError, ApiResult, App};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use contract::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

const OVERLAY_PREFIX: &str = "historical-memory.overlay.v1.";
const RECEIPT_PREFIX: &str = "historical-memory.receipt.v1.";
const MAX_STATE_BYTES: usize = 4096;
const MAX_OVERLAYS: i64 = 10_000;
const MAX_RECEIPTS: i64 = 20_000;
fn error(status: StatusCode, message: &'static str) -> ApiError {
    ApiError(status, message.into())
}
fn invalid(message: &'static str) -> ApiError {
    error(StatusCode::BAD_REQUEST, message)
}
fn corrupt() -> ApiError {
    error(
        StatusCode::CONFLICT,
        "Historical memory metadata is malformed or changed. No archive or overlay was reset.",
    )
}
fn hash(bytes: &[u8]) -> String {
    smith_media::sha256(bytes)
}
fn json<T: Serialize>(value: &T) -> ApiResult<String> {
    serde_json::to_string(value).map_err(|_| corrupt())
}
fn parse<T: serde::de::DeserializeOwned>(value: &str) -> ApiResult<T> {
    serde_json::from_str(value).map_err(|_| corrupt())
}
fn import_id(db: &Connection) -> ApiResult<String> {
    let mut query = db.prepare("SELECT id,length(CAST(report_json AS BLOB)),CASE WHEN length(CAST(report_json AS BLOB))<=65536 THEN report_json END FROM legacy_imports LIMIT 2")?;
    let mut rows = query.query([])?;
    let row = rows.next()?.ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            "This workspace has no original import archive.",
        )
    })?;
    let id: String = row.get(0)?;
    let size: i64 = row.get(1)?;
    let value: Option<String> = row.get(2)?;
    if size > 65536 || !identifier(&id) || rows.next()?.is_some() {
        return Err(corrupt());
    }
    let report: Value = parse(&value.ok_or_else(corrupt)?)?;
    if report["import_id"].as_str() != Some(&id) || report["source_schema"] != 25 {
        return Err(corrupt());
    }
    Ok(id)
}
fn unhex(value: &str) -> ApiResult<Vec<u8>> {
    if value.len() % 2 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(corrupt());
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).map_err(|_| corrupt()))
        .collect()
}
fn fields(row: &ExactRow) -> ApiResult<BTreeMap<&str, &ExactValue>> {
    if row.columns.len() != row.values.len() || row.columns.len() > 512 {
        return Err(corrupt());
    }
    let mut result = BTreeMap::new();
    for (name, value) in row.columns.iter().zip(&row.values) {
        if result.insert(name.as_str(), value).is_some() {
            return Err(corrupt());
        }
        match value {
            ExactValue::Integer(v) => {
                v.parse::<i64>().map_err(|_| corrupt())?;
            }
            ExactValue::RealBits(v) => {
                if unhex(v)?.len() != 8 {
                    return Err(corrupt());
                }
            }
            ExactValue::Blob(v) | ExactValue::TextBytes(v) => {
                unhex(v)?;
            }
            _ => {}
        }
    }
    Ok(result)
}
fn state_key(source: &HistoricalSource) -> ApiResult<String> {
    // Payload digest is checked in the stored value, rather than changing the
    // key and silently creating an empty overlay if archive bytes are replaced.
    Ok(format!(
        "{OVERLAY_PREFIX}{}",
        hash(json(&(&source.import_id, &source.table, source.row_number))?.as_bytes())
    ))
}
fn stored(db: &Connection, key: &str) -> ApiResult<Option<String>> {
    let row:Option<(i64,Option<String>)> = db.query_row("SELECT length(CAST(value AS BLOB)),CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB))<=4096 THEN value END FROM settings WHERE key=?1",[key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    match row {
        None => Ok(None),
        Some((n, Some(v))) if n <= MAX_STATE_BYTES as i64 => Ok(Some(v)),
        _ => Err(corrupt()),
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayState {
    format: u32,
    source: HistoricalSource,
    overlay: HistoricalOverlay,
}
fn overlay(db: &Connection, source: &HistoricalSource) -> ApiResult<HistoricalOverlay> {
    let Some(raw) = stored(db, &state_key(source)?)? else {
        return Ok(HistoricalOverlay::default());
    };
    let state: OverlayState = parse(&raw)?;
    if state.format != 1
        || state.source != *source
        || state.overlay.revision == 0
        || state.overlay.revision > i64::MAX as u64
    {
        return Err(corrupt());
    }
    Ok(state.overlay)
}
fn project(
    db: &Connection,
    import: &str,
    kind: HistoricalKind,
    number: i64,
    raw: &str,
) -> ApiResult<HistoricalDetail> {
    if raw.len() > MAX_ROW_BYTES || number < 0 {
        return Err(error(StatusCode::PAYLOAD_TOO_LARGE,"This original memory exceeds the bounded historical view. Exact bytes remain in the encrypted archive."));
    }
    let original: ExactRow = parse(raw)?;
    let map = fields(&original)?;
    let get = |name: &str| map.get(name).map(|v| (*v).clone());
    let known: &[&str] = match kind {
        HistoricalKind::Episode => &[
            "rowid",
            "id",
            "agent_id",
            "session_id",
            "summary",
            "salience",
            "pinned",
            "embedding",
            "created_at",
            "valid_to",
        ],
        HistoricalKind::Lesson => &[
            "rowid",
            "id",
            "agent_id",
            "source_session_id",
            "polarity",
            "trigger",
            "root_cause",
            "corrected_behavior",
            "scope",
            "usage_count",
            "success_count",
            "embedding",
            "created_at",
            "valid_to",
        ],
    };
    let mut warnings = Vec::new();
    for key in known.iter().filter(|v| **v != "rowid") {
        if !map.contains_key(*key) {
            warnings.push(format!("Missing original column: {key}"));
        }
    }
    let data = match kind {
        HistoricalKind::Episode => HistoricalData::Episode {
            agent_id: get("agent_id"),
            session_id: get("session_id"),
            summary: get("summary"),
            salience: get("salience"),
            original_pinned: get("pinned"),
            created_at: get("created_at"),
            valid_to: get("valid_to"),
        },
        HistoricalKind::Lesson => HistoricalData::Lesson {
            agent_id: get("agent_id"),
            source_session_id: get("source_session_id"),
            polarity: get("polarity"),
            trigger: get("trigger"),
            root_cause: get("root_cause"),
            corrected_behavior: get("corrected_behavior"),
            scope: get("scope"),
            usage_count: get("usage_count"),
            success_count: get("success_count"),
            created_at: get("created_at"),
            valid_to: get("valid_to"),
        },
    };
    if data.summary().is_none() {
        warnings.push("Original summary/trigger is not UTF-8 TEXT; use exact details.".into());
    }
    if let Some(value) = map.get("polarity") {
        if !matches!(value,ExactValue::Text(v) if v=="guiding" || v=="cautionary") {
            warnings
                .push("Unknown original lesson polarity was preserved without coercion.".into());
        }
    }
    if let Some(value) = map.get("pinned") {
        if !matches!(value,ExactValue::Integer(v) if v=="0" || v=="1") {
            warnings.push("Nonstandard original pin value was preserved.".into());
        }
    }
    let mut embedding = EmbeddingInfo {
        stored_kind: "missing".into(),
        byte_len: None,
        sha256: None,
        source_encoding: None,
        dimensions: None,
        model_provenance: "unknown".into(),
        reuse_allowed: false,
    };
    match map.get("embedding") {
        None => {}
        Some(ExactValue::Null) => embedding.stored_kind = "null".into(),
        Some(ExactValue::Blob(value)) => {
            let bytes = unhex(value)?;
            embedding.stored_kind = "blob".into();
            embedding.byte_len = Some(bytes.len() as u64);
            embedding.sha256 = Some(hash(&bytes));
            embedding.source_encoding = Some("little_endian_f32".into());
            if !bytes.is_empty() && bytes.len() % 4 == 0 {
                embedding.dimensions = Some((bytes.len() / 4) as u64);
            } else {
                warnings.push(
                    "Empty or malformed embedding bytes retained; no vector was activated.".into(),
                );
            }
        }
        Some(_) => {
            embedding.stored_kind = "other".into();
            warnings.push("Non-BLOB embedding value retained in exact details.".into());
        }
    }
    let source = HistoricalSource {
        import_id: import.into(),
        source_schema: 25,
        table: kind.table().into(),
        row_number: number,
        payload_sha256: hash(raw.as_bytes()),
    };
    let record = HistoricalRecord {
        overlay: overlay(db, &source)?,
        source,
        original_id: get("id"),
        data,
        embedding,
        unknown_columns: original
            .columns
            .iter()
            .filter(|n| !known.contains(&n.as_str()))
            .cloned()
            .collect(),
        warnings,
        runtime_activation: "dormant".into(),
    };
    Ok(HistoricalDetail { record, original })
}
fn read(db: &Connection, kind: HistoricalKind, number: i64) -> ApiResult<HistoricalDetail> {
    let import = import_id(db)?;
    let row:Option<(i64,Option<String>)>=db.query_row("SELECT length(CAST(payload_json AS BLOB)),CASE WHEN length(CAST(payload_json AS BLOB))<=?3 THEN payload_json END FROM legacy_records WHERE table_name=?1 AND row_number=?2",params![kind.table(),number,MAX_ROW_BYTES as i64],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let (n, raw) = row.ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            "Original memory row is absent from this archive.",
        )
    })?;
    if n > MAX_ROW_BYTES as i64 {
        return Err(error(StatusCode::PAYLOAD_TOO_LARGE,"This original memory exceeds the bounded historical view. Exact bytes remain in the encrypted archive."));
    }
    project(db, &import, kind, number, &raw.ok_or_else(corrupt)?)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    filter: String,
    cursor: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    format: u32,
    import_id: String,
    kind: HistoricalKind,
    after: i64,
    filter_sha256: String,
}
fn matches(record: &HistoricalRecord, filter: &HistoricalFilter) -> bool {
    if record.overlay.trashed != filter.trash {
        return false;
    }
    let scope = match &filter.scope {
        AgentScope::All => true,
        AgentScope::Global => matches!(record.data.agent_id(), Some(ExactValue::Null)),
        AgentScope::Agent { id } => {
            record.data.agent_id().and_then(ExactValue::text) == Some(id.as_str())
        }
    };
    scope
        && filter.session_id.as_ref().is_none_or(|id| {
            record.data.session_id().and_then(ExactValue::text) == Some(id.as_str())
        })
        && (filter.query.is_empty()
            || record.data.summary().is_some_and(|s| {
                s.to_ascii_lowercase()
                    .contains(&filter.query.to_ascii_lowercase())
            }))
}
fn list(db: &Connection, input: ListQuery) -> ApiResult<HistoricalPageReply> {
    if input.filter.len() > 4096 {
        return Err(invalid("Historical memory filter exceeds its bound."));
    }
    let filter: HistoricalFilter = parse(&input.filter)?;
    if !filter.valid() {
        return Err(invalid("Invalid historical memory scope or literal query."));
    }
    let import = import_id(db)?;
    let filter_hash = hash(json(&filter)?.as_bytes());
    let mut after = -1;
    if let Some(raw) = input.cursor {
        if raw.len() > 2048 {
            return Err(invalid("Historical cursor exceeds its bound."));
        }
        let cursor: Cursor = parse(&raw)?;
        if cursor.format != 1
            || cursor.import_id != import
            || cursor.kind != filter.kind
            || cursor.filter_sha256 != filter_hash
            || cursor.after < 0
        {
            return Err(error(
                StatusCode::CONFLICT,
                "Historical memory cursor belongs to another archive or filter.",
            ));
        }
        after = cursor.after;
    }
    let total: i64 = db.query_row(
        "SELECT count(*) FROM legacy_records WHERE table_name=?1",
        [filter.kind.table()],
        |r| r.get(0),
    )?;
    let mut query=db.prepare("SELECT row_number,length(CAST(payload_json AS BLOB)) FROM legacy_records WHERE table_name=?1 AND row_number>?2 ORDER BY row_number LIMIT ?3")?;
    let candidates = query
        .query_map(
            params![filter.kind.table(), after, MAX_SCAN_ROWS as i64],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut records = Vec::new();
    let mut retained_issues = Vec::new();
    let mut scanned = 0;
    let mut bytes = 0;
    for (number, size) in candidates {
        if records.len() + retained_issues.len() >= PAGE_SIZE {
            break;
        }
        if size < 0 || number < 0 {
            return Err(corrupt());
        }
        if size as usize <= MAX_ROW_BYTES && bytes + size as usize > MAX_PAGE_BYTES / 2 {
            break;
        }
        scanned += 1;
        after = number;
        if size as usize > MAX_ROW_BYTES {
            retained_issues.push(RetainedIssue {
                row_number: number,
                code: "oversized_original_retained".into(),
                payload_bytes: size as u64,
            });
            continue;
        }
        let raw: String = db.query_row(
            "SELECT payload_json FROM legacy_records WHERE table_name=?1 AND row_number=?2",
            params![filter.kind.table(), number],
            |r| r.get(0),
        )?;
        bytes += raw.len();
        // A malformed archive is an explicit failure, never a missing record or
        // an excuse to reset the overlay. Exact source bytes remain untouched.
        let record = project(db, &import, filter.kind, number, &raw)?.record;
        if matches(&record, &filter) {
            records.push(record);
        }
    }
    let more: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM legacy_records WHERE table_name=?1 AND row_number>?2)",
        params![filter.kind.table(), after],
        |r| r.get(0),
    )?;
    let next_cursor = if more {
        if scanned == 0 {
            return Err(corrupt());
        }
        Some(json(&Cursor {
            format: 1,
            import_id: import.clone(),
            kind: filter.kind,
            after,
            filter_sha256: filter_hash,
        })?)
    } else {
        None
    };
    let reply = HistoricalPageReply {
        import_id: import,
        filter,
        records,
        retained_issues,
        next_cursor,
        scanned,
        total_archived_rows: total as u64,
        runtime_activation: "dormant".into(),
    };
    if json(&reply)?.len() > MAX_PAGE_BYTES {
        return Err(error(StatusCode::PAYLOAD_TOO_LARGE,"Historical memory page exceeds its display bound; exact archive bytes remain retained."));
    }
    Ok(reply)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceipt {
    format: u32,
    request_sha256: String,
    receipt: HistoricalReceipt,
}
fn check_capacity(db: &Connection, prefix: &str, limit: i64) -> ApiResult<()> {
    let count: i64 = db.query_row(
        "SELECT count(*) FROM settings WHERE substr(key,1,length(?1))=?1",
        [prefix],
        |r| r.get(0),
    )?;
    if count >= limit {
        return Err(error(StatusCode::CONFLICT,"Historical memory metadata capacity reached. Existing archive, Trash state and receipts were retained."));
    }
    Ok(())
}
fn write(db: &Connection, key: &str, value: &impl Serialize) -> ApiResult<()> {
    let value = json(value)?;
    if value.len() > MAX_STATE_BYTES {
        return Err(corrupt());
    }
    db.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
    Ok(())
}
fn change(
    db: &mut Connection,
    kind: HistoricalKind,
    number: i64,
    input: HistoricalChange,
) -> ApiResult<HistoricalReceipt> {
    if !input.source.valid(kind)
        || input.source.row_number != number
        || !request_id(&input.request_id)
        || input.expected_revision > i64::MAX as u64
    {
        return Err(invalid("Invalid exact historical memory change."));
    }
    let request_hash = hash(json(&input)?.as_bytes());
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = read(&tx, kind, number)?.record;
    if current.source != input.source {
        return Err(error(
            StatusCode::CONFLICT,
            "Original memory identity changed. Refresh the archive view.",
        ));
    }
    let receipt_key = format!("{RECEIPT_PREFIX}{}", input.request_id);
    if let Some(raw) = stored(&tx, &receipt_key)? {
        let saved: StoredReceipt = parse(&raw)?;
        if saved.format != 1
            || saved.request_sha256 != request_hash
            || saved.receipt.source != input.source
            || saved.receipt.request_id != input.request_id
            || saved.receipt.action != input.action
            || saved.receipt.overlay.revision > current.overlay.revision
            || saved.receipt.overlay.trashed != (input.action == HistoricalAction::Trash)
            || saved.receipt.overlay.revision < input.expected_revision
            || saved.receipt.overlay.revision > input.expected_revision.saturating_add(1)
            || (saved.receipt.overlay.revision == 0 && saved.receipt.overlay.trashed)
        {
            return Err(corrupt());
        }
        return Ok(saved.receipt);
    }
    if current.overlay.revision != input.expected_revision {
        return Err(error(
            StatusCode::CONFLICT,
            "Historical memory Trash changed. Refresh before another action.",
        ));
    }
    check_capacity(&tx, RECEIPT_PREFIX, MAX_RECEIPTS)?;
    let trashed = input.action == HistoricalAction::Trash;
    let mut next = current.overlay;
    if next.trashed != trashed {
        next.revision = next
            .revision
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(corrupt)?;
        next.trashed = trashed;
        let key = state_key(&input.source)?;
        if stored(&tx, &key)?.is_none() {
            check_capacity(&tx, OVERLAY_PREFIX, MAX_OVERLAYS)?;
        }
        write(
            &tx,
            &key,
            &OverlayState {
                format: 1,
                source: input.source.clone(),
                overlay: next.clone(),
            },
        )?;
    }
    let receipt = HistoricalReceipt {
        source: input.source,
        request_id: input.request_id,
        action: input.action,
        overlay: next,
    };
    write(
        &tx,
        &receipt_key,
        &StoredReceipt {
            format: 1,
            request_sha256: request_hash,
            receipt: receipt.clone(),
        },
    )?;
    tx.commit()?;
    Ok(receipt)
}
async fn list_route(
    State(app): State<Arc<App>>,
    Query(input): Query<ListQuery>,
) -> ApiResult<Json<HistoricalPageReply>> {
    Ok(Json(list(&app.db.lock().unwrap(), input)?))
}
async fn detail_route(
    State(app): State<Arc<App>>,
    Path((kind, number)): Path<(HistoricalKind, i64)>,
) -> ApiResult<Json<HistoricalDetail>> {
    Ok(Json(read(&app.db.lock().unwrap(), kind, number)?))
}
async fn change_route(
    State(app): State<Arc<App>>,
    Path((kind, number)): Path<(HistoricalKind, i64)>,
    Json(input): Json<HistoricalChange>,
) -> ApiResult<Json<HistoricalReceipt>> {
    Ok(Json(change(
        &mut app.db.lock().unwrap(),
        kind,
        number,
        input,
    )?))
}
pub(super) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/legacy-import/memory", get(list_route))
        .route(
            "/api/legacy-import/memory/{kind}/{number}",
            get(detail_route).patch(change_route),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        smith_profile_storage::initialize_schema(&db).unwrap();
        db.execute(
            "INSERT INTO legacy_imports VALUES('synthetic-import',?1,0)",
            [json!({"import_id":"synthetic-import","source_schema":25}).to_string()],
        )
        .unwrap();
        db
    }
    fn raw(kind: HistoricalKind, agent: Option<&str>, summary: &str) -> String {
        let mut fields = vec![
            ("rowid", ExactValue::Integer("-51".into())),
            ("id", ExactValue::Text("opaque-original-id".into())),
            (
                "agent_id",
                agent
                    .map(|v| ExactValue::Text(v.into()))
                    .unwrap_or(ExactValue::Null),
            ),
            (
                "created_at",
                ExactValue::Text("2025-01-02T03:04:05.123+03:00".into()),
            ),
            ("valid_to", ExactValue::Text("2099-01-01T00:00:00Z".into())),
            ("embedding", ExactValue::Blob("0000803f00000080".into())),
            ("unknown_future", ExactValue::TextBytes("ff00".into())),
        ];
        match kind {
            HistoricalKind::Episode => fields.extend([
                ("session_id", ExactValue::Text("original-session".into())),
                ("summary", ExactValue::Text(summary.into())),
                ("salience", ExactValue::RealBits("000000000000e03f".into())),
                ("pinned", ExactValue::Integer("1".into())),
            ]),
            HistoricalKind::Lesson => fields.extend([
                (
                    "source_session_id",
                    ExactValue::Text("original-session".into()),
                ),
                ("trigger", ExactValue::Text(summary.into())),
                ("polarity", ExactValue::Text("unrecognized-polarity".into())),
                ("root_cause", ExactValue::Text("Original diagnosis".into())),
                (
                    "corrected_behavior",
                    ExactValue::Text("Historical instruction is dormant".into()),
                ),
                (
                    "scope",
                    ExactValue::Text("free text; not an authority grant".into()),
                ),
                ("usage_count", ExactValue::Integer(i64::MAX.to_string())),
                ("success_count", ExactValue::Integer("7".into())),
            ]),
        }
        serde_json::to_string(&ExactRow {
            columns: fields.iter().map(|(k, _)| k.to_string()).collect(),
            values: fields.into_iter().map(|(_, v)| v).collect(),
        })
        .unwrap()
    }
    fn insert(
        db: &Connection,
        kind: HistoricalKind,
        n: i64,
        agent: Option<&str>,
        summary: &str,
    ) -> String {
        let raw = raw(kind, agent, summary);
        db.execute(
            "INSERT INTO legacy_records VALUES(?1,?2,'opaque-original-id',?3)",
            params![kind.table(), n, raw],
        )
        .unwrap();
        raw
    }
    fn page(
        db: &Connection,
        filter: HistoricalFilter,
        cursor: Option<String>,
    ) -> HistoricalPageReply {
        list(
            db,
            ListQuery {
                filter: json(&filter).unwrap(),
                cursor,
            },
        )
        .unwrap()
    }
    fn request(record: &HistoricalRecord, action: HistoricalAction, n: u64) -> HistoricalChange {
        HistoricalChange {
            source: record.source.clone(),
            expected_revision: record.overlay.revision,
            action,
            request_id: format!("00000000-0000-4000-8000-{n:012x}"),
        }
    }
    #[test]
    fn episodes_and_lessons_preserve_exact_provenance_values_without_active_projection() {
        let db = fixture();
        for kind in [HistoricalKind::Episode, HistoricalKind::Lesson] {
            let raw = insert(
                &db,
                kind,
                0,
                Some("original-agent"),
                "Synthetic historical text",
            );
            let detail = read(&db, kind, 0).unwrap();
            assert_eq!(detail.original, parse::<ExactRow>(&raw).unwrap());
            assert_eq!(detail.record.source.payload_sha256, hash(raw.as_bytes()));
            assert_eq!(detail.record.embedding.dimensions, Some(2));
            assert_eq!(
                detail.record.embedding.sha256,
                Some(hash(&[0, 0, 128, 63, 0, 0, 0, 128]))
            );
            assert!(!detail.record.embedding.reuse_allowed);
            assert_eq!(detail.record.embedding.model_provenance, "unknown");
            assert_eq!(detail.record.unknown_columns, vec!["unknown_future"]);
            assert_eq!(detail.record.runtime_activation, "dormant");
            if let HistoricalData::Lesson {
                polarity,
                usage_count,
                ..
            } = detail.record.data
            {
                assert_eq!(
                    polarity,
                    Some(ExactValue::Text("unrecognized-polarity".into()))
                );
                assert_eq!(usage_count, Some(ExactValue::Integer(i64::MAX.to_string())));
            }
            assert_eq!(
                db.query_row(
                    "SELECT payload_json FROM legacy_records WHERE table_name=?1",
                    [kind.table()],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                raw
            );
        }
        for table in [
            "agent_memories",
            "agent_tasks",
            "agent_definitions",
            "settings",
        ] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
    #[test]
    fn bounded_scan_scopes_agents_sessions_and_literal_queries_without_hidden_activation() {
        let db = fixture();
        for n in 0..300 {
            insert(
                &db,
                HistoricalKind::Episode,
                n,
                Some("agent-a"),
                "Unmatched original episode",
            );
        }
        insert(
            &db,
            HistoricalKind::Episode,
            300,
            None,
            "Literal 100%_ MATCH",
        );
        insert(
            &db,
            HistoricalKind::Lesson,
            0,
            Some("agent-b"),
            "Separate lesson",
        );
        let filter = HistoricalFilter {
            scope: AgentScope::Global,
            query: "100%_ match".into(),
            session_id: Some("original-session".into()),
            ..Default::default()
        };
        let first = page(&db, filter.clone(), None);
        assert_eq!(first.scanned, 256);
        assert!(first.records.is_empty());
        let next = page(&db, filter.clone(), first.next_cursor.clone());
        assert_eq!(next.records.len(), 1);
        assert!(next.next_cursor.is_none());
        assert_eq!(next.records[0].source.row_number, 300);
        let other = HistoricalFilter {
            scope: AgentScope::Agent {
                id: "agent-b".into(),
            },
            ..filter.clone()
        };
        assert!(list(
            &db,
            ListQuery {
                filter: json(&other).unwrap(),
                cursor: first.next_cursor
            }
        )
        .is_err());
        let agent_page = page(
            &db,
            HistoricalFilter {
                scope: AgentScope::Agent {
                    id: "agent-a".into(),
                },
                ..Default::default()
            },
            None,
        );
        assert_eq!(agent_page.records.len(), 32);
        assert_eq!(agent_page.total_archived_rows, 301);
        assert_eq!(
            page(
                &db,
                HistoricalFilter {
                    kind: HistoricalKind::Lesson,
                    ..Default::default()
                },
                None
            )
            .records
            .len(),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM settings", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn oversized_or_invalid_originals_are_retained_explicitly_and_never_coerced() {
        let db = fixture();
        let original = insert(
            &db,
            HistoricalKind::Episode,
            0,
            None,
            &"x".repeat(MAX_ROW_BYTES),
        );
        let reply = page(&db, HistoricalFilter::default(), None);
        assert!(reply.records.is_empty());
        assert_eq!(reply.retained_issues.len(), 1);
        assert_eq!(
            read(&db, HistoricalKind::Episode, 0).err().unwrap().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            db.query_row("SELECT payload_json FROM legacy_records", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            original
        );
        db.execute("UPDATE legacy_records SET payload_json='malformed'", [])
            .unwrap();
        assert!(list(
            &db,
            ListQuery {
                filter: json(&HistoricalFilter::default()).unwrap(),
                cursor: None
            }
        )
        .is_err());
        assert_eq!(
            db.query_row("SELECT payload_json FROM legacy_records", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "malformed"
        );
    }
    #[test]
    fn trash_restore_cas_and_idempotency_keep_archive_immutable_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.sqlite");
        let mut db = fixture();
        insert(&db, HistoricalKind::Episode, 0, None, "Retained original");
        // Copy this synthetic fixture only; reopening verifies durable receipts.
        db.execute("VACUUM INTO ?1", [path.to_str().unwrap()])
            .unwrap();
        drop(db);
        db = Connection::open(&path).unwrap();
        let before = read(&db, HistoricalKind::Episode, 0).unwrap();
        let input = request(&before.record, HistoricalAction::Trash, 1);
        let receipt = change(&mut db, HistoricalKind::Episode, 0, input.clone()).unwrap();
        assert_eq!(
            receipt.overlay,
            HistoricalOverlay {
                revision: 1,
                trashed: true
            }
        );
        assert!(page(&db, HistoricalFilter::default(), None)
            .records
            .is_empty());
        assert_eq!(
            page(
                &db,
                HistoricalFilter {
                    trash: true,
                    ..Default::default()
                },
                None
            )
            .records
            .len(),
            1
        );
        assert_eq!(
            change(&mut db, HistoricalKind::Episode, 0, input.clone()).unwrap(),
            receipt
        );
        assert!(change(
            &mut db,
            HistoricalKind::Episode,
            0,
            request(&before.record, HistoricalAction::Restore, 2)
        )
        .is_err());
        drop(db);
        let mut db = Connection::open(&path).unwrap();
        assert_eq!(
            change(&mut db, HistoricalKind::Episode, 0, input.clone()).unwrap(),
            receipt
        );
        let current = read(&db, HistoricalKind::Episode, 0).unwrap();
        let restored = change(
            &mut db,
            HistoricalKind::Episode,
            0,
            request(&current.record, HistoricalAction::Restore, 3),
        )
        .unwrap();
        assert_eq!(
            restored.overlay,
            HistoricalOverlay {
                revision: 2,
                trashed: false
            }
        );
        assert_eq!(
            change(&mut db, HistoricalKind::Episode, 0, input).unwrap(),
            receipt
        );
        assert_eq!(
            read(&db, HistoricalKind::Episode, 0).unwrap().original,
            before.original
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM agent_memories", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn changed_source_corrupted_overlay_and_corrupted_receipt_fail_without_reset() {
        let mut db = fixture();
        insert(&db, HistoricalKind::Episode, 0, None, "Original");
        let current = read(&db, HistoricalKind::Episode, 0).unwrap().record;
        let input = request(&current, HistoricalAction::Trash, 1);
        change(&mut db, HistoricalKind::Episode, 0, input.clone()).unwrap();
        let key = format!("{RECEIPT_PREFIX}{}", input.request_id);
        let original = stored(&db, &key).unwrap().unwrap();
        for bad_overlay in [
            HistoricalOverlay {
                revision: 0,
                trashed: false,
            },
            HistoricalOverlay {
                revision: 1,
                trashed: false,
            },
            HistoricalOverlay {
                revision: 99,
                trashed: true,
            },
        ] {
            let mut saved: StoredReceipt = parse(&original).unwrap();
            saved.receipt.overlay = bad_overlay;
            write(&db, &key, &saved).unwrap();
            assert!(change(&mut db, HistoricalKind::Episode, 0, input.clone()).is_err());
        }
        db.execute(
            "UPDATE settings SET value=?2 WHERE key=?1",
            params![key, original],
        )
        .unwrap();
        db.execute(
            "UPDATE legacy_records SET payload_json=?1",
            [raw(HistoricalKind::Episode, None, "Replaced archive")],
        )
        .unwrap();
        assert!(read(&db, HistoricalKind::Episode, 0).is_err());
        assert!(change(&mut db, HistoricalKind::Episode, 0, input).is_err());
        assert!(stored(&db, &state_key(&current.source).unwrap())
            .unwrap()
            .is_some());
    }
    #[test]
    fn receipt_failure_rolls_back_overlay_and_bounded_state_never_evicts_receipts() {
        let mut db = fixture();
        insert(&db, HistoricalKind::Episode, 0, None, "Original");
        let current = read(&db, HistoricalKind::Episode, 0).unwrap().record;
        db.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON settings WHEN instr(new.key,'historical-memory.receipt.v1.')=1 BEGIN SELECT RAISE(ABORT,'Synthetic receipt failure'); END;").unwrap();
        assert!(change(
            &mut db,
            HistoricalKind::Episode,
            0,
            request(&current, HistoricalAction::Trash, 1)
        )
        .is_err());
        assert_eq!(
            read(&db, HistoricalKind::Episode, 0)
                .unwrap()
                .record
                .overlay,
            HistoricalOverlay::default()
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM settings", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        db.execute_batch("DROP TRIGGER fail_receipt").unwrap();
        db.execute(
            "INSERT INTO settings VALUES(?1,?2)",
            params![
                state_key(&current.source).unwrap(),
                "x".repeat(MAX_STATE_BYTES + 1)
            ],
        )
        .unwrap();
        assert!(read(&db, HistoricalKind::Episode, 0).is_err());
        assert_eq!(
            db.query_row("SELECT length(value) FROM settings", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            (MAX_STATE_BYTES + 1) as i64
        );
    }
}
