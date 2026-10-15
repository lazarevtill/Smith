mod acceleration_api;
#[cfg(test)]
#[path = "../tests/support/acceleration_endpoints.rs"]
mod acceleration_endpoints;
mod action_history;
mod agent_api;
mod agent_definitions;
mod agent_extensions;
mod agent_store;
mod catalog_mode;
mod chat_management;
mod conversations;
mod desktop;
mod drafts;
mod fonts;
mod hardware_inventory;
mod hardware_inventory_platform;
pub mod historical_memory_contract;
mod inference_provenance;
mod legacy_import_api;
mod local_catalog_api;
#[cfg(test)]
mod managed_runtime;
mod media_api;
mod media_runtime;
mod memory_api;
mod message_feedback;
mod mlx_api;
mod model_selection;
mod native_api;
mod profile_api;
mod runtime_catalog;
mod runtime_preferences;
mod streaming;
mod task_model;
mod ui_assets;
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{sse::Event, Html, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{watch, Notify};
type ApiResult<T> = Result<T, ApiError>;
#[derive(Debug)]
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
impl From<rusqlite::Error> for ApiError {
    fn from(_: rusqlite::Error) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Local storage failed. Your saved history has not been deleted.".into(),
        )
    }
}
fn bad(s: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, s.into())
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct Config {
    provider: String,
    endpoint: String,
    model: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "mock".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            model: "smith-demo".into(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct RunState {
    message_id: String,
    content: String,
    status: String,
    error: Option<String>,
    snapshot: Config,
    warning: Option<String>,
    finish_reason: Option<String>,
    transport_incomplete: bool,
    persisted: bool,
}
struct Run {
    tx: watch::Sender<RunState>,
    cancel: Arc<Notify>,
    chat_id: String,
}
// No Serialize/Debug implementation: this is the only credential container.
// HeaderValue's sensitive flag also redacts it from HTTP diagnostics.
struct SessionAuth {
    target_provider: String,
    target_endpoint: String,
    bearer: header::HeaderValue,
}
#[derive(Default)]
struct AuthState {
    secret: Option<SessionAuth>,
    revision: u64,
}
struct Consent {
    epoch: String,
    counter: u64,
}
impl Consent {
    fn new() -> Self {
        Self {
            epoch: id(),
            counter: 0,
        }
    }
    fn token(&self) -> String {
        format!("{}:{}", self.epoch, self.counter)
    }
    fn advance(&mut self) {
        self.counter += 1;
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    revision: String,
    provider: String,
    endpoint: String,
}
fn stale_target() -> ApiError {
    ApiError(StatusCode::CONFLICT,"The selected connection has changed. Review your model or server in Models or Settings before sending. Your prompt was not sent or saved.".into())
}
fn validate_expected(consent: &Consent, c: &Config, expected: &Expected) -> ApiResult<()> {
    if expected.revision != consent.token() || expected.provider != c.provider {
        return Err(stale_target());
    }
    // Demo has no network destination. Match its opaque saved value for older
    // profiles too, without trying to parse an unused or invalid URL.
    let same_endpoint = if matches!(c.provider.as_str(), "mock" | "managed" | "mlx") {
        expected.endpoint == c.endpoint
    } else {
        endpoint(&expected.endpoint).map_err(|_| stale_target())?
            == endpoint(&c.endpoint).map_err(|_| stale_target())?
    };
    if !same_endpoint {
        return Err(stale_target());
    }
    Ok(())
}
struct App {
    profile_session: String,
    tasks: profile_api::TaskTracker,
    consent: Mutex<Consent>,
    db: Mutex<Connection>,
    config: Mutex<Config>,
    auth: Mutex<AuthState>,
    tested: Mutex<Option<Config>>,
    models: Mutex<Vec<Value>>,
    models_revision: Mutex<Option<String>>,
    runs: Mutex<HashMap<String, Arc<Run>>>,
    native: native_api::NativeState,
    acceleration: acceleration_api::AccelerationState,
    mlx: mlx_api::MlxState,
    local_catalog: local_catalog_api::CatalogState,
    agent: agent_api::AgentState,
    media_runtime: media_runtime::MediaState,
}
impl App {
    fn spawn_task<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn(future)
    }
    fn profile_session(&self) -> &str {
        &self.profile_session
    }
}
#[cfg(any(test, feature = "synthetic-profile"))]
fn open_db(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > smith_profile_storage::SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidQuery);
    }
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
    if version < smith_profile_storage::SCHEMA_VERSION {
        smith_profile_storage::initialize_schema(&c)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }
    smith_profile_storage::recover_interrupted(&c)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(c)
}
#[cfg(any(test, feature = "synthetic-profile"))]
fn app(path: &std::path::Path) -> rusqlite::Result<Arc<App>> {
    app_from_connection(open_db(path)?)
}
#[cfg(any(test, feature = "synthetic-profile"))]
fn app_from_connection(c: Connection) -> rusqlite::Result<Arc<App>> {
    app_from_connection_owned(c).map_err(|failure| failure.1)
}
fn app_from_connection_owned(
    c: Connection,
) -> Result<Arc<App>, Box<(Connection, rusqlite::Error)>> {
    let stored = c.query_row("SELECT value FROM settings WHERE key='config'", [], |r| {
        r.get::<_, String>(0)
    });
    let config = match stored {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Config::default()),
        Err(e) => Err(e),
    };
    let config = match config {
        Ok(config) => config,
        Err(error) => return Err(Box::new((c, error))),
    };
    Ok(Arc::new(App {
        profile_session: id(),
        tasks: profile_api::TaskTracker::default(),
        consent: Mutex::new(Consent::new()),
        db: Mutex::new(c),
        config: Mutex::new(config),
        auth: Mutex::new(AuthState::default()),
        tested: Mutex::new(None),
        models: Mutex::new(vec![]),
        models_revision: Mutex::new(None),
        runs: Mutex::new(HashMap::new()),
        native: native_api::NativeState::default(),
        acceleration: acceleration_api::AccelerationState::default(),
        mlx: mlx_api::MlxState::default(),
        local_catalog: local_catalog_api::CatalogState::default(),
        agent: agent_api::AgentState::default(),
        media_runtime: media_runtime::MediaState::default(),
    }))
}
// Numeric private addresses prevent DNS rebinding; no redirects or proxy inheritance.
fn endpoint(raw: &str) -> ApiResult<url::Url> {
    let mut u = url::Url::parse(raw)
        .map_err(|_| bad("Enter an http(s) URL with a private numeric IP or localhost."))?;
    if !matches!(u.scheme(), "http" | "https")
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(bad(
            "Endpoint must use HTTP(S), without credentials, query, or fragment.",
        ));
    }
    let h = u.host_str().ok_or_else(|| bad("Endpoint has no host."))?;
    if h == "localhost" {
        u.set_host(Some("127.0.0.1")).unwrap();
    } else {
        let ip = h.trim_matches(['[', ']']).parse::<IpAddr>().map_err(|_| {
            bad("Use localhost or a numeric private/LAN IP; public DNS is disabled.")
        })?;
        let allowed = match ip {
            IpAddr::V4(v) => v.is_loopback() || v.is_private(),
            IpAddr::V6(v) => v.is_loopback() || ((v.segments()[0] & 0xfe00) == 0xfc00),
        };
        if !allowed {
            return Err(bad(
                "Public and link-local endpoints are disabled. Use loopback or a private LAN IP.",
            ));
        }
    }
    Ok(u)
}
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(60))
        .build()
        .unwrap()
}
fn provider_url(c: &Config, path: &str) -> ApiResult<String> {
    let u = endpoint(&c.endpoint)?;
    Ok(format!("{}/{}", u.as_str().trim_end_matches('/'), path))
}
fn auth_present(a: &App, c: &Config) -> bool {
    a.auth
        .lock()
        .unwrap()
        .secret
        .as_ref()
        .is_some_and(|s| s.target_provider == c.provider && s.target_endpoint == c.endpoint)
}
fn session_bearer(a: &App, c: &Config) -> Option<header::HeaderValue> {
    a.auth
        .lock()
        .unwrap()
        .secret
        .as_ref()
        .filter(|s| s.target_provider == c.provider && s.target_endpoint == c.endpoint)
        .map(|s| s.bearer.clone())
}
fn provider_request(
    c: &Config,
    method: reqwest::Method,
    path: &str,
    bearer: Option<&header::HeaderValue>,
) -> ApiResult<reqwest::RequestBuilder> {
    let mut request = client().request(method, provider_url(c, path)?);
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, bearer.clone());
    }
    Ok(request)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenInput {
    token: String,
    provider: String,
    endpoint: String,
}
async fn session_auth(State(a): State<Arc<App>>) -> Json<Value> {
    let consent = a.consent.lock().unwrap();
    let c = a.config.lock().unwrap().clone();
    Json(json!({"present":auth_present(&a,&c),"revision":consent.token()}))
}
async fn set_session_auth(
    State(a): State<Arc<App>>,
    Json(input): Json<TokenInput>,
) -> ApiResult<Json<Value>> {
    let mut consent = a.consent.lock().unwrap();
    apply_session_auth(&a, &mut consent, input)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewedTokenInput {
    expected: Expected,
    token: String,
}
async fn set_reviewed_session_auth(
    State(a): State<Arc<App>>,
    Json(input): Json<ReviewedTokenInput>,
) -> ApiResult<Json<Value>> {
    let mut consent = a.consent.lock().unwrap();
    validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
    apply_session_auth(
        &a,
        &mut consent,
        TokenInput {
            provider: input.expected.provider,
            endpoint: input.expected.endpoint,
            token: input.token,
        },
    )
}
fn apply_session_auth(a: &App, consent: &mut Consent, input: TokenInput) -> ApiResult<Json<Value>> {
    if a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Finish or cancel the managed operation before changing credentials.".into(),
        ));
    }
    let c = a.config.lock().unwrap();
    if matches!(c.provider.as_str(), "mock" | "managed" | "mlx") {
        return Err(bad(
            "Choose a real local server before adding its session token.",
        ));
    }
    let expected = endpoint(&input.endpoint)?;
    if input.provider != c.provider || expected != endpoint(&c.endpoint)? {
        return Err(ApiError(StatusCode::CONFLICT,"The configured target changed. Save and confirm your intended connection before setting its session token.".into()));
    }
    if input.token.is_empty()
        || input.token.len() > 8192
        || !input
            .token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
    {
        return Err(bad(
            "Enter a bearer token of 1–8192 token characters, without spaces or line breaks.",
        ));
    }
    let mut bearer = header::HeaderValue::from_str(&format!("Bearer {}", input.token))
        .map_err(|_| bad("Invalid bearer token."))?;
    bearer.set_sensitive(true);
    let mut auth = a.auth.lock().unwrap();
    auth.secret = Some(SessionAuth {
        target_provider: c.provider.clone(),
        target_endpoint: c.endpoint.clone(),
        bearer,
    });
    auth.revision = auth.revision.wrapping_add(1);
    *a.tested.lock().unwrap() = None;
    a.models.lock().unwrap().clear();
    *a.models_revision.lock().unwrap() = None;
    consent.advance();
    Ok(Json(json!({"present":true,"revision":consent.token()})))
}
async fn clear_session_auth(State(a): State<Arc<App>>) -> ApiResult<Json<Value>> {
    let mut consent = a.consent.lock().unwrap();
    remove_session_auth(&a, &mut consent)
}
async fn clear_reviewed_session_auth(
    State(a): State<Arc<App>>,
    Json(expected): Json<Expected>,
) -> ApiResult<Json<Value>> {
    let mut consent = a.consent.lock().unwrap();
    validate_expected(&consent, &a.config.lock().unwrap(), &expected)?;
    remove_session_auth(&a, &mut consent)
}
fn remove_session_auth(a: &App, consent: &mut Consent) -> ApiResult<Json<Value>> {
    if a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Finish or cancel the managed operation before changing credentials.".into(),
        ));
    }
    let mut auth = a.auth.lock().unwrap();
    auth.secret = None;
    auth.revision = auth.revision.wrapping_add(1);
    *a.tested.lock().unwrap() = None;
    a.models.lock().unwrap().clear();
    *a.models_revision.lock().unwrap() = None;
    consent.advance();
    Ok(Json(json!({"present":false,"revision":consent.token()})))
}
fn remote_model(v: &Value) -> bool {
    if let Some(o) = v.as_object() {
        for (k, x) in o {
            if ["remote_host", "remote_model"].contains(&k.as_str())
                && !x.is_null()
                && x.as_str() != Some("")
            {
                return true;
            }
            if remote_model(x) {
                return true;
            }
        }
    }
    if let Some(s) = v.as_str() {
        let s = s.to_lowercase();
        return s.ends_with(":cloud") || s.ends_with("-cloud") || s.contains("-cloud:");
    }
    false
}
async fn read_limited(r: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut stream = r.bytes_stream();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .map_err(|_| "Server response timed out.".to_string())?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|_| "Could not read server response.".to_string())?;
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err("Server response exceeds 1 MB.".into());
        }
        bytes.extend(chunk)
    }
    Ok(bytes)
}
async fn guard(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|s| s.to_str().ok())
        .unwrap_or("");
    let valid = host
        .split(':')
        .next()
        .map(|s| s == "127.0.0.1" || s == "localhost")
        .unwrap_or(false);
    if !valid {
        return (StatusCode::FORBIDDEN, "Invalid host").into_response();
    }
    if matches!(req.method().as_str(), "POST" | "PUT" | "DELETE" | "PATCH")
        && req.headers().get(header::ORIGIN).is_none()
    {
        return (StatusCode::FORBIDDEN, "Origin required").into_response();
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let expected = format!("http://{host}");
        if origin.to_str().ok() != Some(expected.as_str()) {
            return (StatusCode::FORBIDDEN, "Cross-origin access blocked").into_response();
        }
    }
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY,"default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'".parse().unwrap());
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
fn router(a: Arc<App>) -> Router {
    Router::new()
        .merge(fonts::routes())
        .merge(drafts::routes())
        .merge(legacy_import_api::routes())
        .merge(local_catalog_api::routes())
        .merge(media_api::routes())
        .merge(message_feedback::routes())
        .merge(memory_api::routes())
        .merge(agent_api::routes())
        .merge(agent_definitions::routes())
        .merge(agent_extensions::routes())
        .merge(action_history::routes())
        .merge(acceleration_api::routes())
        .merge(mlx_api::routes())
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route("/api/config", get(get_config).put(set_config))
        .route(
            "/api/config/reviewed",
            axum::routing::put(set_reviewed_config),
        )
        .route("/api/model", axum::routing::put(model_selection::select))
        .route("/api/native/status", get(native_api::status))
        .route("/api/native/plans", post(native_api::prepare))
        .route("/api/native/load", post(native_api::load))
        .route("/api/native/cancel", post(native_api::cancel))
        .route("/api/native/unload", post(native_api::unload))
        .route(
            "/api/session-auth",
            get(session_auth)
                .put(set_session_auth)
                .delete(clear_session_auth),
        )
        .route("/api/connection/test", post(test_connection))
        .route(
            "/api/session-auth/reviewed",
            axum::routing::put(set_reviewed_session_auth).delete(clear_reviewed_session_auth),
        )
        .route("/api/models", get(models))
        .route("/api/chats", get(chats).post(new_chat))
        .route(
            "/api/chats/{id}",
            get(chat).patch(rename_chat).delete(trash_chat),
        )
        .route("/api/chats/{id}/messages", post(send))
        .route("/api/chats/{id}/retry", post(retry))
        .route("/api/chats/{id}/edit", post(edit_message))
        .route("/api/chats/{id}/regenerate", post(regenerate))
        .route("/api/chats/{id}/branch", post(select_branch))
        .route("/api/chats/{id}/export", get(export))
        .route("/api/runs/{id}/events", get(events))
        .route("/api/runs/{id}/cancel", post(cancel))
        .route("/api/chats/{id}/purge", post(delete_chat))
        .route("/api/artifacts", get(artifacts).post(artifact))
        .route("/api/artifacts/{id}", get(download))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(middleware::from_fn(guard))
        .with_state(a)
}
async fn get_config(State(a): State<Arc<App>>) -> Json<Value> {
    let consent = a.consent.lock().unwrap();
    let c = a.config.lock().unwrap().clone();
    let tested = if c.provider == "managed" {
        native_api::ready(&a, &c).is_ok()
    } else if c.provider == "mlx" {
        mlx_api::ready(&a, &c).is_ok()
    } else {
        c.provider == "mock" || a.tested.lock().unwrap().as_ref() == Some(&c)
    };
    let has_auth = auth_present(&a, &c);
    let mut v = serde_json::to_value(c).unwrap();
    v["revision"] = json!(consent.token());
    v["tested"] = json!(tested);
    v["auth_present"] = json!(has_auth);
    Json(v)
}
async fn set_config(State(a): State<Arc<App>>, Json(c): Json<Config>) -> ApiResult<Json<Value>> {
    let mut consent = a.consent.lock().unwrap();
    apply_config(&a, &mut consent, c)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewedConfigInput {
    config: Config,
    expected: Expected,
}
async fn set_reviewed_config(
    State(a): State<Arc<App>>,
    Json(input): Json<ReviewedConfigInput>,
) -> ApiResult<Json<Value>> {
    let mut consent = a.consent.lock().unwrap();
    validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
    apply_config(&a, &mut consent, input.config)
}
fn apply_config(a: &App, consent: &mut Consent, mut c: Config) -> ApiResult<Json<Value>> {
    if a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Finish or cancel the managed operation before changing the connection.".into(),
        ));
    }
    if !matches!(
        c.provider.as_str(),
        "mock" | "ollama" | "openai" | "managed" | "mlx"
    ) {
        return Err(bad("Unknown provider."));
    }
    if c.provider == "mock" {
        // Do not retain a hidden real-server address when switching to Demo.
        c.endpoint.clear();
        c.model = "smith-demo".into();
    } else if c.provider == "managed" {
        if c.endpoint.is_empty() {
            c.model.clear();
        } else {
            native_api::ready(a, &c)?;
        }
    } else if c.provider == "mlx" {
        if c.endpoint.is_empty() {
            c.model.clear();
        } else {
            mlx_api::ready(a, &c)?;
        }
    } else {
        endpoint(&c.endpoint)?;
    }
    if c.model.len() > 256 {
        return Err(bad("Model name too long."));
    }
    a.db.lock().unwrap().execute(
        "INSERT OR REPLACE INTO settings VALUES('config',?1)",
        [serde_json::to_string(&c).unwrap()],
    )?;
    let mut configured = a.config.lock().unwrap();
    if configured.provider != c.provider || configured.endpoint != c.endpoint {
        let mut auth = a.auth.lock().unwrap();
        auth.secret = None;
        auth.revision = auth.revision.wrapping_add(1);
    }
    *configured = c;
    *a.tested.lock().unwrap() = None;
    a.models.lock().unwrap().clear();
    *a.models_revision.lock().unwrap() = None;
    consent.advance();
    Ok(Json(json!({"ok":true,"revision":consent.token()})))
}
async fn test_connection(
    State(a): State<Arc<App>>,
    Json(expected): Json<Expected>,
) -> ApiResult<Json<Value>> {
    let (c, bearer) = {
        let consent = a.consent.lock().unwrap();
        let c = a.config.lock().unwrap().clone();
        validate_expected(&consent, &c, &expected)?;
        *a.tested.lock().unwrap() = None;
        a.models.lock().unwrap().clear();
        *a.models_revision.lock().unwrap() = None;
        let bearer = session_bearer(&a, &c);
        (c, bearer)
    };
    let ms = if c.provider == "mock" {
        vec![json!({"id":"smith-demo"})]
    } else if c.provider == "managed" {
        native_api::ready(&a, &c)?;
        native_api::models(&a, &c)
    } else if c.provider == "mlx" {
        mlx_api::ready(&a, &c)?;
        mlx_api::models(&a, &c)
    } else {
        let path = if c.provider == "ollama" {
            "api/tags"
        } else {
            "models"
        };
        let r = tokio::time::timeout(
            Duration::from_secs(10),
            provider_request(&c, reqwest::Method::GET, path, bearer.as_ref())?.send(),
        )
        .await
        .map_err(|_| bad("Connection timed out. Check the server address and firewall."))?
        .map_err(|_| {
            bad("Could not connect. Start your inference server and check the address.")
        })?;
        if !r.status().is_success() {
            return Err(bad("Server rejected model discovery. Check the provider, endpoint, and session bearer token if authentication is required."));
        }
        let bytes = read_limited(r).await.map_err(|e| bad(&e))?;
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|_| bad("Server returned invalid model JSON."))?;
        let key = if c.provider == "ollama" {
            "models"
        } else {
            "data"
        };
        v[key]
            .as_array()
            .ok_or_else(|| bad("Unexpected models response."))?
            .iter()
            .filter(|v| !remote_model(v))
            .filter_map(|v| {
                v[if c.provider == "ollama" { "name" } else { "id" }]
                    .as_str()
                    .map(|s| json!({"id":s}))
            })
            .collect()
    };
    let consent = a.consent.lock().unwrap();
    let configured = a.config.lock().unwrap();
    validate_expected(&consent, &configured, &expected)?;
    *a.tested.lock().unwrap() = Some(c.clone());
    *a.models.lock().unwrap() = ms.clone();
    *a.models_revision.lock().unwrap() = Some(consent.token());
    Ok(Json(
        json!({"ok":true,"revision":consent.token(),"provider":c.provider,"endpoint":c.endpoint,"message":if ms.is_empty(){"Connected, but no models installed."}else{"Connected. Models discovered."},"models":ms}),
    ))
}
async fn models(
    State(a): State<Arc<App>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    let c = a.config.lock().unwrap();
    if query.get("revision") != Some(&consent.token()) {
        return Err(stale_target());
    }
    let ms = if c.provider == "mock" {
        vec![json!({"id":"smith-demo"})]
    } else if c.provider == "managed" {
        native_api::models(&a, &c)
    } else if c.provider == "mlx" {
        mlx_api::ready(&a, &c)?;
        mlx_api::models(&a, &c)
    } else {
        if a.models_revision.lock().unwrap().as_ref() != Some(&consent.token())
            || a.tested.lock().unwrap().as_ref() != Some(&c)
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Test the displayed connection before requesting its models.".into(),
            ));
        }
        a.models.lock().unwrap().clone()
    };
    Ok(Json(
        json!({"revision":consent.token(),"provider":c.provider,"endpoint":c.endpoint,"models":ms}),
    ))
}

async fn chats(
    State(a): State<Arc<App>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(chat_management::list(
        &a.db.lock().unwrap(),
        query.get("q").map(String::as_str).unwrap_or("")
    )?)))
}
#[derive(Deserialize)]
struct NewChat {
    title: Option<String>,
    #[serde(flatten)]
    expected: Expected,
}
async fn new_chat(State(a): State<Arc<App>>, Json(input): Json<NewChat>) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    let c = a.config.lock().unwrap().clone();
    validate_expected(&consent, &c, &input.expected)?;
    let i = id();
    let title = input
        .title
        .as_deref()
        .unwrap_or("New conversation")
        .chars()
        .take(100)
        .collect::<String>();
    let summary = chat_management::create(&a.db.lock().unwrap(), &i, &title, now())?;
    Ok(Json(json!(summary)))
}
fn messages(a: &App, i: &str) -> ApiResult<Vec<Value>> {
    conversations::all(&a.db.lock().unwrap(), i)
}
async fn chat(State(a): State<Arc<App>>, Path(i): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(conversations::payload(&a.db.lock().unwrap(), &i)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchInput {
    message_id: String,
    version: i64,
    #[serde(flatten)]
    expected: Expected,
}
async fn select_branch(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
    Json(input): Json<BranchInput>,
) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
    let runs = a.runs.lock().unwrap();
    if runs
        .values()
        .any(|r| r.chat_id == i && r.tx.borrow().status == "streaming")
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Stop the current response before switching branches.".into(),
        ));
    }
    let mut db = a.db.lock().unwrap();
    action_history::select_branch(&mut db, &i, &input.message_id, input.version, now())?;
    Ok(Json(conversations::payload(&db, &i)?))
}
#[derive(Deserialize)]
struct RenameChat {
    title: String,
    version: i64,
    #[serde(flatten)]
    expected: Expected,
}
async fn rename_chat(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
    Json(input): Json<RenameChat>,
) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    let c = a.config.lock().unwrap().clone();
    validate_expected(&consent, &c, &input.expected)?;
    let runs = a.runs.lock().unwrap();
    if runs
        .values()
        .any(|run| run.chat_id == i && run.tx.borrow().status == "streaming")
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Wait for the current response before renaming this conversation.".into(),
        ));
    }
    Ok(Json(json!(chat_management::rename(
        &mut a.db.lock().unwrap(),
        &i,
        &input.title,
        input.version,
        now()
    )?)))
}
#[derive(Deserialize)]
struct TrashChat {
    version: i64,
    #[serde(flatten)]
    expected: Expected,
}
async fn trash_chat(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
    Json(input): Json<TrashChat>,
) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
    let runs = a.runs.lock().unwrap();
    if runs
        .values()
        .any(|r| r.chat_id == i && r.tx.borrow().status == "streaming")
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Stop the current response before moving this conversation to Trash.".into(),
        ));
    }
    Ok(Json(action_history::trash(
        &mut a.db.lock().unwrap(),
        &[action_history::Target {
            id: i,
            version: input.version,
        }],
        None,
        now(),
    )?))
}
#[derive(Deserialize)]
struct DeleteChat {
    confirm_id: String,
    version: i64,
    artifact_policy: chat_management::ArtifactPolicy,
    #[serde(flatten)]
    expected: Expected,
}
async fn delete_chat(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
    Json(input): Json<DeleteChat>,
) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    let c = a.config.lock().unwrap().clone();
    validate_expected(&consent, &c, &input.expected)?;
    if input.confirm_id != i {
        return Err(bad("Confirm the exact conversation selected for deletion."));
    }
    let mut runs = a.runs.lock().unwrap();
    if runs
        .values()
        .any(|r| r.chat_id == i && r.tx.borrow().status == "streaming")
    {
        return Err(ApiError(StatusCode::CONFLICT,"Stop the current generation and wait for it to finish before deleting this conversation.".into()));
    }
    let result = chat_management::delete(
        &mut a.db.lock().unwrap(),
        &i,
        input.version,
        input.artifact_policy,
    )?;
    runs.retain(|_, run| run.chat_id != i);
    Ok(Json(result))
}

#[derive(Deserialize)]
struct Prompt {
    content: String,
    model: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendInput {
    content: String,
    model: Option<String>,
    version: Option<i64>,
    #[serde(default)]
    attachments: Vec<conversations::Attachment>,
    #[serde(default)]
    media: Vec<media_api::MediaReference>,
    #[serde(flatten)]
    expected: Expected,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryInput {
    model: String,
    version: Option<i64>,
    message_id: Option<String>,
    #[serde(flatten)]
    expected: Expected,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    message_id: String,
    version: i64,
    content: String,
    model: Option<String>,
    // Omitted means retain all original attachments; [] explicitly removes them.
    attachments: Option<Vec<conversations::Attachment>>,
    media: Option<Vec<media_api::MediaReference>>,
    #[serde(flatten)]
    expected: Expected,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegenerateInput {
    message_id: String,
    version: i64,
    model: String,
    #[serde(flatten)]
    expected: Expected,
}
#[derive(Default)]
struct ConversationIntent {
    version: Option<i64>,
    edit: Option<String>,
    regenerate: Option<String>,
    attachments: Option<Vec<conversations::Attachment>>,
    media: Option<Vec<media_api::MediaReference>>,
}
async fn send(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
    Json(input): Json<SendInput>,
) -> ApiResult<Json<Value>> {
    if input.version.is_none() {
        return Err(bad(
            "Reload the conversation and include its current version before sending.",
        ));
    }
    start_intent(
        a,
        i,
        Some(Prompt {
            content: input.content,
            model: input.model,
        }),
        input.expected,
        None,
        ConversationIntent {
            version: input.version,
            attachments: Some(input.attachments),
            media: Some(input.media),
            ..Default::default()
        },
    )
    .await
}
async fn retry(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
    Json(input): Json<RetryInput>,
) -> ApiResult<Json<Value>> {
    if input.version.is_none() || input.message_id.is_none() {
        return Err(bad("Retry requires the displayed conversation version and response message_id. Reload the conversation first."));
    }
    start_intent(
        a,
        i,
        None,
        input.expected,
        Some(input.model),
        ConversationIntent {
            version: input.version,
            regenerate: input.message_id,
            ..Default::default()
        },
    )
    .await
}
async fn edit_message(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
    Json(input): Json<EditInput>,
) -> ApiResult<Json<Value>> {
    start_intent(
        a,
        i,
        Some(Prompt {
            content: input.content,
            model: input.model,
        }),
        input.expected,
        None,
        ConversationIntent {
            version: Some(input.version),
            edit: Some(input.message_id),
            attachments: input.attachments,
            media: input.media,
            ..Default::default()
        },
    )
    .await
}
async fn regenerate(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
    Json(input): Json<RegenerateInput>,
) -> ApiResult<Json<Value>> {
    start_intent(
        a,
        i,
        None,
        input.expected,
        Some(input.model),
        ConversationIntent {
            version: Some(input.version),
            regenerate: Some(input.message_id),
            ..Default::default()
        },
    )
    .await
}
#[cfg(test)]
async fn start(
    a: Arc<App>,
    i: String,
    p: Option<Prompt>,
    expected: Expected,
    retry_model: Option<String>,
) -> ApiResult<Json<Value>> {
    start_intent(
        a,
        i,
        p,
        expected,
        retry_model,
        ConversationIntent::default(),
    )
    .await
}
async fn start_intent(
    a: Arc<App>,
    i: String,
    p: Option<Prompt>,
    expected: Expected,
    retry_model: Option<String>,
    intent: ConversationIntent,
) -> ApiResult<Json<Value>> {
    // Lock order matches deletion/navigation: consent -> runs -> database.
    let consent = a.consent.lock().unwrap();
    let mut c = a.config.lock().unwrap().clone();
    validate_expected(&consent, &c, &expected)?;
    let configured = c.clone();
    let mut runs = a.runs.lock().unwrap();
    if runs
        .values()
        .any(|r| r.chat_id == i && r.tx.borrow().status == "streaming")
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Wait for the current answer or stop it.".into(),
        ));
    }
    if matches!(c.provider.as_str(), "managed" | "mlx")
        && runs.values().any(|r| {
            let s = r.tx.borrow();
            s.status == "streaming" && s.snapshot.provider == c.provider
        })
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "The managed model is answering another conversation. Stop it or wait.".into(),
        ));
    }
    let mut db = a.db.lock().unwrap();
    let tx = db.transaction()?;
    let summary = chat_management::summary(&tx, &i)?;
    if let Some(version) = intent.version {
        chat_management::check_version(&tx, &i, version)?;
    }
    conversations::backfill(&tx, &i)?;
    let all = conversations::all(&tx, &i)?;
    let leaf = conversations::leaf(&tx, &i, &all)?;
    let prior = conversations::path(&all, leaf.as_deref())?;
    let original = if let Some(selected) = intent.edit.as_ref().or(intent.regenerate.as_ref()) {
        Some(prior.iter().find(|m|m["id"]==*selected).ok_or_else(||ApiError(StatusCode::CONFLICT,"The selected message is no longer on the active branch. Reopen this conversation before trying again.".into()))?)
    } else if p.is_none() {
        prior.iter().rev().find(|m| m["role"] == "assistant")
    } else {
        None
    };
    if intent.edit.is_some() && original.is_none_or(|m| m["role"] != "user") {
        return Err(bad("Select a user prompt to edit."));
    }
    if p.is_none() {
        let original = original.ok_or_else(|| bad("No response to regenerate."))?;
        if original["role"] != "assistant" {
            return Err(bad("Select an assistant response to regenerate."));
        }
        let snapshot=serde_json::from_value::<Config>(original["snapshot"].clone()).map_err(|_|bad("This older attempt has no saved provider snapshot. Send a new prompt after choosing your connection."))?;
        let same_endpoint = if matches!(snapshot.provider.as_str(), "mock" | "managed" | "mlx") {
            snapshot.endpoint == expected.endpoint
        } else {
            endpoint(&snapshot.endpoint).ok() == endpoint(&expected.endpoint).ok()
        };
        if snapshot.provider != expected.provider
            || !same_endpoint
            || retry_model.as_deref() != Some(snapshot.model.as_str())
        {
            return Err(ApiError(StatusCode::CONFLICT,"Retry must use the displayed original model and endpoint. Restore and test that target, or send a new prompt deliberately.".into()));
        }
        c = snapshot;
    }
    if !matches!(c.provider.as_str(), "mock" | "managed" | "mlx")
        && a.tested.lock().unwrap().as_ref() != Some(&configured)
    {
        return Err(bad("Test this connection before sending a prompt."));
    }
    if let Some(ref p) = p {
        if p.content.trim().is_empty() || p.content.len() > 100_000 {
            return Err(bad("Write a prompt between 1 and 100,000 bytes."));
        }
        if let Some(ref model) = p.model {
            c.model = model.clone();
        }
    }
    let attachments = match intent.attachments {
        Some(files) => files,
        None if intent.edit.is_some() => {
            serde_json::from_value::<Vec<Value>>(original.unwrap()["attachments"].clone())
                .unwrap_or_default()
                .into_iter()
                .map(|f| conversations::Attachment {
                    name: f["name"].as_str().unwrap().into(),
                    content: f["content"].as_str().unwrap().into(),
                    media_type: f["media_type"].as_str().unwrap().into(),
                })
                .collect()
        }
        None => vec![],
    };
    conversations::validate_attachments(&attachments)?;
    let media = match intent.media {
        Some(media) => media,
        None if intent.edit.is_some() => {
            media_api::references_for_message(&tx, original.unwrap()["id"].as_str().unwrap())?
        }
        None => vec![],
    };
    if attachments.len() + media.len() > 3 {
        return Err(bad("Attach at most three files per message."));
    }
    let prepared_media = media_api::validate_references(&tx, &media)?;
    let media_bytes = prepared_media
        .iter()
        .map(|p| match &p.manifest.content {
            smith_media::Content::Document { context, .. } => context.len(),
            _ => 0,
        })
        .sum::<usize>();
    if media_bytes + attachments.iter().map(|a| a.content.len()).sum::<usize>() > 100_000 {
        return Err(bad("Attachment excerpts must total at most 100,000 bytes."));
    }
    if c.model.trim().is_empty() {
        return Err(bad("Choose a model."));
    }
    if !matches!(c.provider.as_str(), "mock" | "managed" | "mlx")
        && remote_model(&json!({"name":c.model}))
    {
        return Err(bad(
            "Cloud-backed models are disabled. Choose a locally installed model.",
        ));
    }
    if !matches!(c.provider.as_str(), "mock" | "managed" | "mlx")
        && a.models_revision.lock().unwrap().as_ref() != Some(&consent.token())
    {
        return Err(stale_target());
    }
    if !matches!(c.provider.as_str(), "mock" | "managed" | "mlx")
        && !a
            .models
            .lock()
            .unwrap()
            .iter()
            .any(|m| m["id"].as_str() == Some(c.model.as_str()))
    {
        return Err(bad("Choose an available model from Test connection."));
    }
    let bearer = session_bearer(&a, &c);
    let rid = id();
    let mid = id();
    let mut parent = if let Some(original) = original {
        original["parent_id"].as_str().map(str::to_owned)
    } else {
        leaf
    };
    // A response must fork from its own user turn, never a later prompt.
    if p.is_none()
        && !all
            .iter()
            .any(|m| m["id"].as_str() == parent.as_deref() && m["role"] == "user")
    {
        return Err(bad("This response has no user prompt to regenerate."));
    }
    let mut user_id = None;
    if let Some(ref p) = p {
        let uid = id();
        tx.execute("INSERT INTO messages(id,chat_id,role,content,status,error,created_at,run_id) VALUES(?1,?2,'user',?3,'completed',NULL,?4,NULL)",params![uid,i,p.content,now()])?;
        conversations::insert_link(&tx, &uid, parent.as_deref())?;
        conversations::attach(&tx, &uid, &attachments)?;
        media_api::attach_prepared(&tx, &uid, &prepared_media)?;
        parent = Some(uid.clone());
        user_id = Some(uid);
        if all.is_empty() {
            tx.execute(
                "UPDATE chats SET title=?2 WHERE id=?1 AND title_locked=0",
                params![i, p.content.chars().take(60).collect::<String>()],
            )?;
        }
    }
    tx.execute("INSERT INTO messages(id,chat_id,role,content,status,error,created_at,run_id) VALUES(?1,?2,'assistant','','streaming',NULL,?3,?4)",params![mid,i,now(),rid])?;
    conversations::insert_link(&tx, &mid, parent.as_deref())?;
    tx.execute(
        "INSERT INTO run_configs(run_id,provider,endpoint,model) VALUES(?1,?2,?3,?4)",
        params![rid, c.provider, c.endpoint, c.model],
    )?;
    conversations::set_leaf(&tx, &i, &mid)?;
    chat_management::touch(&tx, &i, now(), true)?;
    // Resolve and validate provider context before committing any accepted attempt.
    let all = conversations::all(&tx, &i)?;
    let history = request_history(conversations::path(&all, Some(&mid))?);
    let native_destination = if c.provider == "managed" {
        Some(native_api::destination(&a, &c, &history)?)
    } else {
        None
    };
    let mlx_destination = if c.provider == "mlx" {
        Some(mlx_api::destination(&a, &c, &history)?)
    } else {
        None
    };
    if let Some(destination) = mlx_destination.as_ref() {
        inference_provenance::insert(&tx, &rid, destination.provenance())?;
    }
    let payload = conversations::payload(&tx, &i)?;
    tx.commit()?;
    drop(db);
    if runs.len() >= 128 {
        runs.retain(|_, r| r.tx.borrow().status == "streaming");
    }
    let state = RunState {
        message_id: mid.clone(),
        content: String::new(),
        status: "streaming".into(),
        error: None,
        snapshot: c.clone(),
        warning: None,
        finish_reason: None,
        transport_incomplete: false,
        persisted: true,
    };
    let (tx, _) = watch::channel(state);
    let run = Arc::new(Run {
        tx,
        cancel: Arc::new(Notify::new()),
        chat_id: i.clone(),
    });
    runs.insert(rid.clone(), run.clone());
    drop(runs);
    drop(consent);
    let task_app = a.clone();
    a.spawn_task(async move {
        let a = task_app;
        let result = if let Some(destination) = mlx_destination {
            mlx_api::generate(&a, &run, destination, history).await
        } else if let Some(destination) = native_destination {
            native_api::generate(&a, &run, destination, history).await
        } else {
            tokio::select! {biased;_=run.cancel.notified()=>Err("cancelled".to_string()),r=generate(&a,&run,&c,history,bearer)=>r}
        };
        finish_run(&a, &run, result);
    });
    Ok(Json(
        json!({"run_id":rid,"message_id":mid,"user_message_id":user_id,"version":summary.version+1,"conversation":payload}),
    ))
}
fn finish_run(a: &App, run: &Run, result: Result<streaming::Outcome, String>) {
    let mut s = run.tx.borrow().clone();
    match result {
        Ok(outcome) => {
            s.status = outcome.status;
            s.warning = outcome.notice;
            s.finish_reason = Some(outcome.finish_reason);
            s.transport_incomplete = outcome.transport_incomplete;
        }
        Err(e) if e == "cancelled" => {
            s.status = "cancelled".into();
            if s.snapshot.provider == "managed" {
                s.warning=Some("Managed request cancelled. An admitted native generation stops and reaps its owned runtime; check its state and explicitly load again when needed.".into());
            }
        }
        Err(e) => {
            s.status = "failed".into();
            s.error = Some(e)
        }
    };
    if persist(a, &s).is_err() {
        s.warning = None;
        s.status = "failed".into();
        s.persisted = false;
        s.error = Some(
                "Could not save this answer. Copy the full visible response before leaving; saved-history export may omit its latest text.".into(),
            );
    }
    run.tx.send_replace(s);
}
#[cfg(feature = "synthetic-profile")]
async fn stop_runs(a: &App) {
    let runs = a
        .runs
        .lock()
        .unwrap()
        .values()
        .filter(|r| r.tx.borrow().status == "streaming")
        .cloned()
        .collect::<Vec<_>>();
    for run in &runs {
        run.cancel.notify_one();
    }
    let wait = futures_util::future::join_all(runs.into_iter().map(|run| async move {
        let mut rx = run.tx.subscribe();
        loop {
            if rx.borrow_and_update().status != "streaming" {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    }));
    let _ = tokio::time::timeout(Duration::from_secs(1), wait).await;
}
// Retry alternatives remain in the audit history but only the latest successful
// answer for each user turn enters the next provider request.
fn request_history(messages: Vec<Value>) -> Vec<Value> {
    let mut history: Vec<Value> = Vec::new();
    for message in messages {
        if message["status"] != "completed" && message["status"] != "limited" {
            continue;
        }
        if message["role"] == "assistant"
            && history.last().is_some_and(|m| m["role"] == "assistant")
        {
            history.pop();
        }
        let mut message = message;
        message["content"] = json!(conversations::provider_content(&message));
        history.push(message);
    }
    history
}
fn persist(a: &App, s: &RunState) -> ApiResult<()> {
    let mut db = a.db.lock().unwrap();
    let tx = db.transaction()?;
    tx.execute(
        "UPDATE messages SET content=?2,status=?3,error=?4 WHERE id=?1",
        params![s.message_id, s.content, s.status, s.error],
    )?;
    tx.execute("UPDATE run_configs SET warning=?2,finish_reason=?3,transport_incomplete=?4 WHERE run_id=(SELECT run_id FROM messages WHERE id=?1)",params![s.message_id,s.warning,s.finish_reason,s.transport_incomplete])?;
    let chat_id: String = tx.query_row(
        "SELECT chat_id FROM messages WHERE id=?1",
        [&s.message_id],
        |r| r.get(0),
    )?;
    chat_management::touch(&tx, &chat_id, now(), false)?;
    tx.commit()?;
    Ok(())
}
fn append(a: &App, r: &Run, text: &str) -> Result<(), String> {
    let mut s = r.tx.borrow().clone();
    if s.content.len() + text.len() > 2_000_000 {
        return Err("Answer exceeded the 2 MB safety limit. Partial text is saved.".into());
    }
    s.content.push_str(text);
    if persist(a, &s).is_err() {
        s.persisted = false;
        r.tx.send_replace(s);
        return Err(
            "Cannot save answer to local storage. Copy the full visible response before leaving; saved-history export may omit its latest text."
                .into(),
        );
    }
    r.tx.send_replace(s);
    Ok(())
}
async fn generate(
    a: &App,
    r: &Run,
    c: &Config,
    history: Vec<Value>,
    bearer: Option<header::HeaderValue>,
) -> Result<streaming::Outcome, String> {
    if c.provider == "mock" {
        let text="This is Smith’s offline demonstration — no model or network request was used.\n\nYour prompt and each part of this answer are saved locally. Stop at any time, reopen this conversation, or save an answer as a text artifact.\n\nTo use a real model, open Models to review and load a local model, or Settings to select and test your local server. You choose where your prompts go.";
        for word in text.split_inclusive(' ') {
            tokio::time::sleep(Duration::from_millis(40)).await;
            append(a, r, word)?;
        }
        return Ok(streaming::Outcome {
            status: "completed".into(),
            notice: None,
            finish_reason: "stop".into(),
            transport_incomplete: false,
        });
    }
    let msgs = history
        .iter()
        .map(|m| json!({"role":m["role"],"content":m["content"]}))
        .collect::<Vec<_>>();
    let path = if c.provider == "ollama" {
        "api/chat"
    } else {
        "chat/completions"
    };
    if c.provider == "ollama" {
        let resp = provider_request(c, reqwest::Method::POST, "api/show", bearer.as_ref())
            .map_err(|e| e.1)?
            .json(&json!({"model":c.model}))
            .send()
            .await
            .map_err(|_| "Could not verify local model provenance.".to_string())?;
        if !resp.status().is_success() {
            return Err(
                "Cannot verify local model weights. Choose a locally installed model.".into(),
            );
        }
        let bytes = read_limited(resp).await?;
        let v: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid model metadata.".to_string())?;
        if remote_model(&v)
            || v["model_info"]
                .as_object()
                .map(|v| v.is_empty())
                .unwrap_or(true)
        {
            return Err("Model is remote or local weights could not be verified. Cloud-backed inference is disabled.".into());
        }
        if !v["capabilities"]
            .as_array()
            .map(|caps| caps.iter().any(|cap| cap == "completion"))
            .unwrap_or(false)
        {
            return Err("This model does not advertise text completion. Choose a chat model or update the local server.".into());
        }
    }
    let response = provider_request(c, reqwest::Method::POST, path, bearer.as_ref())
        .map_err(|e| e.1)?
        .json(&json!({"model":c.model,"messages":msgs,"stream":true}))
        .send()
        .await
        .map_err(|_| {
            "Could not connect to the model server. Check that it is running, then retry."
                .to_string()
        })?;
    if !response.status().is_success() {
        return Err(format!(
            "Model server returned HTTP {}. Check the model, server, and endpoint, then retry.",
            response.status().as_u16()
        ));
    }
    let mut stream = response.bytes_stream();
    let mut sse = streaming::Sse::default();
    let mut ndjson = streaming::Lines::default();
    let mut completion = streaming::Completion::default();
    loop {
        let next = match tokio::time::timeout(Duration::from_secs(60), stream.next()).await {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(None) => {
                if c.provider == "ollama" {
                    if let Some(frame) = ndjson.finish()? {
                        let (text, outcome) = streaming::ollama(&frame)?;
                        if let Some(text) = text {
                            append(a, r, &text)?;
                        }
                        if let Some(outcome) = outcome {
                            return Ok(outcome);
                        }
                    }
                    return Err("Server disconnected before confirming completion. Partial answer is saved.".into());
                }
                return completion.outcome();
            }
            _ => {
                if c.provider == "openai" {
                    if let Ok(outcome) = completion.outcome() {
                        return Ok(outcome);
                    }
                }
                return Err("Connection interrupted or timed out before completion. Partial answer is saved.".into());
            }
        };
        if c.provider == "ollama" {
            for frame in ndjson.feed(&next) {
                let frame = frame?;
                if frame.trim().is_empty() {
                    continue;
                }
                let (text, outcome) = streaming::ollama(&frame)?;
                if let Some(text) = text {
                    append(a, r, &text)?;
                }
                if let Some(outcome) = outcome {
                    return Ok(outcome);
                }
            }
        } else {
            for event in sse.feed(&next) {
                let event = event?;
                let parsed = completion.accept(&event);
                if let Ok(Some(ref text)) = parsed {
                    append(a, r, text)?;
                }
                if let Some(reason) = completion.finish_reason() {
                    let mut state = r.tx.borrow().clone();
                    state.finish_reason = Some(reason.into());
                    persist(a, &state).map_err(|_| {
                        "Cannot checkpoint completion to local storage.".to_string()
                    })?;
                    r.tx.send_replace(state);
                }
                parsed?;
                if completion.done {
                    return completion.outcome();
                }
            }
        }
    }
}

async fn events(
    State(a): State<Arc<App>>,
    Path(i): Path<String>,
) -> ApiResult<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>> {
    let r = a.runs.lock().unwrap().get(&i).cloned().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "Run no longer active. Reload saved history.".into(),
        )
    })?;
    let mut rx = r.tx.subscribe();
    Ok(Sse::new(async_stream::stream!{loop{let s=rx.borrow_and_update().clone();let terminal=s.status!="streaming";yield Ok(Event::default().event("state").data(serde_json::to_string(&s).unwrap()));if terminal{break}
if rx.changed().await.is_err(){break}}}).keep_alive(axum::response::sse::KeepAlive::default()))
}
async fn cancel(State(a): State<Arc<App>>, Path(i): Path<String>) -> ApiResult<Json<Value>> {
    let r = a
        .runs
        .lock()
        .unwrap()
        .get(&i)
        .cloned()
        .ok_or_else(|| bad("Run not found."))?;
    if r.tx.borrow().status == "streaming" {
        r.cancel.notify_one();
    }
    Ok(Json(json!({"ok":true})))
}
fn text_response(text: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=smith-export.txt",
            ),
        ],
        text,
    )
        .into_response()
}
async fn export(State(a): State<Arc<App>>, Path(i): Path<String>) -> ApiResult<Response> {
    chat_management::summary(&a.db.lock().unwrap(), &i)?;
    let ms = messages(&a, &i)?;
    Ok(text_response(
        ms.iter()
            .map(|m| {
                format!(
                    "{} [{}] id={} parent={}\n{}\n\n",
                    m["role"].as_str().unwrap_or(""),
                    m["status"].as_str().unwrap_or(""),
                    m["id"].as_str().unwrap_or(""),
                    m["parent_id"].as_str().unwrap_or("root"),
                    conversations::provider_content(m)
                )
            })
            .collect(),
    ))
}
async fn artifact(State(a): State<Arc<App>>, Json(v): Json<Value>) -> ApiResult<Json<Value>> {
    let chat = v["chat_id"]
        .as_str()
        .ok_or_else(|| bad("Choose a conversation."))?;
    let content = v["content"]
        .as_str()
        .ok_or_else(|| bad("Text content required."))?;
    if content.len() > 2_000_000 {
        return Err(bad(
            "Text artifacts support up to 2 MB, matching the answer limit.",
        ));
    }
    let name = v["name"]
        .as_str()
        .unwrap_or("Answer.txt")
        .chars()
        .take(100)
        .collect::<String>();
    let i = id();
    let mut db = a.db.lock().unwrap();
    let tx = db.transaction()?;
    let previous_version = chat_management::summary(&tx, chat)?.version;
    tx.execute(
        "INSERT INTO artifacts VALUES(?1,?2,?3,?4,?5)",
        params![i, chat, name, content, now()],
    )?;
    // A captured delete intent must not silently include a subsequently saved artifact.
    chat_management::touch(&tx, chat, now(), true)?;
    tx.commit()?;
    Ok(Json(
        json!({"id":i,"chat_id":chat,"name":name,"content":content,"created_at":now(),"previous_chat_version":previous_version,"chat_version":previous_version+1}),
    ))
}
async fn artifacts(
    State(a): State<Arc<App>>,
    Query(v): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let db = a.db.lock().unwrap();
    let mut q=db.prepare("SELECT id,chat_id,name,content,created_at FROM artifacts WHERE (?1 IS NULL OR chat_id=?1) ORDER BY rowid DESC")?;
    let rows=q.query_map([v.get("chat_id").map(String::as_str)],|r|Ok(json!({"id":r.get::<_,String>(0)?,"chat_id":r.get::<_,Option<String>>(1)?,"name":r.get::<_,String>(2)?,"content":r.get::<_,String>(3)?,"created_at":r.get::<_,i64>(4)?})))?.collect::<Result<Vec<_>,_>>()?;
    Ok(Json(json!(rows)))
}
async fn download(State(a): State<Arc<App>>, Path(i): Path<String>) -> ApiResult<Response> {
    media_api::artifact_original(State(a), Path(i))
        .await
        .map_err(Into::into)
}
fn main() {
    if let Some(code) = catalog_mode::dispatch(std::env::args_os().skip(1)) {
        std::process::exit(code);
    }
    // Consume the one-launch secret before creating runtime threads.
    let result = desktop::Owner::from_environment().and_then(|owner| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|_| "Cannot start local application runtime.".to_owned())?
            .block_on(serve(owner))
    });
    if let Err(message) = result {
        eprintln!("Smith Next could not start: {message}");
        std::process::exit(1);
    }
}
async fn serve(desktop: Option<desktop::Owner>) -> Result<(), String> {
    if desktop.is_some() && std::env::var_os("SMITH_NEXT_SYNTHETIC_PROFILE").is_some() {
        return Err("Desktop launch requires normal local workspace storage.".into());
    }
    let dir = match std::env::var_os("SMITH_NEXT_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => directories::ProjectDirs::from("cloud", "lazarev", "SmithNext")
            .ok_or(
                "Home directory unavailable. Set SMITH_NEXT_DATA_DIR to a new writable directory.",
            )?
            .data_local_dir()
            .to_path_buf(),
    };
    let port = std::env::var("SMITH_NEXT_PORT")
        .unwrap_or("18471".into())
        .parse::<u16>()
        .map_err(|_| "SMITH_NEXT_PORT must be a number between 0 and 65535.".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|_| {
        "Cannot create the profile directory. Choose a writable SMITH_NEXT_DATA_DIR.".to_string()
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|_| {
            "Cannot secure the profile directory. Check ownership and permissions.".to_string()
        })?;
    }
    let profile_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("profile.lock"))
        .map_err(|_| {
            "Cannot open the profile lock. Check profile directory permissions.".to_string()
        })?;
    fs2::FileExt::try_lock_exclusive(&profile_lock).map_err(|_|"This profile is already open in another Smith Next process. Open its browser window, stop that process, or use another SMITH_NEXT_DATA_DIR.".to_string())?;
    // Bind before touching SQLite: an occupied port must not trigger recovery writes.
    let listener = if let Some(owner) = &desktop {
        owner.listener()?
    } else {
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,port)).await.map_err(|error|match error.kind(){std::io::ErrorKind::AddrInUse=>format!("Loopback port {port} is already in use. Open the existing Smith window or choose another SMITH_NEXT_PORT."),std::io::ErrorKind::PermissionDenied=>"Permission to open a loopback listener was denied. Check the local execution permissions.".into(),_=>"Cannot open the loopback listener. Choose another SMITH_NEXT_PORT.".into()})?
    };
    let address = listener
        .local_addr()
        .map_err(|_| "Cannot read loopback listener address.".to_string())?;
    #[cfg(feature = "synthetic-profile")]
    if std::env::var("SMITH_NEXT_SYNTHETIC_PROFILE").as_deref() == Ok("plaintext") {
        if std::env::var_os("SMITH_NEXT_DATA_DIR").is_none() {
            return Err(
                "Synthetic plaintext fixtures require an explicit disposable SMITH_NEXT_DATA_DIR."
                    .into(),
            );
        }
        eprintln!(
            "SYNTHETIC TEST MODE: plaintext fixture profile; not encrypted application storage."
        );
        let a = app(&dir.join("smith-next.sqlite3"))
            .map_err(|_| "Cannot open synthetic fixture profile.".to_string())?;
        let fixture=router(a.clone()).merge(ui_assets::routes()).route("/api/profile/status",get(||async{Json(json!({"state":"unlocked","revision":"synthetic-test","session":"synthetic-test","encrypted":false,"synthetic":true,"operation":null}))}));
        println!("Smith Next synthetic fixture: http://{address}");
        let shutdown_app = a.clone();
        let result = axum::serve(listener, fixture)
            .with_graceful_shutdown(async move {
                let _ = tokio::signal::ctrl_c().await;
                stop_runs(&shutdown_app).await;
                agent_api::begin_shutdown(&shutdown_app);
                media_runtime::begin_shutdown(&shutdown_app);
                shutdown_app.acceleration.begin_shutdown();
                shutdown_app.mlx.begin_shutdown();
                shutdown_app.local_catalog.begin_shutdown();
                let _ = agent_api::shutdown(&shutdown_app).await;
                let _ = shutdown_app.native.shutdown().await;
                let _ = shutdown_app.mlx.shutdown().await;
                shutdown_app.tasks.drain().await;
                let _ = media_runtime::local_transcription::shutdown(&shutdown_app).await;
                let _ = shutdown_app.mlx.shutdown().await;
                let _ = shutdown_app.local_catalog.shutdown_result();
            })
            .await;
        agent_api::shutdown(&a).await?;
        a.acceleration.begin_shutdown();
        a.mlx.begin_shutdown();
        a.local_catalog.begin_shutdown();
        a.native.shutdown().await?;
        let _ = a.mlx.shutdown().await;
        a.tasks.drain().await;
        media_runtime::local_transcription::shutdown(&a).await?;
        a.mlx.shutdown().await?;
        a.acceleration.shutdown_result()?;
        a.local_catalog.shutdown_result()?;
        return result.map_err(|_| "Synthetic test web service stopped unexpectedly.".into());
    }
    if std::env::var_os("SMITH_NEXT_SYNTHETIC_PROFILE").is_some() {
        return Err("Synthetic profile mode requires the explicit synthetic-profile test build and plaintext mode value.".into());
    }
    let host = profile_api::Host::new(dir)?;
    let routes = desktop::routes(profile_api::routes(host.clone()), desktop.clone(), address);
    if let Some(owner) = &desktop {
        owner.announce(address)?;
    } else {
        println!("Smith Next review server: http://{address} — local workspace ready");
    }
    let shutdown_host = host.clone();
    let result = axum::serve(listener, routes)
        .with_graceful_shutdown(async move {
            desktop::shutdown_signal(desktop).await;
            let _ = shutdown_host.shutdown().await;
        })
        .await;
    host.shutdown().await?;
    result.map_err(|_| {
        "Local web service stopped unexpectedly. Profile files were retained.".to_string()
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn test_start(a: Arc<App>, i: String, p: Option<Prompt>) -> ApiResult<Json<Value>> {
        let c = a.config.lock().unwrap().clone();
        let expected = Expected {
            revision: a.consent.lock().unwrap().token(),
            provider: c.provider,
            endpoint: c.endpoint,
        };
        let model = if p.is_none() {
            messages(&a, &i)?
                .iter()
                .rev()
                .find(|m| m["role"] == "assistant")
                .and_then(|m| m["snapshot"]["model"].as_str().map(str::to_owned))
        } else {
            None
        };
        start(a, i, p, expected, model).await
    }
    #[test]
    fn endpoint_boundaries() {
        for s in [
            "http://127.0.0.1:11434",
            "http://localhost:8080/v1",
            "http://192.168.1.10:8080",
            "http://[::1]:8080",
            "http://[fd00::1]:8080",
        ] {
            assert!(endpoint(s).is_ok(), "{s}")
        }
        for s in [
            "https://example.com",
            "http://8.8.8.8",
            "http://169.254.169.254",
            "http://user:pass@127.0.0.1",
            "file:///tmp/x",
            "http://127.0.0.1/?token=x",
            "http://[::ffff:8.8.8.8]",
            "http://[fe80::1]",
        ] {
            assert!(endpoint(s).is_err(), "{s}")
        }
    }
    #[test]
    fn recovery_keeps_partial() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("test.db");
        let c = open_db(&p).unwrap();
        c.execute(
            "INSERT INTO chats(id,title,created_at) VALUES('c','Test',1)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO messages VALUES('m','c','assistant','partial','streaming',NULL,1,'r')",
            [],
        )
        .unwrap();
        drop(c);
        let a = app(&p).unwrap();
        let ms = messages(&a, "c").unwrap();
        assert_eq!(ms[0]["content"], "partial");
        assert_eq!(ms[0]["status"], "interrupted");
    }
    #[tokio::test]
    async fn mock_is_durable_and_cancelled() {
        let d = tempfile::tempdir().unwrap();
        let a = app(&d.path().join("t.db")).unwrap();
        a.db.lock()
            .unwrap()
            .execute(
                "INSERT INTO chats(id,title,created_at) VALUES('c','Test',1)",
                [],
            )
            .unwrap();
        let Json(v) = test_start(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "Synthetic test".into(),
                model: None,
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("start"));
        let rid = v["run_id"].as_str().unwrap();
        assert_eq!(messages(&a, "c").unwrap()[0]["content"], "Synthetic test");
        tokio::time::sleep(Duration::from_millis(140)).await;
        let r = a.runs.lock().unwrap().get(rid).unwrap().clone();
        r.cancel.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let ms = messages(&a, "c").unwrap();
        assert_eq!(ms[1]["status"], "cancelled");
        assert!(!ms[1]["content"].as_str().unwrap().is_empty());
    }
    async fn terminal(run: &Run) -> RunState {
        let mut rx = run.tx.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = rx.borrow_and_update().clone();
                if state.status != "streaming" {
                    return state;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap()
    }
    fn test_app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().unwrap();
        let a = app(&dir.path().join("t.db")).unwrap();
        a.db.lock()
            .unwrap()
            .execute(
                "INSERT INTO chats(id,title,created_at) VALUES('c','Test',1)",
                [],
            )
            .unwrap();
        (dir, a)
    }
    #[tokio::test]
    async fn readonly_prompt_is_atomic_and_never_starts_generation() {
        let (_dir, a) = test_app();
        a.db.lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON;")
            .unwrap();
        assert!(test_start(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "synthetic".into(),
                model: None
            })
        )
        .await
        .is_err());
        assert!(messages(&a, "c").unwrap().is_empty());
        assert!(a.runs.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn readonly_checkpoint_preserves_unsaved_visible_text_and_can_retry() {
        let (_dir, a) = test_app();
        let Json(v) = test_start(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "synthetic".into(),
                model: Some("chosen-demo-model".into()),
            }),
        )
        .await
        .unwrap();
        let run = a
            .runs
            .lock()
            .unwrap()
            .get(v["run_id"].as_str().unwrap())
            .unwrap()
            .clone();
        a.db.lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON;")
            .unwrap();
        let state = terminal(&run).await;
        assert_eq!(state.status, "failed");
        assert!(!state.content.is_empty());
        assert!(state.error.unwrap().contains("Could not save"));
        assert_eq!(messages(&a, "c").unwrap()[1]["content"], "");
        a.db.lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=OFF;")
            .unwrap();
        let Json(v) = test_start(a.clone(), "c".into(), None).await.unwrap();
        let retry = a
            .runs
            .lock()
            .unwrap()
            .get(v["run_id"].as_str().unwrap())
            .unwrap()
            .clone();
        assert_eq!(retry.tx.borrow().snapshot.model, "chosen-demo-model");
        retry.cancel.notify_one();
        terminal(&retry).await;
        let ms = messages(&a, "c").unwrap();
        assert_eq!(ms.len(), 3);
        assert_eq!(ms[2]["snapshot"]["model"], "chosen-demo-model");
    }
    #[test]
    fn corrupt_or_future_profile_fails_without_reset() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.db");
        let db = open_db(&p).unwrap();
        db.execute("INSERT INTO settings VALUES('config','broken')", [])
            .unwrap();
        drop(db);
        assert!(app(&p).is_err());
        let db = Connection::open(&p).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM settings WHERE key='config'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "broken"
        );
        db.execute_batch("PRAGMA user_version=99;").unwrap();
        drop(db);
        assert!(app(&p).is_err());
    }
    #[test]
    fn schema_one_upgrades_without_losing_history() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("v1.db");
        let db = Connection::open(&p).unwrap();
        db.execute_batch("CREATE TABLE chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL); INSERT INTO chats(id,title,created_at) VALUES('c','preserved',1); PRAGMA user_version=1;").unwrap();
        drop(db);
        let a = app(&p).unwrap();
        let db = a.db.lock().unwrap();
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            smith_profile_storage::SCHEMA_VERSION
        );
        assert_eq!(
            db.query_row("SELECT title FROM chats WHERE id='c'", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
    }
    #[test]
    fn confirmed_completion_survives_restart_without_done() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.db");
        let db = open_db(&p).unwrap();
        db.execute_batch("INSERT INTO chats(id,title,created_at) VALUES('c','test',1); INSERT INTO messages VALUES('m','c','assistant','confirmed text','streaming',NULL,1,'r'); INSERT INTO run_configs(run_id,provider,endpoint,model,finish_reason) VALUES('r','openai','http://127.0.0.1:8080/v1','fixture','stop');").unwrap();
        drop(db);
        let a = app(&p).unwrap();
        let m = messages(&a, "c").unwrap();
        assert_eq!(m[0]["status"], "completed");
        assert_eq!(m[0]["content"], "confirmed text");
        assert_eq!(m[0]["transport_incomplete"], true);
        assert!(m[0]["warning"].is_string());
    }
    #[test]
    fn retry_alternatives_are_not_duplicated_in_provider_context() {
        let messages = vec![
            json!({"role":"user","status":"completed","content":"q"}),
            json!({"role":"assistant","status":"completed","content":"old"}),
            json!({"role":"assistant","status":"failed","content":"failed"}),
            json!({"role":"assistant","status":"limited","content":"new"}),
            json!({"role":"user","status":"completed","content":"next"}),
        ];
        let history = request_history(messages);
        assert_eq!(history.len(), 3);
        assert_eq!(history[1]["content"], "new");
    }
    #[tokio::test]
    async fn sqlite_full_rejects_prompt_atomically() {
        let (_dir, a) = test_app();
        {
            let db = a.db.lock().unwrap();
            let pages: i64 = db.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
            db.execute_batch(&format!("PRAGMA max_page_count={pages};"))
                .unwrap();
            let error = db
                .execute(
                    "INSERT INTO messages VALUES('probe','c','user',?1,'completed',NULL,1,NULL)",
                    ["x".repeat(90_000)],
                )
                .unwrap_err();
            assert!(
                matches!(error,rusqlite::Error::SqliteFailure(ref e,_) if e.code==rusqlite::ErrorCode::DiskFull)
            );
        }
        let error = test_start(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "synthetic".repeat(10_000),
                model: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(messages(&a, "c").unwrap().is_empty());
        assert!(a.runs.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn session_auth_is_target_scoped_ephemeral_and_never_config() {
        let (dir, a) = test_app();
        let c = Config {
            provider: "ollama".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            model: "fixture".into(),
        };
        let _ = set_config(State(a.clone()), Json(c.clone())).await.unwrap();
        let _ = set_session_auth(
            State(a.clone()),
            Json(TokenInput {
                token: "synthetic-test-secret".into(),
                provider: c.provider.clone(),
                endpoint: c.endpoint.clone(),
            }),
        )
        .await
        .unwrap();
        assert!(auth_present(&a, &c));
        let req = provider_request(
            &c,
            reqwest::Method::GET,
            "api/tags",
            session_bearer(&a, &c).as_ref(),
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            req.headers()[header::AUTHORIZATION],
            "Bearer synthetic-test-secret"
        );
        assert!(req.headers()[header::AUTHORIZATION].is_sensitive());
        let config = get_config(State(a.clone())).await.0;
        assert_eq!(config["auth_present"], true);
        assert!(!serde_json::to_string(&config)
            .unwrap()
            .contains("synthetic-test-secret"));
        let mismatch = set_session_auth(
            State(a.clone()),
            Json(TokenInput {
                token: "wrong-target-secret".into(),
                provider: c.provider.clone(),
                endpoint: "http://127.0.0.1:9999".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(mismatch.0, StatusCode::CONFLICT);
        let other = Config {
            endpoint: "http://127.0.0.1:9999".into(),
            ..c.clone()
        };
        assert!(session_bearer(&a, &other).is_none());
        let _ = set_config(State(a.clone()), Json(other)).await.unwrap();
        assert!(!auth_present(&a, &c));
        let reopened = app(&dir.path().join("t.db")).unwrap();
        assert!(reopened.auth.lock().unwrap().secret.is_none());
        let db = reopened.db.lock().unwrap();
        let saved: String = db
            .query_row("SELECT value FROM settings WHERE key='config'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!saved.contains("secret"));
    }
    fn expected_now(a: &App) -> Expected {
        let guard = a.consent.lock().unwrap();
        let c = a.config.lock().unwrap();
        Expected {
            revision: guard.token(),
            provider: c.provider.clone(),
            endpoint: c.endpoint.clone(),
        }
    }
    #[tokio::test]
    async fn stale_consent_rejects_send_retry_test_models_create_before_writes() {
        let (_dir, a) = test_app();
        let stale = expected_now(&a);
        let fresh = Config {
            endpoint: "http://127.0.0.1:2222".into(),
            ..Config::default()
        };
        let _ = set_config(State(a.clone()), Json(fresh)).await.unwrap();
        let before = messages(&a, "c").unwrap();
        let err = start(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "private stale prompt".into(),
                model: None,
            }),
            stale.clone(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(
            start(
                a.clone(),
                "c".into(),
                None,
                stale.clone(),
                Some("smith-demo".into())
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            test_connection(State(a.clone()), Json(stale.clone()))
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            new_chat(
                State(a.clone()),
                Json(NewChat {
                    title: Some("private stale title".into()),
                    expected: stale.clone()
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            models(
                State(a.clone()),
                Query(HashMap::from([("revision".into(), stale.revision)]))
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(messages(&a, "c").unwrap(), before);
        assert!(a.runs.lock().unwrap().is_empty());
        assert_eq!(
            a.db.lock()
                .unwrap()
                .query_row("SELECT count(*) FROM chats", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn final_save_failure_preserves_full_visible_recovery_text() {
        let (_dir, a) = test_app();
        a.db.lock().unwrap().execute_batch("INSERT INTO messages VALUES('m','c','assistant','saved prefix','streaming',NULL,1,'r'); INSERT INTO run_configs(run_id,provider,endpoint,model) VALUES('r','mock','http://127.0.0.1:11434','smith-demo');").unwrap();
        let full = "saved prefix plus unsaved tail 🦀";
        let (tx, _) = watch::channel(RunState {
            message_id: "m".into(),
            content: full.into(),
            status: "streaming".into(),
            error: None,
            snapshot: Config::default(),
            warning: None,
            finish_reason: None,
            transport_incomplete: false,
            persisted: true,
        });
        let run = Run {
            tx,
            cancel: Arc::new(Notify::new()),
            chat_id: "c".into(),
        };
        a.db.lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON;")
            .unwrap();
        finish_run(
            &a,
            &run,
            Ok(streaming::Outcome {
                status: "completed".into(),
                notice: Some("The answer is saved.".into()),
                finish_reason: "stop".into(),
                transport_incomplete: false,
            }),
        );
        let state = run.tx.borrow();
        assert_eq!(state.status, "failed");
        assert!(!state.persisted);
        assert!(state.warning.is_none());
        assert_eq!(state.content.as_bytes(), full.as_bytes());
        assert!(state
            .error
            .as_ref()
            .unwrap()
            .contains("Copy the full visible response before leaving"));
        assert!(!state.error.as_ref().unwrap().contains("export your text"));
        assert_eq!(messages(&a, "c").unwrap()[0]["content"], "saved prefix");
    }
    #[test]
    fn coalesced_stream_error_keeps_prefix_in_database() {
        let (_dir, a) = test_app();
        a.db.lock().unwrap().execute_batch("INSERT INTO messages VALUES('m','c','assistant','','streaming',NULL,1,'r'); INSERT INTO run_configs(run_id,provider,endpoint,model) VALUES('r','mock','http://127.0.0.1:11434','smith-demo');").unwrap();
        let (tx, _) = watch::channel(RunState {
            message_id: "m".into(),
            content: String::new(),
            status: "streaming".into(),
            error: None,
            snapshot: Config::default(),
            warning: None,
            finish_reason: None,
            transport_incomplete: false,
            persisted: true,
        });
        let run = Run {
            tx,
            cancel: Arc::new(Notify::new()),
            chat_id: "c".into(),
        };
        let mut sse = streaming::Sse::default();
        let mut completion = streaming::Completion::default();
        for frame in sse.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"saved prefix\"}}]}\n\nevent: error\ndata: {}\n\n"){match frame{Ok(frame)=>{if let Some(text)=completion.accept(&frame).unwrap(){append(&a,&run,&text).unwrap();}},Err(error)=>finish_run(&a,&run,Err(error))}}
        let ms = messages(&a, "c").unwrap();
        assert_eq!(ms[0]["content"], "saved prefix");
        assert_eq!(ms[0]["status"], "failed");
    }
    #[test]
    fn wire_consent_required_and_flattened() {
        let input = json!({"content":"x","model":"smith-demo","revision":"instance:1","provider":"mock","endpoint":"http://127.0.0.1:11434"});
        assert!(serde_json::from_value::<SendInput>(input).is_ok());
        assert!(serde_json::from_value::<SendInput>(json!({"content":"x"})).is_err());
    }
    #[tokio::test]
    async fn demo_after_invalid_endpoint_is_canonical_and_works_without_a_target() {
        let (_dir, a) = test_app();
        for retained in [
            "",
            "not a URL",
            "https://example.com/private",
            "http://127.0.0.1:1",
        ] {
            let _ = set_config(
                State(a.clone()),
                Json(Config {
                    provider: "mock".into(),
                    endpoint: retained.into(),
                    model: "smith-demo".into(),
                }),
            )
            .await
            .unwrap();
            let c = get_config(State(a.clone())).await.0;
            assert_eq!(c["endpoint"], "");
            let consent = expected_now(&a);
            validate_expected(
                &a.consent.lock().unwrap(),
                &a.config.lock().unwrap(),
                &consent,
            )
            .unwrap();
            assert!(test_connection(State(a.clone()), Json(consent))
                .await
                .unwrap()
                .0["ok"]
                .as_bool()
                .unwrap());
        }
        let Json(v) = test_start(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "synthetic offline demo".into(),
                model: Some("smith-demo".into()),
            }),
        )
        .await
        .unwrap();
        let run = a
            .runs
            .lock()
            .unwrap()
            .get(v["run_id"].as_str().unwrap())
            .unwrap()
            .clone();
        // The deliberately paced full Demo answer takes longer than the
        // cancellation tests' two-second terminal deadline.
        let mut rx = run.tx.subscribe();
        let state = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = rx.borrow_and_update().clone();
                if state.status != "streaming" {
                    break state;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(state.status, "completed");
        assert_eq!(state.snapshot.provider, "mock");
        assert_eq!(state.snapshot.endpoint, "");
        assert!(state
            .content
            .contains("no model or network request was used"));
    }
    #[tokio::test]
    async fn rename_persists_and_is_not_overwritten_on_first_prompt() {
        let (dir, a) = test_app();
        let Json(c) = rename_chat(
            State(a.clone()),
            Path("c".into()),
            Json(RenameChat {
                title: "  Новое имя 🦀  ".into(),
                version: 0,
                expected: expected_now(&a),
            }),
        )
        .await
        .unwrap();
        assert_eq!(c["title"], "Новое имя 🦀");
        assert_eq!(c["version"], 1);
        assert_eq!(
            rename_chat(
                State(a.clone()),
                Path("c".into()),
                Json(RenameChat {
                    title: "stale".into(),
                    version: 0,
                    expected: expected_now(&a)
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        for title in ["   ", "line\nbreak"] {
            assert!(
                chat_management::rename(&mut a.db.lock().unwrap(), "c", title, 1, now()).is_err()
            );
        }
        let Json(v) = test_start(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "first prompt".into(),
                model: None,
            }),
        )
        .await
        .unwrap();
        let run = a
            .runs
            .lock()
            .unwrap()
            .get(v["run_id"].as_str().unwrap())
            .unwrap()
            .clone();
        run.cancel.notify_one();
        terminal(&run).await;
        assert_eq!(
            chat_management::summary(&a.db.lock().unwrap(), "c")
                .unwrap()
                .title,
            "Новое имя 🦀"
        );
        let reopened = app(&dir.path().join("t.db")).unwrap();
        assert_eq!(
            chat_management::summary(&reopened.db.lock().unwrap(), "c")
                .unwrap()
                .title,
            "Новое имя 🦀"
        );
    }
    #[test]
    fn sidebar_search_matches_unicode_title_and_latest_preview_only() {
        let (_dir, a) = test_app();
        let mut db = a.db.lock().unwrap();
        chat_management::create(&db, "other", "ПРОЕКТ 100%", 2).unwrap();
        db.execute_batch("INSERT INTO messages VALUES('a','c','user','older hidden needle','completed',NULL,1,NULL); INSERT INTO messages VALUES('b','c','assistant',' Latest    visible preview 🦀 ','completed',NULL,2,NULL);").unwrap();
        assert_eq!(chat_management::list(&db, "проект").unwrap()[0].id, "other");
        assert_eq!(chat_management::list(&db, "100%").unwrap()[0].id, "other");
        assert_eq!(
            chat_management::list(&db, "visible").unwrap()[0].subtitle,
            "Latest visible preview 🦀"
        );
        assert!(chat_management::list(&db, "hidden needle")
            .unwrap()
            .is_empty());
        assert!(chat_management::list(&db, "_").unwrap().is_empty());
        chat_management::rename(&mut db, "c", "Newest", 0, 3).unwrap();
        assert_eq!(chat_management::list(&db, "").unwrap()[0].id, "c");
    }
    fn seed_chat_content(a: &App) {
        a.db.lock().unwrap().execute_batch("INSERT INTO messages VALUES('m','c','assistant','private saved answer','completed',NULL,1,'r'); INSERT INTO run_configs(run_id,provider,endpoint,model) VALUES('r','mock','http://127.0.0.1:11434','smith-demo'); INSERT INTO artifacts VALUES('a','c','kept.txt','independent artifact',1);").unwrap();
    }
    #[tokio::test]
    async fn artifact_save_invalidates_captured_delete_and_is_atomic_with_version() {
        let (_dir, a) = test_app();
        let Json(saved) = artifact(
            State(a.clone()),
            Json(json!({"chat_id":"c","name":"new.txt","content":"newly saved 🦀"})),
        )
        .await
        .unwrap();
        assert_eq!(
            chat_management::summary(&a.db.lock().unwrap(), "c")
                .unwrap()
                .version,
            1
        );
        let error = delete_chat(
            State(a.clone()),
            Path("c".into()),
            Json(DeleteChat {
                confirm_id: "c".into(),
                version: 0,
                artifact_policy: chat_management::ArtifactPolicy::Delete,
                expected: expected_now(&a),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        let Json(rows) = artifacts(State(a.clone()), Query(HashMap::new()))
            .await
            .unwrap();
        assert_eq!(rows[0]["id"], saved["id"]);
        a.db.lock().unwrap().execute_batch("CREATE TRIGGER refuse_chat_touch BEFORE UPDATE ON chats BEGIN SELECT RAISE(ABORT,'synthetic version failure'); END;").unwrap();
        assert!(artifact(
            State(a.clone()),
            Json(json!({"chat_id":"c","name":"must-rollback.txt","content":"not acknowledged"})),
        )
        .await
        .is_err());
        let Json(rows) = artifacts(State(a.clone()), Query(HashMap::new()))
            .await
            .unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(
            chat_management::summary(&a.db.lock().unwrap(), "c")
                .unwrap()
                .version,
            1
        );
    }
    #[tokio::test]
    async fn delete_keep_detaches_artifacts_and_removes_chat_derived_attempts() {
        let (_dir, a) = test_app();
        seed_chat_content(&a);
        let Json(result) = delete_chat(
            State(a.clone()),
            Path("c".into()),
            Json(DeleteChat {
                confirm_id: "c".into(),
                version: 0,
                artifact_policy: chat_management::ArtifactPolicy::Keep,
                expected: expected_now(&a),
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["artifacts_kept"], 1);
        assert_eq!(
            chat(State(a.clone()), Path("c".into()))
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            export(State(a.clone()), Path("c".into()))
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        let Json(art) = artifacts(State(a.clone()), Query(HashMap::new()))
            .await
            .unwrap();
        assert_eq!(art[0]["id"], "a");
        assert!(art[0]["chat_id"].is_null());
        assert_eq!(art[0]["content"], "independent artifact");
        let db = a.db.lock().unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM run_configs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            0
        );
    }
    #[tokio::test]
    async fn delete_all_is_explicit_scoped_and_atomic_on_storage_failure() {
        let (_dir, a) = test_app();
        seed_chat_content(&a);
        {
            let db = a.db.lock().unwrap();
            chat_management::create(&db, "other", "Unrelated", 2).unwrap();
            db.execute(
                "INSERT INTO artifacts VALUES('other-art','other','safe.txt','unchanged',1)",
                [],
            )
            .unwrap();
            db.execute_batch("CREATE TRIGGER refuse_delete BEFORE DELETE ON messages BEGIN SELECT RAISE(ABORT,'synthetic storage failure'); END;").unwrap();
        }
        assert!(chat_management::delete(
            &mut a.db.lock().unwrap(),
            "c",
            0,
            chat_management::ArtifactPolicy::Keep
        )
        .is_err());
        let Json(arts) = artifacts(State(a.clone()), Query(HashMap::new()))
            .await
            .unwrap();
        assert!(arts
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == "a" && v["chat_id"] == "c"));
        assert_eq!(messages(&a, "c").unwrap().len(), 1);
        a.db.lock()
            .unwrap()
            .execute_batch("DROP TRIGGER refuse_delete;")
            .unwrap();
        let Json(result) = delete_chat(
            State(a.clone()),
            Path("c".into()),
            Json(DeleteChat {
                confirm_id: "c".into(),
                version: 0,
                artifact_policy: chat_management::ArtifactPolicy::Delete,
                expected: expected_now(&a),
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["artifacts_deleted"], 1);
        assert_eq!(
            chat_management::list(&a.db.lock().unwrap(), "").unwrap()[0].id,
            "other"
        );
        let Json(arts) = artifacts(State(a.clone()), Query(HashMap::new()))
            .await
            .unwrap();
        assert_eq!(arts.as_array().unwrap().len(), 1);
        assert_eq!(arts[0]["id"], "other-art");
    }
    #[tokio::test]
    async fn delete_requires_confirmation_current_version_and_stopped_generation() {
        let (_dir, a) = test_app();
        let Json(v) = test_start(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "active".into(),
                model: None,
            }),
        )
        .await
        .unwrap();
        let run = a
            .runs
            .lock()
            .unwrap()
            .get(v["run_id"].as_str().unwrap())
            .unwrap()
            .clone();
        let version = chat_management::summary(&a.db.lock().unwrap(), "c")
            .unwrap()
            .version;
        let error = delete_chat(
            State(a.clone()),
            Path("c".into()),
            Json(DeleteChat {
                confirm_id: "c".into(),
                version,
                artifact_policy: chat_management::ArtifactPolicy::Keep,
                expected: expected_now(&a),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        run.cancel.notify_one();
        terminal(&run).await;
        assert_eq!(
            delete_chat(
                State(a.clone()),
                Path("c".into()),
                Json(DeleteChat {
                    confirm_id: "wrong".into(),
                    version,
                    artifact_policy: chat_management::ArtifactPolicy::Delete,
                    expected: expected_now(&a)
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            delete_chat(
                State(a.clone()),
                Path("c".into()),
                Json(DeleteChat {
                    confirm_id: "c".into(),
                    version: version - 1,
                    artifact_policy: chat_management::ArtifactPolicy::Delete,
                    expected: expected_now(&a)
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        assert!(!messages(&a, "c").unwrap().is_empty());
    }
    #[test]
    fn schema_three_migration_preserves_artifact_bytes_and_valid_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v3.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL); CREATE TABLE messages(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,run_id TEXT); CREATE TABLE artifacts(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL); CREATE TABLE run_configs(run_id TEXT PRIMARY KEY,provider TEXT NOT NULL,endpoint TEXT NOT NULL,model TEXT NOT NULL,warning TEXT,finish_reason TEXT,transport_incomplete INTEGER NOT NULL DEFAULT 0); INSERT INTO chats VALUES('c','preserved',123); INSERT INTO artifacts VALUES('a','c','note.txt','preserved unicode 🦀',456); PRAGMA user_version=3;").unwrap();
        drop(db);
        let a = app(&path).unwrap();
        let db = a.db.lock().unwrap();
        assert_eq!(
            db.query_row("SELECT content FROM artifacts WHERE id='a'", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
            "preserved unicode 🦀"
        );
        assert_eq!(chat_management::summary(&db, "c").unwrap().updated_at, 123);
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            smith_profile_storage::SCHEMA_VERSION
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            0
        );
    }
}
