//! Explicit local memory management. These records enter through a user action
//! or a separately approved scoped tool; browsing/rating a chat never ingests it.
use crate::*;

const PAGE: i64 = 32;
const MAX_FACT: usize = 8192;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/memory", get(list).post(create))
        .route("/api/memory/{id}", axum::routing::patch(change))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    #[serde(default)]
    query: String,
    namespace: Option<String>,
    before: Option<i64>,
    #[serde(default)]
    deleted: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMemory {
    namespace: String,
    fact: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    SetPinned {
        version: i64,
        pinned: bool,
    },
    Trash {
        version: i64,
    },
    Restore {
        version: i64,
    },
    Edit {
        version: i64,
        fact: String,
        tags: Vec<String>,
    },
}

fn owner(db: &Connection) -> ApiResult<String> {
    Ok(db.query_row(
        "SELECT principal_id FROM agent_profile WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}

fn validate_text(fact: &str, tags: &[String]) -> ApiResult<()> {
    if fact.trim().is_empty()
        || fact.len() > MAX_FACT
        || tags.len() > 32
        || tags
            .iter()
            .any(|tag| tag.trim().is_empty() || tag.len() > 64 || tag.chars().any(char::is_control))
    {
        return Err(bad(
            "Memory needs 1–8192 bytes of text and at most 32 tags of 1–64 bytes.",
        ));
    }
    if serde_json::to_vec(tags)
        .map_err(|_| bad("Invalid memory tags."))?
        .len()
        > 4096
    {
        return Err(bad(
            "Memory tags exceed the 4096-byte encoded storage limit. Use fewer or shorter tags.",
        ));
    }
    Ok(())
}

fn validate_namespace(namespace: &str) -> ApiResult<()> {
    if namespace.is_empty()
        || namespace.len() > 64
        || !namespace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(bad(
            "Choose a memory namespace of 1–64 letters, numbers, dots, hyphens or underscores.",
        ));
    }
    Ok(())
}

fn read(db: &Connection, id: &str) -> ApiResult<Value> {
    let row = db.query_row(
        "SELECT m.id,m.namespace,substr(m.content,1,8193),substr(m.tags_json,1,4097),m.version,COALESCE(s.pinned,0),COALESCE(s.deleted,0),m.updated_at FROM agent_memories m LEFT JOIN memory_metadata s ON s.memory_id=m.id WHERE m.id=?1 AND m.principal_id=?2",
        params![id,owner(db)?], row,
    ).optional()?;
    row.ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "Memory record not found in this workspace.".into(),
        )
    })
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let fact: String = r.get(2)?;
    let tags: String = r.get(3)?;
    if fact.len() > MAX_FACT || tags.len() > 4096 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let tags: Vec<String> =
        serde_json::from_str(&tags).map_err(|_| rusqlite::Error::InvalidQuery)?;
    if tags.len() > 32 || tags.iter().any(|tag| tag.len() > 64) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(
        json!({"id":r.get::<_,String>(0)?,"namespace":r.get::<_,String>(1)?,"fact":fact,"tags":tags,"version":r.get::<_,i64>(4)?,"pinned":r.get::<_,bool>(5)?,"deleted":r.get::<_,bool>(6)?,"updated_at":r.get::<_,i64>(7)?}),
    )
}

fn page(db: &Connection, filter: Filter) -> ApiResult<Value> {
    if let Some(namespace) = &filter.namespace {
        validate_namespace(namespace)?;
    }
    if filter.query.len() > 1024 || filter.before.is_some_and(|n| n <= 0) {
        return Err(bad(
            "Use a query of at most 1024 bytes and the returned memory cursor.",
        ));
    }
    // A stable monotonically decreasing rowid cursor keeps new records out of an
    // in-progress page sequence. Pinning does not reorder or repeat pages.
    let mut statement = db.prepare("SELECT m.id,m.namespace,substr(m.content,1,8193),substr(m.tags_json,1,4097),m.version,COALESCE(s.pinned,0),COALESCE(s.deleted,0),m.updated_at,m.rowid FROM agent_memories m LEFT JOIN memory_metadata s ON s.memory_id=m.id WHERE m.principal_id=?1 AND COALESCE(s.deleted,0)=?2 AND (?3 IS NULL OR m.rowid<?3) AND instr(lower(m.content),lower(?4))>0 AND (?6 IS NULL OR m.namespace=?6) ORDER BY m.rowid DESC LIMIT ?5")?;
    let mut rows = statement
        .query_map(
            params![
                owner(db)?,
                filter.deleted,
                filter.before,
                filter.query,
                PAGE + 1,
                filter.namespace
            ],
            |r| Ok((row(r)?, r.get::<_, i64>(8)?)),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let more = rows.len() > PAGE as usize;
    rows.truncate(PAGE as usize);
    let next = if more { rows.last().map(|r| r.1) } else { None };
    Ok(
        json!({"memories":rows.into_iter().map(|r|r.0).collect::<Vec<_>>(),"next_before":next,"matching":"literal_ascii_case_insensitive","deleted":filter.deleted}),
    )
}

fn insert(db: &mut Connection, input: NewMemory) -> ApiResult<Value> {
    validate_text(&input.fact, &input.tags)?;
    validate_namespace(&input.namespace)?;
    let tx = db.transaction()?;
    let principal = owner(&tx)?;
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM agent_memories WHERE principal_id=?1",
        [&principal],
        |r| r.get(0),
    )?;
    if count >= 10000 {
        return Err(bad(
            "This workspace has reached its 10000-record memory limit, including Trash.",
        ));
    }
    let id = crate::id();
    tx.execute("INSERT INTO agent_memories(id,principal_id,namespace,content,tags_json,created_at,updated_at,version) VALUES(?1,?2,?3,?4,?5,?6,?6,1)",params![id,principal,input.namespace,input.fact,serde_json::to_string(&input.tags).map_err(|_|bad("Invalid memory tags."))?,now()])?;
    let result = read(&tx, &id)?;
    tx.commit()?;
    Ok(result)
}

fn mutate(db: &mut Connection, id: &str, input: Change) -> ApiResult<Value> {
    let version = match &input {
        Change::SetPinned { version, .. }
        | Change::Trash { version }
        | Change::Restore { version }
        | Change::Edit { version, .. } => *version,
    };
    if version < 1 {
        return Err(bad("Use the current memory version."));
    }
    if let Change::Edit { fact, tags, .. } = &input {
        validate_text(fact, tags)?;
    }
    let tx = db.transaction()?;
    let current = read(&tx, id)?;
    if current["version"].as_i64() != Some(version) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "This memory changed. Reload it before editing.".into(),
        ));
    }
    let was_deleted = current["deleted"].as_bool().unwrap();
    let mut pinned = current["pinned"].as_bool().unwrap();
    let mut deleted = was_deleted;
    let changed;
    match input {
        Change::SetPinned { pinned: value, .. } if !was_deleted => {
            changed = pinned != value;
            pinned = value;
        }
        Change::Trash { .. } => {
            changed = !deleted;
            deleted = true;
        }
        Change::Restore { .. } => {
            changed = deleted;
            deleted = false;
        }
        Change::Edit { fact, tags, .. } if !was_deleted => {
            changed =
                current["fact"].as_str() != Some(fact.as_str()) || current["tags"] != json!(tags);
            if changed {
                tx.execute(
                    "UPDATE agent_memories SET content=?1,tags_json=?2 WHERE id=?3",
                    params![
                        fact,
                        serde_json::to_string(&tags).map_err(|_| bad("Invalid memory tags."))?,
                        id
                    ],
                )?;
            }
        }
        _ => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Restore this memory from Trash before editing or pinning it.".into(),
            ))
        }
    }
    if changed {
        let next = version
            .checked_add(1)
            .ok_or_else(|| bad("Memory version limit reached."))?;
        tx.execute("INSERT INTO memory_metadata(memory_id,pinned,deleted,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(memory_id) DO UPDATE SET pinned=excluded.pinned,deleted=excluded.deleted,updated_at=excluded.updated_at",params![id,pinned,deleted,now()])?;
        tx.execute(
            "UPDATE agent_memories SET version=?1,updated_at=?2 WHERE id=?3",
            params![next, now(), id],
        )?;
    }
    let result = read(&tx, id)?;
    tx.commit()?;
    Ok(result)
}

async fn list(State(app): State<Arc<App>>, Query(filter): Query<Filter>) -> ApiResult<Json<Value>> {
    Ok(Json(page(&app.db.lock().unwrap(), filter)?))
}
async fn create(
    State(app): State<Arc<App>>,
    Json(input): Json<NewMemory>,
) -> ApiResult<Json<Value>> {
    Ok(Json(insert(&mut app.db.lock().unwrap(), input)?))
}
async fn change(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<Change>,
) -> ApiResult<Json<Value>> {
    Ok(Json(mutate(&mut app.db.lock().unwrap(), &id, input)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "foreign_keys", true).unwrap();
        smith_profile_storage::initialize_schema(&db).unwrap();
        db
    }
    fn record(n: usize) -> NewMemory {
        NewMemory {
            namespace: "facts".into(),
            fact: format!("Synthetic fact {n}"),
            tags: vec!["synthetic".into()],
        }
    }
    #[test]
    fn pin_trash_restore_are_versioned_and_keep_the_fact() {
        let mut db = fixture();
        let saved = insert(&mut db, record(1)).unwrap();
        let id = saved["id"].as_str().unwrap();
        let pinned = mutate(
            &mut db,
            id,
            Change::SetPinned {
                version: 1,
                pinned: true,
            },
        )
        .unwrap();
        assert_eq!(pinned["version"], 2);
        assert!(mutate(&mut db, id, Change::Trash { version: 1 }).is_err());
        let trashed = mutate(&mut db, id, Change::Trash { version: 2 }).unwrap();
        assert_eq!(trashed["version"], 3);
        assert_eq!(page(&db, Filter::default()).unwrap()["memories"], json!([]));
        let trash = page(
            &db,
            Filter {
                deleted: true,
                ..Filter::default()
            },
        )
        .unwrap();
        assert_eq!(trash["memories"][0]["fact"], saved["fact"]);
        assert!(mutate(
            &mut db,
            id,
            Change::SetPinned {
                version: 3,
                pinned: false
            }
        )
        .is_err());
        let restored = mutate(&mut db, id, Change::Restore { version: 3 }).unwrap();
        assert_eq!(restored["version"], 4);
        assert_eq!(restored["pinned"], true);
        let unchanged = mutate(&mut db, id, Change::Restore { version: 4 }).unwrap();
        assert_eq!(unchanged["version"], 4);
        assert!(mutate(&mut db, id, Change::Trash { version: 2 }).is_err());
    }
    #[test]
    fn stable_pages_exclude_new_rows_and_search_is_literal() {
        let mut db = fixture();
        for n in 0..35 {
            insert(&mut db, record(n)).unwrap();
        }
        let first = page(&db, Filter::default()).unwrap();
        assert_eq!(first["memories"].as_array().unwrap().len(), 32);
        let before = first["next_before"].as_i64();
        assert!(before.is_some());
        insert(&mut db, record(99)).unwrap();
        let last = page(
            &db,
            Filter {
                before,
                ..Filter::default()
            },
        )
        .unwrap();
        assert_eq!(last["memories"].as_array().unwrap().len(), 3);
        assert!(last["next_before"].is_null());
        let result = page(
            &db,
            Filter {
                query: "FACT 99".into(),
                ..Filter::default()
            },
        )
        .unwrap();
        assert_eq!(result["memories"].as_array().unwrap().len(), 1);
        assert_eq!(
            page(
                &db,
                Filter {
                    query: "%".into(),
                    ..Filter::default()
                }
            )
            .unwrap()["memories"],
            json!([])
        );
        assert!(page(
            &db,
            Filter {
                before: Some(0),
                ..Filter::default()
            }
        )
        .is_err());
        assert!(insert(
            &mut db,
            NewMemory {
                namespace: "bad namespace".into(),
                ..record(0)
            }
        )
        .is_err());
    }
    #[test]
    fn namespace_is_exact_and_encoded_tag_bound_refuses_without_write() {
        let mut db = fixture();
        insert(&mut db, record(1)).unwrap();
        insert(
            &mut db,
            NewMemory {
                namespace: "facts-two".into(),
                ..record(2)
            },
        )
        .unwrap();
        let scoped = page(
            &db,
            Filter {
                namespace: Some("facts".into()),
                ..Filter::default()
            },
        )
        .unwrap();
        assert_eq!(scoped["memories"].as_array().unwrap().len(), 1);
        assert_eq!(scoped["memories"][0]["namespace"], "facts");
        assert!(page(
            &db,
            Filter {
                namespace: Some("".into()),
                ..Filter::default()
            }
        )
        .is_err());
        let error = insert(
            &mut db,
            NewMemory {
                tags: vec!["\"".repeat(64); 32],
                ..record(3)
            },
        )
        .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("4096"));
        assert_eq!(
            page(&db, Filter::default()).unwrap()["memories"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn encrypted_reopen_keeps_trash_and_stale_versions_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .canonicalize()
            .unwrap()
            .join(smith_profile_storage::DB_FILE);
        let key =
            smith_profile_storage::Secret::new("Synthetic memory restart key".into()).unwrap();
        let mut db = smith_profile_storage::create(&path, &key).unwrap();
        let saved = insert(&mut db, record(7)).unwrap();
        let id = saved["id"].as_str().unwrap().to_string();
        mutate(&mut db, &id, Change::Trash { version: 1 }).unwrap();
        smith_profile_storage::close(db).unwrap();
        assert!(!std::fs::read(&path)
            .unwrap()
            .windows(b"Synthetic fact 7".len())
            .any(|v| v == b"Synthetic fact 7"));
        let mut db = smith_profile_storage::unlock(&path, &key).unwrap();
        assert_eq!(read(&db, &id).unwrap()["deleted"], true);
        assert!(mutate(&mut db, &id, Change::Restore { version: 1 }).is_err());
        let restored = mutate(&mut db, &id, Change::Restore { version: 2 }).unwrap();
        assert_eq!(restored["version"], 3);
        smith_profile_storage::close(db).unwrap();
    }
}
