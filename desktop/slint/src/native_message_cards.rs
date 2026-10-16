//! One explicit historical detail selection. Uses the original message cards and
//! image surfaces. No historical value is turned into a tool action or URL.
use super::*;
use crate::historical_message_contract::{ImageState, ResultLink};
use crate::historical_source_bridge::HistoricalMessage;

pub(super) struct Action {
    id: String,
    selection: crate::historical_message_contract::MessageSelection,
    image: Option<usize>,
    navigation: NavigationEpoch,
}
impl Action {
    pub fn execute<T: Transport>(
        self,
        b: &mut Bridge<T>,
    ) -> crate::bridge::Result<HistoricalMessage> {
        let c = b
            .view
            .conversation
            .as_ref()
            .ok_or("Conversation changed. Refresh original details.")?;
        if c.chat.id != self.selection.chat_id
            || c.chat.version != self.selection.chat_version
            || c.active_leaf.as_deref() != Some(&self.selection.active_leaf)
        {
            return Err("Conversation changed. Refresh original details.".into());
        }
        b.historical_message(&self.id, &self.navigation, self.image)
    }
}
pub(super) fn wire(
    w: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    let screen_generation = state.borrow().original_screen_generation.clone();
    w.on_native_original_navigation(move || {
        screen_generation.set(
            screen_generation
                .get()
                .checked_add(1)
                .expect("screen generation exhaustion"),
        );
    });
    let state = state.clone();
    let tx = tx.clone();
    let weak = w.as_weak();
    w.on_native_original_details(move |id, image| {
        let mut s = state.borrow_mut();
        if s.original_pending || !s.view.available || s.screen != 0 {
            return;
        }
        let Some(message) = s
            .view
            .conversation
            .as_ref()
            .and_then(|c| c.active_messages.iter().find(|m| m.id == id.as_str()))
        else {
            return;
        };
        if !eligible(message) {
            return;
        }
        let image = if image == -1 {
            None
        } else {
            match usize::try_from(image) {
                Ok(i) if i < 32 => Some(i),
                _ => return,
            }
        };
        let c = s.view.conversation.as_ref().expect("admitted conversation");
        let Some(leaf) = c.active_leaf.clone() else {
            return;
        };
        let selection = crate::historical_message_contract::MessageSelection {
            chat_id: c.chat.id.clone(),
            chat_version: c.chat.version,
            active_leaf: leaf,
            message_id: id.to_string(),
        };
        s.original_pending = true;
        s.original_generation = s
            .original_generation
            .checked_add(1)
            .expect("original generation exhaustion");
        s.original_message = None;
        let _ = tx.send((
            s.epoch.current(),
            Command::Original(
                s.original_generation,
                Action {
                    id: id.to_string(),
                    selection,
                    image,
                    navigation: s.epoch,
                },
            ),
        ));
        if let Some(w) = weak.upgrade() {
            w.set_native_original_pending(true);
            project(&w, &s, &View::default());
        }
    });
}
pub(super) fn eligible(m: &crate::bridge::Message) -> bool {
    // The importer marks every original image/tool/system context as a
    // reference. Ordinary new user turns also have no run ID: do not give them
    // a historical lookup action merely because they are persisted.
    m.persisted && m.run_id.is_none() && m.status == "imported_reference"
}
pub(super) fn invalidate(s: &mut UiState) {
    s.original_generation = s
        .original_generation
        .checked_add(1)
        .expect("original generation exhaustion");
    s.original_message = None;
    s.original_pending = false;
}
pub(super) fn segments(s: &UiState, id: &str) -> Vec<MessageSegment> {
    let Some(message) = s.original_message.as_ref().filter(|p| p.message_id() == id) else {
        return vec![];
    };
    let Ok((cards, preview)) = message.view(&s.view, &s.epoch) else {
        return vec![];
    };
    let note = |text: String| MessageSegment {
        kind: "historical-note".into(),
        text: text.into(),
        ..Default::default()
    };
    let mut rows = Vec::new();
    for call in &cards.calls {
        let mut text = call
            .arguments
            .as_ref()
            .map(|a| a.text.clone())
            .unwrap_or_else(|| "Arguments unavailable; original record retained.".into());
        if call.arguments.as_ref().is_some_and(|a| a.truncated) {
            text.push_str("\n[Preview truncated; full original retained]");
        }
        if call.issue.is_some() || call.arguments_valid_json == Some(false) {
            text.push_str("\n[Original data could not be fully interpreted]");
        }
        rows.push(MessageSegment {
            kind: "tool".into(),
            lang: format!(
                "{} · original record",
                call.name.as_deref().unwrap_or("Tool")
            )
            .into(),
            text: text.into(),
            ..Default::default()
        });
    }
    match &cards.result_link {
        ResultLink::NotToolResult => (),
        ResultLink::Unique { call_id, .. } => rows.push(note(format!(
            "Original result for call {call_id}. Historical record only."
        ))),
        ResultLink::Ambiguous { .. } => rows.push(note(
            "Original result has multiple possible calls; no link selected.".into(),
        )),
        _ => rows.push(note(
            "Original result has no verified call link; its text is retained above.".into(),
        )),
    }
    for reference in &cards.images {
        match &reference.state {
            ImageState::Embedded { width, height, .. } => {
                if let Some(p) = preview.filter(|p| p.index == reference.index) {
                    let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &p.rgba, p.width, p.height,
                    );
                    rows.push(MessageSegment {
                        kind: "historical-image".into(),
                        image: slint::Image::from_rgba8(pixels),
                        ..Default::default()
                    });
                    rows.push(note(format!(
                        "Original image {} · {width} × {height}",
                        reference.index + 1
                    )));
                } else {
                    rows.push(MessageSegment {
                        kind: "historical-image-load".into(),
                        text: format!("Preview original image {}", reference.index + 1).into(),
                        action_index: reference.index as i32,
                        ..Default::default()
                    });
                }
            }
            ImageState::RemoteUnresolved { .. } => rows.push(note(format!(
                "Image {}: external reference retained; not fetched.",
                reference.index + 1
            ))),
            ImageState::Retained { .. } => rows.push(note(format!(
                "Image {}: preview unavailable; original retained.",
                reference.index + 1
            ))),
        }
    }
    if !cards.issues.is_empty() {
        rows.push(note(
            "Some original fields cannot be previewed. Their exact data remains retained.".into(),
        ));
    }
    if rows.is_empty() {
        rows.push(note(
            "No original images or tool details on this message.".into(),
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_persisted_user_turn_is_not_a_historical_reference() {
        let mut m = crate::bridge::Message {
            id: "new-user".into(),
            role: "user".into(),
            status: "completed".into(),
            persisted: true,
            ..Default::default()
        };
        assert!(!eligible(&m));
        m.status = "imported_reference".into();
        assert!(eligible(&m));
        m.run_id = Some("new-run".into());
        assert!(!eligible(&m));
    }
}
