use crate::workspace::{EditExpectation, EditRequest, MAX_SEARCH_BYTES};
use crate::{
    error::bounded_text, Capability, Error, ExecutionGrant, HostToolAdapter, HostToolContext,
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
impl ToolCall {
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum Availability {
    Implemented,
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub source: String,
    pub parameters: Value,
    pub availability: Availability,
    pub required_capability: Option<Capability>,
    pub requires_approval: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ToolLimits {
    pub max_file_bytes: usize,
    pub max_directory_entries: usize,
    pub max_calls_per_task: usize,
}
impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 64 * 1024,
            max_directory_entries: 128,
            max_calls_per_task: 64,
        }
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    pub(crate) limits: ToolLimits,
    adapters: Vec<Arc<dyn HostToolAdapter>>,
    host_tools: BTreeMap<String, (usize, ToolDescriptor)>,
}

pub(crate) enum Request {
    Read(String),
    List(String),
    Write {
        path: String,
        contents: String,
    },
    Search {
        query: String,
        max_results: usize,
        max_bytes: usize,
    },
    Edit(EditRequest),
    Discover,
    Plan(Vec<String>),
    Clarify(String),
    Complete(String),
    Host {
        adapter: usize,
        call: ToolCall,
        requires_approval: bool,
    },
}
impl Request {
    pub(crate) fn requires_approval(&self) -> bool {
        matches!(
            self,
            Self::Write { .. }
                | Self::Edit(_)
                | Self::Host {
                    requires_approval: true,
                    ..
                }
        )
    }
}

impl ToolRegistry {
    pub fn new(limits: ToolLimits) -> Result<Self> {
        if limits.max_file_bytes == 0
            || limits.max_file_bytes > 1024 * 1024
            || limits.max_directory_entries == 0
            || limits.max_directory_entries > 1024
            || limits.max_calls_per_task == 0
            || limits.max_calls_per_task > 64
        {
            return Err(Error::InvalidInput(
                "tool limits exceed hard ceilings".into(),
            ));
        }
        Ok(Self {
            limits,
            ..Self::default()
        })
    }

    /// Trusted-host construction only. Adapters may fill an unimplemented source
    /// tool but cannot replace built-in tools or another registered adapter.
    pub fn with_adapter(mut self, adapter: Arc<dyn HostToolAdapter>) -> Result<Self> {
        let descriptors = adapter.descriptors();
        let index = self.adapters.len();
        if descriptors.is_empty() {
            return Err(Error::InvalidInput("adapter has no descriptors".into()));
        }
        for descriptor in descriptors {
            let existing = self
                .all_tools()
                .into_iter()
                .find(|d| d.name == descriptor.name)
                .ok_or_else(|| {
                    Error::Unsupported("adapter name is outside the source inventory".into())
                })?;
            if existing.availability == Availability::Implemented
                || self.host_tools.contains_key(&descriptor.name)
                || descriptor.required_capability.is_none()
                || descriptor.parameters["type"] != "object"
                || descriptor.parameters["additionalProperties"] != false
            {
                return Err(Error::InvalidInput(
                    "adapter registration conflicts or lacks strict capability/schema".into(),
                ));
            }
            self.host_tools
                .insert(descriptor.name.clone(), (index, descriptor));
        }
        self.adapters.push(adapter);
        Ok(self)
    }

    /// Complete source inventory, with explicit implementation status. Discovery
    /// never grants execution permission. Unsupported names remain visible.
    pub fn all_tools(&self) -> Vec<ToolDescriptor> {
        let mut tools = vec![
            descriptor(
                "read_file",
                "fs",
                Some(Capability::ReadWorkspace),
                false,
                json!({"path":{"type":"string"}}),
                &["path"],
            ),
            descriptor(
                "write_file",
                "fs",
                Some(Capability::WriteWorkspace),
                true,
                json!({"path":{"type":"string"},"contents":{"type":"string"}}),
                &["path", "contents"],
            ),
            descriptor(
                "list_directory",
                "fs",
                Some(Capability::ListWorkspace),
                false,
                json!({"path":{"type":"string","default":"."}}),
                &[],
            ),
            descriptor(
                "search",
                "fs",
                Some(Capability::SearchWorkspace),
                false,
                json!({"query":{"type":"string","minLength":1,"maxLength":4096},"max_results":{"type":"integer","minimum":1},"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_SEARCH_BYTES}}),
                &["query"],
            ),
            {
                let mut edit = descriptor(
                    "edit_file",
                    "fs",
                    Some(Capability::EditWorkspace),
                    true,
                    json!({"path":{"type":"string"},"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},"expected_contents":{"type":"string"},"expected_sha256":{"type":"string","pattern":"^[a-fA-F0-9]{64}$"}}),
                    &["path", "old_string", "new_string"],
                );
                edit.parameters["oneOf"] =
                    json!([{"required":["expected_contents"]},{"required":["expected_sha256"]}]);
                edit
            },
            descriptor(
                "discover_capabilities",
                "capabilities",
                None,
                false,
                json!({}),
                &[],
            ),
            descriptor(
                "update_plan",
                "agent",
                Some(Capability::TaskControl),
                false,
                json!({"steps":{"type":"array","items":{"type":"string"}}}),
                &["steps"],
            ),
            descriptor(
                "ask_clarification",
                "agent",
                Some(Capability::TaskControl),
                false,
                json!({"question":{"type":"string"}}),
                &["question"],
            ),
            descriptor(
                "task_complete",
                "agent",
                Some(Capability::TaskControl),
                false,
                json!({"summary":{"type":"string"}}),
                &["summary"],
            ),
        ];
        for (source, names) in [
            ("shell", &["run_shell"][..]),
            (
                "git",
                &[
                    "git_clone",
                    "git_commit",
                    "git_diff",
                    "git_blame",
                    "git_status",
                    "git_log",
                ][..],
            ),
            (
                "computer",
                &["screen_elements", "screen_click", "screen_type"][..],
            ),
            ("web", &["web_fetch"][..]),
            ("channel", &["send_webhook"][..]),
            ("memory", &["recall_memory", "save_memory"][..]),
            ("embedding", &["embed_text"][..]),
            ("document", &["read_document", "write_document"][..]),
            ("config", &["smith_status"][..]),
            (
                "conversation",
                &[
                    "list_conversations",
                    "search_conversations",
                    "read_conversation",
                    "export_conversation",
                ][..],
            ),
            ("system", &["system_monitor"][..]),
            ("chart", &["render_chart"][..]),
            ("image", &["generate_image"][..]),
            ("artifact", &["save_artifact"][..]),
        ] {
            for name in names {
                let mut d = descriptor(name, source, None, false, json!({}), &[]);
                d.availability = Availability::Unsupported(format!(
                    "{source} adapter is not connected in this host; no automatic fallback"
                ));
                tools.push(d);
            }
        }
        for tool in &mut tools {
            if let Some((_, registered)) = self.host_tools.get(&tool.name) {
                *tool = registered.clone();
            }
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    pub fn available_tools(&self, grant: &ExecutionGrant) -> Vec<ToolDescriptor> {
        self.all_tools()
            .into_iter()
            .filter(|d| {
                d.availability == Availability::Implemented
                    && d.required_capability.is_none_or(|cap| grant.permits(cap))
            })
            .collect()
    }

    pub(crate) fn prepare(&self, grant: &ExecutionGrant, call: &ToolCall) -> Result<Request> {
        if call.id.is_empty() || call.id.len() > 128 || call.name.len() > 128 {
            return Err(Error::InvalidInput("tool call identity".into()));
        }
        bounded_text(
            &call.arguments.to_string(),
            self.limits.max_file_bytes * 6 + 8192,
            "tool arguments",
        )?;
        let descriptor = self
            .all_tools()
            .into_iter()
            .find(|d| d.name == call.name)
            .ok_or_else(|| {
                Error::Unsupported(format!("unknown or unconnected tool: {}", call.name))
            })?;
        if let Availability::Unsupported(reason) = descriptor.availability {
            return Err(Error::Unsupported(format!("{}: {reason}", call.name)));
        }
        if descriptor
            .required_capability
            .is_some_and(|cap| !grant.permits(cap))
        {
            return Err(Error::Denied(format!(
                "{} capability is not granted",
                call.name
            )));
        }
        if let Some((adapter, _)) = self.host_tools.get(&call.name) {
            self.adapters[*adapter].validate(grant, call)?;
            return Ok(Request::Host {
                adapter: *adapter,
                call: call.clone(),
                requires_approval: descriptor.requires_approval,
            });
        }
        Ok(match call.name.as_str() {
            "read_file" => {
                let args = parse::<PathArgs>(&call.arguments)?;
                crate::workspace::validate_name(&args.path)?;
                Request::Read(args.path)
            }
            "list_directory" => {
                let args = parse::<ListArgs>(&call.arguments)?;
                if args.path != "." {
                    return Err(Error::UnsafePath(
                        "only the workspace root can be listed".into(),
                    ));
                }
                Request::List(args.path)
            }
            "write_file" => {
                let args = parse::<WriteArgs>(&call.arguments)?;
                crate::workspace::validate_name(&args.path)?;
                bounded_text(
                    &args.contents,
                    self.limits.max_file_bytes,
                    "file write bytes",
                )?;
                Request::Write {
                    path: args.path,
                    contents: args.contents,
                }
            }
            "search" => {
                let args = parse::<SearchArgs>(&call.arguments)?;
                if args.query.is_empty() {
                    return Err(Error::InvalidInput(
                        "query must be a nonempty literal".into(),
                    ));
                }
                bounded_text(&args.query, 4096, "search query bytes")?;
                let max_results = args
                    .max_results
                    .unwrap_or(50.min(self.limits.max_directory_entries));
                let max_bytes = args.max_bytes.unwrap_or(MAX_SEARCH_BYTES);
                if max_results == 0
                    || max_results > self.limits.max_directory_entries
                    || max_bytes == 0
                    || max_bytes > MAX_SEARCH_BYTES
                {
                    return Err(Error::LimitExceeded("search result or byte budget".into()));
                }
                Request::Search {
                    query: args.query,
                    max_results,
                    max_bytes,
                }
            }
            "edit_file" => {
                let args = parse::<EditArgs>(&call.arguments)?;
                crate::workspace::validate_name(&args.path)?;
                if args.old_string.is_empty() {
                    return Err(Error::InvalidInput("old_string must be nonempty".into()));
                }
                bounded_text(
                    &args.old_string,
                    self.limits.max_file_bytes,
                    "old_string bytes",
                )?;
                bounded_text(
                    &args.new_string,
                    self.limits.max_file_bytes,
                    "new_string bytes",
                )?;
                let expected = match (args.expected_contents, args.expected_sha256) {
                    (Some(contents), None) => {
                        bounded_text(
                            &contents,
                            self.limits.max_file_bytes,
                            "expected content bytes",
                        )?;
                        EditExpectation::Contents(contents)
                    }
                    (None, Some(hash))
                        if hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()) =>
                    {
                        EditExpectation::Sha256(hash.to_ascii_lowercase())
                    }
                    _ => {
                        return Err(Error::InvalidInput(
                            "provide exactly one expected_contents or 64-digit expected_sha256"
                                .into(),
                        ))
                    }
                };
                Request::Edit(EditRequest {
                    path: args.path,
                    old_string: args.old_string,
                    new_string: args.new_string,
                    expected,
                })
            }
            "discover_capabilities" => {
                parse::<EmptyArgs>(&call.arguments)?;
                Request::Discover
            }
            "update_plan" => {
                let args = parse::<PlanArgs>(&call.arguments)?;
                if args.steps.is_empty()
                    || args.steps.len() > 32
                    || args.steps.iter().any(|s| s.is_empty() || s.len() > 1024)
                {
                    return Err(Error::InvalidInput(
                        "plan requires 1–32 nonempty steps of at most 1024 bytes".into(),
                    ));
                }
                Request::Plan(args.steps)
            }
            "ask_clarification" => {
                let args = parse::<QuestionArgs>(&call.arguments)?;
                text_argument(&args.question)?;
                Request::Clarify(args.question)
            }
            "task_complete" => {
                let args = parse::<SummaryArgs>(&call.arguments)?;
                text_argument(&args.summary)?;
                Request::Complete(args.summary)
            }
            _ => unreachable!("implemented descriptor and dispatcher must agree"),
        })
    }

    // Execution is crate-private so write approval cannot be bypassed by a
    // public registry.call() route. Host applications use TaskEngine::call.
    pub(crate) fn execute(
        &self,
        grant: &ExecutionGrant,
        request: Request,
        task_id: &str,
        started_revision: u64,
        check_active: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        match request {
            Request::Host { adapter, call, .. } => {
                check_active()?;
                self.adapters[adapter].validate(grant, &call)?;
                let result = self.adapters[adapter].execute(
                    &HostToolContext {
                        task_id,
                        started_revision,
                        call: &call,
                        grant,
                    },
                    check_active,
                )?;
                if result.to_string().len() > 256 * 1024 {
                    return Err(Error::UncertainSideEffect("host adapter exceeded its bounded result contract; inspect its receipt before retrying".into()));
                }
                Ok(result)
            }
            Request::Read(path) => {
                let contents = grant.workspace().read(&path, self.limits.max_file_bytes)?;
                let sha256 = crate::workspace::sha256(&contents);
                Ok(json!({"path":path,"contents":contents,"sha256":sha256}))
            }
            Request::List(path) => Ok(
                json!({"entries":grant.workspace().list(&path, self.limits.max_directory_entries)?}),
            ),
            Request::Write { path, contents } => {
                grant.workspace().write_atomic(
                    &path,
                    &contents,
                    self.limits.max_file_bytes,
                    self.limits.max_directory_entries,
                )?;
                Ok(json!({"path":path,"bytes_written":contents.len()}))
            }
            Request::Search {
                query,
                max_results,
                max_bytes,
            } => serde_json::to_value(grant.workspace().search(
                &query,
                max_results,
                max_bytes,
                self.limits.max_file_bytes,
                self.limits.max_directory_entries,
                check_active,
            )?)
            .map_err(|error| Error::InvalidInput(error.to_string())),
            Request::Edit(request) => serde_json::to_value(grant.workspace().edit_atomic(
                &request,
                self.limits.max_file_bytes,
                check_active,
            )?)
            .map_err(|error| Error::InvalidInput(error.to_string())),
            Request::Discover => {
                Ok(json!({"tools":self.all_tools(),"granted":self.available_tools(grant)}))
            }
            _ => Err(Error::InvalidTransition(
                "task-control tools require the state machine".into(),
            )),
        }
    }
}

fn descriptor(
    name: &str,
    source: &str,
    cap: Option<Capability>,
    approval: bool,
    properties: Value,
    required: &[&str],
) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        source: format!("builtin:{source}"),
        parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        availability: Availability::Implemented,
        required_capability: cap,
        requires_approval: approval,
    }
}
fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|e| Error::InvalidInput(e.to_string()))
}
fn text_argument(s: &str) -> Result<()> {
    if s.trim().is_empty() {
        return Err(Error::InvalidInput("nonempty text required".into()));
    }
    bounded_text(s, 8192, "text argument")
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    contents: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    max_results: Option<usize>,
    max_bytes: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    expected_contents: Option<String>,
    expected_sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default = "root_path")]
    path: String,
}
fn root_path() -> String {
    ".".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanArgs {
    steps: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionArgs {
    question: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryArgs {
    summary: String,
}
