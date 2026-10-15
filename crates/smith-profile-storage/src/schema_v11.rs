//! Immutable requested model intent, captured with the accepted run. No backfill:
//! absence on historical runs means unknown, never an inferred model setting.
//! The host validates the bounded versioned DTO; storage grants no authority.
pub(crate) const SQL: &str = "
CREATE TABLE run_model_provenance(run_id TEXT PRIMARY KEY NOT NULL REFERENCES run_configs(run_id) ON DELETE CASCADE,format_version INTEGER NOT NULL CHECK(format_version=1),provenance TEXT NOT NULL CHECK(typeof(provenance)='text' AND length(CAST(provenance AS BLOB)) BETWEEN 2 AND 16384));
";

pub(crate) const REQUIRED: &[(&str, &[&str])] = &[(
    "run_model_provenance",
    &["run_id", "format_version", "provenance"],
)];

#[cfg(test)]
mod tests {
    use crate::{schema, Connection, Secret, DB_FILE, SCHEMA_VERSION};
    use std::path::{Path, PathBuf};

    fn key() -> Secret {
        Secret::new("Synthetic schema11 provenance key".into()).unwrap()
    }

    fn historical(conflict: bool) -> (tempfile::TempDir, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().canonicalize().unwrap().join(DB_FILE);
        let db = crate::open_cipher(&path, &key(), true).unwrap();
        schema::initialize_schema(&db).unwrap();
        db.execute_batch("DROP TABLE run_model_provenance; PRAGMA user_version=10;
            INSERT INTO chats(rowid,id,title,created_at,updated_at,version,title_locked,activity) VALUES(-17,'chat','Retained original chat',1,2,7,1,3);
            INSERT INTO run_configs(rowid,run_id,provider,endpoint,model,finish_reason,transport_incomplete) VALUES(43,'old','mlx','mlx:retained','Retained model','stop',0);
            INSERT INTO messages(rowid,id,chat_id,role,content,status,created_at,run_id) VALUES(701,'answer','chat','assistant','Retained exact answer','completed',2,'old');
            INSERT INTO message_branches VALUES('answer',NULL);
            INSERT INTO conversation_heads VALUES('chat','answer');
            INSERT INTO settings VALUES('opaque','Retained unknown value');").unwrap();
        if conflict {
            db.execute_batch("CREATE TABLE run_model_provenance(sentinel BLOB); INSERT INTO run_model_provenance VALUES(x'00ff0102');").unwrap();
        }
        schema::install_storage(&db).unwrap();
        crate::close(db).unwrap();
        (temporary, path)
    }

    fn run(db: &Connection, id: &str) {
        db.execute("INSERT INTO run_configs(run_id,provider,endpoint,model) VALUES(?1,'mlx','mlx:synthetic','synthetic')", [id]).unwrap();
    }

    fn generation(db: &Connection) -> i64 {
        schema::identity(db).unwrap().1
    }

    fn bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    #[test]
    fn schema10_upgrade_preserves_identity_rows_and_unknown_old_intent() {
        let (_temporary, path) = historical(false);
        let db = crate::open_cipher(&path, &key(), false).unwrap();
        let before = schema::identity(&db).unwrap();
        crate::close(db).unwrap();
        let db = crate::unlock(&path, &key()).unwrap();
        assert_eq!(
            schema::identity(&db).unwrap(),
            (before.0.clone(), before.1 + 1)
        );
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        let preserved:(i64,i64,i64,String,String,String)=db.query_row("SELECT c.rowid,m.rowid,r.rowid,m.content,r.finish_reason,s.value FROM chats c JOIN messages m ON m.chat_id=c.id JOIN run_configs r ON r.run_id=m.run_id JOIN settings s ON s.key='opaque'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        assert_eq!(
            preserved,
            (
                -17,
                701,
                43,
                "Retained exact answer".into(),
                "stop".into(),
                "Retained unknown value".into()
            )
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM run_model_provenance", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        schema::validate(&db).unwrap();
        crate::close(db).unwrap();
        let db = crate::unlock(&path, &key()).unwrap();
        assert_eq!(schema::identity(&db).unwrap(), (before.0, before.1 + 1));
        crate::close(db).unwrap();
    }

    #[test]
    fn schema11_rehearsal_conflict_and_wrong_key_preserve_original_bytes() {
        let (temporary, path) = historical(true);
        let original = bytes(&path);
        assert!(crate::unlock(&path, &Secret::new("Wrong synthetic key".into()).unwrap()).is_err());
        assert_eq!(bytes(&path), original);
        assert!(crate::unlock(&path, &key()).is_err());
        assert_eq!(bytes(&path), original);
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 1);
        let db = crate::open_cipher(&path, &key(), false).unwrap();
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            10
        );
        assert_eq!(
            db.query_row("SELECT sentinel FROM run_model_provenance", [], |r| r
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            [0, 255, 1, 2]
        );
        schema::validate(&db).unwrap();
        crate::close(db).unwrap();
    }

    #[test]
    fn schema11_future_or_corrupt_profile_is_not_reset() {
        for corrupt in [false, true] {
            let (_temporary, path) = historical(false);
            if corrupt {
                let mut data = bytes(&path);
                data[100] ^= 0x80;
                std::fs::write(&path, data).unwrap();
            } else {
                let db = crate::open_cipher(&path, &key(), false).unwrap();
                db.pragma_update(None, "user_version", 12).unwrap();
                crate::close(db).unwrap();
            }
            let original = bytes(&path);
            assert!(crate::unlock(&path, &key()).is_err());
            assert_eq!(bytes(&path), original);
        }
    }

    #[test]
    fn provenance_is_atomic_audited_bounded_and_preserved_across_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().canonicalize().unwrap().join(DB_FILE);
        let mut db = crate::create(&path, &key()).unwrap();
        let before = generation(&db);
        {
            let tx = db.transaction().unwrap();
            run(&tx, "aborted");
            tx.execute(
                "INSERT INTO run_model_provenance VALUES('aborted',1,'{}')",
                [],
            )
            .unwrap();
            assert_eq!(generation(&tx), before + 2);
        }
        assert_eq!(generation(&db), before);
        assert_eq!(
            db.query_row("SELECT count(*) FROM run_configs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let payload = "{\"synthetic_requested_intent\":\"retained, not an execution grant\"}";
        {
            let tx = db.transaction().unwrap();
            run(&tx, "kept");
            tx.execute(
                "INSERT INTO run_model_provenance VALUES('kept',1,?1)",
                [payload],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(generation(&db), before + 2);
        assert!(db
            .execute("INSERT INTO run_model_provenance VALUES('kept',1,'{}')", [])
            .is_err());
        assert!(db
            .execute(
                "INSERT INTO run_model_provenance VALUES('missing',1,'{}')",
                []
            )
            .is_err());
        for (version, text) in [
            (2, "{}".to_string()),
            (1, "x".to_string()),
            (1, "x".repeat(16385)),
            (1, "🦀".repeat(4097)),
        ] {
            assert!(db.execute("UPDATE run_model_provenance SET format_version=?1,provenance=?2 WHERE run_id='kept'",rusqlite::params![version,text]).is_err());
        }
        assert!(db
            .execute(
                "UPDATE run_model_provenance SET provenance=x'7b7d' WHERE run_id='kept'",
                []
            )
            .is_err());
        assert_eq!(generation(&db), before + 2);
        schema::validate(&db).unwrap();
        crate::close(db).unwrap();
        assert!(!bytes(&path)
            .windows(payload.len())
            .any(|window| window == payload.as_bytes()));
        let db = crate::unlock(&path, &key()).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT format_version,provenance FROM run_model_provenance WHERE run_id='kept'",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            )
            .unwrap(),
            (1, payload.into())
        );
        assert_eq!(generation(&db), before + 2);
        db.execute(
            "UPDATE run_model_provenance SET provenance='{}' WHERE run_id='kept'",
            [],
        )
        .unwrap();
        db.execute(
            "UPDATE run_model_provenance SET provenance=?1 WHERE run_id='kept'",
            [payload],
        )
        .unwrap();
        assert_eq!(generation(&db), before + 4);
        db.execute("DELETE FROM run_configs WHERE run_id='kept'", [])
            .unwrap();
        assert_eq!(generation(&db), before + 6);
        assert_eq!(
            db.query_row("SELECT count(*) FROM run_model_provenance", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        crate::close(db).unwrap();
    }
}
