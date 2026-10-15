use serde_json::{json, Value};
use smith_native_bridge::bridge::{Bridge, Result, Transport};
use std::sync::{Arc, Mutex};
struct State {
    revision: i64,
    kind: String,
    chat_version: i64,
    session: &'static str,
    writes: usize,
}
struct Mock(Arc<Mutex<State>>);
impl Transport for Mock {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        _: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut s = self.0.lock().unwrap();
        if path == "/api/profile/status" {
            return Ok(json!({"state":"unlocked","session":s.session}));
        }
        if path.ends_with("/feedback") {
            let body = body.unwrap();
            if body["expected_revision"] != s.revision || body["chat_version"] != s.chat_version {
                return Err("stale feedback".into());
            }
            s.revision += 1;
            s.kind = body["kind"].as_str().unwrap().into();
            s.writes += 1;
            return Ok(json!({"message_id":"answer","kind":s.kind,"revision":s.revision}));
        }
        Ok(match path {
            "/api/config" => {
                json!({"revision":"r","provider":"mock","endpoint":"","model":"demo","tested":false})
            }
            "/api/chats" => json!([]),
            "/api/media/artifacts" => json!([]),
            "/api/agent/tasks" => json!({"tasks":[]}),
            "/api/actions" => json!({"revision":0}),
            "/api/trash" => json!({"chats":[]}),
            "/api/drafts/new" | "/api/drafts/chat" => {
                json!({"owner":if path.ends_with("new"){"new"}else{"chat"},"version":0,"text":""})
            }
            "/api/chats/chat" => {
                let message = json!({"id":"answer","role":"assistant","content":"Saved answer","status":"completed","feedback":s.kind,"feedback_revision":s.revision,"reasoning":"Imported rationale","run_id":null});
                json!({"chat":{"id":"chat","title":"Synthetic","version":s.chat_version},"messages":[message],"active_messages":[message],"active_leaf":"answer"})
            }
            _ if path.starts_with("/api/models?") => json!({"revision":"r","models":[]}),
            _ => panic!("Unexpected {method} {path}"),
        })
    }
}
#[test]
fn exact_answer_cas_clear_tombstone_and_revoked_review() {
    let s = Arc::new(Mutex::new(State {
        revision: 0,
        kind: "".into(),
        chat_version: 4,
        session: "one",
        writes: 0,
    }));
    let mut b = Bridge::new(Mock(s.clone()));
    b.initialize().unwrap();
    b.open("chat").unwrap();
    let old = b.feedback_review("answer").unwrap();
    b.feedback(&old, "thumbs_up").unwrap();
    assert_eq!(
        b.view.conversation.as_ref().unwrap().active_messages[0].feedback,
        "thumbs_up"
    );
    assert!(b.feedback(&old, "thumbs_down").is_err());
    let current = b.feedback_review("answer").unwrap();
    b.feedback(&current, "").unwrap();
    assert_eq!(s.lock().unwrap().revision, 2);
    let current = b.feedback_review("answer").unwrap();
    s.lock().unwrap().session = "two";
    assert!(b.feedback(&current, "thumbs_up").is_err());
    assert_eq!(s.lock().unwrap().writes, 2);
    assert!(!b.view.available);
}
