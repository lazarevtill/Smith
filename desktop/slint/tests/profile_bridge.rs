//! Pure transport tests: no service, model, original profile, or keychain.
use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Bridge, Result, Transport},
    profile_bridge::*,
    task_bridge::new_task_request_id,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "smith-native-import-pure-{}",
            new_task_request_id().unwrap()
        ));
        std::fs::create_dir(&p).unwrap();
        for n in ["host", "source", "managed"] {
            std::fs::create_dir(p.join(n)).unwrap();
        }
        std::fs::write(
            p.join("source/smith.db"),
            b"synthetic metadata fixture only",
        )
        .unwrap();
        Self(p)
    }
    fn selection(&self) -> SourceSelection {
        SourceSelection::new(&self.0.join("source"), &self.0.join("managed"), true).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
#[derive(Default)]
struct State {
    status: Value,
    posts: Vec<(String, bool)>,
    uncertain: bool,
}
struct Mock(Arc<Mutex<State>>);
impl Transport for Mock {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        _session: Option<&str>,
        _body: Option<Value>,
    ) -> Result<Value> {
        assert_eq!(method, "GET");
        let state = self.0.lock().unwrap();
        match path {
            "/api/profile/status" => Ok(state.status.clone()),
            "/api/config" => {
                Ok(json!({"revision":"c","provider":"mlx","endpoint":"","model":"","tested":false}))
            }
            "/api/chats" | "/api/media/artifacts" | "/api/agent/tasks" => Ok(json!([])),
            "/api/trash" => Ok(json!({"chats":[]})),
            "/api/actions" => Ok(json!({"revision":0,"undo":null,"redo":null})),
            "/api/drafts/new" => {
                Ok(json!({"owner":"new","version":0,"text":"","attachments":[],"media":[]}))
            }
            _ => panic!("Unexpected nonsecret test route: {path}"),
        }
    }
    fn sensitive_request(
        &mut self,
        method: &str,
        path: &str,
        _session: Option<&str>,
        body: SensitiveBody,
    ) -> Result<Value> {
        assert_eq!(method, "POST");
        let value = body.value();
        assert_eq!(value["confirm"], true);
        assert_eq!(value["offline_copy_attested"], true);
        assert!(value.get("passphrase").is_none());
        let keyed = value.get("original_key").is_some();
        if keyed {
            match value["original_key"]["format"].as_str().unwrap() {
                "master" => assert_eq!(
                    value["original_key"]["master_hex"].as_str().unwrap().len(),
                    64
                ),
                "derived" => {
                    assert!(!value["original_key"]["database_passphrase"]
                        .as_str()
                        .unwrap()
                        .is_empty());
                    assert_eq!(
                        value["original_key"]["artifact_key_hex"]
                            .as_str()
                            .unwrap()
                            .len(),
                        64
                    );
                    assert!(value["original_key"].get("master_hex").is_none());
                }
                _ => panic!("Unexpected original credential format"),
            }
        }
        let mut state = self.0.lock().unwrap();
        state.posts.push((path.to_owned(), keyed));
        if state.uncertain {
            Err("Simulated dropped acknowledgement".into())
        } else {
            Ok(json!({"operation_id":format!("op-{}",state.posts.len())}))
        }
    }
}
fn key() -> OriginalKey {
    OriginalKey::master("1".repeat(64)).unwrap()
}
fn status(revision: &str, session: &str, selection: &str, operation: Value) -> Value {
    json!({"state":"unlocked","revision":revision,"session":session,"selection":selection,"automatic":true,"encrypted":true,"operation":operation,"error":null})
}
fn report() -> Value {
    json!({"import_id":"synthetic-import","profile_id":null,"source_schema":25,"target_schema":8,"source_tables":15,"source_rows":30,"source_files":4,"verified_blobs":2,"external_stores":1,"external_artifacts":1,"mapped_chats":1,"mapped_messages":6,"mapped_artifacts":2,"mapped_tasks":1,"projection_warnings":2,"retained_tables":15,"retained_rows":30,"retained_schema_objects":22,"mapped_domains":["conversation_graphs"],"dormant_domains":["tools","schedules"],"verified":true,"baseline_unchanged":true,"later_writes":0,"rollback_eligible":false})
}
fn complete(state: &Arc<Mutex<State>>, id: &str, kind: &str, result: Value) {
    state.lock().unwrap().status = status(
        "r2",
        "s2",
        "default",
        json!({"id":id,"kind":kind,"status":"succeeded","error":null,"result":result}),
    );
}
fn setup() -> (Temp, Arc<Mutex<State>>, Bridge<Mock>, ProfileWorkflow) {
    let temp = Temp::new();
    let state = Arc::new(Mutex::new(State {
        status: status("r1", "s1", "default", Value::Null),
        ..Default::default()
    }));
    let bridge = Bridge::new(Mock(state.clone()));
    let workflow = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    (temp, state, bridge, workflow)
}
fn inspected(
    temp: &Temp,
    state: &Arc<Mutex<State>>,
    bridge: &mut Bridge<Mock>,
    workflow: &mut ProfileWorkflow,
) -> ProfileReview {
    workflow.inspect(bridge, temp.selection(), key()).unwrap();
    complete(state, "op-1", "legacy-preflight", report());
    workflow.status(bridge).unwrap();
    workflow.review().unwrap()
}
#[test]
fn inspection_revokes_before_post_exact_operation_then_one_use_review() {
    let (t, s, mut b, mut w) = setup();
    b.view.available = true;
    b.view.draft.text = "Old visible draft".into();
    let ticket = w.inspect(&mut b, t.selection(), key()).unwrap();
    assert_eq!(ticket.id, "op-1");
    assert!(!b.view.available);
    assert!(b.view.draft.text.is_empty());
    complete(&s, "wrong", "legacy-preflight", report());
    assert!(w.status(&mut b).is_err());
    assert!(w.review().is_err());
    complete(&s, "op-1", "legacy-preflight", report());
    let status = w.status(&mut b).unwrap();
    assert_eq!(
        status
            .operation
            .unwrap()
            .import_report()
            .unwrap()
            .mapped_messages,
        6
    );
    let review = w.review().unwrap();
    let duplicate = review.clone();
    w.execute(&mut b, review, key()).unwrap();
    assert!(w.execute(&mut b, duplicate, key()).is_err());
    assert_eq!(s.lock().unwrap().posts.len(), 2);
}
#[test]
fn stale_revision_or_session_consumes_review_without_import() {
    for (r, sess) in [("new-revision", "s2"), ("r2", "new-session")] {
        let (t, s, mut b, mut w) = setup();
        let review = inspected(&t, &s, &mut b, &mut w);
        let duplicate = review.clone();
        s.lock().unwrap().status = status(r, sess, "default", Value::Null);
        assert!(w.execute(&mut b, review, key()).is_err());
        assert!(w.execute(&mut b, duplicate, key()).is_err());
        assert_eq!(s.lock().unwrap().posts.len(), 1);
    }
}
#[test]
fn dropped_acknowledgement_never_creates_review_or_retries() {
    let (t, s, mut b, mut w) = setup();
    s.lock().unwrap().uncertain = true;
    assert!(w.inspect(&mut b, t.selection(), key()).is_err());
    complete(&s, "unknown", "legacy-preflight", report());
    w.status(&mut b).unwrap();
    assert!(w.review().is_err());
    assert!(w.inspect(&mut b, t.selection(), key()).is_err());
    assert_eq!(s.lock().unwrap().posts.len(), 1);
    w.open(&mut b).unwrap();
    assert!(b.view.available);
    assert!(w.review().is_err());
}
#[test]
fn later_external_session_cannot_rebind_old_inspection() {
    let (t, s, mut b, mut w) = setup();
    let _ = inspected(&t, &s, &mut b, &mut w);
    s.lock().unwrap().status = status("r3", "s3", "imported", Value::Null);
    w.status(&mut b).unwrap();
    assert!(w.review().is_err());
}
#[test]
fn failed_preflight_can_open_but_never_earns_execution_review() {
    let (t, s, mut b, mut w) = setup();
    w.inspect(&mut b, t.selection(), key()).unwrap();
    s.lock().unwrap().status = status(
        "r2",
        "s2",
        "default",
        json!({"id":"op-1","kind":"legacy-preflight","status":"failed","error":"Wrong key or unsupported copy.","result":null}),
    );
    assert!(w.status(&mut b).unwrap().operation.unwrap().terminal());
    assert!(w.review().is_err());
    w.open(&mut b).unwrap();
    assert!(b.view.available);
}
#[test]
fn rollback_uses_no_original_or_output_key_and_is_one_use() {
    let (t, s, mut b, mut w) = setup();
    assert!(w.review_rollback(&mut b, t.selection()).is_err());
    s.lock().unwrap().status = status("r2", "s2", "imported", Value::Null);
    let review = w.review_rollback(&mut b, t.selection()).unwrap();
    assert_eq!(review.action(), ReviewAction::Deactivate);
    let duplicate = review.clone();
    w.rollback(&mut b, review).unwrap();
    assert!(w.rollback(&mut b, duplicate).is_err());
    assert_eq!(
        s.lock().unwrap().posts.as_slice(),
        [("/api/profile/legacy-rollback".into(), false)]
    );
}
#[test]
fn dirty_drafts_block_profile_operations_without_discard() {
    let (t, s, mut b, mut w) = setup();
    b.view.draft.dirty = true;
    b.view.draft.text = "Unsaved native draft".into();
    assert!(w.inspect(&mut b, t.selection(), key()).is_err());
    assert_eq!(b.view.draft.text, "Unsaved native draft");
    assert!(s.lock().unwrap().posts.is_empty());
}
#[test]
fn copy_attestation_owned_root_alias_hardlink_and_replacement_are_rejected() {
    let (t, s, mut b, mut w) = setup();
    assert!(SourceSelection::new(&t.0.join("source"), &t.0.join("managed"), false).is_err());
    std::fs::write(t.0.join("host/smith.db"), b"owned live marker").unwrap();
    let live = SourceSelection::new(&t.0.join("host"), &t.0.join("managed"), true).unwrap();
    assert!(w.inspect(&mut b, live, key()).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(t.0.join("source"), t.0.join("alias")).unwrap();
        assert!(SourceSelection::new(&t.0.join("alias"), &t.0.join("managed"), true).is_err());
        std::fs::hard_link(t.0.join("source/smith.db"), t.0.join("hardlink")).unwrap();
        assert!(SourceSelection::new(&t.0.join("source"), &t.0.join("managed"), true).is_err());
        std::fs::remove_file(t.0.join("hardlink")).unwrap();
    }
    let selected = t.selection();
    std::fs::rename(t.0.join("source"), t.0.join("old-source")).unwrap();
    std::fs::create_dir(t.0.join("source")).unwrap();
    std::fs::write(t.0.join("source/smith.db"), b"replacement").unwrap();
    assert!(w.inspect(&mut b, selected, key()).is_err());
    assert!(s.lock().unwrap().posts.is_empty());
}
#[test]
fn invalid_keys_and_unqualified_transports_fail_closed() {
    assert!(
        OriginalKey::master("invalid secret must not be reflected".into())
            .err()
            .unwrap()
            .contains("64")
    );
    assert!(OriginalKey::derived("".into(), "2".repeat(64)).is_err());
    struct Unqualified;
    impl Transport for Unqualified {
        fn request(
            &mut self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<Value>,
        ) -> Result<Value> {
            Ok(status("r1", "s1", "default", Value::Null))
        }
    }
    let temp = Temp::new();
    let mut w = ProfileWorkflow::new(&temp.0.join("host")).unwrap();
    let mut b = Bridge::new(Unqualified);
    assert!(w.inspect(&mut b, temp.selection(), key()).is_err());
    assert!(!b.view.available);
}

#[test]
fn future_report_schema_is_not_an_execution_review() {
    let (t, s, mut b, mut w) = setup();
    w.inspect(&mut b, t.selection(), key()).unwrap();
    let mut future = report();
    future["target_schema"] = json!(12);
    complete(&s, "op-1", "legacy-preflight", future);
    assert!(w.status(&mut b).is_err());
    assert!(w.review().is_err());
}

#[test]
fn separate_original_keys_are_explicit_and_not_treated_as_master() {
    let (t, s, mut b, mut w) = setup();
    let key =
        OriginalKey::derived("Synthetic database-only passphrase".into(), "2".repeat(64)).unwrap();
    w.inspect(&mut b, t.selection(), key).unwrap();
    assert_eq!(s.lock().unwrap().posts.len(), 1);
}

#[test]
fn explicitly_qualified_current_and_historical_import_reports_are_accepted() {
    for target in [7, 8, 9, 10, 11] {
        let (t, s, mut b, mut w) = setup();
        w.inspect(&mut b, t.selection(), key()).unwrap();
        let mut supported = report();
        supported["target_schema"] = json!(target);
        complete(&s, "op-1", "legacy-preflight", supported);
        w.status(&mut b).unwrap();
        assert_eq!(w.review().unwrap().report().unwrap().target_schema, target);
    }
}
