//! Read-only historical import views inside the unlocked profile lease. Stored
//! tools, URLs, prompts, schedules and task transcripts never execute here.
#[path = "historical_memory_api.rs"]
mod historical_memory_api;
#[path = "historical_message_api.rs"]
mod historical_message_api;
use crate::{ApiError, ApiResult, App};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rusqlite::{params, Connection, OptionalExtension, Params};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

fn missing() -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        "Imported record is not available in this profile.".into(),
    )
}
// Bound bytes before materializing imported strings into Rust or JSON. A refused
// row remains intact in the encrypted typed evidence and original-file archive.
fn bounded<P: Params>(db: &Connection, sql: &str, args: P) -> ApiResult<()> {
    let bytes: i64 = db.query_row(sql, args, |r| r.get(0))?;
    if bytes > 2 * 1024 * 1024 {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE,
            "This historical page exceeds the 2 MiB display limit. Request fewer rows. An oversized individual record remains intact in the encrypted import; no bytes were discarded.".into()));
    }
    Ok(())
}
fn size_expr(columns: &[&str]) -> String {
    columns
        .iter()
        .map(|name| format!("coalesce(length(CAST({name} AS BLOB)),0)"))
        .collect::<Vec<_>>()
        .join("+")
}
fn bounded_page<P: Params>(
    db: &Connection,
    table: &str,
    columns: &[&str],
    suffix: &str,
    args: P,
) -> ApiResult<()> {
    bounded(
        db,
        &format!(
            "SELECT coalesce(sum(n),0) FROM (SELECT {} AS n FROM {table} {suffix})",
            size_expr(columns)
        ),
        args,
    )
}
#[derive(Default, Deserialize)]
struct Page {
    offset: Option<u64>,
    limit: Option<u64>,
}
impl Page {
    fn bounds(&self) -> ApiResult<(i64, i64)> {
        let limit = self.limit.unwrap_or(50);
        let offset = self.offset.unwrap_or(0);
        if limit == 0 || limit > 100 || offset > 1_000_000 {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "Use an imported-record page of 1–100 rows.".into(),
            ));
        }
        Ok((offset as i64, limit as i64))
    }
}
async fn summary(State(app): State<Arc<App>>) -> ApiResult<Json<Value>> {
    let db = app.db.lock().unwrap();
    let reports = db
        .prepare("SELECT report_json FROM legacy_imports ORDER BY id")?
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|r| r.map(|s| serde_json::from_str::<Value>(&s).unwrap_or(Value::Null)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let tables=db.prepare("SELECT table_name,count(*) FROM legacy_records GROUP BY table_name ORDER BY table_name")?.query_map([],|r|Ok(json!({"table":r.get::<_,String>(0)?,"rows":r.get::<_,i64>(1)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    Ok(Json(
        json!({"imports":reports,"report_scope":"at_import_creation; current rollback eligibility requires locked verification","tables":tables,"runtime_activation":"dormant","notice":"Imported tools, agents, schedules and external references are historical data. They do not grant authority or run automatically."}),
    ))
}
async fn records(
    State(app): State<Arc<App>>,
    Path(table): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<Json<Value>> {
    let (offset, limit) = page.bounds()?;
    let db = app.db.lock().unwrap();
    let total: i64 = db.query_row(
        "SELECT count(*) FROM legacy_records WHERE table_name=?1",
        [&table],
        |r| r.get(0),
    )?;
    bounded_page(
        &db,
        "legacy_records",
        &["table_name", "record_id", "payload_json"],
        "WHERE table_name=?1 ORDER BY row_number LIMIT ?2 OFFSET ?3",
        params![table, limit, offset],
    )?;
    let values=db.prepare("SELECT row_number,record_id,payload_json FROM legacy_records WHERE table_name=?1 ORDER BY row_number LIMIT ?2 OFFSET ?3")?.query_map(params![table,limit,offset],|r|{let raw:String=r.get(2)?;Ok(json!({"row_number":r.get::<_,i64>(0)?,"id":r.get::<_,Option<String>>(1)?,"record":serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null)}))})?.collect::<std::result::Result<Vec<_>,_>>()?;
    Ok(Json(
        json!({"table":table,"total":total,"offset":offset,"rows":values,"runtime_activation":"dormant"}),
    ))
}
async fn messages(State(app): State<Arc<App>>, Query(page): Query<Page>) -> ApiResult<Json<Value>> {
    let (offset, limit) = page.bounds()?;
    let db = app.db.lock().unwrap();
    bounded_page(
        &db,
        "legacy_message_details d LEFT JOIN messages m ON m.id=d.message_id",
        &[
            "d.message_id",
            "m.chat_id",
            "m.role",
            "d.original_status",
            "d.projection_warning",
        ],
        "ORDER BY d.message_id LIMIT ?1 OFFSET ?2",
        params![limit, offset],
    )?;
    let values=db.prepare("SELECT d.message_id,m.chat_id,m.role,d.original_status,d.original_seq,d.projection_warning FROM legacy_message_details d LEFT JOIN messages m ON m.id=d.message_id ORDER BY d.message_id LIMIT ?1 OFFSET ?2")?.query_map(params![limit,offset],|r|Ok(json!({"id":r.get::<_,String>(0)?,"chat_id":r.get::<_,Option<String>>(1)?,"role":r.get::<_,Option<String>>(2)?,"original_status":r.get::<_,String>(3)?,"original_seq":r.get::<_,i64>(4)?,"projection_warning":r.get::<_,Option<String>>(5)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let total: i64 = db.query_row("SELECT count(*) FROM legacy_message_details", [], |r| {
        r.get(0)
    })?;
    Ok(Json(
        json!({"total":total,"offset":offset,"messages":values}),
    ))
}
async fn message(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let db = app.db.lock().unwrap();
    bounded_page(
        &db,
        "legacy_message_details",
        &[
            "message_id",
            "original_status",
            "original_created_at",
            "reasoning",
            "tool_calls",
            "tool_call_id",
            "images",
            "projection_warning",
        ],
        "WHERE message_id=?1",
        [&id],
    )?;
    let value=db.query_row("SELECT message_id,original_status,original_seq,original_created_at,reasoning,tool_calls,tool_call_id,images,projection_warning FROM legacy_message_details WHERE message_id=?1",[id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"original_status":r.get::<_,String>(1)?,"original_seq":r.get::<_,i64>(2)?,"original_created_at":r.get::<_,String>(3)?,"reasoning":r.get::<_,Option<String>>(4)?,"tool_calls_json":r.get::<_,String>(5)?,"tool_call_id":r.get::<_,Option<String>>(6)?,"images_json":r.get::<_,String>(7)?,"projection_warning":r.get::<_,Option<String>>(8)?,"runtime_activation":"dormant"}))).optional()?.ok_or_else(missing)?;
    Ok(Json(value))
}
async fn tasks(State(app): State<Arc<App>>, Query(page): Query<Page>) -> ApiResult<Json<Value>> {
    let (offset, limit) = page.bounds()?;
    let db = app.db.lock().unwrap();
    bounded_page(
        &db,
        "legacy_tasks",
        &[
            "id",
            "agent_id",
            "status",
            "trigger_kind",
            "input",
            "output",
            "error",
            "transcript",
            "original_created_at",
            "original_updated_at",
        ],
        "ORDER BY id LIMIT ?1 OFFSET ?2",
        params![limit, offset],
    )?;
    let values=db.prepare("SELECT id,agent_id,status,trigger_kind,input,output,error,transcript,original_created_at,original_updated_at FROM legacy_tasks ORDER BY id LIMIT ?1 OFFSET ?2")?.query_map(params![limit,offset],|r|Ok(json!({"id":r.get::<_,String>(0)?,"agent_id":r.get::<_,Option<String>>(1)?,"original_status":r.get::<_,String>(2)?,"trigger_kind":r.get::<_,String>(3)?,"input":r.get::<_,String>(4)?,"output":r.get::<_,Option<String>>(5)?,"error":r.get::<_,Option<String>>(6)?,"transcript_json":r.get::<_,String>(7)?,"original_created_at":r.get::<_,String>(8)?,"original_updated_at":r.get::<_,String>(9)?,"runtime_activation":"dormant","resumable":false})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let total: i64 = db.query_row("SELECT count(*) FROM legacy_tasks", [], |r| r.get(0))?;
    Ok(Json(json!({"total":total,"offset":offset,"tasks":values})))
}
async fn artifacts(
    State(app): State<Arc<App>>,
    Query(page): Query<Page>,
) -> ApiResult<Json<Value>> {
    let (offset, limit) = page.bounds()?;
    let db = app.db.lock().unwrap();
    bounded_page(
        &db,
        "legacy_artifacts",
        &[
            "id",
            "session_id",
            "message_id",
            "store_id",
            "title",
            "mime",
            "original_relpath",
            "original_created_at",
            "content_kind",
            "sha256",
        ],
        "ORDER BY id LIMIT ?1 OFFSET ?2",
        params![limit, offset],
    )?;
    let values=db.prepare("SELECT id,session_id,message_id,store_id,title,mime,original_relpath,original_created_at,content_kind,length(bytes),sha256 FROM legacy_artifacts ORDER BY id LIMIT ?1 OFFSET ?2")?.query_map(params![limit,offset],|r|Ok(json!({"id":r.get::<_,String>(0)?,"session_id":r.get::<_,Option<String>>(1)?,"message_id":r.get::<_,Option<String>>(2)?,"store_id":r.get::<_,String>(3)?,"title":r.get::<_,String>(4)?,"mime":r.get::<_,String>(5)?,"original_relpath":r.get::<_,String>(6)?,"original_created_at":r.get::<_,String>(7)?,"content_kind":r.get::<_,String>(8)?,"byte_len":r.get::<_,Option<i64>>(9)?,"sha256":r.get::<_,Option<String>>(10)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let total: i64 = db.query_row("SELECT count(*) FROM legacy_artifacts", [], |r| r.get(0))?;
    Ok(Json(
        json!({"total":total,"offset":offset,"artifacts":values,"external_files_opened":false}),
    ))
}
async fn artifact_bytes(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let db = app.db.lock().unwrap();
    let size: Option<i64> = db
        .query_row(
            "SELECT length(bytes) FROM legacy_artifacts WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(missing)?;
    if size.is_some_and(|n| n > 16 * 1024 * 1024) {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE,"This original exceeds the 16 MiB browser download limit. Exact bytes remain in the encrypted import.".into()));
    }
    let (bytes, expected): (Option<Vec<u8>>, Option<String>) = db
        .query_row(
            "SELECT bytes,sha256 FROM legacy_artifacts WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(missing)?;
    let bytes = bytes.ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            "This is an external reference. Its external file was not opened or imported.".into(),
        )
    })?;
    if expected.as_deref() != Some(&smith_media::sha256(&bytes)) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Imported artifact integrity verification failed.".into(),
        ));
    }
    // Always download inert bytes; no imported filename/header/MIME can execute
    // HTML or inject headers. Descriptive original metadata is separate JSON.
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"imported-artifact.bin\"",
            ),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .merge(historical_memory_api::routes())
        .merge(historical_message_api::routes())
        .route("/api/legacy-import", get(summary))
        .route("/api/legacy-import/records/{table}", get(records))
        .route("/api/legacy-import/messages", get(messages))
        .route("/api/legacy-import/messages/{id}", get(message))
        .route("/api/legacy-import/tasks", get(tasks))
        .route("/api/legacy-import/artifacts", get(artifacts))
        .route(
            "/api/legacy-import/artifacts/{id}/bytes",
            get(artifact_bytes),
        )
}
