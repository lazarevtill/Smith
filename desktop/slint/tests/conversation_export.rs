use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Bridge, Result, Transport},
    conversation_export::ConversationExportReview,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

struct State {
    session: String,
    snapshot: Value,
    rotate_after_snapshot: bool,
    requests: Vec<(String, String)>,
}
struct Mock(Arc<Mutex<State>>);
impl Transport for Mock {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        assert_eq!(
            method, "GET",
            "Export must never mutate a chat or call a provider"
        );
        assert!(body.is_none());
        let mut state = self.0.lock().unwrap();
        state.requests.push((method.into(), path.into()));
        if path == "/api/profile/status" {
            return Ok(json!({"state":"unlocked","session":state.session}));
        }
        assert_eq!(session, Some(state.session.as_str()));
        Ok(match path {
            "/api/config" => {
                json!({"revision":"config:1","provider":"mock","endpoint":"","model":"demo","tested":false})
            }
            "/api/chats" => json!([state.snapshot["chat"]]),
            "/api/chats/copied%2Fsession" => {
                let snapshot = state.snapshot.clone();
                if state.rotate_after_snapshot {
                    state.session = "new-session".into();
                }
                snapshot
            }
            "/api/media/artifacts" => json!([]),
            "/api/agent/tasks" => json!({"tasks":[]}),
            "/api/actions" => json!({"revision":0}),
            "/api/trash" => json!({"chats":[]}),
            "/api/drafts/new" => json!({"owner":"new","version":0,"text":""}),
            "/api/drafts/copied%2Fsession" => {
                json!({"owner":"copied/session","version":0,"text":""})
            }
            _ if path.starts_with("/api/models?") => json!({"revision":"config:1","models":[]}),
            _ => panic!("Unexpected route {path}"),
        })
    }
}
fn snapshot() -> Value {
    let user = json!({"id":"u","parent_id":null,"role":"user","content":"A raw question\r\nПривет","status":"completed","run_id":null,"attachments":[{"name":"context.txt","content":"ATTACHMENT_ONLY_SECRET"}],"media_context":"PROVIDER_ONLY_EXPANSION"});
    let discarded = json!({"id":"old","parent_id":"u","role":"assistant","content":"ABANDONED_BRANCH","status":"completed","run_id":null});
    let answer = json!({"id":"a","parent_id":"u","role":"assistant","content":"<think>\n\n</think>\nAnswer\r\n```rust\nraw()\n```\n","status":"limited","run_id":null,"reasoning":null,"feedback":"thumbs_up","feedback_revision":2,"transport_incomplete":true,"unknown_retained":{"original":"é\u{0000}"}});
    let mut active = vec![user.clone(), answer.clone()];
    active[1]["siblings"] = json!(["old", "a"]);
    active[1]["branch_index"] = json!(2);
    json!({"chat":{"id":"copied/session","title":"Synthetic exported chat","version":4},"messages":[user,discarded,answer],"active_messages":active,"active_leaf":"a"})
}
fn make(open: bool) -> (Bridge<Mock>, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State {
        session: "first-session".into(),
        snapshot: snapshot(),
        rotate_after_snapshot: false,
        requests: Vec::new(),
    }));
    let mut bridge = Bridge::new(Mock(state.clone()));
    bridge.initialize().unwrap();
    if open {
        bridge.open("copied/session").unwrap();
    }
    (bridge, state)
}
fn review(bridge: &Bridge<Mock>) -> ConversationExportReview {
    bridge
        .conversation_export_reviews()
        .remove("copied/session")
        .unwrap()
}
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "smith-conversation-export-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn active_branch_markdown_keeps_raw_text_and_status_without_context_substitution() {
    let (mut bridge, state) = make(true);
    let root = Directory::new();
    let path = root.path("conversation.md");
    bridge.export_conversation(&review(&bridge), &path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let snapshot = &state.lock().unwrap().snapshot;
    assert_eq!(text, format!("# Conversation\n\n**user:**\n\n{}\n\n---\n\n> Reply limited\n\n**assistant:**\n\n{}\n\n---\n\n", snapshot["active_messages"][0]["content"].as_str().unwrap(), snapshot["active_messages"][1]["content"].as_str().unwrap()));
    for excluded in [
        "ABANDONED_BRANCH",
        "ATTACHMENT_ONLY_SECRET",
        "PROVIDER_ONLY_EXPANSION",
    ] {
        assert!(!text.contains(excluded));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(bridge.view.status, "Conversation exported.");
}
#[test]
fn json_preserves_current_api_objects_unknown_fields_and_raw_bytes_as_values() {
    let (mut bridge, state) = make(false);
    let root = Directory::new();
    let path = root.path("conversation.JSON");
    bridge.export_conversation(&review(&bridge), &path).unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved, state.lock().unwrap().snapshot["active_messages"]);
    assert_eq!(saved[1]["unknown_retained"]["original"], "é\u{0000}");
    assert!(
        bridge.view.conversation.is_none(),
        "Export must not navigate or adopt another chat"
    );
}
#[test]
fn selected_branch_uses_its_displayed_version_when_sidebar_cache_is_older() {
    let (mut bridge, _) = make(true);
    bridge.view.chats[0].version = 3;
    let captured = review(&bridge);
    assert_eq!(captured.chat().version, 4);
    let root = Directory::new();
    bridge
        .export_conversation(&captured, &root.path("selected.md"))
        .unwrap();
}
#[test]
fn changed_chat_version_leaf_or_text_refuses_without_creating_file() {
    for change in 0..3 {
        let (mut bridge, state) = make(true);
        let review = review(&bridge);
        let root = Directory::new();
        let path = root.path("stale.md");
        {
            let mut s = state.lock().unwrap();
            match change {
                0 => s.snapshot["chat"]["version"] = json!(5),
                1 => s.snapshot["active_leaf"] = json!("old"),
                _ => {
                    s.snapshot["active_messages"][1]["content"] = json!("Changed");
                    s.snapshot["messages"][2]["content"] = json!("Changed");
                }
            }
        }
        assert!(bridge.export_conversation(&review, &path).is_err());
        assert!(!path.exists());
    }
}
#[test]
fn prior_session_review_cannot_be_rebound_and_late_revocation_suppresses_write() {
    for reinitialize in [false, true] {
        let (mut bridge, state) = make(true);
        let old = review(&bridge);
        state.lock().unwrap().session = "second-session".into();
        if reinitialize {
            bridge.initialize().unwrap();
            bridge.open("copied/session").unwrap();
        }
        let root = Directory::new();
        let path = root.path("old.json");
        assert!(bridge.export_conversation(&old, &path).is_err());
        assert!(!path.exists());
    }
    let (mut bridge, state) = make(true);
    let old = review(&bridge);
    state.lock().unwrap().rotate_after_snapshot = true;
    let root = Directory::new();
    let path = root.path("late.json");
    assert!(bridge.export_conversation(&old, &path).is_err());
    assert!(!path.exists());
    assert!(!bridge.view.available);
}
#[test]
fn unsaved_or_streaming_visible_content_has_no_export_review() {
    for status in ["streaming", "unknown_status"] {
        let (mut bridge, _) = make(true);
        bridge.view.conversation.as_mut().unwrap().active_messages[1].status = status.into();
        assert!(bridge.conversation_export_reviews().is_empty());
    }
    let (mut bridge, _) = make(true);
    bridge.view.conversation.as_mut().unwrap().active_messages[1].persisted = false;
    assert!(bridge.conversation_export_reviews().is_empty());
}
#[test]
fn malformed_incomplete_or_nonterminal_snapshot_never_creates_a_partial_export() {
    for case in 0..9 {
        let (mut bridge, state) = make(false);
        let reviewed = review(&bridge);
        {
            let mut state = state.lock().unwrap();
            let s = &mut state.snapshot;
            match case {
                0 => s["active_messages"][1]["parent_id"] = json!("missing"),
                1 => s["active_messages"][0]["parent_id"] = json!("a"),
                2 => s["active_messages"][1]["content"] = json!("not the saved message"),
                3 => {
                    s["messages"][2]["status"] = json!("streaming");
                    s["active_messages"][1]["status"] = json!("streaming");
                }
                4 => {
                    s["messages"][2]["persisted"] = json!(false);
                    s["active_messages"][1]["persisted"] = json!(false);
                }
                5 => {
                    let duplicate = s["active_messages"][0].clone();
                    s["messages"].as_array_mut().unwrap().push(duplicate);
                }
                6 => s["active_messages"] = json!([]),
                7 => s["messages"] = json!([]),
                _ => {
                    s["active_messages"].as_array_mut().unwrap().remove(0);
                }
            }
        }
        let root = Directory::new();
        let path = root.path("invalid.md");
        assert!(
            bridge.export_conversation(&reviewed, &path).is_err(),
            "case {case}"
        );
        assert!(!path.exists());
    }
}
#[test]
fn bounds_are_checked_before_file_creation_even_on_a_non_http_transport() {
    let (mut bridge, state) = make(false);
    let reviewed = review(&bridge);
    state.lock().unwrap().snapshot["extra_snapshot_data"] =
        json!("x".repeat(smith_native_bridge::conversation_export::MAX_SNAPSHOT_BYTES));
    let root = Directory::new();
    let path = root.path("large.json");
    assert!(bridge
        .export_conversation(&reviewed, &path)
        .unwrap_err()
        .contains("2 MiB"));
    assert!(!path.exists());
}
#[test]
fn existing_file_symlink_bad_extension_and_relative_destination_are_refused() {
    let (mut bridge, _) = make(true);
    let root = Directory::new();
    let path = root.path("existing.md");
    std::fs::write(&path, b"Keep existing bytes").unwrap();
    assert!(bridge.export_conversation(&review(&bridge), &path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"Keep existing bytes");
    #[cfg(unix)]
    {
        let alias = root.path("alias.json");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(bridge
            .export_conversation(&review(&bridge), &alias)
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"Keep existing bytes");
    }
    let unknown = root.path("mislabelled.txt");
    assert!(bridge
        .export_conversation(&review(&bridge), &unknown)
        .is_err());
    assert!(!unknown.exists());
    assert!(bridge
        .export_conversation(&review(&bridge), Path::new("relative.md"))
        .is_err());
}
#[test]
fn a_cloned_review_cannot_duplicate_a_write_or_retry_after_a_failed_attempt() {
    let (mut bridge, _) = make(true);
    let reviewed = review(&bridge);
    let clone = reviewed.clone();
    let root = Directory::new();
    bridge
        .export_conversation(&reviewed, &root.path("one.md"))
        .unwrap();
    assert!(bridge
        .export_conversation(&clone, &root.path("two.json"))
        .is_err());
    assert!(!root.path("two.json").exists());
    let fresh = review(&bridge);
    std::fs::write(root.path("existing.md"), b"existing").unwrap();
    assert!(bridge
        .export_conversation(&fresh, &root.path("existing.md"))
        .is_err());
    assert!(bridge
        .export_conversation(&fresh, &root.path("retry.md"))
        .is_err());
    assert!(!root.path("retry.md").exists());
}
