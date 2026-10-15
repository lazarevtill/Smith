use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Bridge, Result, Transport},
    extensions_bridge::*,
};
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct State {
    session: u8,
    requests: Vec<(String, String, Option<Value>)>,
    replies: Vec<Value>,
    lose: bool,
}
struct Mock(Arc<Mutex<State>>);
impl Transport for Mock {
    fn request(&mut self, m: &str, p: &str, _: Option<&str>, body: Option<Value>) -> Result<Value> {
        let mut s = self.0.lock().unwrap();
        s.requests.push((m.into(), p.into(), body));
        if p == "/api/profile/status" {
            return Ok(json!({"state":"unlocked","session":format!("s{}",s.session)}));
        }
        if p.starts_with("/api/agent/extensions") || p.starts_with("/api/legacy-import/records/") {
            if m != "GET" && s.lose {
                return Err("Synthetic lost response".into());
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
fn row() -> ExtensionRecord {
    serde_json::from_value(json!({"id":"22222222-2222-4222-8222-222222222222","revision":1,"kind":"skill","name":"Scoped instruction","description":"Synthetic","enabled":true,"deleted":false,"instruction_bytes":9,"execution_status":"instruction_data","provenance":{"kind":"authored"},"created_at":1,"updated_at":1,"definition":{"kind":"skill","name":"Scoped instruction","description":"Synthetic","instructions":"Read only","category":""}})).unwrap()
}
fn review(b: &mut Bridge<Mock>, s: &Arc<Mutex<State>>, r: &ExtensionRecord) -> ExtensionReview {
    s.lock().unwrap().replies = vec![
        json!({"extensions":[r.summary],"next_after":null}),
        json!({"extension":r}),
    ];
    let c = b.extension_catalog().unwrap();
    b.extension_open(&c.select(&c.rows[0]).unwrap()).unwrap()
}
fn writes(s: &Arc<Mutex<State>>) -> usize {
    s.lock()
        .unwrap()
        .requests
        .iter()
        .filter(|(m, p, _)| m != "GET" && p.starts_with("/api/agent/extensions"))
        .count()
}
#[test]
fn no_op_save_and_exact_changed_revision_are_validated() {
    let (mut b, s) = setup();
    let r = row();
    let old = review(&mut b, &s, &r);
    let e = b
        .extension_editor()
        .unwrap()
        .edit(Some(&old), r.definition.clone(), true)
        .unwrap();
    s.lock().unwrap().replies = vec![json!({"extension":r})];
    assert_eq!(b.extension_save(&e).unwrap().record(), &r);
    assert!(b.extension_save(&e).is_err());
    assert_eq!(writes(&s), 1);
    let old = review(&mut b, &s, &r);
    let e = b
        .extension_editor()
        .unwrap()
        .edit(Some(&old), r.definition.clone(), false)
        .unwrap();
    s.lock().unwrap().replies = vec![json!({"extension":r})];
    assert!(b.extension_save(&e).is_err());
}
#[test]
fn trash_restore_preserve_enabled_preference_and_instruction_text() {
    let (mut b, s) = setup();
    let r = row();
    let old = review(&mut b, &s, &r);
    let mut trashed = r.clone();
    trashed.summary.deleted = true;
    trashed.summary.revision = 2;
    s.lock().unwrap().replies = vec![json!({"extension":trashed,"task_history_preserved":true})];
    let receipt = b.extension_trash(&old).unwrap();
    let mut restored = r.clone();
    restored.summary.revision = 3;
    s.lock().unwrap().replies = vec![json!({"extension":restored,"task_history_preserved":true})];
    assert_eq!(b.extension_restore(&receipt).unwrap().record(), &restored);
    assert_eq!(writes(&s), 2);
}
#[test]
fn old_catalog_cannot_rebind_after_workspace_reinitialization() {
    let (mut b, s) = setup();
    let r = row();
    s.lock().unwrap().replies = vec![json!({"extensions":[r.summary],"next_after":null})];
    let c = b.extension_catalog().unwrap();
    s.lock().unwrap().session = 1;
    b.initialize().unwrap();
    assert!(b.extension_open(&c.select(&c.rows[0]).unwrap()).is_err());
    assert_eq!(writes(&s), 0);
}
#[test]
fn unknown_save_consumes_cloned_editor_and_record_authority() {
    let (mut b, s) = setup();
    let r = row();
    let editor = b.extension_editor().unwrap();
    let edit = editor.edit(None, r.definition.clone(), true).unwrap();
    s.lock().unwrap().lose = true;
    assert!(b.extension_save(&edit).is_err());
    assert!(b.extension_save(&edit.clone()).is_err());
    let again = editor.edit(None, r.definition.clone(), false).unwrap();
    assert!(b.extension_save(&again).is_err());
    assert_eq!(writes(&s), 1);
    s.lock().unwrap().lose = false;
    let old = review(&mut b, &s, &r);
    let clone = old.clone();
    s.lock().unwrap().lose = true;
    assert!(b.extension_trash(&old).is_err());
    assert!(b.extension_trash(&clone).is_err());
    assert_eq!(writes(&s), 2);
}
#[test]
fn malformed_empty_continuation_and_changed_definition_fail_closed() {
    let (mut b, s) = setup();
    s.lock().unwrap().replies =
        vec![json!({"extensions":[],"next_after":"22222222-2222-4222-8222-222222222222"})];
    assert!(b.extension_catalog().is_err());
    let r = row();
    let mut changed = r.clone();
    changed.summary.revision = 2;
    s.lock().unwrap().replies = vec![
        json!({"extensions":[r.summary],"next_after":null}),
        json!({"extension":changed}),
    ];
    let c = b.extension_catalog().unwrap();
    assert!(b.extension_open(&c.select(&c.rows[0]).unwrap()).is_err());
    assert_eq!(writes(&s), 0);
}
#[test]
fn definitions_reject_execution_substitution_and_selection_duplicates() {
    let mut r = row();
    r.summary.execution_status = "execute".into();
    let (mut b, s) = setup();
    s.lock().unwrap().replies = vec![json!({"extensions":[r.summary],"next_after":null})];
    assert!(b.extension_catalog().is_err());
    let selected = InstructionRef {
        id: r.summary.id,
        revision: 1,
    };
    assert!(InstructionSelection::Explicit {
        references: vec![selected.clone(), selected]
    }
    .validate()
    .is_err());
    let d = ExtensionDefinition::Recipe {
        name: "Recipe data".into(),
        description: String::new(),
        command: "echo {text}".into(),
        parameters: json!({"type":"object"}),
    };
    assert!(d.validate().is_ok());
    assert_eq!(d.kind(), ExtensionKind::Recipe);
    assert_eq!(d.instructions(), None);
}

fn plugin_review() -> Value {
    json!({"review_id":"33333333-3333-4333-8333-333333333333","source_sha256":"a".repeat(64),"definition":{"kind":"plugin","manifest_id":"synthetic.plugin","name":"Selected plugin","description":"Chosen directory only","version":"1","authors":[],"homepage":null,"license":null,"instructions":"Read only","instructions_file":"instructions.md","tools":["read_file"]},"provenance":{"kind":"selected_directory","source":{"directory_name":"chosen","directory_device":1,"directory_inode":2,"selection_sha256":"b".repeat(64),"files":[{"name":"plugin.json","bytes":128,"sha256":"c".repeat(64),"device":1,"inode":3},{"name":"instructions.md","bytes":9,"sha256":"d".repeat(64),"device":1,"inode":4}],"source_sha256":"a".repeat(64)}},"expires_in_seconds":300,"enabled_on_import":false,"execution":"none","trust":"explicit_local_selection_not_publisher_verification"})
}
fn import_receipt(review: &Value) -> Value {
    let d = &review["definition"];
    json!({"extension":{"id":"44444444-4444-4444-8444-444444444444","revision":1,"kind":d["kind"],"name":d["name"],"description":d["description"],"enabled":false,"deleted":false,"instruction_bytes":9,"execution_status":"instruction_data","provenance":review["provenance"],"created_at":1,"updated_at":1,"definition":d},"execution":"none","replayed":true})
}
#[test]
fn plugin_lost_reply_retains_exact_import_identity_for_explicit_retry() {
    let (mut b, s) = setup();
    let r = plugin_review();
    s.lock().unwrap().replies = vec![r.clone()];
    let editor = b.extension_editor().unwrap();
    let review = b
        .extension_plugin_review(&editor, std::path::Path::new("/synthetic/chosen"))
        .unwrap();
    assert!(!review.submitted());
    assert_eq!(review.definition().name(), "Selected plugin");
    s.lock().unwrap().lose = true;
    assert!(b.extension_import(&review, false).is_err());
    assert!(review.submitted());
    assert!(b.extension_import(&review.clone(), false).is_err());
    s.lock().unwrap().lose = false;
    s.lock().unwrap().replies = vec![import_receipt(&r)];
    let receipt = b.extension_import(&review, true).unwrap();
    assert!(!receipt.record().summary.enabled);
    let locked = s.lock().unwrap();
    let imports = locked
        .requests
        .iter()
        .filter(|(_, p, _)| p.ends_with("/plugin-import"))
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 2);
    assert_eq!(imports[0].2, imports[1].2);
    assert_eq!(
        imports[0].2.as_ref().unwrap()["request_id"],
        review.request_id()
    );
}
#[test]
fn plugin_review_and_import_refuse_changed_source_or_session() {
    let (mut b, s) = setup();
    let mut invalid = plugin_review();
    invalid["provenance"]["source"]["source_sha256"] = json!("f".repeat(64));
    s.lock().unwrap().replies = vec![invalid];
    let editor = b.extension_editor().unwrap();
    assert!(b
        .extension_plugin_review(&editor, std::path::Path::new("/synthetic/chosen"))
        .is_err());
    let r = plugin_review();
    s.lock().unwrap().replies = vec![r.clone()];
    let review = b
        .extension_plugin_review(&editor, std::path::Path::new("/synthetic/chosen"))
        .unwrap();
    let mut altered = import_receipt(&r);
    altered["extension"]["enabled"] = json!(true);
    s.lock().unwrap().replies = vec![altered];
    assert!(b.extension_import(&review, false).is_err());
    s.lock().unwrap().session = 1;
    b.initialize().unwrap();
    let before = writes(&s);
    assert!(b.extension_import(&review, true).is_err());
    assert!(b
        .extension_plugin_review(&editor, std::path::Path::new("/synthetic/chosen"))
        .is_err());
    assert_eq!(writes(&s), before);
}
#[test]
fn permanent_removal_requires_exact_trashed_review_and_preserved_receipts() {
    let (mut b, s) = setup();
    let mut r = row();
    let live = review(&mut b, &s, &r);
    assert!(b.extension_purge(&live, &r.summary.id).is_err());
    assert_eq!(writes(&s), 0);
    r.summary.deleted = true;
    r.summary.revision = 2;
    let trashed = review(&mut b, &s, &r);
    assert!(b.extension_purge(&trashed, "another id").is_err());
    s.lock().unwrap().replies = vec![
        json!({"id":r.summary.id,"purged":true,"task_history_preserved":true,"import_receipts_preserved":true}),
    ];
    b.extension_purge(&trashed, &r.summary.id).unwrap();
    assert!(b.extension_purge(&trashed, &r.summary.id).is_err());
    assert_eq!(writes(&s), 1);
}
#[test]
fn archived_extension_row_keeps_session_and_exact_import_provenance() {
    let (mut b, s) = setup();
    let editor = b.extension_editor().unwrap();
    s.lock().unwrap().replies = vec![
        json!({"table":"skills","offset":0,"total":1,"runtime_activation":"dormant","rows":[{"row_number":7,"id":"old","record":{"columns":["name"],"values":[{"type":"text","value":"Original skill"}]}}]}),
    ];
    let page = b
        .extension_legacy_first(&editor, ExtensionKind::Skill)
        .unwrap();
    assert!(!page.has_next());
    assert_eq!(page.rows[0].name, "Original skill");
    let old = row();
    let r = json!({"definition":old.definition,"provenance":{"kind":"legacy","source_table":"skills","row_number":7,"record_id":"old","source_sha256":"a".repeat(64),"original_enabled":true},"source_sha256":"a".repeat(64),"enabled_on_import":false,"execution":"none"});
    s.lock().unwrap().replies = vec![r.clone()];
    let reviewed = b.extension_legacy_review(&page.rows[0]).unwrap();
    s.lock().unwrap().replies = vec![import_receipt(&r)];
    assert!(
        !b.extension_import(&reviewed, false)
            .unwrap()
            .record()
            .summary
            .enabled
    );
    s.lock().unwrap().session = 1;
    b.initialize().unwrap();
    let before = writes(&s);
    assert!(b.extension_legacy_review(&page.rows[0]).is_err());
    assert_eq!(writes(&s), before);
}
