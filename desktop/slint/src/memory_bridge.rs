//! Explicit local memory curation. One bounded page per actor command; no model,
//! ingestion, ambient namespace, hidden retry or inferred successful write.
use crate::bridge::{encode_component, Bridge, Result, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub const PAGE_SIZE: usize = 32;
pub const MAX_RECORDS: usize = 10_000;
pub const MAX_PAGES: usize = 313;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryFilter {
    /// None explicitly browses all namespaces owned by this workspace.
    pub namespace: Option<String>,
    pub query: String,
    pub deleted: bool,
}
impl MemoryFilter {
    fn validate(&self) -> Result<()> {
        if let Some(namespace) = &self.namespace {
            validate_namespace(namespace)?;
        }
        if self.query.len() > 1024 {
            return Err("Use a literal memory query of at most 1024 bytes.".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecord {
    pub id: String,
    pub namespace: String,
    pub fact: String,
    pub tags: Vec<String>,
    pub version: i64,
    pub pinned: bool,
    pub deleted: bool,
    pub updated_at: i64,
}
fn validate_namespace(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err("Choose an explicit namespace of 1–64 ASCII letters, numbers, dots, hyphens or underscores.".into());
    }
    Ok(())
}
fn validate_text(fact: &str, tags: &[String]) -> Result<()> {
    if fact.trim().is_empty()
        || fact.len() > 8192
        || tags.len() > 32
        || tags
            .iter()
            .any(|tag| tag.trim().is_empty() || tag.len() > 64 || tag.chars().any(char::is_control))
        || serde_json::to_vec(tags)
            .map_err(|_| "Invalid memory tags.")?
            .len()
            > 4096
    {
        return Err("Use 1–8192 bytes of memory text and at most 32 nonempty tags of 1–64 bytes (4096 encoded bytes total).".into());
    }
    Ok(())
}
fn validate_record(record: &MemoryRecord) -> Result<()> {
    validate_namespace(&record.namespace)?;
    validate_text(&record.fact, &record.tags)?;
    if record.id.is_empty()
        || record.id.len() > 128
        || record.id.chars().any(char::is_control)
        || record.version < 1
    {
        return Err("Invalid memory record identity or version.".into());
    }
    Ok(())
}
/// Opaque, immutable continuation tied to its original session and filter.
#[derive(Clone)]
pub struct MemoryCursor {
    used: Arc<AtomicBool>,
    session: String,
    filter: MemoryFilter,
    before: i64,
    pages: usize,
    seen: BTreeSet<String>,
}
#[derive(Clone)]
pub struct MemoryPage {
    pub filter: MemoryFilter,
    pub memories: Vec<MemoryRecord>,
    pub next: Option<MemoryCursor>,
    session: String,
    reviews: Vec<MemoryReview>,
}
impl MemoryPage {
    /// Only an exact row from this captured page can be reviewed. Cloned pages
    /// share the original one-use guard, including after an uncertain write.
    pub fn review(&self, record: &MemoryRecord) -> Result<MemoryReview> {
        self.reviews
            .iter()
            .find(|review| review.record == *record)
            .cloned()
            .ok_or_else(|| "This record was not fetched in this memory page. Refresh it.".into())
    }
    pub fn editor(&self) -> MemoryEditor {
        MemoryEditor {
            session: self.session.clone(),
        }
    }
}
#[derive(Clone)]
pub struct MemoryReview {
    used: Arc<AtomicBool>,
    session: String,
    record: MemoryRecord,
}
impl MemoryReview {
    fn new(session: String, record: MemoryRecord) -> Self {
        Self {
            used: Arc::new(AtomicBool::new(false)),
            session,
            record,
        }
    }
    pub fn record(&self) -> &MemoryRecord {
        &self.record
    }
}
#[derive(Clone)]
pub struct MemoryEditor {
    session: String,
}
#[derive(Clone)]
pub struct MemoryCreateReview {
    used: Arc<AtomicBool>,
    session: String,
    namespace: String,
    fact: String,
    tags: Vec<String>,
}
impl MemoryCreateReview {
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn fact(&self) -> &str {
        &self.fact
    }
    pub fn tags(&self) -> &[String] {
        &self.tags
    }
}
impl MemoryEditor {
    pub fn create(
        &self,
        namespace: String,
        fact: String,
        tags: Vec<String>,
    ) -> Result<MemoryCreateReview> {
        validate_namespace(&namespace)?;
        validate_text(&fact, &tags)?;
        Ok(MemoryCreateReview {
            used: Arc::new(AtomicBool::new(false)),
            session: self.session.clone(),
            namespace,
            fact,
            tags,
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    memories: Vec<MemoryRecord>,
    next_before: Option<i64>,
    matching: String,
    deleted: bool,
    #[serde(default)]
    namespace: Option<String>,
}
impl<T: Transport> Bridge<T> {
    fn memory_session(&self) -> Result<String> {
        if !self.view.available {
            return Err("The local workspace is unavailable.".into());
        }
        self.session
            .clone()
            .ok_or_else(|| "The local workspace is unavailable.".into())
    }
    fn memory_check(&mut self, session: &str) -> Result<()> {
        if !self.view.available || self.session.as_deref() != Some(session) {
            return Err("This memory view belongs to an earlier workspace. Refresh it.".into());
        }
        self.check_session()
    }
    pub fn memory_editor(&self) -> Result<MemoryEditor> {
        Ok(MemoryEditor {
            session: self.memory_session()?,
        })
    }
    pub fn memory_first(&mut self, filter: MemoryFilter) -> Result<MemoryPage> {
        filter.validate()?;
        let session = self.memory_session()?;
        self.memory_fetch(session, filter, None, 0, BTreeSet::new())
    }
    pub fn memory_next(&mut self, cursor: &MemoryCursor) -> Result<MemoryPage> {
        if cursor.used.swap(true, Ordering::SeqCst) {
            return Err("This memory cursor was already used. Refresh the view.".into());
        }
        self.memory_fetch(
            cursor.session.clone(),
            cursor.filter.clone(),
            Some(cursor.before),
            cursor.pages,
            cursor.seen.clone(),
        )
    }
    fn memory_fetch(
        &mut self,
        session: String,
        filter: MemoryFilter,
        before: Option<i64>,
        pages: usize,
        mut seen: BTreeSet<String>,
    ) -> Result<MemoryPage> {
        filter.validate()?;
        if pages >= MAX_PAGES || seen.len() >= MAX_RECORDS || before.is_some_and(|v| v <= 0) {
            return Err("Memory pagination reached its bound. Start a fresh view.".into());
        }
        self.memory_check(&session)?;
        let mut path = format!(
            "/api/memory?query={}&deleted={}",
            encode_component(&filter.query),
            filter.deleted
        );
        if let Some(namespace) = &filter.namespace {
            path.push_str(&format!("&namespace={}", encode_component(namespace)));
        }
        if let Some(before) = before {
            path.push_str(&format!("&before={before}"));
        }
        let result = self.transport.request("GET", &path, Some(&session), None);
        self.memory_check(&session)?;
        let page: Envelope =
            serde_json::from_value(result?).map_err(|_| "Invalid bounded memory page.")?;
        if page.matching != "literal_ascii_case_insensitive"
            || page.deleted != filter.deleted
            || page
                .namespace
                .as_ref()
                .is_some_and(|v| Some(v) != filter.namespace.as_ref())
            || page.memories.len() > PAGE_SIZE
            || seen.len() + page.memories.len() > MAX_RECORDS
        {
            return Err("The memory page differs from its filter or exceeds its bound.".into());
        }
        for record in &page.memories {
            validate_record(record)?;
            if record.deleted != filter.deleted
                || filter
                    .namespace
                    .as_ref()
                    .is_some_and(|v| v != &record.namespace)
                || !seen.insert(record.id.clone())
            {
                return Err(
                    "The memory page changed scope or repeated a record. Refresh it.".into(),
                );
            }
        }
        let next = match page.next_before {
            None => None,
            Some(next)
                if next > 0
                    && before.is_none_or(|previous| next < previous)
                    && page.memories.len() == PAGE_SIZE
                    && pages + 1 < MAX_PAGES
                    && seen.len() < MAX_RECORDS =>
            {
                Some(MemoryCursor {
                    used: Arc::new(AtomicBool::new(false)),
                    session: session.clone(),
                    filter: filter.clone(),
                    before: next,
                    pages: pages + 1,
                    seen,
                })
            }
            _ => {
                return Err(
                    "Invalid, repeated or nondecreasing memory cursor. Refresh the view.".into(),
                )
            }
        };
        let reviews = page
            .memories
            .iter()
            .map(|record| MemoryReview::new(session.clone(), record.clone()))
            .collect();
        Ok(MemoryPage {
            reviews,
            filter,
            memories: page.memories,
            next,
            session,
        })
    }
    /// The validated receipt retains its source session and is the next one-use review.
    /// An error after dispatch requires refresh; never blindly replay this call.
    fn memory_change(
        &mut self,
        review: &MemoryReview,
        mut expected: MemoryRecord,
        body: Value,
    ) -> Result<MemoryReview> {
        validate_record(&review.record)?;
        validate_record(&expected)?;
        self.memory_check(&review.session)?;
        let changed = expected != review.record;
        expected.version = if changed {
            review
                .record
                .version
                .checked_add(1)
                .ok_or("Memory version is exhausted.")?
        } else {
            review.record.version
        };
        if review.used.swap(true, Ordering::SeqCst) {
            return Err(
                "This memory change was already attempted. Refresh before another action.".into(),
            );
        }
        let result = self.transport.request(
            "PATCH",
            &format!("/api/memory/{}", encode_component(&review.record.id)),
            Some(&review.session),
            Some(body),
        );
        self.memory_check(&review.session)?;
        let value=result.map_err(|_|"Memory change is unconfirmed. Refresh this namespace and compare the record before another action.")?;
        let saved: MemoryRecord = serde_json::from_value(value)
            .map_err(|_| "Invalid memory receipt. Refresh before another action.")?;
        validate_record(&saved)?;
        if changed {
            expected.updated_at = saved.updated_at;
        }
        if saved != expected {
            return Err(
                "Memory receipt differs from the reviewed change. Refresh before another action."
                    .into(),
            );
        }
        Ok(MemoryReview::new(review.session.clone(), saved))
    }
    pub fn memory_set_pinned(
        &mut self,
        review: &MemoryReview,
        pinned: bool,
    ) -> Result<MemoryReview> {
        if review.record.deleted {
            return Err("Restore a memory before pinning it.".into());
        }
        let mut expected = review.record.clone();
        expected.pinned = pinned;
        self.memory_change(
            review,
            expected,
            json!({"action":"set_pinned","version":review.record.version,"pinned":pinned}),
        )
    }
    pub fn memory_trash(&mut self, review: &MemoryReview) -> Result<MemoryReview> {
        let mut expected = review.record.clone();
        expected.deleted = true;
        self.memory_change(
            review,
            expected,
            json!({"action":"trash","version":review.record.version}),
        )
    }
    pub fn memory_restore(&mut self, review: &MemoryReview) -> Result<MemoryReview> {
        let mut expected = review.record.clone();
        expected.deleted = false;
        self.memory_change(
            review,
            expected,
            json!({"action":"restore","version":review.record.version}),
        )
    }
    pub fn memory_edit(
        &mut self,
        review: &MemoryReview,
        fact: String,
        tags: Vec<String>,
    ) -> Result<MemoryReview> {
        validate_text(&fact, &tags)?;
        if review.record.deleted {
            return Err("Restore a memory before editing it.".into());
        }
        let mut expected = review.record.clone();
        expected.fact = fact.clone();
        expected.tags = tags.clone();
        self.memory_change(
            review,
            expected,
            json!({"action":"edit","version":review.record.version,"fact":fact,"tags":tags}),
        )
    }
    pub fn memory_create(&mut self, review: &MemoryCreateReview) -> Result<MemoryReview> {
        validate_namespace(&review.namespace)?;
        validate_text(&review.fact, &review.tags)?;
        self.memory_check(&review.session)?;
        if review.used.swap(true, Ordering::SeqCst) {
            return Err("This memory creation was already attempted. Refresh before creating another record.".into());
        }
        let result = self.transport.request(
            "POST",
            "/api/memory",
            Some(&review.session),
            Some(json!({"namespace":review.namespace,"fact":review.fact,"tags":review.tags})),
        );
        self.memory_check(&review.session)?;
        let saved:MemoryRecord=serde_json::from_value(result.map_err(|_|"Memory creation is unconfirmed. Refresh this namespace before creating another record; do not retry automatically.")?).map_err(|_|"Invalid memory creation receipt. Refresh before continuing.")?;
        validate_record(&saved)?;
        if saved.namespace != review.namespace
            || saved.fact != review.fact
            || saved.tags != review.tags
            || saved.version != 1
            || saved.pinned
            || saved.deleted
        {
            return Err("Memory creation receipt differs from the requested record. Refresh before continuing.".into());
        }
        Ok(MemoryReview::new(review.session.clone(), saved))
    }
}
