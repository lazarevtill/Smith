//! Serialized catalog state; independent of the Slint renderer.
use crate::{
    bridge::{Bridge, Result, Transport},
    catalog_bridge::*,
};
#[derive(Clone, Default)]
pub struct CatalogPanel {
    pub page: Option<CatalogPage>,
    pub details: Option<EntryDetails>,
    pub pending: Option<CatalogMutation>,
    pub operation: Option<CatalogOperation>,
    pub prepare_recovery: Option<PrepareRecovery>,
    pub prepared: Option<PreparedReview>,
    pub notice: String,
    pub setup: Option<SetupAdapter>,
    pub setup_attempted: bool,
}
impl CatalogPanel {
    pub fn blocked(&self) -> bool {
        self.pending.is_some()
            || self.prepare_recovery.is_some()
            || self
                .operation
                .as_ref()
                .is_some_and(|o| !o.snapshot().terminal())
    }
    pub fn refresh<T: Transport>(&mut self, b: &mut Bridge<T>, trash: bool) -> Result<()> {
        let page = b.catalog_list(trash)?;
        // Browsing alone never resolves an unknown mutation or adopts a worker.
        self.prepared = page.prepared_review(&b.view.config).ok();
        self.page = Some(page);
        self.details = None;
        Ok(())
    }
    fn settle<T: Transport>(&mut self, b: &mut Bridge<T>, outcome: MutationOutcome) -> Result<()> {
        match outcome {
            MutationOutcome::Pending(op) => {
                self.notice = "Reading the selected local files…".into();
                self.operation = Some(op);
                Ok(())
            }
            MutationOutcome::Committed(r) => {
                self.pending = None;
                self.operation = None;
                self.notice = match r.operation.as_str() {
                    "select_v1" => "Selected for review. The running model is unchanged.",
                    "trash_v1" => "Saved entry moved to Trash. Model files are unchanged.",
                    "restore_v1" => "Saved entry restored. It has not been selected or loaded.",
                    _ => "Saved model entry updated.",
                }
                .into();
                let trash = self.page.as_ref().is_some_and(|p| p.trashed);
                self.refresh(b, trash)
            }
        }
    }
    pub fn submit<T: Transport>(&mut self, b: &mut Bridge<T>, m: CatalogMutation) -> Result<()> {
        if self.blocked() {
            return Err("Inspect the pending saved-model action first.".into());
        }
        self.pending = Some(m.clone());
        self.notice = format!("Saving request {}…", m.request_id());
        let result = b.catalog_submit_checked(&m);
        match result {
            Ok(outcome) => self.settle(b, outcome),
            Err(CatalogMutationError::NotSubmitted(e)) => {
                self.pending = None;
                self.notice =
                    "Save was refused before admission. Refresh the catalog before editing again."
                        .into();
                Err(e)
            }
            Err(CatalogMutationError::OutcomeUnknown(e)) => {
                self.notice =
                    "Save outcome is unconfirmed. Inspect its receipt before another edit.".into();
                Err(e)
            }
        }
    }
    pub fn inspect<T: Transport>(&mut self, b: &mut Bridge<T>) -> Result<()> {
        if let Some(m) = self.pending.clone() {
            let result = b.catalog_recover(&m)?;
            self.settle(b, result)?;
        } else if let Some(r) = self.prepare_recovery.clone() {
            self.operation = Some(b.catalog_recover_prepare(&r)?);
            self.prepare_recovery = None;
        }
        self.poll(b)
    }
    pub fn retry<T: Transport>(&mut self, b: &mut Bridge<T>) -> Result<()> {
        let m = self
            .pending
            .clone()
            .ok_or("There is no retained mutation to retry.")?;
        if self
            .operation
            .as_ref()
            .is_some_and(|o| !o.snapshot().terminal())
        {
            return Err("Wait for the owned file reader to drain.".into());
        }
        let result = b.catalog_submit(&m)?;
        self.settle(b, result)
    }
    pub fn prepare<T: Transport>(
        &mut self,
        b: &mut Bridge<T>,
        review: PrepareReview,
    ) -> Result<()> {
        if self.blocked() {
            return Err("Inspect the pending saved-model action first.".into());
        }
        self.prepared = None;
        self.setup_attempted = false;
        match b.catalog_prepare(&review) {
            Ok(op) => {
                self.operation = Some(op);
                self.notice = "Checking the selected file bytes…".into();
                Ok(())
            }
            Err(PrepareFailure::NotSubmitted(e)) => Err(e),
            Err(PrepareFailure::OutcomeUnknown { message, recovery }) => {
                self.prepare_recovery = Some(*recovery);
                self.notice="Preparation admission is unconfirmed. Inspect status; it will not be replayed.".into();
                Err(message)
            }
        }
    }
    pub fn cancel<T: Transport>(&mut self, b: &mut Bridge<T>) -> Result<()> {
        let op = self
            .operation
            .as_ref()
            .ok_or("No selected file reader is running.")?;
        self.operation = Some(b.catalog_cancel(op)?);
        self.notice = "Cancellation requested; waiting for the reader to finish.".into();
        Ok(())
    }
    pub fn poll<T: Transport>(&mut self, b: &mut Bridge<T>) -> Result<()> {
        let Some(old) = self.operation.clone() else {
            return Ok(());
        };
        let op = if old.snapshot().terminal() {
            old
        } else {
            b.catalog_poll(&old)?
        };
        self.operation = Some(op.clone());
        if !op.snapshot().terminal() {
            return Ok(());
        }
        match op.snapshot().status {
            OperationPhase::Succeeded => {
                if let Some(m) = self.pending.clone() {
                    let receipt = b
                        .catalog_mutation_completion(&m, &op)?
                        .ok_or("Catalog worker completed without its receipt.")?;
                    self.settle(b, MutationOutcome::Committed(receipt))?;
                } else {
                    self.operation = None;
                    self.notice =
                        "Selected files checked. Continue in Inference to review setup.".into();
                    self.refresh(b, false)?;
                }
            }
            OperationPhase::Cancelled | OperationPhase::Failed => {
                self.pending = None;
                self.operation = None;
                self.prepare_recovery = None;
                self.notice = op.snapshot().error.clone().unwrap_or_else(|| {
                    "File check cancelled; no new selection was published.".into()
                });
            }
            OperationPhase::Running => unreachable!(),
        };
        Ok(())
    }
}
