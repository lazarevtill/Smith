//! Explicitly selected, user-trusted whisper.cpp CLI execution. This adapter
//! never discovers/downloads assets or captures audio. A reviewed executable is
//! native code, not a sandboxed document; packaging checks do not prove its
//! behavior. Only the owned child and its process group are terminated.
use super::{validate_transcript, wav_duration};
#[path = "audio_normalization.rs"]
mod audio_normalization;
use crate::{
    media_api::{self, MediaError, MediaResult},
    App,
};
use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use smith_managed_runtime::Cancellation;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Semaphore,
};

const MAX_EXECUTABLE: u64 = 64 * 1024 * 1024;
const MAX_MODEL: u64 = 256 * 1024 * 1024;
const MAX_JSON: usize = 512 * 1024;
const MAX_JOBS: usize = 32;
static REVIEWS: Semaphore = Semaphore::const_new(1);

#[derive(Default)]
pub struct TranscriptionState {
    inner: Arc<Mutex<Inner>>,
}
#[derive(Default)]
struct Inner {
    closed: bool,
    reviews: BTreeMap<String, Arc<Review>>,
    jobs: BTreeMap<String, Job>,
    retained: Vec<CleanupOwner>,
    cleaning: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    len: u64,
    modified: i64,
    modified_ns: i64,
    changed: i64,
    changed_ns: i64,
}
#[derive(Clone, Debug)]
struct Asset {
    path: PathBuf,
    identity: Identity,
    name: String,
    sha256: String,
    byte_len: u64,
}
impl Asset {
    fn summary(&self) -> Value {
        json!({"name":self.name,"sha256":self.sha256,"byte_len":self.byte_len})
    }
}
#[derive(Clone)]
struct Review {
    id: String,
    executable: Asset,
    model: Asset,
    declared_multilingual: bool,
    normalizer: Option<Asset>,
}
impl Review {
    fn summary(&self) -> Value {
        json!({"review_id":self.id,"normalizer":self.normalizer.as_ref().map(|a|json!({"adapter":"apple_afconvert","executable":a.summary(),"formats":["pcm_wav","pcm_aiff","aac_m4a","alac_m4a"],"max_source_bytes":8388608,"max_audio_seconds":60,"decoder_wall_seconds":20,"notice":"Selected-file normalization uses the reviewed Apple system converter. Only bounded self-contained PCM WAV/AIFF and AAC-LC/ALAC M4A are admitted. No truncation, microphone, video, or URLs. Codec availability is checked by execution."})),"adapter":"whisper_cpp_cli","executable":self.executable.summary(),"model":self.model.summary(),"declared_multilingual":self.declared_multilingual,"allowed_languages":if self.declared_multilingual {vec!["en","ru","auto"]} else {vec!["en"]},"verified_model":false,"network_adapter":false,"requested_device":"cpu","limits":{"max_audio_bytes":2097152,"max_audio_seconds":60,"sample_rate":16000,"channels":1,"bits_per_sample":16,"model_bytes":MAX_MODEL,"wall_seconds":180,"threads":2},"notice":"Run only an executable and model you trust. Native code runs with your user permissions; packaging and hash checks do not sandbox it. This review reads files without executing them. Allowed languages reflect only the model header; no transcription accuracy has been verified."})
    }
}
struct Job {
    fingerprint: String,
    review: Arc<Review>,
    source: Value,
    cancellation: Cancellation,
    state: &'static str,
    result: Option<Value>,
    error: Option<MediaError>,
    saved: Option<Value>,
    save_fingerprint: Option<String>,
}
impl Job {
    fn snapshot(&self, id: &str) -> Value {
        json!({"request_id":id,"state":self.state,"source":self.source,"result":self.result,"error":self.error.as_ref().map(|e|json!({"code":e.code,"message":e.message})),"saved":self.saved,"provenance":{"adapter":"whisper_cpp_cli","executable":self.review.executable.summary(),"model":self.review.model.summary(),"requested_device":"cpu","real_model_verified":false,"normalizer":if self.source["normalization_requested"]==true {self.review.normalizer.as_ref().map(Asset::summary)}else{None}},"original_audio_retained":false})
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewInput {
    executable_path: String,
    model_path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput {
    review_id: String,
    request_id: String,
    name: String,
    wav_base64: String,
    language: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileStartInput {
    review_id: String,
    request_id: String,
    name: String,
    audio_base64: String,
    language: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SaveInput {
    request_id: String,
    title: String,
    format: String,
}

fn error(status: StatusCode, code: &'static str, message: &'static str) -> MediaError {
    MediaError {
        status,
        code,
        message,
    }
}
fn invalid() -> MediaError {
    error(
        StatusCode::BAD_REQUEST,
        "transcription_invalid",
        "Choose supported local assets, a valid WAV file, and a supported transcript format.",
    )
}
fn unsupported() -> MediaError {
    error(StatusCode::UNSUPPORTED_MEDIA_TYPE,"transcription_adapter_unsupported","Select a self-contained arm64 macOS whisper.cpp CLI with only system library imports and an existing Whisper GGML model no larger than 256 MiB.")
}
fn changed() -> MediaError {
    error(
        StatusCode::CONFLICT,
        "transcription_asset_changed",
        "A selected executable or model changed. Review both assets again before running.",
    )
}
fn cancelled() -> MediaError {
    error(
        StatusCode::CONFLICT,
        "transcription_cancelled",
        "Transcription cancelled; no transcript was saved.",
    )
}
fn unavailable() -> MediaError {
    error(
        StatusCode::CONFLICT,
        "transcription_unavailable",
        "The transcription review or job is unavailable in this workspace session.",
    )
}
fn failed() -> MediaError {
    error(StatusCode::UNPROCESSABLE_ENTITY,"transcription_failed","The selected CLI failed or returned unsupported transcript output. No transcript was saved.")
}
fn limit() -> MediaError {
    error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "transcription_limit",
        "The transcription input, output, or session resource limit was exceeded.",
    )
}
fn cleanup_failed() -> MediaError {
    error(StatusCode::INTERNAL_SERVER_ERROR,"transcription_cleanup_failed","The owned transcription process or scratch files could not be confirmed cleaned up. Do not start another job until the workspace restarts.")
}
fn io_error() -> MediaError {
    error(StatusCode::UNPROCESSABLE_ENTITY,"transcription_file_unavailable","A selected local file could not be read safely, or private transcription scratch could not be created.")
}
fn valid_id(id: &str) -> MediaResult<()> {
    super::valid_id(id).map_err(|_| invalid())
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
}
fn active(cancel: &Cancellation) -> MediaResult<()> {
    if cancel.is_cancelled() {
        Err(cancelled())
    } else {
        Ok(())
    }
}

pub fn begin_shutdown(app: &App) {
    if let Ok(mut inner) = app.media_runtime.local_transcription.inner.lock() {
        inner.closed = true;
        for job in inner.jobs.values() {
            job.cancellation.cancel();
        }
    }
}
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/media/transcription/review", post(review))
        .route("/api/media/transcription/cleanup", post(retry_cleanup))
        .route(
            "/api/media/transcription/start",
            post(start).layer(DefaultBodyLimit::max(3 * 1024 * 1024)),
        )
        .route(
            "/api/media/transcription/start-file",
            post(start_file).layer(DefaultBodyLimit::max(12 * 1024 * 1024)),
        )
        .route("/api/media/transcription/jobs/{id}", get(status))
        .route("/api/media/transcription/jobs/{id}/cancel", post(cancel))
        .route("/api/media/transcription/jobs/{id}/save", post(save))
}
async fn review(
    State(app): State<Arc<App>>,
    input: Result<Json<ReviewInput>, JsonRejection>,
) -> MediaResult<Json<Value>> {
    let Json(input) = input.map_err(|_| invalid())?;
    let permit = REVIEWS.try_acquire().map_err(|_| {
        error(
            StatusCode::CONFLICT,
            "transcription_busy",
            "Another asset review is running.",
        )
    })?;
    {
        let inner = app
            .media_runtime
            .local_transcription
            .inner
            .lock()
            .map_err(|_| unavailable())?;
        if inner.closed {
            return Err(cancelled());
        }
        if inner.reviews.len() >= 8 {
            return Err(limit());
        }
    }
    // TaskTracker owns the blocking hash operation even when the HTTP future is
    // cancelled during profile shutdown. No DB handle is taken by that worker.
    let worker = app.spawn_task(async move {
        let _permit = permit;
        tokio::task::spawn_blocking(move || prepare_review(&input))
            .await
            .map_err(|_| failed())?
    });
    let selected = Arc::new(worker.await.map_err(|_| failed())??);
    let mut inner = app
        .media_runtime
        .local_transcription
        .inner
        .lock()
        .map_err(|_| unavailable())?;
    if inner.closed {
        return Err(cancelled());
    }
    if inner.reviews.len() >= 8 {
        return Err(limit());
    }
    let summary = selected.summary();
    inner.reviews.insert(selected.id.clone(), selected);
    Ok(Json(summary))
}
fn prepare_review(input: &ReviewInput) -> MediaResult<Review> {
    let cancellation = Cancellation::new();
    let (executable, header) = read_asset(
        FsPath::new(&input.executable_path),
        MAX_EXECUTABLE,
        None,
        &cancellation,
    )?;
    inspect_executable(&header)?;
    let (model, header) = read_asset(
        FsPath::new(&input.model_path),
        MAX_MODEL,
        None,
        &cancellation,
    )?;
    inspect_model(&header)?;
    if executable.path == model.path {
        return Err(invalid());
    }
    Ok(Review {
        id: crate::id(),
        executable,
        model,
        // whisper.cpp's GGML vocabulary declares multilingual support at
        // 51865 tokens. The CLI silently rewrites other language options to
        // English below that threshold, so reject those before admission.
        declared_multilingual: word(&header, 4)? >= 51865,
        normalizer: trusted_normalizer(&cancellation).ok(),
    })
}
async fn start(
    State(app): State<Arc<App>>,
    input: Result<Json<StartInput>, JsonRejection>,
) -> MediaResult<Json<Value>> {
    let Json(input) = input.map_err(|_| invalid())?;
    start_common(app, input, false)
}
async fn start_file(
    State(app): State<Arc<App>>,
    input: Result<Json<FileStartInput>, JsonRejection>,
) -> MediaResult<Json<Value>> {
    let Json(input) = input.map_err(|_| invalid())?;
    start_common(
        app,
        StartInput {
            review_id: input.review_id,
            request_id: input.request_id,
            name: input.name,
            wav_base64: input.audio_base64,
            language: input.language,
        },
        true,
    )
}
fn audio_error(e: audio_normalization::AudioError) -> MediaError {
    match e { audio_normalization::AudioError::Limit => limit(), audio_normalization::AudioError::Unsupported => error(StatusCode::UNSUPPORTED_MEDIA_TYPE,"transcription_audio_unsupported","Select supported self-contained PCM WAV/AIFF or AAC-LC/ALAC M4A, at most 8 MiB and 60 seconds. Video, external references, unsupported metadata and codecs are refused.") }
}
fn start_common(app: Arc<App>, input: StartInput, normalize: bool) -> MediaResult<Json<Value>> {
    valid_id(&input.request_id)?;
    valid_id(&input.review_id)?;
    if !valid_name(&input.name)
        || (!normalize && !input.name.to_ascii_lowercase().ends_with(".wav"))
        || input.wav_base64.len()
            > if normalize {
                12 * 1024 * 1024
            } else {
                3 * 1024 * 1024
            }
        || !matches!(input.language.as_str(), "en" | "ru" | "auto")
    {
        return Err(invalid());
    }
    let bytes = STANDARD.decode(&input.wav_base64).map_err(|_| invalid())?;
    let normalized = if normalize {
        Some(audio_normalization::inspect(&input.name, &bytes).map_err(audio_error)?)
    } else {
        None
    };
    let duration = if let Some(audio) = normalized {
        audio.seconds
    } else {
        wav_duration(&bytes)
            .map_err(|_| audio_error(audio_normalization::AudioError::Unsupported))?
    };
    let mut source = json!({"name":input.name,"sha256":smith_media::sha256(&bytes),"byte_len":bytes.len(),"audio_seconds":duration});
    if let Some(audio) = normalized {
        source["format"] = json!(audio.format.name());
        source["channels"] = json!(audio.channels);
        source["normalization_requested"] = json!(true);
    }
    let fingerprint = smith_media::sha256(
        serde_json::to_string(
            &json!({"review":input.review_id,"source":source,"language":input.language}),
        )
        .map_err(|_| invalid())?
        .as_bytes(),
    );
    let mut inner = app
        .media_runtime
        .local_transcription
        .inner
        .lock()
        .map_err(|_| unavailable())?;
    if inner.closed {
        return Err(cancelled());
    }
    if let Some(job) = inner.jobs.get(&input.request_id) {
        if job.fingerprint != fingerprint {
            return Err(error(
                StatusCode::CONFLICT,
                "transcription_request_conflict",
                "This request ID belongs to another transcription.",
            ));
        }
        return Ok(Json(job.snapshot(&input.request_id)));
    }
    if inner.cleaning
        || !inner.retained.is_empty()
        || inner
            .jobs
            .values()
            .any(|j| matches!(j.state, "running" | "cancelling" | "cleanup_failed"))
    {
        return Err(error(
            StatusCode::CONFLICT,
            "transcription_busy",
            "Finish or cancel the current local transcription first.",
        ));
    }
    if inner.jobs.len() >= MAX_JOBS {
        return Err(limit());
    }
    let selected = inner
        .reviews
        .get(&input.review_id)
        .cloned()
        .ok_or_else(unavailable)?;
    if !selected.declared_multilingual && input.language != "en" {
        return Err(error(StatusCode::UNSUPPORTED_MEDIA_TYPE,"transcription_language_unsupported","The selected model declares English-only support. Select English or review a multilingual model; the requested language will not be changed automatically."));
    }
    if normalize && selected.normalizer.is_none() {
        return Err(error(StatusCode::CONFLICT,"transcription_normalizer_unavailable","The Apple system audio converter is unavailable. Review again, or select canonical PCM16 mono 16 kHz WAV without normalization."));
    }
    let cancellation = Cancellation::new();
    inner.jobs.insert(
        input.request_id.clone(),
        Job {
            fingerprint,
            review: selected.clone(),
            source,
            cancellation: cancellation.clone(),
            state: "running",
            result: None,
            error: None,
            saved: None,
            save_fingerprint: None,
        },
    );
    let snapshot = inner.jobs[&input.request_id].snapshot(&input.request_id);
    let tracked = app.clone();
    app.spawn_task(async move {
        let completion = run_prepared(
            selected,
            bytes,
            input.language,
            duration,
            cancellation.clone(),
            Duration::from_secs(180),
            normalized,
        )
        .await;
        if let Ok(mut inner) = tracked.media_runtime.local_transcription.inner.lock() {
            let closed = inner.closed;
            let retained = completion.owner.is_some();
            if let Some(owner) = completion.owner {
                inner.retained.push(owner);
            }
            if let Some(job) = inner.jobs.get_mut(&input.request_id) {
                match completion.result {
                    Err(e) if retained => {
                        job.state = "cleanup_failed";
                        job.error = Some(e);
                    }
                    _ if closed || cancellation.is_cancelled() => {
                        job.state = "cancelled";
                        job.error = Some(cancelled());
                    }
                    Ok(output) => {
                        job.state = "completed";
                        job.result = Some(output);
                    }
                    Err(e) => {
                        job.state = "failed";
                        job.error = Some(e);
                    }
                }
            }
        }
    });
    Ok(Json(snapshot))
}
async fn status(State(app): State<Arc<App>>, Path(id): Path<String>) -> MediaResult<Json<Value>> {
    valid_id(&id)?;
    let inner = app
        .media_runtime
        .local_transcription
        .inner
        .lock()
        .map_err(|_| unavailable())?;
    if inner.closed {
        return Err(cancelled());
    }
    Ok(Json(
        inner.jobs.get(&id).ok_or_else(unavailable)?.snapshot(&id),
    ))
}
async fn cancel(State(app): State<Arc<App>>, Path(id): Path<String>) -> MediaResult<Json<Value>> {
    valid_id(&id)?;
    let mut inner = app
        .media_runtime
        .local_transcription
        .inner
        .lock()
        .map_err(|_| unavailable())?;
    if inner.closed {
        return Err(cancelled());
    }
    let job = inner.jobs.get_mut(&id).ok_or_else(unavailable)?;
    if job.saved.is_some() {
        return Err(error(
            StatusCode::CONFLICT,
            "transcription_already_saved",
            "This transcript was already saved. Cancelling cannot remove its artifact.",
        ));
    }
    job.cancellation.cancel();
    job.result = None;
    if job.state == "running" {
        job.state = "cancelling";
    } else if job.state != "cleanup_failed" {
        job.state = "cancelled";
        job.error = Some(cancelled());
    }
    Ok(Json(job.snapshot(&id)))
}
async fn save(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Result<Json<SaveInput>, JsonRejection>,
) -> MediaResult<Json<Value>> {
    let Json(input) = input.map_err(|_| invalid())?;
    valid_id(&id)?;
    valid_id(&input.request_id)?;
    if !valid_name(&input.title)
        || !matches!(
            input.format.as_str(),
            "text" | "timestamped" | "srt" | "vtt"
        )
    {
        return Err(invalid());
    }
    let fingerprint = smith_media::sha256(&serde_json::to_vec(&input).map_err(|_| invalid())?);
    // This authority lock serializes cancellation/profile closure with the entire
    // transaction, so a late completion cannot publish after revocation.
    let mut inner = app
        .media_runtime
        .local_transcription
        .inner
        .lock()
        .map_err(|_| unavailable())?;
    if inner.closed {
        return Err(cancelled());
    }
    let job = inner.jobs.get_mut(&id).ok_or_else(unavailable)?;
    if let Some(saved) = &job.saved {
        if job.save_fingerprint.as_ref() != Some(&fingerprint) {
            return Err(error(
                StatusCode::CONFLICT,
                "transcription_save_conflict",
                "This transcript was already saved with a different request, title, or format.",
            ));
        }
        let mut saved = saved.clone();
        saved["duplicate"] = json!(true);
        return Ok(Json(saved));
    }
    if job.cancellation.is_cancelled() || job.state != "completed" {
        return Err(unavailable());
    }
    let result = job.result.as_ref().ok_or_else(unavailable)?;
    let payload = json!({"schema":"smith.transcript.v1","title":input.title,"format":input.format,"transcript":result,"source":job.source,"original_audio_retained":false,"provenance":{"adapter":"whisper_cpp_cli","executable":job.review.executable.summary(),"model":job.review.model.summary(),"requested_device":"cpu","job_id":id,"language_quality_verified":false}});
    let bytes = serde_json::to_vec_pretty(&payload).map_err(|_| failed())?;
    let mut request = smith_media::AttachmentRequest::new(
        format!("{}.transcript.json", input.title),
        Some("application/json"),
    );
    request
        .metadata
        .insert("operation".into(), "local_transcription".into());
    request.metadata.insert(
        "source_sha256".into(),
        job.source["sha256"].as_str().ok_or_else(failed)?.into(),
    );
    request.metadata.insert("transcription_job".into(), id);
    let manifest = smith_media::inspect(request, &bytes, &smith_media::Limits::default())?;
    let mut db = app.db.lock().map_err(|_| unavailable())?;
    let tx = db.transaction()?;
    let saved = media_api::save_generated_in_transaction(
        &tx,
        &input.request_id,
        None,
        None,
        bytes,
        manifest,
    )?;
    tx.commit()?;
    job.save_fingerprint = Some(fingerprint);
    job.saved = Some(saved.clone());
    Ok(Json(saved))
}

#[cfg(unix)]
fn identity(metadata: &fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    Identity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        len: metadata.len(),
        modified: metadata.mtime(),
        modified_ns: metadata.mtime_nsec(),
        changed: metadata.ctime(),
        changed_ns: metadata.ctime_nsec(),
    }
}
#[cfg(not(unix))]
fn identity(metadata: &fs::Metadata) -> Identity {
    Identity {
        dev: 0,
        ino: 0,
        len: metadata.len(),
        modified: 0,
        modified_ns: 0,
        changed: 0,
        changed_ns: 0,
    }
}
fn read_asset(
    path: &FsPath,
    max: u64,
    mut destination: Option<&mut dyn Write>,
    cancel: &Cancellation,
) -> MediaResult<(Asset, Vec<u8>)> {
    let spelling = path
        .to_str()
        .filter(|s| s.len() <= 4096 && !s.chars().any(char::is_control))
        .ok_or_else(invalid)?;
    if !path.is_absolute()
        || spelling.ends_with('/')
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(invalid());
    }
    let canonical = fs::canonicalize(path).map_err(|_| io_error())?;
    if canonical.as_os_str() != path.as_os_str() {
        return Err(io_error());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).map_err(|_| io_error())?;
    let before = file.metadata().map_err(|_| io_error())?;
    if !before.is_file() || before.len() == 0 || before.len() > max {
        return Err(limit());
    }
    let before = identity(&before);
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut header = Vec::new();
    let mut block = [0u8; 64 * 1024];
    loop {
        active(cancel)?;
        let n = file.read(&mut block).map_err(|_| io_error())?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max {
            return Err(limit());
        }
        hash.update(&block[..n]);
        if header.len() < 1024 * 1024 {
            let keep = n.min(1024 * 1024 - header.len());
            header.extend_from_slice(&block[..keep]);
        }
        if let Some(output) = destination.as_deref_mut() {
            output.write_all(&block[..n]).map_err(|_| io_error())?;
        }
    }
    let after = identity(&file.metadata().map_err(|_| io_error())?);
    if before != after || total != before.len {
        return Err(changed());
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| valid_name(n))
        .ok_or_else(invalid)?
        .to_owned();
    Ok((
        Asset {
            path: canonical,
            identity: after,
            name,
            sha256: hash.finalize().iter().map(|b| format!("{b:02x}")).collect(),
            byte_len: total,
        },
        header,
    ))
}
fn word(bytes: &[u8], at: usize) -> MediaResult<u32> {
    bytes
        .get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(unsupported)
}
fn inspect_executable(bytes: &[u8]) -> MediaResult<()> {
    // Thin arm64 MH_EXECUTE only; no universal slices, scripts, @rpath or injected
    // DYLD environment commands. Dynamic behavior remains trusted native code.
    if !cfg!(all(target_os = "macos", target_arch = "aarch64"))
        || word(bytes, 0)? != 0xfeedfacf
        || word(bytes, 4)? != 0x0100000c
        || word(bytes, 12)? != 2
    {
        return Err(unsupported());
    }
    let count = word(bytes, 16)? as usize;
    let size = word(bytes, 20)? as usize;
    if count == 0 || count > 1024 || size > 1024 * 1024 - 32 || bytes.len() < size + 32 {
        return Err(unsupported());
    }
    let mut at = 32;
    let end = 32 + size;
    let mut system_imports = 0;
    for _ in 0..count {
        let command = word(bytes, at)?;
        let len = word(bytes, at + 4)? as usize;
        if len < 8 || len % 8 != 0 || at + len > end {
            return Err(unsupported());
        }
        if matches!(command, 0x8000001c | 0x27) {
            return Err(unsupported());
        }
        if matches!(
            command,
            0xc | 0x80000018 | 0x8000001f | 0x20 | 0x80000023 | 0xe
        ) {
            let offset = word(bytes, at + 8)? as usize;
            if offset < 12 || offset >= len {
                return Err(unsupported());
            }
            let raw = &bytes[at + offset..at + len];
            let nul = raw.iter().position(|b| *b == 0).ok_or_else(unsupported)?;
            let name = std::str::from_utf8(&raw[..nul]).map_err(|_| unsupported())?;
            if name.contains("/../")
                || name.contains("/./")
                || name.chars().any(char::is_control)
                || !(name.starts_with("/usr/lib/")
                    || name.starts_with("/System/Library/Frameworks/"))
            {
                return Err(unsupported());
            }
            if command == 0xe && name != "/usr/lib/dyld" {
                return Err(unsupported());
            }
            system_imports += 1;
        }
        at += len;
    }
    if at != end || system_imports == 0 {
        return Err(unsupported());
    }
    Ok(())
}
fn inspect_model(header: &[u8]) -> MediaResult<()> {
    // GGML Whisper v1 header only. Bounds prevent absurd dimension claims before
    // invoking the selected native parser; full tensor semantics belong to it.
    if header.get(..4) != Some(b"lmgg") || header.len() < 48 {
        return Err(unsupported());
    }
    let dimensions: Vec<u32> = (1..=10)
        .map(|i| word(header, i * 4))
        .collect::<MediaResult<_>>()?;
    let [vocab, audio_ctx, audio_state, audio_heads, audio_layers, text_ctx, text_state, text_heads, text_layers, mels] =
        dimensions[..]
    else {
        return Err(unsupported());
    };
    if !(1..=52000).contains(&vocab)
        || !(1..=1500).contains(&audio_ctx)
        || !(1..=1536).contains(&audio_state)
        || !(1..=24).contains(&audio_heads)
        || audio_state % audio_heads != 0
        || !(1..=32).contains(&audio_layers)
        || !(1..=448).contains(&text_ctx)
        || !(1..=1536).contains(&text_state)
        || !(1..=24).contains(&text_heads)
        || text_state % text_heads != 0
        || !(1..=32).contains(&text_layers)
        || !matches!(mels, 80 | 128)
        || word(header, 44)? > 10000
    {
        return Err(unsupported());
    }
    Ok(())
}
struct Scratch(PathBuf);
impl Scratch {
    fn create() -> MediaResult<Self> {
        let base = fs::canonicalize(std::env::temp_dir()).map_err(|_| io_error())?;
        let path = base.join(format!("smith-transcription-{}", crate::id()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|_| io_error())?;
        Ok(Self(path))
    }
    fn new_file(&self, name: &str) -> MediaResult<File> {
        let mut o = OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        o.open(self.0.join(name)).map_err(|_| io_error())
    }
    fn cleanup(&self) -> MediaResult<()> {
        match fs::remove_dir_all(&self.0) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(cleanup_failed()),
        }
    }
}
// There is intentionally no best-effort Drop that silently discards cleanup
// ownership. Every created scratch is held by a CleanupOwner until removal is
// confirmed; the profile closure gate retains errors and can retry them.
struct CleanupOwner {
    scratch: Arc<Scratch>,
    child: Option<tokio::process::Child>,
    pid: Option<u32>,
    group_pending: bool,
    #[cfg(test)]
    signal_attempts: usize,
    #[cfg(test)]
    wait_fault_once: bool,
}
impl CleanupOwner {
    fn new(scratch: Scratch) -> Self {
        Self {
            scratch: Arc::new(scratch),
            child: None,
            pid: None,
            group_pending: false,
            #[cfg(test)]
            signal_attempts: 0,
            #[cfg(test)]
            wait_fault_once: false,
        }
    }
    async fn stop_child(&mut self) -> MediaResult<Option<std::process::ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        if self.group_pending {
            // The leader has NEVER been waited/reaped yet, so its PID/PGID
            // cannot have been reused. Never signal this group after any wait.
            #[cfg(unix)]
            {
                let pid = self.pid.ok_or_else(cleanup_failed)?;
                #[cfg(test)]
                {
                    self.signal_attempts += 1;
                }
                let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                if result != 0 {
                    let errno = std::io::Error::last_os_error().raw_os_error();
                    // Darwin reports EPERM for a group containing only an
                    // unreaped zombie. Accept that case only after observing
                    // our exact exited leader and proving it is the sole group
                    // member. A denied signal to any live/unknown member keeps
                    // ownership retained instead of treating EPERM as success.
                    if errno != Some(libc::ESRCH)
                        && !(errno == Some(libc::EPERM) && sole_exited_group_member(pid))
                    {
                        return Err(cleanup_failed());
                    }
                }
            }
            #[cfg(test)]
            {
                self.signal_attempts += 1;
            }
            let _ = child.start_kill();
            self.group_pending = false;
        }
        #[cfg(test)]
        if self.wait_fault_once {
            self.wait_fault_once = false;
            return Err(cleanup_failed());
        }
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .map_err(|_| cleanup_failed())?
            .map_err(|_| cleanup_failed())?;
        self.child = None;
        self.pid = None;
        Ok(Some(status))
    }
    async fn cleanup(&mut self) -> MediaResult<()> {
        self.stop_child().await?;
        self.scratch.cleanup()
    }
}
#[cfg(target_os = "macos")]
fn sole_exited_group_member(pid: u32) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let observed = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if observed != 0 || info.si_pid != pid as i32 {
        return false;
    }
    // Two fixed slots suffice: exactly one matching member is the only
    // accepted result. Full/truncated/error lists are refused. proc_listpgrppids
    // returns a PID count, although its buffer size argument is bytes.
    let mut members = [0i32; 2];
    let count = unsafe {
        libc::proc_listpgrppids(
            pid as i32,
            members.as_mut_ptr().cast(),
            std::mem::size_of_val(&members) as i32,
        )
    };
    count == 1 && members[0] == pid as i32
}
#[cfg(not(target_os = "macos"))]
fn sole_exited_group_member(_: u32) -> bool {
    false
}
struct Completion {
    result: MediaResult<Value>,
    owner: Option<CleanupOwner>,
}
async fn finish_owner(mut owner: CleanupOwner, result: MediaResult<Value>) -> Completion {
    if owner.cleanup().await.is_err() {
        Completion {
            result: Err(cleanup_failed()),
            owner: Some(owner),
        }
    } else {
        Completion {
            result,
            owner: None,
        }
    }
}
/// Called after TaskTracker drains. An error keeps the entire App/profile alive
/// with its exact unresolved child/scratch owner; closing the DB is not success.
pub async fn shutdown(app: &App) -> Result<(), String> {
    let state = app.media_runtime.local_transcription.inner.clone();
    app.spawn_task(async move { cleanup_state(state).await })
        .await
        .map_err(|_| {
            "Transcription cleanup task failed; workspace ownership remains retained.".to_owned()
        })?
}
async fn cleanup_state(state: Arc<Mutex<Inner>>) -> Result<(), String> {
    let mut owners = {
        let mut inner = state
            .lock()
            .map_err(|_| "Transcription cleanup state is unavailable.".to_owned())?;
        if inner.cleaning
            || inner
                .jobs
                .values()
                .any(|job| matches!(job.state, "running" | "cancelling"))
        {
            return Err("Local transcription work has not finished draining.".into());
        }
        inner.cleaning = true;
        std::mem::take(&mut inner.retained)
    };
    let mut pending = Vec::new();
    for mut owner in owners.drain(..) {
        if owner.cleanup().await.is_err() {
            pending.push(owner);
        }
    }
    let mut inner = state
        .lock()
        .map_err(|_| "Transcription cleanup state is unavailable.".to_owned())?;
    inner.cleaning = false;
    inner.retained.extend(pending);
    if !inner.retained.is_empty() {
        return Err("Local transcription child or scratch cleanup is not confirmed. Workspace ownership is retained; retry cleanup before closing.".into());
    }
    for job in inner.jobs.values_mut() {
        if job.state == "cleanup_failed" {
            job.state = "failed";
            job.error = Some(error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "transcription_cleanup_recovered",
                "Cleanup is now confirmed. The previous transcription was not saved.",
            ));
        }
    }
    Ok(())
}
async fn retry_cleanup(State(app): State<Arc<App>>) -> MediaResult<Json<Value>> {
    {
        let inner = app
            .media_runtime
            .local_transcription
            .inner
            .lock()
            .map_err(|_| unavailable())?;
        if inner.closed {
            return Err(cancelled());
        }
    }
    let tracked = app.clone();
    app.spawn_task(async move { shutdown(&tracked).await })
        .await
        .map_err(|_| cleanup_failed())?
        .map_err(|_| cleanup_failed())?;
    Ok(Json(json!({"cleanup_confirmed":true})))
}
fn snapshot_in(
    scratch: &Scratch,
    selected: &Review,
    bytes: &[u8],
    cancel: &Cancellation,
) -> MediaResult<()> {
    snapshot_assets(scratch, selected, cancel)?;
    scratch
        .new_file("selected.wav")?
        .write_all(bytes)
        .map_err(|_| io_error())?;
    active(cancel)
}
fn snapshot_assets(scratch: &Scratch, selected: &Review, cancel: &Cancellation) -> MediaResult<()> {
    for (asset, name, max) in [
        (&selected.executable, "whisper-cli", MAX_EXECUTABLE),
        (&selected.model, "model.bin", MAX_MODEL),
    ] {
        active(cancel)?;
        let mut out = scratch.new_file(name)?;
        let (copied, header) = read_asset(&asset.path, max, Some(&mut out), cancel)?;
        if copied.identity != asset.identity || copied.sha256 != asset.sha256 {
            return Err(changed());
        }
        if name == "whisper-cli" {
            inspect_executable(&header)?;
        } else {
            inspect_model(&header)?;
        }
        out.sync_all().map_err(|_| io_error())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            out.set_permissions(fs::Permissions::from_mode(if name == "whisper-cli" {
                0o500
            } else {
                0o400
            }))
            .map_err(|_| io_error())?;
        }
    }
    active(cancel)
}
#[cfg(test)]
fn snapshot(selected: &Review, bytes: &[u8], cancel: &Cancellation) -> MediaResult<Scratch> {
    let scratch = Scratch::create()?;
    if let Err(error) = snapshot_in(&scratch, selected, bytes, cancel) {
        scratch.cleanup()?;
        return Err(error);
    }
    Ok(scratch)
}
async fn exit_unreaped(pid: u32) -> MediaResult<()> {
    #[cfg(target_os = "macos")]
    loop {
        // SAFETY: waitid receives a properly initialized writable siginfo_t;
        // WNOWAIT observes only our direct child and preserves its PID custody.
        let observed = {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::EINTR) {
                    continue;
                }
                return Err(error(StatusCode::INTERNAL_SERVER_ERROR,"transcription_child_custody_lost","The child wait identity could not be verified. No further process signals will be sent; cleanup ownership is retained."));
            }
            info.si_pid == pid as i32
        };
        if observed {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        Err(unsupported())
    }
}
async fn drain<R: AsyncRead + Unpin>(mut stream: R) -> MediaResult<()> {
    let mut total = 0;
    let mut bytes = [0u8; 4096];
    loop {
        let n = stream.read(&mut bytes).await.map_err(|_| failed())?;
        if n == 0 {
            return Ok(());
        }
        total += n;
        if total > 128 * 1024 {
            return Err(limit());
        }
    }
}
fn trusted_normalizer(cancel: &Cancellation) -> MediaResult<Asset> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        // Apple's protected executable cannot be relocated without its platform
        // signature killing the copy. Admit only this fixed canonical OS path,
        // with root-owned, non-writable, non-symlink ancestors. Privileged OS
        // replacement and system dylibs are the trusted OS dependency boundary.
        for path in ["/", "/usr", "/usr/bin", "/usr/bin/afconvert"] {
            let m = fs::symlink_metadata(path).map_err(|_| io_error())?;
            if m.file_type().is_symlink()
                || m.uid() != 0
                || m.mode() & 0o022 != 0
                || (path != "/usr/bin/afconvert" && !m.is_dir())
            {
                return Err(io_error());
            }
        }
        let (asset, _) = read_asset(
            FsPath::new("/usr/bin/afconvert"),
            4 * 1024 * 1024,
            None,
            cancel,
        )?;
        Ok(asset)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cancel;
        Err(unsupported())
    }
}
fn encoded_name(format: audio_normalization::Format) -> &'static str {
    match format {
        audio_normalization::Format::Wav => "encoded.wav",
        audio_normalization::Format::Aiff => "encoded.aiff",
        audio_normalization::Format::Aac | audio_normalization::Format::Alac => "encoded.m4a",
    }
}
fn canonical_decoder_wave(bytes: &[u8]) -> MediaResult<Vec<u8>> {
    if wav_duration(bytes).is_ok() {
        return Ok(bytes.to_vec());
    }
    // Apple's AAC/ALAC decoder writes WAVE_FORMAT_EXTENSIBLE even for mono
    // integer PCM. Admit only its exact PCM16/16k/mono shape and PCM GUID, then
    // reserialize the header. Samples are copied byte-for-byte, never trimmed.
    if bytes.len() < 68
        || bytes.len() > 2 * 1024 * 1024
        || bytes.get(..4) != Some(b"RIFF")
        || bytes.get(8..16) != Some(b"WAVEfmt ")
        || word(bytes, 4)? as usize + 8 != bytes.len()
        || word(bytes, 16)? != 40
        || bytes.get(60..64) != Some(b"data")
        || word(bytes, 64)? as usize + 68 != bytes.len()
    {
        return Err(failed());
    }
    let fmt = &bytes[20..60];
    if fmt[..20]
        != [
            0xfe, 0xff, 1, 0, 0x80, 0x3e, 0, 0, 0, 0x7d, 0, 0, 2, 0, 16, 0, 22, 0, 16, 0,
        ]
        || !matches!(word(fmt, 20)?, 0 | 4)
        || fmt[24..] != [1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]
    {
        return Err(failed());
    }
    let data = &bytes[68..];
    let mut canonical = b"RIFF".to_vec();
    canonical.extend((data.len() as u32 + 36).to_le_bytes());
    canonical.extend(b"WAVEfmt ");
    canonical.extend(16u32.to_le_bytes());
    canonical.extend(1u16.to_le_bytes());
    canonical.extend(&fmt[2..16]);
    canonical.extend(b"data");
    canonical.extend((data.len() as u32).to_le_bytes());
    canonical.extend(data);
    wav_duration(&canonical).map_err(|_| failed())?;
    Ok(canonical)
}
async fn normalize_owned(
    owner: &mut CleanupOwner,
    decoder: &Asset,
    audio: audio_normalization::Audio,
    cancel: &Cancellation,
) -> MediaResult<Value> {
    let check_cancel = cancel.clone();
    let expected = decoder.clone();
    tokio::task::spawn_blocking(move || {
        let current = trusted_normalizer(&check_cancel)?;
        if current.identity != expected.identity || current.sha256 != expected.sha256 {
            return Err(changed());
        }
        Ok(())
    })
    .await
    .map_err(|_| failed())??;
    active(cancel)?;
    let mut command = Command::new("/usr/bin/afconvert");
    command.args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"]);
    // Apple's converter rejects --mix for mono-to-mono conversion (-50).
    // Enable the fixed downmix option only for validated stereo source audio.
    if audio.channels == 2 {
        command.arg("--mix");
    }
    command.args(["--no-filler", encoded_name(audio.format), "decoded.wav"]);
    run_command(owner,command,cancel,Duration::from_secs(20),2*1024*1024,20).await.map_err(|e|{
        if matches!(e.code,"transcription_failed"|"transcription_spawn_failed") {
            error(StatusCode::UNPROCESSABLE_ENTITY,"transcription_decode_failed","The Apple converter could not decode this file within the supported bounds. The required system codec may be unavailable. No partial audio or transcript was accepted.")
        } else {e}
    })?;
    let mut bytes = Vec::new();
    let (decoded, _) = read_asset(
        &owner.scratch.0.join("decoded.wav"),
        2 * 1024 * 1024,
        Some(&mut bytes),
        cancel,
    )?;
    let bytes = canonical_decoder_wave(&bytes)?;
    let mut output = owner.scratch.new_file("selected.wav")?;
    output.write_all(&bytes).map_err(|_| io_error())?;
    output.sync_all().map_err(|_| io_error())?;
    let normalized_sha256 = smith_media::sha256(&bytes);
    let duration = wav_duration(&bytes)
        .map_err(|_| audio_error(audio_normalization::AudioError::Unsupported))?;
    // Priming/padding is already validated by the source preflight. Accept
    // only rounding at the sample-rate boundary, never an incomplete decode.
    let tolerance = 0.002;
    if (duration - audio.seconds).abs() > tolerance {
        return Err(error(StatusCode::UNPROCESSABLE_ENTITY,"transcription_decode_failed","Decoded duration disagrees with the bounded source duration; no partial audio was accepted."));
    }
    Ok(
        json!({"adapter":"apple_afconvert","executable":decoder.summary(),"source_format":audio.format.name(),"declared_source_seconds":audio.seconds,"decoder_output":{"sha256":decoded.sha256,"byte_len":decoded.byte_len},"normalized":{"sha256":normalized_sha256,"byte_len":bytes.len(),"audio_seconds":duration,"sample_rate":16000,"channels":1,"bits_per_sample":16},"requested_conversion":{"sample_rate":16000,"channels":1,"bits_per_sample":16,"stereo_downmix":audio.channels==2},"truncated":false,"audio_bytes_retained":false,"system_dependency_boundary":"Verified fixed root-owned /usr/bin/afconvert and operating-system codec libraries; no relocated executable or external codec is selected."}),
    )
}
async fn run_prepared(
    selected: Arc<Review>,
    bytes: Vec<u8>,
    language: String,
    duration: f64,
    cancel: Cancellation,
    deadline: Duration,
    normalization: Option<audio_normalization::Audio>,
) -> Completion {
    let scratch = match Scratch::create() {
        Ok(s) => s,
        Err(e) => {
            return Completion {
                result: Err(e),
                owner: None,
            }
        }
    };
    let mut owner = CleanupOwner::new(scratch);
    let copy_cancel = cancel.clone();
    let snapshot = owner.scratch.clone();
    let normalizer = selected.normalizer.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        if let Some(audio) = normalization {
            snapshot_assets(&snapshot, &selected, &copy_cancel)?;
            snapshot
                .new_file(encoded_name(audio.format))?
                .write_all(&bytes)
                .map_err(|_| io_error())?;
            active(&copy_cancel)
        } else {
            snapshot_in(&snapshot, &selected, &bytes, &copy_cancel)
        }
    })
    .await
    .map_err(|_| failed())
    .and_then(|r| r);
    if let Err(error) = prepared {
        return finish_owner(owner, Err(error)).await;
    }
    let result = async {
        let metadata = if let Some(audio) = normalization {
            Some(
                normalize_owned(
                    &mut owner,
                    normalizer.as_ref().ok_or_else(unavailable)?,
                    audio,
                    &cancel,
                )
                .await?,
            )
        } else {
            None
        };
        let duration = metadata
            .as_ref()
            .and_then(|m| m["normalized"]["audio_seconds"].as_f64())
            .unwrap_or(duration);
        let mut result =
            run_child_owned(&mut owner, &language, duration, &cancel, deadline).await?;
        if let Some(metadata) = metadata {
            result["normalization"] = metadata;
        }
        Ok(result)
    }
    .await;
    // A failed final wait retains the live child and scratch without deleting
    // files or relying on kill_on_drop. Only explicit cleanup retries own it.
    if owner.child.is_some() {
        return Completion {
            result: Err(cleanup_failed()),
            owner: Some(owner),
        };
    }
    finish_owner(owner, result).await
}
async fn run_child_owned(
    owner: &mut CleanupOwner,
    language: &str,
    duration: f64,
    cancel: &Cancellation,
    deadline: Duration,
) -> MediaResult<Value> {
    active(cancel)?;
    let mut command = Command::new(owner.scratch.0.join("whisper-cli"));
    command.args([
        "-m",
        "model.bin",
        "-f",
        "selected.wav",
        "-l",
        language,
        "-np",
        "-oj",
        "-of",
        "transcript",
        "-ng",
        "-t",
        "2",
        "-p",
        "1",
    ]);
    run_command(owner, command, cancel, deadline, 1024 * 1024, 120).await?;
    active(cancel)?;
    let path = owner.scratch.0.join("transcript.json");
    let (asset, bytes) = read_asset(&path, MAX_JSON as u64, None, cancel)?;
    if asset.byte_len as usize != bytes.len() {
        return Err(limit());
    }
    parse_output(&bytes, duration)
}
#[cfg(unix)]
async fn run_command(
    owner: &mut CleanupOwner,
    mut command: Command,
    cancel: &Cancellation,
    deadline: Duration,
    file_limit: libc::rlim_t,
    cpu_limit: libc::rlim_t,
) -> MediaResult<()> {
    active(cancel)?;
    if owner.child.is_some() {
        return Err(cleanup_failed());
    }
    command
        .current_dir(&owner.scratch.0)
        .env_clear()
        .env("LANG", "C")
        .env("HOME", &owner.scratch.0)
        .env("TMPDIR", &owner.scratch.0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false);
    #[cfg(unix)]
    {
        command.process_group(0);
        // SAFETY: the post-fork closure uses only async-signal-safe syscalls with
        // stack constants; no allocation occurs on the successful path.
        unsafe {
            command.pre_exec(move || {
                libc::umask(0o077);
                for (resource, value) in [
                    (libc::RLIMIT_FSIZE, file_limit),
                    (libc::RLIMIT_CPU, cpu_limit),
                    (libc::RLIMIT_NOFILE, 64),
                ] {
                    let bound = libc::rlimit {
                        rlim_cur: value,
                        rlim_max: value,
                    };
                    if libc::setrlimit(resource, &bound) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    let child=command.spawn().map_err(|_|error(StatusCode::UNPROCESSABLE_ENTITY,"transcription_spawn_failed","The reviewed CLI could not start. It must be a self-contained executable compatible with this Mac."))?;
    owner.pid = child.id();
    owner.child = Some(child);
    owner.group_pending = true;
    let pid = owner.pid.ok_or_else(cleanup_failed)?;
    let child = owner.child.as_mut().ok_or_else(cleanup_failed)?;
    let stdout = child.stdout.take().ok_or_else(cleanup_failed)?;
    let stderr = child.stderr.take().ok_or_else(cleanup_failed)?;
    let operation = async {
        tokio::select! {biased;_=cancel.cancelled()=>Err(cancelled()), result=async{tokio::try_join!(exit_unreaped(pid),drain(stdout),drain(stderr))}=>result.map(|_|())}
    };
    let outcome = tokio::time::timeout(deadline, operation)
        .await
        .unwrap_or_else(|_| {
            Err(error(
                StatusCode::REQUEST_TIMEOUT,
                "transcription_timeout",
                "Local transcription exceeded its time limit; the owned process was stopped.",
            ))
        });
    if outcome
        .as_ref()
        .is_err_and(|e| e.code == "transcription_child_custody_lost")
    {
        // ECHILD/unknown wait identity removes signal authority. Repeated
        // cleanup may wait again, but never signal this numeric PID or group.
        owner.group_pending = false;
        return Err(cleanup_failed());
    }
    let status = owner.stop_child().await?.ok_or_else(cleanup_failed)?;
    outcome?;
    if !status.success() {
        return Err(failed());
    }
    active(cancel)
}

#[cfg(not(unix))]
async fn run_command(
    _owner: &mut CleanupOwner,
    _command: Command,
    cancel: &Cancellation,
    _deadline: Duration,
    _file_limit: u64,
    _cpu_limit: u64,
) -> MediaResult<()> {
    // No process may start without the adapter's resource and custody controls.
    // These numeric limits are not interpreted as supported Windows rlimits.
    active(cancel)?;
    Err(unsupported())
}

#[cfg(all(test, not(unix)))]
#[tokio::test]
async fn unsupported_command_never_acquires_child_ownership() {
    // No scratch directory or executable is created by this refusal test.
    let mut owner = CleanupOwner::new(Scratch(PathBuf::new()));
    let cancel = Cancellation::new();
    for expected in [
        "transcription_adapter_unsupported",
        "transcription_cancelled",
    ] {
        let error = run_command(
            &mut owner,
            Command::new("smith-transcription-must-not-spawn"),
            &cancel,
            Duration::from_secs(1),
            1,
            1,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, expected);
        assert!(owner.child.is_none());
        assert!(owner.pid.is_none());
        assert!(!owner.group_pending);
        assert_eq!(owner.signal_attempts, 0);
        cancel.cancel();
    }
}

#[cfg(test)]
async fn run_child(
    scratch: &Scratch,
    language: &str,
    duration: f64,
    cancel: &Cancellation,
    deadline: Duration,
) -> MediaResult<Value> {
    let mut owner = CleanupOwner::new(Scratch(scratch.0.clone()));
    let result = run_child_owned(&mut owner, language, duration, cancel, deadline).await;
    assert!(
        owner.child.is_none(),
        "test must retain and retry cleanup if ownership remains"
    );
    result
}
fn parse_output(bytes: &[u8], duration: f64) -> MediaResult<Value> {
    if bytes.len() > MAX_JSON {
        return Err(limit());
    }
    let root: Value = serde_json::from_slice(bytes).map_err(|_| failed())?;
    let language = root["result"]["language"].as_str().ok_or_else(failed)?;
    if !matches!(language, "en" | "ru") {
        return Err(error(StatusCode::UNPROCESSABLE_ENTITY,"transcription_language_unsupported","This local adapter currently accepts English or Russian transcripts. Auto detection returned another language."));
    }
    let input = root["transcription"]
        .as_array()
        .filter(|a| a.len() <= 1000)
        .ok_or_else(failed)?;
    let mut segments = Vec::new();
    let mut text = String::new();
    for segment in input {
        let from = segment["offsets"]["from"].as_u64().ok_or_else(failed)?;
        let to = segment["offsets"]["to"].as_u64().ok_or_else(failed)?;
        if from > 60050 || to > 60050 {
            return Err(failed());
        }
        let content = segment["text"].as_str().ok_or_else(failed)?;
        if content.len() > 8192
            || content
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err(failed());
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(content.trim());
        if text.len() > 64 * 1024 {
            return Err(limit());
        }
        segments.push(json!({"start":from as f64/1000.0,"end":to as f64/1000.0,"text":content}));
    }
    // Empty audio/no detected speech is a valid explicit empty transcript.
    if segments.is_empty() {
        return Ok(
            json!({"text":"","language":language,"segments":[],"srt":"","vtt":"WEBVTT\n\n","timestamped":"","audio_seconds":duration}),
        );
    }
    let mut output = validate_transcript(
        json!({"text":text,"language":language,"segments":segments}),
        duration,
    )
    .map_err(|_| failed())?;
    let timestamped = segments
        .iter()
        .map(|segment| {
            let seconds = segment["start"].as_f64().unwrap_or(0.0) as u64;
            format!(
                "[{:02}:{:02}:{:02}] {}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60,
                segment["text"].as_str().unwrap_or("").trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    output["timestamped"] = json!(timestamped);
    Ok(output)
}

#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use std::sync::OnceLock;
    use tower::ServiceExt;
    fn executable() -> PathBuf {
        static BUILT: OnceLock<tempfile::TempDir> = OnceLock::new();
        BUILT
            .get_or_init(|| {
                let dir = tempfile::tempdir().unwrap();
                let status = std::process::Command::new("/usr/bin/clang")
                    .args(["-O1", "-o"])
                    .arg(dir.path().join("fixture-cli"))
                    .arg(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/fixtures/transcription_child.c"
                    ))
                    .status()
                    .unwrap();
                assert!(status.success());
                dir
            })
            .path()
            .canonicalize()
            .unwrap()
            .join("fixture-cli")
    }
    fn model() -> Vec<u8> {
        let mut bytes = b"lmgg".to_vec();
        for v in [50000u32, 1500, 384, 6, 4, 448, 384, 6, 4, 80, 1] {
            bytes.extend(v.to_le_bytes());
        }
        bytes
    }
    fn wav(mode: u8) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend(32036u32.to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(16000u32.to_le_bytes());
        bytes.extend(32000u32.to_le_bytes());
        bytes.extend(2u16.to_le_bytes());
        bytes.extend(16u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(32000u32.to_le_bytes());
        bytes.resize(32044, 0);
        bytes[44] = mode;
        bytes
    }
    fn selected() -> (tempfile::TempDir, Arc<Review>) {
        let dir = tempfile::tempdir().unwrap();
        let model_path = dir.path().canonicalize().unwrap().join("fixture-model.bin");
        fs::write(&model_path, model()).unwrap();
        let r = prepare_review(&ReviewInput {
            executable_path: executable().to_str().unwrap().into(),
            model_path: model_path.to_str().unwrap().into(),
        })
        .unwrap();
        (dir, Arc::new(r))
    }
    fn app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app(&dir.path().join("synthetic.db")).unwrap();
        (dir, app)
    }
    async fn call(app: &Arc<App>, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(if method == "GET" {
                Body::empty()
            } else {
                Body::from(serde_json::to_vec(&body).unwrap())
            })
            .unwrap();
        let response = routes()
            .with_state(app.clone())
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    async fn reviewed(app: &Arc<App>, r: &Review) -> String {
        static REVIEW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = REVIEW.lock().await;
        let (status, value) = call(
            app,
            "POST",
            "/api/media/transcription/review",
            json!({"executable_path":r.executable.path,"model_path":r.model.path}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["verified_model"], false);
        value["review_id"].as_str().unwrap().into()
    }
    async fn start_job(app: &Arc<App>, review: &str, mode: u8) -> (String, Value) {
        let id = crate::id();
        let (status,result)=call(app,"POST","/api/media/transcription/start",json!({"review_id":review,"request_id":id,"name":"synthetic.wav","wav_base64":STANDARD.encode(wav(mode)),"language":"en"})).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        (id, result)
    }
    async fn finished(app: &Arc<App>, id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (status, value) = call(
                    app,
                    "GET",
                    &format!("/api/media/transcription/jobs/{id}"),
                    Value::Null,
                )
                .await;
                assert_eq!(status, StatusCode::OK);
                if !matches!(value["state"].as_str(), Some("running" | "cancelling")) {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    #[test]
    fn whisper_output_rejects_bad_offsets_controls_and_languages() {
        let good = json!({"result":{"language":"ru"},"transcription":[{"offsets":{"from":0,"to":500},"text":"Привет <&>\nмир"}]});
        let value = parse_output(&serde_json::to_vec(&good).unwrap(), 1.0).unwrap();
        assert_eq!(
            value["segments"][0]["text"],
            good["transcription"][0]["text"]
        );
        assert!(value["vtt"].as_str().unwrap().contains("&lt;&amp;&gt;"));
        assert!(value["timestamped"]
            .as_str()
            .unwrap()
            .contains("[00:00:00]"));
        for bad in [
            json!({"result":{"language":"en"},"transcription":[{"offsets":{"from":20,"to":10},"text":"x"}]}),
            json!({"result":{"language":"en"},"transcription":[{"offsets":{"from":0,"to":1001},"text":"x"}]}),
            json!({"result":{"language":"xx"},"transcription":[]}),
            json!({"result":{"language":"en"},"transcription":[{"offsets":{"from":0,"to":50},"text":"\u{0001}"}]}),
        ] {
            assert!(parse_output(&serde_json::to_vec(&bad).unwrap(), 0.5).is_err());
        }
        assert!(parse_output(b"garbage", 1.0).is_err());
        assert!(parse_output(&vec![b' '; MAX_JSON + 1], 1.0).is_err());
    }
    #[test]
    fn model_and_executable_preflight_rejects_invalid_dimensions_and_dynamic_paths() {
        inspect_model(&model()).unwrap();
        for at in [4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44] {
            let mut bad = model();
            bad[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(inspect_model(&bad).is_err());
        }
        assert!(inspect_executable(b"#!/bin/sh\nexit 0").is_err());
        let mut header = fs::read(executable()).unwrap();
        inspect_executable(&header).unwrap();
        header[32..36].copy_from_slice(&0x8000001cu32.to_le_bytes());
        assert!(inspect_executable(&header).is_err());
    }
    #[test]
    fn reviews_are_read_only_and_snapshots_refuse_path_replacement() {
        let (_dir, selected) = selected();
        assert!(!selected.model.path.with_file_name("child.pid").exists());
        let replacement = selected.model.path.with_file_name("replacement.bin");
        fs::write(&replacement, model()).unwrap();
        fs::rename(replacement, &selected.model.path).unwrap();
        assert_eq!(
            snapshot(&selected, &wav(0), &Cancellation::new())
                .err()
                .unwrap()
                .code,
            "transcription_asset_changed"
        );
    }
    #[test]
    fn selected_asset_links_and_special_files_are_refused() {
        use std::os::unix::fs::symlink;
        let (dir, selected) = selected();
        let alias = dir.path().join("alias.bin");
        symlink(&selected.model.path, &alias).unwrap();
        assert!(read_asset(&alias, MAX_MODEL, None, &Cancellation::new()).is_err());
        assert!(read_asset(dir.path(), MAX_MODEL, None, &Cancellation::new()).is_err());
        let mut oversized = selected.model.path.clone();
        oversized.set_file_name("sparse.bin");
        File::create(&oversized)
            .unwrap()
            .set_len(MAX_MODEL + 1)
            .unwrap();
        assert!(read_asset(&oversized, MAX_MODEL, None, &Cancellation::new()).is_err());
    }
    #[tokio::test]
    async fn owned_native_child_runs_fixed_arguments_and_preserves_unicode() {
        let (_dir, selected) = selected();
        let scratch = snapshot(&selected, &wav(0), &Cancellation::new()).unwrap();
        let path = scratch.0.clone();
        let result = run_child(
            &scratch,
            "en",
            1.0,
            &Cancellation::new(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(result["text"], "Hello & Привет.");
        let pid: i32 = fs::read_to_string(scratch.0.join("child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        scratch.cleanup().unwrap();
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn bounded_child_failure_flood_symlink_and_timeout_are_reaped() {
        let (_dir, selected) = selected();
        for mode in [1, 2, 3, 4, 5] {
            let cancellation = Cancellation::new();
            let scratch = snapshot(&selected, &wav(mode), &cancellation).unwrap();
            let result =
                run_child(&scratch, "en", 1.0, &cancellation, Duration::from_secs(2)).await;
            assert!(result.is_err(), "mode {mode}");
            let pid: i32 = fs::read_to_string(scratch.0.join("child.pid"))
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            scratch.cleanup().unwrap();
        }
    }
    #[tokio::test]
    async fn public_api_runs_previews_saves_once_and_reloads_exact_artifact() {
        let (profile, app) = app();
        let (_assets, selected) = selected();
        let review = reviewed(&app, &selected).await;
        assert_eq!(selected.summary()["allowed_languages"], json!(["en"]));
        let (id, admitted) = start_job(&app, &review, 0).await;
        assert_eq!(admitted["state"], "running");
        let output = finished(&app, &id).await;
        assert_eq!(output["state"], "completed", "{output}");
        assert_eq!(output["result"]["text"], "Hello & Привет.");
        assert_eq!(output["original_audio_retained"], false);
        let save_id = crate::id();
        let body = json!({"request_id":save_id,"title":"Fixture transcript","format":"srt"});
        let path = format!("/api/media/transcription/jobs/{id}/save");
        let (status, saved) = call(&app, "POST", &path, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        let (status, repeated) = call(&app, "POST", &path, body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(repeated["artifact"]["id"], saved["artifact"]["id"]);
        assert_eq!(repeated["duplicate"], true);
        let (status, _) = call(
            &app,
            "POST",
            &path,
            json!({"request_id":save_id,"title":"Changed","format":"text"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let media_id = saved["document"]["id"].as_str().unwrap().to_owned();
        let bytes = media_api::host_original(&app.db.lock().unwrap(), &media_id)
            .unwrap()
            .0;
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["transcript"], output["result"]);
        assert_eq!(payload["format"], "srt");
        assert_eq!(payload["source"]["sha256"], smith_media::sha256(&wav(0)));
        assert_eq!(
            payload["provenance"]["model"]["sha256"],
            selected.model.sha256
        );
        app.tasks.drain().await;
        drop(app);
        let restarted = crate::app(&profile.path().join("synthetic.db")).unwrap();
        assert_eq!(
            media_api::host_original(&restarted.db.lock().unwrap(), &media_id)
                .unwrap()
                .0,
            bytes
        );
        assert_eq!(
            call(
                &restarted,
                "GET",
                &format!("/api/media/transcription/jobs/{id}"),
                Value::Null
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }
    #[tokio::test]
    async fn cancel_revokes_completed_preview_and_forbids_saving() {
        let (_profile, app) = app();
        let (_assets, selected) = selected();
        let review = reviewed(&app, &selected).await;
        let (id, _) = start_job(&app, &review, 0).await;
        assert_eq!(finished(&app, &id).await["state"], "completed");
        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/api/media/transcription/jobs/{id}/cancel"),
                json!({})
            )
            .await
            .1["state"],
            "cancelled"
        );
        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/api/media/transcription/jobs/{id}/save"),
                json!({"request_id":crate::id(),"title":"Forbidden","format":"text"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM artifacts", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[tokio::test]
    async fn active_cancel_and_profile_shutdown_drain_the_tracked_child() {
        let (_profile, app) = app();
        let (_assets, selected) = selected();
        let review = reviewed(&app, &selected).await;
        let (id, _) = start_job(&app, &review, 1).await;
        let result = call(
            &app,
            "POST",
            &format!("/api/media/transcription/jobs/{id}/cancel"),
            json!({}),
        )
        .await;
        assert_eq!(result.0, StatusCode::OK);
        assert_eq!(finished(&app, &id).await["state"], "cancelled");
        let (_id, _) = start_job(&app, &review, 1).await;
        begin_shutdown(&app);
        tokio::time::timeout(Duration::from_secs(5), app.tasks.drain())
            .await
            .unwrap();
        let inner = app.media_runtime.local_transcription.inner.lock().unwrap();
        assert!(inner.closed);
        assert!(inner.jobs.values().all(|job| job.state == "cancelled"));
    }
    #[tokio::test]
    async fn strict_api_admission_rejects_extra_fields_and_wrong_audio_without_execution() {
        let (_profile, app) = app();
        let (_assets, selected) = selected();
        assert_eq!(call(&app,"POST","/api/media/transcription/review",json!({"executable_path":selected.executable.path,"model_path":selected.model.path,"args":["--url"]})).await.0,StatusCode::BAD_REQUEST);
        let review = reviewed(&app, &selected).await;
        for (name, bytes, lang) in [
            ("bad.mp3", wav(0), "en"),
            ("file.wav", b"bad".to_vec(), "en"),
            ("file.wav", wav(0), "xx"),
            ("file.wav", wav(0), "ru"),
            ("file.wav", wav(0), "auto"),
        ] {
            assert!(!call(&app,"POST","/api/media/transcription/start",json!({"request_id":crate::id(),"review_id":review,"name":name,"wav_base64":STANDARD.encode(bytes),"language":lang})).await.0.is_success());
        }
        assert!(app
            .media_runtime
            .local_transcription
            .inner
            .lock()
            .unwrap()
            .jobs
            .is_empty());
    }
    #[tokio::test]
    async fn unresolved_scratch_is_retained_and_profile_close_requires_retry() {
        let (_profile, app) = app();
        let scratch = Scratch::create().unwrap();
        let path = scratch.0.clone();
        let backup = path.with_extension("retained-test");
        fs::rename(&path, &backup).unwrap();
        fs::write(&path, b"synthetic obstacle").unwrap();
        app.media_runtime
            .local_transcription
            .inner
            .lock()
            .unwrap()
            .retained
            .push(CleanupOwner::new(scratch));
        begin_shutdown(&app);
        assert!(shutdown(&app).await.is_err());
        assert_eq!(
            app.media_runtime
                .local_transcription
                .inner
                .lock()
                .unwrap()
                .retained
                .len(),
            1
        );
        assert!(backup.exists());
        fs::remove_file(&path).unwrap();
        fs::rename(&backup, &path).unwrap();
        shutdown(&app).await.unwrap();
        assert!(!path.exists());
        assert!(app
            .media_runtime
            .local_transcription
            .inner
            .lock()
            .unwrap()
            .retained
            .is_empty());
    }
    #[tokio::test]
    async fn exited_leader_stays_unreaped_until_group_termination() {
        let (_dir, selected) = selected();
        let scratch = snapshot(&selected, &wav(0), &Cancellation::new()).unwrap();
        let child = Command::new(scratch.0.join("whisper-cli"))
            .args([
                "-m",
                "model.bin",
                "-f",
                "selected.wav",
                "-l",
                "en",
                "-np",
                "-oj",
                "-of",
                "transcript",
                "-ng",
                "-t",
                "2",
                "-p",
                "1",
            ])
            .env_clear()
            .current_dir(&scratch.0)
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        tokio::time::timeout(Duration::from_secs(3), exit_unreaped(pid))
            .await
            .unwrap()
            .unwrap();
        // Repeated WNOWAIT observation succeeds only while this exact child is
        // still waitable; no intervening Child::wait/try_wait may reap it.
        exit_unreaped(pid).await.unwrap();
        assert!(sole_exited_group_member(pid));
        assert!(child.id().is_some());
        let mut owner = CleanupOwner::new(scratch);
        owner.pid = Some(pid);
        owner.group_pending = true;
        owner.child = Some(child);
        assert!(owner.stop_child().await.unwrap().unwrap().success());
        assert!(owner.child.is_none());
        owner.cleanup().await.unwrap();
    }
    #[tokio::test]
    async fn exited_leader_with_live_descendant_is_not_a_sole_zombie_group() {
        let (_dir, selected) = selected();
        let scratch = snapshot(&selected, &wav(6), &Cancellation::new()).unwrap();
        let child = Command::new(scratch.0.join("whisper-cli"))
            .args([
                "-m",
                "model.bin",
                "-f",
                "selected.wav",
                "-l",
                "en",
                "-np",
                "-oj",
                "-of",
                "transcript",
                "-ng",
                "-t",
                "2",
                "-p",
                "1",
            ])
            .env_clear()
            .current_dir(&scratch.0)
            .process_group(0)
            .kill_on_drop(false)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        tokio::time::timeout(Duration::from_secs(3), exit_unreaped(pid))
            .await
            .unwrap()
            .unwrap();
        let descendant: i32 = fs::read_to_string(scratch.0.join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(!sole_exited_group_member(pid));
        let mut owner = CleanupOwner::new(scratch);
        owner.pid = Some(pid);
        owner.group_pending = true;
        owner.child = Some(child);
        assert!(owner.stop_child().await.unwrap().unwrap().success());
        // The descendant is not our direct child: the OS reaps it. Observe its
        // disappearance without sending any signal after the leader's wait.
        tokio::time::timeout(Duration::from_secs(3), async {
            while unsafe { libc::kill(descendant, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        owner.cleanup().await.unwrap();
    }
    #[tokio::test]
    async fn cleanup_retry_after_uncertain_wait_never_signals_numeric_id_again() {
        let (_dir, selected) = selected();
        let scratch = snapshot(&selected, &wav(1), &Cancellation::new()).unwrap();
        let child = Command::new(scratch.0.join("whisper-cli"))
            .args([
                "-m",
                "model.bin",
                "-f",
                "selected.wav",
                "-l",
                "en",
                "-np",
                "-oj",
                "-of",
                "transcript",
                "-ng",
                "-t",
                "2",
                "-p",
                "1",
            ])
            .env_clear()
            .current_dir(&scratch.0)
            .process_group(0)
            .kill_on_drop(false)
            .spawn()
            .unwrap();
        let mut owner = CleanupOwner::new(scratch);
        owner.pid = child.id();
        owner.child = Some(child);
        owner.group_pending = true;
        owner.wait_fault_once = true;
        assert!(owner.stop_child().await.is_err());
        assert_eq!(owner.signal_attempts, 2);
        assert!(owner.child.is_some());
        assert!(owner.scratch.0.exists());
        owner.cleanup().await.unwrap();
        assert_eq!(owner.signal_attempts, 2);
        assert!(owner.child.is_none());
        assert!(!owner.scratch.0.exists());
    }
    #[tokio::test]
    async fn lost_wait_identity_removes_signal_custody() {
        let mut child = Command::new("/usr/bin/true")
            .kill_on_drop(false)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        child.wait().await.unwrap();
        assert_eq!(
            exit_unreaped(pid).await.unwrap_err().code,
            "transcription_child_custody_lost"
        );
    }
    #[test]
    fn extensible_decoder_pcm_is_reheadered_without_changing_samples() {
        let original = wav(0);
        let mut extended = b"RIFF".to_vec();
        extended.extend((original.len() as u32 + 16).to_le_bytes());
        extended.extend(b"WAVEfmt ");
        extended.extend(40u32.to_le_bytes());
        extended.extend([0xfe, 0xff]);
        extended.extend(&original[22..36]);
        extended.extend([
            22, 0, 16, 0, 4, 0, 0, 0, 1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113,
        ]);
        extended.extend(&original[36..]);
        assert_eq!(canonical_decoder_wave(&extended).unwrap(), original);
        for at in [20, 22, 24, 36, 38, 40, 44, 66] {
            let mut bad = extended.clone();
            bad[at] = 255;
            assert!(canonical_decoder_wave(&bad).is_err(), "offset {at}");
        }
        let mut truncated = extended;
        truncated.pop();
        assert!(canonical_decoder_wave(&truncated).is_err());
    }
    fn aiff_wave() -> Vec<u8> {
        let mut b = b"FORM".to_vec();
        b.extend(32046u32.to_be_bytes());
        b.extend(b"AIFFCOMM");
        b.extend(18u32.to_be_bytes());
        b.extend(1u16.to_be_bytes());
        b.extend(16000u32.to_be_bytes());
        b.extend(16u16.to_be_bytes());
        b.extend([0x40, 0x0c, 0xfa, 0, 0, 0, 0, 0, 0, 0]);
        b.extend(b"SSND");
        b.extend(32008u32.to_be_bytes());
        b.extend([0; 8]);
        b.resize(32054, 0);
        b
    }
    fn decoder_fixture() -> PathBuf {
        static BUILT: OnceLock<tempfile::TempDir> = OnceLock::new();
        BUILT
            .get_or_init(|| {
                let dir = tempfile::tempdir().unwrap();
                assert!(std::process::Command::new("/usr/bin/clang")
                    .args(["-O1", "-o"])
                    .arg(dir.path().join("fixture-decoder"))
                    .arg(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/fixtures/normalization_child.c"
                    ))
                    .status()
                    .unwrap()
                    .success());
                dir
            })
            .path()
            .canonicalize()
            .unwrap()
            .join("fixture-decoder")
    }
    #[tokio::test]
    async fn decoder_caps_and_sequential_child_custody() {
        for mode in ["good", "flood", "oversize", "hold"] {
            let mut owner = CleanupOwner::new(Scratch::create().unwrap());
            let mut cmd = Command::new(decoder_fixture());
            cmd.arg(mode);
            let result = run_command(
                &mut owner,
                cmd,
                &Cancellation::new(),
                Duration::from_millis(300),
                2 * 1024 * 1024,
                20,
            )
            .await;
            if mode == "good" {
                result.unwrap();
            } else {
                assert!(result.is_err(), "{mode}");
            }
            assert!(owner.child.is_none());
            assert!(!owner.group_pending);
            let pid = fs::read_to_string(owner.scratch.0.join("decoder.pid"))
                .unwrap()
                .parse::<i32>()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            if mode == "oversize" {
                assert!(
                    fs::metadata(owner.scratch.0.join("decoded.wav"))
                        .unwrap()
                        .len()
                        <= 2 * 1024 * 1024
                );
            }
            // A second child uses the same retained scratch, with fresh signal
            // authority only for that new child. No numeric ID is reused.
            let mut cmd = Command::new(decoder_fixture());
            cmd.arg("good");
            run_command(
                &mut owner,
                cmd,
                &Cancellation::new(),
                Duration::from_secs(2),
                2 * 1024 * 1024,
                20,
            )
            .await
            .unwrap();
            let path = owner.scratch.0.clone();
            owner.cleanup().await.unwrap();
            assert!(!path.exists());
        }
    }
    #[tokio::test]
    async fn decoder_cancel_reaps_and_changed_os_identity_refuses_before_spawn() {
        let mut owner = CleanupOwner::new(Scratch::create().unwrap());
        let path = owner.scratch.0.clone();
        let cancel = Cancellation::new();
        let signal = cancel.clone();
        let observed = path.clone();
        let trigger = tokio::spawn(async move {
            while !observed.join("decoder.pid").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            signal.cancel();
        });
        let mut cmd = Command::new(decoder_fixture());
        cmd.arg("hold");
        assert_eq!(
            run_command(
                &mut owner,
                cmd,
                &cancel,
                Duration::from_secs(2),
                2 * 1024 * 1024,
                20
            )
            .await
            .unwrap_err()
            .code,
            "transcription_cancelled"
        );
        trigger.await.unwrap();
        owner.cleanup().await.unwrap();
        assert!(!path.exists());
        let mut owner = CleanupOwner::new(Scratch::create().unwrap());
        let mut decoder = trusted_normalizer(&Cancellation::new()).unwrap();
        decoder.sha256 = "0".repeat(64);
        let audio = audio_normalization::inspect("a.wav", &wav(0)).unwrap();
        assert_eq!(
            normalize_owned(&mut owner, &decoder, audio, &Cancellation::new())
                .await
                .unwrap_err()
                .code,
            "transcription_asset_changed"
        );
        assert!(owner.child.is_none());
        owner.cleanup().await.unwrap();
    }
    #[tokio::test]
    async fn normalization_admission_rejects_before_job_insertion() {
        let (_dir, app) = app();
        let (_assets, selected) = selected();
        let review = reviewed(&app, &selected).await;
        for (name, bytes) in [
            ("movie.mp4", vec![0; 44]),
            ("audio.wav", vec![0; 44]),
            ("audio.m4a", b"not mp4".to_vec()),
        ] {
            let(status,value)=call(&app,"POST","/api/media/transcription/start-file",json!({"review_id":review,"request_id":crate::id(),"name":name,"audio_base64":STANDARD.encode(bytes),"language":"en"})).await;
            assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{value}");
        }
        assert!(app
            .media_runtime
            .local_transcription
            .inner
            .lock()
            .unwrap()
            .jobs
            .is_empty());
    }
    #[tokio::test]
    #[ignore = "Explicit actual Apple decoder qualification; no real model. Requires ordinary macOS codec access."]
    async fn actual_apple_normalization_preserves_source_and_saved_provenance() {
        let (_dir, app) = app();
        let (_assets, selected) = selected();
        let review = reviewed(&app, &selected).await;
        for (name, bytes, format) in [
            ("selected.wav", wav(0), "pcm_wav"),
            ("selected.aiff", aiff_wave(), "pcm_aiff"),
            (
                "selected.m4a",
                include_bytes!("../tests/fixtures/normalization-aac.m4a").to_vec(),
                "aac_m4a",
            ),
            (
                "selected.m4a",
                include_bytes!("../tests/fixtures/normalization-alac.m4a").to_vec(),
                "alac_m4a",
            ),
        ] {
            let id = crate::id();
            let original_hash = smith_media::sha256(&bytes);
            let(status,value)=call(&app,"POST","/api/media/transcription/start-file",json!({"review_id":review,"request_id":id,"name":name,"audio_base64":STANDARD.encode(&bytes),"language":"en"})).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            let done = finished(&app, &id).await;
            assert_eq!(done["state"], "completed", "{done}");
            assert_eq!(done["source"]["sha256"], original_hash);
            assert_eq!(done["source"]["byte_len"], bytes.len());
            let normalization = &done["result"]["normalization"];
            assert_eq!(normalization["source_format"], format);
            assert_eq!(normalization["truncated"], false);
            assert_eq!(
                normalization["normalized"]["sha256"]
                    .as_str()
                    .unwrap()
                    .len(),
                64
            );
            let(status,saved)=call(&app,"POST",&format!("/api/media/transcription/jobs/{id}/save"),json!({"request_id":crate::id(),"title":"Generated normalization fixture","format":"text"})).await;
            assert_eq!(status, StatusCode::OK, "{saved}");
            let bytes = media_api::host_original(
                &app.db.lock().unwrap(),
                saved["document"]["id"].as_str().unwrap(),
            )
            .unwrap()
            .0;
            let payload: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(payload["source"]["sha256"], original_hash);
            assert_eq!(&payload["transcript"]["normalization"], normalization);
        }
    }
}
