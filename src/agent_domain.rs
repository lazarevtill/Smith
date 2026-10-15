//! App-backed tools over the already-keyed profile connection. No inference,
//! network, host filesystem paths or authority inferred from tool arguments.
use crate::{agent_store::AgentStore, App};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use smith_agent_core::{
    Availability, Capability, Error, ExecutionGrant, HostToolAdapter, HostToolContext,
    ResourceKind, Result, ToolCall, ToolDescriptor,
};
use std::sync::Weak;

pub struct DomainTools {
    store: AgentStore,
    app: Weak<App>,
}
impl DomainTools {
    pub fn new(store: AgentStore, app: Weak<App>) -> Self {
        Self { store, app }
    }
}
fn sql(_: rusqlite::Error) -> Error {
    Error::Persistence("Encrypted domain storage is unavailable".into())
}
fn parse<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T> {
    serde_json::from_value(v.clone())
        .map_err(|_| Error::InvalidInput("Unexpected fields or invalid tool arguments".into()))
}
fn text(value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max {
        Err(Error::InvalidInput(format!(
            "Expected nonempty text of at most {max} bytes"
        )))
    } else {
        Ok(())
    }
}
fn scope(grant: &ExecutionGrant, kind: ResourceKind, id: &str) -> Result<()> {
    if grant.permits_resource(kind, id) {
        Ok(())
    } else {
        Err(Error::Denied(
            "The exact resource was not selected in this task's grant".into(),
        ))
    }
}
fn descriptor(
    name: &str,
    cap: Capability,
    approval: bool,
    properties: Value,
    required: &[&str],
) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        source: "host:encrypted-profile-domain-v1".into(),
        parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        availability: Availability::Implemented,
        required_capability: Some(cap),
        requires_approval: approval,
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    max_results: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    max_results: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    session_id: String,
    max_messages: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    session_id: String,
    format: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    title: String,
    content: String,
    chat_id: Option<String>,
    version: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recall {
    namespace: String,
    query: String,
    max_results: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Memory {
    namespace: String,
    fact: String,
    #[serde(default)]
    tags: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chart {
    kind: Option<String>,
    title: Option<String>,
    labels: Option<Vec<String>>,
    values: Vec<f64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadDocument {
    media_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteDocument {
    name: String,
    format: crate::media_api::OutputFormat,
    table: crate::media_api::TableInput,
    chat_id: Option<String>,
    version: Option<i64>,
}
fn media_error(error: crate::media_api::MediaError) -> Error {
    let error: crate::ApiError = error.into();
    match error.0 {
        axum::http::StatusCode::NOT_FOUND => Error::NotFound,
        axum::http::StatusCode::CONFLICT => Error::Conflict,
        axum::http::StatusCode::PAYLOAD_TOO_LARGE => Error::LimitExceeded(error.1),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR => {
            Error::Persistence("Encrypted document operation failed".into())
        }
        _ => Error::InvalidInput(error.1),
    }
}
fn limit(value: Option<usize>, default: usize, max: usize) -> Result<usize> {
    let n = value.unwrap_or(default);
    if n == 0 || n > max {
        Err(Error::LimitExceeded("Tool result count".into()))
    } else {
        Ok(n)
    }
}
fn conversation_scopes(grant: &ExecutionGrant) -> Result<Vec<&str>> {
    let ids = grant
        .resources()
        .iter()
        .filter(|r| r.kind == ResourceKind::Conversation)
        .map(|r| r.id.as_str())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err(Error::Denied("No conversations were selected".into()));
    }
    Ok(ids)
}
impl HostToolAdapter for DomainTools {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            descriptor(
                "read_document",
                Capability::ReadDocument,
                false,
                json!({"media_id":{"type":"string","format":"uuid"}}),
                &["media_id"],
            ),
            descriptor(
                "write_document",
                Capability::WriteDocument,
                true,
                json!({"name":{"type":"string"},"format":{"type":"string","enum":["csv","markdown","xlsx","pdf","pptx"]},"table":{"type":"object","properties":{"headers":{"type":"array","items":{"type":"string"}},"rows":{"type":"array","items":{"type":"array"}}},"required":["headers","rows"],"additionalProperties":false},"chat_id":{"type":"string"},"version":{"type":"integer"}}),
                &["name", "format", "table"],
            ),
            descriptor(
                "smith_status",
                Capability::ReadStatus,
                false,
                json!({}),
                &[],
            ),
            descriptor(
                "list_conversations",
                Capability::ReadConversations,
                false,
                json!({"max_results":{"type":"integer","minimum":1,"maximum":100}}),
                &[],
            ),
            descriptor(
                "search_conversations",
                Capability::ReadConversations,
                false,
                json!({"query":{"type":"string","maxLength":1024},"max_results":{"type":"integer","minimum":1,"maximum":100}}),
                &["query"],
            ),
            descriptor(
                "read_conversation",
                Capability::ReadConversations,
                false,
                json!({"session_id":{"type":"string"},"max_messages":{"type":"integer","minimum":1,"maximum":100}}),
                &["session_id"],
            ),
            descriptor(
                "export_conversation",
                Capability::ExportConversations,
                false,
                json!({"session_id":{"type":"string"},"format":{"type":"string","enum":["markdown","json"]}}),
                &["session_id"],
            ),
            descriptor(
                "save_artifact",
                Capability::SaveArtifact,
                true,
                json!({"title":{"type":"string","maxLength":120},"content":{"type":"string","maxLength":65536},"chat_id":{"type":"string"},"version":{"type":"integer","minimum":0}}),
                &["title", "content"],
            ),
            descriptor(
                "recall_memory",
                Capability::ReadMemory,
                false,
                json!({"namespace":{"type":"string"},"query":{"type":"string","maxLength":1024},"max_results":{"type":"integer","minimum":1,"maximum":50}}),
                &["namespace", "query"],
            ),
            descriptor(
                "save_memory",
                Capability::WriteMemory,
                true,
                json!({"namespace":{"type":"string"},"fact":{"type":"string","maxLength":8192},"tags":{"type":"array","items":{"type":"string"},"maxItems":16}}),
                &["namespace", "fact"],
            ),
            descriptor(
                "render_chart",
                Capability::RenderChart,
                false,
                json!({"kind":{"type":"string","enum":["bar","line"]},"title":{"type":"string","maxLength":256},"labels":{"type":"array","items":{"type":"string"},"maxItems":64},"values":{"type":"array","items":{"type":"number"},"minItems":1,"maxItems":64}}),
                &["values"],
            ),
        ]
    }
    fn validate(&self, grant: &ExecutionGrant, call: &ToolCall) -> Result<()> {
        match call.name.as_str() {
            "read_document" => {
                let a: ReadDocument = parse(&call.arguments)?;
                scope(grant, ResourceKind::Document, &a.media_id)?;
            }
            "write_document" => {
                let a: WriteDocument = parse(&call.arguments)?;
                text(&a.name, 128)?;
                if a.table.headers.is_empty()
                    || a.table.headers.len() > 50
                    || a.table.rows.len() > 500
                    || a.table
                        .rows
                        .iter()
                        .any(|r| r.len() != a.table.headers.len())
                {
                    return Err(Error::InvalidInput("Document table dimensions".into()));
                }
                match (&a.chat_id, a.version) {
                    (Some(id), Some(v)) if v >= 0 => scope(grant, ResourceKind::Conversation, id)?,
                    (None, None) => scope(grant, ResourceKind::ArtifactCollection, "unowned")?,
                    _ => {
                        return Err(Error::InvalidInput(
                            "Linked documents require chat_id and version".into(),
                        ))
                    }
                }
            }
            "smith_status" => {
                parse::<Empty>(&call.arguments)?;
            }
            "list_conversations" => {
                limit(parse::<List>(&call.arguments)?.max_results, 20, 100)?;
                conversation_scopes(grant)?;
            }
            "search_conversations" => {
                let a: Search = parse(&call.arguments)?;
                text(&a.query, 1024)?;
                limit(a.max_results, 10, 100)?;
                conversation_scopes(grant)?;
            }
            "read_conversation" => {
                let a: Read = parse(&call.arguments)?;
                scope(grant, ResourceKind::Conversation, &a.session_id)?;
                limit(a.max_messages, 50, 100)?;
            }
            "export_conversation" => {
                let a: Export = parse(&call.arguments)?;
                scope(grant, ResourceKind::Conversation, &a.session_id)?;
                if !matches!(a.format.as_deref(), None | Some("json" | "markdown")) {
                    return Err(Error::InvalidInput("Export format".into()));
                }
            }
            "save_artifact" => {
                let a: Artifact = parse(&call.arguments)?;
                text(&a.title, 120)?;
                if a.title.chars().any(char::is_control) || a.content.len() > 65536 {
                    return Err(Error::InvalidInput(
                        "Artifact title or content bound".into(),
                    ));
                }
                match (&a.chat_id, a.version) {
                    (Some(id), Some(v)) if v >= 0 => scope(grant, ResourceKind::Conversation, id)?,
                    (None, None) => scope(grant, ResourceKind::ArtifactCollection, "unowned")?,
                    _ => {
                        return Err(Error::InvalidInput(
                            "Linked artifacts require exact chat_id and version".into(),
                        ))
                    }
                }
            }
            "recall_memory" => {
                let a: Recall = parse(&call.arguments)?;
                scope(grant, ResourceKind::MemoryNamespace, &a.namespace)?;
                text(&a.query, 1024)?;
                limit(a.max_results, 20, 50)?;
            }
            "save_memory" => {
                let a: Memory = parse(&call.arguments)?;
                scope(grant, ResourceKind::MemoryNamespace, &a.namespace)?;
                text(&a.fact, 8192)?;
                if a.tags.len() > 16
                    || a.tags
                        .iter()
                        .any(|t| t.is_empty() || t.len() > 64 || t.chars().any(char::is_control))
                {
                    return Err(Error::InvalidInput("Memory tags".into()));
                }
            }
            "render_chart" => {
                let a: Chart = parse(&call.arguments)?;
                if !matches!(a.kind.as_deref(), None | Some("bar" | "line"))
                    || a.values.is_empty()
                    || a.values.len() > 64
                    || a.values.iter().any(|v| !v.is_finite() || v.abs() > 1e12)
                    || a.title.as_ref().is_some_and(|s| s.len() > 256)
                    || a.labels.as_ref().is_some_and(|labels| {
                        labels.len() != a.values.len() || labels.iter().any(|s| s.len() > 128)
                    })
                {
                    return Err(Error::InvalidInput(
                        "Chart requires 1–64 finite values, matching labels, and bounded text"
                            .into(),
                    ));
                }
            }
            _ => return Err(Error::Unsupported("Domain tool not registered".into())),
        }
        Ok(())
    }
    fn execute(
        &self,
        context: &HostToolContext<'_>,
        check_active: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        check_active()?;
        self.validate(context.grant, context.call)?;
        match context.call.name.as_str() {
            "read_document" => {
                let a: ReadDocument = parse(&context.call.arguments)?;
                self.store.with_db(|db| {
                    self.store.check_context(db, context)?;
                    crate::media_api::host_read_document(db, &a.media_id).map_err(media_error)
                })
            }
            "write_document" => {
                let a: WriteDocument = parse(&context.call.arguments)?;
                let (bytes, manifest) =
                    crate::media_api::host_prepare_document(a.name, a.format, a.table)
                        .map_err(media_error)?;
                check_active()?;
                self.store.effect(context, |tx| {
                    crate::media_api::save_generated_in_transaction(
                        tx,
                        &context.call.id,
                        a.chat_id.as_deref(),
                        a.version,
                        bytes,
                        manifest,
                    )
                    .map_err(media_error)
                })
            }
            "smith_status" => {
                let app = self
                    .app
                    .upgrade()
                    .ok_or_else(|| Error::Denied("Profile closed".into()))?;
                let config = app
                    .config
                    .lock()
                    .map_err(|_| sql(rusqlite::Error::InvalidQuery))?
                    .clone();
                // Endpoint/user credentials and credential-presence flags are not exposed.
                Ok(
                    json!({"default_model":config.model,"provider":config.provider,"storage":"encrypted_sqlcipher","binding":"loopback","automatic_chat_tools":false,"authority":"explicit_task_grants"}),
                )
            }
            "list_conversations" => {
                let a: List = parse(&context.call.arguments)?;
                self.store.with_db(|db|{self.store.check_context(db,context)?;let mut rows=conversation_list(db,context.grant)?;let max=limit(a.max_results,20,100)?;let truncated=rows.len()>max;rows.truncate(max);Ok(json!({"conversations":rows,"truncated":truncated,"scope":"selected_conversations"}))})
            }
            "search_conversations" => {
                let a: Search = parse(&context.call.arguments)?;
                self.store.with_db(|db| {
                    self.store.check_context(db, context)?;
                    search(db, context.grant, &a)
                })
            }
            "read_conversation" => {
                let a: Read = parse(&context.call.arguments)?;
                self.store.with_db(|db| {
                    self.store.check_context(db, context)?;
                    read(db, &a.session_id, limit(a.max_messages, 50, 100)?, false)
                })
            }
            "export_conversation" => {
                let a: Export = parse(&context.call.arguments)?;
                self.store.with_db(|db|{self.store.check_context(db,context)?;let transcript=read(db,&a.session_id,200,true)?;let format=a.format.as_deref().unwrap_or("markdown");let content=if format=="json"{serde_json::to_string_pretty(&transcript).map_err(|_|Error::InvalidInput("Export encoding".into()))?}else{let mut out=format!("# {}\n\n",transcript["title"].as_str().unwrap_or("Conversation"));for message in transcript["messages"].as_array().unwrap(){out.push_str(&format!("## {} [{}]\n\n{}\n\n",message["role"].as_str().unwrap(),message["id"].as_str().unwrap(),message["content"].as_str().unwrap()));}out};if content.len()>128*1024{return Err(Error::LimitExceeded("Export bytes".into()));}Ok(json!({"session_id":a.session_id,"format":format,"content":content,"messages":transcript["messages"].as_array().unwrap().len(),"scope":"all_message_branches","attachments":"not_included"}))})
            }
            "save_artifact" => {
                let a: Artifact = parse(&context.call.arguments)?;
                self.store.effect(context,|tx|{
                if let Some(chat)=&a.chat_id { crate::chat_management::check_version(tx,chat,a.version.unwrap()).map_err(|_|Error::Conflict)?;if streaming(tx,chat)?{return Err(Error::Conflict);} }
                let id=crate::id();tx.execute("INSERT INTO artifacts(id,chat_id,name,content,created_at) VALUES(?1,?2,?3,?4,?5)",params![id,a.chat_id,a.title.trim(),a.content,crate::now()]).map_err(sql)?;
                if let Some(chat)=&a.chat_id{crate::chat_management::touch(tx,chat,crate::now(),true).map_err(sql)?;}else{crate::action_history::invalidate_redo(tx).map_err(sql)?;}
                Ok(json!({"ok":true,"artifact_id":id,"title":a.title.trim(),"content_bytes":a.content.len()}))
            })
            }
            "save_memory" => {
                let a: Memory = parse(&context.call.arguments)?;
                self.store.effect(context,|tx|{
                let count:i64=tx.query_row("SELECT count(*) FROM agent_memories WHERE principal_id=?1",[context.grant.principal().id()],|r|r.get(0)).map_err(sql)?;if count>=10000{return Err(Error::LimitExceeded("Profile memory records".into()));}
                let id=crate::id();tx.execute("INSERT INTO agent_memories(id,principal_id,namespace,content,tags_json,created_at,updated_at,version) VALUES(?1,?2,?3,?4,?5,?6,?6,1)",params![id,context.grant.principal().id(),a.namespace,a.fact,serde_json::to_string(&a.tags).unwrap(),crate::now()]).map_err(sql)?;crate::action_history::invalidate_redo(tx).map_err(sql)?;
                Ok(json!({"saved":true,"memory_id":id,"namespace":a.namespace,"version":1}))
            })
            }
            "recall_memory" => {
                let a: Recall = parse(&context.call.arguments)?;
                self.store.with_db(|db|{
                self.store.check_context(db,context)?;let max=limit(a.max_results,20,50)?;let mut statement=db.prepare("SELECT m.id,substr(m.content,1,8193),substr(m.tags_json,1,4097),m.version FROM agent_memories m LEFT JOIN memory_metadata s ON s.memory_id=m.id WHERE m.principal_id=?1 AND m.namespace=?2 AND COALESCE(s.deleted,0)=0 AND instr(lower(m.content),lower(?3))>0 ORDER BY COALESCE(s.pinned,0) DESC,m.updated_at DESC,m.id LIMIT ?4").map_err(sql)?;
                let rows=statement.query_map(params![context.grant.principal().id(),a.namespace,a.query,(max+1)as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?))).map_err(sql)?.collect::<std::result::Result<Vec<_>,_>>().map_err(sql)?;
                let truncated=rows.len()>max;let mut budget=0;let mut memories=vec![];for (id,fact,tags,version) in rows.into_iter().take(max){if fact.len()>8192||tags.len()>4096{return Err(Error::LimitExceeded("Memory record exceeds bounded read size".into()));}budget+=fact.len()+tags.len();if budget>128*1024{return Err(Error::LimitExceeded("Memory result bytes; request fewer results".into()));}memories.push(json!({"id":id,"fact":fact,"tags":serde_json::from_str::<Value>(&tags).map_err(|_|sql(rusqlite::Error::InvalidQuery))?,"version":version}));}Ok(json!({"memories":memories,"namespace":a.namespace,"matching":"literal_ascii_case_insensitive","truncated":truncated}))
            })
            }
            "render_chart" => render_chart(parse(&context.call.arguments)?),
            _ => Err(Error::Unsupported("Domain tool not registered".into())),
        }
    }
}
fn active(db: &Connection, id: &str) -> Result<String> {
    db.query_row("SELECT substr(title,1,8193) FROM chats WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM chat_trash WHERE chat_id=?1)",[id],|r|r.get(0)).optional().map_err(sql)?.ok_or(Error::NotFound)
}
fn streaming(db: &Connection, id: &str) -> Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE chat_id=?1 AND status='streaming')",
        [id],
        |r| r.get(0),
    )
    .map_err(sql)
}
fn conversation_list(db: &Connection, grant: &ExecutionGrant) -> Result<Vec<Value>> {
    let mut result = vec![];
    for id in conversation_scopes(grant)? {
        let row:Option<(String,i64,i64)>=db.query_row("SELECT substr(title,1,256),version,updated_at FROM chats WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM chat_trash WHERE chat_id=?1)",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(sql)?;
        if let Some((title, version, updated)) = row {
            result.push(json!({"id":id,"title":title,"version":version,"updated_at":updated}));
        }
    }
    result.sort_by(|a, b| {
        b["updated_at"]
            .as_i64()
            .cmp(&a["updated_at"].as_i64())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    Ok(result)
}
fn search(db: &Connection, grant: &ExecutionGrant, args: &Search) -> Result<Value> {
    let max = limit(args.max_results, 10, 100)?;
    let query = args.query.to_lowercase();
    let mut matches = vec![];
    let mut scanned = 0;
    let mut bytes = 0;
    let mut truncated = false;
    'chats: for chat in conversation_list(db, grant)? {
        let mut statement=db.prepare("SELECT id,substr(content,1,16384),length(CAST(content AS BLOB)) FROM messages WHERE chat_id=?1 ORDER BY rowid LIMIT 1001").map_err(sql)?;
        let mut rows = statement
            .query([chat["id"].as_str().unwrap()])
            .map_err(sql)?;
        while let Some(row) = rows.next().map_err(sql)? {
            scanned += 1;
            if scanned > 1000 || bytes >= 4 * 1024 * 1024 {
                truncated = true;
                break 'chats;
            }
            let content: String = row.get(1).map_err(sql)?;
            let full: i64 = row.get(2).map_err(sql)?;
            if bytes + content.len() > 4 * 1024 * 1024 {
                truncated = true;
                break 'chats;
            }
            bytes += content.len();
            truncated |= full as usize > content.len();
            if content.to_lowercase().contains(&query) {
                matches.push(json!({"session_id":chat["id"],"title":chat["title"],"message_id":row.get::<_,String>(0).map_err(sql)?,"snippet":content.chars().take(200).collect::<String>()}));
                if matches.len() >= max {
                    truncated = true;
                    break 'chats;
                }
            }
        }
    }
    Ok(
        json!({"matches":matches,"scanned_messages":scanned.min(1000),"bytes_read":bytes,"truncated":truncated,"matching":"literal_unicode_lowercase"}),
    )
}
fn read(db: &Connection, id: &str, max: usize, complete: bool) -> Result<Value> {
    let title = active(db, id)?;
    if title.len() > 8192 {
        return Err(Error::LimitExceeded("Conversation title".into()));
    }
    let mut statement=db.prepare("SELECT m.id,m.role,substr(m.content,1,32768),length(CAST(m.content AS BLOB)),m.status,b.parent_id FROM messages m LEFT JOIN message_branches b ON b.message_id=m.id WHERE m.chat_id=?1 ORDER BY m.rowid LIMIT ?2").map_err(sql)?;
    let mut rows = statement
        .query(params![id, (max + 1) as i64])
        .map_err(sql)?;
    let mut messages = vec![];
    let mut bytes = title.len();
    let mut truncated = false;
    while let Some(row) = rows.next().map_err(sql)? {
        if messages.len() == max {
            truncated = true;
            break;
        }
        let content: String = row.get(2).map_err(sql)?;
        let full: i64 = row.get(3).map_err(sql)?;
        if bytes + content.len() > 128 * 1024 {
            truncated = true;
            break;
        }
        bytes += content.len();
        truncated |= full as usize > content.len();
        messages.push(json!({"id":row.get::<_,String>(0).map_err(sql)?,"role":row.get::<_,String>(1).map_err(sql)?,"content":content,"status":row.get::<_,String>(4).map_err(sql)?,"parent_id":row.get::<_,Option<String>>(5).map_err(sql)?}));
    }
    if complete && truncated {
        return Err(Error::LimitExceeded(
            "Complete export exceeds 200 messages or 128 KiB; no partial export was returned"
                .into(),
        ));
    }
    Ok(
        json!({"session_id":id,"title":title,"messages":messages,"truncated":truncated,"scope":"all_message_branches","attachments":"not_included"}),
    )
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn render_chart(chart: Chart) -> Result<Value> {
    use std::fmt::Write;
    let kind = chart.kind.as_deref().unwrap_or("bar");
    let title = chart.title.unwrap_or_default();
    let n = chart.values.len();
    let labels = chart
        .labels
        .unwrap_or_else(|| (1..=n).map(|i| i.to_string()).collect());
    let low = chart.values.iter().copied().fold(0.0, f64::min);
    let high = chart.values.iter().copied().fold(0.0, f64::max);
    let range = (high - low).max(1.0);
    let y = |v: f64| 48.0 + (high - v) / range * 290.0;
    let zero = y(0.0);
    let step = 640.0 / n as f64;
    let mut svg=format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 760 420\" role=\"img\"><title>{}</title><rect width=\"760\" height=\"420\" fill=\"white\"/><text x=\"60\" y=\"28\" fill=\"#222\" font-family=\"sans-serif\" font-size=\"18\">{}</text><path d=\"M60 48 V338 H700 M60 {:.2} H700\" fill=\"none\" stroke=\"#777\"/>",escape(&title),escape(&title),zero);
    let mut points = String::new();
    for (i, value) in chart.values.iter().enumerate() {
        let x = 60.0 + step * (i as f64 + 0.5);
        let py = y(*value);
        if kind == "bar" {
            write!(svg,"<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"#2455a4\"><title>{}: {}</title></rect>",x-step*0.35,py.min(zero),step*0.7,(py-zero).abs(),escape(&labels[i]),value).unwrap();
        } else {
            write!(points, "{x:.2},{py:.2} ").unwrap();
        }
        write!(svg,"<text x=\"{x:.2}\" y=\"360\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#222\">{}</text>",escape(&labels[i])).unwrap();
    }
    if kind == "line" {
        write!(
            svg,
            "<polyline points=\"{points}\" fill=\"none\" stroke=\"#2455a4\" stroke-width=\"2\"/>"
        )
        .unwrap();
    }
    svg.push_str("</svg>");
    Ok(
        json!({"kind":kind,"svg":svg,"values":chart.values,"labels":labels,"media_type":"image/svg+xml"}),
    )
}
