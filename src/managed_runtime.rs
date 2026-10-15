//! R0 fixture-only runtime supervisor. Compiled only by the parent #[cfg(test)]
//! registration; no app routes, production runtime, inference or downloads.
//!
//! The actor owns its Child throughout its lifetime. Readiness uses private
//! pipes, and control inputs are closed enums rather than commands from users.

use serde::Deserialize;
use serde_json::json;
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{self, Instant},
};

const FRAME_LIMIT: usize = 16 * 1024;

#[derive(Clone)]
struct Cancellation(watch::Sender<bool>);
impl Cancellation {
    fn new() -> Self {
        Self(watch::channel(false).0)
    }
    fn cancel(&self) {
        self.0.send_replace(true);
    }
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Stopped,
    Starting,
    Loading,
    Ready,
    Stopping,
    Failed,
    CleanupFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    InvalidModel,
    Busy,
    StaleGeneration,
    Cancelled,
    ShuttingDown,
    Spawn,
    Protocol,
    StartupDeadline,
    ChildExited,
    Io,
    Cleanup,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    owner: String,
    generation: u64,
    phase: Phase,
    model: Option<String>,
    pid: Option<u32>,
    active_leases: usize,
    execution_kind: &'static str,
    failure: Option<Failure>,
    spawn_count: usize,
    reaped_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Availability {
    Available,
    Unsupported,
    Unknown,
}
#[derive(Debug, PartialEq, Eq)]
struct Capabilities {
    generation: u64,
    synthetic_text: Availability,
    native_text: Availability,
    hardware: Availability,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Startup {
    Normal,
    Delayed,
    StallReady,
    ExitEarly,
    ExitAfterReady,
    Malformed,
    Oversized,
    WrongNonce,
    WrongModel,
    WrongVersion,
    IgnoreShutdown,
    FloodStderr,
}
impl Startup {
    fn wire(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Delayed => "delayed",
            Self::StallReady => "stall_ready",
            Self::ExitEarly => "exit_early",
            Self::ExitAfterReady => "exit_after_ready",
            Self::Malformed => "malformed",
            Self::Oversized => "oversized",
            Self::WrongNonce => "wrong_nonce",
            Self::WrongModel => "wrong_model",
            Self::WrongVersion => "wrong_version",
            Self::IgnoreShutdown => "ignore_shutdown",
            Self::FloodStderr => "flood_stderr",
        }
    }
}

#[derive(Clone, Copy)]
enum RunMode {
    Finite,
    Stall,
    IgnoreCancel,
    Flood,
}
impl RunMode {
    fn wire(self) -> &'static str {
        match self {
            Self::Finite => "finite",
            Self::Stall => "stall",
            Self::IgnoreCancel => "ignore_cancel",
            Self::Flood => "flood",
        }
    }
}

#[derive(Clone, Copy)]
struct Budgets {
    startup: Duration,
    grace: Duration,
    reap: Duration,
}
impl Default for Budgets {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(3),
            grace: Duration::from_millis(250),
            reap: Duration::from_secs(2),
        }
    }
}

struct FixtureSpec {
    python: PathBuf,
    script: PathBuf,
    directory: tempfile::TempDir,
    budgets: Budgets,
}
impl FixtureSpec {
    fn local() -> Result<Self, Failure> {
        let python = [
            "/opt/homebrew/bin/python3",
            "/usr/local/bin/python3",
            "/usr/bin/python3",
        ]
        .iter()
        .find_map(|path| std::fs::canonicalize(path).ok())
        .ok_or(Failure::Spawn)?;
        let script = std::fs::canonicalize(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/managed_child.py"),
        )
        .map_err(|_| Failure::Spawn)?;
        Ok(Self {
            python,
            script,
            directory: tempfile::tempdir().map_err(|_| Failure::Io)?,
            budgets: Budgets::default(),
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum ChildEvent {
    Loading {
        nonce: String,
        generation: u64,
        model: String,
        protocol: u8,
    },
    Ready {
        nonce: String,
        generation: u64,
        model: String,
        protocol: u8,
    },
    Token {
        run_id: String,
        text: String,
    },
    Done {
        run_id: String,
    },
    Cancelled {
        run_id: String,
    },
}

struct OwnedChild {
    child: Child,
    stdin: ChildStdin,
    events: mpsc::Receiver<Result<ChildEvent, Failure>>,
    readers: Vec<JoinHandle<()>>,
}
impl OwnedChild {
    async fn write(&mut self, value: serde_json::Value, budget: Duration) -> Result<(), Failure> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| Failure::Protocol)?;
        bytes.push(b'\n');
        time::timeout(budget, self.stdin.write_all(&bytes))
            .await
            .map_err(|_| Failure::Io)?
            .map_err(|_| Failure::Io)
    }

    async fn stop(&mut self, budgets: Budgets) -> Result<(), Failure> {
        // No OS-wide process search: the retained handle is the only authority.
        let deadline = Instant::now() + budgets.reap;
        let _ = self
            .write(
                json!({"op":"shutdown"}),
                budgets
                    .grace
                    .min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        let reaped = matches!(
            time::timeout(
                budgets
                    .grace
                    .min(deadline.saturating_duration_since(Instant::now())),
                self.child.wait()
            )
            .await,
            Ok(Ok(_))
        );
        if !reaped {
            self.child.start_kill().map_err(|_| Failure::Cleanup)?;
            time::timeout_at(deadline, self.child.wait())
                .await
                .map_err(|_| Failure::Cleanup)?
                .map_err(|_| Failure::Cleanup)?;
        }
        for reader in self.readers.drain(..) {
            reader.abort();
            let _ = reader.await;
        }
        Ok(())
    }
}

// Limit memory before decoding, including a malicious line without a newline.
async fn next_frame(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<Option<Vec<u8>>, Failure> {
    let mut output = Vec::new();
    loop {
        let bytes = reader.fill_buf().await.map_err(|_| Failure::Io)?;
        if bytes.is_empty() {
            return if output.is_empty() {
                Ok(None)
            } else {
                Err(Failure::Protocol)
            };
        }
        let newline = bytes.iter().position(|&b| b == b'\n');
        let count = newline.map_or(bytes.len(), |index| index + 1);
        if output.len() + count > FRAME_LIMIT {
            return Err(Failure::Protocol);
        }
        output.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(output));
        }
    }
}

fn spawn_fixture(spec: &FixtureSpec) -> Result<OwnedChild, Failure> {
    let mut child = Command::new(&spec.python)
        .args(["-I", "-u"])
        .arg(&spec.script)
        .current_dir(spec.directory.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", spec.directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Failure::Spawn)?;
    let stdin = child.stdin.take().ok_or(Failure::Spawn)?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or(Failure::Spawn)?);
    let mut stderr = child.stderr.take().ok_or(Failure::Spawn)?;
    let (sender, events) = mpsc::channel(8);
    let output_reader = tokio::spawn(async move {
        loop {
            let item = match next_frame(&mut stdout).await {
                Ok(Some(bytes)) => serde_json::from_slice(&bytes).map_err(|_| Failure::Protocol),
                Ok(None) => Err(Failure::ChildExited),
                Err(error) => Err(error),
            };
            let terminal = item.is_err();
            if sender.send(item).await.is_err() || terminal {
                return;
            }
        }
    });
    let error_reader = tokio::spawn(async move {
        // Drain bounded chunks, retain no child text. Secrets cannot enter diagnostics.
        let mut buffer = [0_u8; 4096];
        while let Ok(count) = stderr.read(&mut buffer).await {
            if count == 0 {
                break;
            }
        }
    });
    Ok(OwnedChild {
        child,
        stdin,
        events,
        readers: vec![output_reader, error_reader],
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Completed,
    Cancelled,
}

struct RunHandle {
    generation: u64,
    cancel: Cancellation,
    tokens: mpsc::Receiver<String>,
    completion: oneshot::Receiver<Result<Outcome, Failure>>,
}
impl RunHandle {
    fn cancel(&self) {
        self.cancel.cancel();
    }
    async fn collect(&mut self) -> Result<(Outcome, Vec<String>), Failure> {
        let mut tokens = Vec::new();
        while let Some(token) = self.tokens.recv().await {
            tokens.push(token);
        }
        let outcome = (&mut self.completion).await.map_err(|_| Failure::Io)??;
        Ok((outcome, tokens))
    }
}
impl Drop for RunHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

enum Request {
    Load {
        model: String,
        startup: Startup,
        cancel: Cancellation,
        reply: oneshot::Sender<Result<Snapshot, Failure>>,
    },
    Start {
        generation: u64,
        mode: RunMode,
        reply: oneshot::Sender<Result<RunHandle, Failure>>,
    },
    Unload {
        generation: u64,
        model: String,
        idle: Option<Duration>,
        reply: oneshot::Sender<Result<bool, Failure>>,
    },
}

struct Loading {
    reply: oneshot::Sender<Result<Snapshot, Failure>>,
    cancel: Cancellation,
    deadline: Instant,
    nonce: String,
}
struct Running {
    id: String,
    cancel: Cancellation,
    tokens: mpsc::Sender<String>,
    completion: oneshot::Sender<Result<Outcome, Failure>>,
    cancel_deadline: Option<Instant>,
}

struct Runtime {
    requests: mpsc::Sender<Request>,
    snapshots: watch::Receiver<Snapshot>,
    stop: Cancellation,
    actor: tokio::sync::Mutex<Option<JoinHandle<Snapshot>>>,
}
impl Runtime {
    fn new(spec: FixtureSpec) -> Self {
        let snapshot = Snapshot {
            owner: uuid::Uuid::new_v4().to_string(),
            generation: 0,
            phase: Phase::Stopped,
            model: None,
            pid: None,
            active_leases: 0,
            execution_kind: "fixture",
            failure: None,
            spawn_count: 0,
            reaped_count: 0,
        };
        let (updates, snapshots) = watch::channel(snapshot.clone());
        let (requests, receive) = mpsc::channel(8);
        let stop = Cancellation::new();
        let actor = tokio::spawn(
            Actor {
                spec,
                snapshot,
                updates,
                requests: receive,
                stop: stop.clone(),
                child: None,
                loading: None,
                running: None,
                last_used: Instant::now(),
                startup: Startup::Normal,
            }
            .drive(),
        );
        Self {
            requests,
            snapshots,
            stop,
            actor: tokio::sync::Mutex::new(Some(actor)),
        }
    }
    fn status(&self) -> Snapshot {
        self.snapshots.borrow().clone()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            generation: self.status().generation,
            synthetic_text: Availability::Available,
            native_text: Availability::Unsupported,
            hardware: Availability::Unknown,
        }
    }
    fn require_capability(&self, name: &str) -> Result<(), Failure> {
        if name == "synthetic_text" {
            Ok(())
        } else {
            Err(Failure::Unsupported)
        }
    }
    async fn load(
        &self,
        model: &str,
        startup: Startup,
        cancel: Cancellation,
    ) -> Result<Snapshot, Failure> {
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if self.stop.is_cancelled() {
            return Err(Failure::ShuttingDown);
        }
        let (reply, received) = oneshot::channel();
        let request = Request::Load {
            model: model.into(),
            startup,
            cancel: cancel.clone(),
            reply,
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Failure::Cancelled),
            _ = self.stop.cancelled() => return Err(Failure::ShuttingDown),
            sent = self.requests.send(request) => sent.map_err(|_| Failure::ShuttingDown)?,
        }
        // Once admitted, the owner returns only after cancellation cleanup. A
        // dropped caller closes the reply; the actor detects it and cleans up.
        received.await.map_err(|_| Failure::ShuttingDown)?
    }
    async fn start(&self, generation: u64, mode: RunMode) -> Result<RunHandle, Failure> {
        let (reply, received) = oneshot::channel();
        self.requests
            .send(Request::Start {
                generation,
                mode,
                reply,
            })
            .await
            .map_err(|_| Failure::ShuttingDown)?;
        received.await.map_err(|_| Failure::ShuttingDown)?
    }
    async fn unload(
        &self,
        generation: u64,
        model: &str,
        idle: Option<Duration>,
    ) -> Result<bool, Failure> {
        let (reply, received) = oneshot::channel();
        self.requests
            .send(Request::Unload {
                generation,
                model: model.into(),
                idle,
                reply,
            })
            .await
            .map_err(|_| Failure::ShuttingDown)?;
        received.await.map_err(|_| Failure::ShuttingDown)?
    }
    async fn shutdown(&self) -> Result<Snapshot, Failure> {
        self.stop.cancel();
        let mut actor = self.actor.lock().await;
        if let Some(task) = actor.take() {
            task.await.map_err(|_| Failure::Cleanup)?;
        }
        let snapshot = self.status();
        if snapshot.phase == Phase::CleanupFailed {
            Err(Failure::Cleanup)
        } else {
            Ok(snapshot)
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Actor {
    spec: FixtureSpec,
    snapshot: Snapshot,
    updates: watch::Sender<Snapshot>,
    requests: mpsc::Receiver<Request>,
    stop: Cancellation,
    child: Option<OwnedChild>,
    loading: Option<Loading>,
    running: Option<Running>,
    last_used: Instant,
    startup: Startup,
}
fn canonical(model: &str) -> Result<&'static str, Failure> {
    match model {
        "fixture-a" | "fixture/repo/fixture-a.gguf" => Ok("fixture-a"),
        "fixture-b" => Ok("fixture-b"),
        _ => Err(Failure::InvalidModel),
    }
}
impl Actor {
    fn publish(&self) {
        self.updates.send_replace(self.snapshot.clone());
    }
    fn finish_run(&mut self, result: Result<Outcome, Failure>) {
        if let Some(run) = self.running.take() {
            let _ = run.completion.send(result);
        }
        self.snapshot.active_leases = 0;
        self.last_used = Instant::now();
        self.publish();
    }
    async fn cleanup(&mut self, terminal: Phase, failure: Option<Failure>) -> Result<(), Failure> {
        self.snapshot.phase = Phase::Stopping;
        self.publish();
        if let Some(child) = &mut self.child {
            if child.stop(self.spec.budgets).await.is_err() {
                self.snapshot.phase = Phase::CleanupFailed;
                self.snapshot.failure = Some(Failure::Cleanup);
                self.publish();
                return Err(Failure::Cleanup);
            }
            self.snapshot.reaped_count += 1;
        }
        self.child = None;
        self.snapshot.phase = terminal;
        self.snapshot.pid = None;
        self.snapshot.model = None;
        self.snapshot.failure = failure;
        self.publish();
        Ok(())
    }
    async fn fail(&mut self, failure: Failure) {
        let final_failure = self
            .cleanup(Phase::Failed, Some(failure))
            .await
            .err()
            .unwrap_or(failure);
        if let Some(load) = self.loading.take() {
            let _ = load.reply.send(Err(final_failure));
        }
        self.finish_run(Err(final_failure));
    }
    async fn drive(mut self) -> Snapshot {
        let mut tick = time::interval(Duration::from_millis(10));
        loop {
            tokio::select! {
                biased;
                _ = self.stop.cancelled() => break,
                _ = tick.tick() => self.maintenance().await,
                command = self.requests.recv() => match command { Some(command) => self.command(command).await, None => break },
                event = async { match self.child.as_mut() { Some(child) => child.events.recv().await, None => std::future::pending().await } } => {
                    match event { Some(Ok(event)) => self.event(event).await, Some(Err(failure)) => self.fail(failure).await, None => self.fail(Failure::ChildExited).await }
                }
            }
        }
        let cleanup = self.cleanup(Phase::Stopped, None).await;
        if let Some(load) = self.loading.take() {
            let _ = load
                .reply
                .send(Err(cleanup.err().unwrap_or(Failure::ShuttingDown)));
        }
        self.finish_run(if cleanup.is_ok() {
            Ok(Outcome::Cancelled)
        } else {
            Err(Failure::Cleanup)
        });
        self.snapshot
    }
    async fn maintenance(&mut self) {
        if let Some(load) = &self.loading {
            if load.cancel.is_cancelled() || load.reply.is_closed() {
                let outcome = self
                    .cleanup(Phase::Stopped, None)
                    .await
                    .err()
                    .unwrap_or(Failure::Cancelled);
                if let Some(load) = self.loading.take() {
                    let _ = load.reply.send(Err(outcome));
                }
                return;
            }
            if Instant::now() >= load.deadline {
                self.fail(Failure::StartupDeadline).await;
                return;
            }
        }
        if let Some(run) = &self.running {
            if run.cancel.is_cancelled() || run.completion.is_closed() {
                if let Some(deadline) = run.cancel_deadline {
                    if Instant::now() >= deadline {
                        let cleanup = self.cleanup(Phase::Stopped, None).await;
                        self.finish_run(cleanup.map(|_| Outcome::Cancelled));
                        return;
                    }
                } else {
                    let id = run.id.clone();
                    if let Some(run) = &mut self.running {
                        run.cancel_deadline = Some(Instant::now() + self.spec.budgets.grace);
                    }
                    if let Some(child) = &mut self.child {
                        if child
                            .write(json!({"op":"cancel","run_id":id}), self.spec.budgets.grace)
                            .await
                            .is_err()
                        {
                            let cleanup = self.cleanup(Phase::Stopped, None).await;
                            self.finish_run(cleanup.map(|_| Outcome::Cancelled));
                            return;
                        }
                    }
                }
            }
        }
        if let Some(child) = &mut self.child {
            if !matches!(child.child.try_wait(), Ok(None)) {
                self.fail(Failure::ChildExited).await;
            }
        }
    }
    async fn command(&mut self, request: Request) {
        match request {
            Request::Load {
                model,
                startup,
                cancel,
                reply,
            } => {
                if cancel.is_cancelled() || reply.is_closed() {
                    let _ = reply.send(Err(Failure::Cancelled));
                    return;
                }
                let model = match canonical(&model) {
                    Ok(model) => model.to_string(),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                if self.loading.is_some() || self.running.is_some() {
                    let _ = reply.send(Err(Failure::Busy));
                    return;
                }
                if self.snapshot.phase == Phase::Ready
                    && self.snapshot.model.as_ref() == Some(&model)
                    && self.startup == startup
                    && self
                        .child
                        .as_mut()
                        .is_some_and(|child| matches!(child.child.try_wait(), Ok(None)))
                {
                    let _ = reply.send(Ok(self.snapshot.clone()));
                    return;
                }
                if self.cleanup(Phase::Stopped, None).await.is_err() {
                    let _ = reply.send(Err(Failure::Cleanup));
                    return;
                }
                if cancel.is_cancelled() || self.stop.is_cancelled() || reply.is_closed() {
                    let _ = reply.send(Err(Failure::Cancelled));
                    return;
                }
                self.snapshot.generation += 1;
                self.snapshot.phase = Phase::Starting;
                self.snapshot.model = Some(model.clone());
                self.snapshot.failure = None;
                self.startup = startup;
                self.publish();
                let nonce = uuid::Uuid::new_v4().to_string();
                self.loading = Some(Loading {
                    reply,
                    cancel,
                    deadline: Instant::now() + self.spec.budgets.startup,
                    nonce: nonce.clone(),
                });
                match spawn_fixture(&self.spec) {
                    Ok(mut child) => {
                        self.snapshot.pid = child.child.id();
                        self.snapshot.spawn_count += 1;
                        let wrote = child.write(json!({"nonce":nonce,"generation":self.snapshot.generation,"model":model,"startup":startup.wire()}), self.spec.budgets.grace).await;
                        self.child = Some(child);
                        self.publish();
                        if wrote.is_err() {
                            self.fail(Failure::Io).await;
                        }
                    }
                    Err(error) => self.fail(error).await,
                }
            }
            Request::Start {
                generation,
                mode,
                reply,
            } => {
                if generation != self.snapshot.generation {
                    let _ = reply.send(Err(Failure::StaleGeneration));
                    return;
                }
                if self.running.is_some() || self.loading.is_some() {
                    let _ = reply.send(Err(Failure::Busy));
                    return;
                }
                if self.snapshot.phase != Phase::Ready {
                    let _ = reply.send(Err(Failure::ChildExited));
                    return;
                }
                if !self
                    .child
                    .as_mut()
                    .is_some_and(|child| matches!(child.child.try_wait(), Ok(None)))
                {
                    self.fail(Failure::ChildExited).await;
                    let _ = reply.send(Err(Failure::ChildExited));
                    return;
                }
                let id = uuid::Uuid::new_v4().to_string();
                let cancel = Cancellation::new();
                let (tokens, token_receiver) = mpsc::channel(4);
                let (completion, completed) = oneshot::channel();
                self.running = Some(Running {
                    id: id.clone(),
                    cancel: cancel.clone(),
                    tokens,
                    completion,
                    cancel_deadline: None,
                });
                self.snapshot.active_leases = 1;
                self.publish();
                let handle = RunHandle {
                    generation,
                    cancel,
                    tokens: token_receiver,
                    completion: completed,
                };
                if reply.send(Ok(handle)).is_err() {
                    self.maintenance().await;
                    return;
                }
                if let Some(child) = &mut self.child {
                    if child
                        .write(
                            json!({"op":"run","run_id":id,"mode":mode.wire()}),
                            self.spec.budgets.grace,
                        )
                        .await
                        .is_err()
                    {
                        self.fail(Failure::Io).await;
                    }
                }
            }
            Request::Unload {
                generation,
                model,
                idle,
                reply,
            } => {
                if generation != self.snapshot.generation {
                    let _ = reply.send(Err(Failure::StaleGeneration));
                    return;
                }
                if self.snapshot.phase == Phase::Stopped {
                    let _ = reply.send(Ok(false));
                    return;
                }
                if canonical(&model).ok() != self.snapshot.model.as_deref() {
                    let _ = reply.send(Err(Failure::InvalidModel));
                    return;
                }
                if self.running.is_some() || self.loading.is_some() {
                    let _ = reply.send(if idle.is_some() {
                        Ok(false)
                    } else {
                        Err(Failure::Busy)
                    });
                    return;
                }
                if idle.is_some_and(|threshold| self.last_used.elapsed() < threshold) {
                    let _ = reply.send(Ok(false));
                    return;
                }
                let outcome = self.cleanup(Phase::Stopped, None).await.map(|_| true);
                let _ = reply.send(outcome);
            }
        }
    }
    async fn event(&mut self, event: ChildEvent) {
        let is_ready = matches!(&event, ChildEvent::Ready { .. });
        let is_cancelled = matches!(&event, ChildEvent::Cancelled { .. });
        match event {
            ChildEvent::Loading {
                nonce,
                generation,
                model,
                protocol,
            }
            | ChildEvent::Ready {
                nonce,
                generation,
                model,
                protocol,
            } => {
                let valid = self
                    .loading
                    .as_ref()
                    .is_some_and(|load| load.nonce == nonce)
                    && generation == self.snapshot.generation
                    && self.snapshot.model.as_ref() == Some(&model)
                    && protocol == 1;
                if !valid {
                    self.fail(Failure::Protocol).await;
                    return;
                }
                if self
                    .loading
                    .as_ref()
                    .is_some_and(|load| load.cancel.is_cancelled() || load.reply.is_closed())
                {
                    self.maintenance().await;
                    return;
                }
                if !is_ready && self.snapshot.phase == Phase::Starting {
                    self.snapshot.phase = Phase::Loading;
                    self.publish();
                } else if is_ready && self.snapshot.phase == Phase::Loading {
                    if !self
                        .child
                        .as_mut()
                        .is_some_and(|child| matches!(child.child.try_wait(), Ok(None)))
                    {
                        self.fail(Failure::ChildExited).await;
                        return;
                    }
                    self.snapshot.phase = Phase::Ready;
                    self.last_used = Instant::now();
                    self.publish();
                    if let Some(load) = self.loading.take() {
                        let _ = load.reply.send(Ok(self.snapshot.clone()));
                    }
                } else {
                    self.fail(Failure::Protocol).await;
                }
            }
            ChildEvent::Token { run_id, text } => {
                let Some(run) = &self.running else {
                    self.fail(Failure::Protocol).await;
                    return;
                };
                if run.id != run_id {
                    self.fail(Failure::Protocol).await;
                    return;
                }
                if run.cancel.is_cancelled() || run.cancel_deadline.is_some() {
                    return;
                }
                tokio::select! {
                    biased;
                    _ = self.stop.cancelled() => {},
                    _ = run.cancel.cancelled() => {},
                    result = run.tokens.send(text) => { if result.is_err() { run.cancel.cancel(); } },
                }
            }
            ChildEvent::Done { run_id } | ChildEvent::Cancelled { run_id } => {
                let Some(run) = &self.running else {
                    self.fail(Failure::Protocol).await;
                    return;
                };
                if run.id != run_id {
                    self.fail(Failure::Protocol).await;
                    return;
                }
                let cancelled = run.cancel.is_cancelled() || run.cancel_deadline.is_some();
                if is_cancelled && !cancelled {
                    self.fail(Failure::Protocol).await;
                    return;
                }
                self.finish_run(Ok(if cancelled {
                    Outcome::Cancelled
                } else {
                    Outcome::Completed
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> Runtime {
        Runtime::new(FixtureSpec::local().expect("local Python fixture prerequisites"))
    }
    async fn ready(runtime: &Runtime) -> Snapshot {
        runtime
            .load("fixture-a", Startup::Normal, Cancellation::new())
            .await
            .unwrap()
    }
    async fn settled(runtime: &Runtime, phase: Phase) -> Snapshot {
        time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = runtime.status();
                if snapshot.phase == phase {
                    return snapshot;
                }
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("state deadline")
    }
    async fn all(run: &mut RunHandle) -> Result<(Outcome, Vec<String>), Failure> {
        time::timeout(Duration::from_secs(3), run.collect())
            .await
            .expect("run deadline")
    }

    #[tokio::test]
    async fn stopped_alias_load_and_completed_fixture_run() {
        let runtime = runtime();
        assert_eq!(runtime.status().phase, Phase::Stopped);
        assert_eq!(runtime.status().spawn_count, 0);
        assert!(runtime.status().model.is_none());
        let loaded = ready(&runtime).await;
        assert_eq!(loaded.execution_kind, "fixture");
        assert_eq!(
            runtime.capabilities(),
            Capabilities {
                generation: loaded.generation,
                synthetic_text: Availability::Available,
                native_text: Availability::Unsupported,
                hardware: Availability::Unknown
            }
        );
        assert!(loaded.pid.is_some());
        let alias = runtime
            .load(
                "fixture/repo/fixture-a.gguf",
                Startup::Normal,
                Cancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(alias, loaded);
        assert_eq!(
            runtime.require_capability("native_text"),
            Err(Failure::Unsupported)
        );
        assert_eq!(runtime.require_capability("synthetic_text"), Ok(()));
        let mut run = runtime
            .start(loaded.generation, RunMode::Finite)
            .await
            .unwrap();
        assert_eq!(run.generation, loaded.generation);
        let (outcome, tokens) = all(&mut run).await.unwrap();
        assert_eq!(outcome, Outcome::Completed);
        assert_eq!(tokens.concat(), "fixture-0 fixture-1 fixture-2 ");
        run.cancel();
        assert_eq!(runtime.status().active_leases, 0);
        assert!(runtime
            .unload(loaded.generation, "fixture-a", None)
            .await
            .unwrap());
        assert!(!runtime
            .unload(loaded.generation, "fixture-a", None)
            .await
            .unwrap());
        assert_eq!(runtime.shutdown().await.unwrap().phase, Phase::Stopped);
        assert_eq!(runtime.shutdown().await.unwrap().phase, Phase::Stopped);
    }

    #[tokio::test]
    async fn invalid_and_precancelled_load_never_spawn_or_stop_current_model() {
        let runtime = runtime();
        assert_eq!(
            runtime
                .load("../other", Startup::Normal, Cancellation::new())
                .await
                .unwrap_err(),
            Failure::InvalidModel
        );
        let cancel = Cancellation::new();
        cancel.cancel();
        assert_eq!(
            runtime
                .load("fixture-a", Startup::Normal, cancel)
                .await
                .unwrap_err(),
            Failure::Cancelled
        );
        assert_eq!(runtime.status().spawn_count, 0);
        let before = ready(&runtime).await;
        assert_eq!(
            runtime
                .load(
                    "different-repo/fixture-a.gguf",
                    Startup::Normal,
                    Cancellation::new()
                )
                .await
                .unwrap_err(),
            Failure::InvalidModel
        );
        assert_eq!(runtime.status(), before);
        assert_eq!(
            runtime
                .load("fixture-b", Startup::WrongNonce, Cancellation::new())
                .await
                .unwrap_err(),
            Failure::Protocol
        );
        let failed = runtime.status();
        assert_eq!(failed.phase, Phase::Failed);
        assert!(failed.model.is_none());
        assert_eq!(failed.spawn_count, failed.reaped_count);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delayed_readiness_shutdown_and_cancel_before_first_token() {
        let runtime = Arc::new(runtime());
        let task = {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                runtime
                    .load("fixture-a", Startup::Delayed, Cancellation::new())
                    .await
            })
        };
        settled(&runtime, Phase::Loading).await;
        let stopped = runtime.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), Err(Failure::ShuttingDown));
        assert_eq!(stopped.spawn_count, stopped.reaped_count);
        let runtime = super::tests::runtime();
        let loaded = ready(&runtime).await;
        let mut early = runtime
            .start(loaded.generation, RunMode::Stall)
            .await
            .unwrap();
        early.cancel();
        assert_eq!(all(&mut early).await.unwrap().0, Outcome::Cancelled);
        let loaded = ready(&runtime).await;
        let mut later = runtime
            .start(loaded.generation, RunMode::Finite)
            .await
            .unwrap();
        early.cancel(); // An old handle cannot cancel a new admitted run.
        assert_eq!(all(&mut later).await.unwrap().0, Outcome::Completed);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_during_loading_reaps_before_reply_and_blocks_second_load() {
        let runtime = Arc::new(runtime());
        let cancel = Cancellation::new();
        let task = {
            let runtime = runtime.clone();
            let cancel = cancel.clone();
            tokio::spawn(
                async move { runtime.load("fixture-a", Startup::StallReady, cancel).await },
            )
        };
        settled(&runtime, Phase::Loading).await;
        assert_eq!(
            runtime
                .load("fixture-b", Startup::Normal, Cancellation::new())
                .await
                .unwrap_err(),
            Failure::Busy
        );
        let start = Instant::now();
        cancel.cancel();
        assert_eq!(task.await.unwrap().unwrap_err(), Failure::Cancelled);
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(runtime.status().phase, Phase::Stopped);
        assert!(runtime.status().pid.is_none());
        assert_eq!(runtime.status().spawn_count, 1);
        assert_eq!(runtime.status().reaped_count, 1);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_identity_and_early_exit_never_become_ready() {
        for startup in [
            Startup::Malformed,
            Startup::Oversized,
            Startup::WrongNonce,
            Startup::WrongModel,
            Startup::WrongVersion,
            Startup::ExitEarly,
        ] {
            let runtime = runtime();
            let error = runtime
                .load("fixture-a", startup, Cancellation::new())
                .await
                .unwrap_err();
            assert!(matches!(error, Failure::Protocol | Failure::ChildExited));
            assert_eq!(runtime.status().phase, Phase::Failed);
            assert!(runtime.status().model.is_none());
            assert!(runtime.status().pid.is_none());
            runtime.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn startup_timeout_and_missing_executable_cleanup() {
        let mut spec = FixtureSpec::local().unwrap();
        spec.budgets.startup = Duration::from_millis(150);
        let runtime = Runtime::new(spec);
        assert_eq!(
            runtime
                .load("fixture-a", Startup::StallReady, Cancellation::new())
                .await
                .unwrap_err(),
            Failure::StartupDeadline
        );
        assert!(runtime.status().pid.is_none());
        runtime.shutdown().await.unwrap();
        let mut spec = FixtureSpec::local().unwrap();
        spec.python = spec.directory.path().join("missing-fixture-python");
        let runtime = Runtime::new(spec);
        assert_eq!(
            runtime
                .load("fixture-a", Startup::Normal, Cancellation::new())
                .await
                .unwrap_err(),
            Failure::Spawn
        );
        assert_eq!(runtime.status().spawn_count, 0);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn active_lease_blocks_switch_unload_and_idle_eviction() {
        let runtime = runtime();
        let loaded = ready(&runtime).await;
        let mut run = runtime
            .start(loaded.generation, RunMode::Stall)
            .await
            .unwrap();
        assert!(run.tokens.recv().await.is_some());
        assert_eq!(runtime.status().active_leases, 1);
        assert_eq!(
            runtime
                .load("fixture-b", Startup::Normal, Cancellation::new())
                .await
                .unwrap_err(),
            Failure::Busy
        );
        assert_eq!(
            runtime.unload(loaded.generation, "fixture-a", None).await,
            Err(Failure::Busy)
        );
        assert!(!runtime
            .unload(loaded.generation, "fixture-a", Some(Duration::ZERO))
            .await
            .unwrap());
        run.cancel();
        assert_eq!(all(&mut run).await.unwrap().0, Outcome::Cancelled);
        assert_eq!(runtime.status().active_leases, 0);
        assert!(runtime
            .unload(loaded.generation, "fixture-a", Some(Duration::ZERO))
            .await
            .unwrap());
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lease_admission_and_idle_eviction_have_one_winner() {
        let runtime = runtime();
        let loaded = ready(&runtime).await;
        // Both commands are enqueued before either response is observed. Their
        // actor transitions, including lease admission, are indivisible.
        let (started, evicted) = tokio::join!(
            runtime.start(loaded.generation, RunMode::Stall),
            runtime.unload(loaded.generation, "fixture-a", Some(Duration::ZERO)),
        );
        let mut run = started.unwrap();
        assert!(!evicted.unwrap());
        run.cancel();
        assert_eq!(all(&mut run).await.unwrap().0, Outcome::Cancelled);
        let loaded = ready(&runtime).await;
        let (evicted, started) = tokio::join!(
            runtime.unload(loaded.generation, "fixture-a", Some(Duration::ZERO)),
            runtime.start(loaded.generation, RunMode::Finite),
        );
        assert!(evicted.unwrap());
        assert!(matches!(started, Err(Failure::ChildExited)));
        assert_eq!(runtime.status().active_leases, 0);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_stalled_ignored_and_backpressured_runs_finish() {
        for mode in [RunMode::Stall, RunMode::IgnoreCancel, RunMode::Flood] {
            let runtime = runtime();
            let loaded = ready(&runtime).await;
            let mut run = runtime.start(loaded.generation, mode).await.unwrap();
            assert!(run.tokens.recv().await.is_some());
            if matches!(mode, RunMode::Flood) {
                time::sleep(Duration::from_millis(80)).await;
            }
            let start = Instant::now();
            run.cancel();
            assert_eq!(all(&mut run).await.unwrap().0, Outcome::Cancelled);
            assert!(start.elapsed() < Duration::from_secs(2));
            assert_eq!(runtime.status().active_leases, 0);
            runtime.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn stale_generation_and_wrong_model_cannot_unload_new_child() {
        let runtime = runtime();
        let first = ready(&runtime).await;
        let next = runtime
            .load("fixture-b", Startup::Normal, Cancellation::new())
            .await
            .unwrap();
        assert!(next.generation > first.generation);
        assert!(matches!(
            runtime.start(first.generation, RunMode::Finite).await,
            Err(Failure::StaleGeneration)
        ));
        assert_eq!(
            runtime.unload(first.generation, "fixture-a", None).await,
            Err(Failure::StaleGeneration)
        );
        assert_eq!(
            runtime.unload(next.generation, "fixture-a", None).await,
            Err(Failure::InvalidModel)
        );
        assert_eq!(runtime.status(), next);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ready_child_exit_invalidates_model_without_automatic_restart() {
        let runtime = runtime();
        let loaded = runtime
            .load("fixture-a", Startup::ExitAfterReady, Cancellation::new())
            .await
            .unwrap();
        let failed = settled(&runtime, Phase::Failed).await;
        assert_eq!(failed.failure, Some(Failure::ChildExited));
        assert!(failed.model.is_none());
        assert_eq!(failed.spawn_count, 1);
        assert!(matches!(
            runtime.start(loaded.generation, RunMode::Finite).await,
            Err(Failure::ChildExited)
        ));
        let next = ready(&runtime).await;
        assert_eq!(next.spawn_count, 2);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn forced_shutdown_preserves_sibling_and_drains_stderr() {
        let sibling = runtime();
        let sentinel = ready(&sibling).await;
        let runtime = runtime();
        runtime
            .load("fixture-a", Startup::IgnoreShutdown, Cancellation::new())
            .await
            .unwrap();
        let start = Instant::now();
        assert_eq!(runtime.shutdown().await.unwrap().phase, Phase::Stopped);
        assert!(start.elapsed() < Duration::from_secs(2));
        let mut run = sibling
            .start(sentinel.generation, RunMode::Finite)
            .await
            .unwrap();
        assert_eq!(all(&mut run).await.unwrap().0, Outcome::Completed);
        assert_eq!(sibling.status().pid, sentinel.pid);
        sibling.shutdown().await.unwrap();
        let runtime = super::tests::runtime();
        let loaded = runtime
            .load("fixture-a", Startup::FloodStderr, Cancellation::new())
            .await
            .unwrap();
        assert!(!format!("{loaded:?}").contains("synthetic-secret"));
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn caller_drop_and_shutdown_during_load_or_run_leave_no_owned_child() {
        let runtime = Arc::new(runtime());
        let task = {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                runtime
                    .load("fixture-a", Startup::StallReady, Cancellation::new())
                    .await
            })
        };
        settled(&runtime, Phase::Loading).await;
        task.abort();
        let _ = task.await;
        assert!(settled(&runtime, Phase::Stopped).await.pid.is_none());
        let loaded = ready(&runtime).await;
        let run = runtime
            .start(loaded.generation, RunMode::Stall)
            .await
            .unwrap();
        drop(run);
        time::timeout(Duration::from_secs(2), async {
            while runtime.status().active_leases != 0 {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let _run = runtime
            .start(loaded.generation, RunMode::Stall)
            .await
            .unwrap();
        let stopped = runtime.shutdown().await.unwrap();
        assert!(stopped.pid.is_none());
        assert_eq!(stopped.active_leases, 0);
    }

    #[tokio::test]
    async fn twenty_cycles_release_children_and_leases() {
        let runtime = runtime();
        for cycle in 1..=20 {
            let loaded = ready(&runtime).await;
            assert_eq!(loaded.spawn_count, cycle);
            let mut run = runtime
                .start(loaded.generation, RunMode::Finite)
                .await
                .unwrap();
            assert_eq!(all(&mut run).await.unwrap().0, Outcome::Completed);
            runtime
                .unload(loaded.generation, "fixture-a", None)
                .await
                .unwrap();
            let stopped = runtime.status();
            assert!(stopped.pid.is_none());
            assert_eq!(stopped.active_leases, 0);
            assert_eq!(stopped.reaped_count, cycle);
        }
        runtime.shutdown().await.unwrap();
    }
}
