//! Host-owned task persistence on the application's already-keyed connection.
//! This module never opens a database, receives a key, or writes a policy file.
use crate::App;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use smith_agent_core::{
    decode_task_record, decode_trace_event, encode_transition, Capability, Error, ExecutionGrant,
    HostToolContext, Principal, ResourceKind, ResourceScope, Result, Task, ToolRegistry,
    TraceEvent, TraceRecord, TraceStore, Workspace, WorkspaceIdentity,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Weak},
};

const MAX_WORKSPACES: i64 = 32;
const MAX_TASKS: i64 = 256;

#[derive(Clone)]
pub struct AgentStore {
    app: Weak<App>,
    owner: Principal,
}

#[derive(Clone)]
pub struct WorkspacePolicy {
    pub id: String,
    pub label: String,
    pub identity: WorkspaceIdentity,
    pub capabilities: BTreeSet<Capability>,
    pub resources: BTreeSet<ResourceScope>,
    pub revision: u64,
    pub status: String,
}

impl AgentStore {
    /// App construction owns verification/keying of this connection. There is
    /// intentionally no path-based or caller-selected connection constructor.
    pub fn new(app: &Arc<App>) -> Result<Self> {
        let owner = {
            let db = app.db.lock().map_err(|_| storage())?;
            let id: String = db
                .query_row(
                    "SELECT principal_id FROM agent_profile WHERE id=1",
                    [],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            Principal::new(id)?
        };
        Ok(Self {
            app: Arc::downgrade(app),
            owner,
        })
    }

    pub fn owner(&self) -> &Principal {
        &self.owner
    }

    pub(crate) fn with_db<T>(
        &self,
        action: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| Error::Denied("profile is closed".into()))?;
        let mut db = app.db.lock().map_err(|_| storage())?;
        action(&mut db)
    }

    pub fn provision_scoped(
        &self,
        label: String,
        capabilities: BTreeSet<Capability>,
        resources: BTreeSet<ResourceScope>,
    ) -> Result<WorkspacePolicy> {
        if label.trim().is_empty() || label.len() > 80 || label.chars().any(char::is_control) {
            return Err(Error::InvalidInput(
                "workspace label must contain 1–80 bytes without control characters".into(),
            ));
        }
        if capabilities.iter().any(|c| !supported_capability(*c)) {
            return Err(Error::Denied("unsupported workspace capability".into()));
        }
        if resources.len() > 128
            || resources
                .iter()
                .any(|r| r.id.is_empty() || r.id.len() > 256 || r.id.chars().any(char::is_control))
        {
            return Err(Error::InvalidInput("resource scopes".into()));
        }
        self.with_db(|db| {
            for resource in &resources {
                let found=match resource.kind {
                    ResourceKind::Conversation=>db.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM chat_trash WHERE chat_id=?1))",[&resource.id],|r|r.get::<_,bool>(0)).map_err(sql)?,
                    ResourceKind::Document=>db.query_row("SELECT EXISTS(SELECT 1 FROM media_objects WHERE id=?1)",[&resource.id],|r|r.get::<_,bool>(0)).map_err(sql)?,
                    ResourceKind::ArtifactCollection=>resource.id=="unowned",
                    ResourceKind::MemoryNamespace=>resource.id.len()<=64 && resource.id.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_.".contains(&b)),
                    ResourceKind::ProviderBinding|ResourceKind::EffectBinding=>true,
                };
                if !found { return Err(Error::Denied("selected resource is unavailable or invalid".into())); }
            }
            Ok(())
        })?;
        let workspace = Workspace::create_temporary()?;
        let policy = WorkspacePolicy {
            id: crate::id(),
            label: label.trim().into(),
            identity: workspace.identity().clone(),
            capabilities,
            resources,
            revision: 1,
            status: "active".into(),
        };
        let result = self.with_db(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
            let count: i64 = tx.query_row("SELECT count(*) FROM agent_workspaces WHERE principal_id=?1", [self.owner.id()], |r| r.get(0)).map_err(sql)?;
            if count >= MAX_WORKSPACES { return Err(Error::LimitExceeded("profile temporary workspaces".into())); }
            tx.execute("INSERT INTO agent_workspaces(id,principal_id,label,identity,capabilities,resources,policy_revision,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,1,'active',?7)", params![policy.id, self.owner.id(), policy.label, encode(&policy.identity)?, encode(&policy.capabilities)?, encode(&policy.resources)?,crate::now()]).map_err(sql)?;
            tx.commit().map_err(sql)
        });
        if result.is_err() {
            // Provisioning has not exposed this empty root to a task. Never
            // recursively delete a caller-selected path during cleanup.
            let _ = std::fs::remove_dir(workspace.host_path());
        }
        result?;
        Ok(policy)
    }

    pub fn policies(&self) -> Result<Vec<WorkspacePolicy>> {
        self.with_db(|db| {
            let mut query = db.prepare("SELECT id,label,identity,capabilities,policy_revision,status,resources FROM agent_workspaces WHERE principal_id=?1 ORDER BY created_at,id LIMIT 33").map_err(sql)?;
            let rows = query.query_map([self.owner.id()], policy_row).map_err(sql)?;
            let policies = rows.map(|r| decode_policy(r.map_err(sql)?)).collect::<Result<Vec<_>>>()?;
            if policies.len() > MAX_WORKSPACES as usize { return Err(Error::LimitExceeded("profile temporary workspaces".into())); }
            Ok(policies)
        })
    }

    pub fn policy(&self, id: &str) -> Result<WorkspacePolicy> {
        self.with_db(|db| policy(db, self.owner.id(), id))
    }
    /// Explicit human grant revision before a workspace is attached to any task.
    /// Existing task authority cannot be widened by editing its workspace policy.
    pub fn revise_policy(
        &self,
        id: &str,
        revision: u64,
        capabilities: BTreeSet<Capability>,
        resources: BTreeSet<ResourceScope>,
    ) -> Result<WorkspacePolicy> {
        if capabilities.iter().any(|c| !supported_capability(*c))
            || resources.len() > 128
            || resources
                .iter()
                .any(|r| r.id.is_empty() || r.id.len() > 256 || r.id.chars().any(char::is_control))
        {
            return Err(Error::InvalidInput("Explicit grant selection".into()));
        }
        self.with_db(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;let existing=policy(&tx,self.owner.id(),id)?;
            if existing.revision!=revision || tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_tasks WHERE workspace_id=?1)",[id],|r|r.get::<_,bool>(0)).map_err(sql)?{return Err(Error::Conflict);}
            for resource in &resources {
                let valid=match resource.kind {
                    ResourceKind::Conversation=>tx.query_row("SELECT EXISTS(SELECT 1 FROM chats WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM chat_trash WHERE chat_id=?1))",[&resource.id],|r|r.get::<_,bool>(0)).map_err(sql)?,
                    ResourceKind::Document=>tx.query_row("SELECT EXISTS(SELECT 1 FROM media_objects WHERE id=?1)",[&resource.id],|r|r.get::<_,bool>(0)).map_err(sql)?,
                    ResourceKind::ArtifactCollection=>resource.id=="unowned",
                    ResourceKind::MemoryNamespace=>resource.id.len()<=64&&resource.id.bytes().all(|b|b.is_ascii_alphanumeric()||b"._-".contains(&b)),
                    ResourceKind::EffectBinding|ResourceKind::ProviderBinding=>true,
                };if !valid{return Err(Error::Denied("Selected resource is unavailable".into()));}
            }
            tx.execute("UPDATE agent_workspaces SET capabilities=?2,resources=?3,policy_revision=policy_revision+1 WHERE id=?1",params![id,encode(&capabilities)?,encode(&resources)?]).map_err(sql)?;tx.commit().map_err(sql)?;policy(db,self.owner.id(),id)
        })
    }

    pub fn selectable_resources(&self) -> Result<serde_json::Value> {
        self.with_db(|db|{
            let chats=db.prepare("SELECT id,substr(title,1,256),version FROM chats WHERE NOT EXISTS(SELECT 1 FROM chat_trash WHERE chat_id=chats.id) ORDER BY updated_at DESC LIMIT 129").map_err(sql)?.query_map([],|r|Ok(serde_json::json!({"kind":"conversation","id":r.get::<_,String>(0)?,"label":r.get::<_,String>(1)?,"version":r.get::<_,i64>(2)?}))).map_err(sql)?.collect::<std::result::Result<Vec<_>,_>>().map_err(sql)?;
            let documents=db.prepare("SELECT id,name FROM media_objects ORDER BY created_at DESC LIMIT 129").map_err(sql)?.query_map([],|r|Ok(serde_json::json!({"kind":"document","id":r.get::<_,String>(0)?,"label":r.get::<_,String>(1)?}))).map_err(sql)?.collect::<std::result::Result<Vec<_>,_>>().map_err(sql)?;
            Ok(serde_json::json!({"conversations":chats.iter().take(128).collect::<Vec<_>>(),"documents":documents.iter().take(128).collect::<Vec<_>>(),"truncated":chats.len()>128||documents.len()>128}))
        })
    }

    pub fn policy_for_task(&self, id: &str) -> Result<WorkspacePolicy> {
        self.with_db(|db| {
            let workspace: Option<String> = db
                .query_row(
                    "SELECT workspace_id FROM agent_tasks WHERE id=?1 AND principal_id=?2",
                    params![id, self.owner.id()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            policy(db, self.owner.id(), &workspace.ok_or(Error::NotFound)?)
        })
    }

    pub fn grant(&self, policy: &WorkspacePolicy) -> Result<ExecutionGrant> {
        if policy.status != "active" {
            return Err(Error::Denied("workspace is unavailable".into()));
        }
        ExecutionGrant::new_scoped(
            self.owner.clone(),
            Arc::new(Workspace::reopen_temporary(&policy.identity)?),
            "manual-file-task",
            policy.capabilities.iter().copied(),
            policy.resources.iter().cloned(),
        )
    }

    /// Reopen only the current host policy, then narrow it to the immutable
    /// task manifest. A saved persona cannot acquire its workspace's other grants.
    pub fn grant_for_task(&self, id: &str) -> Result<ExecutionGrant> {
        let task = self.load(id)?;
        self.grant(&self.policy_for_task(id)?)?.delegate_scoped(
            task.grant.persona,
            task.grant.capabilities,
            task.grant.resources,
        )
    }

    pub fn registry(&self) -> Result<ToolRegistry> {
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| Error::Denied("profile closed".into()))?;
        let mut registry = ToolRegistry::default().with_adapter(Arc::new(
            crate::agent_api::domain::DomainTools::new(self.clone(), Arc::downgrade(&app)),
        ))?;
        for adapter in app.agent.adapters()? {
            registry = registry.with_adapter(adapter)?;
        }
        Ok(registry)
    }

    /// Atomically authorize the current exact call, publish its local DB effect
    /// and receipt. An explicit recovery retry cannot publish it a second time.
    pub fn effect(
        &self,
        context: &HostToolContext<'_>,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        self.with_db(|db| {
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
            self.check_context(&tx,context)?;
            let fingerprint=smith_agent_core::arguments_sha256(&context.call.arguments);
            let prior:Option<(String,String,String)>=tx.query_row("SELECT tool_name,arguments_sha256,result_json FROM agent_tool_receipts WHERE task_id=?1 AND call_id=?2",params![context.task_id,context.call.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(sql)?;
            if let Some((name,hash,result))=prior {
                if name!=context.call.name || hash!=fingerprint { return Err(Error::Conflict); }
                return serde_json::from_str(&result).map_err(|_|storage());
            }
            let result=operation(&tx)?;
            let encoded=encode(&result)?;
            if encoded.len()>256*1024 { return Err(Error::LimitExceeded("tool receipt bytes".into())); }
            tx.execute("INSERT INTO agent_tool_receipts(task_id,call_id,tool_name,arguments_sha256,result_json,created_at) VALUES(?1,?2,?3,?4,?5,?6)",params![context.task_id,context.call.id,context.call.name,fingerprint,encoded,crate::now()]).map_err(sql)?;
            tx.commit().map_err(sql)?;
            Ok(result)
        })
    }
    pub(crate) fn check_context(
        &self,
        db: &Connection,
        context: &HostToolContext<'_>,
    ) -> Result<()> {
        let snapshot: String = db
            .query_row(
                "SELECT snapshot FROM agent_tasks WHERE id=?1 AND principal_id=?2",
                params![context.task_id, self.owner.id()],
                |r| r.get(0),
            )
            .map_err(sql)?;
        let task = self.decode_owned(&snapshot)?;
        if task.revision != context.started_revision
            || task.state
                != (smith_agent_core::TaskState::Running {
                    in_flight: Some(context.call.clone()),
                })
            || context.grant.principal() != self.owner()
            || context.grant.manifest().workspace != task.grant.workspace
            || !context
                .grant
                .manifest()
                .resources
                .is_subset(&task.grant.resources)
            || !context
                .grant
                .manifest()
                .capabilities
                .is_subset(&task.grant.capabilities)
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    pub(crate) fn begin_host_operation(
        &self,
        operation: &str,
        request: &serde_json::Value,
    ) -> Result<String> {
        if request.to_string().len() > 64 * 1024 {
            return Err(Error::LimitExceeded("Host operation audit".into()));
        }
        let id = crate::id();
        self.with_db(|db|{let count:i64=db.query_row("SELECT count(*) FROM agent_host_operations WHERE principal_id=?1",[self.owner.id()],|r|r.get(0)).map_err(sql)?;if count>=1024{return Err(Error::LimitExceeded("Profile host-operation audit records; existing audit is retained".into()));}db.execute("INSERT INTO agent_host_operations(id,principal_id,operation,request_json,state,created_at,updated_at) VALUES(?1,?2,?3,?4,'started',?5,?5)",params![id,self.owner.id(),operation,request.to_string(),crate::now()]).map_err(sql)?;Ok(())})?;
        Ok(id)
    }
    pub(crate) fn finish_host_operation(
        &self,
        id: &str,
        state: &str,
        result: &serde_json::Value,
    ) -> Result<()> {
        if !matches!(state, "finished" | "failed" | "uncertain")
            || result.to_string().len() > 256 * 1024
        {
            return Err(Error::InvalidInput("Host audit result".into()));
        }
        self.with_db(|db|{let n=db.execute("UPDATE agent_host_operations SET state=?1,result_json=?2,updated_at=?3 WHERE id=?4 AND principal_id=?5 AND state='started'",params![state,result.to_string(),crate::now(),id,self.owner.id()]).map_err(sql)?;if n!=1{return Err(Error::Conflict);}Ok(())})
    }
    pub(crate) fn host_operations(&self) -> Result<Vec<serde_json::Value>> {
        self.with_db(|db|db.prepare("SELECT id,operation,state,request_json,result_json,created_at,updated_at FROM agent_host_operations WHERE principal_id=?1 ORDER BY created_at DESC LIMIT 50").map_err(sql)?.query_map([self.owner.id()],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"operation":r.get::<_,String>(1)?,"state":r.get::<_,String>(2)?,"request":serde_json::from_str::<serde_json::Value>(&r.get::<_,String>(3)?).unwrap_or_default(),"result":r.get::<_,Option<String>>(4)?.and_then(|v|serde_json::from_str::<serde_json::Value>(&v).ok()),"created_at":r.get::<_,String>(5)?,"updated_at":r.get::<_,String>(6)?}))).map_err(sql)?.collect::<std::result::Result<Vec<_>,_>>().map_err(sql))
    }
    pub(crate) fn recover_host_operations(&self) -> Result<()> {
        self.with_db(|db|{db.execute("UPDATE agent_host_operations SET state='uncertain',updated_at=?1 WHERE principal_id=?2 AND state='started'",params![crate::now(),self.owner.id()]).map_err(sql)?;Ok(())})
    }

    pub fn tasks(&self) -> Result<Vec<Task>> {
        self.with_db(|db| {
            let mut query = db.prepare("SELECT snapshot FROM agent_tasks WHERE principal_id=?1 ORDER BY updated_at DESC,id LIMIT 257").map_err(sql)?;
            let rows = query.query_map([self.owner.id()], |r| r.get::<_,String>(0)).map_err(sql)?;
            let tasks = rows.map(|r| self.decode_owned(&r.map_err(sql)?)).collect::<Result<Vec<_>>>()?;
            if tasks.len() > MAX_TASKS as usize { return Err(Error::LimitExceeded("profile tasks".into())); }
            Ok(tasks)
        })
    }

    pub fn events_page(
        &self,
        id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<TraceRecord>> {
        if !(1..=100).contains(&limit) {
            return Err(Error::InvalidInput("trace limit must be 1–100".into()));
        }
        if after.is_some_and(|revision| revision > i64::MAX as u64) {
            return Err(Error::InvalidInput("trace revision is out of range".into()));
        }
        self.with_db(|db| {
            verify_owner(db, id, self.owner.id())?;
            let mut query = db.prepare("SELECT revision,event FROM agent_task_trace WHERE task_id=?1 AND (?2 IS NULL OR revision>?2) ORDER BY revision LIMIT ?3").map_err(sql)?;
            let rows = query.query_map(params![id,after,limit as i64], |r| Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?))).map_err(sql)?;
            rows.map(|r| {
                let (revision,event) = r.map_err(sql)?;
                Ok(TraceRecord{task_id:id.into(),revision,event:decode_trace_event(&event)?})
            }).collect()
        })
    }

    fn decode_owned(&self, snapshot: &str) -> Result<Task> {
        let task = decode_task_record(snapshot)?;
        if task.grant.principal != self.owner.id() {
            return Err(Error::Denied("task owner mismatch".into()));
        }
        Ok(task)
    }
}

impl TraceStore for AgentStore {
    fn load(&self, id: &str) -> Result<Task> {
        self.with_db(|db| {
            let snapshot: Option<String> = db
                .query_row(
                    "SELECT snapshot FROM agent_tasks WHERE id=?1 AND principal_id=?2",
                    params![id, self.owner.id()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            self.decode_owned(&snapshot.ok_or(Error::NotFound)?)
        })
    }

    fn commit(&self, expected: Option<u64>, task: &Task, event: &TraceEvent) -> Result<()> {
        if task.grant.principal != self.owner.id() {
            return Err(Error::Denied("task owner mismatch".into()));
        }
        let (snapshot, event) = encode_transition(expected, task, event)?;
        self.with_db(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
            let current: Option<(u64,String,String)> = tx.query_row("SELECT revision,workspace_id,snapshot FROM agent_tasks WHERE id=?1 AND principal_id=?2", params![task.id,self.owner.id()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(sql)?;
            if current.as_ref().map(|r| r.0) != expected { return Err(Error::Conflict); }
            let workspace = if let Some((_,workspace,prior)) = current {
                let prior = self.decode_owned(&prior)?;
                if prior.grant != task.grant || prior.agent_persona != task.agent_persona || prior.extensions != task.extensions { return Err(Error::Denied("immutable task authority, persona or extensions changed".into())); }
                workspace
            } else {
                crate::agent_extensions::validate_capture(&tx,self.owner.id(),&task.extensions)?;
                let count: i64 = tx.query_row("SELECT count(*) FROM agent_tasks WHERE principal_id=?1", [self.owner.id()], |r| r.get(0)).map_err(sql)?;
                if count >= MAX_TASKS { return Err(Error::LimitExceeded("profile tasks".into())); }
                let workspace: Option<String> = tx.query_row("SELECT id FROM agent_workspaces WHERE principal_id=?1 AND identity=?2 AND status='active'", params![self.owner.id(),encode(&task.grant.workspace)?], |r| r.get(0)).optional().map_err(sql)?;
                let workspace = workspace.ok_or_else(|| Error::Denied("workspace was not provisioned by this profile".into()))?;
                let policy = policy(&tx,self.owner.id(),&workspace)?;
                if !task.grant.capabilities.is_subset(&policy.capabilities) || !task.grant.resources.is_subset(&policy.resources) { return Err(Error::Denied("task exceeds approved workspace policy".into())); }
                workspace
            };
            tx.execute("INSERT INTO agent_tasks(id,workspace_id,principal_id,revision,snapshot,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?6) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,snapshot=excluded.snapshot,updated_at=excluded.updated_at", params![task.id,workspace,self.owner.id(),task.revision,snapshot,crate::now()]).map_err(sql)?;
            tx.execute("INSERT INTO agent_task_trace(task_id,revision,event) VALUES(?1,?2,?3)", params![task.id,task.revision,event]).map_err(sql)?;
            tx.commit().map_err(sql)
        })
    }

    fn events(&self, id: &str) -> Result<Vec<TraceRecord>> {
        let mut output = vec![];
        let mut after = None;
        loop {
            let page = self.events_page(id, after, 100)?;
            if page.is_empty() {
                break;
            }
            after = page.last().map(|e| e.revision);
            output.extend(page);
            if output.len() > 2048 {
                return Err(Error::LimitExceeded("task trace events".into()));
            }
        }
        Ok(output)
    }
}

type PolicyRow = (String, String, String, String, u64, String, String);
fn policy_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PolicyRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}
fn decode_policy(row: PolicyRow) -> Result<WorkspacePolicy> {
    let capabilities: BTreeSet<Capability> = decode(&row.3)?;
    if capabilities.iter().any(|c| !supported_capability(*c)) {
        return Err(Error::Denied(
            "unsupported stored workspace capability".into(),
        ));
    }
    Ok(WorkspacePolicy {
        id: row.0,
        label: row.1,
        identity: decode(&row.2)?,
        capabilities,
        resources: decode(&row.6)?,
        revision: row.4,
        status: row.5,
    })
}
pub(crate) fn policy(db: &Connection, owner: &str, id: &str) -> Result<WorkspacePolicy> {
    let row = db.query_row("SELECT id,label,identity,capabilities,policy_revision,status,resources FROM agent_workspaces WHERE id=?1 AND principal_id=?2",params![id,owner],policy_row).optional().map_err(sql)?;
    decode_policy(row.ok_or(Error::NotFound)?)
}
fn verify_owner(db: &Connection, id: &str, owner: &str) -> Result<()> {
    let found: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_tasks WHERE id=?1 AND principal_id=?2)",
            params![id, owner],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if found {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}
fn supported_capability(c: Capability) -> bool {
    matches!(
        c,
        Capability::ReadWorkspace
            | Capability::ListWorkspace
            | Capability::WriteWorkspace
            | Capability::SearchWorkspace
            | Capability::EditWorkspace
            | Capability::TaskControl
            | Capability::ReadConversations
            | Capability::ExportConversations
            | Capability::SaveArtifact
            | Capability::ReadMemory
            | Capability::WriteMemory
            | Capability::RenderChart
            | Capability::ReadStatus
            | Capability::ReadDocument
            | Capability::WriteDocument
            | Capability::EmbedText
            | Capability::GenerateImage
            | Capability::GitRead
            | Capability::GitWrite
            | Capability::WebFetch
            | Capability::SendWebhook
            | Capability::SystemMonitor
            | Capability::Shell
            | Capability::ComputerUse
            | Capability::ModelInference
    )
}
fn encode<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|_| storage())
}
fn decode<T: serde::de::DeserializeOwned>(v: &str) -> Result<T> {
    if v.len() > 64 * 1024 {
        return Err(Error::LimitExceeded("workspace policy bytes".into()));
    }
    serde_json::from_str(v).map_err(|_| storage())
}
fn sql(_: rusqlite::Error) -> Error {
    storage()
}
fn storage() -> Error {
    Error::Persistence("Encrypted task storage is unavailable; existing data was retained.".into())
}
