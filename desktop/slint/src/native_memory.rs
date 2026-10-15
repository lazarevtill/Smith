use super::*;
use crate::memory_bridge::*;
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub(super) struct Panel {
    pub filter: Option<MemoryFilter>,
    pub rows: Vec<MemoryRecord>,
    pub reviews: BTreeMap<String, MemoryReview>,
    pub editor: Option<MemoryEditor>,
    pub next: Option<MemoryCursor>,
    pub undo: Option<MemoryReview>,
    pub message: String,
}
pub(super) enum Action {
    First(MemoryFilter),
    Next(MemoryCursor),
    Create(MemoryCreateReview),
    Edit(MemoryReview, String, Vec<String>),
    Pin(MemoryReview, bool),
    Trash(MemoryReview),
    Restore(MemoryReview),
}
impl Action {
    pub fn changes_editor(&self) -> bool {
        matches!(self, Self::Create(_) | Self::Edit(..))
    }
    pub fn execute<T: Transport>(
        self,
        b: &mut Bridge<T>,
        p: &mut Panel,
    ) -> crate::bridge::Result<()> {
        if !matches!(&self, Self::First(_) | Self::Next(_)) {
            // A lost mutation reply may still have changed filter membership.
            p.next = None;
        }
        let result_label = match &self {
            Self::Create(_) | Self::Edit(..) => "Memory saved.",
            Self::Pin(_, true) => "Memory pinned.",
            Self::Pin(_, false) => "Memory unpinned.",
            Self::Trash(_) => "Memory moved to Trash. Restore is available.",
            Self::Restore(_) => "Memory restored.",
            _ => "",
        };
        let receipt = match self {
            Self::First(filter) => {
                *p = Panel::default();
                let page = b.memory_first(filter)?;
                p.adopt(page)?;
                return Ok(());
            }
            Self::Next(cursor) => {
                p.next = None;
                let page = b.memory_next(&cursor)?;
                if p.filter.as_ref() != Some(&page.filter) {
                    return Err("Memory scope changed. Refresh its results.".into());
                }
                p.adopt(page)?;
                return Ok(());
            }
            Self::Create(review) => match b.memory_create(&review) {
                Ok(record) => record,
                Err(error) => {
                    p.editor = None;
                    return Err(error);
                }
            },
            Self::Edit(review, fact, tags) => b.memory_edit(&review, fact, tags)?,
            Self::Pin(review, pinned) => b.memory_set_pinned(&review, pinned)?,
            Self::Trash(review) => {
                let record = b.memory_trash(&review)?;
                p.undo = Some(record.clone());
                p.message = "Memory moved to Trash. Restore is available.".into();
                record
            }
            Self::Restore(review) => {
                let record = b.memory_restore(&review)?;
                p.undo = None;
                p.message = "Memory restored.".into();
                record
            }
        };
        let record = receipt.record().clone();
        p.rows.retain(|v| v.id != record.id);
        p.reviews.remove(&record.id);
        if p.filter
            .as_ref()
            .is_some_and(|f| matches_filter(&record, f))
        {
            p.reviews.insert(record.id.clone(), receipt);
            p.rows.insert(0, record);
        }
        p.message = format!("{result_label} {} memories shown.", p.rows.len());
        // A successful mutation changes the insertion/filter view. Continuing a
        // prior page would mix snapshots; a fresh explicit Search is required.
        p.next = None;
        Ok(())
    }
}
fn matches_filter(r: &MemoryRecord, f: &MemoryFilter) -> bool {
    r.deleted == f.deleted
        && f.namespace.as_ref().is_none_or(|n| n == &r.namespace)
        && (f.query.is_empty()
            || r.fact
                .to_ascii_lowercase()
                .contains(&f.query.to_ascii_lowercase()))
}
impl Panel {
    fn adopt(&mut self, page: MemoryPage) -> crate::bridge::Result<()> {
        let reviews = page
            .memories
            .iter()
            .map(|row| Ok((row.id.clone(), page.review(row)?)))
            .collect::<crate::bridge::Result<Vec<_>>>()?;
        self.editor = Some(page.editor());
        self.filter = Some(page.filter);
        self.next = page.next;
        self.reviews.extend(reviews);
        self.rows.extend(page.memories);
        self.message = format!("{} memories shown.", self.rows.len());
        Ok(())
    }
}
fn filter(w: &MainWindow) -> MemoryFilter {
    let g = w.global::<NativeMemory>();
    let namespace = g.get_namespace().trim().to_owned();
    MemoryFilter {
        namespace: (!namespace.is_empty()).then_some(namespace),
        query: w.get_memory_query().to_string(),
        deleted: g.get_deleted(),
    }
}
pub(super) fn invalidate(w: &MainWindow, s: &mut UiState) {
    s.memory_generation = s.memory_generation.wrapping_add(1);
    s.memory_panel = Panel::default();
    s.memory_selected = None;
    s.memory_pending = false;
    s.memory_filter = Some(filter(w));
    w.global::<NativeMemory>().set_editing(false);
    w.global::<NativeMemory>().set_fact("".into());
    w.global::<NativeMemory>().set_tags("".into());
    project(w, s);
}
pub(super) fn capture_filter(w: &MainWindow, s: &mut UiState) {
    if s.memory_filter.as_ref() != Some(&filter(w)) {
        invalidate(w, s);
    }
}
fn enqueue(s: &Rc<RefCell<UiState>>, tx: &mpsc::Sender<(u64, Command)>, action: Action) {
    let mut s = s.borrow_mut();
    if !s.view.available || s.memory_pending {
        return;
    }
    s.memory_pending = true;
    let _ = tx.send((
        s.epoch.current(),
        Command::Memory(s.memory_generation, action),
    ));
}
pub(super) fn wire(
    w: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    {
        let state = state.clone();
        let weak = w.as_weak();
        w.global::<NativeMemory>().on_scope_changed(move || {
            if let Some(w) = weak.upgrade() {
                invalidate(&w, &mut state.borrow_mut());
            }
        });
    }
    for browse in [false, true] {
        let weak = w.as_weak();
        let state = state.clone();
        let tx = tx.clone();
        let callback = move || {
            let Some(w) = weak.upgrade() else { return };
            if browse {
                w.set_memory_query("".into());
            }
            {
                let mut s = state.borrow_mut();
                invalidate(&w, &mut s);
            }
            enqueue(&state, &tx, Action::First(filter(&w)));
        };
        if browse {
            w.on_browse_memory(callback);
        } else {
            w.on_search_memory(callback);
        }
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeMemory>().on_next(move || {
            let next = state.borrow().memory_panel.next.clone();
            if let Some(next) = next {
                enqueue(&state, &tx, Action::Next(next));
            }
        });
    }
    for pin in [false, true] {
        let state = state.clone();
        let tx = tx.clone();
        let callback = move |id: SharedString| {
            let review = state
                .borrow()
                .memory_panel
                .reviews
                .get(id.as_str())
                .cloned();
            if let Some(review) = review {
                let action = if pin {
                    Action::Pin(review.clone(), !review.record().pinned)
                } else if review.record().deleted {
                    Action::Restore(review)
                } else {
                    Action::Trash(review)
                };
                enqueue(&state, &tx, action);
            }
        };
        if pin {
            w.on_toggle_pin_memory(callback);
        } else {
            w.on_delete_memory(callback);
        }
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeMemory>().on_undo_trash(move || {
            let undo = state.borrow().memory_panel.undo.clone();
            if let Some(review) = undo {
                enqueue(&state, &tx, Action::Restore(review));
            }
        });
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        w.global::<NativeMemory>().on_edit(move |id| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            if s.memory_pending {
                return;
            }
            if let Some(review) = s.memory_panel.reviews.get(id.as_str()).cloned() {
                if review.record().deleted {
                    return;
                }
                let g = w.global::<NativeMemory>();
                g.set_fact(review.record().fact.clone().into());
                g.set_tags(review.record().tags.join("\n").into());
                g.set_create_namespace(review.record().namespace.clone().into());
                g.set_editing(true);
                g.set_editor_open(true);
                s.memory_selected = Some(review);
            }
        });
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        w.global::<NativeMemory>().on_new_memory(move || {
            let Some(w) = weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            if s.memory_pending {
                return;
            }
            s.memory_selected = None;
            let g = w.global::<NativeMemory>();
            g.set_editing(false);
            g.set_editor_open(true);
            g.set_create_namespace(g.get_namespace());
            g.set_fact("".into());
            g.set_tags("".into());
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        w.global::<NativeMemory>().on_save(move || {
            let Some(w) = weak.upgrade() else { return };
            let g = w.global::<NativeMemory>();
            let result = (|| -> crate::bridge::Result<Action> {
                let s = state.borrow();
                let fact = g.get_fact().to_string();
                let tags = g
                    .get_tags()
                    .lines()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .collect();
                if let Some(review) = s.memory_selected.clone() {
                    Ok(Action::Edit(review, fact, tags))
                } else {
                    Ok(Action::Create(
                        s.memory_panel
                            .editor
                            .as_ref()
                            .ok_or("Search or Browse memory before creating a record.")?
                            .create(g.get_create_namespace().to_string(), fact, tags)?,
                    ))
                }
            })();
            match result {
                Ok(action) => enqueue(&state, &tx, action),
                Err(error) => state.borrow_mut().last_error = Some(error),
            }
        });
    }
}
pub(super) fn project(w: &MainWindow, s: &UiState) {
    let g = w.global::<NativeMemory>();
    g.set_busy(s.memory_pending || !s.view.available);
    g.set_can_next(s.memory_panel.next.is_some());
    g.set_can_create(s.memory_panel.editor.is_some());
    g.set_can_undo(s.memory_panel.undo.is_some());
    g.set_status(s.memory_panel.message.clone().into());
    w.set_memory_searching(s.memory_pending);
    w.set_memory_browsing(false);
    w.set_memory_rows(ModelRc::new(VecModel::from(
        s.memory_panel
            .rows
            .iter()
            .map(|r| MemoryItem {
                id: r.id.clone().into(),
                summary: r.fact.clone().into(),
                pinned: r.pinned,
                meta: format!(
                    "{} · revision {}{}",
                    r.namespace,
                    r.version,
                    if r.tags.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", r.tags.join(", "))
                    }
                )
                .into(),
            })
            .collect::<Vec<_>>(),
    )));
}
pub(super) fn clear(w: &MainWindow, s: &mut UiState) {
    s.memory_generation = s.memory_generation.wrapping_add(1);
    s.memory_panel = Default::default();
    s.memory_pending = false;
    s.memory_selected = None;
    s.memory_filter = None;
    let g = w.global::<NativeMemory>();
    g.set_fact("".into());
    g.set_tags("".into());
    g.set_namespace("".into());
    g.set_create_namespace("".into());
    g.set_editing(false);
    g.set_editor_open(false);
    g.set_deleted(false);
    w.set_memory_query("".into());
    project(w, s);
}
