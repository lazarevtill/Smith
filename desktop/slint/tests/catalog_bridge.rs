use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Bridge, Result, Transport},
    catalog_bridge::*,
};
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct State {
    session: u8,
    calls: Vec<(String, String, Option<Value>)>,
    replies: Vec<Value>,
    lose: bool,
    reject: bool,
}
struct Mock(Arc<Mutex<State>>);
impl Transport for Mock {
    fn catalog_mutation(
        &mut self,
        m: &str,
        p: &str,
        session: &str,
        body: Value,
    ) -> std::result::Result<Value, CatalogMutationError> {
        if self.0.lock().unwrap().reject {
            return Err(CatalogMutationError::NotSubmitted(
                "409 stale catalog".into(),
            ));
        }
        self.request(m, p, Some(session), Some(body))
            .map_err(CatalogMutationError::OutcomeUnknown)
    }

    fn request(&mut self, m: &str, p: &str, _: Option<&str>, body: Option<Value>) -> Result<Value> {
        let mut s = self.0.lock().unwrap();
        s.calls.push((m.into(), p.into(), body));
        if p == "/api/profile/status" {
            return Ok(json!({"state":"unlocked","session":format!("s{}",s.session)}));
        }
        if p.starts_with("/api/local-catalog") {
            if m != "GET" && s.lose {
                return Err("Synthetic lost reply".into());
            }
            return Ok(s.replies.remove(0));
        }
        Ok(match p {
            "/api/config" => {
                json!({"revision":"r","provider":"mock","endpoint":"","model":"smith-demo","tested":false})
            }
            "/api/chats" | "/api/media/artifacts" => json!([]),
            "/api/agent/tasks" => json!({"tasks":[]}),
            "/api/trash" => json!({"chats":[]}),
            "/api/actions" => json!({"revision":0}),
            "/api/drafts/new" => json!({"owner":"new","version":0,"text":""}),
            _ if p.starts_with("/api/models?") => json!({"revision":"r","models":[]}),
            _ => panic!("Unexpected {m} {p}"),
        })
    }
}
fn setup() -> (Bridge<Mock>, Arc<Mutex<State>>) {
    let s = Arc::new(Mutex::new(State::default()));
    let mut b = Bridge::new(Mock(s.clone()));
    b.initialize().unwrap();
    (b, s)
}
fn token(n: u64) -> Value {
    json!({"revision":n,"sha256":if n==0{Value::Null}else{json!("a".repeat(64))}})
}
fn row() -> Value {
    json!({"id":"local-0000000000000001","revision":1,"label":"Saved model","trashed":false,"format":"gguf","declared_capabilities":["text_chat"],"origin":"external_user_selected","completeness":"selected_files_only","selected_for_review":false})
}
fn list(rows: Vec<Value>, n: u64, selected: Option<&str>) -> Value {
    json!({"catalog":token(n),"entries":rows,"selected_for_review":selected,"operations":[],"prepared":null,"inspection":"stored_metadata_only","automatic_load":false,"hints_applied":false})
}
fn page(b: &mut Bridge<Mock>, s: &Arc<Mutex<State>>, v: Value) -> CatalogPage {
    s.lock().unwrap().replies.push(v);
    b.catalog_list(false).unwrap()
}
fn receipt(m: &CatalogMutation, mut e: Value, operation: &str, n: u64) -> Value {
    e["revision"] = json!(n);
    json!({"receipt":{"request_id":m.request_id(),"catalog_revision":n,"entry":e,"operation":operation},"catalog":token(n),"execution":"none","model_files_changed":false,"replayed":false})
}
fn operation(kind: &str, req: Option<&str>, n: u64) -> Value {
    json!({"id":"22222222-2222-4222-8222-222222222222","kind":kind,"status":"running","request_id":req,"request_sha256":req.map(|_|"b".repeat(64)),"submitted_catalog":token(n),"entry_id":if kind=="register"{Value::Null}else{json!("local-0000000000000001")},"cancel_requested":false,"worker_drained":false,"result":null,"error":null})
}
fn writes(s: &Arc<Mutex<State>>) -> usize {
    s.lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(m, p, _)| m != "GET" && p.starts_with("/api/local-catalog"))
        .count()
}
#[test]
fn passive_list_membership_and_session_are_bound() {
    let (mut b, s) = setup();
    let original = b.view.config.clone();
    let p = page(&mut b, &s, list(vec![row()], 1, None));
    assert_eq!(writes(&s), 0);
    assert_eq!(b.view.config, original);
    let mut changed = p.entries[0].clone();
    changed.label = "Other".into();
    assert!(p.review(&changed).is_err());
    let review = p.review(&p.entries[0]).unwrap();
    s.lock().unwrap().session = 1;
    b.initialize().unwrap();
    assert!(b.catalog_submit(&review.trash().unwrap()).is_err());
    assert_eq!(writes(&s), 0);
}
#[test]
fn missing_digest_duplicate_rows_and_execution_flags_fail_closed() {
    let (mut b, s) = setup();
    for mut v in [
        list(vec![], 0, None),
        list(vec![row(), row()], 1, None),
        list(vec![], 0, None),
    ] {
        if v["entries"].as_array().unwrap().is_empty() && v["catalog"].get("sha256").is_some() {
            v["catalog"].as_object_mut().unwrap().remove("sha256");
        }
        s.lock().unwrap().replies.push(v);
        assert!(b.catalog_list(false).is_err());
    }
    let mut v = list(vec![], 0, None);
    v["automatic_load"] = json!(true);
    s.lock().unwrap().replies.push(v);
    assert!(b.catalog_list(false).is_err());
}
#[test]
fn lost_mutation_get_recovery_never_resubmits_or_changes_target() {
    let (mut b, s) = setup();
    let p = page(&mut b, &s, list(vec![row()], 1, None));
    let m = p
        .review(&p.entries[0])
        .unwrap()
        .rename("Renamed".into())
        .unwrap();
    s.lock().unwrap().lose = true;
    assert!(b.catalog_submit(&m).is_err());
    let mut e = row();
    e["label"] = json!("Renamed");
    let result = receipt(&m, e, "rename_v1", 2);
    s.lock().unwrap().replies.push(json!({"catalog":token(5),"receipt":{"request_id":m.request_id(),"request_sha256":"c".repeat(64),"result":result["receipt"]},"operation":null,"missing_receipt_is_not_success":true}));
    assert!(matches!(
        b.catalog_recover(&m).unwrap(),
        MutationOutcome::Committed(_)
    ));
    assert_eq!(writes(&s), 1);
    assert_eq!(b.view.config.model, "smith-demo");
}
#[test]
fn exact_retry_reuses_uuid_and_body_and_refuses_changed_receipt() {
    let (mut b, s) = setup();
    let p = page(&mut b, &s, list(vec![row()], 1, None));
    let m = p.review(&p.entries[0]).unwrap().trash().unwrap();
    s.lock().unwrap().lose = true;
    assert!(b.catalog_submit(&m).is_err());
    s.lock().unwrap().lose = false;
    let mut e = row();
    e["trashed"] = json!(true);
    s.lock()
        .unwrap()
        .replies
        .push(receipt(&m, e, "trash_v1", 2));
    b.catalog_submit(&m).unwrap();
    let c = s.lock().unwrap();
    let requests = c
        .calls
        .iter()
        .filter(|(m, p, _)| m == "DELETE" && p.starts_with("/api/local-catalog"))
        .collect::<Vec<_>>();
    assert_eq!(requests[0], requests[1]);
    drop(c);
    let mut wrong = receipt(&m, row(), "trash_v1", 2);
    wrong["receipt"]["request_id"] = json!("33333333-3333-4333-8333-333333333333");
    s.lock().unwrap().replies.push(wrong);
    assert!(b.catalog_submit(&m).is_err());
}
#[test]
fn passive_selection_keeps_entry_revision_and_never_loads() {
    let (mut b, s) = setup();
    let p = page(&mut b, &s, list(vec![row()], 4, None));
    let m = p.review(&p.entries[0]).unwrap().select().unwrap();
    let mut e = row();
    e["selected_for_review"] = json!(true);
    let mut r = receipt(&m, e, "select_v1", 5);
    r["receipt"]["entry"]["revision"] = json!(1);
    s.lock().unwrap().replies.push(r);
    b.catalog_submit(&m).unwrap();
    assert_eq!(writes(&s), 1);
    assert_eq!(b.view.config.model, "smith-demo");
}
#[test]
fn async_identity_and_drain_are_truthful() {
    let (mut b, s) = setup();
    let mut op = operation("prepare", None, 1);
    let mut v = list(vec![row()], 1, None);
    v["operations"] = json!([op]);
    let p = page(&mut b, &s, v);
    let o = p.operation("22222222-2222-4222-8222-222222222222").unwrap();
    op["status"] = json!("failed");
    op["error"] = json!("Cleanup not confirmed");
    s.lock().unwrap().replies.push(json!({"operation":op}));
    let failed = b.catalog_poll(&o).unwrap();
    assert!(!failed.snapshot().terminal());
    op["worker_drained"] = json!(true);
    s.lock().unwrap().replies.push(json!({"operation":op}));
    let terminal = b.catalog_poll(&failed).unwrap();
    assert!(terminal.snapshot().terminal());
    op["status"] = json!("running");
    op["worker_drained"] = json!(false);
    s.lock().unwrap().replies.push(json!({"operation":op}));
    assert!(b.catalog_poll(&terminal).is_err());
    drop(p);
}
#[test]
fn prepare_refusal_and_unknown_receipt_watermark_are_distinct() {
    let (mut b, s) = setup();
    let mut e = row();
    e["selected_for_review"] = json!(true);
    let mut before = list(vec![e], 2, Some("local-0000000000000001"));
    let mut prior = operation("prepare", None, 2);
    before["operations"] = json!([prior]);
    let p = page(&mut b, &s, before.clone());
    let review = p.prepare_review(&p.entries[0], &b.view.config).unwrap();
    s.lock().unwrap().replies.push(before.clone());
    s.lock().unwrap().lose = true;
    let recovery = match b.catalog_prepare(&review) {
        Err(PrepareFailure::OutcomeUnknown { recovery, .. }) => recovery,
        _ => panic!("Must retain unknown outcome"),
    };
    s.lock().unwrap().replies.push(before.clone());
    assert!(b.catalog_recover_prepare(&recovery).is_err());
    prior["id"] = json!("44444444-4444-4444-8444-444444444444");
    before["operations"].as_array_mut().unwrap().push(prior);
    s.lock().unwrap().replies.push(before);
    assert_eq!(
        b.catalog_recover_prepare(&recovery).unwrap().snapshot().id,
        "44444444-4444-4444-8444-444444444444"
    );
    assert_eq!(writes(&s), 1);
    b.view.config.revision = "changed".into();
    assert!(matches!(
        b.catalog_prepare(&review),
        Err(PrepareFailure::NotSubmitted(_))
    ));
    assert_eq!(writes(&s), 1);
}
#[test]
fn setup_handoff_uses_current_settings_without_hints_and_is_single_attempt() {
    let (mut b, s) = setup();
    let mut e = row();
    e["label"] = json!("Qwen 0.6B · Локальная модель");
    e["selected_for_review"] = json!(true);
    let mut v = list(vec![e], 2, Some("local-0000000000000001"));
    v["prepared"] = json!({"id":"d".repeat(64),"catalog_revision":2,"entry_id":"local-0000000000000001","entry_revision":1,"manifest_sha256":"c".repeat(64),"context":{"session_id":"s0","config_revision":"r","inventory_generation":"fresh-observation"}});
    let p = page(&mut b, &s, v.clone());
    let review = p.prepared_review(&b.view.config).unwrap();
    b.view.settings.setup = json!({"preferences":{"revision":7}});
    let settings = SetupSettings::Gguf {
        reviewed: b.settings_review().unwrap(),
        model_id: review.runtime_model_id().to_owned(),
    };
    s.lock().unwrap().replies = vec![
        v.clone(),
        json!({"operation_id":"55555555-5555-4555-8555-555555555555"}),
    ];
    assert_eq!(
        b.catalog_setup(&review, settings.clone()).unwrap().adapter,
        SetupAdapter::Gguf
    );
    s.lock().unwrap().replies = vec![v];
    assert!(b.catalog_setup(&review, settings).is_err());
    assert_eq!(writes(&s), 1);
    let state = s.lock().unwrap();
    let body = state
        .calls
        .iter()
        .find(|(_, p, _)| p == "/api/local-catalog/setup")
        .unwrap()
        .2
        .as_ref()
        .unwrap();
    assert_eq!(body["model_id"], "local-0000000000000001");
    assert_eq!(body["settings_revision"], 7);
    assert!(body.get("hints").is_none());
    assert!(body.get("confirm").is_none());
}

#[test]
fn controller_retains_unknown_request_across_refresh_and_get_only_inspection() {
    use smith_native_bridge::catalog_controller::CatalogPanel;
    let (mut b, s) = setup();
    let mut panel = CatalogPanel::default();
    s.lock().unwrap().replies.push(list(vec![row()], 1, None));
    panel.refresh(&mut b, false).unwrap();
    let p = panel.page.as_ref().unwrap();
    let m = p
        .review(&p.entries[0])
        .unwrap()
        .rename("Renamed".into())
        .unwrap();
    s.lock().unwrap().lose = true;
    assert!(panel.submit(&mut b, m.clone()).is_err());
    assert!(panel.blocked());
    s.lock().unwrap().replies.push(list(vec![row()], 1, None));
    panel.refresh(&mut b, false).unwrap();
    assert!(panel.blocked());
    let mut e = row();
    e["label"] = json!("Renamed");
    let result = receipt(&m, e.clone(), "rename_v1", 2);
    e["revision"] = json!(2);
    s.lock().unwrap().replies = vec![
        json!({"catalog":token(2),"receipt":{"request_id":m.request_id(),"request_sha256":"a".repeat(64),"result":result["receipt"]},"operation":null,"missing_receipt_is_not_success":true}),
        list(vec![e], 2, None),
    ];
    panel.inspect(&mut b).unwrap();
    assert!(!panel.blocked());
    assert!(panel.pending.is_none());
    assert_eq!(writes(&s), 1);
}
#[test]
fn controller_handles_already_terminal_cancellation_receipt_without_sticking() {
    use smith_native_bridge::catalog_controller::CatalogPanel;
    let (mut b, s) = setup();
    let mut e = row();
    e["selected_for_review"] = json!(true);
    let before = list(vec![e], 2, Some("local-0000000000000001"));
    let p = page(&mut b, &s, before.clone());
    let review = p.prepare_review(&p.entries[0], &b.view.config).unwrap();
    let mut op = operation("prepare", None, 2);
    op["status"] = json!("cancelled");
    op["worker_drained"] = json!(true);
    s.lock().unwrap().replies = vec![before, json!({"operation_id":op["id"],"operation":op})];
    let mut panel = CatalogPanel::default();
    panel.prepare(&mut b, review).unwrap();
    panel.poll(&mut b).unwrap();
    assert!(!panel.blocked());
    assert!(panel.operation.is_none());
    assert!(panel.prepared.is_none());
    assert_eq!(writes(&s), 1);
}
#[test]
fn path_capture_is_explicit_bounded_and_never_reads_or_applies_hints() {
    let selection = gguf_selection(
        std::path::Path::new("/nonexistent/runtime"),
        std::path::Path::new("/nonexistent/model.gguf"),
    )
    .unwrap();
    assert_eq!(selection.runtime.files[0].name, "runtime");
    assert_eq!(selection.model.files[0].role, FileRole::Weights);
    assert_eq!(selection.description.architecture, "unspecified");
    assert_eq!(
        selection.description.hints,
        AdapterHints::LlamaGguf(GgufHints::default())
    );
    assert!(gguf_selection(
        std::path::Path::new("relative"),
        std::path::Path::new("/model")
    )
    .is_err());
    let (mut b, s) = setup();
    let p = page(&mut b, &s, list(vec![], 0, None));
    let mut invalid = selection;
    invalid.model.files[0].name = "".into();
    assert!(p.editor().register("Model".into(), invalid).is_err());
    assert_eq!(writes(&s), 0);
}

#[test]
fn initial_authoritative_refusal_recovers_but_retry_refusal_cannot_clear_unknown() {
    use smith_native_bridge::catalog_controller::CatalogPanel;
    let (mut b, s) = setup();
    let mut panel = CatalogPanel::default();
    s.lock().unwrap().replies.push(list(vec![row()], 1, None));
    panel.refresh(&mut b, false).unwrap();
    let p = panel.page.as_ref().unwrap();
    let m = p.review(&p.entries[0]).unwrap().trash().unwrap();
    s.lock().unwrap().reject = true;
    assert!(panel.submit(&mut b, m.clone()).is_err());
    assert!(!panel.blocked());
    {
        let mut s = s.lock().unwrap();
        s.reject = false;
        s.lose = true;
    }
    assert!(panel.submit(&mut b, m).is_err());
    assert!(panel.blocked());
    s.lock().unwrap().reject = true;
    assert!(panel.retry(&mut b).is_err());
    assert!(panel.blocked());
    assert!(panel.pending.is_some());
}
#[test]
fn foreign_saved_paths_remain_passive_browsable_metadata() {
    let (mut b, s) = setup();
    let p = page(&mut b, &s, list(vec![row()], 1, None));
    let r = p.review(&p.entries[0]).unwrap();
    let source =
        json!({"label":"user selected","revision":"explicit","origin":"external_user_selected"});
    let package = |name: &str, role: &str| json!({"root":"C:\\Selected","source":source,"completeness":"selected_files_only","files":[{"name":name,"role":role,"bytes":17,"sha256":"a".repeat(64)}]});
    let selection = gguf_selection(
        std::path::Path::new("/runtime"),
        std::path::Path::new("/model.gguf"),
    )
    .unwrap();
    s.lock().unwrap().replies.push(json!({"catalog":token(1),"inspection":"stored_metadata_only","file_availability":"not_checked","entry":{"id":r.entry().id,"revision":1,"label":"Saved model","trashed":false,"manifest":{"description":selection.description,"runtime":package("runtime.exe","executable"),"model":package("model.gguf","weights"),"sha256":"b".repeat(64)}}}));
    let d = b.catalog_details(&r).unwrap();
    assert_eq!(
        d.entry.manifest.runtime.root.to_str().unwrap(),
        "C:\\Selected"
    );
    assert_eq!(writes(&s), 0);
}

#[test]
fn missing_inventory_refusal_does_not_trap_preparation_recovery() {
    use smith_native_bridge::catalog_controller::CatalogPanel;
    let (mut b, s) = setup();
    let mut e = row();
    e["selected_for_review"] = json!(true);
    let v = list(vec![e], 2, Some("local-0000000000000001"));
    let p = page(&mut b, &s, v.clone());
    let review = p.prepare_review(&p.entries[0], &b.view.config).unwrap();
    let mut panel = CatalogPanel {
        page: Some(p),
        ..Default::default()
    };
    {
        let mut s = s.lock().unwrap();
        s.replies.push(v);
        s.reject = true;
    }
    assert!(panel.prepare(&mut b, review).is_err());
    assert!(!panel.blocked());
    assert!(panel.prepare_recovery.is_none());
    assert!(panel.operation.is_none());
}

#[test]
fn setup_authoritative_refusal_releases_review_but_unknown_does_not() {
    let (mut b, s) = setup();
    let mut e = row();
    e["selected_for_review"] = json!(true);
    let mut v = list(vec![e], 2, Some("local-0000000000000001"));
    v["prepared"] = json!({"id":"d".repeat(64),"catalog_revision":2,"entry_id":"local-0000000000000001","entry_revision":1,"manifest_sha256":"c".repeat(64),"context":{"session_id":"s0","config_revision":"r","inventory_generation":"observed"}});
    let p = page(&mut b, &s, v.clone());
    let review = p.prepared_review(&b.view.config).unwrap();
    b.view.settings.setup = json!({"preferences":{"revision":7}});
    let settings = SetupSettings::Gguf {
        reviewed: b.settings_review().unwrap(),
        model_id: review.runtime_model_id().into(),
    };
    {
        let mut s = s.lock().unwrap();
        s.replies.push(v.clone());
        s.reject = true;
    }
    assert!(matches!(
        b.catalog_setup_checked(&review, settings.clone()),
        Err(SetupFailure::NotSubmitted(_))
    ));
    {
        let mut s = s.lock().unwrap();
        s.replies.push(v.clone());
        s.reject = false;
        s.lose = true;
    }
    assert!(matches!(
        b.catalog_setup_checked(&review, settings.clone()),
        Err(SetupFailure::OutcomeUnknown(_))
    ));
    s.lock().unwrap().replies.push(v);
    assert!(matches!(
        b.catalog_setup_checked(&review, settings),
        Err(SetupFailure::NotSubmitted(_))
    ));
    assert_eq!(writes(&s), 1);
}
