use serde_json::{json, Value};
use smith_native_bridge::{
    agents_bridge::*,
    bridge::{Bridge, Result, Transport},
    task_bridge::WorkspaceSnapshot,
};
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct State {
    revoked: bool,
    mutation_revokes: bool,
    requests: Vec<(String, String, Option<Value>)>,
    pages: Vec<Value>,
    receipt: Value,
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
        s.requests.push((method.into(), path.into(), body));
        if path == "/api/profile/status" {
            return Ok(json!({"state":"unlocked","session":if s.revoked{"second"}else{"first"}}));
        }
        if path.starts_with("/api/agent/definitions") {
            if method == "GET" {
                return if !s.pages.is_empty() {
                    Ok(s.pages.remove(0))
                } else {
                    panic!("Unexpected extra page request")
                };
            }
            if s.mutation_revokes {
                s.revoked = true;
            }
            return Ok(s.receipt.clone());
        }
        Ok(match path {
            "/api/config" => {
                json!({"revision":"r1","provider":"mock","endpoint":"","model":"smith-demo","tested":false})
            }
            "/api/chats" => json!([]),
            "/api/agent/tasks" => json!({"tasks":[]}),
            "/api/media/artifacts" => json!([]),
            "/api/trash" => json!({"chats":[]}),
            "/api/actions" => json!({"revision":0}),
            "/api/drafts/new" => json!({"owner":"new","version":0,"text":""}),
            _ if path.starts_with("/api/models?") => json!({"revision":"r1","models":[]}),
            _ => panic!("Unexpected request {path}"),
        })
    }
}
fn make() -> (
    Bridge<Mock>,
    Arc<Mutex<State>>,
    WorkspaceSnapshot,
    AgentDefinition,
) {
    let state = Arc::new(Mutex::new(State::default()));
    let mut b = Bridge::new(Mock(state.clone()));
    b.initialize().unwrap();
    let workspace=serde_json::from_value(json!({"id":"11111111-1111-4111-8111-111111111111","label":"Synthetic","capabilities":["read","control"],"resources":[],"policy_revision":1,"available":true,"output_boundary":"plaintext_private_temporary_files"})).unwrap();
    let row=serde_json::from_value(json!({"id":"22222222-2222-4222-8222-222222222222","revision":1,"definition":{"name":"Reader","system_prompt":"Read only","soul":"Be concise","workspace_id":"11111111-1111-4111-8111-111111111111","policy_revision":1,"capabilities":["read"],"resources":[],"model":{"mode":"selected_at_launch"}},"created_at":1,"updated_at":1,"deleted":false})).unwrap();
    (b, state, workspace, row)
}
fn reviewed(b: &mut Bridge<Mock>, s: &Arc<Mutex<State>>, row: &AgentDefinition) -> AgentReview {
    s.lock().unwrap().pages = vec![json!({"definitions":[row],"next_after":null})];
    b.agent_catalog().unwrap().review(row).unwrap()
}
fn mutations(s: &Arc<Mutex<State>>) -> usize {
    s.lock()
        .unwrap()
        .requests
        .iter()
        .filter(|v| v.0 != "GET")
        .count()
}
#[test]
fn unchanged_save_accepts_same_revision_changed_save_requires_next() {
    let (mut b, s, workspace, row) = make();
    let r = reviewed(&mut b, &s, &row);
    let edit = b
        .agent_editor()
        .unwrap()
        .edit(Some(&r), row.definition.clone(), &workspace)
        .unwrap();
    s.lock().unwrap().receipt = json!({"definition":row});
    let r = b.agent_save(&edit, true).unwrap();
    assert_eq!(r.definition().revision, 1);
    let mut changed = row.definition.clone();
    changed.name = "Updated".into();
    let edit = b
        .agent_editor()
        .unwrap()
        .edit(Some(&r), changed.clone(), &workspace)
        .unwrap();
    let mut returned = row.clone();
    returned.definition = changed.clone();
    s.lock().unwrap().receipt = json!({"definition":returned});
    assert!(b.agent_save(&edit, true).is_err());
    returned.revision = 2;
    s.lock().unwrap().receipt = json!({"definition":returned});
    assert!(b.agent_save(&edit, true).is_err()); // an uncertain receipt is never replayed
    let fresh = reviewed(&mut b, &s, &row);
    let edit = b
        .agent_editor()
        .unwrap()
        .edit(Some(&fresh), changed.clone(), &workspace)
        .unwrap();
    assert_eq!(
        b.agent_save(&edit, true).unwrap().definition().definition,
        changed
    );
}
#[test]
fn subset_and_old_session_refuse_without_mutation() {
    let (mut b, s, workspace, row) = make();
    let r = reviewed(&mut b, &s, &row);
    let mut config = row.definition.clone();
    config
        .capabilities
        .push(smith_native_bridge::task_bridge::Permission::Write);
    assert!(b.agent_edit_review(Some(&r), config, &workspace).is_err());
    let edit = b
        .agent_edit_review(Some(&r), row.definition.clone(), &workspace)
        .unwrap();
    s.lock().unwrap().revoked = true;
    assert!(b.agent_save(&edit, true).is_err());
    assert_eq!(mutations(&s), 0);
    assert!(!b.view.available);
}
#[test]
fn revoked_mutation_reply_cannot_repopulate_editor() {
    let (mut b, s, workspace, row) = make();
    let edit = b
        .agent_edit_review(None, row.definition.clone(), &workspace)
        .unwrap();
    {
        let mut state = s.lock().unwrap();
        state.receipt = json!({"definition":row});
        state.mutation_revokes = true;
    }
    assert!(b.agent_save(&edit, true).is_err());
    assert!(!b.view.available);
    assert_eq!(mutations(&s), 1);
}
#[test]
fn cursor_must_end_at_final_row_and_cannot_loop_on_empty_pages() {
    let (mut b, s, _, row) = make();
    s.lock().unwrap().pages =
        vec![json!({"definitions":[],"next_after":"33333333-3333-4333-8333-333333333333"})];
    assert!(b.agent_catalog().is_err());
    s.lock().unwrap().pages =
        vec![json!({"definitions":[row],"next_after":"33333333-3333-4333-8333-333333333333"})];
    assert!(b.agent_catalog().is_err());
    s.lock().unwrap().pages = vec![
        json!({"definitions":[row],"next_after":row.id}),
        json!({"definitions":[],"next_after":null}),
    ];
    assert_eq!(b.agent_catalog().unwrap().definitions.len(), 1);
}
#[test]
fn launch_creates_only_pending_and_does_not_grant_model_implicitly() {
    let (mut b, s, _, row) = make();
    let review = reviewed(&mut b, &s, &row);
    let launch = b
        .agent_editor()
        .unwrap()
        .launch(&review, "Read the reviewed workspace".into(), None)
        .unwrap();
    s.lock().unwrap().receipt = json!({"task":{"id":"33333333-3333-4333-8333-333333333333","revision":1,"workspace_id":row.definition.workspace_id,"input":"Read the reviewed workspace","state":{"kind":"pending"},"plan":[],"clarification_answers":[],"last_result":null,"model_steps":0,"model_bytes":0,"last_model_reply":null},"workspace":{"id":row.definition.workspace_id,"label":"Synthetic","capabilities":["read"],"resources":[],"policy_revision":1,"available":true,"output_boundary":"plaintext_private_temporary_files"},"definition":{"id":row.id,"revision":1},"replayed":false,"model_started":false});
    b.agent_launch(&launch, true).unwrap();
    let s = s.lock().unwrap();
    let posts = s
        .requests
        .iter()
        .filter(|v| v.0 == "POST")
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].1.ends_with("/launch"));
    assert!(posts[0].2.as_ref().unwrap().get("model_review").is_none());
}

#[test]
fn catalog_cannot_rebind_old_definition_after_workspace_reinitialization() {
    let (mut b, s, _, row) = make();
    s.lock().unwrap().pages = vec![json!({"definitions":[row],"next_after":null})];
    let catalog = b.agent_catalog().unwrap();
    s.lock().unwrap().revoked = true;
    b.initialize().unwrap();
    let old = catalog.review(&row).unwrap();
    assert!(b.agent_delete(&old, true).is_err());
    assert_eq!(mutations(&s), 0);
    let mut forged = row.clone();
    forged.definition.name = "Changed outside fetched page".into();
    assert!(catalog.review(&forged).is_err());
}
#[test]
fn cloned_catalog_and_creation_editor_cannot_replay_uncertain_writes() {
    let (mut b, s, workspace, row) = make();
    s.lock().unwrap().pages = vec![json!({"definitions":[row],"next_after":null})];
    let catalog = b.agent_catalog().unwrap();
    let old = catalog.review(&row).unwrap();
    s.lock().unwrap().receipt = json!({});
    assert!(b.agent_delete(&old, true).is_err());
    assert!(b
        .agent_delete(&catalog.clone().review(&row).unwrap(), true)
        .is_err());
    assert_eq!(mutations(&s), 1);
    let editor = b.agent_editor().unwrap();
    let create = editor
        .edit(None, row.definition.clone(), &workspace)
        .unwrap();
    assert!(b.agent_save(&create, true).is_err());
    let repeated = editor
        .clone()
        .edit(None, row.definition.clone(), &workspace)
        .unwrap();
    assert!(b.agent_save(&repeated, true).is_err());
    assert_eq!(mutations(&s), 2);
}

#[test]
fn schema10_exact_extension_selection_survives_unrelated_agent_edit() {
    use smith_native_bridge::extensions_bridge::{InstructionRef, InstructionSelection};
    let (mut b, s, w, mut row) = make();
    row.definition.extensions = InstructionSelection::Explicit {
        references: vec![InstructionRef {
            id: "33333333-3333-4333-8333-333333333333".into(),
            revision: 7,
        }],
    };
    let old = reviewed(&mut b, &s, &row);
    let mut changed = row.definition.clone();
    changed.name = "Renamed with exact instructions".into();
    let edit = b
        .agent_editor()
        .unwrap()
        .edit(Some(&old), changed.clone(), &w)
        .unwrap();
    let mut saved = row.clone();
    saved.definition = changed;
    saved.revision += 1;
    s.lock().unwrap().receipt = json!({"definition":saved});
    let receipt = b.agent_save(&edit, true).unwrap();
    assert_eq!(
        receipt.definition().definition.extensions,
        row.definition.extensions
    );
    let lock = s.lock().unwrap();
    let body = lock
        .requests
        .iter()
        .find(|(method, _, _)| method == "PUT")
        .unwrap()
        .2
        .as_ref()
        .unwrap();
    assert_eq!(
        body["definition"]["extensions"],
        json!({"mode":"explicit","references":[{"id":"33333333-3333-4333-8333-333333333333","revision":7}]})
    );
}
