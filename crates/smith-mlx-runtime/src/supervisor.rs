use crate::{protocol, *};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{self, Instant},
};

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}
pub struct LaunchFailure {
    pub error: Failure,
    pub runtime: Runtime,
}
impl std::fmt::Debug for LaunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
struct RuntimeInner {
    tx: mpsc::Sender<CommandRequest>,
    status: watch::Receiver<Snapshot>,
    closing: Cancellation,
    busy: Arc<AtomicBool>,
    worker: tokio::sync::Mutex<WorkerJoin>,
    identity: RuntimeIdentity,
}
#[derive(Clone)]
struct RuntimeIdentity {
    runtime_plan_id: String,
    worker_sha256: String,
    runtime_bundle_sha256: String,
    package_sha256: String,
}
impl RuntimeIdentity {
    fn new(plan: &PlanSummary) -> Self {
        Self {
            runtime_plan_id: plan.id.clone(),
            worker_sha256: plan.worker.sha256.clone(),
            runtime_bundle_sha256: plan.runtime_bundle_sha256.clone(),
            package_sha256: plan.model.package_sha256.clone(),
        }
    }
    fn capture(&self, state: &Snapshot, request: &GenerationRequest) -> GenerationProvenance {
        GenerationProvenance {
            runtime_plan_id: self.runtime_plan_id.clone(),
            worker_sha256: self.worker_sha256.clone(),
            runtime_bundle_sha256: self.runtime_bundle_sha256.clone(),
            package_sha256: self.package_sha256.clone(),
            session_id: state.session_id.clone(),
            runtime_revision: state.revision,
            capability: if request.thinking_mode().is_some() {
                Capability::Chat
            } else {
                Capability::RawCompletion
            },
            template: state.manifest.as_ref().and_then(|m| m.template),
            thinking_mode: request.thinking_mode(),
            sampling: request.sampling().clone(),
            worker_features: state.worker_features,
        }
    }
}
struct WorkerJoin {
    handle: Option<JoinHandle<()>>,
    failed: bool,
}
impl Drop for RuntimeInner {
    fn drop(&mut self) {
        self.closing.cancel()
    }
}
pub struct Operation {
    cancel: Cancellation,
    result: oneshot::Receiver<Result<Outcome>>,
    progress: watch::Receiver<Progress>,
    provenance: Arc<OnceLock<GenerationProvenance>>,
}
impl Operation {
    pub fn cancel(&self) {
        self.cancel.cancel()
    }
    pub fn snapshot(&self) -> Progress {
        self.progress.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }
    /// Set once after actor admission, before dispatch. Retained after failure/cancellation.
    pub fn provenance(&self) -> Option<&GenerationProvenance> {
        self.provenance.get()
    }
    pub async fn wait(&mut self) -> Result<Outcome> {
        (&mut self.result).await.map_err(|_| Failure::Cleanup)?
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        self.cancel.cancel()
    }
}
struct CommandRequest {
    expected: u64,
    id: String,
    request: Request,
    cancel: Cancellation,
    progress: watch::Sender<Progress>,
    reply: oneshot::Sender<Result<Outcome>>,
    provenance: Arc<OnceLock<GenerationProvenance>>,
}
impl Runtime {
    pub async fn launch(approved: ApprovedPlan, cancel: Cancellation) -> Result<Self> {
        Self::launch_owned(approved, cancel)
            .await
            .map_err(|failure| failure.error)
    }
    pub async fn launch_owned(
        approved: ApprovedPlan,
        cancel: Cancellation,
    ) -> std::result::Result<Self, LaunchFailure> {
        let closing = cancel;
        let identity = RuntimeIdentity::new(&approved.0.summary);
        let busy = Arc::new(AtomicBool::new(false));
        let (status_tx, status) = watch::channel(Snapshot {
            session_id: uuid::Uuid::new_v4().to_string(),
            revision: 0,
            phase: Phase::Starting,
            pid: None,
            manifest: None,
            inspection: None,
            failure: None,
            spawned: 0,
            reaped: 0,
            accepting: true,
            backend: "mlx_swift_metal_only",
            execution_kind: approved.0.summary.execution_kind,
            worker_features: WorkerFeatures::default(),
        });
        let (tx, rx) = mpsc::channel(1);
        let (ready_tx, ready) = oneshot::channel();
        let worker = tokio::spawn(actor(
            approved.0,
            rx,
            status_tx,
            closing.clone(),
            busy.clone(),
            ready_tx,
        ));
        let runtime = Self {
            inner: Arc::new(RuntimeInner {
                tx,
                status,
                closing,
                busy,
                worker: tokio::sync::Mutex::new(WorkerJoin {
                    handle: Some(worker),
                    failed: false,
                }),
                identity,
            }),
        };
        match ready.await {
            Ok(Ok(())) => Ok(runtime),
            result => {
                let failure = result
                    .ok()
                    .and_then(|r| r.err())
                    .unwrap_or(Failure::Cleanup);
                let error = match runtime.shutdown().await {
                    Ok(_) => failure,
                    Err(e) => e,
                };
                Err(LaunchFailure { error, runtime })
            }
        }
    }
    pub fn status(&self) -> Snapshot {
        self.inner.status.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.inner.status.clone()
    }
    /// Call after load and before persisting a selected chat mode. This checks the
    /// live revision, exact loaded template and negotiated worker feature, not model names.
    pub fn validate_chat_thinking(&self, expected_revision: u64, mode: ThinkingMode) -> Result<()> {
        let state = self.ready_for_provenance(expected_revision)?;
        mode.validate(
            state.manifest.as_ref().ok_or(Failure::NotLoaded)?,
            state.worker_features,
        )
    }
    /// Bounded descriptive intent for the host's accepted-run transaction. It
    /// does not enqueue work; begin() revalidates the same revision at dispatch.
    pub fn generation_provenance(
        &self,
        expected_revision: u64,
        request: &GenerationRequest,
    ) -> Result<GenerationProvenance> {
        let state = self.ready_for_provenance(expected_revision)?;
        request.validate(
            state.manifest.as_ref().ok_or(Failure::NotLoaded)?,
            state.worker_features,
        )?;
        Ok(self.inner.identity.capture(&state, request))
    }
    fn ready_for_provenance(&self, expected_revision: u64) -> Result<Snapshot> {
        let state = self.status();
        if self.inner.closing.is_cancelled() || !state.accepting {
            return Err(Failure::ChildExited);
        }
        if expected_revision != state.revision {
            return Err(Failure::StaleRevision);
        }
        if self.inner.busy.load(Ordering::Acquire) || state.phase == Phase::Generating {
            return Err(Failure::Busy);
        }
        if state.phase != Phase::Ready || state.manifest.is_none() {
            return Err(Failure::NotLoaded);
        }
        Ok(state)
    }
    pub fn begin(&self, expected_revision: u64, request: Request) -> Result<Operation> {
        if self.inner.closing.is_cancelled() || !self.inner.status.borrow().accepting {
            return Err(Failure::ChildExited);
        }
        if self
            .inner
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Failure::Busy);
        }
        let id = format!("op-{}", uuid::Uuid::new_v4().simple());
        let cancel = Cancellation::new();
        let (progress_tx, progress) = watch::channel(Progress {
            request_id: id.clone(),
            revision: 0,
            text: String::new(),
            incomplete: matches!(request, Request::Generate(_)),
            finished: false,
        });
        let (reply, result) = oneshot::channel();
        let provenance = Arc::new(OnceLock::new());
        let cmd = CommandRequest {
            expected: expected_revision,
            id,
            request,
            cancel: cancel.clone(),
            progress: progress_tx,
            reply,
            provenance: provenance.clone(),
        };
        if self.inner.tx.try_send(cmd).is_err() {
            self.inner.busy.store(false, Ordering::Release);
            return Err(Failure::ChildExited);
        }
        Ok(Operation {
            cancel,
            result,
            progress,
            provenance,
        })
    }
    pub fn begin_shutdown(&self) {
        self.inner.closing.cancel()
    }
    pub async fn shutdown(&self) -> Result<Snapshot> {
        self.inner.closing.cancel();
        // Keep the handle in the mutex while awaiting it. Cancelling this waiter
        // leaves the next caller able to wait for the same child cleanup.
        let mut worker = self.inner.worker.lock().await;
        if let Some(handle) = worker.handle.as_mut() {
            worker.failed = handle.await.is_err();
            worker.handle = None;
        }
        let s = self.status();
        if worker.failed
            || s.accepting
            || s.spawned != s.reaped
            || s.failure == Some(Failure::Cleanup)
        {
            return Err(Failure::Cleanup);
        }
        Ok(s)
    }
}

struct Wire {
    child: Child,
    input: ChildStdin,
    events: mpsc::Receiver<Result<protocol::Frame>>,
    readers: Vec<JoinHandle<()>>,
    _scratch: tempfile::TempDir,
    scratch_root: std::path::PathBuf,
}
struct Closed {
    reaped: bool,
    failure: Option<Failure>,
    prior_failure: Option<Failure>,
    retained: Option<Wire>,
}
struct PendingScratch {
    _scratch: tempfile::TempDir,
    root: std::path::PathBuf,
    prior_failure: Failure,
}
struct StartupFailure {
    error: Failure,
    retained: Option<PendingScratch>,
}
impl From<Failure> for StartupFailure {
    fn from(error: Failure) -> Self {
        Self {
            error,
            retained: None,
        }
    }
}
impl Wire {
    async fn spawn(
        plan: &LaunchPlan,
        cancel: &Cancellation,
    ) -> std::result::Result<Self, StartupFailure> {
        plan.unchanged_worker()?;
        let mut scratch_builder = tempfile::Builder::new();
        scratch_builder.prefix("smith-mlx-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            scratch_builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let scratch = scratch_builder.tempdir().map_err(|_| Failure::Io)?;
        let canonical = std::fs::canonicalize(scratch.path()).map_err(|_| Failure::Io)?;
        let selected = crate::bundle::snapshot(plan, &canonical, cancel).await;
        let executable = match selected {
            Ok(p) => p,
            Err(e) => {
                return Err(reject_before_spawn(scratch, canonical, e).await);
            }
        };
        let final_check = plan.unchanged_worker().and_then(|_| {
            if cancel.is_cancelled() {
                Err(Failure::Cancelled)
            } else {
                Ok(())
            }
        });
        if let Err(e) = final_check {
            return Err(reject_before_spawn(scratch, canonical, e).await);
        }
        let spawned = Command::new(&executable)
            .arg("--scratch-root")
            .arg(&canonical)
            .env_clear()
            .env("LANG", "C")
            .current_dir(&canonical)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(_) => {
                return Err(reject_before_spawn(scratch, canonical, Failure::Spawn).await);
            }
        };
        let input = child.stdin.take().ok_or(Failure::Spawn)?;
        let mut output = child.stdout.take().ok_or(Failure::Spawn)?;
        let mut errors = child.stderr.take().ok_or(Failure::Spawn)?;
        let (tx, events) = mpsc::channel(8);
        let err_tx = tx.clone();
        let stdout = tokio::spawn(async move {
            let mut frame = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = match output.read(&mut chunk).await {
                    Ok(0) => {
                        let e = if frame.is_empty() {
                            Failure::ChildExited
                        } else {
                            Failure::Protocol
                        };
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                    Ok(n) => n,
                    Err(_) => {
                        let _ = tx.send(Err(Failure::Io)).await;
                        break;
                    }
                };
                for byte in &chunk[..n] {
                    if *byte == b'\n' {
                        let parsed = protocol::parse(&frame);
                        let bad = parsed.is_err();
                        if tx.send(parsed).await.is_err() || bad {
                            return;
                        }
                        frame.clear();
                    } else {
                        if frame.len() >= FRAME_BYTES {
                            let _ = tx.send(Err(Failure::OutputLimit)).await;
                            return;
                        }
                        frame.push(*byte);
                    }
                }
            }
        });
        let stderr = tokio::spawn(async move {
            let mut total = 0usize;
            let mut b = [0; 4096];
            loop {
                match errors.read(&mut b).await {
                    Ok(0) => return,
                    Ok(n) => {
                        total += n;
                        if total > 131072 {
                            let _ = err_tx.send(Err(Failure::OutputLimit)).await;
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = err_tx.send(Err(Failure::Io)).await;
                        return;
                    }
                }
            }
        });
        Ok(Self {
            child,
            input,
            events,
            readers: vec![stdout, stderr],
            _scratch: scratch,
            scratch_root: canonical,
        })
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| Failure::InvalidRequest)?;
        if bytes.len() > FRAME_BYTES {
            return Err(Failure::InvalidRequest);
        }
        bytes.push(b'\n');
        time::timeout(Duration::from_secs(2), self.input.write_all(&bytes))
            .await
            .map_err(|_| Failure::Deadline)?
            .map_err(|_| Failure::Io)
    }
    async fn event(&mut self) -> Result<protocol::Frame> {
        self.events.recv().await.ok_or(Failure::ChildExited)?
    }
    async fn close(mut self, graceful: bool, budgets: Budgets) -> Closed {
        let mut failure = None;
        if graceful {
            let id = format!("shutdown-{}", uuid::Uuid::new_v4().simple());
            let handshake = async {
                self.send(json!({"v":1,"id":id,"op":"shutdown"})).await?;
                time::timeout(budgets.drain, async {
                    let f = self.event().await?;
                    if f.id != id || f.event != "done" {
                        return Err(Failure::Protocol);
                    }
                    protocol::keys(&f.value, &[])
                })
                .await
                .map_err(|_| Failure::Deadline)?
            }
            .await;
            failure = handshake.err();
        }
        if !graceful || failure.is_some() {
            let _ = self.child.start_kill();
        }
        let waited = time::timeout(budgets.reap, self.child.wait()).await;
        if let Ok(Ok(exit)) = waited {
            if graceful && !exit.success() && failure.is_none() {
                failure = Some(Failure::ChildExited);
            }
        } else {
            if graceful && failure.is_none() {
                failure = Some(Failure::Deadline);
            }
            let _ = self.child.start_kill();
            if !matches!(
                time::timeout(budgets.reap, self.child.wait()).await,
                Ok(Ok(_))
            ) {
                return Closed {
                    reaped: false,
                    failure: Some(Failure::Cleanup),
                    prior_failure: failure,
                    retained: Some(self),
                };
            }
        }
        for reader in self.readers.drain(..) {
            reader.abort();
            let _ = reader.await;
        }
        if !removed_scratch(&self.scratch_root).await {
            return Closed {
                reaped: true,
                failure: Some(Failure::Cleanup),
                prior_failure: failure,
                retained: Some(self),
            };
        }
        Closed {
            reaped: true,
            failure,
            prior_failure: None,
            retained: None,
        }
    }
}
fn publish(tx: &watch::Sender<Snapshot>, state: &mut Snapshot) {
    state.revision += 1;
    tx.send_replace(state.clone());
}
async fn actor(
    plan: LaunchPlan,
    mut requests: mpsc::Receiver<CommandRequest>,
    status: watch::Sender<Snapshot>,
    closing: Cancellation,
    busy: Arc<AtomicBool>,
    ready: oneshot::Sender<Result<()>>,
) {
    let mut state = status.borrow().clone();
    let startup = async {
        plan.revalidate(&closing).await?;
        if closing.is_cancelled() {
            return Err(StartupFailure::from(Failure::Cancelled));
        }
        Wire::spawn(&plan, &closing).await
    }
    .await;
    let mut wire = match startup {
        Ok(w) => w,
        Err(e) => {
            state.phase = Phase::Failed;
            state.failure = Some(e.error.clone());
            state.accepting = false;
            publish(&status, &mut state);
            if let Some(retained) = e.retained {
                tokio::spawn(retain_scratch(retained, status, state.clone()));
            }
            let _ = ready.send(Err(e.error));
            return;
        }
    };
    state.pid = wire.child.id();
    state.spawned = 1;
    publish(&status, &mut state);
    let hello_id = format!("hello-{}", uuid::Uuid::new_v4().simple());
    let handshake = async {
        wire.send(json!({"v":1,"id":hello_id,"op":"hello"})).await?;
        protocol::hello(wire.event().await?, &hello_id)
    };
    let handshake = tokio::select! {biased;_ = closing.cancelled()=>Err(Failure::Cancelled),r=time::timeout(plan.summary.budgets.hello,handshake)=>r.map_err(|_|Failure::Deadline).and_then(|x|x)};
    state.worker_features = match handshake {
        Ok(features) => features,
        Err(e) => {
            finish(wire, false, &plan, status, &mut state, Some(e.clone())).await;
            let _ = ready.send(Err(e));
            return;
        }
    };
    state.phase = Phase::Idle;
    publish(&status, &mut state);
    let _ = ready.send(Ok(()));
    let mut count = 1usize;
    loop {
        let cmd = tokio::select! {
            biased;
            _ = closing.cancelled()=>{
                requests.close();
                finish(wire,true,&plan,status,&mut state,None).await;
                reject_pending(&mut requests,&busy,Failure::Cancelled);
                return;
            },
            event=wire.event()=>{
                let failure=event.err().unwrap_or(Failure::Protocol);
                requests.close();
                finish(wire,false,&plan,status,&mut state,Some(failure.clone())).await;
                reject_pending(&mut requests,&busy,failure);
                return;
            },
            c=requests.recv()=>match c{Some(c)=>c,None=>{finish(wire,true,&plan,status,&mut state,None).await;return}}
        };
        let validation = if cmd.expected != state.revision {
            Err(Failure::StaleRevision)
        } else if count > 4092 {
            Err(Failure::RequestLimit)
        } else {
            match &cmd.request {
                Request::Load if state.manifest.is_some() => Err(Failure::Busy),
                Request::Generate(g) => state
                    .manifest
                    .as_ref()
                    .ok_or(Failure::NotLoaded)
                    .and_then(|m| g.validate(m, state.worker_features)),
                _ => Ok(()),
            }
        };
        if let Err(e) = validation {
            busy.store(false, Ordering::Release);
            cmd.progress.send_modify(|p| p.finished = true);
            let _ = cmd.reply.send(Err(e));
            continue;
        }
        if let Request::Generate(g) = &cmd.request {
            let _ = cmd
                .provenance
                .set(RuntimeIdentity::new(&plan.summary).capture(&state, g));
        }
        count += 1;
        state.phase = match cmd.request {
            Request::Inspect => Phase::Inspecting,
            Request::Load => Phase::Loading,
            Request::Generate(_) => Phase::Generating,
            Request::Unload => Phase::Unloading,
        };
        publish(&status, &mut state);
        let (result, retire, cancel_sent) = run_operation(
            &mut wire,
            &plan,
            &cmd,
            &closing,
            state
                .manifest
                .as_ref()
                .map(|m| m.context_limit)
                .unwrap_or(0),
        )
        .await;
        count += usize::from(cancel_sent);
        if retire {
            let failure = result.as_ref().err().cloned().or(Some(Failure::Cancelled));
            finish(wire, false, &plan, status, &mut state, failure).await;
            cmd.progress.send_modify(|p| {
                p.finished = true;
                p.incomplete = true
            });
            busy.store(false, Ordering::Release);
            let result = if state.reaped != state.spawned || state.failure == Some(Failure::Cleanup)
            {
                Err(Failure::Cleanup)
            } else {
                result
            };
            let _ = cmd.reply.send(result);
            return;
        }
        if let Ok(Outcome::Manifest(m)) = &result {
            if matches!(cmd.request, Request::Load) {
                state.manifest = Some(m.clone())
            }
            state.inspection = Some(m.clone());
        }
        if matches!(result, Ok(Outcome::Unloaded)) {
            state.manifest = None;
        }
        state.phase = if state.manifest.is_some() {
            Phase::Ready
        } else {
            Phase::Idle
        };
        state.failure = None;
        publish(&status, &mut state);
        busy.store(false, Ordering::Release);
        cmd.progress.send_modify(|p| p.finished = true);
        let _ = cmd.reply.send(result);
    }
}
fn reject_pending(
    requests: &mut mpsc::Receiver<CommandRequest>,
    busy: &AtomicBool,
    failure: Failure,
) {
    while let Ok(cmd) = requests.try_recv() {
        cmd.progress.send_modify(|p| p.finished = true);
        let _ = cmd.reply.send(Err(failure.clone()));
    }
    busy.store(false, Ordering::Release);
}
async fn finish(
    wire: Wire,
    graceful: bool,
    plan: &LaunchPlan,
    status: watch::Sender<Snapshot>,
    state: &mut Snapshot,
    failure: Option<Failure>,
) {
    state.phase = Phase::Stopping;
    state.accepting = false;
    publish(&status, state);
    let result = wire.close(graceful, plan.summary.budgets).await;
    if result.reaped {
        state.reaped = state.spawned;
        state.pid = None;
    }
    state.manifest = None;
    let prior_failure = result.prior_failure.or(failure);
    state.failure = result.failure.or(prior_failure.clone());
    state.phase = if state.failure.is_some() {
        Phase::Failed
    } else {
        Phase::Stopped
    };
    publish(&status, state);
    if let Some(wire) = result.retained {
        tokio::spawn(retain_cleanup(wire, status, state.clone(), prior_failure));
    }
}
async fn removed_scratch(scratch: &std::path::Path) -> bool {
    if crate::bundle::make_removable(scratch).is_err() {
        return false;
    }
    match tokio::fs::remove_dir_all(scratch).await {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}
async fn reject_before_spawn(
    scratch: tempfile::TempDir,
    root: std::path::PathBuf,
    cause: Failure,
) -> StartupFailure {
    if removed_scratch(&root).await {
        StartupFailure::from(cause)
    } else {
        StartupFailure {
            error: Failure::Cleanup,
            retained: Some(PendingScratch {
                _scratch: scratch,
                root,
                prior_failure: cause,
            }),
        }
    }
}
async fn retain_scratch(
    pending: PendingScratch,
    status: watch::Sender<Snapshot>,
    mut state: Snapshot,
) {
    while !removed_scratch(&pending.root).await {
        time::sleep(Duration::from_secs(1)).await;
    }
    state.failure = Some(pending.prior_failure.clone());
    state.phase = Phase::Failed;
    drop(pending);
    publish(&status, &mut state);
}
async fn retain_cleanup(
    mut wire: Wire,
    status: watch::Sender<Snapshot>,
    mut state: Snapshot,
    prior_failure: Option<Failure>,
) {
    // A failed wait is not proof of exit. Keep the exact child, readers and
    // scratch alive across errors; never release model bytes before reap.
    while state.spawned != state.reaped {
        time::sleep(Duration::from_secs(1)).await;
        if wire.child.wait().await.is_ok() {
            state.reaped = state.spawned;
            state.pid = None;
            publish(&status, &mut state);
        }
    }
    for reader in wire.readers.drain(..) {
        reader.abort();
        let _ = reader.await;
    }
    while !removed_scratch(&wire.scratch_root).await {
        time::sleep(Duration::from_secs(1)).await;
    }
    drop(wire);
    state.failure = prior_failure;
    state.phase = if state.failure.is_some() {
        Phase::Failed
    } else {
        Phase::Stopped
    };
    publish(&status, &mut state);
}
async fn run_operation(
    wire: &mut Wire,
    plan: &LaunchPlan,
    cmd: &CommandRequest,
    closing: &Cancellation,
    context_limit: u32,
) -> (Result<Outcome>, bool, bool) {
    if closing.is_cancelled() || cmd.cancel.is_cancelled() {
        return (Err(Failure::Cancelled), false, false);
    }
    let request = match protocol::request(&cmd.id, &cmd.request, &plan.summary.model) {
        Ok(r) => r,
        Err(e) => return (Err(e), false, false),
    };
    if let Err(e) = wire.send(request).await {
        return (Err(e), true, false);
    }
    let mut started = false;
    let mut frames = 0u32;
    let mut cancel_id = None;
    let mut cancel_ack = false;
    let mut terminal = None;
    let budget = if matches!(cmd.request, Request::Generate(_)) {
        plan.summary.budgets.generation
    } else {
        plan.summary.budgets.operation
    };
    let mut deadline = Instant::now() + budget;
    let mut cause = Failure::Cancelled;
    loop {
        if cancel_id.is_none() || cancel_ack {
            if let Some(result) = terminal.take() {
                return (result, cancel_id.is_some(), cancel_id.is_some());
            }
        }
        let frame = tokio::select! {biased;
            _ = closing.cancelled(),if cancel_id.is_none()=>None,
            _ = cmd.cancel.cancelled(),if cancel_id.is_none()=>None,
            _ = time::sleep_until(deadline)=>{if cancel_id.is_some(){return(cancelled_result(cmd,cause),true,true)}cause=Failure::Deadline;None},
            result=wire.event()=>match result{Ok(f)=>Some(f),Err(e)=>return(Err(e),true,cancel_id.is_some())}
        };
        let Some(frame) = frame else {
            let id = format!("cancel-{}", uuid::Uuid::new_v4().simple());
            if let Err(e) = wire
                .send(json!({"v":1,"id":id,"op":"cancel","target_id":cmd.id}))
                .await
            {
                return (Err(e), true, true);
            }
            cancel_id = Some(id);
            deadline = Instant::now() + plan.summary.budgets.drain;
            continue;
        };
        frames += 1;
        if frames > 8192 {
            return (Err(Failure::OutputLimit), true, cancel_id.is_some());
        }
        if cancel_id.as_deref() == Some(frame.id.as_str()) {
            let valid = if frame.event == "done" {
                protocol::keys(&frame.value, &["target_id", "cancel_requested"]).is_ok()
                    && frame.value["target_id"] == cmd.id
                    && frame.value["cancel_requested"] == true
            } else if frame.event == "error" {
                protocol::worker_error(&frame)
                    == Ok(Failure::Worker {
                        code: "not_active".into(),
                    })
            } else {
                false
            };
            if cancel_ack || !valid {
                return (Err(Failure::Protocol), true, true);
            }
            cancel_ack = true;
            continue;
        }
        if frame.id != cmd.id || terminal.is_some() {
            return (Err(Failure::Protocol), true, cancel_id.is_some());
        }
        let result = match frame.event.as_str() {
            "started" => {
                if started || protocol::keys(&frame.value, &[]).is_err() {
                    Err(Failure::Protocol)
                } else {
                    started = true;
                    continue;
                }
            }
            "delta" => {
                if !started || !matches!(cmd.request, Request::Generate(_)) {
                    Err(Failure::Protocol)
                } else {
                    let mut p = cmd.progress.borrow().clone();
                    match protocol::patch(&frame, &mut p) {
                        Ok(()) => {
                            cmd.progress.send_replace(p);
                            continue;
                        }
                        Err(e) => Err(e),
                    }
                }
            }
            "error" => {
                let e = protocol::worker_error(&frame).unwrap_or(Failure::Protocol);
                if cancel_id.is_some()
                    && e == (Failure::Worker {
                        code: "cancelled".into(),
                    })
                {
                    cancelled_result(cmd, cause.clone())
                } else {
                    Err(e)
                }
            }
            "done" => {
                if !started {
                    Err(Failure::Protocol)
                } else {
                    match &cmd.request {
                        Request::Inspect | Request::Load => {
                            protocol::manifest(&frame, &plan.summary.model).map(Outcome::Manifest)
                        }
                        Request::Unload => {
                            protocol::keys(&frame.value, &[]).map(|_| Outcome::Unloaded)
                        }
                        Request::Generate(g) => completion(&frame, cmd, g, context_limit),
                    }
                }
            }
            _ => Err(Failure::Protocol),
        };
        if cancel_id.is_some() {
            if result.is_err() {
                return (result, true, true);
            }
            terminal = Some(cancelled_result(cmd, cause.clone()));
        } else if result.is_err() {
            return (result, true, false);
        } else {
            terminal = Some(result)
        }
    }
}
fn cancelled_result(cmd: &CommandRequest, cause: Failure) -> Result<Outcome> {
    if matches!(cmd.request, Request::Generate(_)) && cause == Failure::Cancelled {
        Ok(Outcome::Completion(Completion {
            text: cmd.progress.borrow().text.clone(),
            incomplete: true,
            input_tokens: None,
            output_tokens: None,
            finish_reason: FinishReason::Cancelled,
            thinking_mode: None,
        }))
    } else {
        Err(cause)
    }
}
fn completion(
    frame: &protocol::Frame,
    cmd: &CommandRequest,
    g: &GenerationRequest,
    context_limit: u32,
) -> Result<Outcome> {
    let confirmed_thinking = g.wire_thinking_mode();
    if let Some(mode) = confirmed_thinking {
        protocol::keys(
            &frame.value,
            &[
                "input_tokens",
                "output_tokens",
                "finish_reason",
                "thinking_mode",
            ],
        )?;
        let acknowledged: ThinkingMode =
            serde_json::from_value(frame.value["thinking_mode"].clone())
                .map_err(|_| Failure::Protocol)?;
        if acknowledged != mode {
            return Err(Failure::Protocol);
        }
    } else {
        protocol::keys(
            &frame.value,
            &["input_tokens", "output_tokens", "finish_reason"],
        )?;
    }
    let input = frame.value["input_tokens"]
        .as_u64()
        .filter(|n| *n <= 8192)
        .ok_or(Failure::Protocol)? as u32;
    let sampling = g.sampling();
    let output = frame.value["output_tokens"]
        .as_u64()
        .filter(|n| *n <= u64::from(sampling.max_tokens))
        .ok_or(Failure::Protocol)? as u32;
    if input + output > context_limit {
        return Err(Failure::Protocol);
    }
    let reason: FinishReason = serde_json::from_value(frame.value["finish_reason"].clone())
        .map_err(|_| Failure::Protocol)?;
    let incomplete = reason == FinishReason::Cancelled;
    cmd.progress.send_modify(|p| p.incomplete = incomplete);
    Ok(Outcome::Completion(Completion {
        text: cmd.progress.borrow().text.clone(),
        incomplete,
        input_tokens: Some(input),
        output_tokens: Some(output),
        finish_reason: reason,
        // A matching wire echo does not turn an incomplete terminal into a
        // successful acknowledgement. Requested intent remains in provenance.
        thinking_mode: if incomplete { None } else { confirmed_thinking },
    }))
}

#[cfg(all(test, unix))]
mod cleanup_tests {
    use super::*;
    #[tokio::test]
    async fn pre_spawn_cleanup_failure_retains_ownership_and_original_failure() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(scratch.path()).unwrap();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("snapshot"), b"synthetic pre-spawn bytes").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let failure = reject_before_spawn(scratch, root.clone(), Failure::ConsentMismatch).await;
        assert_eq!(failure.error, Failure::Cleanup);
        let pending = failure.retained.unwrap();
        assert!(root.exists());
        let state = Snapshot {
            session_id: "test".into(),
            revision: 1,
            phase: Phase::Failed,
            pid: None,
            manifest: None,
            inspection: None,
            failure: Some(Failure::Cleanup),
            spawned: 0,
            reaped: 0,
            accepting: false,
            backend: "mlx_swift_metal_only",
            execution_kind: ExecutionKind::CompatibleFixture,
            worker_features: WorkerFeatures::default(),
        };
        let (tx, rx) = watch::channel(state.clone());
        let task = tokio::spawn(retain_scratch(pending, tx, state));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(!root.exists());
        assert_eq!(rx.borrow().failure, Some(Failure::ConsentMismatch));
        assert_eq!(rx.borrow().spawned, 0);
    }
}
