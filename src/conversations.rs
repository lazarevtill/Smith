//! Bounded text branch graph, based on original smith-ui/src/branches.rs and
//! smith-storage/src/chat.rs. Immutable messages; selection follows the newest
//! descendant of a chosen sibling. This is not a legacy SQLCipher importer.
use crate::{bad, chat_management, ApiError, ApiResult, StatusCode};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub name: String,
    pub content: String,
    #[serde(default = "text_mime")]
    pub media_type: String,
}
fn text_mime() -> String {
    "text/plain".into()
}
pub fn validate_attachments(files: &[Attachment]) -> ApiResult<()> {
    if files.len() > 3 || files.iter().map(|f| f.content.len()).sum::<usize>() > 100_000 {
        return Err(bad(
            "Attach up to 3 UTF-8 text files, totaling at most 100,000 bytes.",
        ));
    }
    for f in files {
        let suffix = f
            .name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase());
        if suffix.as_deref().is_some_and(|ext| {
            !matches!(
                ext,
                "txt"
                    | "md"
                    | "csv"
                    | "json"
                    | "log"
                    | "rs"
                    | "py"
                    | "js"
                    | "ts"
                    | "html"
                    | "css"
                    | "xml"
                    | "yaml"
                    | "yml"
                    | "toml"
            )
        }) {
            return Err(bad("Choose a plain-text file such as .txt, .md, .csv or source code. Images, PDF and office documents are not supported here."));
        }
        if f.name.trim().is_empty()
            || f.name.chars().count() > 120
            || f.name.chars().any(char::is_control)
            || f.name.contains(['/', '\\'])
            || f.media_type != "text/plain"
            || f.content
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err(bad("Attachments must be plain UTF-8 text with a simple filename; binary, image and document formats are not supported."));
        }
    }
    Ok(())
}
// Old pilot retries were consecutive assistants. Link them to the same prompt,
// while ordinary turns retain the exact order and the newest attempt is active.
pub fn backfill(db: &Connection, chat: &str) -> rusqlite::Result<()> {
    let mut q = db.prepare("SELECT id,role FROM messages WHERE chat_id=?1 ORDER BY rowid")?;
    let rows = q
        .query_map([chat], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut previous: Option<String> = None;
    let mut user: Option<String> = None;
    for (id, role) in rows {
        let parent = if role == "assistant" {
            user.as_ref().or(previous.as_ref())
        } else {
            previous.as_ref()
        };
        db.execute(
            "INSERT OR IGNORE INTO message_branches(message_id,parent_id) VALUES(?1,?2)",
            params![id, parent],
        )?;
        if role == "user" {
            user = Some(id.clone());
        }
        previous = Some(id);
    }
    db.execute(
        "INSERT OR IGNORE INTO conversation_heads(chat_id,leaf_id) VALUES(?1,?2)",
        params![chat, previous],
    )?;
    Ok(())
}
pub fn all(db: &Connection, chat: &str) -> ApiResult<Vec<Value>> {
    let mut q=db.prepare("SELECT m.id,m.role,m.content,m.status,m.error,m.created_at,m.run_id,c.provider,c.endpoint,c.model,c.warning,c.finish_reason,c.transport_incomplete,b.parent_id,b.message_id,COALESCE(f.kind,''),COALESCE(f.revision,0),d.reasoning FROM messages m LEFT JOIN run_configs c ON m.run_id=c.run_id LEFT JOIN message_branches b ON b.message_id=m.id LEFT JOIN message_feedback f ON f.message_id=m.id LEFT JOIN legacy_message_details d ON d.message_id=m.id WHERE m.chat_id=?1 ORDER BY m.rowid")?;
    let mut out=q.query_map([chat],|r|Ok(json!({"id":r.get::<_,String>(0)?,"role":r.get::<_,String>(1)?,"content":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"error":r.get::<_,Option<String>>(4)?,"created_at":r.get::<_,i64>(5)?,"run_id":r.get::<_,Option<String>>(6)?,"warning":r.get::<_,Option<String>>(10)?,"finish_reason":r.get::<_,Option<String>>(11)?,"transport_incomplete":r.get::<_,Option<bool>>(12)?.unwrap_or(false),"snapshot":match r.get::<_,Option<String>>(7)? {Some(provider)=>json!({"provider":provider,"endpoint":r.get::<_,String>(8)?,"model":r.get::<_,String>(9)?}),None=>Value::Null},"parent_id":r.get::<_,Option<String>>(13)?,"linked":r.get::<_,Option<String>>(14)?.is_some(),"feedback":r.get::<_,String>(15)?,"feedback_revision":r.get::<_,i64>(16)?,"reasoning":r.get::<_,Option<String>>(17)?})))?.collect::<Result<Vec<_>,_>>()?;
    let mut previous = Value::Null;
    let mut user = Value::Null;
    for m in &mut out {
        if m["linked"] == false {
            m["parent_id"] = if m["role"] == "assistant" && !user.is_null() {
                user.clone()
            } else {
                previous.clone()
            };
        }
        m.as_object_mut().unwrap().remove("linked");
        if m["role"] == "user" {
            user = m["id"].clone();
        }
        previous = m["id"].clone();
        m["model_provenance"] = match m["run_id"].as_str() {
            Some(run_id) => json!(crate::inference_provenance::read(
                db,
                run_id,
                m["snapshot"]["provider"].as_str().unwrap_or("")
            )?),
            None => Value::Null,
        };
        let mut q=db.prepare("SELECT id,name,media_type,content FROM text_attachments WHERE message_id=?1 ORDER BY rowid")?;
        m["attachments"]=json!(q.query_map([m["id"].as_str().unwrap()], |r|Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"media_type":r.get::<_,String>(2)?,"content":r.get::<_,String>(3)?})))?.collect::<Result<Vec<_>,_>>()?);
        m["media"] = json!(crate::media_api::message_media(
            db,
            m["id"].as_str().unwrap()
        )?);
        m["media_context"] = json!(crate::media_api::provider_media_content(
            db,
            m["id"].as_str().unwrap()
        )?);
    }
    Ok(out)
}
pub fn leaf(db: &Connection, chat: &str, all: &[Value]) -> ApiResult<Option<String>> {
    use rusqlite::OptionalExtension;
    let saved: Option<Option<String>> = db
        .query_row(
            "SELECT leaf_id FROM conversation_heads WHERE chat_id=?1",
            [chat],
            |r| r.get(0),
        )
        .optional()?;
    Ok(saved
        .flatten()
        .or_else(|| all.last().and_then(|m| m["id"].as_str().map(str::to_owned))))
}
pub fn path(all: &[Value], leaf: Option<&str>) -> ApiResult<Vec<Value>> {
    let by_id: HashMap<&str, &Value> = all.iter().map(|m| (m["id"].as_str().unwrap(), m)).collect();
    let mut seen = HashSet::new();
    let mut cursor = leaf;
    let mut out = vec![];
    while let Some(id) = cursor {
        if !seen.insert(id) {
            return Err(bad(
                "The saved branch contains a cycle. Export history before repairing this profile.",
            ));
        }
        let m=by_id.get(id).ok_or_else(||bad("The saved branch has a missing parent. Export history before repairing this profile."))?;
        out.push((*m).clone());
        cursor = m["parent_id"].as_str();
    }
    out.reverse();
    for m in &mut out {
        let siblings: Vec<&str> = all
            .iter()
            .filter(|s| s["parent_id"] == m["parent_id"] && s["role"] == m["role"])
            .filter_map(|s| s["id"].as_str())
            .collect();
        m["branch_index"] = json!(
            siblings
                .iter()
                .position(|s| Some(*s) == m["id"].as_str())
                .unwrap_or(0)
                + 1
        );
        m["siblings"] = json!(siblings);
    }
    Ok(out)
}
pub fn tip(all: &[Value], selected: &str) -> ApiResult<String> {
    if !all.iter().any(|m| m["id"] == selected) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Message not found in this conversation.".into(),
        ));
    }
    let mut tip = selected.to_owned();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(tip.clone()) {
            return Err(bad("The saved branch contains a cycle."));
        }
        match all.iter().rev().find(|m| m["parent_id"] == tip) {
            Some(m) => tip = m["id"].as_str().unwrap().to_owned(),
            None => return Ok(tip),
        }
    }
}
pub fn payload(db: &Connection, chat: &str) -> ApiResult<Value> {
    let summary = chat_management::summary(db, chat)?;
    let all = all(db, chat)?;
    let leaf = leaf(db, chat, &all)?;
    let active = path(&all, leaf.as_deref())?;
    Ok(json!({"chat":summary,"messages":all,"active_messages":active,"active_leaf":leaf}))
}
pub fn set_leaf(db: &Connection, chat: &str, leaf: &str) -> rusqlite::Result<usize> {
    db.execute("INSERT INTO conversation_heads(chat_id,leaf_id) VALUES(?1,?2) ON CONFLICT(chat_id) DO UPDATE SET leaf_id=excluded.leaf_id",params![chat,leaf])
}
pub fn insert_link(db: &Connection, id: &str, parent: Option<&str>) -> rusqlite::Result<usize> {
    db.execute(
        "INSERT INTO message_branches(message_id,parent_id) VALUES(?1,?2)",
        params![id, parent],
    )
}
pub fn attach(db: &Connection, message: &str, files: &[Attachment]) -> rusqlite::Result<()> {
    for f in files {
        db.execute("INSERT INTO text_attachments(id,message_id,name,media_type,content) VALUES(?1,?2,?3,?4,?5)",params![crate::id(),message,f.name,f.media_type,f.content])?;
    }
    Ok(())
}
pub fn provider_content(message: &Value) -> String {
    let mut text = message["content"].as_str().unwrap_or("").to_owned();
    if let Some(files) = message["attachments"].as_array() {
        for f in files {
            text.push_str(&format!(
                "\n\n--- Attached text file: {} ---\n{}\n--- End attached text file ---",
                f["name"].as_str().unwrap_or("text"),
                f["content"].as_str().unwrap_or("")
            ));
        }
    }
    text.push_str(message["media_context"].as_str().unwrap_or(""));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app, start_intent, Config, ConversationIntent, Expected, Prompt};
    use axum::{
        extract::{Path, State},
        Json,
    };
    use std::sync::Arc;
    fn expected(a: &crate::App) -> Expected {
        let c = a.config.lock().unwrap().clone();
        Expected {
            revision: a.consent.lock().unwrap().token(),
            provider: c.provider,
            endpoint: c.endpoint,
        }
    }
    fn payload(a: &crate::App) -> Value {
        super::payload(&a.db.lock().unwrap(), "c").unwrap()
    }
    async fn finish(a: &Arc<crate::App>, rid: &str) {
        let run = a.runs.lock().unwrap()[rid].clone();
        run.cancel.notify_one();
        let mut rx = run.tx.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while rx.borrow_and_update().status == "streaming" {
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }
    async fn send(a: &Arc<crate::App>, content: &str, intent: ConversationIntent) -> Value {
        let run = start_intent(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: content.into(),
                model: None,
            }),
            expected(a),
            None,
            intent,
        )
        .await
        .unwrap()
        .0;
        finish(a, run["run_id"].as_str().unwrap()).await;
        run
    }
    fn setup() -> (tempfile::TempDir, Arc<crate::App>) {
        let tmp = tempfile::tempdir().unwrap();
        let a = app(&tmp.path().join("test.sqlite")).unwrap();
        chat_management::create(&a.db.lock().unwrap(), "c", "test", 1).unwrap();
        (tmp, a)
    }
    #[tokio::test]
    async fn edits_and_regeneration_preserve_all_branches_and_precise_context() {
        let (_tmp, a) = setup();
        let first = send(
            &a,
            "original 🦀",
            ConversationIntent {
                attachments: Some(vec![Attachment {
                    name: "source.txt".into(),
                    content: "attached context".into(),
                    media_type: text_mime(),
                }]),
                ..Default::default()
            },
        )
        .await;
        let second = send(&a, "downstream", ConversationIntent::default()).await;
        let before = payload(&a);
        let original = first["user_message_id"].as_str().unwrap();
        let edited = send(
            &a,
            "edited root",
            ConversationIntent {
                version: before["chat"]["version"].as_i64(),
                edit: Some(original.into()),
                ..Default::default()
            },
        )
        .await;
        let edited_state = payload(&a);
        assert_eq!(edited_state["messages"].as_array().unwrap().len(), 6);
        assert_eq!(edited_state["active_messages"].as_array().unwrap().len(), 2);
        assert!(edited_state["active_messages"][0]["parent_id"].is_null());
        assert_eq!(
            edited_state["active_messages"][0]["attachments"][0]["content"],
            "attached context"
        );
        assert_eq!(
            edited_state["active_messages"][0]["siblings"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        for old in before["messages"].as_array().unwrap() {
            assert!(edited_state["messages"].as_array().unwrap().contains(old));
        }
        let switched = crate::select_branch(
            State(a.clone()),
            Path("c".into()),
            Json(crate::BranchInput {
                message_id: original.into(),
                version: edited_state["chat"]["version"].as_i64().unwrap(),
                expected: expected(&a),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(switched["active_leaf"], second["message_id"]);
        assert_eq!(switched["active_messages"].as_array().unwrap().len(), 4);
        let regenerated = start_intent(
            a.clone(),
            "c".into(),
            None,
            expected(&a),
            Some(Config::default().model),
            ConversationIntent {
                version: switched["chat"]["version"].as_i64(),
                regenerate: Some(first["message_id"].as_str().unwrap().into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .0;
        finish(&a, regenerated["run_id"].as_str().unwrap()).await;
        let after = payload(&a);
        assert_eq!(after["messages"].as_array().unwrap().len(), 7);
        assert_eq!(after["active_messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            after["active_messages"][1]["parent_id"],
            first["user_message_id"]
        );
        assert_eq!(
            after["active_messages"][1]["siblings"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let context = crate::request_history(after["active_messages"].as_array().unwrap().clone());
        assert_eq!(context.len(), 1);
        assert!(context[0]["content"]
            .as_str()
            .unwrap()
            .contains("attached context"));
        assert!(!context[0]["content"]
            .as_str()
            .unwrap()
            .contains("downstream"));
        let db = a.db.lock().unwrap();
        assert_eq!(
            leaf(&db, "c", &all(&db, "c").unwrap()).unwrap().unwrap(),
            regenerated["message_id"].as_str().unwrap()
        );
        assert!(all(&db, "c")
            .unwrap()
            .iter()
            .any(|m| m["id"] == edited["message_id"]));
    }
    #[tokio::test]
    async fn stale_foreign_active_and_failed_edits_leave_no_partial_graph_or_run() {
        let (_tmp, a) = setup();
        let first = send(&a, "first", ConversationIntent::default()).await;
        let before = payload(&a);
        let runs = a.runs.lock().unwrap().len();
        for intent in [
            ConversationIntent {
                version: Some(-1),
                edit: Some(first["user_message_id"].as_str().unwrap().into()),
                ..Default::default()
            },
            ConversationIntent {
                version: before["chat"]["version"].as_i64(),
                edit: Some("foreign-or-missing".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(
                start_intent(
                    a.clone(),
                    "c".into(),
                    Some(Prompt {
                        content: "edit".into(),
                        model: None
                    }),
                    expected(&a),
                    None,
                    intent
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::CONFLICT
            );
            assert_eq!(payload(&a), before);
            assert_eq!(a.runs.lock().unwrap().len(), runs);
        }
        a.db.lock().unwrap().execute_batch("CREATE TRIGGER fail_link BEFORE INSERT ON message_branches BEGIN SELECT RAISE(ABORT,'synthetic graph storage failure'); END;").unwrap();
        assert!(start_intent(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "edit".into(),
                model: None
            }),
            expected(&a),
            None,
            ConversationIntent {
                version: before["chat"]["version"].as_i64(),
                edit: Some(first["user_message_id"].as_str().unwrap().into()),
                ..Default::default()
            }
        )
        .await
        .is_err());
        assert_eq!(payload(&a), before);
        assert_eq!(a.runs.lock().unwrap().len(), runs);
        a.db.lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_link")
            .unwrap();
        let active = start_intent(
            a.clone(),
            "c".into(),
            Some(Prompt {
                content: "active".into(),
                model: None,
            }),
            expected(&a),
            None,
            ConversationIntent::default(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(
            crate::select_branch(
                State(a.clone()),
                Path("c".into()),
                Json(crate::BranchInput {
                    message_id: first["message_id"].as_str().unwrap().into(),
                    version: active["version"].as_i64().unwrap(),
                    expected: expected(&a)
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        finish(&a, active["run_id"].as_str().unwrap()).await;
    }
    fn v4_fixture(path: &std::path::Path) -> Connection {
        let db = Connection::open(path).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL DEFAULT 0,version INTEGER NOT NULL DEFAULT 0,title_locked INTEGER NOT NULL DEFAULT 0,activity INTEGER NOT NULL DEFAULT 0);CREATE TABLE messages(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,run_id TEXT);CREATE TABLE artifacts(id TEXT PRIMARY KEY,chat_id TEXT REFERENCES chats(id) ON DELETE SET NULL,name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL);CREATE TABLE run_configs(run_id TEXT PRIMARY KEY,provider TEXT NOT NULL,endpoint TEXT NOT NULL,model TEXT NOT NULL,warning TEXT,finish_reason TEXT,transport_incomplete INTEGER NOT NULL DEFAULT 0);PRAGMA user_version=4;").unwrap();
        db
    }
    #[test]
    fn schema_four_migrates_retries_without_changing_any_message_or_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("v4.sqlite");
        let db = v4_fixture(&file);
        db.execute_batch(" INSERT INTO chats(id,title,created_at,version,title_locked) VALUES('c','kept title',1,7,1); INSERT INTO settings VALUES('other','retained'); INSERT INTO messages VALUES('u','c','user','original 🦀','completed',NULL,1,NULL),('a','c','assistant','partial','cancelled','retained error',2,'r'),('b','c','assistant','retry','limited',NULL,3,'s'),('v','c','user','second','completed',NULL,4,NULL); INSERT INTO run_configs(run_id,provider,endpoint,model,warning,finish_reason,transport_incomplete) VALUES('r','mock','http://127.0.0.1:11434','demo','notice','length',1); INSERT INTO artifacts VALUES('art','c','test.txt','artifact',1);").unwrap();
        let old: Vec<(String, String, String, Option<String>)> = db
            .prepare("SELECT id,content,status,error FROM messages ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        drop(db);
        let db = crate::open_db(&file).unwrap();
        let new: Vec<(String, String, String, Option<String>)> = db
            .prepare("SELECT id,content,status,error FROM messages ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(old, new);
        let all = all(&db, "c").unwrap();
        assert_eq!(all[1]["parent_id"], "u");
        assert_eq!(all[2]["parent_id"], "u");
        assert_eq!(all[3]["parent_id"], "b");
        let active = path(&all, leaf(&db, "c", &all).unwrap().as_deref()).unwrap();
        assert_eq!(
            active
                .iter()
                .map(|m| m["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["u", "b", "v"]
        );
        assert_eq!(chat_management::summary(&db, "c").unwrap().version, 7);
        assert_eq!(
            db.query_row("SELECT value FROM settings WHERE key='other'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "retained"
        );
        assert_eq!(
            db.query_row(
                "SELECT warning FROM run_configs WHERE run_id='r'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "notice"
        );
        assert_eq!(
            db.query_row("SELECT content FROM artifacts", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "artifact"
        );
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            smith_profile_storage::SCHEMA_VERSION
        );
        assert!(db
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none());
    }
    #[test]
    fn schema_five_failure_rolls_back_all_new_tables_and_preserves_source_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("invalid-v4.sqlite");
        let db = v4_fixture(&file);
        db.execute_batch(" INSERT INTO chats(id,title,created_at) VALUES('c','preserve invalid profile',1); INSERT INTO messages VALUES('bad','c',X'FF','preserve exact content','completed',NULL,1,NULL);").unwrap();
        drop(db);
        assert!(crate::open_db(&file).is_err());
        let db = Connection::open(&file).unwrap();
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            4
        );
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('message_branches','conversation_heads','text_attachments')",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(
            db.query_row(
                "SELECT hex(role),content FROM messages WHERE id='bad'",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            )
            .unwrap(),
            ("FF".into(), "preserve exact content".into())
        );
    }

    #[test]
    fn malformed_graph_and_unsupported_attachments_are_rejected() {
        let cycle = vec![
            json!({"id":"a","parent_id":"b","role":"user"}),
            json!({"id":"b","parent_id":"a","role":"assistant"}),
        ];
        assert!(path(&cycle, Some("a")).is_err());
        assert!(tip(&cycle, "a").is_err());
        assert!(path(&cycle, Some("missing")).is_err());
        for file in [
            Attachment {
                name: "image.png".into(),
                content: "encoded".into(),
                media_type: "image/png".into(),
            },
            Attachment {
                name: "../source.txt".into(),
                content: "text".into(),
                media_type: text_mime(),
            },
            Attachment {
                name: "binary.txt".into(),
                content: "nul\0".into(),
                media_type: text_mime(),
            },
            Attachment {
                name: "large.txt".into(),
                content: "x".repeat(100_001),
                media_type: text_mime(),
            },
        ] {
            assert!(validate_attachments(&[file]).is_err());
        }
        assert!(validate_attachments(&[Attachment {
            name: "unicode.txt".into(),
            content: "hello 🦀\n\ttext".into(),
            media_type: text_mime()
        }])
        .is_ok());
    }
}
