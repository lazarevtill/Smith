//! Local navigation contains only UI destinations and opaque conversation IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub screen: i32,
    pub chat: Option<String>,
}
#[derive(Default)]
pub struct Navigation {
    entries: Vec<Place>,
    cursor: usize,
}
impl Navigation {
    pub fn record(&mut self, place: Place) {
        if self.entries.get(self.cursor) == Some(&place) {
            return;
        }
        if !self.entries.is_empty() {
            self.entries.truncate(self.cursor + 1);
        }
        self.entries.push(place);
        if self.entries.len() > 100 {
            self.entries.remove(0);
        }
        self.cursor = self.entries.len() - 1;
    }
    pub fn target(&self, direction: i32) -> Option<(usize, Place)> {
        let index = if direction < 0 {
            self.cursor.checked_sub(1)?
        } else {
            self.cursor.checked_add(1)?
        };
        self.entries.get(index).cloned().map(|p| (index, p))
    }
    pub fn commit(&mut self, index: usize) {
        if index < self.entries.len() {
            self.cursor = index;
        }
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cursor = 0;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn back_forward_and_new_navigation_drop_only_forward_destinations() {
        let mut h = Navigation::default();
        h.record(Place {
            screen: 0,
            chat: None,
        });
        h.record(Place {
            screen: 0,
            chat: Some("copied/id".into()),
        });
        h.record(Place {
            screen: 2,
            chat: Some("copied/id".into()),
        });
        let (i, p) = h.target(-1).unwrap();
        assert_eq!(p.screen, 0);
        h.commit(i);
        assert_eq!(h.target(1).unwrap().1.screen, 2);
        h.record(Place {
            screen: 6,
            chat: Some("copied/id".into()),
        });
        assert!(h.target(1).is_none());
        h.clear();
        assert!(h.target(-1).is_none());
    }
}
