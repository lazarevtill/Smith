use crate::*;
use sha2::{Digest, Sha256};

/// Functional state: a successful edit returns a candidate for one encrypted
/// host CAS transaction; it does not write, publish or change this snapshot.
#[derive(Clone)]
pub struct Catalog {
    record: CatalogRecord,
    stored_sha256: Option<String>,
    stored_revision: u64,
}
pub struct SelectionReview {
    summary: ReviewSummary,
}
pub struct PreparedSelection {
    summary: ReviewSummary,
    entry: EntryRecord,
}
impl SelectionReview {
    pub fn summary(&self) -> &ReviewSummary {
        &self.summary
    }
}
impl PreparedSelection {
    pub fn summary(&self) -> &ReviewSummary {
        &self.summary
    }
    pub fn entry(&self) -> &EntryRecord {
        &self.entry
    }
}
impl Default for Catalog {
    fn default() -> Self {
        Self::empty()
    }
}
impl Catalog {
    pub fn empty() -> Self {
        Self {
            record: CatalogRecord {
                format_version: 1,
                revision: 0,
                entries: vec![],
                selected_for_review: None,
            },
            stored_sha256: None,
            stored_revision: 0,
        }
    }
    /// Decode-only, including for unavailable paths or foreign-platform roots.
    /// Oversized/unknown/malformed data is refused, never reset to empty.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        let record: CatalogRecord =
            serde_json::from_slice(bytes).map_err(|_| Error::InvalidRecord)?;
        validate::record(&record)?;
        let stored_revision = record.revision;
        Ok(Self {
            record,
            stored_sha256: Some(hex(&Sha256::digest(bytes))),
            stored_revision,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(&self.record).map_err(|_| Error::InvalidRecord)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(bytes)
    }
    pub fn revision(&self) -> u64 {
        self.record.revision
    }
    /// Hash of the exact bytes originally read. None means the settings row was
    /// absent. Compare this and storage_base_revision in the host transaction.
    /// A candidate produced by an edit retains its *base* fingerprint until the
    /// host commits and decodes the written bytes anew.
    pub fn storage_base_sha256(&self) -> Option<&str> {
        self.stored_sha256.as_deref()
    }
    pub fn storage_base_revision(&self) -> u64 {
        self.stored_revision
    }
    pub fn record(&self) -> &CatalogRecord {
        &self.record
    }
    pub fn details(&self, id: &str) -> Result<&EntryRecord> {
        self.record
            .entries
            .iter()
            .find(|e| e.id == id)
            .ok_or(Error::MissingEntry)
    }
    pub fn list(&self) -> Vec<EntrySummary> {
        self.summaries(false)
    }
    pub fn list_trash(&self) -> Vec<EntrySummary> {
        self.summaries(true)
    }
    fn summaries(&self, trashed: bool) -> Vec<EntrySummary> {
        self.record
            .entries
            .iter()
            .filter(|e| e.trashed == trashed)
            .map(|e| EntrySummary {
                id: e.id.clone(),
                revision: e.revision,
                label: e.label.clone(),
                trashed: e.trashed,
                format: e.manifest.description.format,
                declared_capabilities: e.manifest.description.declared_capabilities.clone(),
                origin: Origin::ExternalUserSelected,
                completeness: Completeness::SelectedFilesOnly,
                selected_for_review: self.record.selected_for_review.as_deref() == Some(&e.id),
            })
            .collect()
    }
    fn next(&self, expected: u64) -> Result<Self> {
        if expected != self.revision() {
            return Err(Error::Conflict);
        }
        if expected >= MAX_REVISION {
            return Err(Error::LimitExceeded);
        }
        let mut next = self.clone();
        next.record.revision += 1;
        Ok(next)
    }
    fn finish(self) -> Result<Self> {
        validate::record(&self.record)?;
        self.encode()?;
        Ok(self)
    }
    pub fn insert(&self, expected: u64, label: String, assets: CapturedAssets) -> Result<Self> {
        let mut next = self.next(expected)?;
        if next.record.entries.len() >= MAX_ENTRIES {
            return Err(Error::LimitExceeded);
        }
        let revision = next.revision();
        next.record.entries.push(EntryRecord {
            id: format!("local-{revision:016x}"),
            revision,
            label,
            trashed: false,
            manifest: assets.0,
        });
        next.finish()
    }
    pub fn replace(
        &self,
        expected: u64,
        id: &str,
        entry_revision: u64,
        label: String,
        assets: CapturedAssets,
    ) -> Result<Self> {
        let mut next = self.next(expected)?;
        let revision = next.revision();
        let e = next
            .record
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(Error::MissingEntry)?;
        if e.revision != entry_revision {
            return Err(Error::Conflict);
        }
        if e.trashed {
            return Err(Error::EntryTrashed);
        }
        *e = EntryRecord {
            id: id.into(),
            revision,
            label,
            trashed: false,
            manifest: assets.0,
        };
        next.finish()
    }
    /// Label-only edit. No file access, settings merge or active-model switch.
    pub fn rename(
        &self,
        expected: u64,
        id: &str,
        entry_revision: u64,
        label: String,
    ) -> Result<Self> {
        let mut next = self.next(expected)?;
        let revision = next.revision();
        let e = next
            .record
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(Error::MissingEntry)?;
        if e.revision != entry_revision {
            return Err(Error::Conflict);
        }
        if e.trashed {
            return Err(Error::EntryTrashed);
        }
        e.label = label;
        e.revision = revision;
        next.finish()
    }
    fn set_trashed(
        &self,
        expected: u64,
        id: &str,
        entry_revision: u64,
        trashed: bool,
    ) -> Result<Self> {
        let mut next = self.next(expected)?;
        let revision = next.revision();
        let e = next
            .record
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(Error::MissingEntry)?;
        if e.revision != entry_revision || e.trashed == trashed {
            return Err(Error::Conflict);
        }
        e.trashed = trashed;
        e.revision = revision;
        if trashed && next.record.selected_for_review.as_deref() == Some(id) {
            next.record.selected_for_review = None;
        }
        next.finish()
    }
    /// Recoverable record-only removal. Selected external bytes are untouched.
    pub fn trash(&self, expected: u64, id: &str, entry_revision: u64) -> Result<Self> {
        self.set_trashed(expected, id, entry_revision, true)
    }
    /// Restore the same entry identity; never select or load it implicitly.
    pub fn restore(&self, expected: u64, id: &str, entry_revision: u64) -> Result<Self> {
        self.set_trashed(expected, id, entry_revision, false)
    }
    pub fn select_for_review(&self, expected: u64, id: &str, entry_revision: u64) -> Result<Self> {
        let mut next = self.next(expected)?;
        if next.details(id)?.revision != entry_revision {
            return Err(Error::Conflict);
        }
        if next.details(id)?.trashed {
            return Err(Error::EntryTrashed);
        }
        next.record.selected_for_review = Some(id.into());
        next.finish()
    }
    pub fn review(
        &self,
        id: &str,
        entry_revision: u64,
        context: PreparationContext,
    ) -> Result<SelectionReview> {
        validate::context(&context)?;
        let entry = self.details(id)?;
        if entry.trashed {
            return Err(Error::EntryTrashed);
        }
        if self.record.selected_for_review.as_deref() != Some(id) {
            return Err(Error::WrongSelection);
        }
        if entry.revision != entry_revision {
            return Err(Error::StaleReview);
        }
        let mut summary = ReviewSummary {
            id: String::new(),
            catalog_revision: self.revision(),
            entry_id: id.into(),
            entry_revision,
            manifest_sha256: entry.manifest.sha256.clone(),
            context,
        };
        summary.id = digest(&("smith-local-catalog/review/v1", &summary, digest(entry)?))?;
        Ok(SelectionReview { summary })
    }
    pub fn check_review(
        &self,
        review: &SelectionReview,
        current: &PreparationContext,
    ) -> Result<()> {
        let s = review.summary();
        if &s.context != current || s.catalog_revision != self.revision() {
            return Err(Error::StaleReview);
        }
        let fresh = self
            .review(&s.entry_id, s.entry_revision, current.clone())
            .map_err(|_| Error::StaleReview)?;
        if fresh.summary != *s {
            return Err(Error::StaleReview);
        }
        Ok(())
    }
    /// Synchronous, bounded full-byte reads. Run on a tracked blocking worker;
    /// continue draining it after cancellation. Returned data confers no load
    /// permission. Host must re-read CAS/context after completion and still call
    /// the selected runtime adapter's independent prepare/approve/load path.
    pub fn prepare(
        &self,
        review: &SelectionReview,
        current: &PreparationContext,
        control: &ReadControl,
    ) -> Result<PreparedSelection> {
        self.check_review(review, current)?;
        let entry = self.details(&review.summary.entry_id)?;
        files::revalidate(&entry.manifest, control)?;
        Ok(PreparedSelection {
            summary: review.summary.clone(),
            entry: entry.clone(),
        })
    }
}
