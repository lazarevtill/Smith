//! A sidebar selection captures exact chat versions and the history revision.
//! Filtering or refresh never silently upgrades selected rows before mutation.
use crate::bridge::{Actions, Bridge, Chat, Result, Transport};
use std::collections::BTreeMap;
#[derive(Clone, Default)]
pub struct Selection {
    pub restore: bool,
    rows: BTreeMap<String, Chat>,
    history: Option<Actions>,
}
impl Selection {
    pub fn count(&self) -> usize {
        self.rows.len()
    }
    pub fn contains(&self, id: &str) -> bool {
        self.rows.contains_key(id)
    }
    pub fn clear(&mut self) {
        self.rows.clear();
        self.history = None;
    }
    pub fn toggle(&mut self, chat: &Chat, history: &Actions) -> Result<()> {
        if self.rows.remove(&chat.id).is_some() {
            if self.rows.is_empty() {
                self.history = None;
            }
            return Ok(());
        }
        if self.rows.len() >= 100 {
            return Err("Select up to 100 conversations per batch.".into());
        }
        if self
            .history
            .as_ref()
            .is_some_and(|h| h.revision != history.revision)
        {
            return Err("Action history changed. Clear the selection and select the current conversations again.".into());
        }
        self.history = Some(history.clone());
        self.rows.insert(chat.id.clone(), chat.clone());
        Ok(())
    }
    pub fn select_visible(&mut self, rows: &[Chat], history: &Actions) -> Result<()> {
        if rows.len() > 100 {
            return Err(
                "Filter the list to 100 conversations or fewer before selecting all visible rows."
                    .into(),
            );
        }
        self.clear();
        for chat in rows {
            self.toggle(chat, history)?;
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct BatchContext {
    session: String,
}
impl BatchContext {
    pub fn review(&self, selection: &Selection) -> Result<BatchReview> {
        Ok(BatchReview {
            session: self.session.clone(),
            chats: selection.rows.values().cloned().collect(),
            history: selection
                .history
                .clone()
                .ok_or("Select conversations first.")?,
            restore: selection.restore,
        })
    }
}
#[derive(Clone)]
pub struct BatchReview {
    session: String,
    chats: Vec<Chat>,
    history: Actions,
    restore: bool,
}
impl<T: Transport> Bridge<T> {
    pub fn batch_context(&self) -> Option<BatchContext> {
        self.session.clone().map(|session| BatchContext { session })
    }
    pub fn batch_review(&self, selection: &Selection) -> Result<BatchReview> {
        let history = selection
            .history
            .clone()
            .ok_or("Select conversations first.")?;
        if history.revision != self.view.actions.revision {
            return Err("Action history changed after selection. Clear and select again.".into());
        }
        let current = if selection.restore {
            &self.view.trash
        } else {
            &self.view.chats
        };
        if selection.rows.values().any(|row| {
            !current
                .iter()
                .any(|c| c.id == row.id && c.version == row.version)
        }) {
            return Err("A selected conversation changed. Clear and select again.".into());
        }
        Ok(BatchReview {
            session: self.session.clone().ok_or("Workspace unavailable.")?,
            chats: selection.rows.values().cloned().collect(),
            history,
            restore: selection.restore,
        })
    }
    pub fn batch_apply(&mut self, review: &BatchReview) -> Result<()> {
        if self.session.as_deref() != Some(&review.session) {
            return Err("The selection belongs to an earlier workspace.".into());
        }
        self.check_session()?;
        self.trash(&review.chats, &review.history, review.restore)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn c(id: &str, v: i64) -> Chat {
        Chat {
            id: id.into(),
            version: v,
            ..Default::default()
        }
    }
    #[test]
    fn selected_versions_never_refresh_and_scope_is_explicit() {
        let mut s = Selection::default();
        let h = Actions {
            revision: 3,
            ..Default::default()
        };
        s.toggle(&c("one", 1), &h).unwrap();
        assert!(s
            .toggle(
                &c("two", 2),
                &Actions {
                    revision: 4,
                    ..Default::default()
                }
            )
            .is_err());
        assert_eq!(s.rows["one"].version, 1);
        s.toggle(&c("one", 99), &h).unwrap();
        assert_eq!(s.count(), 0);
        s.select_visible(&[c("one", 99)], &h).unwrap();
        assert_eq!(s.rows["one"].version, 99);
    }
    #[test]
    fn visible_batch_is_bounded_without_destroying_existing_selection() {
        let mut s = Selection::default();
        s.toggle(&c("kept", 0), &Actions::default()).unwrap();
        assert!(s
            .select_visible(
                &(0..101).map(|i| c(&i.to_string(), 0)).collect::<Vec<_>>(),
                &Actions::default()
            )
            .is_err());
        assert!(s.contains("kept"));
    }
}
