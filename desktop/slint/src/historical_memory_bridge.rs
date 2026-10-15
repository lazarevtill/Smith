//! Explicit dormant original Episodes/Lessons. Reads never become active facts;
//! Trash reviews are bound to the archive, source bytes and workspace session.
use crate::bridge::{encode_component, Bridge, Result, Transport};
use crate::historical_memory_contract as contract;
pub use contract::*;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[derive(Clone)]
pub struct HistoricalReview {
    session: String,
    record: HistoricalRecord,
    used: Arc<AtomicBool>,
}
impl HistoricalReview {
    fn new(session: String, record: HistoricalRecord) -> Self {
        Self {
            session,
            record,
            used: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn record(&self) -> &HistoricalRecord {
        &self.record
    }
}
#[derive(Clone)]
pub struct HistoricalCursor {
    session: String,
    import_id: String,
    filter: HistoricalFilter,
    opaque: String,
    pages: usize,
    seen: BTreeSet<i64>,
    cursors: BTreeSet<String>,
    used: Arc<AtomicBool>,
}
#[derive(Clone)]
pub struct HistoricalPage {
    pub import_id: String,
    pub filter: HistoricalFilter,
    pub records: Vec<HistoricalRecord>,
    pub retained_issues: Vec<RetainedIssue>,
    pub scanned: usize,
    pub total_archived_rows: u64,
    pub next: Option<HistoricalCursor>,
    reviews: Vec<HistoricalReview>,
}
impl HistoricalPage {
    pub fn review(&self, record: &HistoricalRecord) -> Result<HistoricalReview> {
        self.reviews
            .iter()
            .find(|r| r.record == *record)
            .cloned()
            .ok_or_else(|| {
                "This original record was not fetched in this historical page. Refresh it.".into()
            })
    }
}
#[derive(Clone)]
pub struct HistoricalDetails {
    pub original: ExactRow,
    review: HistoricalReview,
}
impl HistoricalDetails {
    pub fn record(&self) -> &HistoricalRecord {
        self.review.record()
    }
    pub fn review(&self) -> HistoricalReview {
        self.review.clone()
    }
}
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    if serde_json::to_vec(&value)
        .map_err(|_| "Invalid historical response.")?
        .len()
        > MAX_PAGE_BYTES
    {
        return Err(
            "Historical response exceeds the display bound; exact archive bytes remain retained."
                .into(),
        );
    }
    serde_json::from_value(value)
        .map_err(|_| "Invalid typed historical response. Refresh the archive view.".into())
}
fn check_record(record: &HistoricalRecord, kind: HistoricalKind, import: &str) -> Result<()> {
    if !record.valid() || record.data.kind() != kind || record.source.import_id != import {
        return Err("Historical record identity or dormant-state receipt is invalid.".into());
    }
    Ok(())
}
fn same_original(a: &HistoricalRecord, b: &HistoricalRecord) -> bool {
    let mut a = a.clone();
    a.overlay = b.overlay.clone();
    a == *b
}
impl<T: Transport> Bridge<T> {
    fn historical_session(&self) -> Result<String> {
        if !self.view.available {
            return Err("The owned workspace is unavailable.".into());
        }
        self.session
            .clone()
            .ok_or_else(|| "The owned workspace is unavailable.".into())
    }
    fn historical_check(&mut self, session: &str) -> Result<()> {
        if !self.view.available || self.session.as_deref() != Some(session) {
            return Err("This historical view belongs to an earlier workspace. Refresh it.".into());
        }
        self.check_session()
    }
    pub fn historical_first(&mut self, filter: HistoricalFilter) -> Result<HistoricalPage> {
        if !filter.valid() {
            return Err("Invalid historical scope or literal query.".into());
        }
        let session = self.historical_session()?;
        self.historical_fetch(
            session,
            filter,
            None,
            None,
            0,
            BTreeSet::new(),
            BTreeSet::new(),
        )
    }
    pub fn historical_next(&mut self, cursor: &HistoricalCursor) -> Result<HistoricalPage> {
        if cursor.used.swap(true, Ordering::SeqCst) {
            return Err("Historical cursor already used. Refresh before continuing.".into());
        }
        self.historical_fetch(
            cursor.session.clone(),
            cursor.filter.clone(),
            Some(cursor.import_id.clone()),
            Some(cursor.opaque.clone()),
            cursor.pages,
            cursor.seen.clone(),
            cursor.cursors.clone(),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn historical_fetch(
        &mut self,
        session: String,
        filter: HistoricalFilter,
        expected_import: Option<String>,
        cursor: Option<String>,
        pages: usize,
        mut seen: BTreeSet<i64>,
        mut cursors: BTreeSet<String>,
    ) -> Result<HistoricalPage> {
        if pages >= MAX_NATIVE_PAGES {
            return Err("Historical browsing reached its page bound. Narrow the scope and refresh; no archived rows were deleted.".into());
        }
        self.historical_check(&session)?;
        let filter_json =
            serde_json::to_string(&filter).map_err(|_| "Invalid historical filter.")?;
        let mut path = format!(
            "/api/legacy-import/memory?filter={}",
            encode_component(&filter_json)
        );
        if let Some(cursor) = cursor {
            path.push_str(&format!("&cursor={}", encode_component(&cursor)));
        }
        let result = self.transport.request("GET", &path, Some(&session), None);
        self.historical_check(&session)?;
        let reply: HistoricalPageReply = decode(result?)?;
        if !identifier(&reply.import_id)
            || expected_import
                .as_ref()
                .is_some_and(|v| v != &reply.import_id)
            || reply.filter != filter
            || reply.runtime_activation != "dormant"
            || reply.records.len() + reply.retained_issues.len() > PAGE_SIZE
            || reply.scanned > MAX_SCAN_ROWS
            || reply.records.len() + reply.retained_issues.len() > reply.scanned
        {
            return Err("Historical page changed its archive, scope or bounds.".into());
        }
        for record in &reply.records {
            check_record(record, filter.kind, &reply.import_id)?;
            if record.overlay.trashed != filter.trash || !seen.insert(record.source.row_number) {
                return Err("Historical page repeated a record or changed Trash scope.".into());
            }
            let matches_scope = match &filter.scope {
                AgentScope::All => true,
                AgentScope::Global => matches!(record.data.agent_id(), Some(ExactValue::Null)),
                AgentScope::Agent { id } => {
                    record.data.agent_id().and_then(ExactValue::text) == Some(id)
                }
            };
            if !matches_scope
                || filter.session_id.as_ref().is_some_and(|id| {
                    record.data.session_id().and_then(ExactValue::text) != Some(id)
                })
                || (!filter.query.is_empty()
                    && !record.data.summary().is_some_and(|s| {
                        s.to_ascii_lowercase()
                            .contains(&filter.query.to_ascii_lowercase())
                    }))
            {
                return Err("Historical record differs from its exact selected filter.".into());
            }
        }
        for issue in &reply.retained_issues {
            if issue.row_number < 0
                || issue.code != "oversized_original_retained"
                || issue.payload_bytes <= MAX_ROW_BYTES as u64
                || !seen.insert(issue.row_number)
            {
                return Err("Invalid retained historical record notice.".into());
            }
        }
        let next = if let Some(opaque) = reply.next_cursor {
            if opaque.is_empty()
                || opaque.len() > 2048
                || reply.scanned == 0
                || pages + 1 >= MAX_NATIVE_PAGES
                || !cursors.insert(opaque.clone())
            {
                return Err("Historical scan cursor did not progress or reached its bound. Narrow the scope and refresh.".into());
            }
            Some(HistoricalCursor {
                session: session.clone(),
                import_id: reply.import_id.clone(),
                filter: filter.clone(),
                opaque,
                pages: pages + 1,
                seen,
                cursors,
                used: Arc::new(AtomicBool::new(false)),
            })
        } else {
            None
        };
        let reviews = reply
            .records
            .iter()
            .map(|r| HistoricalReview::new(session.clone(), r.clone()))
            .collect();
        Ok(HistoricalPage {
            import_id: reply.import_id,
            filter,
            records: reply.records,
            retained_issues: reply.retained_issues,
            scanned: reply.scanned,
            total_archived_rows: reply.total_archived_rows,
            next,
            reviews,
        })
    }
    pub fn historical_detail(&mut self, review: &HistoricalReview) -> Result<HistoricalDetails> {
        self.historical_check(&review.session)?;
        let path = format!(
            "/api/legacy-import/memory/{}/{}",
            review.record.data.kind().path(),
            review.record.source.row_number
        );
        let result = self
            .transport
            .request("GET", &path, Some(&review.session), None);
        self.historical_check(&review.session)?;
        let detail: HistoricalDetail = decode(result?)?;
        check_record(
            &detail.record,
            review.record.data.kind(),
            &review.record.source.import_id,
        )?;
        if !same_original(&detail.record, &review.record)
            || detail.original.columns.len() != detail.original.values.len()
            || detail.original.columns.len() > 512
        {
            return Err("Original historical detail changed. Refresh before continuing.".into());
        }
        Ok(HistoricalDetails {
            original: detail.original,
            review: HistoricalReview::new(review.session.clone(), detail.record),
        })
    }
    pub fn historical_trash(&mut self, review: &HistoricalReview) -> Result<HistoricalReview> {
        self.historical_change(review, HistoricalAction::Trash)
    }
    pub fn historical_restore(&mut self, review: &HistoricalReview) -> Result<HistoricalReview> {
        self.historical_change(review, HistoricalAction::Restore)
    }
    fn historical_change(
        &mut self,
        review: &HistoricalReview,
        action: HistoricalAction,
    ) -> Result<HistoricalReview> {
        self.historical_check(&review.session)?;
        if review.used.swap(true, Ordering::SeqCst) {
            return Err(
                "This historical change was already attempted. Refresh before another action."
                    .into(),
            );
        }
        let desired = action == HistoricalAction::Trash;
        let revision = if review.record.overlay.trashed == desired {
            review.record.overlay.revision
        } else {
            review
                .record
                .overlay
                .revision
                .checked_add(1)
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or("Historical Trash revision is exhausted.")?
        };
        let request_id = crate::task_bridge::new_task_request_id()?;
        let change = HistoricalChange {
            source: review.record.source.clone(),
            expected_revision: review.record.overlay.revision,
            action,
            request_id: request_id.clone(),
        };
        let path = format!(
            "/api/legacy-import/memory/{}/{}",
            review.record.data.kind().path(),
            review.record.source.row_number
        );
        let result = self.transport.request(
            "PATCH",
            &path,
            Some(&review.session),
            Some(serde_json::to_value(change).map_err(|_| "Invalid historical change.")?),
        );
        self.historical_check(&review.session)?;
        let receipt:HistoricalReceipt=decode(result.map_err(|_|"Historical change is unconfirmed. Refresh its Trash state and compare before another action; no automatic retry was made.")?)?;
        if receipt.request_id != request_id
            || receipt.source != review.record.source
            || receipt.action != action
            || receipt.overlay
                != (HistoricalOverlay {
                    revision,
                    trashed: desired,
                })
        {
            return Err("Historical change receipt differs from the reviewed action. Refresh before continuing.".into());
        }
        // A replayed receipt can describe an earlier success. Adopt only a fresh
        // matching read, never overwrite a later concurrent Trash/restore state.
        let refreshed = self.historical_detail(review)?;
        if refreshed.record().overlay != receipt.overlay {
            return Err(
                "Historical Trash changed again after this receipt. Refresh its current state."
                    .into(),
            );
        }
        Ok(refreshed.review())
    }
}
