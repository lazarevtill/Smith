use crate::{
    plan,
    startup::StartupCapture,
    transport::{self, Endpoint},
    *,
};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{self, Instant},
};

#[derive(Clone)]
pub struct Runtime(Arc<Handle>);
struct Handle {
    tx: mpsc::Sender<Request>,
    state: watch::Receiver<Snapshot>,
    shutdown: Cancellation,
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
struct CancelOnDrop(Option<Cancellation>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel();
        }
    }
}

/// The run handle owns a generation lease until a terminal outcome is committed.
/// Dropping/cancelling it stops and reaps the entire owned runtime, then releases
/// the lease. A caller must retain it until `wait` completes for cleanup evidence.
pub struct Generation {
    pub target: Target,
    pub run_id: Uuid,
    cancel: Cancellation,
    result: oneshot::Receiver<Result<Completion>>,
    finished: bool,
}
impl Generation {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
    pub async fn wait(&mut self) -> Result<Completion> {
        let result = (&mut self.result)
            .await
            .map_err(|_| Failure::ShuttingDown)?;
        self.finished = true;
        result
    }
}
impl Drop for Generation {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel.cancel();
        }
    }
}
impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
impl Runtime {
    /// Requires an active Tokio runtime. Creates no process, listener, or probe.
    pub fn new() -> Self {
        let supported = plan::platform_supported();
        let snapshot = Snapshot {
            target: Target {
                owner: Uuid::new_v4(),
                generation: 0,
            },
            revision: 0,
            phase: if supported {
                Phase::Stopped
            } else {
                Phase::Unsupported
            },
            model_id: None,
            plan_id: None,
            pid: None,
            active_leases: 0,
            execution_kind: None,
            requested_backend: None,
            observed_backend: Evidence::Unknown,
            process_tree_cleanup: Evidence::Unsupported,
            parent_crash_cleanup: Evidence::Unsupported,
            text_chat: Evidence::Unknown,
            failure: (!supported).then_some(Failure::UnsupportedPlatform),
            spawned: 0,
            reaped: 0,
            accepting: true,
        };
        let (tx, rx) = mpsc::channel(32);
        let (state, state_rx) = watch::channel(snapshot.clone());
        let shutdown = Cancellation::new();
        let (events, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(
            Actor {
                rx,
                state,
                shutdown: shutdown.clone(),
                snapshot,
                child: None,
                startup: None,
                job: None,
                events,
                event_rx,
                last_used: Instant::now(),
            }
            .drive(),
        );
        Self(Arc::new(Handle {
            tx,
            state: state_rx,
            shutdown,
        }))
    }
    /// Immediate last observed state. Actor checks child liveness every 20 ms.
    pub fn status(&self) -> Snapshot {
        self.0.state.borrow().clone()
    }
    /// Requests a fresh actor liveness observation without spawning/network IO.
    pub async fn refresh_status(&self) -> Result<Snapshot> {
        if !self.status().accepting {
            return Ok(self.status());
        }
        let (tx, rx) = oneshot::channel();
        self.0
            .tx
            .send(Request::Status(tx))
            .await
            .map_err(|_| Failure::ShuttingDown)?;
        rx.await.map_err(|_| Failure::ShuttingDown)
    }
    /// Bounded startup metadata for the exact current owner/generation. Returns
    /// None until retention freezes before Ready, or when no child was spawned.
    /// Available after unload, but not after shutdown closes this actor.
    pub async fn startup_observation(&self, target: Target) -> Result<Option<StartupObservation>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .tx
            .send(Request::StartupObservation { target, reply: tx })
            .await
            .map_err(|_| Failure::ShuttingDown)?;
        rx.await.map_err(|_| Failure::ShuttingDown)?
    }
    /// Explicit trusted local diagnostics. Raw lines may contain private paths;
    /// do not expose this through HTTP, logs, telemetry, or user-model context.
    /// Full endpoint-key occurrences are redacted. Prompt-time output is never
    /// retained. The same exact-target and retention rules as metadata apply.
    pub async fn startup_diagnostics(&self, target: Target) -> Result<Option<StartupDiagnostics>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .tx
            .send(Request::Startup { target, reply: tx })
            .await
            .map_err(|_| Failure::ShuttingDown)?;
        rx.await.map_err(|_| Failure::ShuttingDown)?
    }
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.0.state.clone()
    }
    pub async fn load(
        &self,
        expected_revision: u64,
        plan: ApprovedPlan,
        cancel: Cancellation,
    ) -> Result<Snapshot> {
        let mut guard = CancelOnDrop(Some(cancel.clone()));
        if self.0.shutdown.is_cancelled() {
            return Err(Failure::ShuttingDown);
        }
        // Hashing is cancellable and outside actor admission. B validates before A
        // is disturbed; the actor checks metadata again at the spawn boundary.
        tokio::select! { biased;
            _ = self.0.shutdown.cancelled() => return Err(Failure::ShuttingDown),
            result = plan.revalidate(&cancel) => result?,
        }
        let (tx, rx) = oneshot::channel();
        tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Failure::Cancelled),
            _ = self.0.shutdown.cancelled() => return Err(Failure::ShuttingDown),
            result = self.0.tx.send(Request::Load { expected_revision, plan: Box::new(plan), cancel: cancel.clone(), reply: tx }) => result.map_err(|_| Failure::ShuttingDown)?,
        }
        // Once admitted, cancellation returns only after actor cleanup. Dropping
        // this future also latches cancellation via guard.
        let result = rx.await.map_err(|_| Failure::ShuttingDown)?;
        guard.0.take();
        result
    }
    pub async fn start_generation(
        &self,
        target: Target,
        capability: Capability,
        request: TextRequest,
        cancel: Cancellation,
    ) -> Result<Generation> {
        self.start_generation_format(
            target,
            capability,
            request,
            cancel,
            crate::task_schema::ResponseFormat::Text,
        )
        .await
    }
    /// Request the fixed Smith task-step JSON envelope from the selected runtime.
    /// This does not grant tools or certify schema adherence: the host must still
    /// validate the exact completion, available tools, arguments and authority.
    /// Errors never retry as plain text; returned text is preserved unchanged.
    pub async fn start_task_generation(
        &self,
        target: Target,
        request: TextRequest,
        cancel: Cancellation,
    ) -> Result<Generation> {
        self.start_generation_format(
            target,
            Capability::TextChat,
            request,
            cancel,
            crate::task_schema::ResponseFormat::TaskStepV1,
        )
        .await
    }
    async fn start_generation_format(
        &self,
        target: Target,
        capability: Capability,
        request: TextRequest,
        cancel: Cancellation,
        format: crate::task_schema::ResponseFormat,
    ) -> Result<Generation> {
        let mut guard = CancelOnDrop(Some(cancel.clone()));
        let (tx, rx) = oneshot::channel();
        tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Failure::Cancelled),
            _ = self.0.shutdown.cancelled() => return Err(Failure::ShuttingDown),
            result = self.0.tx.send(Request::Generate { target, capability, request, format, cancel: cancel.clone(), reply: tx }) => result.map_err(|_| Failure::ShuttingDown)?,
        }
        let result = rx.await.map_err(|_| Failure::ShuttingDown)?;
        // Ownership of cancellation moves to Generation, rather than cancelling
        // the admitted run as this method returns.
        guard.0.take();
        result
    }
    pub async fn unload(&self, target: Target, model_id: String) -> Result<Snapshot> {
        self.stop_request(target, model_id, None).await
    }
    pub async fn unload_if_idle(
        &self,
        target: Target,
        model_id: String,
        threshold: Duration,
    ) -> Result<Snapshot> {
        self.stop_request(target, model_id, Some(threshold)).await
    }
    async fn stop_request(
        &self,
        target: Target,
        model_id: String,
        idle: Option<Duration>,
    ) -> Result<Snapshot> {
        let (tx, rx) = oneshot::channel();
        self.0
            .tx
            .send(Request::Unload {
                target,
                model_id,
                idle,
                reply: tx,
            })
            .await
            .map_err(|_| Failure::ShuttingDown)?;
        rx.await.map_err(|_| Failure::ShuttingDown)?
    }
    /// Idempotent. A Cleanup error means the child is still retained by the actor
    /// for subsequent reap attempts; status never falsely claims Stopped.
    pub async fn shutdown(&self) -> Result<Snapshot> {
        let mut rx = self.0.state.clone();
        self.0.shutdown.cancel();
        loop {
            let state = rx.borrow_and_update().clone();
            if !state.accepting && matches!(state.phase, Phase::Stopped | Phase::CleanupFailed) {
                return if state.phase == Phase::CleanupFailed {
                    Err(Failure::Cleanup)
                } else {
                    Ok(state)
                };
            }
            rx.changed().await.map_err(|_| Failure::Cleanup)?;
        }
    }
}

enum Request {
    Status(oneshot::Sender<Snapshot>),
    StartupObservation {
        target: Target,
        reply: oneshot::Sender<Result<Option<StartupObservation>>>,
    },
    Startup {
        target: Target,
        reply: oneshot::Sender<Result<Option<StartupDiagnostics>>>,
    },
    Load {
        expected_revision: u64,
        plan: Box<ApprovedPlan>,
        cancel: Cancellation,
        reply: oneshot::Sender<Result<Snapshot>>,
    },
    Generate {
        target: Target,
        capability: Capability,
        request: TextRequest,
        format: crate::task_schema::ResponseFormat,
        cancel: Cancellation,
        reply: oneshot::Sender<Result<Generation>>,
    },
    Unload {
        target: Target,
        model_id: String,
        idle: Option<Duration>,
        reply: oneshot::Sender<Result<Snapshot>>,
    },
}
struct OwnedChild {
    child: Child,
    endpoint: Endpoint,
    plan: ApprovedPlan,
    startup: StartupCapture,
    output_readers: Vec<JoinHandle<()>>,
    _directory: tempfile::TempDir,
}
impl OwnedChild {
    fn spawn(plan: ApprovedPlan, target: Target) -> Result<Self> {
        plan.unchanged()?;
        let directory = plan::socket_directory()?;
        let socket = directory.path().join("runtime.sock");
        let key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let manifest = &plan.summary().manifest;
        let config = &plan.summary().config;
        let mut command = Command::new(&manifest.executable.path);
        command
            .env_clear()
            .env("LLAMA_API_KEY", &key)
            .env("LANG", "C")
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .arg("--model")
            .arg(&manifest.model.path)
            .arg("--alias")
            .arg(&manifest.model_id)
            .arg("--host")
            .arg(&socket)
            .args([
                "--port",
                "0",
                "--parallel",
                "1",
                "--no-webui",
                "--no-agent",
                "--offline",
                "--no-mmproj",
                "--fit",
                "off",
            ])
            .arg("--ctx-size")
            .arg(config.context_tokens.to_string())
            .arg("--threads")
            .arg(config.threads.to_string());
        match &config.backend {
            BackendRequest::Cpu => {
                command.args(["--device", "none", "--n-gpu-layers", "0"]);
            }
            BackendRequest::Device { name, gpu_layers } => {
                command
                    .arg("--device")
                    .arg(name)
                    .arg("--n-gpu-layers")
                    .arg(gpu_layers.to_string());
            }
        }
        command.args(&plan.summary().advanced_argv);
        command.args(&plan.summary().startup_logging_argv);
        let mut child = command.spawn().map_err(|_| Failure::Spawn)?;
        let pid = child.id().ok_or(Failure::Spawn)?;
        let startup = StartupCapture::new(
            StartupIdentity {
                target,
                plan_id: plan.summary().id.clone(),
                executable_sha256: manifest.executable.sha256.clone(),
                execution_kind: manifest.execution_kind,
                pid,
            },
            key.clone(),
        );
        let output_readers = vec![
            startup.reader(
                child.stdout.take().expect("piped stdout"),
                StartupStream::Stdout,
            ),
            startup.reader(
                child.stderr.take().expect("piped stderr"),
                StartupStream::Stderr,
            ),
        ];
        let endpoint = Endpoint {
            path: socket,
            pid,
            key,
            model: manifest.model_id.clone(),
        };
        Ok(Self {
            child,
            endpoint,
            plan,
            startup,
            output_readers,
            _directory: directory,
        })
    }
    async fn close_output(&mut self) {
        self.startup.freeze();
        // Do not wait for EOF: an unsupported descendant could retain a pipe.
        // Abort and join every reader before dropping the owned process slot.
        for reader in &self.output_readers {
            reader.abort();
        }
        for reader in self.output_readers.drain(..) {
            let _ = reader.await;
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.startup.freeze();
        for reader in &self.output_readers {
            reader.abort();
        }
    }
}
enum JobReply {
    Load(oneshot::Sender<Result<Snapshot>>),
    Generate(oneshot::Sender<Result<Completion>>),
}
struct Job {
    id: Uuid,
    task: JoinHandle<()>,
    cancel: Cancellation,
    deadline: Instant,
    reply: JobReply,
}
enum JobResult {
    Loaded,
    Generated(Completion),
}
struct Event {
    id: Uuid,
    result: Result<JobResult>,
}
struct Actor {
    rx: mpsc::Receiver<Request>,
    state: watch::Sender<Snapshot>,
    shutdown: Cancellation,
    snapshot: Snapshot,
    child: Option<OwnedChild>,
    startup: Option<StartupCapture>,
    job: Option<Job>,
    events: mpsc::UnboundedSender<Event>,
    event_rx: mpsc::UnboundedReceiver<Event>,
    last_used: Instant,
}
impl Actor {
    fn publish(&mut self) {
        self.snapshot.revision += 1;
        self.state.send_replace(self.snapshot.clone());
    }
    fn target(&self, target: Target) -> Result<()> {
        if self.snapshot.target != target {
            Err(Failure::StaleGeneration)
        } else {
            Ok(())
        }
    }
    async fn drive(mut self) {
        let mut tick = time::interval(Duration::from_millis(20));
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { biased;
                _ = self.shutdown.cancelled(), if self.snapshot.accepting => {
                    self.snapshot.accepting = false;
                    self.publish();
                    if self.job.is_some() { self.fail_job(Failure::ShuttingDown).await; }
                    else { let _ = self.cleanup(Phase::Stopped, None).await; }
                    self.rx.close();
                },
                _ = tick.tick() => self.maintenance().await,
                Some(event) = self.event_rx.recv() => self.event(event).await,
                request = self.rx.recv(), if !self.rx.is_closed() || !self.rx.is_empty() => {
                    if let Some(request) = request { self.command(request).await; }
                    else { self.shutdown.cancel(); }
                },
            }
            if !self.snapshot.accepting && self.child.is_none() {
                self.snapshot.phase = Phase::Stopped;
                self.snapshot.failure = None;
                self.publish();
                break;
            }
        }
    }
    async fn maintenance(&mut self) {
        if let Some(job) = &self.job {
            let error = if job.cancel.is_cancelled() {
                Some(Failure::Cancelled)
            } else if Instant::now() >= job.deadline {
                Some(match job.reply {
                    JobReply::Load(_) => Failure::StartupTimeout,
                    JobReply::Generate(_) => Failure::GenerationTimeout,
                })
            } else if self
                .child
                .as_ref()
                .is_some_and(|c| transport::exited_without_reaping(c.endpoint.pid))
            {
                Some(Failure::ChildExited)
            } else {
                None
            };
            if let Some(error) = error {
                self.fail_job(error).await;
            }
        } else if let Some(child) = &mut self.child {
            match child.child.try_wait() {
                Ok(Some(_)) => {
                    self.snapshot.reaped += 1;
                    child.close_output().await;
                    self.child = None;
                    self.clear_resident();
                    self.snapshot.phase = if self.snapshot.accepting {
                        Phase::Failed
                    } else {
                        Phase::Stopped
                    };
                    self.snapshot.failure = Some(Failure::ChildExited);
                    self.publish();
                }
                Err(_) => {
                    self.snapshot.phase = Phase::CleanupFailed;
                    self.snapshot.failure = Some(Failure::Cleanup);
                    self.publish();
                }
                Ok(None) => {
                    if self.snapshot.phase == Phase::CleanupFailed {
                        let _ = self.cleanup(Phase::Stopped, None).await;
                    }
                }
            }
        }
    }
    fn clear_resident(&mut self) {
        self.snapshot.pid = None;
        self.snapshot.model_id = None;
        self.snapshot.plan_id = None;
        self.snapshot.active_leases = 0;
        self.snapshot.text_chat = Evidence::Unknown;
        self.snapshot.execution_kind = None;
        self.snapshot.requested_backend = None;
        self.snapshot.observed_backend = Evidence::Unknown;
    }
    async fn cleanup(&mut self, phase: Phase, failure: Option<Failure>) -> Result<()> {
        self.snapshot.phase = Phase::Stopping;
        self.publish();
        if let Some(owned) = &mut self.child {
            owned.startup.freeze();
            // All transport workers were joined before cleanup; try_wait/wait
            // may now release the PID without any peer-proof reuse race.
            let _ = owned.child.start_kill();
            match time::timeout(owned.plan.summary().config.budgets.reap, owned.child.wait()).await
            {
                Ok(Ok(_)) => {
                    self.snapshot.reaped += 1;
                    owned.close_output().await;
                    self.child = None;
                }
                _ => {
                    self.snapshot.phase = Phase::CleanupFailed;
                    self.snapshot.failure = Some(Failure::Cleanup);
                    self.publish();
                    return Err(Failure::Cleanup);
                }
            }
        }
        self.clear_resident();
        self.snapshot.phase = phase;
        self.snapshot.failure = failure;
        self.publish();
        Ok(())
    }
    async fn fail_job(&mut self, reason: Failure) {
        let Some(job) = self.job.take() else {
            return;
        };
        job.task.abort();
        let _ = job.task.await;
        let phase = if matches!(reason, Failure::Cancelled | Failure::ShuttingDown) {
            Phase::Stopped
        } else {
            Phase::Failed
        };
        let error = self
            .cleanup(phase, Some(reason))
            .await
            .err()
            .unwrap_or(reason);
        match job.reply {
            JobReply::Load(reply) => {
                let _ = reply.send(Err(error));
            }
            JobReply::Generate(reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }
    async fn command(&mut self, request: Request) {
        if let Request::Status(reply) = request {
            self.maintenance().await;
            let _ = reply.send(self.snapshot.clone());
            return;
        }
        match request {
            Request::StartupObservation { target, reply } => {
                let result = self
                    .target(target)
                    .map(|()| self.startup.as_ref().and_then(StartupCapture::observation));
                let _ = reply.send(result);
            }
            Request::Startup { target, reply } => {
                let result = self
                    .target(target)
                    .map(|()| self.startup.as_ref().and_then(StartupCapture::diagnostics));
                let _ = reply.send(result);
            }
            Request::Load {
                expected_revision,
                plan,
                cancel,
                reply,
            } => {
                let rejected = if cancel.is_cancelled() {
                    Some(Failure::Cancelled)
                } else if !self.snapshot.accepting {
                    Some(Failure::ShuttingDown)
                } else if !plan::platform_supported() {
                    Some(Failure::UnsupportedPlatform)
                } else if plan.summary().manifest.protocol != Protocol::LlamaCppUnixHttpV1 {
                    Some(Failure::UnsupportedProtocol)
                } else if self.job.is_some() || self.snapshot.phase == Phase::CleanupFailed {
                    Some(Failure::Busy)
                } else if expected_revision != self.snapshot.revision {
                    Some(Failure::StaleRevision)
                } else {
                    plan.unchanged().err()
                };
                if let Some(e) = rejected {
                    let _ = reply.send(Err(e));
                    return;
                }
                if self.snapshot.phase == Phase::Ready
                    && self.snapshot.plan_id.as_deref() == Some(&plan.summary().id)
                {
                    self.maintenance().await;
                    if self.snapshot.phase == Phase::Ready {
                        let _ = reply.send(Ok(self.snapshot.clone()));
                        return;
                    }
                }
                if self.child.is_some() && self.cleanup(Phase::Stopped, None).await.is_err() {
                    let _ = reply.send(Err(Failure::Cleanup));
                    return;
                }
                if cancel.is_cancelled() || self.shutdown.is_cancelled() {
                    let _ = reply.send(Err(Failure::Cancelled));
                    return;
                }
                self.snapshot.target.generation += 1;
                self.startup = None;
                self.snapshot.failure = None;
                let child = match OwnedChild::spawn(*plan, self.snapshot.target) {
                    Ok(child) => child,
                    Err(e) => {
                        self.snapshot.phase = Phase::Failed;
                        self.snapshot.failure = Some(e);
                        self.publish();
                        let _ = reply.send(Err(e));
                        return;
                    }
                };
                self.startup = Some(child.startup.clone());
                self.snapshot.spawned += 1;
                self.snapshot.pid = Some(child.endpoint.pid);
                self.snapshot.model_id = Some(child.endpoint.model.clone());
                self.snapshot.plan_id = Some(child.plan.summary().id.clone());
                self.snapshot.execution_kind = Some(child.plan.summary().manifest.execution_kind);
                self.snapshot.requested_backend = Some(child.plan.summary().config.backend.clone());
                self.snapshot.observed_backend = Evidence::Unknown;
                self.snapshot.text_chat = Evidence::Unknown;
                self.snapshot.phase = Phase::Loading;
                self.publish();
                let id = Uuid::new_v4();
                let endpoint = child.endpoint.clone();
                let events = self.events.clone();
                let deadline = Instant::now() + child.plan.summary().config.budgets.startup;
                let task = tokio::spawn(async move {
                    let result = transport::ready(endpoint).await.map(|_| JobResult::Loaded);
                    let _ = events.send(Event { id, result });
                });
                self.child = Some(child);
                self.job = Some(Job {
                    id,
                    task,
                    cancel,
                    deadline,
                    reply: JobReply::Load(reply),
                });
            }
            Request::Generate {
                target,
                capability,
                request,
                format,
                cancel,
                reply,
            } => {
                let rejected = if cancel.is_cancelled() {
                    Some(Failure::Cancelled)
                } else if !self.snapshot.accepting {
                    Some(Failure::ShuttingDown)
                } else if let Err(e) = self.target(target) {
                    Some(e)
                } else if capability != Capability::TextChat {
                    Some(Failure::UnsupportedCapability)
                } else if self.job.is_some() {
                    Some(Failure::Busy)
                } else if self.snapshot.phase != Phase::Ready {
                    Some(Failure::NotReady)
                } else if !self
                    .child
                    .as_ref()
                    .is_some_and(|c| c.plan.summary().manifest.declares_text_chat)
                {
                    Some(Failure::UnsupportedCapability)
                } else {
                    None
                };
                if let Some(e) = rejected {
                    let _ = reply.send(Err(e));
                    return;
                }
                self.maintenance().await;
                if self.snapshot.phase != Phase::Ready {
                    let _ = reply.send(Err(Failure::ChildExited));
                    return;
                }
                let child = self.child.as_ref().expect("Ready owns a child");
                let body = match request.body(&child.endpoint.model, format) {
                    Ok(body) => body,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };
                let endpoint = child.endpoint.clone();
                let kind = child.plan.summary().manifest.execution_kind;
                let deadline = Instant::now() + child.plan.summary().config.budgets.generation;
                let id = Uuid::new_v4();
                let events = self.events.clone();
                let task = tokio::spawn(async move {
                    let result = transport::generate(endpoint, body, target, id, kind)
                        .await
                        .map(JobResult::Generated);
                    let _ = events.send(Event { id, result });
                });
                let (result_tx, result_rx) = oneshot::channel();
                self.job = Some(Job {
                    id,
                    task,
                    cancel: cancel.clone(),
                    deadline,
                    reply: JobReply::Generate(result_tx),
                });
                self.snapshot.active_leases = 1;
                self.publish();
                let generation = Generation {
                    target,
                    run_id: id,
                    cancel,
                    result: result_rx,
                    finished: false,
                };
                let _ = reply.send(Ok(generation)); // send failure drops lease -> cancel
            }
            Request::Unload {
                target,
                model_id,
                idle,
                reply,
            } => {
                let rejected = if !self.snapshot.accepting {
                    Some(Failure::ShuttingDown)
                } else if let Err(e) = self.target(target) {
                    Some(e)
                } else if self.job.is_some() {
                    Some(Failure::Busy)
                } else if self.snapshot.model_id.as_deref() != Some(&model_id) {
                    Some(Failure::WrongModel)
                } else if idle.is_some_and(|threshold| self.last_used.elapsed() < threshold) {
                    Some(Failure::Busy)
                } else {
                    None
                };
                if let Some(e) = rejected {
                    let _ = reply.send(Err(e));
                    return;
                }
                let result = self
                    .cleanup(Phase::Stopped, None)
                    .await
                    .map(|_| self.snapshot.clone());
                let _ = reply.send(result);
            }
            Request::Status(_) => unreachable!(),
        }
    }
    async fn event(&mut self, event: Event) {
        let Some(job) = &self.job else {
            return;
        };
        if job.id != event.id {
            return;
        }
        if job.cancel.is_cancelled() {
            self.fail_job(Failure::Cancelled).await;
            return;
        }
        if Instant::now() >= job.deadline {
            let reason = match job.reply {
                JobReply::Load(_) => Failure::StartupTimeout,
                JobReply::Generate(_) => Failure::GenerationTimeout,
            };
            self.fail_job(reason).await;
            return;
        }
        if self
            .child
            .as_ref()
            .is_some_and(|c| transport::exited_without_reaping(c.endpoint.pid))
        {
            self.fail_job(Failure::ChildExited).await;
            return;
        }
        if let Err(error) = event.result {
            self.fail_job(error).await;
            return;
        }
        let job = self.job.take().expect("checked job");
        let _ = job.task.await;
        self.last_used = Instant::now();
        match (job.reply, event.result.expect("checked result")) {
            (JobReply::Load(reply), JobResult::Loaded) => {
                self.child.as_ref().expect("loaded child").startup.freeze();
                self.snapshot.phase = Phase::Ready;
                self.snapshot.text_chat = if self
                    .child
                    .as_ref()
                    .is_some_and(|c| c.plan.summary().manifest.declares_text_chat)
                {
                    Evidence::DeclaredByManifest
                } else {
                    Evidence::Unsupported
                };
                self.publish();
                let _ = reply.send(Ok(self.snapshot.clone()));
            }
            (JobReply::Generate(reply), JobResult::Generated(completion)) => {
                self.snapshot.active_leases = 0;
                self.snapshot.text_chat = Evidence::ProtocolObserved;
                self.publish();
                let _ = reply.send(Ok(completion));
            }
            _ => {
                let _ = self.cleanup(Phase::Failed, Some(Failure::Protocol)).await;
            }
        }
    }
}
