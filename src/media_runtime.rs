//! Explicit selected-provider media operations. A reviewed binding is transient,
//! capability-specific and tied to the current profile/connection/model. No
//! discovery, fallback, model download, URL fetch or capture device is used.
#[path = "local_transcription.rs"]
pub mod local_transcription;
use crate::{
    media_api::{self, MediaError, MediaResult},
    task_model, App, Expected,
};
use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smith_agent_core::{
    Availability, Capability, Error, ExecutionGrant, HostToolAdapter, HostToolContext,
    ResourceKind, ToolCall, ToolDescriptor,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaCapability {
    EmbedText,
    GenerateImage,
    Vision,
    TranscribeAudio,
}
#[derive(Default)]
pub struct MediaState {
    inner: Mutex<Inner>,
    local_transcription: local_transcription::TranscriptionState,
}
#[derive(Default)]
struct Inner {
    closed: bool,
    bindings: BTreeMap<String, Arc<Binding>>,
    jobs: BTreeMap<String, Job>,
    adapter_registered: bool,
}
struct Binding {
    id: String,
    capability: MediaCapability,
    selection: task_model::ModelSelection,
    auth_revision: u64,
}
struct Job {
    fingerprint: String,
    cancelled: bool,
    result: Option<MediaResult<Value>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    EmbedText {
        text: String,
    },
    GenerateImage {
        prompt: String,
        width: u32,
        height: u32,
        name: String,
    },
    Vision {
        prompt: String,
        media_id: String,
    },
    TranscribeAudio {
        wav_base64: String,
        language: String,
    },
}
impl Operation {
    fn capability(&self) -> MediaCapability {
        match self {
            Self::EmbedText { .. } => MediaCapability::EmbedText,
            Self::GenerateImage { .. } => MediaCapability::GenerateImage,
            Self::Vision { .. } => MediaCapability::Vision,
            Self::TranscribeAudio { .. } => MediaCapability::TranscribeAudio,
        }
    }
    fn validate(&self) -> MediaResult<()> {
        match self {
            Self::EmbedText { text } => text_limit(text, 16 * 1024)?,
            Self::GenerateImage {
                prompt,
                width,
                height,
                name,
            } => {
                text_limit(prompt, 4096)?;
                // This compatibility profile exposes size, not diffusion steps.
                // Unsupported original steps/edit/upscale are not silently ignored.
                if ![256, 512, 1024].contains(width) || width != height {
                    return Err(unsupported(
                        "This image protocol supports square 256, 512 or 1024 pixel output.",
                    ));
                }
                if name.len() > 255
                    || !name.ends_with(".png")
                    || name.contains(['/', '\\', ':'])
                    || name.chars().any(char::is_control)
                {
                    return Err(invalid());
                }
            }
            Self::Vision { prompt, media_id } => {
                text_limit(prompt, 4096)?;
                valid_id(media_id)?;
            }
            Self::TranscribeAudio {
                wav_base64,
                language,
            } => {
                if wav_base64.len() > 3 * 1024 * 1024
                    || !matches!(language.as_str(), "en" | "ru" | "auto")
                {
                    return Err(invalid());
                }
                let bytes = STANDARD.decode(wav_base64).map_err(|_| invalid())?;
                wav_duration(&bytes)?;
            }
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewInput {
    expected: Expected,
    capability: MediaCapability,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteInput {
    binding_id: String,
    request_id: String,
    operation: Operation,
}

fn err(status: StatusCode, code: &'static str, message: &'static str) -> MediaError {
    MediaError {
        status,
        code,
        message,
    }
}
fn invalid() -> MediaError {
    err(
        StatusCode::BAD_REQUEST,
        "media_provider_invalid",
        "The media operation or its input is invalid.",
    )
}
fn unsupported(message: &'static str) -> MediaError {
    err(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "media_provider_unsupported",
        message,
    )
}
fn stale() -> MediaError {
    err(
        StatusCode::CONFLICT,
        "media_provider_stale",
        "Review this media operation again for the current connection and model.",
    )
}
fn cancelled() -> MediaError {
    err(StatusCode::CONFLICT,"media_provider_cancelled","Media operation cancelled. The selected server may still finish a request already received; no result was saved.")
}
fn response_error() -> MediaError {
    err(
        StatusCode::UNPROCESSABLE_ENTITY,
        "media_provider_response_invalid",
        "The selected server returned an invalid or unsupported media response.",
    )
}
fn limit() -> MediaError {
    err(
        StatusCode::PAYLOAD_TOO_LARGE,
        "media_provider_limit",
        "The media request or response exceeds its resource limit.",
    )
}
fn uncertain() -> MediaError {
    err(
        StatusCode::CONFLICT,
        "media_provider_outcome_unknown",
        "The selected server may have processed this request. It was not retried automatically.",
    )
}
fn valid_id(id: &str) -> MediaResult<()> {
    if uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id) {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn text_limit(text: &str, max: usize) -> MediaResult<()> {
    if text.is_empty() {
        return Err(invalid());
    }
    if text.len() > max {
        return Err(limit());
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(invalid());
    }
    Ok(())
}

pub fn begin_shutdown(app: &App) {
    local_transcription::begin_shutdown(app);
    if let Ok(mut inner) = app.media_runtime.inner.lock() {
        inner.closed = true;
        for job in inner.jobs.values_mut() {
            job.cancelled = true;
        }
    }
}
fn validate_binding(app: &App, binding: &Binding) -> MediaResult<()> {
    if app.media_runtime.inner.lock().map_err(|_| stale())?.closed {
        return Err(cancelled());
    }
    task_model::validate_selection(app, &binding.selection).map_err(|_| stale())?;
    if app.auth.lock().map_err(|_| stale())?.revision != binding.auth_revision {
        return Err(stale());
    }
    Ok(())
}
fn binding(app: &App, id: &str, capability: MediaCapability) -> MediaResult<Arc<Binding>> {
    let selected = app
        .media_runtime
        .inner
        .lock()
        .map_err(|_| stale())?
        .bindings
        .get(id)
        .cloned()
        .ok_or_else(stale)?;
    if selected.capability != capability {
        return Err(stale());
    }
    validate_binding(app, &selected)?;
    Ok(selected)
}
async fn review(
    State(app): State<Arc<App>>,
    input: Result<Json<ReviewInput>, JsonRejection>,
) -> MediaResult<Json<Value>> {
    let Json(input) = input.map_err(|_| invalid())?;
    let selected = task_model::selection(&app, &input.expected).map_err(|_| stale())?;
    text_limit(&selected.config.model, 256)?;
    if selected.config.provider != "openai" {
        return Err(unsupported("Choose and test an OpenAI-compatible local or private LAN server for this explicit media operation."));
    }
    let bound = Arc::new(Binding {
        id: crate::id(),
        capability: input.capability,
        selection: selected,
        auth_revision: app.auth.lock().map_err(|_| stale())?.revision,
    });
    let mut inner = app.media_runtime.inner.lock().map_err(|_| stale())?;
    if inner.closed {
        return Err(cancelled());
    }
    inner.bindings.retain(|_, existing| {
        existing.selection.binding_id == bound.selection.binding_id
            && existing.auth_revision == bound.auth_revision
    });
    if let Some(existing) = inner
        .bindings
        .values()
        .find(|existing| existing.capability == bound.capability)
    {
        return Ok(Json(
            json!({"binding_id":existing.id,"capability":existing.capability,"provenance":existing.selection.provenance,"verified_capability":false,"notice":"No media input has been sent by this review."}),
        ));
    }
    if inner.bindings.len() >= 8 {
        return Err(limit());
    }
    if !inner.adapter_registered {
        app.agent
            .register_adapter(Arc::new(ProviderTools {
                app: Arc::downgrade(&app),
            }))
            .map_err(|_| stale())?;
        inner.adapter_registered = true;
    }
    inner.bindings.insert(bound.id.clone(), bound.clone());
    Ok(Json(
        json!({"binding_id":bound.id,"capability":bound.capability,"provenance":bound.selection.provenance,"verified_capability":false,"notice":"Review is permission for this operation and target. Text connection testing does not prove media capability. Nothing has been sent yet."}),
    ))
}
fn start(
    app: &App,
    id: &str,
    bound: &Binding,
    operation: &Operation,
) -> MediaResult<Option<Value>> {
    valid_id(id)?;
    operation.validate()?;
    validate_binding(app, bound)?;
    let fingerprint = smith_media::sha256(
        &serde_json::to_vec(&json!({"binding":bound.id,"operation":operation}))
            .map_err(|_| invalid())?,
    );
    let mut inner = app.media_runtime.inner.lock().map_err(|_| stale())?;
    if inner.closed {
        return Err(cancelled());
    }
    if let Some(job) = inner.jobs.get(id) {
        if job.fingerprint != fingerprint {
            return Err(stale());
        }
        return match &job.result {
            Some(Ok(result)) => Ok(Some(result.clone())),
            Some(Err(e)) => Err(copy_error(e)),
            None => Err(uncertain()),
        };
    }
    if inner.jobs.len() >= 32 || inner.jobs.values().filter(|j| j.result.is_none()).count() >= 2 {
        return Err(limit());
    }
    inner.jobs.insert(
        id.into(),
        Job {
            fingerprint,
            cancelled: false,
            result: None,
        },
    );
    Ok(None)
}
fn copy_error(e: &MediaError) -> MediaError {
    err(e.status, e.code, e.message)
}
fn check_job(app: &App, id: &str, bound: &Binding) -> MediaResult<()> {
    validate_binding(app, bound)?;
    let inner = app.media_runtime.inner.lock().map_err(|_| stale())?;
    if inner.closed || inner.jobs.get(id).is_none_or(|j| j.cancelled) {
        return Err(cancelled());
    }
    Ok(())
}
fn finish(app: &App, id: &str, result: &MediaResult<Value>) {
    if let Ok(mut inner) = app.media_runtime.inner.lock() {
        if let Some(job) = inner.jobs.get_mut(id) {
            job.result = Some(match result {
                Ok(value) => Ok(value.clone()),
                Err(error) => Err(copy_error(error)),
            });
        }
    }
}
async fn execute(
    State(app): State<Arc<App>>,
    input: Result<Json<ExecuteInput>, JsonRejection>,
) -> MediaResult<Json<Value>> {
    let Json(input) = input.map_err(|_| invalid())?;
    let bound = binding(&app, &input.binding_id, input.operation.capability())?;
    if let Some(value) = start(&app, &input.request_id, &bound, &input.operation)? {
        return Ok(Json(value));
    }
    let runner = app.clone();
    // Registration precedes blocking work. Request cancellation cannot detach
    // untracked plaintext buffers or DB ownership during profile lock.
    app.spawn_task(async move {
        tokio::task::spawn_blocking(move || {
            let result = perform(
                &runner,
                &bound,
                &input.request_id,
                &input.operation,
                &mut || Ok(()),
                None,
            );
            finish(&runner, &input.request_id, &result);
            result
        })
        .await
        .map_err(|_| uncertain())?
    })
    .await
    .map_err(|_| uncertain())?
    .map(Json)
}
async fn cancel(State(app): State<Arc<App>>, Path(id): Path<String>) -> MediaResult<Json<Value>> {
    let mut inner = app.media_runtime.inner.lock().map_err(|_| stale())?;
    if let Some(job) = inner.jobs.get_mut(&id) {
        job.cancelled = true;
    }
    Ok(Json(json!({"cancelled":true})))
}
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .merge(local_transcription::routes())
        .route("/api/media/provider/review", post(review))
        .route(
            "/api/media/provider/execute",
            post(execute).layer(DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        .route("/api/media/provider/cancel/{id}", post(cancel))
}

enum Prepared {
    Json {
        path: &'static str,
        body: Value,
    },
    Wav {
        bytes: Vec<u8>,
        language: String,
        duration: f64,
    },
}
fn prepare(app: &App, bound: &Binding, operation: &Operation) -> MediaResult<Prepared> {
    operation.validate()?;
    let model = &bound.selection.config.model;
    Ok(match operation {
        Operation::EmbedText { text } => Prepared::Json {
            path: "embeddings",
            body: json!({"model":model,"input":[text],"encoding_format":"float"}),
        },
        Operation::GenerateImage {
            prompt,
            width,
            height,
            ..
        } => Prepared::Json {
            path: "images/generations",
            body: json!({"model":model,"prompt":prompt,"n":1,"size":format!("{width}x{height}"),"response_format":"b64_json"}),
        },
        Operation::Vision { prompt, media_id } => {
            let (bytes, manifest) =
                media_api::host_original(&*app.db.lock().map_err(|_| stale())?, media_id)?;
            if !matches!(
                manifest.content,
                smith_media::Content::Image { frames: 1, .. }
            ) || !matches!(
                manifest.format,
                smith_media::Format::Png | smith_media::Format::Jpeg | smith_media::Format::WebP
            ) {
                return Err(unsupported("Vision requires one validated PNG, JPEG or WebP image explicitly selected by ID."));
            }
            if bytes.len() > 2 * 1024 * 1024 {
                return Err(limit());
            }
            Prepared::Json {
                path: "chat/completions",
                body: json!({"model":model,"stream":false,"max_tokens":1024,"messages":[{"role":"user","content":[{"type":"text","text":prompt},{"type":"image_url","image_url":{"url":format!("data:{};base64,{}",manifest.detected_mime,STANDARD.encode(bytes))}}]}]}),
            }
        }
        Operation::TranscribeAudio {
            wav_base64,
            language,
        } => {
            let bytes = STANDARD.decode(wav_base64).map_err(|_| invalid())?;
            let duration = wav_duration(&bytes)?;
            Prepared::Wav {
                bytes,
                language: language.clone(),
                duration,
            }
        }
    })
}
fn admitted_bearer(app: &App, bound: &Binding) -> MediaResult<Option<axum::http::HeaderValue>> {
    let state = app.media_runtime.inner.lock().map_err(|_| stale())?;
    if state.closed {
        return Err(cancelled());
    }
    // A reviewed operation must never capture a replacement account's token.
    // Snapshot admission under the same authority locks used for mutations;
    // transport uses only this captured value after releasing these locks.
    let consent = app.consent.lock().map_err(|_| stale())?;
    let config = app.config.lock().map_err(|_| stale())?;
    let auth = app.auth.lock().map_err(|_| stale())?;
    crate::validate_expected(&consent, &config, &bound.selection.expected).map_err(|_| stale())?;
    if *config != bound.selection.config
        || auth.revision != bound.auth_revision
        || app.tested.lock().map_err(|_| stale())?.as_ref() != Some(&*config)
    {
        return Err(stale());
    }
    Ok(auth
        .secret
        .as_ref()
        .filter(|s| s.target_provider == config.provider && s.target_endpoint == config.endpoint)
        .map(|s| s.bearer.clone()))
}
async fn transfer(app: &App, bound: &Binding, prepared: Prepared) -> MediaResult<Value> {
    let bearer = admitted_bearer(app, bound)?;
    let (request, duration) = match prepared {
        Prepared::Json { path, body } => (
            crate::provider_request(
                &bound.selection.config,
                reqwest::Method::POST,
                path,
                bearer.as_ref(),
            )
            .map_err(|_| stale())?
            .json(&body),
            None,
        ),
        Prepared::Wav {
            bytes,
            language,
            duration,
        } => {
            let part = reqwest::multipart::Part::bytes(bytes)
                .file_name("selected.wav")
                .mime_str("audio/wav")
                .map_err(|_| invalid())?;
            let mut form = reqwest::multipart::Form::new()
                .text("model", bound.selection.config.model.clone())
                .text("response_format", "verbose_json")
                .part("file", part);
            if language != "auto" {
                form = form.text("language", language);
            }
            (
                crate::provider_request(
                    &bound.selection.config,
                    reqwest::Method::POST,
                    "audio/transcriptions",
                    bearer.as_ref(),
                )
                .map_err(|_| stale())?
                .multipart(form),
                Some(duration),
            )
        }
    };
    let response = request.send().await.map_err(|_| uncertain())?;
    if !response.status().is_success() {
        return Err(unsupported(
            "The selected server rejected this media capability or request. No fallback was used.",
        ));
    }
    let maximum = if bound.capability == MediaCapability::GenerateImage {
        12 * 1024 * 1024
    } else {
        256 * 1024
    };
    if response
        .content_length()
        .is_some_and(|n| n > maximum as u64)
    {
        return Err(limit());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| uncertain())?;
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(limit());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| response_error())?;
    if let Some(seconds) = duration {
        validate_transcript(value, seconds)
    } else {
        Ok(value)
    }
}
fn perform(
    app: &Arc<App>,
    bound: &Arc<Binding>,
    request_id: &str,
    operation: &Operation,
    active: &mut dyn FnMut() -> smith_agent_core::Result<()>,
    host: Option<&HostToolContext<'_>>,
) -> MediaResult<Value> {
    let prepared = prepare(app, bound, operation)?;
    check_job(app, request_id, bound)?;
    active().map_err(|_| cancelled())?;
    let runtime = tokio::runtime::Handle::current();
    let response=runtime.block_on(async{
        let transfer=transfer(app,bound,prepared);tokio::pin!(transfer);
        let deadline=tokio::time::sleep(Duration::from_secs(60));tokio::pin!(deadline);
        let mut tick=tokio::time::interval(Duration::from_millis(50));
        loop {tokio::select!{biased;
            _=tick.tick()=>{check_job(app,request_id,bound)?;active().map_err(|_|cancelled())?;},
            _=&mut deadline=>return Err(uncertain()),
            result=&mut transfer=>return result,
        }}
    })?;
    check_job(app, request_id, bound)?;
    active().map_err(|_| cancelled())?;
    let provenance = json!({"binding_id":bound.id,"capability":bound.capability,"selected":bound.selection.provenance,"transport":"openai_compatible","fallback":false});
    let mut result = match operation {
        Operation::EmbedText { .. } => validate_embedding(response, &bound.selection.config.model)?,
        Operation::Vision { .. } => {
            if response["choices"].as_array().is_none_or(|a| a.len() != 1)
                || response["choices"][0]["finish_reason"] != "stop"
            {
                return Err(response_error());
            }
            let text = response["choices"][0]["message"]["content"]
                .as_str()
                .ok_or_else(response_error)?;
            text_limit(text, 32 * 1024)?;
            json!({"text":text,"pixels_sent":true})
        }
        Operation::TranscribeAudio { .. } => response,
        Operation::GenerateImage {
            width,
            height,
            name,
            ..
        } => {
            if response["data"].as_array().is_none_or(|a| a.len() != 1)
                || response["data"][0].get("url").is_some()
            {
                return Err(response_error());
            }
            let encoded = response["data"][0]["b64_json"]
                .as_str()
                .ok_or_else(response_error)?;
            if encoded.len() > 11 * 1024 * 1024 {
                return Err(limit());
            }
            let bytes = STANDARD.decode(encoded).map_err(|_| response_error())?;
            let mut request = smith_media::AttachmentRequest::new(name, None);
            request
                .metadata
                .insert("provider_binding".into(), bound.id.clone());
            request.metadata.insert(
                "provider_model".into(),
                bound.selection.config.model.clone(),
            );
            request.metadata.insert(
                "provider_endpoint".into(),
                bound.selection.config.endpoint.clone(),
            );
            request
                .metadata
                .insert("provider".into(), bound.selection.config.provider.clone());
            request
                .metadata
                .insert("operation".into(), "generate_image".into());
            let manifest = smith_media::inspect(request, &bytes, &smith_media::Limits::default())?;
            if !matches!(manifest.content,smith_media::Content::Image{width:w,height:h,frames:1,..} if w==*width&&h==*height)
                || manifest.format != smith_media::Format::Png
            {
                return Err(response_error());
            }
            // This mutex serializes shutdown/cancel with the complete DB commit.
            let guard = app.media_runtime.inner.lock().map_err(|_| stale())?;
            if guard.closed || guard.jobs.get(request_id).is_none_or(|j| j.cancelled) {
                return Err(cancelled());
            }
            // Keep target and credential mutation serialized through this commit,
            // as well as cancellation/profile shutdown. These are the same
            // authority locks used by the configured-target mutation handlers.
            let consent = app.consent.lock().map_err(|_| stale())?;
            let config = app.config.lock().map_err(|_| stale())?;
            let auth = app.auth.lock().map_err(|_| stale())?;
            crate::validate_expected(&consent, &config, &bound.selection.expected)
                .map_err(|_| stale())?;
            if *config != bound.selection.config
                || auth.revision != bound.auth_revision
                || app.tested.lock().map_err(|_| stale())?.as_ref() != Some(&*config)
            {
                return Err(stale());
            }
            if let Some(context) = host {
                crate::agent_store::AgentStore::new(app)
                    .map_err(|_| stale())?
                    .effect(context, |tx| {
                        let mut saved = media_api::save_generated_in_transaction(
                            tx, request_id, None, None, bytes, manifest,
                        )
                        .map_err(core_error)?;
                        saved["provenance"] = provenance.clone();
                        Ok(saved)
                    })
                    .map_err(|_| uncertain())?
            } else {
                let mut db = app.db.lock().map_err(|_| stale())?;
                let tx = db.transaction()?;
                let value = media_api::save_generated_in_transaction(
                    &tx, request_id, None, None, bytes, manifest,
                )?;
                tx.commit()?;
                value
            }
        }
    };
    result["provenance"] = provenance;
    Ok(result)
}
fn validate_embedding(value: Value, model: &str) -> MediaResult<Value> {
    if value["data"].as_array().is_none_or(|a| a.len() != 1)
        || value["data"][0]["index"] != 0
        || value["model"].as_str() != Some(model)
    {
        return Err(response_error());
    }
    let vector = value["data"][0]["embedding"]
        .as_array()
        .ok_or_else(response_error)?;
    if vector.is_empty() || vector.len() > 4096 {
        return Err(limit());
    }
    if vector
        .iter()
        .any(|n| n.as_f64().is_none_or(|x| !x.is_finite()))
    {
        return Err(response_error());
    }
    Ok(json!({"dim":vector.len(),"embedding":vector}))
}
fn wav_duration(bytes: &[u8]) -> MediaResult<f64> {
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(limit());
    }
    if bytes.len() < 44
        || &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
        || u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8 != bytes.len()
    {
        return Err(invalid());
    }
    let mut at = 12;
    let mut format = false;
    let mut data = None;
    while at < bytes.len() {
        let h = bytes.get(at..at + 8).ok_or_else(invalid)?;
        let len = u32::from_le_bytes(h[4..8].try_into().unwrap()) as usize;
        at += 8;
        let chunk = bytes.get(at..at + len).ok_or_else(invalid)?;
        match &h[..4] {
            b"fmt " => {
                if format || len != 16 {
                    return Err(unsupported(
                        "Only PCM16 mono 16 kHz WAV input is supported.",
                    ));
                }
                let n = |i| u16::from_le_bytes(chunk[i..i + 2].try_into().unwrap());
                if n(0) != 1
                    || n(2) != 1
                    || u32::from_le_bytes(chunk[4..8].try_into().unwrap()) != 16000
                    || u32::from_le_bytes(chunk[8..12].try_into().unwrap()) != 32000
                    || n(12) != 2
                    || n(14) != 16
                {
                    return Err(unsupported(
                        "Only PCM16 mono 16 kHz WAV input is supported.",
                    ));
                }
                format = true;
            }
            b"data" => {
                if data.replace(len).is_some() {
                    return Err(invalid());
                }
            }
            _ => {
                return Err(unsupported(
                    "WAV metadata or additional chunks are not supported by this audio profile.",
                ))
            }
        };
        at = at.checked_add(len + len % 2).ok_or_else(invalid)?;
    }
    let count = data.ok_or_else(invalid)?;
    if !format || count == 0 || count % 2 != 0 || at != bytes.len() {
        return Err(invalid());
    }
    let duration = count as f64 / 32000.0;
    if duration > 60.0 {
        return Err(limit());
    }
    Ok(duration)
}
fn validate_transcript(value: Value, duration: f64) -> MediaResult<Value> {
    let text = value["text"].as_str().ok_or_else(response_error)?;
    text_limit(text, 64 * 1024)?;
    let language = value["language"].as_str().ok_or_else(response_error)?;
    text_limit(language, 64)?;
    let segments = value["segments"].as_array().ok_or_else(response_error)?;
    if segments.len() > 1000 {
        return Err(limit());
    }
    let mut previous = 0.0;
    let mut clean = Vec::new();
    let mut total = 0;
    let mut srt = String::new();
    let mut vtt = String::from("WEBVTT\n\n");
    for (i, segment) in segments.iter().enumerate() {
        let start = segment["start"].as_f64().ok_or_else(response_error)?;
        let end = segment["end"].as_f64().ok_or_else(response_error)?;
        let text = segment["text"].as_str().ok_or_else(response_error)?;
        text_limit(text, 8192)?;
        total += text.len();
        if total > 64 * 1024 {
            return Err(limit());
        }
        if !start.is_finite()
            || !end.is_finite()
            || start < previous
            || end < start
            || end > duration + 0.05
        {
            return Err(response_error());
        }
        previous = end;
        let stamp = |seconds: f64, sep: char| {
            let ms = (seconds * 1000.0).round() as u64;
            format!(
                "{:02}:{:02}:{:02}{sep}{:03}",
                ms / 3600000,
                (ms / 60000) % 60,
                (ms / 1000) % 60,
                ms % 1000
            )
        };
        // Subtitle cue syntax is a derived rendering. Preserve the original
        // transcript in segments while preventing provider text from creating
        // extra cues, timestamp lines or markup in SRT/WebVTT.
        let cue = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace(['\r', '\n'], " ");
        srt.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            stamp(start, ','),
            stamp(end, ','),
            cue
        ));
        vtt.push_str(&format!(
            "{} --> {}\n{}\n\n",
            stamp(start, '.'),
            stamp(end, '.'),
            cue
        ));
        clean.push(json!({"start":start,"end":end,"text":text}));
    }
    Ok(
        json!({"text":text,"language":language,"segments":clean,"srt":srt,"vtt":vtt,"audio_seconds":duration}),
    )
}
fn core_error(e: MediaError) -> Error {
    match e.code {
        "media_provider_outcome_unknown" => Error::UncertainSideEffect(e.message.into()),
        "media_provider_cancelled" | "media_provider_stale" => Error::Denied(e.message.into()),
        "media_provider_unsupported" => Error::Unsupported(e.message.into()),
        _ => Error::InvalidInput(e.message.into()),
    }
}

struct ProviderTools {
    app: Weak<App>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbedArgs {
    binding_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageArgs {
    binding_id: String,
    prompt: String,
    width: u32,
    height: u32,
    name: String,
}
fn tool_operation(call: &ToolCall) -> smith_agent_core::Result<(String, Operation)> {
    match call.name.as_str() {
        "embed_text" => {
            let a: EmbedArgs = serde_json::from_value(call.arguments.clone())
                .map_err(|_| Error::InvalidInput("embedding arguments".into()))?;
            Ok((a.binding_id, Operation::EmbedText { text: a.text }))
        }
        "generate_image" => {
            let a: ImageArgs = serde_json::from_value(call.arguments.clone())
                .map_err(|_| Error::InvalidInput("image arguments".into()))?;
            Ok((
                a.binding_id,
                Operation::GenerateImage {
                    prompt: a.prompt,
                    width: a.width,
                    height: a.height,
                    name: a.name,
                },
            ))
        }
        _ => Err(Error::Unsupported("media tool".into())),
    }
}
impl HostToolAdapter for ProviderTools {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        [
        ("embed_text",Capability::EmbedText,json!({"type":"object","properties":{"binding_id":{"type":"string"},"text":{"type":"string"}},"required":["binding_id","text"],"additionalProperties":false})),
        ("generate_image",Capability::GenerateImage,json!({"type":"object","properties":{"binding_id":{"type":"string"},"prompt":{"type":"string"},"width":{"type":"integer"},"height":{"type":"integer"},"name":{"type":"string"}},"required":["binding_id","prompt","width","height","name"],"additionalProperties":false}))
    ].into_iter().map(|(name,capability,parameters)|ToolDescriptor{name:name.into(),source:"host:reviewed_media_provider".into(),parameters,availability:Availability::Implemented,required_capability:Some(capability),requires_approval:true}).collect()
    }
    fn validate(&self, grant: &ExecutionGrant, call: &ToolCall) -> smith_agent_core::Result<()> {
        let (id, operation) = tool_operation(call)?;
        let capability = if matches!(operation, Operation::EmbedText { .. }) {
            Capability::EmbedText
        } else {
            Capability::GenerateImage
        };
        if !grant.permits(capability) {
            return Err(Error::Denied("media capability is not granted".into()));
        }
        operation.validate().map_err(core_error)?;
        let manifest = grant.manifest();
        if !manifest
            .resources
            .iter()
            .any(|s| s.kind == ResourceKind::ProviderBinding && s.id == id)
        {
            return Err(Error::Denied(
                "exact reviewed media provider binding required".into(),
            ));
        }
        if matches!(operation, Operation::GenerateImage { .. })
            && !manifest
                .resources
                .iter()
                .any(|s| s.kind == ResourceKind::ArtifactCollection && s.id == "unowned")
        {
            return Err(Error::Denied("unowned artifact collection required".into()));
        }
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| Error::Denied("profile closed".into()))?;
        binding(&app, &id, operation.capability()).map_err(core_error)?;
        Ok(())
    }
    fn execute(
        &self,
        context: &HostToolContext<'_>,
        check_active: &mut dyn FnMut() -> smith_agent_core::Result<()>,
    ) -> smith_agent_core::Result<Value> {
        self.validate(context.grant, context.call)?;
        check_active()?;
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| Error::Denied("profile closed".into()))?;
        let (id, operation) = tool_operation(context.call)?;
        let bound = binding(&app, &id, operation.capability()).map_err(core_error)?;
        let digest =
            smith_media::sha256(format!("{}:{}", context.task_id, context.call.id).as_bytes());
        let request_id = format!(
            "{}-{}-4{}-8{}-{}",
            &digest[..8],
            &digest[8..12],
            &digest[13..16],
            &digest[17..20],
            &digest[20..32]
        );
        if let Some(value) = start(&app, &request_id, &bound, &operation).map_err(core_error)? {
            return Ok(value);
        }
        let result = perform(
            &app,
            &bound,
            &request_id,
            &operation,
            check_active,
            Some(context),
        );
        finish(&app, &request_id, &result);
        result.map_err(core_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn wav() -> Vec<u8> {
        let mut out = b"RIFF".to_vec();
        out.extend(32036u32.to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(16000u32.to_le_bytes());
        out.extend(32000u32.to_le_bytes());
        out.extend(2u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend(32000u32.to_le_bytes());
        out.resize(32044, 0);
        out
    }
    #[test]
    fn wav_pcm_budget_rejects_spoofs_extra_chunks_and_wrong_rate() {
        assert_eq!(wav_duration(&wav()).unwrap(), 1.0);
        let mut wrong = wav();
        wrong[24..28].copy_from_slice(&48000u32.to_le_bytes());
        assert_eq!(
            wav_duration(&wrong).unwrap_err().code,
            "media_provider_unsupported"
        );
        for bytes in [b"RIFFffffWAVE".to_vec(), vec![0; 3 * 1024 * 1024]] {
            assert!(wav_duration(&bytes).is_err());
        }
        let mut extra = wav();
        extra.extend(b"LIST\0\0\0\0");
        let len = extra.len() as u32 - 8;
        extra[4..8].copy_from_slice(&len.to_le_bytes());
        assert!(wav_duration(&extra).is_err());
    }
    #[test]
    fn embedding_output_is_one_finite_vector_for_exact_model() {
        let good = json!({"model":"fixture","data":[{"index":0,"embedding":[0.125,-2.5,3]}]});
        assert_eq!(
            validate_embedding(good.clone(), "fixture").unwrap()["dim"],
            3
        );
        assert!(validate_embedding(good.clone(), "other").is_err());
        for vector in [
            json!([]),
            json!(["NaN"]),
            json!([null]),
            json!(vec![0; 4097]),
        ] {
            let mut bad = good.clone();
            bad["data"][0]["embedding"] = vector;
            assert!(validate_embedding(bad, "fixture").is_err());
        }
    }
    #[test]
    fn transcript_timestamps_are_validated_and_rendered_without_fallback() {
        let good = json!({"text":"Привет","language":"ru","segments":[{"start":0.125,"end":0.875,"text":"Привет"}]});
        let output = validate_transcript(good.clone(), 1.0).unwrap();
        assert_eq!(
            output["srt"],
            "1\n00:00:00,125 --> 00:00:00,875\nПривет\n\n"
        );
        assert!(output["vtt"].as_str().unwrap().starts_with("WEBVTT"));
        for end in [-1.0, 2.0] {
            let mut bad = good.clone();
            bad["segments"][0]["end"] = json!(end);
            assert!(validate_transcript(bad, 1.0).is_err());
        }
        let mut markup = good;
        let original = "<b>A & B</b>\n\n00:00:00.000 --> 99:00:00.000";
        markup["segments"][0]["text"] = json!(original);
        let output = validate_transcript(markup, 1.0).unwrap();
        assert_eq!(output["segments"][0]["text"], original);
        assert!(!output["vtt"].as_str().unwrap().contains("<b>"));
        assert_eq!(output["vtt"].as_str().unwrap().matches("-->").count(), 1);
    }
    #[test]
    fn media_tool_arguments_cannot_add_steps_urls_or_capabilities() {
        for value in [
            json!({"kind":"generate_image","prompt":"x","width":256,"height":256,"name":"x.png","steps":20}),
            json!({"kind":"vision","prompt":"x","media_id":"http://example.invalid/image.png"}),
            json!({"kind":"embed_text","text":"x","approved":true}),
        ] {
            let parsed = serde_json::from_value::<Operation>(value);
            assert!(parsed.is_err() || parsed.unwrap().validate().is_err());
        }
    }
    async fn fixture(delay: Duration) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let router=Router::new().route("/v1/embeddings",post(move||{let count=count.clone();async move{count.fetch_add(1,Ordering::SeqCst);tokio::time::sleep(delay).await;Json(json!({"model":"fixture","data":[{"index":0,"embedding":[0.125,-0.5]}]}))}}))
            .route("/v1/images/generations",post(||async{Json(json!({"data":[{"b64_json":STANDARD.encode(include_bytes!("../crates/smith-media/tests/fixtures/provider-256.png"))}]}))}))
            .route("/v1/chat/completions",post(||async{Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"Synthetic pixels acknowledged"}}]}))}))
            .route("/v1/audio/transcriptions",post(||async{Json(json!({"text":"Fixture","language":"en","segments":[{"start":0.0,"end":0.5,"text":"Fixture"}]}))}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (endpoint, calls, task)
    }
    fn app_for(endpoint: String) -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app(&dir.path().join("fixture.db")).unwrap();
        let mut config = app.config.lock().unwrap().clone();
        config.provider = "openai".into();
        config.endpoint = endpoint;
        config.model = "fixture".into();
        *app.config.lock().unwrap() = config.clone();
        *app.tested.lock().unwrap() = Some(config);
        *app.models.lock().unwrap() = vec![json!({"id":"fixture"})];
        *app.models_revision.lock().unwrap() = Some(app.consent.lock().unwrap().token());
        (dir, app)
    }
    async fn reviewed(app: &Arc<App>, capability: MediaCapability) -> String {
        let config = app.config.lock().unwrap().clone();
        let expected = Expected {
            revision: app.consent.lock().unwrap().token(),
            provider: config.provider,
            endpoint: config.endpoint,
        };
        review(
            State(app.clone()),
            Ok(Json(ReviewInput {
                expected,
                capability,
            })),
        )
        .await
        .unwrap()
        .0["binding_id"]
            .as_str()
            .unwrap()
            .into()
    }
    #[tokio::test]
    async fn host_grants_require_exact_capability_scope_and_fresh_review_after_restart() {
        use smith_agent_core::{Principal, ResourceScope, Workspace};
        let (_dir, app) = app_for("http://127.0.0.1:11434/v1".into());
        let embedding = reviewed(&app, MediaCapability::EmbedText).await;
        assert_eq!(reviewed(&app, MediaCapability::EmbedText).await, embedding);
        let image = reviewed(&app, MediaCapability::GenerateImage).await;
        let adapter = ProviderTools {
            app: Arc::downgrade(&app),
        };
        assert!(adapter.descriptors().iter().all(|d| d.requires_approval));
        let grant = |caps: Vec<Capability>, ids: Vec<(ResourceKind, &str)>| {
            ExecutionGrant::new_scoped(
                Principal::new("synthetic-host").unwrap(),
                Arc::new(Workspace::create_temporary().unwrap()),
                "fixture",
                caps,
                ids.into_iter().map(|(kind, id)| ResourceScope {
                    kind,
                    id: id.into(),
                }),
            )
            .unwrap()
        };
        let call = ToolCall::new(
            "embed_text",
            json!({"binding_id":embedding,"text":"synthetic"}),
        );
        let allowed = grant(
            vec![Capability::EmbedText],
            vec![(ResourceKind::ProviderBinding, &embedding)],
        );
        adapter.validate(&allowed, &call).unwrap();
        assert!(adapter
            .validate(
                &grant(vec![], vec![(ResourceKind::ProviderBinding, &embedding)]),
                &call
            )
            .is_err());
        assert!(adapter
            .validate(
                &grant(
                    vec![Capability::EmbedText],
                    vec![(ResourceKind::ProviderBinding, &image)]
                ),
                &call
            )
            .is_err());
        let image_call = ToolCall::new(
            "generate_image",
            json!({"binding_id":image,"prompt":"synthetic","width":256,"height":256,"name":"fixture.png"}),
        );
        assert!(adapter
            .validate(
                &grant(
                    vec![Capability::GenerateImage],
                    vec![(ResourceKind::ProviderBinding, &image)]
                ),
                &image_call
            )
            .is_err());
        adapter
            .validate(
                &grant(
                    vec![Capability::GenerateImage],
                    vec![
                        (ResourceKind::ProviderBinding, &image),
                        (ResourceKind::ArtifactCollection, "unowned"),
                    ],
                ),
                &image_call,
            )
            .unwrap();
        app.config.lock().unwrap().model = "changed".into();
        assert!(adapter.validate(&allowed, &call).is_err());
        app.config.lock().unwrap().model = "fixture".into();
        app.auth.lock().unwrap().revision += 1;
        assert!(adapter.validate(&allowed, &call).is_err());
        let refreshed = reviewed(&app, MediaCapability::EmbedText).await;
        assert_ne!(refreshed, embedding);
        let (_next_dir, next) = app_for("http://127.0.0.1:11434/v1".into());
        assert_ne!(reviewed(&next, MediaCapability::EmbedText).await, refreshed);
        assert!(ProviderTools {
            app: Arc::downgrade(&next)
        }
        .validate(&allowed, &call)
        .is_err());
        begin_shutdown(&app);
        assert!(binding(&app, &refreshed, MediaCapability::EmbedText).is_err());
    }

    #[tokio::test]
    async fn request_admission_cannot_capture_credentials_replaced_after_review() {
        let (_dir, app) = app_for("http://127.0.0.1:11434/v1".into());
        let config = app.config.lock().unwrap().clone();
        app.auth.lock().unwrap().secret = Some(crate::SessionAuth {
            target_provider: config.provider.clone(),
            target_endpoint: config.endpoint.clone(),
            bearer: axum::http::HeaderValue::from_static("Bearer synthetic-first"),
        });
        let id = reviewed(&app, MediaCapability::EmbedText).await;
        let bound = binding(&app, &id, MediaCapability::EmbedText).unwrap();
        let captured = admitted_bearer(&app, &bound).unwrap().unwrap();
        {
            let mut auth = app.auth.lock().unwrap();
            auth.revision += 1;
            auth.secret.as_mut().unwrap().bearer =
                axum::http::HeaderValue::from_static("Bearer synthetic-replacement");
        }
        assert_eq!(captured, "Bearer synthetic-first");
        assert_eq!(
            admitted_bearer(&app, &bound).unwrap_err().code,
            "media_provider_stale"
        );
        begin_shutdown(&app);
        assert_eq!(
            admitted_bearer(&app, &bound).unwrap_err().code,
            "media_provider_cancelled"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn selected_fixture_transports_persist_image_and_never_implicitly_send_references() {
        let (endpoint, calls, server) = fixture(Duration::ZERO).await;
        let (_dir, app) = app_for(endpoint);
        let binding = reviewed(&app, MediaCapability::EmbedText).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let request_id = crate::id();
        let operation = Operation::EmbedText {
            text: "synthetic input".into(),
        };
        let first = execute(
            State(app.clone()),
            Ok(Json(ExecuteInput {
                binding_id: binding.clone(),
                request_id: request_id.clone(),
                operation: operation.clone(),
            })),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(first["dim"], 2);
        let second = execute(
            State(app.clone()),
            Ok(Json(ExecuteInput {
                binding_id: binding,
                request_id,
                operation,
            })),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let binding = reviewed(&app, MediaCapability::GenerateImage).await;
        let result = execute(
            State(app.clone()),
            Ok(Json(ExecuteInput {
                binding_id: binding,
                request_id: crate::id(),
                operation: Operation::GenerateImage {
                    prompt: "synthetic image".into(),
                    width: 256,
                    height: 256,
                    name: "fixture.png".into(),
                },
            })),
        )
        .await
        .unwrap()
        .0;
        let media_id = result["document"]["id"].as_str().unwrap().to_owned();
        let (bytes, manifest) =
            media_api::host_original(&app.db.lock().unwrap(), &media_id).unwrap();
        assert_eq!(
            bytes,
            include_bytes!("../crates/smith-media/tests/fixtures/provider-256.png")
        );
        assert_eq!(manifest.request.metadata["provider_model"], "fixture");
        assert!(media_api::host_read_document(&app.db.lock().unwrap(), &media_id).is_err());
        let binding = reviewed(&app, MediaCapability::Vision).await;
        let result = execute(
            State(app.clone()),
            Ok(Json(ExecuteInput {
                binding_id: binding,
                request_id: crate::id(),
                operation: Operation::Vision {
                    prompt: "read these pixels explicitly".into(),
                    media_id,
                },
            })),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["pixels_sent"], true);
        let binding = reviewed(&app, MediaCapability::TranscribeAudio).await;
        let result = execute(
            State(app.clone()),
            Ok(Json(ExecuteInput {
                binding_id: binding,
                request_id: crate::id(),
                operation: Operation::TranscribeAudio {
                    wav_base64: STANDARD.encode(wav()),
                    language: "en".into(),
                },
            })),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["segments"][0]["end"], 0.5);
        server.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn profile_shutdown_cancels_transport_and_stale_capability_never_sends() {
        let (endpoint, calls, server) = fixture(Duration::from_secs(2)).await;
        let (_dir, app) = app_for(endpoint);
        let binding = reviewed(&app, MediaCapability::EmbedText).await;
        let wrong = execute(
            State(app.clone()),
            Ok(Json(ExecuteInput {
                binding_id: binding.clone(),
                request_id: crate::id(),
                operation: Operation::GenerateImage {
                    prompt: "x".into(),
                    width: 256,
                    height: 256,
                    name: "x.png".into(),
                },
            })),
        )
        .await
        .unwrap_err();
        assert_eq!(wrong.code, "media_provider_stale");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let runner = app.clone();
        let task = tokio::spawn(async move {
            execute(
                State(runner),
                Ok(Json(ExecuteInput {
                    binding_id: binding,
                    request_id: crate::id(),
                    operation: Operation::EmbedText {
                        text: "synthetic".into(),
                    },
                })),
            )
            .await
        });
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        begin_shutdown(&app);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .code,
            "media_provider_cancelled"
        );
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM media_objects", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_cancel_and_target_or_auth_change_abort_pending_operations() {
        for mode in ["cancel", "target", "auth"] {
            let (endpoint, calls, server) = fixture(Duration::from_secs(2)).await;
            let (_dir, app) = app_for(endpoint);
            let binding_id = reviewed(&app, MediaCapability::EmbedText).await;
            let request_id = crate::id();
            let runner = app.clone();
            let running_id = request_id.clone();
            let job = tokio::spawn(async move {
                execute(
                    State(runner),
                    Ok(Json(ExecuteInput {
                        binding_id,
                        request_id: running_id,
                        operation: Operation::EmbedText {
                            text: "synthetic".into(),
                        },
                    })),
                )
                .await
            });
            for _ in 0..100 {
                if calls.load(Ordering::SeqCst) > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            match mode {
                "cancel" => {
                    assert_eq!(
                        cancel(State(app.clone()), Path(request_id))
                            .await
                            .unwrap()
                            .0["cancelled"],
                        true
                    );
                }
                "target" => app.config.lock().unwrap().model = "changed".into(),
                _ => app.auth.lock().unwrap().revision += 1,
            }
            let error = tokio::time::timeout(Duration::from_secs(1), job)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(
                error.code,
                if mode == "cancel" {
                    "media_provider_cancelled"
                } else {
                    "media_provider_stale"
                }
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn image_response_urls_malformed_bytes_and_wrong_dimensions_never_publish() {
        let router=Router::new().route("/v1/images/generations",post(|Json(input):Json<Value>|async move {
            Json(match input["prompt"].as_str().unwrap() {
                "url"=>json!({"data":[{"url":"http://127.0.0.1:1/never-fetch","b64_json":STANDARD.encode(include_bytes!("../crates/smith-media/tests/fixtures/provider-256.png"))}]}),
                "malformed"=>json!({"data":[{"b64_json":STANDARD.encode(b"not an image")}]}),
                _=>json!({"data":[{"b64_json":STANDARD.encode(include_bytes!("../crates/smith-media/tests/fixtures/provider-256.png"))}]}),
            })
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (_dir, app) = app_for(endpoint);
        let binding_id = reviewed(&app, MediaCapability::GenerateImage).await;
        for prompt in ["url", "malformed", "wrong dimensions"] {
            let error = execute(
                State(app.clone()),
                Ok(Json(ExecuteInput {
                    binding_id: binding_id.clone(),
                    request_id: crate::id(),
                    operation: Operation::GenerateImage {
                        prompt: prompt.into(),
                        width: 512,
                        height: 512,
                        name: "fixture.png".into(),
                    },
                })),
            )
            .await
            .unwrap_err();
            assert!(!error.message.contains("http://"));
            assert_eq!(
                app.db
                    .lock()
                    .unwrap()
                    .query_row("SELECT count(*) FROM media_objects", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        server.abort();
    }
}
