//! Explicitly reviewed effects. No value deserialized from a tool call grants
//! filesystem, executable, or network authority. See docs/TOOL-EFFECTS.md.
mod network;
mod process;
#[cfg(unix)]
mod workspace;
#[cfg(not(unix))]
#[path = "workspace_unsupported.rs"]
mod workspace;

pub use network::DestinationGrant;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt,
    sync::Mutex,
    time::{Duration, Instant},
};
pub use workspace::{ExecutableGrant, WorkspaceGrant};

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    Denied(&'static str),
    Stale,
    Cancelled,
    TimedOut,
    Unsupported(&'static str),
    Limit(&'static str),
    Io,
    Process,
    Cleanup,
    Network,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkOperation {
    WebFetch,
    SendWebhook,
}

#[derive(Clone, Debug, Serialize)]
pub struct BindingSummary {
    pub id: String,
    pub revision: u64,
    pub policy: Value,
}
#[derive(Clone, Debug, Serialize)]
pub struct PlanSummary {
    pub id: String,
    pub binding_id: String,
    pub revision: u64,
    pub tool: String,
    pub arguments: Value,
    pub policy: Value,
    pub workspace_sha256: Option<String>,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub expires_in_seconds: u64,
}
/// Audit/review only. Does not implement Deserialize and cannot execute.
pub struct PreparedPlan {
    summary: PlanSummary,
}
impl PreparedPlan {
    pub fn summary(&self) -> &PlanSummary {
        &self.summary
    }
}
/// Single-use authority created only by approval of an existing server plan.
/// Never reconstruct this value from an HTTP body or persisted audit JSON.
pub struct ApprovedPlan {
    owner: String,
    plan: Plan,
}
impl ApprovedPlan {
    pub fn summary(&self) -> &PlanSummary {
        &self.plan.summary
    }
}
struct Plan {
    summary: PlanSummary,
    operation: Operation,
    created: Instant,
}
enum Operation {
    Shell {
        executable: String,
        argv: Vec<String>,
    },
    Git {
        action: GitAction,
    },
    Fetch,
    Webhook {
        body: Value,
    },
}
enum GitAction {
    Init,
    Clone {
        source: String,
        revision: u64,
        digest: String,
    },
    Commit(String),
    Diff(Option<String>),
    Blame(String),
    Status,
    Log(u64),
}
enum Binding {
    Workspace(workspace::WorkspaceBinding),
    Network(DestinationGrant),
}
struct Entry {
    revision: u64,
    binding: Binding,
}
struct State {
    bindings: BTreeMap<String, Entry>,
    plans: BTreeMap<String, Plan>,
}

/// Long-lived trusted host state. Calls serialize per host so approval cannot
/// race a policy change. Execute from tracked blocking work, not an async task.
pub struct EffectHost {
    id: String,
    state: Mutex<State>,
}
impl Default for EffectHost {
    fn default() -> Self {
        Self::new()
    }
}
impl EffectHost {
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            state: Mutex::new(State {
                bindings: BTreeMap::new(),
                plans: BTreeMap::new(),
            }),
        }
    }
    pub fn bind_workspace(
        &self,
        workspace: WorkspaceGrant,
        executables: Vec<ExecutableGrant>,
        git: Option<ExecutableGrant>,
    ) -> Result<BindingSummary> {
        let binding = workspace::WorkspaceBinding::new(workspace, executables, git)?;
        self.insert(Binding::Workspace(binding))
    }
    pub fn bind_destination(&self, destination: DestinationGrant) -> Result<BindingSummary> {
        self.insert(Binding::Network(destination))
    }
    fn insert(&self, binding: Binding) -> Result<BindingSummary> {
        let mut state = self.state.lock().map_err(|_| Error::Io)?;
        if state.bindings.len() >= 32 {
            return Err(Error::Limit("bindings"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let policy = policy(&binding);
        state.bindings.insert(
            id.clone(),
            Entry {
                revision: 0,
                binding,
            },
        );
        Ok(BindingSummary {
            id,
            revision: 0,
            policy,
        })
    }
    pub fn binding(&self, id: &str) -> Result<BindingSummary> {
        let state = self.state.lock().map_err(|_| Error::Io)?;
        let entry = state.bindings.get(id).ok_or(Error::Denied("binding"))?;
        Ok(BindingSummary {
            id: id.into(),
            revision: entry.revision,
            policy: policy(&entry.binding),
        })
    }
    pub fn revoke(&self, id: &str) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Error::Io)?;
        state.bindings.remove(id).ok_or(Error::Denied("binding"))?;
        state.plans.retain(|_, p| p.summary.binding_id != id);
        Ok(())
    }
    pub fn prepare(
        &self,
        binding_id: &str,
        revision: u64,
        tool: &str,
        arguments: Value,
    ) -> Result<PreparedPlan> {
        if serde_json::to_vec(&arguments)
            .map_err(|_| Error::Invalid("arguments"))?
            .len()
            > 65536
        {
            return Err(Error::Limit("arguments"));
        }
        let mut state = self.state.lock().map_err(|_| Error::Io)?;
        state
            .plans
            .retain(|_, p| p.created.elapsed() < Duration::from_secs(300));
        if state.plans.len() >= 32 {
            return Err(Error::Limit("pending plans"));
        }
        let entry = state
            .bindings
            .get(binding_id)
            .ok_or(Error::Denied("binding"))?;
        if entry.revision != revision {
            return Err(Error::Stale);
        }
        let (operation, timeout, digest) =
            prepare_operation(&state, &entry.binding, tool, &arguments)?;
        let mut summary = PlanSummary {
            id: String::new(),
            binding_id: binding_id.into(),
            revision,
            tool: tool.into(),
            arguments,
            policy: policy(&entry.binding),
            workspace_sha256: digest,
            timeout_ms: timeout,
            max_output_bytes: 256 * 1024,
            expires_in_seconds: 300,
        };
        if let Operation::Git {
            action:
                GitAction::Clone {
                    source,
                    revision,
                    digest,
                },
        } = &operation
        {
            summary.policy["clone_source"] =
                json!({"binding_id":source,"revision":revision,"sha256":digest});
        }
        // A random nonce prevents two otherwise-identical plans being confused.
        let nonce = uuid::Uuid::new_v4().to_string();
        summary.id = hash(
            &[
                nonce.as_bytes(),
                &serde_json::to_vec(&summary).map_err(|_| Error::Io)?,
            ]
            .concat(),
        );
        state.plans.insert(
            summary.id.clone(),
            Plan {
                summary: summary.clone(),
                operation,
                created: Instant::now(),
            },
        );
        Ok(PreparedPlan { summary })
    }
    pub fn approve(&self, exact_plan_id: &str, revision: u64) -> Result<ApprovedPlan> {
        let mut state = self.state.lock().map_err(|_| Error::Io)?;
        let plan = state
            .plans
            .get(exact_plan_id)
            .ok_or(Error::Denied("reviewed plan"))?;
        let entry = state
            .bindings
            .get(&plan.summary.binding_id)
            .ok_or(Error::Denied("binding"))?;
        if plan.summary.revision != revision
            || entry.revision != revision
            || plan.created.elapsed() >= Duration::from_secs(300)
        {
            return Err(Error::Stale);
        }
        let plan = state.plans.remove(exact_plan_id).ok_or(Error::Stale)?;
        Ok(ApprovedPlan {
            owner: self.id.clone(),
            plan,
        })
    }
    /// The caller must check durable task/session cancellation and revisions in
    /// check_active. Returning Err cancels and reaps an owned process before
    /// this method returns. Network POST cancellation cannot undo delivery.
    pub fn execute(
        &self,
        approved: ApprovedPlan,
        check_active: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        if approved.owner != self.id {
            return Err(Error::Denied("plan owner"));
        }
        let plan = approved.plan;
        let mut state = self.state.lock().map_err(|_| Error::Io)?;
        check_active()?;
        let entry = state
            .bindings
            .get(&plan.summary.binding_id)
            .ok_or(Error::Denied("binding"))?;
        if entry.revision != plan.summary.revision
            || plan.created.elapsed() >= Duration::from_secs(300)
        {
            return Err(Error::Stale);
        }
        if let Binding::Workspace(work) = &entry.binding {
            if Some(work.digest()?) != plan.summary.workspace_sha256 {
                return Err(Error::Stale);
            }
            work.verify_executables()?;
        }
        if let Operation::Git {
            action:
                GitAction::Clone {
                    source,
                    revision,
                    digest,
                },
        } = &plan.operation
        {
            match state.bindings.get(source) {
                Some(Entry {
                    binding: Binding::Workspace(source_work),
                    revision: current,
                }) if current == revision && source_work.digest()? == *digest => {}
                _ => return Err(Error::Stale),
            }
        }
        let started = Instant::now();
        let mut bounded_check = || {
            if started.elapsed() >= Duration::from_millis(plan.summary.timeout_ms) {
                return Err(Error::TimedOut);
            }
            check_active()
        };
        let check_active: &mut dyn FnMut() -> Result<()> = &mut bounded_check;
        // Consume the revision before attempting effects. Failure/cancellation
        // never authorizes replay of an uncertain commit or webhook delivery.
        state
            .bindings
            .get_mut(&plan.summary.binding_id)
            .ok_or(Error::Stale)?
            .revision += 1;
        let result = match (
            &state.bindings[&plan.summary.binding_id].binding,
            plan.operation,
        ) {
            (Binding::Workspace(work), Operation::Shell { executable, argv }) => {
                work.shell(&executable, &argv, &plan.summary, check_active)
            }
            (
                Binding::Workspace(work),
                Operation::Git {
                    action:
                        GitAction::Clone {
                            source,
                            revision,
                            digest,
                        },
                },
            ) => {
                let source = match state.bindings.get(&source) {
                    Some(Entry {
                        binding: Binding::Workspace(source),
                        revision: current,
                    }) if *current == revision && source.digest()? == digest => source,
                    _ => return Err(Error::Denied("clone source")),
                };
                work.clone_from(source, &plan.summary, check_active)
            }
            (Binding::Workspace(work), Operation::Git { action }) => {
                work.git(action, &plan.summary, check_active)
            }
            (Binding::Network(destination), Operation::Fetch) => {
                network::execute(destination, None, &plan.summary, check_active)
            }
            (Binding::Network(destination), Operation::Webhook { body }) => {
                network::execute(destination, Some(body), &plan.summary, check_active)
            }
            _ => Err(Error::Denied("binding kind")),
        };
        result
    }
}
fn policy(binding: &Binding) -> Value {
    match binding {
        Binding::Workspace(w) => w.policy(),
        Binding::Network(n) => n.policy(),
    }
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn parse<T: for<'a> Deserialize<'a>>(value: &Value) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|_| Error::Invalid("tool arguments"))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    command: String,
    timeout_secs: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitArgs {
    message: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogArgs {
    max_count: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloneArgs {
    url: String,
    directory: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    url: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookArgs {
    url: String,
    message: String,
    title: Option<String>,
}
fn prepare_operation(
    state: &State,
    binding: &Binding,
    tool: &str,
    args: &Value,
) -> Result<(Operation, u64, Option<String>)> {
    if let Binding::Network(destination) = binding {
        let operation = match tool {
            "web_fetch" if destination.operation() == NetworkOperation::WebFetch => {
                let a: FetchArgs = parse(args)?;
                destination.validate_url(&a.url)?;
                Operation::Fetch
            }
            "send_webhook" if destination.operation() == NetworkOperation::SendWebhook => {
                let a: WebhookArgs = parse(args)?;
                destination.validate_url(&a.url)?;
                if a.message.len() > 32768 || a.title.as_ref().is_some_and(|s| s.len() > 1024) {
                    return Err(Error::Limit("webhook payload"));
                }
                let mut body = json!({"text":a.message});
                if let Some(title) = a.title {
                    body["title"] = json!(title);
                }
                Operation::Webhook { body }
            }
            _ => return Err(Error::Denied("destination operation")),
        };
        return Ok((operation, 15_000, None));
    }
    let Binding::Workspace(work) = binding else {
        return Err(Error::Denied("workspace"));
    };
    if !cfg!(target_os = "macos") {
        return Err(Error::Unsupported(
            "process effects require qualified macOS sandbox",
        ));
    }
    let mut timeout = 10_000;
    let operation = match tool {
        "run_shell" => {
            let a: ShellArgs = parse(args)?;
            let secs = a.timeout_secs.unwrap_or(10);
            if !(1..=30).contains(&secs) {
                return Err(Error::Invalid("timeout_secs"));
            }
            timeout = secs * 1000;
            let tokens = process::parse_command(&a.command)?;
            let executable = work.resolve_command(&tokens[0])?;
            Operation::Shell {
                executable,
                argv: tokens[1..].to_vec(),
            }
        }
        "git_init" => {
            let _: Empty = parse(args)?;
            work.require_git()?;
            Operation::Git {
                action: GitAction::Init,
            }
        }
        "git_clone" => {
            let a: CloneArgs = parse(args)?;
            work.require_git()?;
            if a.directory.as_deref().is_some_and(|s| s != ".") {
                return Err(Error::Unsupported(
                    "clone destination is the selected owned workspace; use directory '.'",
                ));
            }
            let source = a.url.strip_prefix("owned://").ok_or(Error::Unsupported(
                "remote Git transport is not qualified; clone requires owned://binding-id",
            ))?;
            let source = state
                .bindings
                .get(source)
                .ok_or(Error::Denied("clone source binding"))?;
            let Binding::Workspace(source_work) = &source.binding else {
                return Err(Error::Denied("clone source workspace"));
            };
            source_work.require_repository()?;
            Operation::Git {
                action: GitAction::Clone {
                    source: a.url[8..].into(),
                    revision: source.revision,
                    digest: source_work.digest()?,
                },
            }
        }
        "git_commit" => {
            let a: CommitArgs = parse(args)?;
            if a.message.trim().is_empty() || a.message.len() > 8192 || a.message.contains('\0') {
                return Err(Error::Invalid("commit message"));
            }
            Operation::Git {
                action: GitAction::Commit(a.message),
            }
        }
        "git_diff" | "git_blame" => {
            let a: PathArgs = parse(args)?;
            if let Some(ref path) = a.path {
                workspace::relative(path)?;
            }
            let action = if tool == "git_diff" {
                GitAction::Diff(a.path)
            } else {
                GitAction::Blame(a.path.ok_or(Error::Invalid("path"))?)
            };
            Operation::Git { action }
        }
        "git_status" => {
            let _: Empty = parse(args)?;
            Operation::Git {
                action: GitAction::Status,
            }
        }
        "git_log" => {
            let a: LogArgs = parse(args)?;
            let n = a.max_count.unwrap_or(20);
            if !(1..=100).contains(&n) {
                return Err(Error::Invalid("max_count"));
            }
            Operation::Git {
                action: GitAction::Log(n),
            }
        }
        _ => return Err(Error::Unsupported("tool")),
    };
    if tool.starts_with("git_") && tool != "git_init" && tool != "git_clone" {
        work.require_repository()?;
    }
    Ok((operation, timeout, Some(work.digest()?)))
}

/// Metadata only; host registration and explicit grants decide availability.
pub fn descriptors() -> Vec<Value> {
    [
        ("run_shell","shell","Shell",json!({"command":{"type":"string"},"timeout_secs":{"type":"integer","minimum":1,"maximum":30}}),vec!["command"]),
        ("git_clone","git","GitWrite",json!({"url":{"type":"string"},"directory":{"type":"string"}}),vec!["url"]),
        ("git_commit","git","GitWrite",json!({"message":{"type":"string"}}),vec!["message"]),
        ("git_diff","git","GitRead",json!({"path":{"type":"string"}}),vec![]),
        ("git_blame","git","GitRead",json!({"path":{"type":"string"}}),vec!["path"]),
        ("git_status","git","GitRead",json!({}),vec![]),
        ("git_log","git","GitRead",json!({"max_count":{"type":"integer","minimum":1,"maximum":100}}),vec![]),
        ("web_fetch","web","WebFetch",json!({"url":{"type":"string"}}),vec!["url"]),
        ("send_webhook","channels","SendWebhook",json!({"url":{"type":"string"},"message":{"type":"string"},"title":{"type":"string"}}),vec!["url","message"]),
    ].into_iter().map(|(name,source,cap,properties,required)|json!({"name":name,"source":format!("builtin:{source}"),"capability":cap,"requires_approval":true,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})).collect()
}
