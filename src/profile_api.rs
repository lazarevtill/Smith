//! Locked-profile host and lifetime tracking. A lock completes only after owned
//! work drains and the encrypted SQLite connection has actually closed.
use crate::*;
use std::future::Future;

pub struct TaskTracker {
    count: watch::Sender<usize>,
}
impl Default for TaskTracker {
    fn default() -> Self {
        let (count, _) = watch::channel(0);
        Self { count }
    }
}
struct TaskPermit(watch::Sender<usize>);
impl Drop for TaskPermit {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}
impl TaskTracker {
    fn reserve(&self) -> TaskPermit {
        self.count.send_modify(|count| *count += 1);
        TaskPermit(self.count.clone())
    }
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_reserved(self.reserve(), future)
    }
    fn spawn_reserved<F>(&self, permit: TaskPermit, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(async move {
            let _permit = permit;
            future.await
        })
    }
    pub async fn drain(&self) {
        let mut count = self.count.subscribe();
        loop {
            if *count.borrow_and_update() == 0 {
                return;
            }
            if count.changed().await.is_err() {
                return;
            }
        }
    }
}

use axum::body::Body;
use smith_profile_storage as storage;
use std::{
    fs,
    io::{Read, Write},
    path::Path as FilePath,
};
use tokio::sync::RwLock;
use tower::ServiceExt;

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    NeedsCreation,
    Locked,
    Unlocked,
    Locking,
    MigrationRequired,
    Operating,
    Failed,
}
#[derive(Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Selection {
    #[default]
    Default,
    Migrated,
    Deactivated,
    Imported,
    DeactivatedImport,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionFile {
    version: u8,
    selection: Selection,
}
struct Opened {
    app: Arc<App>,
    router: Router,
    gate: Arc<RwLock<()>>,
    revoked: watch::Sender<bool>,
}
#[derive(Serialize)]
struct Operation {
    id: String,
    kind: &'static str,
    status: &'static str,
    error: Option<String>,
    result: Option<Value>,
}
struct HostState {
    phase: Phase,
    revision: Consent,
    selection: Selection,
    opened: Option<Opened>,
    fault_connection: Option<Connection>,
    operation: Option<Operation>,
    error: Option<String>,
    stopping: bool,
    migrated_lock: Option<fs::File>,
}
pub struct Host {
    root: PathBuf,
    automatic: bool,
    retained_manual: bool,
    _local_lock: Option<fs::File>,
    state: Mutex<HostState>,
    operations: TaskTracker,
}
fn unavailable() -> ApiError {
    ApiError(
        StatusCode::LOCKED,
        "Unlock the encrypted profile before opening this workspace.".into(),
    )
}
fn stale_session() -> ApiError {
    ApiError(StatusCode::CONFLICT,"This profile session has ended. Return to the profile screen; this request was not replayed.".into())
}
fn profile_error(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}
fn store_error(error: storage::Error) -> ApiError {
    profile_error(error.to_string())
}
impl Host {
    pub fn new(root: PathBuf) -> Result<Arc<Self>, String> {
        let manual = cfg!(test)
            || (cfg!(feature = "synthetic-profile")
                && std::env::var("SMITH_NEXT_MANUAL_PROFILE").as_deref() == Ok("1"));
        if !manual && std::env::var_os("SMITH_NEXT_MANUAL_PROFILE").is_some() {
            return Err(
                "Manual profile mode is available only in an explicit synthetic test build.".into(),
            );
        }
        let retained_manual = [
            storage::DB_FILE,
            storage::PLAINTEXT_FILE,
            "profile-selection.json",
            "migrated",
            "imported",
        ]
        .iter()
        .any(|name| root.join(name).exists());
        let root = if manual {
            root
        } else {
            storage::prepare_local_workspace(&root).map_err(|e| e.to_string())?
        };
        let local_lock = if manual {
            None
        } else {
            Some(lock_slot(&root, "").map_err(|e| e.1)?)
        };
        let selection = read_selection(&root).map_err(|e| e.1)?;
        let migrated_lock =
            if let Some(slot) = selection_slot(selection).filter(|slot| root.join(slot).exists()) {
                Some(lock_slot(&root, slot).map_err(|e| e.1)?)
            } else {
                None
            };
        let phase = probe_phase(&root, selection).map_err(|e| e.1)?;
        let host = Arc::new(Self {
            root,
            automatic: !manual,
            retained_manual,
            _local_lock: local_lock,
            state: Mutex::new(HostState {
                phase,
                revision: Consent::new(),
                selection,
                opened: None,
                fault_connection: None,
                operation: None,
                error: None,
                stopping: false,
                migrated_lock,
            }),
            operations: TaskTracker::default(),
        });
        if host.automatic {
            host.open_local().map_err(|e| e.1)?;
        }
        Ok(host)
    }
    fn open_local(&self) -> ApiResult<()> {
        let selection = self.state.lock().unwrap().selection;
        if matches!(
            selection,
            Selection::Deactivated | Selection::DeactivatedImport
        ) {
            return Err(profile_error(
                "A retained import was deactivated. Existing files were retained.",
            ));
        }
        let key = storage::local_workspace_secret(&self.root).map_err(store_error)?;
        let db =
            storage::unlock(&selected_path(&self.root, selection), &key).map_err(store_error)?;
        drop(key);
        let app = match app_from_connection_owned(db) {
            Ok(app) => app,
            Err(failure) => {
                let (db, _) = *failure;
                return Err(self.close_connection(db).err().unwrap_or_else(|| {
                    profile_error("Saved local settings are invalid. Existing data was retained.")
                }));
            }
        };
        let recovered = {
            let db = app.db.lock().unwrap();
            storage::recover_interrupted(&db).map_err(store_error)
        };
        let recovered = recovered.and_then(|_| {
            agent_api::recover(&app)
                .map_err(|_| profile_error("Local task recovery failed; data was retained."))
        });
        if let Err(error) = recovered {
            let app = Arc::try_unwrap(app).map_err(|_| {
                profile_error("Local startup storage ownership could not be released.")
            })?;
            let db = app
                .db
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return Err(self.close_connection(db).err().unwrap_or(error));
        }
        let (revoked, _) = watch::channel(false);
        let mut state = self.state.lock().unwrap();
        state.opened = Some(Opened {
            router: crate::router(app.clone()),
            app,
            gate: Arc::new(RwLock::new(())),
            revoked,
        });
        state.phase = Phase::Unlocked;
        state.revision.advance();
        Ok(())
    }
    fn begin_local(
        &self,
        revision: &str,
        kind: &'static str,
    ) -> ApiResult<(String, TaskPermit, Option<Opened>)> {
        let (operation, permit, opened) = {
            let mut state = self.state.lock().unwrap();
            if state.stopping || state.revision.token() != revision {
                return Err(stale_session());
            }
            if state.phase != Phase::Unlocked || state.fault_connection.is_some() {
                return Err(profile_error(
                    "Finish the current data operation before starting another.",
                ));
            }
            let operation = id();
            state.operation = Some(Operation {
                id: operation.clone(),
                kind,
                status: "running",
                error: None,
                result: None,
            });
            state.phase = Phase::Operating;
            state.revision.advance();
            if let Some(opened) = &state.opened {
                opened.revoked.send_replace(true);
                agent_api::begin_shutdown(&opened.app);
                crate::media_runtime::begin_shutdown(&opened.app);
            }
            (operation, self.operations.reserve(), state.opened.take())
        };
        Ok((operation, permit, opened))
    }
    fn snapshot(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({"state":state.phase,"revision":state.revision.token(),"selection":state.selection,"session":state.opened.as_ref().filter(|_|state.phase==Phase::Unlocked).map(|s|s.app.profile_session()),"operation":state.operation,"encrypted":true,"automatic":self.automatic,"retained_manual_profile":self.retained_manual,"error":state.error,"migration_slot":"migrated encrypted profile","import_slot":"imported encrypted profile"})
    }
    fn begin(
        &self,
        revision: &str,
        kind: &'static str,
        allowed: &[Phase],
    ) -> ApiResult<(String, TaskPermit)> {
        let mut state = self.state.lock().unwrap();
        if state.stopping {
            return Err(unavailable());
        }
        if state.revision.token() != revision {
            return Err(stale_session());
        }
        if !allowed.contains(&state.phase)
            || state.opened.is_some()
            || state.fault_connection.is_some()
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Finish the current profile operation, or lock the workspace first.".into(),
            ));
        }
        let operation = id();
        state.operation = Some(Operation {
            id: operation.clone(),
            kind,
            status: "running",
            error: None,
            result: None,
        });
        state.phase = Phase::Operating;
        state.error = None;
        state.revision.advance();
        Ok((operation, self.operations.reserve()))
    }
    fn finish(&self, operation: &str, result: ApiResult<Value>, phase: Phase) {
        let mut state = self.state.lock().unwrap();
        if let Some(op) = state.operation.as_mut().filter(|op| op.id == operation) {
            match result {
                Ok(value) => {
                    op.status = "succeeded";
                    op.result = Some(value);
                }
                Err(error) => {
                    op.status = "failed";
                    op.error = Some(error.1);
                }
            }
            state.phase = phase;
            state.revision.advance();
        }
    }
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), String> {
        {
            let mut state = self.state.lock().unwrap();
            state.stopping = true;
        }
        self.operations.drain().await;
        let opened = {
            let mut state = self.state.lock().unwrap();
            state.phase = Phase::Locking;
            state.revision.advance();
            if let Some(opened) = &state.opened {
                opened.revoked.send_replace(true);
                agent_api::begin_shutdown(&opened.app);
                crate::media_runtime::begin_shutdown(&opened.app);
            }
            state.opened.take()
        };
        if let Some(opened) = opened {
            self.close_opened(opened).await.map_err(|e| e.1)?;
        }
        let fault = { self.state.lock().unwrap().fault_connection.take() };
        if let Some(db) = fault {
            self.close_connection(db).map_err(|e| e.1)?;
        }
        Ok(())
    }
    fn close_connection(&self, db: Connection) -> ApiResult<()> {
        if let Err(failure) = storage::close(db) {
            let (db, error) = failure.into_parts();
            let mut state = self.state.lock().unwrap();
            state.fault_connection = Some(db);
            state.phase = Phase::Failed;
            state.error = Some(error.to_string());
            return Err(store_error(error));
        }
        Ok(())
    }
    async fn close_opened(&self, opened: Opened) -> ApiResult<()> {
        // Gate futures observe revocation and drop their async handlers. All
        // committed sync work and task registration finish before this barrier.
        let drain = opened.gate.write().await;
        let runs = opened
            .app
            .runs
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        {
            let _consent = opened.app.consent.lock().unwrap();
            opened.app.native.begin_shutdown();
            opened.app.acceleration.begin_shutdown();
            opened.app.mlx.begin_shutdown();
            opened.app.local_catalog.begin_shutdown();
        }
        for run in &runs {
            run.cancel.notify_one();
        }
        // Let each admitted run consume its cancellation and persist its terminal
        // outcome before closing the supervisor. A simultaneous supervisor shutdown
        // can otherwise turn an intentional cancel into a ShuttingDown failure.
        for run in &runs {
            let mut state = run.tx.subscribe();
            loop {
                if state.borrow_and_update().status != "streaming" {
                    break;
                }
                if state.changed().await.is_err() {
                    break;
                }
            }
        }
        let agents = agent_api::shutdown(&opened.app).await;
        let native = opened.app.native.shutdown().await;
        // Stop any already published actor, then drain setup publishers. A
        // pending launch can publish an owned child or cleanup fault while
        // draining, so only the second shutdown can decide profile closure.
        let _ = opened.app.mlx.shutdown().await;
        opened.app.tasks.drain().await;
        let transcription = crate::media_runtime::local_transcription::shutdown(&opened.app).await;
        let mlx = opened.app.mlx.shutdown().await;
        let setup = opened.app.acceleration.shutdown_result();
        let catalog = opened.app.local_catalog.shutdown_result();
        if let Err(error) = native
            .and(mlx)
            .and(agents)
            .and(setup)
            .and(transcription)
            .and(catalog)
        {
            drop(drain);
            let mut state = self.state.lock().unwrap();
            state.opened = Some(opened);
            state.phase = Phase::Failed;
            state.error = Some(error.clone());
            return Err(profile_error(error));
        }
        for run in runs {
            run.tx.send_modify(|s| {
                s.content.clear();
                s.error = None;
                s.warning = None;
                s.snapshot = Config::default();
                s.status = "locked".into();
            });
        }
        opened.app.runs.lock().unwrap().clear();
        opened.app.auth.lock().unwrap().secret = None;
        opened.app.models.lock().unwrap().clear();
        *opened.app.tested.lock().unwrap() = None;
        drop(drain);
        let Opened { app, router, .. } = opened;
        drop(router);
        let app = match Arc::try_unwrap(app) {
            Ok(app) => app,
            Err(app) => {
                let (gone, _) = watch::channel(true);
                let retained = Opened {
                    router: Router::new(),
                    app,
                    gate: Arc::new(RwLock::new(())),
                    revoked: gone,
                };
                let mut state = self.state.lock().unwrap();
                state.opened = Some(retained);
                state.phase = Phase::Failed;
                state.error=Some("Profile work still owns storage. It remains unavailable; retry locking after cleanup.".into());
                return Err(profile_error(
                    "Profile storage is still owned; lock was not reported complete.",
                ));
            }
        };
        let App { db, .. } = app;
        let db = db
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.close_connection(db)
    }
}
fn lock_migrated(root: &FilePath) -> ApiResult<fs::File> {
    lock_slot(root, "migrated")
}
fn selection_slot(selection: Selection) -> Option<&'static str> {
    match selection {
        Selection::Migrated | Selection::Deactivated => Some("migrated"),
        Selection::Imported | Selection::DeactivatedImport => Some("imported"),
        Selection::Default => None,
    }
}
fn lock_slot(root: &FilePath, slot: &str) -> ApiResult<fs::File> {
    let directory = root.join(slot);
    let meta = fs::symlink_metadata(&directory)
        .map_err(|_| profile_error("Migrated profile directory is unavailable."))?;
    if !meta.file_type().is_dir() {
        return Err(profile_error("Migrated profile must be a real directory."));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(directory.join("profile.lock"))
        .map_err(|_| profile_error("Cannot open migrated profile ownership lock."))?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|_| {
        profile_error("The migrated profile is already owned by another Smith Next process.")
    })?;
    Ok(file)
}
fn selected_path(root: &FilePath, selection: Selection) -> PathBuf {
    if let Some(slot) = selection_slot(selection) {
        root.join(slot).join(storage::DB_FILE)
    } else {
        root.join(storage::DB_FILE)
    }
}
fn probe_phase(root: &FilePath, selection: Selection) -> ApiResult<Phase> {
    if matches!(
        selection,
        Selection::Deactivated | Selection::DeactivatedImport
    ) {
        return Ok(Phase::MigrationRequired);
    }
    match storage::probe(&selected_path(root, selection)).map_err(store_error)? {
        storage::ProfileKind::Missing => {
            if matches!(selection, Selection::Migrated | Selection::Imported) {
                return Err(profile_error("The selected encrypted migration is missing. Existing profile selection was retained."));
            }
            match storage::probe(&root.join(storage::PLAINTEXT_FILE)).map_err(store_error)? {
                storage::ProfileKind::Missing => Ok(Phase::NeedsCreation),
                _ => Ok(Phase::MigrationRequired),
            }
        }
        storage::ProfileKind::EncryptedCandidate => Ok(Phase::Locked),
        storage::ProfileKind::PlaintextMigrationRequired => Ok(Phase::MigrationRequired),
        storage::ProfileKind::Invalid => Err(profile_error(
            "The profile file is incomplete or unrecognized. It was not opened, reset or replaced.",
        )),
    }
}
fn read_selection(root: &FilePath) -> ApiResult<Selection> {
    let path = root.join("profile-selection.json");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Selection::Default),
        Ok(meta) if meta.file_type().is_file() && meta.len() <= 1024 => {}
        _ => {
            return Err(profile_error(
                "Profile selection is invalid; no database was opened.",
            ))
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut bytes = Vec::new();
    options
        .open(&path)
        .map_err(|_| profile_error("Cannot read profile selection."))?
        .take(1025)
        .read_to_end(&mut bytes)
        .map_err(|_| profile_error("Cannot read profile selection."))?;
    let selected: SelectionFile = serde_json::from_slice(&bytes)
        .map_err(|_| profile_error("Profile selection is invalid; no database was opened."))?;
    if selected.version != 1 {
        return Err(profile_error("Profile selection format is unsupported."));
    }
    Ok(selected.selection)
}
fn write_selection(root: &FilePath, selection: Selection) -> ApiResult<()> {
    let target = root.join("profile-selection.json");
    let temporary = root.join(format!(".profile-selection-{}", id()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| profile_error("Cannot stage profile selection."))?;
        let bytes = serde_json::to_vec(&SelectionFile {
            version: 1,
            selection,
        })
        .unwrap();
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| profile_error("Cannot synchronize profile selection."))?;
        fs::rename(&temporary, &target).map_err(|_| {
            profile_error("Cannot publish profile selection; profile files were retained.")
        })?;
        fs::File::open(root).and_then(|dir|dir.sync_all()).map_err(|_|profile_error("Profile selection was published but durability is unconfirmed. Retain both profile directories."))?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
struct Password(storage::Secret);
impl<'de> Deserialize<'de> for Password {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        storage::Secret::new(String::deserialize(d)?)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenInput {
    revision: String,
    passphrase: Password,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LockInput {
    revision: String,
    session: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationInput {
    revision: String,
    source_copy: String,
    passphrase: Option<Password>,
    offline_copy_attested: bool,
    confirm: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyInput {
    revision: String,
    source_copy: String,
    managed_copy: String,
    original_key: Option<storage::LegacyKeys>,
    passphrase: Option<Password>,
    offline_copy_attested: bool,
    confirm: bool,
}
async fn status(State(host): State<Arc<Host>>) -> Json<Value> {
    Json(host.snapshot())
}
async fn create(
    State(host): State<Arc<Host>>,
    Json(input): Json<OpenInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    open(host, input, true).await
}
async fn unlock(
    State(host): State<Arc<Host>>,
    Json(input): Json<OpenInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    open(host, input, false).await
}
async fn open(
    host: Arc<Host>,
    input: OpenInput,
    creating: bool,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if host.automatic {
        return Err(profile_error(
            "This local workspace opens automatically; no login or passphrase setup is required.",
        ));
    }
    let kind = if creating { "create" } else { "unlock" };
    let (operation, permit) = host.begin(
        &input.revision,
        kind,
        if creating {
            &[Phase::NeedsCreation]
        } else {
            &[Phase::Locked]
        },
    )?;
    let response = json!({"operation_id":operation});
    let task_host = host.clone();
    host.operations.spawn_reserved(permit, async move {
        let host = task_host;
        let selection = host.state.lock().unwrap().selection;
        let path = selected_path(&host.root, selection);
        let result = tokio::task::spawn_blocking(move || {
            if creating {
                storage::create(&path, &input.passphrase.0)
            } else {
                storage::unlock(&path, &input.passphrase.0)
            }
        })
        .await;
        let opened = match result {
            Ok(Ok(db)) => match app_from_connection_owned(db) {
                Ok(app) => Ok(app),
                Err(failure) => {
                    let (db, _) = *failure;
                    let error =
                        profile_error("Saved app settings are invalid. Profile data was retained.");
                    Err(host.close_connection(db).err().unwrap_or(error))
                }
            },
            Ok(Err(e)) => Err(store_error(e)),
            Err(_) => Err(profile_error(
                "Profile operation did not finish. Existing files were retained.",
            )),
        };
        match opened {
            Ok(app) => {
                let recovered = {
                    let db = app.db.lock().unwrap();
                    storage::recover_interrupted(&db).map_err(store_error)
                };
                let recovered = recovered.and_then(|_| {
                    agent_api::recover(&app).map_err(|_| {
                        profile_error("Saved task recovery failed; profile data was retained.")
                    })
                });
                let (revoked, _) = watch::channel(false);
                let opened = Opened {
                    router: crate::router(app.clone()),
                    app,
                    gate: Arc::new(RwLock::new(())),
                    revoked,
                };
                if let Err(error) = recovered {
                    let close = host.close_opened(opened).await;
                    let next = if close.is_ok() {
                        Phase::Locked
                    } else {
                        Phase::Failed
                    };
                    host.finish(&operation, Err(close.err().unwrap_or(error)), next);
                    return;
                }
                host.state.lock().unwrap().opened = Some(opened);
                host.finish(&operation, Ok(json!({"encrypted":true})), Phase::Unlocked);
            }
            Err(error) => {
                let phase = if host.state.lock().unwrap().fault_connection.is_some() {
                    Phase::Failed
                } else {
                    probe_phase(&host.root, selection).unwrap_or(Phase::Failed)
                };
                host.finish(&operation, Err(error), phase);
            }
        }
    });
    Ok((StatusCode::ACCEPTED, Json(response)))
}
async fn lock(
    State(host): State<Arc<Host>>,
    Json(input): Json<LockInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if host.automatic {
        return Err(profile_error(
            "This local workspace opens automatically. Close the application to end this session.",
        ));
    }
    let (operation, opened, fault, permit) = {
        let mut state = host.state.lock().unwrap();
        if state.stopping {
            return Err(unavailable());
        }
        if state.revision.token() != input.revision {
            return Err(stale_session());
        }
        if !matches!(state.phase, Phase::Unlocked | Phase::Failed) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "The profile cannot start another lock operation yet.".into(),
            ));
        }
        if let Some(opened) = &state.opened {
            if state.phase == Phase::Unlocked && opened.app.profile_session() != input.session {
                return Err(stale_session());
            }
            opened.revoked.send_replace(true);
            agent_api::begin_shutdown(&opened.app);
            crate::media_runtime::begin_shutdown(&opened.app);
        } else if state.fault_connection.is_none() {
            return Err(unavailable());
        }
        let operation = id();
        state.operation = Some(Operation {
            id: operation.clone(),
            kind: "lock",
            status: "running",
            error: None,
            result: None,
        });
        state.phase = Phase::Locking;
        state.revision.advance();
        (
            operation,
            state.opened.take(),
            state.fault_connection.take(),
            host.operations.reserve(),
        )
    };
    let response = json!({"operation_id":operation});
    let task_host = host.clone();
    host.operations.spawn_reserved(permit, async move {
        let host = task_host;
        let result = if let Some(opened) = opened {
            host.close_opened(opened).await
        } else if let Some(db) = fault {
            host.close_connection(db)
        } else {
            Ok(())
        };
        let phase = if result.is_ok() {
            Phase::Locked
        } else {
            Phase::Failed
        };
        host.finish(&operation, result.map(|_| json!({"closed":true})), phase);
    });
    Ok((StatusCode::ACCEPTED, Json(response)))
}
async fn migrate(
    State(host): State<Arc<Host>>,
    Json(input): Json<MigrationInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    migration(host, input, false).await
}
async fn rollback(
    State(host): State<Arc<Host>>,
    Json(input): Json<MigrationInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    migration(host, input, true).await
}
async fn migration(
    host: Arc<Host>,
    input: MigrationInput,
    rollback: bool,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !input.confirm
        || !input.offline_copy_attested
        || input.source_copy.len() > 4096
        || !FilePath::new(&input.source_copy).is_absolute()
    {
        return Err(profile_error(
            "Select an absolute stopped copy and confirm this operation.",
        ));
    }
    if !host.automatic && input.passphrase.is_none() {
        return Err(profile_error(
            "An explicit migration passphrase is required in manual test mode.",
        ));
    }
    let kind = if rollback { "rollback" } else { "migrate" };
    let (operation, permit, opened) = if host.automatic {
        host.begin_local(&input.revision, kind)?
    } else {
        let (operation, permit) = host.begin(
            &input.revision,
            kind,
            if rollback {
                &[Phase::Locked]
            } else {
                &[Phase::NeedsCreation, Phase::MigrationRequired]
            },
        )?;
        (operation, permit, None)
    };
    let response = json!({"operation_id":operation});
    let task_host = host.clone();
    host.operations.spawn_reserved(permit,async move{
        let host=task_host;if let Some(opened)=opened {if let Err(error)=host.close_opened(opened).await {host.finish(&operation,Err(error),Phase::Failed);return;}}let root=host.root.clone();let automatic=host.automatic;let selection=host.state.lock().unwrap().selection;
        let result=tokio::task::spawn_blocking(move||->ApiResult<(Selection,Value,Option<fs::File>)>{
            let source=PathBuf::from(input.source_copy);let destination=root.join("migrated");
            let secret=if automatic{Password(storage::local_workspace_secret(&root).map_err(store_error)?)}else{input.passphrase.ok_or_else(||profile_error("Migration passphrase is required."))?};
            if rollback{
                if selection!=Selection::Migrated{return Err(profile_error("Only the selected migrated workspace can be deactivated."));}
                let report=storage::check_rollback(&destination,&source,&secret.0,true).map_err(store_error)?;if !report.eligible{return Err(profile_error("Deactivation is not eligible because committed data or source copies changed. All copies were retained."));}
                let next=if automatic{Selection::Default}else{Selection::Deactivated};write_selection(&root,next)?;let mut value=serde_json::to_value(report).unwrap();value["deactivated"]=json!(true);Ok((next,value,None))
            }else{
                let report=storage::migrate_plaintext_copy(&storage::MigrationRequest{source_copy:&source,destination:&destination,offline_copy_attested:true},&secret.0).map_err(store_error)?;
                let lock=lock_migrated(&root)?;write_selection(&root,Selection::Migrated)?;Ok((Selection::Migrated,serde_json::to_value(report).unwrap(),Some(lock)))
            }
        }).await.unwrap_or_else(|_|Err(profile_error("Migration did not finish; existing files were retained.")));
        match result{
            Ok((selected,value,lock))=>{{let mut state=host.state.lock().unwrap();state.selection=selected;state.migrated_lock=lock;}let phase=if automatic{match host.open_local(){Ok(())=>Phase::Unlocked,Err(e)=>{host.finish(&operation,Err(e),Phase::Failed);return;}}}else{probe_phase(&host.root,selected).unwrap_or(Phase::Failed)};host.finish(&operation,Ok(value),phase);},
            Err(e)=>{let phase=if read_selection(&host.root).ok()!=Some(selection){Phase::Failed}else if automatic{match host.open_local(){Ok(())=>Phase::Unlocked,Err(open_error)=>{host.finish(&operation,Err(open_error),Phase::Failed);return;}}}else{probe_phase(&host.root,selection).unwrap_or(Phase::Failed)};host.finish(&operation,Err(e),phase);}
        }
    });
    Ok((StatusCode::ACCEPTED, Json(response)))
}
async fn legacy_preflight(
    State(host): State<Arc<Host>>,
    Json(input): Json<LegacyInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    legacy_operation(host, input, "legacy-preflight").await
}
async fn legacy_import(
    State(host): State<Arc<Host>>,
    Json(input): Json<LegacyInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    legacy_operation(host, input, "legacy-import").await
}
async fn legacy_rollback(
    State(host): State<Arc<Host>>,
    Json(input): Json<LegacyInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    legacy_operation(host, input, "legacy-rollback").await
}
async fn legacy_operation(
    host: Arc<Host>,
    input: LegacyInput,
    kind: &'static str,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let rollback = kind == "legacy-rollback";
    let preflight = kind == "legacy-preflight";
    if !input.confirm
        || !input.offline_copy_attested
        || [&input.source_copy, &input.managed_copy]
            .iter()
            .any(|p| p.len() > 4096 || !FilePath::new(p).is_absolute())
    {
        return Err(profile_error("Select absolute stopped copies of the original profile and managed artifacts, and confirm this operation."));
    }
    if (!rollback && input.original_key.is_none())
        || (!host.automatic && !preflight && input.passphrase.is_none())
        || (rollback && input.original_key.is_some())
    {
        return Err(profile_error("Original import requires explicitly formatted original credentials; deactivation uses only the new profile passphrase."));
    }
    let (operation, permit, opened) = if host.automatic {
        host.begin_local(&input.revision, kind)?
    } else {
        let (operation, permit) = host.begin(
            &input.revision,
            kind,
            if rollback {
                &[Phase::Locked]
            } else {
                &[Phase::NeedsCreation, Phase::MigrationRequired]
            },
        )?;
        (operation, permit, None)
    };
    let response = json!({"operation_id":operation});
    let task_host = host.clone();
    host.operations.spawn_reserved(permit,async move {
        let host=task_host;if let Some(opened)=opened {if let Err(error)=host.close_opened(opened).await {host.finish(&operation,Err(error),Phase::Failed);return;}}let root=host.root.clone();let automatic=host.automatic;let selection=host.state.lock().unwrap().selection;
        let result=tokio::task::spawn_blocking(move ||->ApiResult<(Selection,Value,Option<fs::File>)>{
            let source=PathBuf::from(input.source_copy);let managed=PathBuf::from(input.managed_copy);let destination=root.join("imported");
            if rollback {
                if selection!=Selection::Imported{return Err(profile_error("Only the selected imported profile can be deactivated."));}
                let secret=if automatic {Password(storage::local_workspace_secret(&root).map_err(store_error)?)}else{input.passphrase.ok_or_else(||profile_error("A profile passphrase is required."))?};
                let report=storage::check_legacy_rollback(&destination,&source,&managed,&secret.0,true).map_err(store_error)?;
                if !report.eligible{return Err(profile_error("Original import deactivation is not eligible because committed data or source copies changed. All copies were retained."));}
                let next=if automatic{Selection::Default}else{Selection::DeactivatedImport};write_selection(&root,next)?;let mut report=serde_json::to_value(report).unwrap();report["deactivated"]=json!(true);return Ok((next,report,None));
            }
            let keys=input.original_key.ok_or_else(||profile_error("Original credentials are required."))?;
            let request=storage::LegacyImportRequest{source_copy:&source,managed_copy:&managed,destination:&destination,offline_copy_attested:true};
            if preflight {let report=storage::preflight_legacy_copy(&request,&keys).map_err(store_error)?;return Ok((selection,serde_json::to_value(report).unwrap(),None));}
            let secret=if automatic {Password(storage::local_workspace_secret(&root).map_err(store_error)?)}else{input.passphrase.ok_or_else(||profile_error("A new profile passphrase is required."))?};
            let report=storage::import_legacy_copy(&request,&keys,&secret.0).map_err(store_error)?;
            let lock=lock_slot(&root,"imported")?;write_selection(&root,Selection::Imported)?;
            Ok((Selection::Imported,serde_json::to_value(report).unwrap(),Some(lock)))
        }).await.unwrap_or_else(|_|Err(profile_error("Original import did not finish. Retain the original and destination copies.")));
        match result {
            Ok((selected,result,lock))=>{ {let mut state=host.state.lock().unwrap();state.selection=selected;if lock.is_some(){state.migrated_lock=lock;}}let phase=if automatic {match host.open_local(){Ok(())=>Phase::Unlocked,Err(error)=>{host.finish(&operation,Err(error),Phase::Failed);return;}}} else {probe_phase(&host.root,selected).unwrap_or(Phase::Failed)};host.finish(&operation,Ok(result),phase);},
            Err(error)=>{let current=read_selection(&host.root).unwrap_or(selection);let phase=if current!=selection {Phase::Failed}else if automatic {match host.open_local(){Ok(())=>Phase::Unlocked,Err(open_error)=>{host.finish(&operation,Err(open_error),Phase::Failed);return;}}}else{probe_phase(&host.root,selection).unwrap_or(Phase::Failed)};host.finish(&operation,Err(error),phase);},
        }
    });
    Ok((StatusCode::ACCEPTED, Json(response)))
}
fn query_session_allowed(path: &str) -> bool {
    let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
    matches!(
        parts.as_slice(),
        ["api", "runs", _, "events"]
            | ["api", "chats", _, "export"]
            | ["api", "artifacts", _]
            | ["api", "media", _, "original"]
            | ["api", "media", "artifacts", _, "original"]
            | ["api", "legacy-import", "artifacts", _, "bytes"]
    )
}
async fn revoked(mut value: watch::Receiver<bool>) {
    loop {
        if *value.borrow_and_update() {
            return;
        }
        if value.changed().await.is_err() {
            return;
        }
    }
}
async fn dispatch(State(host): State<Arc<Host>>, req: Request) -> Response {
    if !req.uri().path().starts_with("/api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut token = req
        .headers()
        .get("x-smith-profile-session")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned);
    if req.method() == axum::http::Method::GET && query_session_allowed(req.uri().path()) {
        let tokens = url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
            .filter(|(k, _)| k == "profile_session")
            .map(|(_, v)| v.into_owned())
            .collect::<Vec<_>>();
        if tokens.len() > 1
            || tokens
                .first()
                .is_some_and(|v| token.as_ref().is_some_and(|t| t != v))
        {
            return stale_session().into_response();
        }
        if token.is_none() {
            token = tokens.into_iter().next();
        }
    }
    let (router, lease, changes) = {
        let state = host.state.lock().unwrap();
        if state.phase != Phase::Unlocked {
            return unavailable().into_response();
        }
        let Some(opened) = &state.opened else {
            return unavailable().into_response();
        };
        if token.as_deref() != Some(opened.app.profile_session()) {
            return stale_session().into_response();
        }
        let Ok(lease) = opened.gate.clone().try_read_owned() else {
            return unavailable().into_response();
        };
        (opened.router.clone(), lease, opened.revoked.subscribe())
    };
    let response = tokio::select! {biased;_=revoked(changes.clone())=>{drop(lease);return unavailable().into_response();},response=router.oneshot(req)=>response.unwrap()};
    drop(lease);
    if *changes.borrow() {
        return unavailable().into_response();
    }
    let (parts, body) = response.into_parts();
    let mut chunks = body.into_data_stream();
    Response::from_parts(
        parts,
        Body::from_stream(
            async_stream::stream! {let stopping=revoked(changes);tokio::pin!(stopping);loop{let chunk=tokio::select!{biased;_=&mut stopping=>break,chunk=chunks.next()=>chunk};match chunk{Some(chunk)=>yield chunk,None=>break}}},
        ),
    )
}
pub fn routes(host: Arc<Host>) -> Router {
    Router::new()
        .merge(fonts::routes())
        .merge(crate::ui_assets::routes())
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/profile",
            get(|| async { Html(include_str!("../web/profile.html")) }),
        )
        .route(
            "/assets/agent-panel.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../web/agent-panel.js"),
                )
            }),
        )
        .route(
            "/assets/media-panel.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../web/media-panel.js"),
                )
            }),
        )
        .route(
            "/assets/navigation.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../web/navigation.js"),
                )
            }),
        )
        .route(
            "/assets/undo-trash.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../web/undo-trash.js"),
                )
            }),
        )
        .route("/api/profile/status", get(status))
        .merge(if host.automatic {
            Router::new()
        } else {
            Router::new()
                .route("/api/profile/create", post(create))
                .route("/api/profile/unlock", post(unlock))
                .route("/api/profile/lock", post(lock))
        })
        .route("/api/profile/migrate", post(migrate))
        .route("/api/profile/rollback", post(rollback))
        .route("/api/profile/legacy-preflight", post(legacy_preflight))
        .route("/api/profile/legacy-import", post(legacy_import))
        .route("/api/profile/legacy-rollback", post(legacy_rollback))
        .fallback(dispatch)
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(middleware::from_fn(guard))
        .with_state(host)
}

#[cfg(test)]
#[path = "../tests/support/profile_review.rs"]
mod independent_review;
