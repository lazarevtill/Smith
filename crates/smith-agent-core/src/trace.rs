use crate::{Error, Result, Task, ToolCall};
#[cfg(feature = "standalone-sqlite")]
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "standalone-sqlite")]
use std::path::Path;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    ModelStarted {
        step_id: String,
        scope_id: String,
        binding_id: String,
        context: Value,
        provenance: Value,
    },
    ModelFinished {
        step_id: String,
        reply: crate::ModelReply,
    },
    ModelFailed {
        step_id: String,
        error: String,
    },
    ModelRejected {
        step_id: String,
        reply: crate::RejectedModelReply,
    },
    Created,
    Started,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    ResumedWithoutReplay,
    ToolRejected {
        name: String,
        error: Error,
    },
    ApprovalRequested {
        call: ToolCall,
    },
    ApprovalDenied {
        call_id: String,
    },
    ToolStarted {
        call: ToolCall,
        approved: bool,
    },
    ToolFinished {
        call_id: String,
        result: Result<Value>,
    },
    PlanUpdated {
        steps: Vec<String>,
    },
    ClarificationRequested {
        question: String,
    },
    ClarificationAnswered {
        answer: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRecord {
    pub task_id: String,
    pub revision: u64,
    pub event: TraceEvent,
}

/// Trusted persistence boundary. Implementations MUST atomically append the
/// event and replace the snapshot only if the revision matches. On error no
/// partial commit may be visible. Traces are data, never execution grants.
pub trait TraceStore: Send + Sync {
    fn load(&self, task_id: &str) -> Result<Task>;
    fn commit(&self, expected_revision: Option<u64>, task: &Task, event: &TraceEvent)
        -> Result<()>;
    fn events(&self, task_id: &str) -> Result<Vec<TraceRecord>>;
}

type MemoryRows = BTreeMap<String, (Task, Vec<TraceRecord>)>;
/// Deterministic test adapter; intentionally not durable across process exit.
#[derive(Clone, Default)]
pub struct MemoryTraceStore {
    rows: Arc<Mutex<MemoryRows>>,
}
impl TraceStore for MemoryTraceStore {
    fn load(&self, id: &str) -> Result<Task> {
        self.rows
            .lock()
            .map_err(poisoned)?
            .get(id)
            .map(|(task, _)| task.clone())
            .ok_or(Error::NotFound)
    }
    fn commit(&self, expected: Option<u64>, task: &Task, event: &TraceEvent) -> Result<()> {
        encode_transition(expected, task, event)?;
        let mut rows = self.rows.lock().map_err(poisoned)?;
        let current = rows.get(&task.id).map(|(t, _)| t.revision);
        if current != expected {
            return Err(Error::Conflict);
        }
        let record = TraceRecord {
            task_id: task.id.clone(),
            revision: task.revision,
            event: event.clone(),
        };
        match rows.get_mut(&task.id) {
            Some((snapshot, events)) => {
                *snapshot = task.clone();
                events.push(record);
            }
            None => {
                rows.insert(task.id.clone(), (task.clone(), vec![record]));
            }
        }
        Ok(())
    }
    fn events(&self, id: &str) -> Result<Vec<TraceRecord>> {
        self.rows
            .lock()
            .map_err(poisoned)?
            .get(id)
            .map(|(_, events)| events.clone())
            .ok_or(Error::NotFound)
    }
}

/// Durable application-owned store. `path` is chosen by trusted host code,
/// outside the tool workspace, and is never accepted from a tool argument.
#[cfg(feature = "standalone-sqlite")]
pub struct SqliteTraceStore {
    connection: Mutex<Connection>,
}
#[cfg(feature = "standalone-sqlite")]
impl SqliteTraceStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS agent_core_tasks (id TEXT PRIMARY KEY, revision INTEGER NOT NULL, snapshot TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS agent_core_trace (task_id TEXT NOT NULL REFERENCES agent_core_tasks(id), revision INTEGER NOT NULL, event TEXT NOT NULL, PRIMARY KEY(task_id, revision));")?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
}
#[cfg(feature = "standalone-sqlite")]
impl TraceStore for SqliteTraceStore {
    fn load(&self, id: &str) -> Result<Task> {
        let connection = self.connection.lock().map_err(poisoned)?;
        let snapshot: Option<String> = connection
            .query_row(
                "SELECT snapshot FROM agent_core_tasks WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        decode_task_record(&snapshot.ok_or(Error::NotFound)?)
    }
    fn commit(&self, expected: Option<u64>, task: &Task, event: &TraceEvent) -> Result<()> {
        let (snapshot, encoded_event) = encode_transition(expected, task, event)?;
        let mut connection = self.connection.lock().map_err(poisoned)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<u64> = tx
            .query_row(
                "SELECT revision FROM agent_core_tasks WHERE id=?1",
                [&task.id],
                |row| row.get(0),
            )
            .optional()?;
        if current != expected {
            return Err(Error::Conflict);
        }
        tx.execute("INSERT INTO agent_core_tasks (id,revision,snapshot) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,snapshot=excluded.snapshot", params![task.id, task.revision, snapshot])?;
        tx.execute(
            "INSERT INTO agent_core_trace (task_id,revision,event) VALUES (?1,?2,?3)",
            params![task.id, task.revision, encoded_event],
        )?;
        tx.commit()?;
        Ok(())
    }
    fn events(&self, id: &str) -> Result<Vec<TraceRecord>> {
        let connection = self.connection.lock().map_err(poisoned)?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_core_tasks WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(Error::NotFound);
        }
        let mut statement = connection.prepare(
            "SELECT revision,event FROM agent_core_trace WHERE task_id=?1 ORDER BY revision",
        )?;
        let rows = statement.query_map([id], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (revision, event) = row?;
            Ok(TraceRecord {
                task_id: id.into(),
                revision,
                event: decode_trace_event(&event)?,
            })
        })
        .collect()
    }
}

/// Encode a bounded snapshot/event pair for a trusted atomic store adapter.
/// The adapter must still enforce the expected revision against durable state.
pub fn encode_transition(
    expected: Option<u64>,
    task: &Task,
    event: &TraceEvent,
) -> Result<(String, String)> {
    task.extensions.validate()?;
    if let Some(rejection) = &task.last_model_rejection {
        rejection.validate()?;
    }
    validate_event(event)?;
    let wanted = expected
        .map_or(Some(0), |r| r.checked_add(1))
        .ok_or(Error::Conflict)?;
    if task.revision != wanted {
        return Err(Error::Conflict);
    }
    let snapshot = encode(task)?;
    let event = encode(event)?;
    Ok((snapshot, event))
}
pub fn decode_task_record(value: &str) -> Result<Task> {
    let task: Task = decode(value)?;
    task.extensions.validate()?;
    if let Some(persona) = &task.agent_persona {
        persona.validate()?;
    }
    if let Some(rejection) = &task.last_model_rejection {
        rejection.validate()?;
    }
    Ok(task)
}
pub fn decode_trace_event(value: &str) -> Result<TraceEvent> {
    let event = decode(value)?;
    validate_event(&event)?;
    Ok(event)
}
fn validate_event(event: &TraceEvent) -> Result<()> {
    if let TraceEvent::ModelRejected { step_id, reply } = event {
        crate::ModelRejectionRecord {
            step_id: step_id.clone(),
            reply: reply.clone(),
        }
        .validate()?;
    }
    Ok(())
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_string(value).map_err(|e| Error::Persistence(e.to_string()))?;
    if value.len() > 16 * 1024 * 1024 {
        return Err(Error::LimitExceeded("trace record bytes".into()));
    }
    Ok(value)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    if value.len() > 16 * 1024 * 1024 {
        return Err(Error::LimitExceeded("stored record bytes".into()));
    }
    serde_json::from_str(value).map_err(|e| Error::Persistence(e.to_string()))
}
fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::Persistence("trace store lock poisoned".into())
}
