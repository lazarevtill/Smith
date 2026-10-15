use super::*;
use crate::historical_memory_bridge::*;
use std::collections::BTreeMap;
#[derive(Clone, Default)]
pub(super) struct Panel {
    pub filter: Option<HistoricalFilter>,
    pub rows: Vec<HistoricalRecord>,
    pub reviews: BTreeMap<i64, HistoricalReview>,
    pub next: Option<HistoricalCursor>,
    pub undo: Option<HistoricalReview>,
    pub details: String,
    pub message: String,
    pub notices: Vec<String>,
    pub scanned: usize,
    pub total: u64,
}
pub(super) enum Action {
    First(HistoricalFilter),
    Next(HistoricalCursor),
    Detail(HistoricalReview),
    Trash(HistoricalReview),
    Restore(HistoricalReview),
}
impl Panel {
    fn adopt(&mut self, page: HistoricalPage) -> crate::bridge::Result<()> {
        let reviews = page
            .records
            .iter()
            .map(|r| Ok((r.source.row_number, page.review(r)?)))
            .collect::<crate::bridge::Result<Vec<_>>>()?;
        self.reviews.extend(reviews);
        self.rows.extend(page.records);
        self.scanned += page.scanned;
        self.total = page.total_archived_rows;
        self.next = page.next;
        self.filter = Some(page.filter);
        self.notices.extend(page.retained_issues.iter().map(|i| {
            format!(
                "Original row {} remains retained but cannot be shown: {} ({} bytes).",
                i.row_number, i.code, i.payload_bytes
            )
        }));
        self.message = format!(
            "{} records shown · {} of {} archived rows scanned.",
            self.rows.len(),
            self.scanned,
            self.total
        );
        Ok(())
    }
}
impl Action {
    pub fn execute<T: Transport>(
        self,
        b: &mut Bridge<T>,
        p: &mut Panel,
    ) -> crate::bridge::Result<()> {
        if matches!(&self, Self::Trash(_) | Self::Restore(_)) {
            p.next = None;
        }
        let receipt = match self {
            Self::First(filter) => {
                *p = Default::default();
                p.adopt(b.historical_first(filter)?)?;
                return Ok(());
            }
            Self::Next(cursor) => {
                p.next = None;
                let page = b.historical_next(&cursor)?;
                if p.filter.as_ref() != Some(&page.filter) {
                    return Err("Historical filters changed. Browse again.".into());
                }
                p.adopt(page)?;
                return Ok(());
            }
            Self::Detail(review) => {
                let detail = b.historical_detail(&review)?;
                p.details = serde_json::to_string_pretty(
                    &serde_json::json!({"record":detail.record(),"exact_original":detail.original}),
                )
                .map_err(|_| "Cannot display exact original details.")?;
                let receipt = detail.review();
                p.reviews
                    .insert(receipt.record().source.row_number, receipt);
                return Ok(());
            }
            Self::Trash(review) => {
                let receipt = b.historical_trash(&review)?;
                p.undo = Some(receipt.clone());
                p.message="Record moved to local historical Trash. The imported original remains retained.".into();
                receipt
            }
            Self::Restore(review) => {
                let receipt = b.historical_restore(&review)?;
                p.undo = None;
                p.message = "Historical record restored. Active recall remains unchanged.".into();
                receipt
            }
        };
        let row = receipt.record().clone();
        p.rows.retain(|r| r.source != row.source);
        p.reviews.remove(&row.source.row_number);
        p.details.clear();
        if p.filter
            .as_ref()
            .is_some_and(|f| f.trash == row.overlay.trashed)
        {
            p.reviews.insert(row.source.row_number, receipt);
            p.rows.insert(0, row);
        }
        Ok(())
    }
}
fn filter(w: &MainWindow) -> HistoricalFilter {
    let g = w.global::<NativeHistorical>();
    HistoricalFilter {
        kind: if g.get_mode() == 2 {
            HistoricalKind::Lesson
        } else {
            HistoricalKind::Episode
        },
        scope: match g.get_scope() {
            1 => AgentScope::Global,
            2 => AgentScope::Agent {
                id: g.get_agent().trim().into(),
            },
            _ => AgentScope::All,
        },
        session_id: (!g.get_session().trim().is_empty()).then(|| g.get_session().trim().into()),
        query: g.get_query().to_string(),
        trash: g.get_trash(),
    }
}
pub(super) fn capture_filter(w: &MainWindow, s: &mut UiState) {
    let identity = (w.global::<NativeHistorical>().get_mode(), filter(w));
    if s.historical_filter.as_ref() != Some(&identity) {
        s.historical_generation = s.historical_generation.wrapping_add(1);
        s.historical_filter = Some(identity);
        s.historical_panel = Default::default();
        s.historical_pending = false;
        w.global::<NativeHistorical>().set_exact_details(false);
    }
}
fn enqueue(state: &Rc<RefCell<UiState>>, tx: &mpsc::Sender<(u64, Command)>, a: Action) {
    let mut s = state.borrow_mut();
    if !s.view.available || s.historical_pending {
        return;
    }
    s.historical_pending = true;
    let _ = tx.send((
        s.epoch.current(),
        Command::Historical(s.historical_generation, a),
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
        w.global::<NativeHistorical>().on_scope_changed(move || {
            if let Some(w) = weak.upgrade() {
                capture_filter(&w, &mut state.borrow_mut());
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        w.global::<NativeHistorical>().on_search(move || {
            if let Some(w) = weak.upgrade() {
                capture_filter(&w, &mut state.borrow_mut());
                enqueue(&state, &tx, Action::First(filter(&w)));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeHistorical>().on_next(move || {
            let next = state.borrow().historical_panel.next.clone();
            if let Some(cursor) = next {
                enqueue(&state, &tx, Action::Next(cursor));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeHistorical>().on_undo(move || {
            let undo = state.borrow().historical_panel.undo.clone();
            if let Some(review) = undo {
                enqueue(&state, &tx, Action::Restore(review));
            }
        });
    }
    for remove in [false, true] {
        let state = state.clone();
        let tx = tx.clone();
        let callback = move |id: SharedString| {
            let r = id
                .parse::<i64>()
                .ok()
                .and_then(|n| state.borrow().historical_panel.reviews.get(&n).cloned());
            if let Some(review) = r {
                let action = if !remove {
                    Action::Detail(review)
                } else if review.record().overlay.trashed {
                    Action::Restore(review)
                } else {
                    Action::Trash(review)
                };
                enqueue(&state, &tx, action);
            }
        };
        if remove {
            w.global::<NativeHistorical>().on_remove(callback);
        } else {
            w.global::<NativeHistorical>().on_inspect(callback);
        }
    }
}
pub(super) fn project(w: &MainWindow, s: &UiState) {
    let g = w.global::<NativeHistorical>();
    let p = &s.historical_panel;
    g.set_busy(s.historical_pending || !s.view.available);
    g.set_can_next(p.next.is_some());
    g.set_can_undo(p.undo.is_some());
    g.set_status(p.message.clone().into());
    g.set_notices(p.notices.join("\n").into());
    g.set_details(p.details.clone().into());
    g.set_rows(ModelRc::new(VecModel::from(
        p.rows
            .iter()
            .map(|r| HistoricalItem {
                id: r.source.row_number.to_string().into(),
                summary: crate::historical_presentation::summary(&r.data).into(),
                metadata: crate::historical_presentation::metadata(&r.data).into(),
            })
            .collect::<Vec<_>>(),
    )));
}
pub(super) fn clear(w: &MainWindow, s: &mut UiState) {
    s.historical_generation = s.historical_generation.wrapping_add(1);
    s.historical_filter = None;
    s.historical_panel = Default::default();
    s.historical_pending = false;
    let g = w.global::<NativeHistorical>();
    g.set_mode(0);
    g.set_scope(0);
    g.set_agent("".into());
    g.set_session("".into());
    g.set_query("".into());
    g.set_trash(false);
    g.set_exact_details(false);
    project(w, s);
}
