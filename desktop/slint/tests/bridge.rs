use smith_native_bridge::{
    bridge::{encode_component, Bridge, NavigationEpoch},
    fixture::Fixture,
};

#[test]
fn restarted_untested_provider_keeps_local_workspace_without_discovery() {
    use serde_json::{json, Value};
    use smith_native_bridge::bridge::{Result, Transport};
    struct SavedExternal;
    impl Transport for SavedExternal {
        fn request(
            &mut self,
            _: &str,
            path: &str,
            _: Option<&str>,
            _: Option<Value>,
        ) -> Result<Value> {
            Ok(match path {
                "/api/profile/status" => json!({"state":"unlocked","session":"fresh-local"}),
                "/api/config" => {
                    json!({"revision":"fresh:0","provider":"openai","endpoint":"http://127.0.0.1:18499/v1","model":"saved-model","tested":false})
                }
                "/api/chats" => {
                    json!([{"id":"retained-copy-id","title":"Saved local chat","version":2}])
                }
                "/api/media/artifacts" => json!({"artifacts":[]}),
                "/api/agent/tasks" => json!({"tasks":[]}),
                "/api/actions" => json!({"revision":0,"undo":null,"redo":null}),
                "/api/trash" => json!({"chats":[]}),
                "/api/drafts/new" => {
                    json!({"owner":"new","version":0,"text":"","attachments":[],"media":[]})
                }
                _ => panic!("Restart must not discover or test a saved provider: {path}"),
            })
        }
    }
    let mut bridge = Bridge::new(SavedExternal);
    bridge.initialize().unwrap();
    assert!(bridge.view.available);
    assert_eq!(bridge.view.chats.len(), 1);
    assert!(bridge.view.models.is_empty());
    assert_eq!(bridge.view.config.model, "saved-model");
}

#[test]
fn encrypted_draft_contract_cas_navigation_and_reversible_management() {
    let fixture = Fixture::start().unwrap();
    let mut bridge = Bridge::new(fixture.transport());
    bridge.initialize().unwrap();
    bridge.new_chat().unwrap();
    let first = bridge.view.conversation.as_ref().unwrap().chat.clone();
    let identity = bridge.view.draft.identity;
    bridge
        .change_draft(identity, "Durable first draft".into())
        .unwrap();
    bridge.new_chat().unwrap(); // flushes the previous owner before navigation
    assert!(bridge.change_draft(identity, "late edit".into()).is_err());
    bridge.open(&first.id).unwrap();
    assert_eq!(bridge.view.draft.text, "Durable first draft");
    let mut second = Bridge::new(fixture.transport());
    second.initialize().unwrap();
    second.open(&first.id).unwrap();
    second
        .change_draft(second.view.draft.identity, "Other client draft".into())
        .unwrap();
    second.save_draft().unwrap();
    bridge
        .change_draft(bridge.view.draft.identity, "Visible local draft".into())
        .unwrap();
    assert!(bridge.new_chat().is_err());
    assert!(bridge.view.draft.conflict);
    assert_eq!(bridge.view.draft.text, "Visible local draft");
    bridge.discard_and_reload_draft().unwrap();
    assert_eq!(bridge.view.draft.text, "Other client draft");
    let config = bridge.view.config.clone();
    bridge.rename(&first, &config, "Renamed natively").unwrap();
    let renamed = bridge.view.conversation.as_ref().unwrap().chat.clone();
    let stale_history = bridge.view.actions.clone();
    bridge.reverse(&stale_history, false).unwrap();
    assert_eq!(
        bridge.view.conversation.as_ref().unwrap().chat.title,
        first.title
    );
    assert!(bridge.reverse(&stale_history, false).is_err());
    bridge.reverse(&bridge.view.actions.clone(), true).unwrap();
    assert_eq!(
        bridge.view.conversation.as_ref().unwrap().chat.title,
        renamed.title
    );
    let chat = bridge.view.conversation.as_ref().unwrap().chat.clone();
    bridge
        .trash(&[chat], &bridge.view.actions.clone(), false)
        .unwrap();
    assert!(bridge.view.conversation.is_none());
    assert_eq!(bridge.view.trash.len(), 1);
    bridge
        .trash(
            &bridge.view.trash.clone(),
            &bridge.view.actions.clone(),
            true,
        )
        .unwrap();
    bridge.open(&first.id).unwrap();
    assert_eq!(bridge.view.draft.text, "Other client draft");
}

#[test]
fn owned_http_chat_history_completion_and_cancellation() {
    let fixture = Fixture::start().unwrap();
    let mut bridge = Bridge::new(fixture.transport());
    bridge.initialize().unwrap();
    assert!(bridge.view.available);
    assert_eq!(bridge.view.models, ["smith-demo"]);
    assert_eq!(bridge.view.artifacts.len(), 1);
    assert_eq!(bridge.view.tasks.len(), 1);
    bridge.new_chat().unwrap();
    let id = bridge.view.conversation.as_ref().unwrap().chat.id.clone();
    let intent = bridge.intent().unwrap();
    bridge.send(&intent, "Native synthetic message").unwrap();
    assert!(bridge.streaming());
    for _ in 0..4 {
        bridge.poll().unwrap();
    }
    assert!(!bridge.streaming());
    assert_eq!(
        bridge
            .view
            .conversation
            .as_ref()
            .unwrap()
            .active_messages
            .len(),
        2
    );
    let saved = bridge.view.conversation.clone();
    bridge.new_chat().unwrap();
    bridge.open(&id).unwrap();
    assert_eq!(saved, bridge.view.conversation);
    let intent = bridge.intent().unwrap();
    bridge
        .send(&intent, "Cancel this synthetic response")
        .unwrap();
    bridge.cancel().unwrap();
    assert!(!bridge.streaming());
    assert_eq!(
        bridge
            .view
            .conversation
            .as_ref()
            .unwrap()
            .active_messages
            .last()
            .unwrap()
            .status,
        "cancelled"
    );
}

#[test]
fn explicit_branch_version_preserves_old_answers() {
    let fixture = Fixture::start().unwrap();
    let mut bridge = Bridge::new(fixture.transport());
    bridge.initialize().unwrap();
    bridge.new_chat().unwrap();
    bridge.send(&bridge.intent().unwrap(), "A prompt").unwrap();
    for _ in 0..4 {
        bridge.poll().unwrap();
    }
    let old = bridge
        .view
        .conversation
        .as_ref()
        .unwrap()
        .active_leaf
        .clone()
        .unwrap();
    let stale = bridge.intent().unwrap();
    bridge.regenerate(&stale, &old).unwrap();
    for _ in 0..4 {
        bridge.poll().unwrap();
    }
    let conversation = bridge.view.conversation.as_ref().unwrap();
    assert_eq!(conversation.messages.len(), 3);
    assert_eq!(conversation.active_messages.len(), 2);
    let new = conversation.active_leaf.clone().unwrap();
    assert_ne!(old, new);
    let writes = fixture.mutations();
    assert!(bridge.branch(&stale, &old).unwrap_err().contains("409"));
    assert_eq!(writes, fixture.mutations());
    bridge.branch(&bridge.intent().unwrap(), &old).unwrap();
    assert_eq!(
        bridge
            .view
            .conversation
            .as_ref()
            .unwrap()
            .active_leaf
            .as_deref(),
        Some(old.as_str())
    );
    bridge.branch(&bridge.intent().unwrap(), &new).unwrap();
    assert_eq!(bridge.view.conversation.as_ref().unwrap().messages.len(), 3);
}

#[test]
fn stale_connection_is_never_replayed() {
    let fixture = Fixture::start().unwrap();
    let mut bridge = Bridge::new(fixture.transport());
    bridge.initialize().unwrap();
    bridge.new_chat().unwrap();
    let intent = bridge.intent().unwrap();
    let writes = fixture.mutations();
    fixture.advance_connection();
    assert!(bridge
        .send(&intent, "Must remain unsent")
        .unwrap_err()
        .contains("409"));
    assert_eq!(writes, fixture.mutations());
}

#[test]
fn session_revocation_clears_all_private_view_models() {
    let fixture = Fixture::start().unwrap();
    let mut bridge = Bridge::new(fixture.transport());
    bridge.initialize().unwrap();
    bridge.new_chat().unwrap();
    let intent = bridge.intent().unwrap();
    let writes = fixture.mutations();
    fixture.rotate_session();
    assert!(bridge.poll().is_err());
    assert!(!bridge.view.available);
    assert!(bridge.view.chats.is_empty());
    assert!(bridge.view.artifacts.is_empty());
    assert!(bridge.view.tasks.is_empty());
    assert!(bridge.view.conversation.is_none());
    assert!(bridge.send(&intent, "Must not cross session").is_err());
    assert_eq!(writes, fixture.mutations());
}

#[test]
fn opaque_ids_and_late_navigation_results_keep_their_owner() {
    assert_eq!(
        encode_component("copy/id with?unicode-π"),
        "copy%2Fid%20with%3Funicode-%CF%80"
    );
    let mut epoch = NavigationEpoch::default();
    let first = epoch.advance();
    assert!(epoch.accepts(first));
    epoch.advance();
    assert!(!epoch.accepts(first));
}

#[test]
fn unsaved_answer_overlay_keeps_fresh_branch_metadata() {
    use serde_json::{json, Value};
    use smith_native_bridge::{
        bridge::{Result, Transport},
        fixture::FixtureTransport,
    };
    struct Recovery {
        inner: FixtureTransport,
        message: String,
    }
    impl Transport for Recovery {
        fn request(
            &mut self,
            method: &str,
            path: &str,
            session: Option<&str>,
            body: Option<Value>,
        ) -> Result<Value> {
            let mut value = self.inner.request(method, path, session, body)?;
            if method == "POST" && path.ends_with("/messages") {
                self.message = value["message_id"].as_str().unwrap().to_owned();
            }
            if method == "GET" && path.starts_with("/api/chats/") && !self.message.is_empty() {
                for message in value["active_messages"].as_array_mut().unwrap() {
                    if message["id"] == self.message {
                        message["siblings"] = json!([self.message, "fresh-alternative"]);
                    }
                }
            }
            Ok(value)
        }
        fn event_snapshot(&mut self, _: &str, _: &str) -> Result<Option<Value>> {
            Ok(Some(
                json!({"message_id":self.message,"content":"Visible answer whose save failed","status":"failed","error":"Synthetic persistence failure","persisted":false}),
            ))
        }
    }
    let fixture = Fixture::start().unwrap();
    let mut bridge = Bridge::new(Recovery {
        inner: fixture.transport(),
        message: String::new(),
    });
    bridge.initialize().unwrap();
    bridge.new_chat().unwrap();
    bridge.send(&bridge.intent().unwrap(), "Prompt").unwrap();
    bridge.poll().unwrap();
    let answer = bridge
        .view
        .conversation
        .as_ref()
        .unwrap()
        .active_messages
        .last()
        .unwrap();
    assert_eq!(answer.content, "Visible answer whose save failed");
    assert!(!answer.persisted);
    assert_eq!(answer.status, "failed");
    assert_eq!(answer.siblings.len(), 2);
    assert_eq!(answer.siblings[1], "fresh-alternative");
}
