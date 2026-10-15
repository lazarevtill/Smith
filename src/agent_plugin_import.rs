//! User-selected declarative plugin imports. Reviews are transient read custody;
//! encrypted receipts are durable exact-retry evidence, never tool authority.
use super::*;
use smith_agent_core::SelectedPlugin;
use std::{
    collections::HashMap,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

const REVIEW_LIFETIME: Duration = Duration::from_secs(300);
const MAX_REVIEWS: usize = 32;

#[derive(Default)]
pub(crate) struct ReviewCache {
    reviews: Mutex<HashMap<String, Arc<Review>>>,
}
struct Review {
    session: String,
    created: Instant,
    selected: SelectedPlugin,
    request: Mutex<Option<String>>,
}
impl Review {
    fn valid(&self, session: &str) -> Result<()> {
        if self.session != session || self.created.elapsed() > REVIEW_LIFETIME {
            return Err(Error::Conflict);
        }
        Ok(())
    }
    fn claim(&self, session: &str, request: &str) -> Result<()> {
        self.valid(session)?;
        let mut claimed = self
            .request
            .lock()
            .map_err(|_| Error::Persistence("Plugin review unavailable".into()))?;
        if claimed.as_deref().is_some_and(|prior| prior != request) {
            return Err(Error::Conflict);
        }
        *claimed = Some(request.into());
        Ok(())
    }
}
impl ReviewCache {
    fn insert(&self, session: &str, selected: SelectedPlugin) -> Result<(String, Arc<Review>)> {
        let mut reviews = self
            .reviews
            .lock()
            .map_err(|_| Error::Persistence("Plugin review cache unavailable".into()))?;
        // No oldest-entry eviction: a fresh review remains usable for its whole
        // lifetime. An in-flight import owns an Arc even if an expired cache
        // entry is removed by a later review.
        reviews.retain(|_, review| review.valid(session).is_ok());
        if reviews.len() >= MAX_REVIEWS {
            return Err(Error::LimitExceeded(
                "Pending plugin reviews (32); complete an import or wait for a review to expire"
                    .into(),
            ));
        }
        let id = crate::id();
        let review = Arc::new(Review {
            session: session.into(),
            created: Instant::now(),
            selected,
            request: Mutex::new(None),
        });
        reviews.insert(id.clone(), review.clone());
        Ok((id, review))
    }
    fn get(&self, id: &str, session: &str) -> Result<Arc<Review>> {
        let reviews = self
            .reviews
            .lock()
            .map_err(|_| Error::Persistence("Plugin review cache unavailable".into()))?;
        let review = reviews.get(id).ok_or(Error::NotFound)?.clone();
        review.valid(session)?;
        Ok(review)
    }
    fn remove(&self, id: &str) {
        if let Ok(mut reviews) = self.reviews.lock() {
            reviews.remove(id);
        }
    }
    pub(crate) fn clear(&self) {
        if let Ok(mut reviews) = self.reviews.lock() {
            reviews.clear();
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    directory: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Import {
    request_id: String,
    review_id: String,
    expected_source_sha256: String,
}
pub(super) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/agent/extensions/plugin-review", post(review))
        .route("/api/agent/extensions/plugin-import", post(import))
}
fn definition(selected: &SelectedPlugin) -> Definition {
    let m = selected.manifest();
    Definition::Plugin {
        manifest_id: m.id.clone(),
        name: m.name.clone(),
        description: m.description.clone(),
        version: m.version.clone(),
        authors: m.authors.clone(),
        homepage: m.homepage.clone(),
        license: m.license.clone(),
        instructions: m.instructions.clone(),
        instructions_file: m.instructions_file.clone(),
        tools: m.tools.clone(),
    }
}
async fn review(State(app): State<Arc<App>>, input: Body<Selection>) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    agent_api::work(app,None,move|app|{
        agent_api::ensure_open(&app)?;
        let selected=SelectedPlugin::inspect(Path::new(&input.directory))?;
        let definition=definition(&selected);definition.validate()?;
        agent_api::ensure_open(&app)?;
        let (id,review)=app.agent.plugin_imports.insert(app.profile_session(),selected)?;
        Ok(Json(json!({"review_id":id,"source_sha256":review.selected.evidence().source_sha256,"definition":definition,"provenance":Origin::SelectedDirectory{source:review.selected.evidence().clone()},"expires_in_seconds":REVIEW_LIFETIME.as_secs(),"enabled_on_import":false,"execution":"none","trust":"explicit_local_selection_not_publisher_verification"})))
    }).await
}
fn receipt(db: &Connection, owner: &str, id: &str, hash: &str) -> Result<Option<Value>> {
    let prior:Option<(String,String)>=db.query_row("SELECT request_sha256,result_json FROM agent_extension_imports WHERE request_id=?1 AND principal_id=?2",params![id,owner],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
    prior
        .map(|(stored, result)| {
            if stored != hash {
                return Err(Error::Conflict);
            }
            bounded(&result, 64 * 1024, "Stored plugin import receipt")?;
            let mut result: Value = serde_json::from_str(&result).map_err(|_| {
                Error::Persistence("Stored plugin import receipt is invalid".into())
            })?;
            result["replayed"] = json!(true);
            Ok(result)
        })
        .transpose()
}
async fn import(State(app): State<Arc<App>>, input: Body<Import>) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    uuid(&input.request_id)?;
    uuid(&input.review_id)?;
    if input.expected_source_sha256.len() != 64
        || !input
            .expected_source_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(
            Error::InvalidInput("Expected the reviewed plugin source digest".into()).into(),
        );
    }
    let hash = arguments_sha256(&json!({"operation":"selected_plugin_import_v1","input":input}));
    agent_api::work(app,None,move|app|{
        agent_api::ensure_open(&app)?;let store=AgentStore::new(&app)?;
        let replay=||store.with_db(|db|receipt(db,store.owner().id(),&input.request_id,&hash));
        if let Some(result)=replay()? { return Ok(Json(result)); }
        let reviewed=match app.agent.plugin_imports.get(&input.review_id,app.profile_session()) {
            Ok(review)=>review,
            Err(error)=>return match replay()? { Some(result)=>Ok(Json(result)),None=>Err(error.into()) },
        };
        if reviewed.selected.evidence().source_sha256!=input.expected_source_sha256 { return Err(Error::Conflict.into()); }
        reviewed.claim(app.profile_session(),&input.request_id)?;
        reviewed.selected.revalidate()?;
        reviewed.valid(app.profile_session())?;
        agent_api::ensure_open(&app)?;
        let definition=definition(&reviewed.selected);definition.validate()?;
        let origin=Origin::SelectedDirectory{source:reviewed.selected.evidence().clone()};
        let result=store.with_db(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
            if let Some(result)=receipt(&tx,store.owner().id(),&input.request_id,&hash)? { return Ok(result); }
            reviewed.valid(app.profile_session())?;
            let count:i64=tx.query_row("SELECT count(*) FROM agent_extension_imports WHERE principal_id=?1",[store.owner().id()],|r|r.get(0)).map_err(db_error)?;
            if count>=1024 { return Err(Error::LimitExceeded("Retained extension import receipts (1024)".into())); }
            // Source bytes after the final recheck cannot alter this snapshot.
            // No filesystem operation or user-provided authority occurs in TX.
            let record=insert(&tx,store.owner().id(),&definition,false,&origin)?;
            let result=json!({"extension":record.view(),"replayed":false,"execution":"none"});
            tx.execute("INSERT INTO agent_extension_imports(request_id,principal_id,source_sha256,request_sha256,extension_id,result_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![input.request_id,store.owner().id(),input.expected_source_sha256,hash,record.id,encode(&result)?,crate::now()]).map_err(db_error)?;
            tx.commit().map_err(db_error)?;Ok(result)
        })?;
        app.agent.plugin_imports.remove(&input.review_id);
        Ok(Json(result))
    }).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn cache_bounds_expiry_session_and_single_request_claim_preserve_owned_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        std::fs::write(
            path.join("plugin.json"),
            r#"{"id":"fixture","name":"Fixture","instructions":"Exact"}"#,
        )
        .unwrap();
        let cache = ReviewCache::default();
        let mut first = None;
        for _ in 0..MAX_REVIEWS {
            let entry = cache
                .insert("session-a", SelectedPlugin::inspect(&path).unwrap())
                .unwrap();
            if first.is_none() {
                first = Some(entry);
            }
        }
        assert!(matches!(
            cache.insert("session-a", SelectedPlugin::inspect(&path).unwrap()),
            Err(Error::LimitExceeded(_))
        ));
        let (id, owned) = first.unwrap();
        assert!(cache.get(&id, "session-b").is_err());
        owned.claim("session-a", "request-a").unwrap();
        owned.claim("session-a", "request-a").unwrap();
        assert!(matches!(
            owned.claim("session-a", "request-b"),
            Err(Error::Conflict)
        ));
        cache.clear();
        assert!(cache.get(&id, "session-a").is_err());
        assert_eq!(
            owned.selected.manifest().instructions.as_deref(),
            Some("Exact")
        );
        let expired = Arc::new(Review {
            session: "session-a".into(),
            created: Instant::now() - REVIEW_LIFETIME - Duration::from_secs(1),
            selected: SelectedPlugin::inspect(&path).unwrap(),
            request: Mutex::new(None),
        });
        cache
            .reviews
            .lock()
            .unwrap()
            .insert("expired".into(), expired.clone());
        assert!(expired.claim("session-a", "request-a").is_err());
        cache
            .insert("session-a", SelectedPlugin::inspect(&path).unwrap())
            .unwrap();
        assert!(!cache.reviews.lock().unwrap().contains_key("expired"));
        assert_eq!(
            expired.selected.manifest().instructions.as_deref(),
            Some("Exact")
        );
    }
}
