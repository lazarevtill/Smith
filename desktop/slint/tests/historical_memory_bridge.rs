//! Pure protocol tests; no profile, model, provider, or original files.
use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Bridge, Result, Transport},
    historical_memory_bridge::*,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
struct State {
    session: String,
    pages: VecDeque<Value>,
    detail: HistoricalDetail,
    requests: Vec<(String, String)>,
    patches: usize,
    fail_patch: bool,
    corrupt_receipt: Option<&'static str>,
    rotate_after_memory: bool,
    advance_after_receipt: bool,
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
        if path.starts_with("/api/legacy-import/memory") {
            s.requests.push((method.into(), path.into()));
            let result = if method == "PATCH" {
                s.patches += 1;
                if s.fail_patch {
                    Err("Lost write acknowledgement".into())
                } else {
                    let input: HistoricalChange = serde_json::from_value(body.unwrap()).unwrap();
                    assert_eq!(input.source, s.detail.record.source);
                    let trashed = input.action == HistoricalAction::Trash;
                    let mut next = s.detail.record.overlay.clone();
                    if next.trashed != trashed {
                        next.revision += 1;
                        next.trashed = trashed;
                    }
                    s.detail.record.overlay = next.clone();
                    let mut value = serde_json::to_value(HistoricalReceipt {
                        source: input.source,
                        request_id: input.request_id,
                        action: input.action,
                        overlay: next,
                    })
                    .unwrap();
                    if let Some(field) = s.corrupt_receipt {
                        match field {
                            "source" => value["source"]["payload_sha256"] = json!("b".repeat(64)),
                            "request_id" => {
                                value[field] = json!("00000000-0000-4000-8000-000000000000")
                            }
                            "action" => value[field] = json!("restore"),
                            "revision" => value["overlay"][field] = json!(55),
                            _ => panic!("Unknown injected corruption"),
                        }
                    }
                    if s.advance_after_receipt {
                        s.detail.record.overlay.revision += 1;
                        s.detail.record.overlay.trashed = !trashed;
                    }
                    Ok(value)
                }
            } else if path.contains('?') {
                Ok(s.pages.pop_front().expect("Unexpected page fetch"))
            } else {
                Ok(serde_json::to_value(&s.detail).unwrap())
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
fn record(kind: HistoricalKind, n: i64) -> HistoricalRecord {
    let txt = |s: &str| Some(ExactValue::Text(s.into()));
    let data = match kind {
        HistoricalKind::Episode => HistoricalData::Episode {
            agent_id: Some(ExactValue::Null),
            session_id: txt("original-session"),
            summary: txt("Original summary"),
            salience: Some(ExactValue::RealBits("000000000000e03f".into())),
            original_pinned: Some(ExactValue::Integer("1".into())),
            created_at: txt("2025-01-01T00:00:00.123Z"),
            valid_to: txt("2099-01-01T00:00:00Z"),
        },
        HistoricalKind::Lesson => HistoricalData::Lesson {
            agent_id: txt("original-agent"),
            source_session_id: txt("original-session"),
            polarity: txt("unknown-polarity"),
            trigger: txt("Original trigger"),
            root_cause: txt("Original cause"),
            corrected_behavior: txt("Dormant advice"),
            scope: txt("free text not grant"),
            usage_count: Some(ExactValue::Integer(i64::MAX.to_string())),
            success_count: Some(ExactValue::Integer("1".into())),
            created_at: txt("2025-01-01T00:00:00Z"),
            valid_to: Some(ExactValue::Null),
        },
    };
    HistoricalRecord {
        source: HistoricalSource {
            import_id: "original-import".into(),
            source_schema: 25,
            table: kind.table().into(),
            row_number: n,
            payload_sha256: "a".repeat(64),
        },
        original_id: txt("opaque-original"),
        data,
        embedding: EmbeddingInfo {
            stored_kind: "blob".into(),
            byte_len: Some(4),
            sha256: Some("c".repeat(64)),
            source_encoding: Some("little_endian_f32".into()),
            dimensions: Some(1),
            model_provenance: "unknown".into(),
            reuse_allowed: false,
        },
        unknown_columns: vec!["unknown".into()],
        warnings: vec![],
        overlay: HistoricalOverlay::default(),
        runtime_activation: "dormant".into(),
    }
}
fn setup() -> (Bridge<Mock>, Arc<Mutex<State>>) {
    let detail = HistoricalDetail {
        record: record(HistoricalKind::Episode, 0),
        original: ExactRow {
            columns: vec!["unknown".into()],
            values: vec![ExactValue::TextBytes("ff00".into())],
        },
    };
    let s = Arc::new(Mutex::new(State {
        session: "s1".into(),
        pages: VecDeque::new(),
        detail,
        requests: vec![],
        patches: 0,
        fail_patch: false,
        corrupt_receipt: None,
        rotate_after_memory: false,
        advance_after_receipt: false,
    }));
    let mut b = Bridge::new(Mock(s.clone()));
    b.initialize().unwrap();
    (b, s)
}
fn page(filter: HistoricalFilter, records: Vec<HistoricalRecord>, cursor: Option<String>) -> Value {
    serde_json::to_value(HistoricalPageReply {
        import_id: "original-import".into(),
        filter,
        scanned: records.len().max(1),
        records,
        retained_issues: vec![],
        next_cursor: cursor,
        total_archived_rows: 500,
        runtime_activation: "dormant".into(),
    })
    .unwrap()
}
fn capture(b: &mut Bridge<Mock>, s: &Arc<Mutex<State>>) -> HistoricalPage {
    let r = s.lock().unwrap().detail.record.clone();
    let f = HistoricalFilter {
        kind: r.data.kind(),
        trash: r.overlay.trashed,
        ..Default::default()
    };
    s.lock()
        .unwrap()
        .pages
        .push_back(page(f.clone(), vec![r], None));
    b.historical_first(f).unwrap()
}
#[test]
fn distinct_episode_lesson_data_and_exact_details_stay_dormant() {
    for kind in [HistoricalKind::Episode, HistoricalKind::Lesson] {
        let (mut b, s) = setup();
        s.lock().unwrap().detail.record = record(kind, 0);
        let page = capture(&mut b, &s);
        let review = page.review(&page.records[0]).unwrap();
        let detail = b.historical_detail(&review).unwrap();
        assert_eq!(detail.record().data.kind(), kind);
        assert_eq!(
            detail.original.values,
            vec![ExactValue::TextBytes("ff00".into())]
        );
        assert_eq!(detail.record().runtime_activation, "dormant");
        assert!(!detail.record().embedding.reuse_allowed);
        assert_eq!(s.lock().unwrap().patches, 0);
    }
}
#[test]
fn page_bound_one_use_changes_require_exact_receipt_and_fresh_detail() {
    let (mut b, s) = setup();
    let page = capture(&mut b, &s);
    let cloned = page.clone();
    let review = page.review(&page.records[0]).unwrap();
    let trashed = b.historical_trash(&review).unwrap();
    assert_eq!(
        trashed.record().overlay,
        HistoricalOverlay {
            revision: 1,
            trashed: true
        }
    );
    assert!(b
        .historical_trash(&cloned.review(&cloned.records[0]).unwrap())
        .is_err());
    let restored = b.historical_restore(&trashed).unwrap();
    assert_eq!(
        restored.record().overlay,
        HistoricalOverlay {
            revision: 2,
            trashed: false
        }
    );
    assert_eq!(s.lock().unwrap().patches, 2);
}
#[test]
fn old_pages_and_receipts_cannot_rebind_after_session_reinitialization() {
    let (mut b, s) = setup();
    let page = capture(&mut b, &s);
    let raw = page.records[0].clone();
    s.lock().unwrap().session = "replacement".into();
    b.initialize().unwrap();
    assert!(b.historical_trash(&page.review(&raw).unwrap()).is_err());
    assert_eq!(s.lock().unwrap().patches, 0);
    let mut fresh = capture(&mut b, &s);
    let mut injected = raw.clone();
    injected.source.payload_sha256 = "d".repeat(64);
    fresh.records.push(injected.clone());
    assert!(fresh.review(&injected).is_err());
}
#[test]
fn unknown_acknowledgement_and_mismatched_receipts_do_not_retry_or_adopt() {
    for mode in ["lost", "source", "request_id", "action", "revision"] {
        let (mut b, s) = setup();
        let page = capture(&mut b, &s);
        let review = page.review(&page.records[0]).unwrap();
        if mode == "lost" {
            s.lock().unwrap().fail_patch = true;
        } else {
            s.lock().unwrap().corrupt_receipt = Some(mode);
        }
        assert!(b.historical_trash(&review).is_err());
        assert!(b.historical_trash(&review.clone()).is_err());
        assert_eq!(s.lock().unwrap().patches, 1);
    }
}
#[test]
fn newer_state_after_receipt_or_rotation_during_response_is_not_adopted() {
    let (mut b, s) = setup();
    let page = capture(&mut b, &s);
    let review = page.review(&page.records[0]).unwrap();
    s.lock().unwrap().advance_after_receipt = true;
    assert!(b.historical_trash(&review).is_err());
    assert_eq!(
        s.lock().unwrap().detail.record.overlay,
        HistoricalOverlay {
            revision: 2,
            trashed: false
        }
    );
    let (mut b, s) = setup();
    let page = capture(&mut b, &s);
    let review = page.review(&page.records[0]).unwrap();
    s.lock().unwrap().rotate_after_memory = true;
    assert!(b.historical_trash(&review).is_err());
    assert!(!b.view.available);
}
#[test]
fn empty_filtered_scan_can_continue_but_repeated_cursor_or_rows_cannot() {
    let (mut b, s) = setup();
    let f = HistoricalFilter::default();
    s.lock()
        .unwrap()
        .pages
        .push_back(page(f.clone(), vec![], Some("cursor-one".into())));
    s.lock().unwrap().pages.push_back(page(
        f.clone(),
        vec![record(HistoricalKind::Episode, 0)],
        Some("cursor-two".into()),
    ));
    s.lock()
        .unwrap()
        .pages
        .push_back(page(f.clone(), vec![], Some("cursor-two".into())));
    let first = b.historical_first(f).unwrap();
    assert!(first.records.is_empty());
    let next = b.historical_next(first.next.as_ref().unwrap()).unwrap();
    assert_eq!(next.records.len(), 1);
    assert!(b.historical_next(next.next.as_ref().unwrap()).is_err());
    assert!(b.historical_next(first.next.as_ref().unwrap()).is_err());
}
#[test]
fn wrong_kind_scope_activation_and_bounds_fail_closed() {
    for mode in [
        "kind",
        "scope",
        "active",
        "embedding",
        "oversized",
        "zero_scan",
    ] {
        let (mut b, s) = setup();
        let f = HistoricalFilter {
            scope: AgentScope::Global,
            ..Default::default()
        };
        let mut value = page(f.clone(), vec![record(HistoricalKind::Episode, 0)], None);
        match mode {
            "kind" => value["records"][0]["source"]["table"] = json!("lessons"),
            "scope" => {
                value["records"][0]["data"]["agent_id"] =
                    json!({"type":"text","value":"another-agent"})
            }
            "active" => value["records"][0]["runtime_activation"] = json!("active"),
            "embedding" => value["records"][0]["embedding"]["reuse_allowed"] = json!(true),
            "oversized" => value["records"] = json!(vec![record(HistoricalKind::Episode, 0); 33]),
            _ => value["scanned"] = json!(0),
        }
        s.lock().unwrap().pages.push_back(value);
        assert!(b.historical_first(f).is_err());
    }
}
#[test]
fn retained_oversized_record_is_explicit_and_pagination_is_finite() {
    let (mut b, s) = setup();
    let f = HistoricalFilter::default();
    let mut value = page(f.clone(), vec![], None);
    value["retained_issues"] = json!([{"row_number":0,"code":"oversized_original_retained","payload_bytes":MAX_ROW_BYTES+1}]);
    s.lock().unwrap().pages.push_back(value);
    let got = b.historical_first(f.clone()).unwrap();
    assert_eq!(got.retained_issues.len(), 1);
    for n in 0..MAX_NATIVE_PAGES {
        s.lock()
            .unwrap()
            .pages
            .push_back(page(f.clone(), vec![], Some(format!("cursor-{n}"))));
    }
    let mut current = b.historical_first(f).unwrap();
    for n in 1..MAX_NATIVE_PAGES {
        let result = b.historical_next(current.next.as_ref().unwrap());
        if n == MAX_NATIVE_PAGES - 1 {
            assert!(result.is_err());
            break;
        }
        current = result.unwrap();
    }
    assert_eq!(s.lock().unwrap().patches, 0);
}
