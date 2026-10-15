//! Separate disposable historical-memory + valid extension fixture.
//! Earlier historical fixture source and executable remain unchanged.
//! Public destination argument, synthetic master key on stdin only.
#[path = "../tests/support/legacy_fixture.rs"]
mod fixture;
use std::io::Read;
fn main() {
    let path = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .expect("new synthetic destination"),
    );
    assert!(!path.exists(), "Synthetic destination must be new");
    let mut input = zeroize::Zeroizing::new(String::new());
    std::io::stdin()
        .take(66)
        .read_to_string(&mut input)
        .unwrap();
    let value = input.trim();
    assert!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "Synthetic key must be hex"
    );
    let mut master = zeroize::Zeroizing::new([0u8; 32]);
    for (n, byte) in master.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[n * 2..n * 2 + 2], 16).unwrap();
    }
    fixture::create(&path, &master);
    let db = fixture::connection(&path.join("smith.db"), &master);
    db.execute_batch("
ALTER TABLE memory_episodes ADD COLUMN original_extra BLOB;
UPDATE memory_episodes SET agent_id='agent',session_id='session',summary='Synthetic agent episode',salience=-0.125,embedding=X'0000803F00000080',created_at='2025-01-02T03:04:05.123+03:00',valid_to='2099-01-01T00:00:00Z',pinned=1,original_extra=X'00FF';
INSERT INTO memory_episodes(rowid,id,agent_id,session_id,summary,salience,embedding,created_at,valid_to,pinned,original_extra) VALUES(-19,'global-episode',NULL,NULL,'Synthetic global episode',0.75,X'01','2025-01-01T00:00:00Z',NULL,0,X'FF');
INSERT INTO memory_episodes(id,agent_id,session_id,summary,salience,embedding,created_at,valid_to,pinned) VALUES('retired-episode','agent','session','Synthetic superseded episode',0.0,NULL,'2024-01-01T00:00:00Z','2025-01-01T00:00:00Z',0);
ALTER TABLE lessons ADD COLUMN original_extra TEXT;
UPDATE lessons SET agent_id='agent',polarity='cautionary',trigger='Synthetic caution trigger',root_cause='Original diagnosis',corrected_behavior='Original dormant advice',scope='free text; no tool grant',source_session_id='session',embedding=X'0000C07F00000080',usage_count=9223372036854775807,success_count=7,valid_to=NULL,created_at='2025-01-02T03:04:05.789+03:00',original_extra=CAST(X'FF00' AS TEXT);
INSERT INTO lessons(id,agent_id,polarity,trigger,root_cause,corrected_behavior,scope,source_session_id,embedding,usage_count,success_count,valid_to,created_at) VALUES('global-lesson',NULL,'guiding','Synthetic global guiding trigger','Original cause','Dormant positive pattern','historical domain',NULL,NULL,3,2,'2025-01-01T00:00:00Z','2024-01-01T00:00:00Z');
INSERT INTO lessons(id,agent_id,polarity,trigger,root_cause,corrected_behavior,scope,source_session_id,embedding,usage_count,success_count,valid_to,created_at) VALUES('unknown-lesson','agent','unknown-original-polarity','Synthetic unknown lesson','Original cause','Dormant advice','historical domain','session',X'',0,0,'2099-01-01T00:00:00Z','2025-01-01T00:00:00Z');
").unwrap();
    db.execute_batch(r#"
UPDATE skills SET name='Synthetic retained skill',description='Original synthetic skill description',instructions='Summarize synthetic facts without granting tools.',category='synthetic',enabled=1,created_at='2025-01-01T00:00:00Z';
UPDATE methods SET name='Synthetic retained method',description='Original synthetic method description',instructions='Inspect then summarize the supplied synthetic input.',enabled=1,created_at='2025-01-01T00:00:00Z';
UPDATE tool_recipes SET name='synthetic_retained_recipe',description='Historical recipe stays unsupported and disabled after import',command='printf historical-fixture-only',parameters='{"type":"object","properties":{"value":{"type":"string"}},"required":[],"additionalProperties":false}',enabled=1,created_at='2025-01-01T00:00:00Z';
"#).unwrap();
    drop(db);
    println!("Synthetic encrypted historical-memory and extension fixture created.");
}
