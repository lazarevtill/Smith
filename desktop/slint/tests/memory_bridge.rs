//! Typed memory protocol regressions with an owned in-process mock. No backend,
//! profile, filesystem, model, external service, or automatic memory ingestion.
use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Bridge, Result, Transport},
    memory_bridge::*,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
struct State {
    session: String,
    pages: VecDeque<Value>,
    receipts: VecDeque<Result<Value>>,
    requests: Vec<(String, String, Option<Value>)>,
    rotate_after_memory: bool,
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
        let mut s = self.0.lock().unwrap();
        if path == "/api/profile/status" {
            return Ok(json!({"state":"unlocked","session":s.session}));
        }
        assert_eq!(session, Some(s.session.as_str()));
        if path.starts_with("/api/memory") {
            s.requests.push((method.to_owned(), path.to_owned(), body));
            let result = if method == "GET" {
                Ok(s.pages.pop_front().expect("Unexpected memory page request"))
            } else {
                s.receipts.pop_front().expect("Unexpected memory mutation")
            };
            if s.rotate_after_memory {
                s.session = "rotated".into();
            }
            return result;
        }
        match path {
            "/api/config" => {
                Ok(json!({"revision":"c","provider":"mlx","endpoint":"","model":"","tested":false}))
            }
            "/api/chats" | "/api/media/artifacts" | "/api/agent/tasks" => Ok(json!([])),
            "/api/actions" => Ok(json!({"revision":0,"undo":null,"redo":null})),
            "/api/trash" => Ok(json!({"chats":[]})),
            "/api/drafts/new" => {
                Ok(json!({"owner":"new","version":0,"text":"","attachments":[],"media":[]}))
            }
            _ => panic!("Unexpected mock route"),
        }
    }
}
fn setup() -> (Bridge<Mock>, Arc<Mutex<State>>) {
    let s = Arc::new(Mutex::new(State {
        session: "s1".into(),
        pages: VecDeque::new(),
        receipts: VecDeque::new(),
        requests: vec![],
        rotate_after_memory: false,
    }));
    let mut b = Bridge::new(Mock(s.clone()));
    b.initialize().unwrap();
    (b, s)
}
fn record(id: usize) -> MemoryRecord {
    MemoryRecord {
        id: format!("memory-{id}"),
        namespace: "facts".into(),
        fact: format!("Synthetic fact {id}"),
        tags: vec!["synthetic".into()],
        version: 1,
        pinned: false,
        deleted: false,
        updated_at: 10,
    }
}
fn filter() -> MemoryFilter {
    MemoryFilter {
        namespace: Some("facts".into()),
        query: String::new(),
        deleted: false,
    }
}
fn page(rows: Vec<MemoryRecord>, next: Option<i64>) -> Value {
    json!({"memories":rows,"next_before":next,"matching":"literal_ascii_case_insensitive","deleted":false,"namespace":"facts"})
}
fn queue(s: &Arc<Mutex<State>>, record: &MemoryRecord) {
    s.lock()
        .unwrap()
        .receipts
        .push_back(Ok(serde_json::to_value(record).unwrap()));
}
// Capture through the real page boundary; keep later request counts focused
// on mutations rather than this fixture setup read.
fn reviewed(b: &mut Bridge<Mock>, s: &Arc<Mutex<State>>, r: &MemoryRecord) -> MemoryReview {
    let f = MemoryFilter {
        deleted: r.deleted,
        namespace: Some(r.namespace.clone()),
        query: String::new(),
    };
    let value = json!({"memories":[r],"next_before":null,"matching":"literal_ascii_case_insensitive","deleted":r.deleted,"namespace":r.namespace});
    s.lock().unwrap().pages.push_back(value);
    let page = b.memory_first(f).unwrap();
    let request = s.lock().unwrap().requests.pop().unwrap();
    assert_eq!(request.0, "GET");
    page.review(r).unwrap()
}
#[test]
fn namespace_and_literal_query_are_encoded_and_continuation_is_exact_one_use() {
    let (mut b, s) = setup();
    s.lock()
        .unwrap()
        .pages
        .push_back(page((0..32).map(record).collect(), Some(200)));
    s.lock()
        .unwrap()
        .pages
        .push_back(page((32..34).map(record).collect(), None));
    let f = MemoryFilter {
        query: "100%_ &wild?".into(),
        ..filter()
    };
    let first = b.memory_first(f.clone()).unwrap();
    assert_eq!(first.filter, f);
    assert_eq!(first.memories.len(), 32);
    let next = first.next.unwrap();
    let duplicate = next.clone();
    let second = b.memory_next(&next).unwrap();
    assert_eq!(second.filter, f);
    assert_eq!(second.memories.len(), 2);
    assert!(second.next.is_none());
    assert!(b.memory_next(&duplicate).is_err());
    let requests = &s.lock().unwrap().requests;
    assert_eq!(requests.len(), 2);
    assert!(requests[0].1.contains("namespace=facts"));
    assert!(requests[0].1.contains("query=100%25_%20%26wild%3F"));
    assert!(requests[1].1.ends_with("&before=200"));
}
#[test]
fn pin_trash_restore_use_exact_receipts_and_versions() {
    let (mut b, s) = setup();
    let original = record(0);
    let mut pinned = original.clone();
    pinned.pinned = true;
    pinned.version = 2;
    queue(&s, &pinned);
    let review = reviewed(&mut b, &s, &original);
    let saved = b.memory_set_pinned(&review, true).unwrap();
    assert_eq!(saved.record(), &pinned);
    let mut trashed = saved.record().clone();
    trashed.deleted = true;
    trashed.version = 3;
    queue(&s, &trashed);
    let saved = b.memory_trash(&saved).unwrap();
    let undo = saved.clone();
    let duplicate = undo.clone();
    let mut restored = saved.record().clone();
    restored.deleted = false;
    restored.version = 4;
    queue(&s, &restored);
    assert_eq!(b.memory_restore(&undo).unwrap().record(), &restored);
    assert!(b.memory_restore(&duplicate).is_err());
    let requests = &s.lock().unwrap().requests;
    assert_eq!(requests.len(), 3);
    for (index, (method, path, body)) in requests.iter().enumerate() {
        assert_eq!(method, "PATCH");
        assert_eq!(path, "/api/memory/memory-0");
        assert_eq!(body.as_ref().unwrap()["version"], (index + 1) as i64);
    }
}
#[test]
fn unchanged_pin_requires_unchanged_version_and_record() {
    let (mut b, s) = setup();
    let r = record(0);
    queue(&s, &r);
    let review = reviewed(&mut b, &s, &r);
    let receipt = b.memory_set_pinned(&review, false).unwrap();
    assert_eq!(receipt.record(), &r);
    let mut wrong = r.clone();
    wrong.version = 2;
    queue(&s, &wrong);
    assert!(b.memory_set_pinned(&receipt, false).is_err());
}
#[test]
fn changed_receipt_must_match_exact_reviewed_identity_scope_content_and_version() {
    for field in [
        "id",
        "namespace",
        "fact",
        "tags",
        "version",
        "deleted",
        "pinned",
    ] {
        let (mut b, s) = setup();
        let r = record(0);
        let mut reply = serde_json::to_value(&r).unwrap();
        reply["pinned"] = json!(true);
        reply["version"] = json!(2);
        reply[field] = match field {
            "version" => json!(77),
            "pinned" => json!(false),
            "deleted" => json!(true),
            "tags" => json!(["unexpected"]),
            _ => json!("unexpected"),
        };
        s.lock().unwrap().receipts.push_back(Ok(reply));
        let review = reviewed(&mut b, &s, &r);
        let duplicate = review.clone();
        assert!(b.memory_set_pinned(&review, true).is_err());
        assert!(b.memory_set_pinned(&duplicate, true).is_err());
        assert_eq!(s.lock().unwrap().requests.len(), 1);
    }
}
#[test]
fn lost_create_or_change_acknowledgement_never_replays_cloned_intent() {
    let (mut b, s) = setup();
    s.lock()
        .unwrap()
        .receipts
        .push_back(Err("Dropped write reply".into()));
    let create = b
        .memory_editor()
        .unwrap()
        .create("facts".into(), "Explicit synthetic fact".into(), vec![])
        .unwrap();
    let duplicate = create.clone();
    assert!(b.memory_create(&create).err().unwrap().contains("Refresh"));
    assert!(b.memory_create(&duplicate).is_err());
    assert_eq!(s.lock().unwrap().requests.len(), 1);
    let review = reviewed(&mut b, &s, &record(1));
    let duplicate = review.clone();
    s.lock()
        .unwrap()
        .receipts
        .push_back(Err("409 stale version".into()));
    assert!(b.memory_trash(&review).is_err());
    assert!(b.memory_trash(&duplicate).is_err());
    assert_eq!(s.lock().unwrap().requests.len(), 2);
}
#[test]
fn session_rotation_before_or_during_mutation_discards_the_response() {
    let (mut b, s) = setup();
    let create = b
        .memory_editor()
        .unwrap()
        .create("facts".into(), "Explicit fact".into(), vec![])
        .unwrap();
    s.lock().unwrap().session = "rotated".into();
    assert!(b.memory_create(&create).is_err());
    assert!(!b.view.available);
    assert!(s.lock().unwrap().requests.is_empty());
    let (mut b, s) = setup();
    let r = record(0);
    let mut saved = r.clone();
    saved.pinned = true;
    saved.version = 2;
    queue(&s, &saved);
    let review = reviewed(&mut b, &s, &r);
    s.lock().unwrap().rotate_after_memory = true;
    assert!(b.memory_set_pinned(&review, true).is_err());
    assert!(!b.view.available);
    assert_eq!(s.lock().unwrap().requests.len(), 1);
}
#[test]
fn create_and_edit_validate_text_tags_namespace_without_any_request() {
    let (mut b, s) = setup();
    let editor = b.memory_editor().unwrap();
    for namespace in ["", "with space", "wild%", "é"] {
        assert!(editor
            .create(namespace.into(), "Fact".into(), vec![])
            .is_err());
    }
    assert!(editor.create("facts".into(), " ".into(), vec![]).is_err());
    assert!(editor
        .create("facts".into(), "x".repeat(8193), vec![])
        .is_err());
    assert!(editor
        .create("facts".into(), "Fact".into(), vec!["bad\n".into()])
        .is_err());
    assert!(editor
        .create("facts".into(), "Fact".into(), vec!["x".into(); 33])
        .is_err());
    // Quotes fit each tag's raw limit, but escaping exceeds the aggregate
    // encoded storage bound; reject before a request can reach the backend.
    assert!(editor
        .create("facts".into(), "Fact".into(), vec!["\"".repeat(64); 32])
        .is_err());
    let mut r = record(1);
    r.deleted = true;
    let review = reviewed(&mut b, &s, &r);
    assert!(b.memory_edit(&review, "New fact".into(), vec![]).is_err());
    assert!(b.memory_set_pinned(&review, true).is_err());
    assert!(s.lock().unwrap().requests.is_empty());
}
#[test]
fn explicit_create_and_edit_return_exact_records_without_implicit_ingestion() {
    let (mut b, s) = setup();
    let first = page(vec![], None);
    s.lock().unwrap().pages.push_back(first);
    let page = b.memory_first(filter()).unwrap();
    assert_eq!(s.lock().unwrap().requests.len(), 1);
    let create = page
        .editor()
        .create("facts".into(), "Explicit fact".into(), vec!["human".into()])
        .unwrap();
    let mut r = record(1);
    r.fact = "Explicit fact".into();
    r.tags = vec!["human".into()];
    queue(&s, &r);
    let review = b.memory_create(&create).unwrap();
    assert_eq!(review.record(), &r);
    r.fact = "Explicit changed fact".into();
    r.tags = vec![];
    r.version = 2;
    queue(&s, &r);
    assert_eq!(
        b.memory_edit(&review, r.fact.clone(), r.tags.clone())
            .unwrap()
            .record(),
        &r
    );
    assert_eq!(s.lock().unwrap().requests.len(), 3);
}
#[test]
fn oversized_wrong_scope_repeated_or_nonmonotonic_pages_are_rejected() {
    for bad in [
        page((0..33).map(record).collect(), None),
        page(vec![], Some(10)),
        page(vec![record(0), record(0)], None),
        {
            let mut r = record(0);
            r.namespace = "other".into();
            page(vec![r], None)
        },
        {
            let mut r = record(0);
            r.deleted = true;
            page(vec![r], None)
        },
    ] {
        let (mut b, s) = setup();
        s.lock().unwrap().pages.push_back(bad);
        assert!(b.memory_first(filter()).is_err());
    }
    for next in [0, 100, 101] {
        let (mut b, s) = setup();
        s.lock()
            .unwrap()
            .pages
            .push_back(page((0..32).map(record).collect(), Some(100)));
        s.lock()
            .unwrap()
            .pages
            .push_back(page((32..64).map(record).collect(), Some(next)));
        let cursor = b.memory_first(filter()).unwrap().next.unwrap();
        assert!(b.memory_next(&cursor).is_err());
    }
    let (mut b, s) = setup();
    s.lock()
        .unwrap()
        .pages
        .push_back(page((0..32).map(record).collect(), Some(100)));
    s.lock()
        .unwrap()
        .pages
        .push_back(page((16..48).map(record).collect(), None));
    let cursor = b.memory_first(filter()).unwrap().next.unwrap();
    assert!(b.memory_next(&cursor).is_err());
}
#[test]
fn pagination_accepts_exact_bound_and_refuses_overflow_without_extra_fetch() {
    for overflow in [false, true] {
        let (mut b, s) = setup();
        let mut rows = 0;
        for index in 0..MAX_PAGES {
            let count = if index == MAX_PAGES - 1 {
                if overflow {
                    17
                } else {
                    16
                }
            } else {
                32
            };
            let values = (rows..rows + count).map(record).collect();
            rows += count;
            let next = (index < MAX_PAGES - 1).then_some(20_000 - index as i64 * 32);
            s.lock().unwrap().pages.push_back(page(values, next));
        }
        let mut current = b.memory_first(filter()).unwrap();
        let mut total = current.memories.len();
        for index in 1..MAX_PAGES {
            let result = b.memory_next(current.next.as_ref().unwrap());
            if overflow && index == MAX_PAGES - 1 {
                assert!(result.is_err());
                break;
            }
            current = result.unwrap();
            total += current.memories.len();
        }
        assert_eq!(s.lock().unwrap().requests.len(), MAX_PAGES);
        if !overflow {
            assert_eq!(total, MAX_RECORDS);
            assert!(current.next.is_none());
        }
    }
}
#[test]
fn captured_cursor_cannot_be_adopted_by_another_session() {
    let (mut b, s) = setup();
    s.lock()
        .unwrap()
        .pages
        .push_back(page((0..32).map(record).collect(), Some(100)));
    let cursor = b.memory_first(filter()).unwrap().next.unwrap();
    s.lock().unwrap().session = "rotated".into();
    assert!(b.memory_next(&cursor).is_err());
    assert!(!b.view.available);
    assert_eq!(s.lock().unwrap().requests.len(), 1);
}

#[test]
fn old_page_records_cannot_be_rebound_after_workspace_reinitialization() {
    let (mut b, s) = setup();
    let r = record(0);
    s.lock()
        .unwrap()
        .pages
        .push_back(page(vec![r.clone()], None));
    let old_page = b.memory_first(filter()).unwrap();
    s.lock().unwrap().session = "replacement".into();
    b.initialize().unwrap();
    let stale_review = old_page.review(&r).unwrap();
    assert!(b.memory_trash(&stale_review).is_err());
    assert!(b.view.available);
    assert_eq!(s.lock().unwrap().requests.len(), 1);
    // A new page may have the same record identity, but different content. Its
    // private membership does not trust public display rows inserted by callers.
    let mut replacement = r.clone();
    replacement.fact = "Replacement workspace fact".into();
    s.lock()
        .unwrap()
        .pages
        .push_back(page(vec![replacement], None));
    let mut new_page = b.memory_first(filter()).unwrap();
    new_page.memories.push(r.clone());
    assert!(new_page.review(&r).is_err());
    let create = old_page
        .editor()
        .create("facts".into(), "Old context".into(), vec![])
        .unwrap();
    assert!(b.memory_create(&create).is_err());
    assert_eq!(s.lock().unwrap().requests.len(), 2);
}

#[test]
fn page_clones_do_not_mint_new_review_after_uncertain_write_or_adopt_old_receipt() {
    let (mut b, s) = setup();
    let r = record(0);
    s.lock()
        .unwrap()
        .pages
        .push_back(page(vec![r.clone()], None));
    let page = b.memory_first(filter()).unwrap();
    let duplicate = page.clone();
    s.lock()
        .unwrap()
        .receipts
        .push_back(Err("Dropped acknowledgement".into()));
    assert!(b.memory_trash(&page.review(&r).unwrap()).is_err());
    assert!(b.memory_trash(&duplicate.review(&r).unwrap()).is_err());
    assert_eq!(s.lock().unwrap().requests.len(), 2);
    // Only a fresh page or an exact successful receipt supplies the next token.
    let review = reviewed(&mut b, &s, &r);
    let mut saved = r.clone();
    saved.version = 2;
    saved.pinned = true;
    queue(&s, &saved);
    let receipt = b.memory_set_pinned(&review, true).unwrap();
    s.lock().unwrap().session = "replacement".into();
    b.initialize().unwrap();
    assert!(b.memory_trash(&receipt).is_err());
    assert_eq!(s.lock().unwrap().requests.len(), 3);
}

#[test]
fn omitted_namespace_explicitly_lists_all_scopes_without_requiring_an_echo() {
    let (mut b, s) = setup();
    let a = record(0);
    let mut z = record(1);
    z.namespace = "project-x".into();
    s.lock().unwrap().pages.push_back(json!({"memories":[a,z],"next_before":null,"matching":"literal_ascii_case_insensitive","deleted":false}));
    let page = b.memory_first(MemoryFilter::default()).unwrap();
    assert_eq!(page.memories.len(), 2);
    assert_eq!(
        page.review(&page.memories[1]).unwrap().record().namespace,
        "project-x"
    );
    assert!(!s.lock().unwrap().requests[0].1.contains("namespace="));
}
