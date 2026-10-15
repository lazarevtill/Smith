//! Versioned, independently authored disposable SQLCipher25 message-card fixture.
//! Usage: <new destination> <owned loopback listener port>; synthetic key on stdin.
//! Does not load an existing profile or contact the listener.
#[path = "../tests/support/legacy_fixture.rs"]
mod fixture;
use rusqlite::params;
use std::io::Read;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let path = std::path::PathBuf::from(args.next().expect("New synthetic destination required"));
    let port: u16 = args
        .next()
        .expect("Owned listener port required")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        port > 0 && args.next().is_none() && !path.exists(),
        "Only a new synthetic destination and owned listener port are accepted"
    );
    let mut input = zeroize::Zeroizing::new(String::new());
    std::io::stdin()
        .take(66)
        .read_to_string(&mut input)
        .unwrap();
    let value = input.trim();
    assert!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "Synthetic stdin key must be hex"
    );
    let mut master = zeroize::Zeroizing::new([0u8; 32]);
    for (n, byte) in master.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[n * 2..n * 2 + 2], 16).unwrap();
    }
    fixture::create(&path, &master);
    let db = fixture::connection(&path.join("smith.db"), &master);
    let cipher: String = db
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .unwrap();
    assert!(cipher.starts_with("4."), "Actual SQLCipher4 required");
    let images: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("support/historical-message-images-v1.json")).unwrap();
    let mut inputs = images[..3]
        .iter()
        .map(|v| v["data_uri"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    inputs.extend([
        format!("http://synthetic:synthetic-password@127.0.0.1:{port}/must-not-fetch?token=synthetic-token"),
        "file:///synthetic-historical-must-not-open".into(),
        "data:image/png;base64,?invalid".into(),
        images[3]["data_uri"].as_str().unwrap().into(),
        images[4]["data_uri"].as_str().unwrap().into(),
        images[0]["data_uri"].as_str().unwrap().replace("image/png","image/jpeg"),
    ]);
    db.execute_batch("ALTER TABLE messages ADD COLUMN original_extra BLOB; UPDATE messages SET original_extra=X'00FF0102';").unwrap();
    db.execute(
        "UPDATE messages SET images=?1 WHERE id='root'",
        [serde_json::to_string(&inputs).unwrap()],
    )
    .unwrap();
    let calls = r#"[{"id":"call","name":"update_plan","arguments":"{literal invalid","future":{"opaque":true}},{"id":"clarify","name":"request_user_input","arguments":"{\"question\":\"Synthetic historical question\"}"}]"#;
    db.execute(
        "UPDATE messages SET tool_calls=?1 WHERE id='branch1'",
        [calls],
    )
    .unwrap();
    let reused = r#"[{"id":"reused","name":"historical_only","arguments":"{}"}]"#;
    let sibling = r#"[{"id":"orphan-call","name":"historical_only","arguments":"{}"}]"#;
    let slow = serde_json::to_string(&vec![images[5]["data_uri"].as_str().unwrap(); 16]).unwrap();
    // Every parent is in this session and strictly earlier. Malformed content is
    // retained data, not a malformed graph that preservation should repair.
    let rows = [
        (
            "clarify-result",
            5,
            "tool",
            "Historical clarification answer",
            Some("branch1"),
            "[]",
            Some("clarify"),
            "[]",
            "complete",
        ),
        (
            "reuse-earlier",
            10,
            "assistant",
            "Earlier historical call",
            Some("root"),
            reused,
            None,
            "[]",
            "complete",
        ),
        (
            "reuse-later",
            11,
            "assistant",
            "Later reused historical call",
            Some("reuse-earlier"),
            reused,
            None,
            "[]",
            "cancelled",
        ),
        (
            "reuse-result",
            12,
            "tool",
            "Ambiguous original result",
            Some("reuse-later"),
            "[]",
            Some("reused"),
            "[]",
            "failed",
        ),
        (
            "sibling-call",
            13,
            "assistant",
            "Different historical branch",
            Some("root"),
            sibling,
            None,
            "[]",
            "complete",
        ),
        (
            "orphan-result",
            14,
            "tool",
            "No matching ancestor",
            Some("root"),
            "[]",
            Some("orphan-call"),
            "[]",
            "incomplete",
        ),
        (
            "malformed-call",
            20,
            "assistant",
            "Retained malformed call JSON",
            Some("root"),
            "{broken",
            None,
            "[]",
            "complete",
        ),
        (
            "malformed-result",
            21,
            "tool",
            "Unresolved original result",
            Some("malformed-call"),
            "[]",
            Some("unparsed"),
            "[]",
            "complete",
        ),
        (
            "invalid-images",
            22,
            "user",
            "Retained malformed image array",
            Some("root"),
            "[]",
            None,
            "[not-json",
            "complete",
        ),
        (
            "slow-images",
            30,
            "user",
            "Synthetic bounded raster drain witness",
            Some("root"),
            "[]",
            None,
            slow.as_str(),
            "complete",
        ),
        (
            "cards-final",
            31,
            "assistant",
            "Synthetic historical card fixture complete",
            Some("root"),
            "[]",
            None,
            "[]",
            "complete",
        ),
    ];
    for (id, seq, role, content, parent, calls, call_id, images, status) in rows {
        db.execute("INSERT INTO messages(id,session_id,seq,role,content,reasoning,tool_calls,tool_call_id,created_at,parent_id,images,status,original_extra) VALUES(?1,'session',?2,?3,?4,'Separate original reasoning',?5,?6,'2026-01-01T00:00:06.789Z',?7,?8,?9,X'00FF0102')",params![id,seq,role,content,calls,call_id,parent,images,status]).unwrap();
    }
    // All external-content indexes reconcile, including the unchanged artifact
    // index. This prevents the earlier insert-only synthetic-trigger pitfall.
    for table in ["memory_fts", "lessons_fts", "artifact_index_fts"] {
        db.execute_batch(&format!("INSERT INTO {table}({table}) VALUES('rebuild'); INSERT INTO {table}({table},rank) VALUES('integrity-check',1);")).unwrap();
    }
    let integrity: String = db
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    db.close().unwrap();
    assert!(!std::fs::read(path.join("smith.db"))
        .unwrap()
        .starts_with(b"SQLite format 3\0"));
    println!("Synthetic encrypted historical-message fixture v1 created and checked.");
}
