//! Pending read-only route wrapper. Register only inside the existing Host's
//! profile admission router; never serve this router directly or add query-token
//! bypasses. This file intentionally is not registered by the isolated slice.
#[path = "historical_message_store.rs"]
mod store;
use crate::{ApiError, ApiResult, App};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Semaphore;

// Bound capture + parsing together. Each task retains its slot even if the
// client disconnects. The profile's TaskTracker drains its blocking work.
static READERS: Semaphore = Semaphore::const_new(1);
fn refused(error: store::Error) -> ApiError {
    ApiError(StatusCode::CONFLICT, error.to_string())
}
fn unavailable() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "Historical image inspection is busy; retry after it finishes.".into(),
    )
}
async fn tracked<T, F>(app: &Arc<App>, work: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> store::Result<T> + Send + 'static,
{
    let permit = READERS.try_acquire().map_err(|_| unavailable())?;
    app.spawn_task(async move {
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work().map_err(refused)
        })
        .await
        .map_err(|_| unavailable())?
    })
    .await
    .map_err(|_| unavailable())?
}
async fn cards(State(app): State<Arc<App>>, Path(row): Path<i64>) -> ApiResult<Response> {
    let owned = app.clone();
    let encoded = tracked(&app, move || {
        let snapshot = {
            let db = owned
                .db
                .lock()
                .map_err(|_| store::Error("The historical archive is unavailable."))?;
            store::capture(&db, row, None)?
        };
        // No SQLite mutex is held during raster decoding or JSON encoding.
        store::projection::encode_cards(&snapshot.cards()?)
    })
    .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        encoded,
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageQuery {
    import_id: String,
    source_schema: u32,
    payload_sha256: String,
}
async fn image(
    State(app): State<Arc<App>>,
    Path((row, index)): Path<(i64, usize)>,
    Query(query): Query<ImageQuery>,
) -> ApiResult<Response> {
    let source = store::contract::MessageSource {
        import_id: query.import_id,
        source_schema: query.source_schema,
        table: "messages".into(),
        row_number: row,
        payload_sha256: query.payload_sha256,
    };
    if !source.valid() || index >= store::contract::MAX_IMAGES {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Use the exact reviewed original image identity.".into(),
        ));
    }
    let owned = app.clone();
    let validated = tracked(&app, move || {
        let snapshot = {
            let db = owned
                .db
                .lock()
                .map_err(|_| store::Error("The historical archive is unavailable."))?;
            store::capture(&db, row, Some(&source))?
        };
        snapshot.embedded_image(&source, index)
    })
    .await?;
    Ok((
        [
            (header::CONTENT_TYPE, validated.mime),
            (header::CACHE_CONTROL, "no-store".into()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            (
                header::CONTENT_DISPOSITION,
                "inline; filename=\"original-image\"".into(),
            ),
            (
                header::HeaderName::from_static("x-smith-content-sha256"),
                validated.sha256,
            ),
        ],
        validated.bytes,
    )
        .into_response())
}
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/legacy-import/message-cards/{row}", get(cards))
        .route(
            "/api/legacy-import/message-cards/{row}/images/{index}/bytes",
            get(image),
        )
}
