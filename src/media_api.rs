//! Validated document uploads/exports in the active encrypted profile connection.
//! The profile host owns unlocking, request leases and connection closure. No
//! MediaStore/filesystem staging, external fetch, execution or vision inference.
use crate::{chat_management, App};
use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smith_media::{
    AttachmentRequest, CellValue, Content, ErrorKind, ExportFormat, Limits, Manifest, Table,
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};
use tokio::sync::Semaphore;

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROFILE_BYTES: i64 = 256 * 1024 * 1024;
const MAX_OBJECTS: i64 = 1024;
const MAX_MANIFEST: usize = 512 * 1024;
const MAX_CONTEXT_BYTES: usize = 100_000;
static PARSERS: Semaphore = Semaphore::const_new(2);
static UPLOADS: Semaphore = Semaphore::const_new(2);

#[derive(Debug)]
pub struct MediaError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: &'static str,
}
pub type MediaResult<T> = Result<T, MediaError>;
impl From<MediaError> for crate::ApiError {
    fn from(error: MediaError) -> Self {
        Self(error.status, error.message.into())
    }
}
impl IntoResponse for MediaError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error":self.message,"code":self.code})),
        )
            .into_response()
    }
}
impl From<rusqlite::Error> for MediaError {
    fn from(_: rusqlite::Error) -> Self {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "media_storage_failed",
            "Document storage failed. Existing files have not been deleted.",
        )
    }
}
impl From<smith_media::Error> for MediaError {
    fn from(e: smith_media::Error) -> Self {
        let (status, code, message) = match e.kind {
            ErrorKind::UnsupportedFormat => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "media_unsupported",
                "This file format or document feature is not supported.",
            ),
            ErrorKind::MimeMismatch => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "media_type_mismatch",
                "The file extension, declared type and contents do not agree.",
            ),
            ErrorKind::LimitExceeded => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "media_limit_exceeded",
                "The file or its decoded contents exceed the document limits.",
            ),
            ErrorKind::InvalidContent => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "media_invalid_content",
                "The file contents are malformed or contain unsupported control characters.",
            ),
            ErrorKind::InvalidRequest | ErrorKind::UnsafePath => (
                StatusCode::BAD_REQUEST,
                "media_invalid_request",
                "Use a supported filename and bounded document metadata.",
            ),
            ErrorKind::Integrity => (
                StatusCode::CONFLICT,
                "media_integrity_failed",
                "The saved document failed integrity verification.",
            ),
            ErrorKind::Io => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "media_storage_failed",
                "Document storage failed.",
            ),
        };
        failure(status, code, message)
    }
}
fn failure(status: StatusCode, code: &'static str, message: &'static str) -> MediaError {
    MediaError {
        status,
        code,
        message,
    }
}
fn invalid() -> MediaError {
    failure(
        StatusCode::BAD_REQUEST,
        "media_invalid_request",
        "The document request is invalid.",
    )
}
fn body_rejection(status: StatusCode) -> MediaError {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        failure(
            status,
            "media_limit_exceeded",
            "The document request exceeds the byte limit.",
        )
    } else {
        invalid()
    }
}
fn integrity() -> MediaError {
    failure(
        StatusCode::CONFLICT,
        "media_integrity_failed",
        "The saved document failed integrity verification.",
    )
}
fn missing() -> MediaError {
    failure(
        StatusCode::NOT_FOUND,
        "media_not_found",
        "The requested document is not available in this profile.",
    )
}
fn busy() -> MediaError {
    failure(
        StatusCode::TOO_MANY_REQUESTS,
        "media_busy",
        "Document validation is busy. Try again when the current files finish.",
    )
}
fn request_id(id: &str) -> MediaResult<()> {
    if uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id) {
        Ok(())
    } else {
        Err(invalid())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaDelivery {
    Context,
    ReferenceOnly,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaReference {
    pub id: String,
    pub delivery: MediaDelivery,
}
#[derive(Clone)]
pub struct PreparedMedia {
    pub reference: MediaReference,
    pub manifest: Manifest,
}
#[derive(Clone)]
struct SavedMedia {
    id: String,
    request_id: String,
    origin: String,
    manifest: Manifest,
    bytes: Vec<u8>,
    created_at: i64,
}

fn summary(saved: &SavedMedia, include_context: bool) -> Value {
    let content = match &saved.manifest.content {
        Content::Document {
            context,
            truncated,
            total_context_chars,
            rows,
            columns,
        } => {
            let mut value = json!({"kind":"document","truncated":truncated,"total_context_chars":total_context_chars,"included_context_chars":context.chars().count(),"rows":rows,"columns":columns});
            if include_context {
                value["context"] = json!(context);
            }
            value
        }
        other => serde_json::to_value(other).expect("serializable content"),
    };
    json!({"id":saved.id,"name":saved.manifest.request.filename,"format":saved.manifest.format,"media_type":saved.manifest.detected_mime,"sha256":saved.manifest.sha256,"byte_len":saved.manifest.byte_len,"metadata":saved.manifest.request.metadata,"content":content,"created_at":saved.created_at,"origin":saved.origin,"allowed_delivery":match saved.manifest.content { Content::Document{..}=>vec!["context"], Content::Image{..}=>vec!["reference_only"] },"original_url":format!("/api/media/{}/original",saved.id)})
}
fn load(db: &Connection, id: &str) -> MediaResult<SavedMedia> {
    request_id(id)?;
    let raw=db.query_row("SELECT id,request_id,origin,name,media_type,format,sha256,byte_len,manifest_json,bytes,created_at FROM media_objects WHERE id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,i64>(7)?,r.get::<_,String>(8)?,r.get::<_,Vec<u8>>(9)?,r.get::<_,i64>(10)?))).optional()?.ok_or_else(missing)?;
    if raw.8.len() > MAX_MANIFEST || raw.9.len() > MAX_BYTES {
        return Err(integrity());
    }
    let manifest: Manifest = serde_json::from_str(&raw.8).map_err(|_| integrity())?;
    if manifest.version != 1
        || manifest.request.filename != raw.3
        || manifest.detected_mime != raw.4
        || serde_json::to_value(manifest.format)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .as_deref()
            != Some(raw.5.as_str())
        || manifest.sha256 != raw.6
        || raw.7 < 0
        || manifest.byte_len != raw.7 as u64
        || raw.7 as usize != raw.9.len()
        || smith_media::sha256(&raw.9) != manifest.sha256
    {
        return Err(integrity());
    }
    if let Content::Document {
        context,
        total_context_chars,
        truncated,
        ..
    } = &manifest.content
    {
        let count = context.chars().count();
        if count > Limits::default().max_context_chars
            || *total_context_chars < count
            || *truncated != (*total_context_chars > count)
        {
            return Err(integrity());
        }
    }
    Ok(SavedMedia {
        id: raw.0,
        request_id: raw.1,
        origin: raw.2,
        manifest,
        bytes: raw.9,
        created_at: raw.10,
    })
}
fn inspect_saved(saved: SavedMedia) -> MediaResult<SavedMedia> {
    let manifest = smith_media::inspect(
        saved.manifest.request.clone(),
        &saved.bytes,
        &Limits::default(),
    )
    .map_err(|_| integrity())?;
    if manifest != saved.manifest {
        return Err(integrity());
    }
    Ok(saved)
}
async fn parse_tracked<T, F>(app: &Arc<App>, work: F) -> MediaResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> MediaResult<T> + Send + 'static,
{
    let permit = PARSERS.try_acquire().map_err(|_| busy())?;
    // Register before spawning. The host drains this task even if HTTP is cancelled.
    app.spawn_task(async move {
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|_| integrity())?
    })
    .await
    .map_err(|_| integrity())?
}
async fn verify_async(app: &Arc<App>, saved: SavedMedia) -> MediaResult<SavedMedia> {
    parse_tracked(app, move || inspect_saved(saved)).await
}

fn store_object(
    db: &Connection,
    request: &str,
    origin: &str,
    bytes: Vec<u8>,
    manifest: Manifest,
) -> MediaResult<(SavedMedia, bool)> {
    request_id(request)?;
    let prior: Option<String> = db
        .query_row(
            "SELECT id FROM media_objects WHERE request_id=?1",
            [request],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = prior {
        let saved = load(db, &id)?;
        if saved.origin != origin || saved.manifest != manifest || saved.bytes != bytes {
            return Err(failure(
                StatusCode::CONFLICT,
                "media_request_conflict",
                "This request ID already belongs to different document contents.",
            ));
        }
        return Ok((saved, true));
    }
    let (count, total): (i64, i64) = db.query_row(
        "SELECT count(*),COALESCE(sum(byte_len),0) FROM media_objects",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if count >= MAX_OBJECTS || total.saturating_add(bytes.len() as i64) > MAX_PROFILE_BYTES {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "media_profile_limit",
            "The profile document limit is reached.",
        ));
    }
    let manifest_json = serde_json::to_string(&manifest).map_err(|_| invalid())?;
    if manifest_json.len() > MAX_MANIFEST {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "media_limit_exceeded",
            "Document metadata exceeds the storage limit.",
        ));
    }
    let saved = SavedMedia {
        id: crate::id(),
        request_id: request.into(),
        origin: origin.into(),
        manifest,
        bytes,
        created_at: crate::now() as i64,
    };
    let format = serde_json::to_value(saved.manifest.format).expect("serializable format");
    db.execute("INSERT INTO media_objects(id,request_id,origin,name,media_type,format,sha256,byte_len,manifest_json,bytes,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![saved.id,saved.request_id,saved.origin,saved.manifest.request.filename,saved.manifest.detected_mime,format.as_str().unwrap(),saved.manifest.sha256,saved.manifest.byte_len as i64,manifest_json,saved.bytes,saved.created_at])?;
    Ok((saved, false))
}

/// Called before the message transaction is accepted. Objects/contexts are immutable;
/// references contain no caller-supplied derived text or paths.
pub fn validate_references(
    db: &Connection,
    references: &[MediaReference],
) -> MediaResult<Vec<PreparedMedia>> {
    if references.len() > 3 {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "media_attachment_limit",
            "Attach at most three files per message.",
        ));
    }
    let mut seen = HashSet::new();
    let mut context_bytes = 0usize;
    let mut result = Vec::new();
    for reference in references {
        if !seen.insert(&reference.id) {
            return Err(invalid());
        }
        let saved = inspect_saved(load(db, &reference.id)?)?;
        match (&saved.manifest.content,&reference.delivery) {
            (Content::Document{context,..},MediaDelivery::Context)=>context_bytes+=context.len(),
            (Content::Image{..},MediaDelivery::ReferenceOnly)=>(),
            (Content::Image{..},_)=>return Err(failure(StatusCode::CONFLICT,"media_vision_unavailable","This model cannot view images. Explicitly keep the image as a reference only, or remove it.")),
            _=>return Err(invalid()),
        }
        if context_bytes > MAX_CONTEXT_BYTES {
            return Err(failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "media_context_limit",
                "The attachment excerpts exceed 100,000 UTF-8 bytes. Attach fewer documents.",
            ));
        }
        result.push(PreparedMedia {
            reference: reference.clone(),
            manifest: saved.manifest,
        });
    }
    Ok(result)
}
pub fn attach_prepared(db: &Connection, message: &str, files: &[PreparedMedia]) -> MediaResult<()> {
    for (position, file) in files.iter().enumerate() {
        db.execute("INSERT INTO message_media(id,message_id,media_id,position,delivery) VALUES(?1,?2,?3,?4,?5)",params![crate::id(),message,file.reference.id,position as i64,match file.reference.delivery {MediaDelivery::Context=>"context",MediaDelivery::ReferenceOnly=>"reference_only"}])?;
    }
    Ok(())
}
pub fn references_for_message(db: &Connection, message: &str) -> MediaResult<Vec<MediaReference>> {
    let mut stmt = db.prepare(
        "SELECT media_id,delivery FROM message_media WHERE message_id=?1 ORDER BY position",
    )?;
    let rows = stmt
        .query_map([message], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(id, delivery)| {
            Ok(MediaReference {
                id,
                delivery: match delivery.as_str() {
                    "context" => MediaDelivery::Context,
                    "reference_only" => MediaDelivery::ReferenceOnly,
                    _ => return Err(integrity()),
                },
            })
        })
        .collect()
}
pub fn message_media(db: &Connection, message: &str) -> MediaResult<Vec<Value>> {
    references_for_message(db, message)?
        .into_iter()
        .map(|reference| {
            let mut value = summary(&load(db, &reference.id)?, true);
            value["delivery"] = json!(reference.delivery);
            Ok(value)
        })
        .collect()
}
/// Compose only admitted bounded excerpts. Image pixels never become model text.
pub fn provider_media_content(db: &Connection, message: &str) -> MediaResult<String> {
    let mut text = String::new();
    for reference in references_for_message(db, message)? {
        let saved = load(db, &reference.id)?;
        match (&saved.manifest.content,reference.delivery) {
            (Content::Document{context,truncated,total_context_chars,..},MediaDelivery::Context)=>{
                text.push_str(&format!("\n\n--- Untrusted document attachment: {} ---\n",saved.manifest.request.filename));
                if *truncated {text.push_str(&format!("[Excerpt: {} of {} characters; original retained.]\n",context.chars().count(),total_context_chars));}
                text.push_str(context); text.push_str("\n--- End document attachment ---");
            },
            (Content::Image{..},MediaDelivery::ReferenceOnly)=>text.push_str(&format!("\n\n[Image reference: {}. Original retained in this conversation; image pixels are not provided to the model.]",saved.manifest.request.filename)),
            _=>return Err(integrity()),
        }
    }
    Ok(text)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadMetadata {
    request_id: String,
    filename: String,
    declared_mime: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}
async fn upload(
    State(app): State<Arc<App>>,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> MediaResult<Json<Value>> {
    let _upload = UPLOADS.try_acquire().map_err(|_| busy())?;
    let multipart = multipart.map_err(|error| body_rejection(error.status()))?;
    let (metadata, bytes) =
        tokio::time::timeout(std::time::Duration::from_secs(30), read_upload(multipart))
            .await
            .map_err(|_| {
                failure(
                    StatusCode::REQUEST_TIMEOUT,
                    "media_upload_timeout",
                    "The upload did not finish within 30 seconds.",
                )
            })??;
    let request = AttachmentRequest {
        filename: metadata.filename,
        declared_mime: metadata.declared_mime,
        metadata: metadata.metadata,
    };
    let (bytes, manifest) = parse_tracked(&app, move || {
        let manifest = smith_media::inspect(request, &bytes, &Limits::default())?;
        Ok((bytes, manifest))
    })
    .await?;
    let mut db = app.db.lock().unwrap();
    let tx = db.transaction()?;
    let (saved, duplicate) = store_object(&tx, &metadata.request_id, "upload", bytes, manifest)?;
    if !duplicate {
        crate::action_history::invalidate_redo(&tx)?;
    }
    tx.commit()?;
    let mut value = summary(&saved, true);
    value["duplicate"] = json!(duplicate);
    Ok(Json(value))
}
async fn read_upload(mut multipart: Multipart) -> MediaResult<(UploadMetadata, Vec<u8>)> {
    let mut metadata = None;
    let mut bytes = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| body_rejection(error.status()))?
    {
        let name = field.name().unwrap_or("").to_owned();
        let limit = match name.as_str() {
            "metadata" if metadata.is_none() => 16 * 1024,
            "file" if bytes.is_none() => MAX_BYTES,
            _ => return Err(invalid()),
        };
        let mut buffer = Vec::new();
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|error| body_rejection(error.status()))?
        {
            if buffer.len().saturating_add(chunk.len()) > limit {
                return Err(failure(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "media_limit_exceeded",
                    "Upload exceeds the document byte limit.",
                ));
            }
            buffer.extend_from_slice(&chunk);
        }
        if name == "metadata" {
            metadata =
                Some(serde_json::from_slice::<UploadMetadata>(&buffer).map_err(|_| invalid())?);
        } else {
            bytes = Some(buffer);
        }
    }
    let metadata = metadata.ok_or_else(invalid)?;
    let bytes = bytes.ok_or_else(invalid)?;
    request_id(&metadata.request_id)?;
    Ok((metadata, bytes))
}

async fn metadata(State(app): State<Arc<App>>, Path(id): Path<String>) -> MediaResult<Json<Value>> {
    let saved = load(&app.db.lock().unwrap(), &id)?;
    Ok(Json(summary(&verify_async(&app, saved).await?, true)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterArtifactInput {}
/// Explicitly expose an existing immutable upload in the standalone artifact
/// catalog. Uploading/attaching alone never changes the artifact catalog.
async fn register_artifact(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Result<Json<RegisterArtifactInput>, JsonRejection>,
) -> MediaResult<Json<Value>> {
    let Json(RegisterArtifactInput {}) = input.map_err(|error| body_rejection(error.status()))?;
    request_id(&id)?;
    let saved = load(&app.db.lock().unwrap(), &id)?;
    let saved = verify_async(&app, saved).await?;
    let mut db = app.db.lock().unwrap();
    let tx = db.transaction()?;
    // Validation runs off the database lock. Check exact selected bytes again
    // within the publication transaction so removal/change cannot race it.
    let current = load(&tx, &id)?;
    if current.manifest != saved.manifest || current.bytes != saved.bytes {
        return Err(integrity());
    }
    let existing: Option<String> = tx
        .query_row(
            "SELECT artifact_id FROM artifact_media WHERE media_id=?1 ORDER BY artifact_id LIMIT 1",
            [&id],
            |row| row.get(0),
        )
        .optional()?;
    let duplicate = existing.is_some();
    let artifact_id = match existing {
        Some(id) => id,
        None => {
            let artifact_id = crate::id();
            let preview = match &saved.manifest.content {
                Content::Document { context, .. } => context.as_str(),
                _ => "",
            };
            tx.execute(
                "INSERT INTO artifacts(id,chat_id,name,content,created_at) VALUES(?1,NULL,?2,?3,?4)",
                params![artifact_id, saved.manifest.request.filename, preview, crate::now()],
            )?;
            tx.execute(
                "INSERT INTO artifact_media(artifact_id,media_id) VALUES(?1,?2)",
                params![artifact_id, id],
            )?;
            crate::action_history::invalidate_redo(&tx)?;
            artifact_id
        }
    };
    let result = json!({"artifact":artifact_summary(&tx,&artifact_id)?,"document":summary(&saved,true),"duplicate":duplicate});
    tx.commit()?;
    Ok(Json(result))
}
fn download_bytes(bytes: Vec<u8>, name: &str, mime: &str) -> MediaResult<Response> {
    let encoded = name
        .as_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._".contains(b) {
                (*b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect::<String>();
    let disposition = HeaderValue::from_str(&format!(
        "attachment; filename=smith-document; filename*=UTF-8''{encoded}"
    ))
    .map_err(|_| integrity())?;
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).map_err(|_| integrity())?,
    );
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}
async fn original(State(app): State<Arc<App>>, Path(id): Path<String>) -> MediaResult<Response> {
    let saved = load(&app.db.lock().unwrap(), &id)?;
    let saved = verify_async(&app, saved).await?;
    download_bytes(
        saved.bytes,
        &saved.manifest.request.filename,
        &saved.manifest.detected_mime,
    )
}
async fn remove_unused(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> MediaResult<Json<Value>> {
    request_id(&id)?;
    let mut db = app.db.lock().unwrap();
    let tx = db.transaction()?;
    let used:i64=tx.query_row("SELECT (SELECT count(*) FROM message_media WHERE media_id=?1)+(SELECT count(*) FROM artifact_media WHERE media_id=?1)+(SELECT count(*) FROM drafts d,json_each(d.media) item WHERE json_extract(item.value,'$.id')=?1)",[&id],|r|r.get(0))?;
    if used != 0 {
        return Err(failure(StatusCode::CONFLICT,"media_in_use","This file belongs to a saved message, artifact or draft and cannot be removed as an unused upload."));
    }
    if tx.execute("DELETE FROM media_objects WHERE id=?1", [&id])? == 0 {
        return Err(missing());
    }
    crate::action_history::invalidate_redo(&tx)?;
    tx.commit()?;
    Ok(Json(json!({"ok":true,"id":id})))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Csv,
    Markdown,
    Xlsx,
    Pdf,
    Pptx,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableInput {
    #[serde(default)]
    pub headers: Vec<String>,
    pub rows: Vec<Vec<CellValue>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateInput {
    request_id: String,
    name: String,
    format: OutputFormat,
    table: TableInput,
    chat_id: Option<String>,
    version: Option<i64>,
}
async fn generate(
    State(app): State<Arc<App>>,
    input: Result<Json<GenerateInput>, JsonRejection>,
) -> MediaResult<Json<Value>> {
    let Json(input) = input.map_err(|error| body_rejection(error.status()))?;
    request_id(&input.request_id)?;
    if input.chat_id.is_some() && input.version.is_none() {
        return Err(invalid());
    }
    let request = AttachmentRequest::new(input.name, None);
    let output = match input.format {
        OutputFormat::Csv => ExportFormat::Csv,
        OutputFormat::Markdown => ExportFormat::Markdown,
        OutputFormat::Xlsx => ExportFormat::Xlsx,
        OutputFormat::Pdf => ExportFormat::Pdf,
        OutputFormat::Pptx => ExportFormat::Pptx,
    };
    let (bytes, manifest) = parse_tracked(&app, move || {
        let generated = smith_media::export_table(
            &Table {
                headers: input.table.headers,
                rows: input.table.rows,
            },
            output,
            &Limits::default(),
        )?;
        let manifest = smith_media::inspect(request, &generated.bytes, &Limits::default())?;
        if manifest.format != generated.format {
            return Err(failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                "media_type_mismatch",
                "The filename must match the generated document format.",
            ));
        }
        Ok::<_, MediaError>((generated.bytes, manifest))
    })
    .await?;
    let value = save_generated(
        &mut app.db.lock().unwrap(),
        &input.request_id,
        input.chat_id.as_deref(),
        input.version,
        bytes,
        manifest,
    )?;
    Ok(Json(value))
}
fn save_generated(
    db: &mut Connection,
    request_id: &str,
    chat_id: Option<&str>,
    version: Option<i64>,
    bytes: Vec<u8>,
    manifest: Manifest,
) -> MediaResult<Value> {
    let tx = db.transaction()?;
    let result = save_generated_in_transaction(&tx, request_id, chat_id, version, bytes, manifest)?;
    tx.commit()?;
    Ok(result)
}

/// Host integration: caller owns the effect/revision/receipt transaction. Never
/// use without an explicit transaction; the HTTP wrapper above owns its own.
pub fn save_generated_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    request_id: &str,
    chat_id: Option<&str>,
    version: Option<i64>,
    bytes: Vec<u8>,
    manifest: Manifest,
) -> MediaResult<Value> {
    if tx.is_autocommit() {
        return Err(invalid());
    }
    self::request_id(request_id)?;
    if smith_media::inspect(manifest.request.clone(), &bytes, &Limits::default())? != manifest {
        return Err(integrity());
    }
    let prior:Option<(String,String)>=tx.query_row("SELECT a.id,m.id FROM media_objects m JOIN artifact_media am ON am.media_id=m.id JOIN artifacts a ON a.id=am.artifact_id WHERE m.request_id=?1",[request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((artifact_id, media_id)) = prior {
        let saved = load(tx, &media_id)?;
        let owner: Option<String> = tx.query_row(
            "SELECT chat_id FROM artifacts WHERE id=?1",
            [&artifact_id],
            |r| r.get(0),
        )?;
        if saved.manifest != manifest || saved.bytes != bytes || owner.as_deref() != chat_id {
            return Err(failure(
                StatusCode::CONFLICT,
                "media_request_conflict",
                "This request ID already belongs to another document operation.",
            ));
        }
        return Ok(
            json!({"artifact":artifact_summary(tx,&artifact_id)?,"document":summary(&saved,true),"duplicate":true}),
        );
    }
    let previous = match chat_id {
        Some(chat) => {
            let version = version.ok_or_else(invalid)?;
            chat_management::check_version(tx, chat, version).map_err(|_| {
                failure(
                    StatusCode::CONFLICT,
                    "media_stale_conversation",
                    "This conversation changed. Reopen the export before saving.",
                )
            })?;
            Some(version)
        }
        None => None,
    };
    let (saved, duplicate) = store_object(tx, request_id, "generated", bytes, manifest)?;
    if duplicate {
        return Err(failure(
            StatusCode::CONFLICT,
            "media_request_conflict",
            "This request ID is already used.",
        ));
    }
    let artifact_id = crate::id();
    let preview = match &saved.manifest.content {
        Content::Document { context, .. } => context.as_str(),
        _ => "",
    };
    tx.execute(
        "INSERT INTO artifacts(id,chat_id,name,content,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![
            artifact_id,
            chat_id,
            saved.manifest.request.filename,
            preview,
            crate::now()
        ],
    )?;
    tx.execute(
        "INSERT INTO artifact_media(artifact_id,media_id) VALUES(?1,?2)",
        params![artifact_id, saved.id],
    )?;
    if let Some(chat) = chat_id {
        chat_management::touch(tx, chat, crate::now(), true)?;
    } else {
        crate::action_history::invalidate_redo(tx)?;
    }
    let artifact = artifact_summary(tx, &artifact_id)?;
    Ok(
        json!({"artifact":artifact,"document":summary(&saved,true),"previous_chat_version":previous,"chat_version":previous.map(|v|v+1),"duplicate":false}),
    )
}

/// Strict host read: only immutable, integrity-verified derived text is returned.
/// Raster references are not interpreted as vision input by this tool.
pub fn host_read_document(db: &Connection, media_id: &str) -> MediaResult<Value> {
    request_id(media_id)?;
    let saved = inspect_saved(load(db, media_id)?)?;
    if !matches!(saved.manifest.content, Content::Document { .. }) {
        return Err(failure(StatusCode::UNSUPPORTED_MEDIA_TYPE,"media_vision_required","This image requires an explicitly selected vision provider; it has no extracted document text."));
    }
    Ok(summary(&saved, true))
}
/// Explicit provider workflows may retrieve validated pixels by exact profile ID.
/// This does not itself authorize an external send.
pub fn host_original(db: &Connection, media_id: &str) -> MediaResult<(Vec<u8>, Manifest)> {
    request_id(media_id)?;
    let saved = inspect_saved(load(db, media_id)?)?;
    Ok((saved.bytes, saved.manifest))
}

/// Prepare bytes before the host acquires its commit transaction. This performs
/// no filesystem access, network requests or database writes.
pub fn host_prepare_document(
    name: String,
    format: OutputFormat,
    table: TableInput,
) -> MediaResult<(Vec<u8>, Manifest)> {
    let output = match format {
        OutputFormat::Csv => ExportFormat::Csv,
        OutputFormat::Markdown => ExportFormat::Markdown,
        OutputFormat::Xlsx => ExportFormat::Xlsx,
        OutputFormat::Pdf => ExportFormat::Pdf,
        OutputFormat::Pptx => ExportFormat::Pptx,
    };
    let generated = smith_media::export_table(
        &Table {
            headers: table.headers,
            rows: table.rows,
        },
        output,
        &Limits::default(),
    )?;
    let manifest = smith_media::inspect(
        AttachmentRequest::new(name, None),
        &generated.bytes,
        &Limits::default(),
    )?;
    if manifest.format != generated.format {
        return Err(failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "media_type_mismatch",
            "The filename must match the generated document format.",
        ));
    }
    Ok((generated.bytes, manifest))
}
fn artifact_summary(db: &Connection, id: &str) -> MediaResult<Value> {
    let (chat,name,content,created,media):(Option<String>,String,String,i64,Option<String>)=db.query_row("SELECT a.chat_id,a.name,a.content,a.created_at,am.media_id FROM artifacts a LEFT JOIN artifact_media am ON a.id=am.artifact_id WHERE a.id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?.ok_or_else(missing)?;
    let mut result = json!({"id":id,"chat_id":chat,"name":name,"created_at":created,"download_url":format!("/api/media/artifacts/{id}/original")});
    if let Some(media) = media {
        result["document"] = summary(&load(db, &media)?, false);
        result["kind"] = json!("document");
    } else {
        result["kind"] = json!("legacy_text");
        result["byte_len"] = json!(content.len());
        result["media_type"] = json!("text/plain");
    }
    // The text projection and retained original are different representations.
    // Bind the original to the actual archived row, never to an ID prefix or
    // client-supplied URL; external references remain unavailable for download.
    let original: Option<Value> = db.query_row(
        "SELECT content_kind,mime,length(bytes),sha256,(SELECT CASE WHEN count(*)=1 THEN json_extract(report_json,'$.source_schema') END FROM legacy_imports) FROM legacy_artifacts WHERE id=?1",
        [id], |r| {
            let kind: String = r.get(0)?;
            let size: Option<i64> = r.get(2)?;
            let hash: Option<String> = r.get(3)?;
            let encoded: String = id.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {(b as char).to_string()} else {format!("%{b:02X}")}).collect();
            let url = (kind=="managed_original" && size.is_some_and(|n|n>=0) && hash.as_ref().is_some_and(|h|h.len()==64 && h.bytes().all(|b|b.is_ascii_hexdigit())))
                .then(||format!("/api/legacy-import/artifacts/{encoded}/bytes"));
            Ok(json!({"source_schema":r.get::<_,Option<i64>>(4)?,"content_kind":kind,"media_type":r.get::<_,String>(1)?,"byte_len":size,"sha256":hash,"download_url":url}))
        },
    ).optional()?;
    if let Some(original) = original {
        result["original_import"] = original;
    }
    Ok(result)
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    chat_id: Option<String>,
    before: Option<i64>,
}
async fn artifacts(
    State(app): State<Arc<App>>,
    Query(query): Query<ListQuery>,
) -> MediaResult<Json<Value>> {
    let db = app.db.lock().unwrap();
    let mut stmt=db.prepare("SELECT id,rowid FROM artifacts WHERE (?1 IS NULL OR chat_id=?1) AND (?2 IS NULL OR rowid<?2) ORDER BY rowid DESC LIMIT 101")?;
    let mut ids = stmt
        .query_map(params![query.chat_id, query.before], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let more = ids.len() > 100;
    ids.truncate(100);
    let next = more.then(|| ids.last().unwrap().1);
    let rows = ids
        .iter()
        .map(|(id, _)| artifact_summary(&db, id))
        .collect::<MediaResult<Vec<_>>>()?;
    Ok(Json(json!({"artifacts":rows,"next_cursor":next})))
}
pub async fn artifact_original(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> MediaResult<Response> {
    let (media, name, text) = {
        let db = app.db.lock().unwrap();
        db.query_row("SELECT am.media_id,a.name,a.content FROM artifacts a LEFT JOIN artifact_media am ON a.id=am.artifact_id WHERE a.id=?1",[id],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()?.ok_or_else(missing)?
    };
    if let Some(media) = media {
        original(State(app), Path(media)).await
    } else {
        download_bytes(text.into_bytes(), &name, "text/plain; charset=utf-8")
    }
}
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .merge(crate::media_runtime::routes())
        .route(
            "/api/media/uploads",
            post(upload).layer(DefaultBodyLimit::max(MAX_BYTES + 64 * 1024)),
        )
        .route(
            "/api/media/generate",
            post(generate).layer(DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        .route("/api/media/artifacts", get(artifacts))
        .route("/api/media/artifacts/{id}/original", get(artifact_original))
        .route("/api/media/{id}", get(metadata).delete(remove_unused))
        .route("/api/media/{id}/original", get(original))
        .route("/api/media/{id}/artifact", post(register_artifact))
}

#[cfg(test)]
mod tests {
    use super::*;
    const XLSX: &[u8] = include_bytes!("../crates/smith-media/tests/fixtures/cells.xlsx");
    const PNG: &[u8] = include_bytes!("../crates/smith-media/tests/fixtures/pixels.png");
    static API_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        smith_profile_storage::initialize_schema(&db).unwrap();
        chat_management::create(&db, "chat", "Synthetic documents", 1).unwrap();
        db
    }
    fn add(db: &Connection, name: &str, bytes: &[u8]) -> SavedMedia {
        let manifest = smith_media::inspect(
            AttachmentRequest::new(name, None),
            bytes,
            &Limits::default(),
        )
        .unwrap();
        store_object(db, &crate::id(), "upload", bytes.to_vec(), manifest)
            .unwrap()
            .0
    }
    fn user(db: &Connection, id: &str) {
        db.execute("INSERT INTO messages(id,chat_id,role,content,status,created_at) VALUES(?1,'chat','user','synthetic prompt','completed',1)",[id]).unwrap();
    }
    fn reference(saved: &SavedMedia, delivery: MediaDelivery) -> MediaReference {
        MediaReference {
            id: saved.id.clone(),
            delivery,
        }
    }
    fn table() -> TableInput {
        TableInput {
            headers: vec!["id".into(), "value".into()],
            rows: vec![vec![
                CellValue::Text("00123".into()),
                CellValue::Number("123.50".into()),
            ]],
        }
    }
    #[test]
    fn imported_artifact_metadata_separates_original_bytes_from_text_projection() {
        let db = db();
        db.execute(
            "INSERT INTO legacy_imports(id,report_json,created_at) VALUES('copy',?1,1)",
            [r#"{"source_schema":25,"verified":true}"#],
        )
        .unwrap();
        for (id, kind, bytes) in [
            (
                "a/b%&",
                "managed_original",
                Some(b"exact synthetic original".as_slice()),
            ),
            ("external", "external_reference", None),
            ("derived", "derived_reference", None),
        ] {
            db.execute("INSERT INTO artifacts(id,name,content,created_at) VALUES(?1,'Synthetic original','Readable projection',1)",[id]).unwrap();
            db.execute("INSERT INTO legacy_artifacts(id,store_id,title,mime,original_relpath,original_created_at,content_kind,bytes,sha256) VALUES(?1,'synthetic','Synthetic original','application/octet-stream','synthetic.bin','synthetic',?2,?3,?4)",params![id,kind,bytes,bytes.map(smith_media::sha256)]).unwrap();
            let row = artifact_summary(&db, id).unwrap();
            assert_eq!(row["kind"], "legacy_text");
            assert_eq!(row["original_import"]["source_schema"], 25);
            assert_eq!(row["original_import"]["content_kind"], kind);
            if let Some(bytes) = bytes {
                assert_eq!(
                    row["original_import"]["download_url"],
                    "/api/legacy-import/artifacts/a%2Fb%25%26/bytes"
                );
                assert_eq!(row["original_import"]["byte_len"], bytes.len());
                assert_eq!(row["original_import"]["sha256"], smith_media::sha256(bytes));
            } else {
                assert!(row["original_import"]["download_url"].is_null());
                assert!(row["original_import"]["byte_len"].is_null());
            }
        }
        db.execute(
            "INSERT INTO artifacts(id,name,content,created_at) VALUES('plain','Plain','Text',1)",
            [],
        )
        .unwrap();
        assert!(artifact_summary(&db, "plain")
            .unwrap()
            .get("original_import")
            .is_none());
    }
    fn generate_input(name: &str, format: OutputFormat, version: i64) -> GenerateInput {
        GenerateInput {
            request_id: crate::id(),
            name: name.into(),
            format,
            table: table(),
            chat_id: Some("chat".into()),
            version: Some(version),
        }
    }

    #[test]
    fn host_document_exports_share_atomic_receipt_transaction_and_read_real_context() {
        let mut db = db();
        for (format, name) in [
            (OutputFormat::Pdf, "report.pdf"),
            (OutputFormat::Pptx, "report.pptx"),
        ] {
            let (bytes, manifest) = host_prepare_document(name.into(), format, table()).unwrap();
            let request = crate::id();
            let tx = db.transaction().unwrap();
            let saved = save_generated_in_transaction(
                &tx,
                &request,
                None,
                None,
                bytes.clone(),
                manifest.clone(),
            )
            .unwrap();
            let id = saved["document"]["id"].as_str().unwrap();
            assert_eq!(host_original(&tx, id).unwrap().0, bytes);
            assert!(host_read_document(&tx, id).unwrap()["content"]["context"]
                .as_str()
                .unwrap()
                .contains("00123"));
            let again =
                save_generated_in_transaction(&tx, &request, None, None, bytes, manifest).unwrap();
            assert_eq!(saved["artifact"]["id"], again["artifact"]["id"]);
            // Simulate a later task-receipt failure: neither effect may survive.
            tx.rollback().unwrap();
            assert_eq!(
                db.query_row("SELECT count(*) FROM media_objects", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn media_original_manifest_metadata_and_idempotency_survive_storage() {
        let db = db();
        let request_id = crate::id();
        let mut request = AttachmentRequest::new("table.csv", Some("text/csv"));
        request
            .metadata
            .insert("source".into(), "synthetic fixture".into());
        let bytes = b"id,value\n00123,123.50\n";
        let manifest = smith_media::inspect(request, bytes, &Limits::default()).unwrap();
        let (saved, duplicate) =
            store_object(&db, &request_id, "upload", bytes.to_vec(), manifest.clone()).unwrap();
        assert!(!duplicate);
        let (again, duplicate) =
            store_object(&db, &request_id, "upload", bytes.to_vec(), manifest.clone()).unwrap();
        assert!(duplicate);
        assert_eq!(again.id, saved.id);
        let loaded = inspect_saved(load(&db, &saved.id).unwrap()).unwrap();
        assert_eq!(loaded.bytes, bytes);
        assert_eq!(loaded.manifest, manifest);
        let changed = smith_media::inspect(
            AttachmentRequest::new("changed.txt", None),
            b"changed",
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(
            store_object(&db, &request_id, "upload", b"changed".to_vec(), changed)
                .err()
                .unwrap()
                .code,
            "media_request_conflict"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM media_objects", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn immutable_attachment_references_keep_sibling_branches_distinct() {
        let db = db();
        user(&db, "original");
        user(&db, "edited");
        user(&db, "retained");
        let first = add(&db, "one.txt", b"original branch data");
        let second = add(&db, "two.txt", b"edited branch data");
        attach_prepared(
            &db,
            "original",
            &validate_references(&db, &[reference(&first, MediaDelivery::Context)]).unwrap(),
        )
        .unwrap();
        attach_prepared(
            &db,
            "edited",
            &validate_references(&db, &[reference(&second, MediaDelivery::Context)]).unwrap(),
        )
        .unwrap();
        let retained = references_for_message(&db, "original").unwrap();
        attach_prepared(
            &db,
            "retained",
            &validate_references(&db, &retained).unwrap(),
        )
        .unwrap();
        assert_eq!(
            provider_media_content(&db, "original").unwrap(),
            provider_media_content(&db, "retained").unwrap()
        );
        assert!(!provider_media_content(&db, "edited")
            .unwrap()
            .contains("original branch data"));
        assert_eq!(message_media(&db, "original").unwrap()[0]["id"], first.id);
        assert_eq!(
            db.query_row("SELECT count(*) FROM media_objects", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn raster_requires_explicit_reference_only_and_context_is_honest() {
        let db = db();
        user(&db, "u");
        let image = add(&db, "image.png", PNG);
        assert_eq!(
            validate_references(&db, &[reference(&image, MediaDelivery::Context)])
                .err()
                .unwrap()
                .code,
            "media_vision_unavailable"
        );
        let prepared =
            validate_references(&db, &[reference(&image, MediaDelivery::ReferenceOnly)]).unwrap();
        attach_prepared(&db, "u", &prepared).unwrap();
        let content = provider_media_content(&db, "u").unwrap();
        assert!(content.contains("image pixels are not provided to the model"));
        assert_eq!(load(&db, &image.id).unwrap().bytes, PNG);
        assert_eq!(
            message_media(&db, "u").unwrap()[0]["allowed_delivery"],
            json!(["reference_only"])
        );
    }

    #[test]
    fn original_bytes_survive_context_truncation_and_xlsx_is_typed() {
        let db = db();
        user(&db, "u");
        let bytes = "🦀".repeat(20_010).into_bytes();
        let file = add(&db, "long.txt", &bytes);
        attach_prepared(
            &db,
            "u",
            &validate_references(&db, &[reference(&file, MediaDelivery::Context)]).unwrap(),
        )
        .unwrap();
        let summary = message_media(&db, "u").unwrap();
        assert_eq!(summary[0]["content"]["truncated"], true);
        assert_eq!(summary[0]["content"]["total_context_chars"], 20_010);
        assert!(provider_media_content(&db, "u")
            .unwrap()
            .contains("20000 of 20010"));
        assert_eq!(load(&db, &file.id).unwrap().bytes, bytes);
        let workbook = add(&db, "book.xlsx", XLSX);
        let Content::Document { context, .. } = &workbook.manifest.content else {
            panic!()
        };
        let parsed: smith_media::Workbook = serde_json::from_str(context).unwrap();
        assert_eq!(
            parsed.sheets[0].cells[2].value,
            CellValue::Text("00123".into())
        );
    }

    #[test]
    fn reference_count_duplicate_ids_and_aggregate_context_have_limits() {
        let db = db();
        let a = add(&db, "a.txt", "🦀".repeat(20_000).as_bytes());
        let b = add(&db, "b.txt", "🦀".repeat(20_000).as_bytes());
        assert_eq!(
            validate_references(
                &db,
                &[
                    reference(&a, MediaDelivery::Context),
                    reference(&b, MediaDelivery::Context)
                ]
            )
            .err()
            .unwrap()
            .code,
            "media_context_limit"
        );
        assert_eq!(
            validate_references(
                &db,
                &[
                    reference(&a, MediaDelivery::Context),
                    reference(&a, MediaDelivery::Context)
                ]
            )
            .err()
            .unwrap()
            .code,
            "media_invalid_request"
        );
        assert_eq!(
            validate_references(&db, &vec![reference(&a, MediaDelivery::Context); 4])
                .err()
                .unwrap()
                .code,
            "media_attachment_limit"
        );
    }

    #[test]
    fn altered_originals_and_derived_context_fail_verification() {
        let db = db();
        let file = add(&db, "note.txt", b"original");
        db.execute(
            "UPDATE media_objects SET bytes=?2 WHERE id=?1",
            params![file.id, b"modified".as_slice()],
        )
        .unwrap();
        assert_eq!(
            load(&db, &file.id).err().unwrap().code,
            "media_integrity_failed"
        );
        db.execute(
            "UPDATE media_objects SET bytes=?2 WHERE id=?1",
            params![file.id, b"original".as_slice()],
        )
        .unwrap();
        let mut manifest = file.manifest.clone();
        if let Content::Document { context, .. } = &mut manifest.content {
            *context = "changed!".into();
        }
        db.execute(
            "UPDATE media_objects SET manifest_json=?2 WHERE id=?1",
            params![file.id, serde_json::to_string(&manifest).unwrap()],
        )
        .unwrap();
        assert_eq!(
            inspect_saved(load(&db, &file.id).unwrap())
                .err()
                .unwrap()
                .code,
            "media_integrity_failed"
        );
    }

    #[tokio::test]
    async fn real_generated_formats_reopen_download_and_preserve_typed_cells() {
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        for (index, (name, format)) in [
            ("report.csv", OutputFormat::Csv),
            ("report.md", OutputFormat::Markdown),
            ("report.xlsx", OutputFormat::Xlsx),
        ]
        .into_iter()
        .enumerate()
        {
            let Json(result) = generate(
                State(app.clone()),
                Ok(Json(generate_input(name, format, index as i64))),
            )
            .await
            .unwrap();
            let document = &result["document"];
            assert_eq!(result["chat_version"], index + 1);
            let saved = load(&app.db.lock().unwrap(), document["id"].as_str().unwrap()).unwrap();
            if name.ends_with(".xlsx") {
                let book = smith_media::read_workbook(&saved.bytes, &Limits::default()).unwrap();
                assert_eq!(
                    book.sheets[0].cells[2].value,
                    CellValue::Text("00123".into())
                );
            } else {
                assert!(std::str::from_utf8(&saved.bytes).unwrap().contains("00123"));
            }
            let response = artifact_original(
                State(app.clone()),
                Path(result["artifact"]["id"].as_str().unwrap().into()),
            )
            .await
            .unwrap();
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let downloaded = axum::body::to_bytes(response.into_body(), MAX_BYTES)
                .await
                .unwrap();
            assert_eq!(downloaded.as_ref(), saved.bytes);
        }
        let Json(list) = artifacts(State(app.clone()), Query(ListQuery::default()))
            .await
            .unwrap();
        assert_eq!(list["artifacts"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn explicit_import_registration_is_idempotent_atomic_and_preserves_original_bytes() {
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        let file = add(
            &app.db.lock().unwrap(),
            "imported.csv",
            b"name,value\nalpha,00123\n",
        );
        let request = || Ok(Json(RegisterArtifactInput {}));
        app.db.lock().unwrap().execute_batch("CREATE TRIGGER reject_import BEFORE INSERT ON artifact_media BEGIN SELECT RAISE(ABORT,'synthetic registration failure'); END;").unwrap();
        assert_eq!(
            register_artifact(State(app.clone()), Path(file.id.clone()), request())
                .await
                .err()
                .unwrap()
                .code,
            "media_storage_failed"
        );
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM artifacts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            load(&app.db.lock().unwrap(), &file.id).unwrap().bytes,
            file.bytes
        );
        app.db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_import")
            .unwrap();
        let Json(first) = register_artifact(State(app.clone()), Path(file.id.clone()), request())
            .await
            .unwrap();
        let Json(retry) = register_artifact(State(app.clone()), Path(file.id.clone()), request())
            .await
            .unwrap();
        assert_eq!(first["duplicate"], false);
        assert_eq!(retry["duplicate"], true);
        assert_eq!(first["artifact"]["id"], retry["artifact"]["id"]);
        assert!(first["artifact"]["chat_id"].is_null());
        assert!(remove_unused(State(app.clone()), Path(file.id.clone()))
            .await
            .is_err());
        let response = artifact_original(
            State(app.clone()),
            Path(first["artifact"]["id"].as_str().unwrap().into()),
        )
        .await
        .unwrap();
        assert_eq!(
            axum::body::to_bytes(response.into_body(), MAX_BYTES)
                .await
                .unwrap()
                .as_ref(),
            file.bytes
        );
        app.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE media_objects SET bytes=?2 WHERE id=?1",
                params![file.id, vec![b'x'; file.bytes.len()]],
            )
            .unwrap();
        assert_eq!(
            register_artifact(State(app.clone()), Path(file.id), request())
                .await
                .err()
                .unwrap()
                .code,
            "media_integrity_failed"
        );
        assert!(
            serde_json::from_value::<RegisterArtifactInput>(json!({"chat_id":"unreviewed"}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn generated_artifact_version_conflicts_and_storage_failures_are_atomic() {
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        let err = generate(
            State(app.clone()),
            Ok(Json(generate_input("report.csv", OutputFormat::Csv, 9))),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.code, "media_stale_conversation");
        app.db.lock().unwrap().execute_batch("CREATE TRIGGER reject_artifact BEFORE INSERT ON artifacts BEGIN SELECT RAISE(ABORT,'synthetic write failure'); END;").unwrap();
        let err = generate(
            State(app.clone()),
            Ok(Json(generate_input("report.xlsx", OutputFormat::Xlsx, 0))),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.code, "media_storage_failed");
        let db = app.db.lock().unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM media_objects", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(chat_management::summary(&db, "chat").unwrap().version, 0);
    }

    #[tokio::test]
    async fn generation_idempotency_and_safe_csv_formula_errors_are_explicit() {
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        let request = crate::id();
        let mut input = generate_input("book.xlsx", OutputFormat::Xlsx, 0);
        input.request_id = request.clone();
        let Json(first) = generate(State(app.clone()), Ok(Json(input))).await.unwrap();
        let mut input = generate_input("book.xlsx", OutputFormat::Xlsx, 0);
        input.request_id = request;
        let Json(again) = generate(State(app.clone()), Ok(Json(input))).await.unwrap();
        assert_eq!(first["artifact"]["id"], again["artifact"]["id"]);
        assert_eq!(again["duplicate"], true);
        let mut input = generate_input("unsafe.csv", OutputFormat::Csv, 1);
        input.table.rows[0][0] = CellValue::Text("=SUM(A1:A2)".into());
        assert_eq!(
            generate(State(app.clone()), Ok(Json(input)))
                .await
                .err()
                .unwrap()
                .code,
            "media_unsupported"
        );
        assert_eq!(
            chat_management::summary(&app.db.lock().unwrap(), "chat")
                .unwrap()
                .version,
            1
        );
    }

    #[test]
    fn encrypted_profile_retains_media_without_plaintext_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("fixture.db");
        let secret =
            smith_profile_storage::Secret::new("synthetic-media-profile-passphrase".into())
                .unwrap();
        let db = smith_profile_storage::create(&path, &secret).unwrap();
        let marker = b"Synthetic document marker 2026: never a plaintext media sidecar.";
        let saved = add(&db, "private.txt", marker);
        let image = add(&db, "image.png", PNG);
        assert!(smith_profile_storage::close(db).is_ok());
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.starts_with(b"SQLite format 3"));
        assert!(!raw.windows(marker.len()).any(|v| v == marker));
        assert!(!dir.path().join("attachments").exists());
        assert!(!dir.path().join(".staging").exists());
        let db = smith_profile_storage::unlock(&path, &secret).unwrap();
        assert_eq!(
            inspect_saved(load(&db, &saved.id).unwrap()).unwrap().bytes,
            marker
        );
        assert_eq!(
            inspect_saved(load(&db, &image.id).unwrap()).unwrap().bytes,
            PNG
        );
        assert!(smith_profile_storage::close(db).is_ok());
    }
    fn multipart_request(
        name: &str,
        bytes: &[u8],
        declared_mime: Option<&str>,
    ) -> axum::http::Request<axum::body::Body> {
        multipart_request_with_id(name, bytes, declared_mime, &crate::id())
    }
    fn multipart_request_with_id(
        name: &str,
        bytes: &[u8],
        declared_mime: Option<&str>,
        request_id: &str,
    ) -> axum::http::Request<axum::body::Body> {
        let metadata =
            json!({"request_id":request_id,"filename":name,"declared_mime":declared_mime});
        let mut body=format!("--fixture-boundary\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\r\n{metadata}\r\n--fixture-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n--fixture-boundary--\r\n");
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/media/uploads")
            .header(
                header::CONTENT_TYPE,
                "multipart/form-data; boundary=fixture-boundary",
            )
            .body(axum::body::Body::from(body))
            .unwrap()
    }
    #[tokio::test]
    async fn real_http_upload_and_download_preserve_xlsx_and_reject_bad_content() {
        use tower::ServiceExt;
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        let router = routes().with_state(app.clone());
        let response = router
            .clone()
            .oneshot(multipart_request("book.xlsx", XLSX, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), MAX_MANIFEST)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["format"], "xlsx");
        assert_eq!(value["allowed_delivery"], json!(["context"]));
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(value["original_url"].as_str().unwrap())
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), MAX_BYTES)
                .await
                .unwrap()
                .as_ref(),
            XLSX
        );
        for (name, bytes, mime, code) in [
            (
                "broken.png",
                b"invalid".as_slice(),
                None,
                "media_type_mismatch",
            ),
            (
                "file.pdf",
                b"unsupported".as_slice(),
                None,
                "media_unsupported",
            ),
            ("book.xlsx", XLSX, Some("text/plain"), "media_type_mismatch"),
        ] {
            let response = router
                .clone()
                .oneshot(multipart_request(name, bytes, mime))
                .await
                .unwrap();
            assert!(!response.status().is_success());
            let value: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(value["code"], code);
        }
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM media_objects", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[tokio::test]
    async fn http_upload_byte_limit_and_referenced_deletion_are_explicit() {
        use tower::ServiceExt;
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        let router = routes().with_state(app.clone());
        for length in [MAX_BYTES + 1, MAX_BYTES + 64 * 1024 + 1] {
            let response = router
                .clone()
                .oneshot(multipart_request("too-big.txt", &vec![b'a'; length], None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let value: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(value["code"], "media_limit_exceeded");
        }
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/media/generate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(vec![b' '; 4 * 1024 * 1024 + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["code"], "media_limit_exceeded");
        let saved = {
            let db = app.db.lock().unwrap();
            user(&db, "u");
            let saved = add(&db, "used.txt", b"keep");
            attach_prepared(
                &db,
                "u",
                &validate_references(&db, &[reference(&saved, MediaDelivery::Context)]).unwrap(),
            )
            .unwrap();
            saved
        };
        assert_eq!(
            remove_unused(State(app.clone()), Path(saved.id.clone()))
                .await
                .err()
                .unwrap()
                .code,
            "media_in_use"
        );
        let unused = add(&app.db.lock().unwrap(), "unused.txt", b"remove");
        let _ = remove_unused(State(app.clone()), Path(unused.id.clone()))
            .await
            .unwrap();
        assert_eq!(
            load(&app.db.lock().unwrap(), &unused.id)
                .err()
                .unwrap()
                .code,
            "media_not_found"
        );
        assert_eq!(
            load(&app.db.lock().unwrap(), &saved.id).unwrap().bytes,
            b"keep"
        );
    }
    #[tokio::test]
    async fn cancelled_http_future_does_not_detach_plaintext_parser_from_profile_drain() {
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let captured = app.clone();
        let request = tokio::spawn(async move {
            parse_tracked(&captured, move || {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                Ok::<_, MediaError>("synthetic plaintext".to_owned())
            })
            .await
        });
        started_rx.await.unwrap();
        request.abort();
        let _ = request.await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), app.tasks.drain())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), app.tasks.drain())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn unused_deletion_preserves_documents_owned_by_any_encrypted_draft() {
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        let saved = add(&app.db.lock().unwrap(), "draft-only.txt", b"draft context");
        let refs = serde_json::to_string(&[reference(&saved, MediaDelivery::Context)]).unwrap();
        app.db.lock().unwrap().execute(
            "INSERT INTO drafts(owner,version,text,attachments,media,updated_at) VALUES('another-draft',1,'','[]',?1,0)",
            [&refs],
        ).unwrap();
        assert_eq!(
            remove_unused(State(app.clone()), Path(saved.id.clone()))
                .await
                .unwrap_err()
                .code,
            "media_in_use"
        );
        assert_eq!(
            load(&app.db.lock().unwrap(), &saved.id).unwrap().bytes,
            b"draft context"
        );
        app.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE drafts SET media='not valid JSON' WHERE owner='another-draft'",
                [],
            )
            .unwrap();
        assert_eq!(
            remove_unused(State(app.clone()), Path(saved.id.clone()))
                .await
                .unwrap_err()
                .code,
            "media_storage_failed"
        );
        app.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE drafts SET media='[]' WHERE owner='another-draft'",
                [],
            )
            .unwrap();
        let _ = remove_unused(State(app.clone()), Path(saved.id.clone()))
            .await
            .unwrap();
        assert_eq!(
            load(&app.db.lock().unwrap(), &saved.id).err().unwrap().code,
            "media_not_found"
        );
    }
    #[tokio::test]
    async fn media_saved_edits_invalidate_redo_atomically_but_retries_and_failures_do_not() {
        use tower::ServiceExt;
        let _guard = API_TESTS.lock().await;
        let app = crate::app_from_connection(db()).unwrap();
        let revision = || {
            app.db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT revision FROM action_history_state WHERE id=1",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        let before = revision();
        app.db.lock().unwrap().execute(
            "INSERT INTO action_history(id,kind,before_json,after_json,state,created_at) VALUES('synthetic-undone','rename','[]','[]','undone',0)", [],
        ).unwrap();
        let router = routes().with_state(app.clone());
        let request_id = crate::id();
        let mut uploaded = String::new();
        for duplicate in [false, true] {
            let response = router
                .clone()
                .oneshot(multipart_request_with_id(
                    "redo.txt",
                    b"synthetic redo fixture",
                    None,
                    &request_id,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let value: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), MAX_MANIFEST)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(value["duplicate"], duplicate);
            uploaded = value["id"].as_str().unwrap().to_owned();
            assert_eq!(revision(), before + 1);
        }
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT state FROM action_history WHERE id='synthetic-undone'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
            "invalidated"
        );
        let _ = metadata(State(app.clone()), Path(uploaded.clone()))
            .await
            .unwrap();
        assert_eq!(revision(), before + 1);
        let mut input = generate_input("library.xlsx", OutputFormat::Xlsx, 0);
        input.chat_id = None;
        input.version = None;
        let _ = generate(State(app.clone()), Ok(Json(input))).await.unwrap();
        assert_eq!(revision(), before + 2);
        let _ = remove_unused(State(app.clone()), Path(uploaded))
            .await
            .unwrap();
        assert_eq!(revision(), before + 3);
        let count: i64 = app
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM media_objects", [], |r| r.get(0))
            .unwrap();
        app.db.lock().unwrap().execute_batch("CREATE TRIGGER reject_history BEFORE UPDATE ON action_history_state BEGIN SELECT RAISE(ABORT,'synthetic history failure'); END;").unwrap();
        let response = router
            .oneshot(multipart_request("rollback.txt", b"do not publish", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(revision(), before + 3);
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM media_objects", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            count
        );
    }
}
