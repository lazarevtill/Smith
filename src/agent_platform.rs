//! Explicit selected-target platform session. No constructor captures the OS.
use super::*;
use smith_platform::{
    Action as PlatformAction, PlatformSession, PlatformToolAdapter, TargetPermissions,
    TargetRequest,
};
use std::time::{Duration, Instant};

pub(super) struct Platform {
    session: Arc<Mutex<PlatformSession>>,
    pub(super) adapter: Arc<PlatformToolAdapter>,
}
impl Platform {
    fn native(owner: &str) -> smith_agent_core::Result<Self> {
        let session = PlatformSession::new(
            owner,
            Box::new(smith_platform::native::NativeBackend::default()),
        )
        .map_err(platform_error)?;
        Ok(Self::from_session(session))
    }
    pub(super) fn from_session(session: PlatformSession) -> Self {
        let session = Arc::new(Mutex::new(session));
        let adapter = Arc::new(PlatformToolAdapter::new(session.clone()));
        Self { session, adapter }
    }
    pub(super) fn shutdown(&self) {
        let _ = self.adapter.revoke_approval();
        if let Ok(mut s) = self.session.lock() {
            s.shutdown();
        }
    }
    fn view(&self) -> smith_agent_core::Result<Value> {
        let s = self.session.lock().map_err(|_| storage())?;
        Ok(
            json!({"binding_id":s.binding(),"resource":{"kind":"effect_binding","id":s.binding()},"target":s.selected_target(),"capabilities":s.capabilities(),"pending_plan":s.pending_plan()}),
        )
    }
}
fn storage() -> Error {
    Error::Persistence("Platform session unavailable".into())
}
fn platform_error(e: smith_platform::Error) -> Error {
    use smith_platform::Error as E;
    match e {
        E::InvalidInput(m) => Error::InvalidInput(m),
        E::Unsupported(m) => Error::Unsupported(m),
        E::UncertainSideEffect(m) => Error::UncertainSideEffect(m),
        other => Error::Denied(format!("Platform request rejected: {other}")),
    }
}
fn session(app: &Arc<App>) -> smith_agent_core::Result<Arc<Platform>> {
    app.agent
        .platform
        .lock()
        .map_err(|_| storage())?
        .as_ref()
        .cloned()
        .ok_or_else(|| Error::Denied("Create an explicit platform session first".into()))
}
fn delay(
    app: &App,
    store: Option<(&AgentStore, &str, u64)>,
    ms: u64,
) -> smith_agent_core::Result<()> {
    if ms > 5000 {
        return Err(Error::InvalidInput(
            "Focus-return delay must be 0–5000 ms".into(),
        ));
    }
    let until = Instant::now() + Duration::from_millis(ms);
    loop {
        ensure_open(app)?;
        if let Some((store, id, revision)) = store {
            if store.load(id)?.revision != revision {
                return Err(Error::Conflict);
            }
        }
        if Instant::now() >= until {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
pub(super) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/agent/platform/session", post(create).get(status))
        .route("/api/agent/platform/targets", get(targets))
        .route("/api/agent/platform/select", post(select))
        .route("/api/agent/platform/snapshot", post(snapshot))
        .route("/api/agent/platform/prepare", post(prepare))
        .route("/api/agent/platform/approve", post(approve))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    confirmed: bool,
}
async fn create(State(app): State<Arc<App>>, input: Body<Confirm>) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(Error::Denied("Confirm creation of a platform session".into()).into());
    }
    work(app, None, move |app| {
        ensure_open(&app)?;
        let store = AgentStore::new(&app)?;
        let mut slot = app.agent.platform.lock().map_err(|_| storage())?;
        if slot.is_none() {
            *slot = Some(Arc::new(Platform::native(store.owner().id())?));
        }
        Ok(Json(slot.as_ref().unwrap().view()?))
    })
    .await
}
async fn status(State(app): State<Arc<App>>) -> AResult<Json<Value>> {
    Ok(Json(session(&app)?.view()?))
}
async fn targets(State(app): State<Arc<App>>) -> AResult<Json<Value>> {
    work(app,None,move|app|{ensure_open(&app)?;Ok(Json(json!({"targets":smith_platform::native::selectable_targets().map_err(platform_error)?})))}).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Select {
    pid: i32,
    process_identity: String,
    read_elements: bool,
    interact: bool,
    confirmed: bool,
    focus_return_delay_ms: u64,
}
async fn select(State(app): State<Arc<App>>, input: Body<Select>) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(
            Error::Denied("Confirm this target and its platform permissions".into()).into(),
        );
    }
    work(app, None, move |app| {
        let platform = session(&app)?;
        delay(&app, None, input.focus_return_delay_ms)?;
        platform.adapter.revoke_approval().map_err(platform_error)?;
        platform
            .session
            .lock()
            .map_err(|_| storage())?
            .select_target(
                TargetRequest::MacProcessIdentity {
                    pid: input.pid,
                    process_identity: input.process_identity,
                },
                TargetPermissions {
                    read_elements: input.read_elements,
                    interact: input.interact,
                },
            )
            .map_err(platform_error)?;
        Ok(Json(platform.view()?))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRequest {
    focus_return_delay_ms: u64,
    confirmed: bool,
}
async fn snapshot(
    State(app): State<Arc<App>>,
    input: Body<SnapshotRequest>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(Error::Denied("Confirm reading the selected target".into()).into());
    }
    work(app, None, move |app| {
        let platform = session(&app)?;
        delay(&app, None, input.focus_return_delay_ms)?;
        let snapshot = platform
            .session
            .lock()
            .map_err(|_| storage())?
            .screen_elements()
            .map_err(platform_error)?;
        Ok(Json(json!({"snapshot":snapshot})))
    })
    .await
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RequestedAction {
    Click { x: i32, y: i32 },
    TypeText { text: String },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    snapshot_id: String,
    action: RequestedAction,
}
async fn prepare(State(app): State<Arc<App>>, input: Body<Prepare>) -> AResult<Json<Value>> {
    let input = body(input)?;
    work(app, None, move |app| {
        ensure_open(&app)?;
        let platform = session(&app)?;
        let action = match input.action {
            RequestedAction::Click { x, y } => PlatformAction::Click { x, y },
            RequestedAction::TypeText { text } => PlatformAction::TypeText { text },
        };
        let plan = platform
            .session
            .lock()
            .map_err(|_| storage())?
            .prepare(&input.snapshot_id, action)
            .map_err(platform_error)?;
        let (name, args) = match &plan.action {
            PlatformAction::Click { x, y } => (
                "screen_click",
                json!({"x":x,"y":y,"plan_id":plan.id,"review":plan}),
            ),
            PlatformAction::TypeText { text } => (
                "screen_type",
                json!({"text":text,"plan_id":plan.id,"review":plan}),
            ),
        };
        Ok(Json(
            json!({"plan":plan,"call":{"name":name,"arguments":args}}),
        ))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approve {
    task_id: String,
    expected_revision: u64,
    call_id: String,
    plan_id: String,
    focus_return_delay_ms: u64,
    confirmed: bool,
}
async fn approve(State(app): State<Arc<App>>, input: Body<Approve>) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(Error::Denied("Confirm this exact platform action".into()).into());
    }
    work(app, Some(input.task_id.clone()), move |app| {
        let store = AgentStore::new(&app)?;
        let grant = store.grant_for_task(&input.task_id)?;
        let engine = TaskEngine::new(store.clone(), store.registry()?);
        let task = store.load(&input.task_id)?;
        let TaskState::NeedsApproval { call } = task.state else {
            return Err(Error::Conflict.into());
        };
        if task.revision != input.expected_revision
            || call.id != input.call_id
            || call.arguments["plan_id"] != input.plan_id
        {
            return Err(Error::Conflict.into());
        }
        let platform = session(&app)?;
        platform.adapter.validate(&grant, &call)?;
        delay(
            &app,
            Some((&store, &input.task_id, input.expected_revision)),
            input.focus_return_delay_ms,
        )?;
        platform
            .adapter
            .approve_for_call(&input.plan_id, &input.task_id, &call)
            .map_err(platform_error)?;
        let result = engine.decide_approval(
            &input.task_id,
            input.expected_revision,
            &grant,
            ApprovalDecision::Approve,
        );
        let _ = platform.adapter.revoke_approval();
        task_response(&store, &input.task_id, Some(result?))
    })
    .await
}
