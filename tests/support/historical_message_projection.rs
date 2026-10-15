use super::*;
use serde_json::json;

fn raw(id: &str, role: &str, seq: i64, parent: Option<&str>, calls: &str, images: &str) -> String {
    serde_json::to_string(&ExactRow {
        columns: [
            "id",
            "session_id",
            "seq",
            "role",
            "parent_id",
            "tool_calls",
            "tool_call_id",
            "images",
            "status",
            "content",
            "reasoning",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        values: vec![
            ExactValue::Text(id.into()),
            ExactValue::Text("session".into()),
            ExactValue::Integer(seq.to_string()),
            ExactValue::Text(role.into()),
            parent
                .map(|v| ExactValue::Text(v.into()))
                .unwrap_or(ExactValue::Null),
            ExactValue::Text(calls.into()),
            ExactValue::Text("call".into()),
            ExactValue::Text(images.into()),
            ExactValue::Text("cancelled".into()),
            ExactValue::Text("Original result remains literal.".into()),
            ExactValue::Text("Separate reasoning.".into()),
        ],
    })
    .unwrap()
}
fn message(
    id: &str,
    role: &str,
    seq: i64,
    parent: Option<&str>,
    calls: &str,
    images: &str,
) -> ArchivedMessage {
    ArchivedMessage::parse(
        "synthetic-import",
        seq,
        &raw(id, role, seq, parent, calls, images),
    )
    .unwrap()
}
fn change(message: &ArchivedMessage, name: &str, value: ExactValue) -> ArchivedMessage {
    let mut row = message.original().clone();
    let n = row.columns.iter().position(|v| v == name).unwrap();
    row.values[n] = value;
    ArchivedMessage::parse(
        "synthetic-import",
        message.source().row_number,
        &serde_json::to_string(&row).unwrap(),
    )
    .unwrap()
}
fn root() -> Ancestry {
    Ancestry {
        nodes: vec![],
        end: Ok(()),
    }
}
fn uri(mime: &str, bytes: &[u8]) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}
const CALL: &str = r#"[{"id":"call","name":"update_plan","arguments":"{broken literal","unknown":{"retained":true}}]"#;

#[test]
fn historical_static_rasters_roundtrip_exact_bytes_and_remain_dormant() {
    for (mime, bytes) in [
        (
            "image/png",
            include_bytes!("../../crates/smith-media/tests/fixtures/pixels.png").as_slice(),
        ),
        (
            "image/jpeg",
            include_bytes!("../../crates/smith-media/tests/fixtures/pixels.jpg").as_slice(),
        ),
        (
            "image/webp",
            include_bytes!("../../crates/smith-media/tests/fixtures/pixels.webp").as_slice(),
        ),
    ] {
        let images = json!([uri(mime, bytes)]).to_string();
        let m = message("image", "user", 1, None, "[]", &images);
        let source = m.source().clone();
        let cards = m.cards(&root()).unwrap();
        assert_eq!(cards.image_count, Some(1));
        assert!(
            matches!(&cards.images[0].state, ImageState::Embedded{decoded_sha256, width, height,..} if decoded_sha256==&digest(bytes) && *width>0 && *height>0)
        );
        let image = m.embedded_image(&source, 0).unwrap();
        assert_eq!(image.bytes, bytes);
        assert_eq!(image.mime, mime);
        assert_eq!(image.sha256, digest(bytes));
        let wire = serde_json::to_value(&cards).unwrap();
        assert_eq!(wire["runtime_activation"], "dormant");
        assert_eq!(wire["images"][0]["fetched"], false);
        assert!(!encode_cards(&cards)
            .unwrap()
            .windows(7)
            .any(|v| v == b"base64,"));
        assert_eq!(m.source(), &source);
    }
}

#[test]
fn historical_remote_and_file_references_never_become_bytes_or_url_labels() {
    let private = "https://user:secret@127.0.0.1:9/private?token=sensitive";
    let images = json!([
        private,
        "file:///private/synthetic-only",
        "data:text/html;base64,PHNjcmlwdD4="
    ])
    .to_string();
    let m = message("references", "user", 1, None, "[]", &images);
    let cards = m.cards(&root()).unwrap();
    assert_eq!(
        cards.images[0].state,
        ImageState::RemoteUnresolved {
            scheme: "https".into()
        }
    );
    assert!(matches!(
        cards.images[1].state,
        ImageState::Retained {
            reason: Reason::UnsupportedScheme
        }
    ));
    assert!(matches!(
        cards.images[2].state,
        ImageState::Retained {
            reason: Reason::UnsupportedDataEncoding
        }
    ));
    let wire = String::from_utf8(encode_cards(&cards).unwrap()).unwrap();
    for forbidden in ["secret", "sensitive", "private?", "file://", "127.0.0.1"] {
        assert!(!wire.contains(forbidden));
    }
    for index in 0..3 {
        assert!(m.embedded_image(m.source(), index).is_err());
    }
    assert_eq!(m.text("images"), Some(images.as_str()));
}

#[test]
fn historical_animation_is_rejected_without_first_frame_fallback() {
    for (mime, bytes) in [
        (
            "image/png",
            include_bytes!("../../crates/smith-media/tests/fixtures/animated.png").as_slice(),
        ),
        (
            "image/webp",
            include_bytes!("../../crates/smith-media/tests/fixtures/animated.webp").as_slice(),
        ),
    ] {
        assert!(matches!(
            classify_image(&uri(mime, bytes)),
            Err(Reason::UnsupportedImageFormat)
        ));
    }
    let png = include_bytes!("../../crates/smith-media/tests/fixtures/pixels.png");
    assert!(matches!(
        classify_image(&uri("image/jpeg", png)),
        Err(Reason::MimeMismatch)
    ));
    assert!(matches!(
        classify_image("data:image/png;base64,?bad"),
        Err(Reason::InvalidBase64)
    ));
    assert!(classify_image("data:image/png;base64,AAAA").is_err());
}

#[test]
fn historical_image_bounds_and_malformed_entries_are_explicit() {
    let many = json!(vec!["https://example.invalid/image"; MAX_IMAGES + 1]).to_string();
    let m = message("many", "user", 1, None, "[]", &many);
    let cards = m.cards(&root()).unwrap();
    assert_eq!(cards.image_count, None);
    assert!(cards
        .issues
        .iter()
        .any(|i| i.reason == Reason::TooManyEntries));
    let malformed = message(
        "bad",
        "user",
        1,
        None,
        "[]",
        r#"["https://example.invalid",42]"#,
    );
    assert!(malformed
        .cards(&root())
        .unwrap()
        .issues
        .iter()
        .any(|i| i.reason == Reason::InvalidJson));
    let oversized = "x".repeat(MAX_IMAGE_FIELD + 1);
    let large = message("large", "user", 1, None, "[]", &oversized);
    assert!(large
        .cards(&root())
        .unwrap()
        .issues
        .iter()
        .any(|i| i.reason == Reason::OversizedField
            && i.retained_bytes == Some(oversized.len() as u64)));
    let encoded = format!(
        "data:image/png;base64,{}",
        "A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4 + 4)
    );
    assert!(matches!(
        classify_image(&encoded),
        Err(Reason::ImageTooLarge)
    ));
}

#[test]
fn historical_image_bytes_require_exact_complete_source_identity() {
    let m = message(
        "image",
        "user",
        1,
        None,
        "[]",
        &json!([uri(
            "image/png",
            include_bytes!("../../crates/smith-media/tests/fixtures/pixels.png")
        )])
        .to_string(),
    );
    for change in 0..4 {
        let mut source = m.source().clone();
        match change {
            0 => source.import_id = "different".into(),
            1 => source.row_number += 1,
            2 => source.payload_sha256 = "0".repeat(64),
            _ => source.table = "artifacts".into(),
        }
        assert!(m.embedded_image(&source, 0).is_err());
    }
    assert!(m.embedded_image(m.source(), 1).is_err());
}

#[test]
fn historical_calls_preserve_order_literal_arguments_unknown_fields_and_status() {
    let calls = r#"[{"id":"one","name":"update_plan","arguments":"{invalid","future":"exact"},{"id":"two","name":"request_user_input","arguments":"{\"question\":\"old\"}"}]"#;
    let m = message("calls", "assistant", 1, None, calls, "[]");
    let cards = m.cards(&root()).unwrap();
    assert_eq!(cards.call_count, Some(2));
    assert_eq!(cards.calls[0].id.as_deref(), Some("one"));
    assert_eq!(cards.calls[1].index, 1);
    assert_eq!(cards.calls[0].arguments.as_ref().unwrap().text, "{invalid");
    assert_eq!(cards.calls[0].arguments_valid_json, Some(false));
    assert_eq!(cards.calls[1].arguments_valid_json, Some(true));
    assert_eq!(cards.calls[0].unknown_fields, vec!["future"]);
    assert_eq!(
        cards.original_status,
        Some(ExactValue::Text("cancelled".into()))
    );
    assert_eq!(m.text("tool_calls"), Some(calls));
}

#[test]
fn historical_duplicate_call_keys_invalid_entries_and_call_limits_refuse_linking() {
    let calls = r#"[{"id":"call","id":"other","name":"x","arguments":"{}"},17,{"id":"missing"}]"#;
    let result = parse_calls(Some(&ExactValue::Text(calls.into())));
    assert!(!result.complete);
    assert_eq!(result.cards[0].issue, Some(Reason::DuplicateCallField));
    assert_eq!(result.cards[1].issue, Some(Reason::InvalidEntry));
    assert_eq!(result.cards[2].issue, Some(Reason::InvalidEntry));
    let many = json!(vec![
        json!({"id":"call","name":"x","arguments":"{}"});
        MAX_CALLS + 1
    ])
    .to_string();
    assert_eq!(
        parse_calls(Some(&ExactValue::Text(many))).issues[0].reason,
        Reason::TooManyEntries
    );
    assert_eq!(
        parse_calls(Some(&ExactValue::Text(" ".repeat(MAX_CALL_FIELD + 1)))).issues[0].reason,
        Reason::OversizedField
    );
}

#[test]
fn historical_previews_are_utf8_safe_and_aggregate_encoding_is_hard_bounded() {
    let argument = "界".repeat(MAX_ARGUMENT_PREVIEW);
    let calls = json!([{"id":"call","name":"x","arguments":argument}]).to_string();
    let m = message("preview", "assistant", 1, None, &calls, "[]");
    let mut cards = m.cards(&root()).unwrap();
    let p = cards.calls[0].arguments.as_ref().unwrap();
    assert!(p.truncated && p.text.len() <= MAX_ARGUMENT_PREVIEW);
    assert!(argument.starts_with(&p.text));
    assert_eq!(p.original_sha256, digest(argument.as_bytes()));
    assert_eq!(p.original_bytes, argument.len() as u64);
    cards.calls[0].arguments.as_mut().unwrap().text = "\u{0001}".repeat(MAX_CARD_BYTES / 2);
    assert!(encode_cards(&cards).is_err());
}

#[test]
fn historical_unique_and_reused_call_ids_resolve_only_on_complete_earlier_path() {
    let result = message("result", "tool", 3, Some("call-message"), "[]", "[]");
    let parent = message("call-message", "assistant", 2, None, CALL, "[]");
    let path = Ancestry {
        nodes: vec![parent],
        end: Ok(()),
    };
    assert!(
        matches!(resolve_result(&result,&path),ResultLink::Unique{call,..} if call.original_message_id=="call-message" && call.call_index==0)
    );
    let parent = message("call-message", "assistant", 2, Some("earlier"), CALL, "[]");
    let earlier = message("earlier", "assistant", 1, None, CALL, "[]");
    assert_eq!(
        resolve_result(
            &result,
            &Ancestry {
                nodes: vec![parent, earlier],
                end: Ok(())
            }
        ),
        ResultLink::Ambiguous {
            call_id: "call".into(),
            candidates: 2
        }
    );
    let duplicate =
        r#"[{"id":"call","name":"x","arguments":"{}"},{"id":"call","name":"y","arguments":"{}"}]"#;
    let parent = message("call-message", "assistant", 2, None, duplicate, "[]");
    assert_eq!(
        resolve_result(
            &result,
            &Ancestry {
                nodes: vec![parent],
                end: Ok(())
            }
        ),
        ResultLink::Ambiguous {
            call_id: "call".into(),
            candidates: 2
        }
    );
}

#[test]
fn historical_siblings_cross_session_future_or_incomplete_paths_never_get_unique_links() {
    let result = message("result", "tool", 3, Some("parent"), "[]", "[]");
    let sibling = message("sibling", "assistant", 2, None, CALL, "[]");
    assert_eq!(
        resolve_result(
            &result,
            &Ancestry {
                nodes: vec![sibling],
                end: Ok(())
            }
        ),
        ResultLink::Unresolved {
            reason: Reason::InvalidIdentity
        }
    );
    let parent = message("parent", "assistant", 2, None, CALL, "[]");
    let other = change(&parent, "session_id", ExactValue::Text("elsewhere".into()));
    assert_eq!(
        resolve_result(
            &result,
            &Ancestry {
                nodes: vec![other],
                end: Ok(())
            }
        ),
        ResultLink::Unresolved {
            reason: Reason::CrossSessionAncestor
        }
    );
    let future = message("parent", "assistant", 4, None, CALL, "[]");
    assert_eq!(
        resolve_result(
            &result,
            &Ancestry {
                nodes: vec![future],
                end: Ok(())
            }
        ),
        ResultLink::Unresolved {
            reason: Reason::NonEarlierAncestor
        }
    );
    assert_eq!(
        resolve_result(
            &result,
            &Ancestry {
                nodes: vec![parent],
                end: Err(Reason::AncestryLimit)
            }
        ),
        ResultLink::Unresolved {
            reason: Reason::AncestryLimit
        }
    );
    assert_eq!(
        resolve_result(&result, &root()),
        ResultLink::Unresolved {
            reason: Reason::MissingAncestor
        }
    );
}

#[test]
fn historical_cycles_unknown_calls_and_absent_call_ids_stay_unresolved() {
    let result = message("result", "tool", 3, Some("parent"), "[]", "[]");
    let parent = message("parent", "assistant", 2, Some("result"), CALL, "[]");
    let cycle = message("result", "tool", 1, None, "[]", "[]");
    assert_eq!(
        resolve_result(
            &result,
            &Ancestry {
                nodes: vec![parent, cycle],
                end: Ok(())
            }
        ),
        ResultLink::Unresolved {
            reason: Reason::Cycle
        }
    );
    let invalid = message("parent", "assistant", 2, None, "{broken", "[]");
    assert_eq!(
        resolve_result(
            &result,
            &Ancestry {
                nodes: vec![invalid],
                end: Ok(())
            }
        ),
        ResultLink::Unresolved {
            reason: Reason::IncompleteCalls
        }
    );
    let no_id = change(&result, "tool_call_id", ExactValue::Null);
    assert_eq!(resolve_result(&no_id, &root()), ResultLink::MissingId);
    let no_parent = message("orphan", "tool", 1, None, "[]", "[]");
    assert_eq!(
        resolve_result(&no_parent, &root()),
        ResultLink::Unmatched {
            call_id: "call".into()
        }
    );
}

#[test]
fn historical_unknown_exact_values_are_not_coerced_or_executed() {
    let mut row: ExactRow =
        serde_json::from_str(&raw("unknown", "assistant", 1, None, "[]", "[]")).unwrap();
    for (name, value) in [
        ("future_blob", ExactValue::Blob("00ff".into())),
        ("future_text", ExactValue::TextBytes("fffe".into())),
        (
            "future_real",
            ExactValue::RealBits("0000000000000080".into()),
        ),
        ("rowid", ExactValue::Integer("-19".into())),
    ] {
        row.columns.push(name.into());
        row.values.push(value);
    }
    let raw = serde_json::to_string(&row).unwrap();
    let m = ArchivedMessage::parse("synthetic-import", 1, &raw).unwrap();
    assert_eq!(m.original(), &row);
    assert_eq!(m.source().payload_sha256, digest(raw.as_bytes()));
    let cards = m.cards(&root()).unwrap();
    assert_eq!(
        cards.unknown_columns,
        vec!["future_blob", "future_text", "future_real"]
    );
    let mut wire = serde_json::to_value(cards).unwrap();
    wire["runtime_activation"] = json!("active");
    assert!(serde_json::from_value::<MessageCards>(wire).is_err());
    assert!(serde_json::from_str::<NotFetched>("true").is_err());
    assert!(serde_json::from_str::<NotFetched>("false").is_ok());
}

#[test]
fn historical_row_vectors_stop_before_materializing_the_first_excess_value() {
    let columns = (0..512)
        .map(|n| json!(format!("c{n}")).to_string())
        .collect::<Vec<_>>()
        .join(",");
    // The next value is deliberately malformed. A pre-materialization refusal
    // must win over attempting to parse or allocate that excess item.
    let excessive_columns = format!("{{\"columns\":[{columns},[\"unterminated");
    let error = serde_json::from_str::<BoundedRow>(&excessive_columns)
        .err()
        .unwrap();
    assert!(error
        .to_string()
        .contains("historical collection entry bound"));
    let nulls = std::iter::repeat_n(r#"{"type":"null"}"#, 512)
        .collect::<Vec<_>>()
        .join(",");
    let excessive_values =
        format!("{{\"columns\":[\"id\"],\"values\":[{nulls},{{\"value\":\"unterminated");
    let error = serde_json::from_str::<BoundedRow>(&excessive_values)
        .err()
        .unwrap();
    assert!(error
        .to_string()
        .contains("historical collection entry bound"));
    let many = json!({"columns":vec!["tiny";100_000],"values":[]}).to_string();
    assert!(many.len() < MAX_ARCHIVE_ROW);
    let error = serde_json::from_str::<BoundedRow>(&many).err().unwrap();
    assert!(error.column() < 10_000);
    assert!(ArchivedMessage::parse("synthetic-import", 1, &many).is_err());
}

#[test]
fn historical_call_maps_refuse_key33_without_decoding_or_falling_back_to_value() {
    let fields = (0..32)
        .map(|n| format!("\"k{n}\":null"))
        .collect::<Vec<_>>()
        .join(",");
    let raw = format!(
        "{{{fields},\"excess\":{{\"large\":\"{}",
        "x".repeat(300_000)
    );
    let error = serde_json::from_str::<CallEntry>(&raw).err().unwrap();
    assert!(error
        .to_string()
        .contains("historical collection entry bound"));
    assert!(error.column() < 1024);
    let field = ExactValue::Text(format!("[{raw}]"));
    let calls = parse_calls(Some(&field));
    assert!(!calls.complete);
    assert_eq!(calls.issues[0].reason, Reason::InvalidJson);
    // Unknown nested values are skipped while exact strings stay literal.
    let calls =
        json!([{"id":"call","name":"x","arguments":"{}","unknown":vec![0;50_000]}]).to_string();
    let parsed = parse_calls(Some(&ExactValue::Text(calls)));
    assert!(parsed.complete);
    assert_eq!(parsed.cards[0].unknown_fields, vec!["unknown"]);
}
