//! Read-only source lookup for one admitted active-branch message. No card/image
//! fetch, original-ID inference, UI change or execution authority is added here.
use crate::{
    bridge::{encode_component, Bridge, NavigationEpoch, Result, Transport},
    historical_message_contract::{MessageSelection, SelectedMessageSource, SourceMapping},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone, PartialEq, Eq)]
struct Context {
    session: String,
    navigation: u64,
    selection: MessageSelection,
    status: String,
}
fn stale() -> String {
    "This original-message lookup belongs to an earlier conversation or workspace. Refresh it."
        .into()
}
fn context<T: Transport>(bridge: &Bridge<T>, id: &str, epoch: &NavigationEpoch) -> Result<Context> {
    if !bridge.view.available {
        return Err(stale());
    }
    let session = bridge.session.clone().ok_or_else(stale)?;
    let conversation = bridge.view.conversation.as_ref().ok_or_else(stale)?;
    let mut matching = conversation.active_messages.iter().filter(|m| m.id == id);
    let message = matching.next().ok_or_else(stale)?;
    if matching.next().is_some()
        || !message.persisted
        || message.run_id.is_some()
        || !matches!(
            message.status.as_str(),
            "completed" | "cancelled" | "failed" | "interrupted" | "imported_reference"
        )
    {
        return Err(stale());
    }
    let selection = MessageSelection {
        chat_id: conversation.chat.id.clone(),
        chat_version: conversation.chat.version,
        active_leaf: conversation.active_leaf.clone().ok_or_else(stale)?,
        message_id: id.into(),
    };
    if !selection.valid() {
        return Err(stale());
    }
    Ok(Context {
        session,
        navigation: epoch.current(),
        selection,
        status: message.status.clone(),
    })
}

/// Private captured identity; cloning retains the same one-use guard. Do not
/// derive Debug/Serialize because the context includes profile admission.
#[derive(Clone)]
pub struct SourceLookup {
    context: Context,
    used: Arc<AtomicBool>,
}
pub struct MessageSourceReceipt {
    context: Context,
    mapping: SourceMapping,
}
impl MessageSourceReceipt {
    /// The controller must pass its current live epoch, not the queued command's
    /// copy. Navigation/revoke clears retained receipts before projecting views.
    pub fn mapping<'a, T: Transport>(
        &'a self,
        bridge: &'a Bridge<T>,
        epoch: &'a NavigationEpoch,
    ) -> Result<&'a SourceMapping> {
        if context(bridge, &self.context.selection.message_id, epoch)? != self.context {
            return Err(stale());
        }
        Ok(&self.mapping)
    }
}
impl<T: Transport> Bridge<T> {
    pub fn review_message_source(
        &self,
        message_id: &str,
        epoch: &NavigationEpoch,
    ) -> Result<SourceLookup> {
        Ok(SourceLookup {
            context: context(self, message_id, epoch)?,
            used: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn fetch_message_source(
        &mut self,
        lookup: &SourceLookup,
        epoch: &NavigationEpoch,
    ) -> Result<MessageSourceReceipt> {
        if lookup.used.swap(true, Ordering::SeqCst) {
            return Err(
                "This original-message lookup was already used. Refresh before retrying.".into(),
            );
        }
        let expected = &lookup.context;
        if context(self, &expected.selection.message_id, epoch)? != *expected {
            return Err(stale());
        }
        self.check_session()?;
        let s = &expected.selection;
        let path = format!(
            "/api/legacy-import/message-source/{}?chat_id={}&chat_version={}&active_leaf={}",
            encode_component(&s.message_id),
            encode_component(&s.chat_id),
            s.chat_version,
            encode_component(&s.active_leaf)
        );
        let result = self
            .transport
            .request("GET", &path, Some(&expected.session), None);
        self.check_session()?;
        if context(self, &s.message_id, epoch)? != *expected {
            return Err(stale());
        }
        let value = result?;
        if serde_json::to_vec(&value).map_err(|_| stale())?.len() > 16 * 1024 {
            return Err("Original-message source receipt exceeds its bound.".into());
        }
        let reply: SelectedMessageSource = serde_json::from_value(value).map_err(|_| stale())?;
        if reply.selection != *s
            || reply.message_status != expected.status
            || !reply.original_source.source.valid()
            || reply.original_source.original_message_id != s.message_id
            || reply.original_source.original_session_id != s.chat_id
        {
            return Err(stale());
        }
        Ok(MessageSourceReceipt {
            context: expected.clone(),
            mapping: reply.original_source,
        })
    }
}

// The source receipt is the only adapter authority. UI requests carry a current
// message ID; neither archive row numbers nor source hashes come from a widget.
struct CardsAdapter<'a, T: Transport> {
    bridge: &'a mut Bridge<T>,
    epoch: &'a NavigationEpoch,
    receipt: &'a MessageSourceReceipt,
}
impl<T: Transport> crate::historical_message_bridge::OwnedMessageTransport for CardsAdapter<'_, T> {
    fn displayed(&self, id: &str) -> Option<crate::historical_message_bridge::DisplayContext> {
        let c = context(self.bridge, id, self.epoch).ok()?;
        if c != self.receipt.context {
            return None;
        }
        let mapping = self.receipt.mapping(self.bridge, self.epoch).ok()?.clone();
        Some(crate::historical_message_bridge::DisplayContext {
            profile_session: c.session,
            navigation_epoch: c.navigation,
            chat_id: c.selection.chat_id,
            chat_version: c.selection.chat_version,
            active_leaf: Some(c.selection.active_leaf),
            message_id: id.into(),
            persisted: true,
            run_id: None,
            message_status: c.status,
            source: Some(mapping),
        })
    }
    fn check_profile_session(
        &mut self,
        expected: &str,
    ) -> crate::historical_message_bridge::Result<()> {
        self.bridge.check_session().map_err(|_| {
            crate::historical_message_bridge::Error("Workspace is no longer available.")
        })?;
        if self.bridge.session.as_deref() != Some(expected) {
            return Err(crate::historical_message_bridge::Error(
                "Workspace changed. Refresh original details.",
            ));
        }
        Ok(())
    }
    fn read_cards(
        &mut self,
        request: &crate::historical_message_bridge::CardsRequest,
        body: &mut crate::historical_message_bridge::ResponseBody,
    ) -> crate::historical_message_bridge::Result<crate::historical_message_bridge::Headers> {
        self.bridge
            .transport
            .historical_cards(request, body)
            .map_err(|_| {
                crate::historical_message_bridge::Error(
                    "Cannot read original message cards. Refresh before retrying.",
                )
            })
    }
    fn read_image(
        &mut self,
        request: &crate::historical_message_bridge::ImageRequest,
        body: &mut crate::historical_message_bridge::ResponseBody,
    ) -> crate::historical_message_bridge::Result<crate::historical_message_bridge::Headers> {
        self.bridge
            .transport
            .historical_image(request, body)
            .map_err(|_| {
                crate::historical_message_bridge::Error(
                    "Cannot read original image. Refresh before retrying.",
                )
            })
    }
}
#[derive(Clone)]
pub struct HistoricalMessage {
    context: Context,
    cards: Arc<crate::historical_message_contract::MessageCards>,
    preview: Option<Arc<RasterPreview>>,
}
pub struct RasterPreview {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
impl HistoricalMessage {
    pub fn message_id(&self) -> &str {
        &self.context.selection.message_id
    }
    /// UI adoption must use the current live epoch and admitted view. A queued
    /// command's snapshot is never sufficient. No bytes/metadata escape stale views.
    pub fn view<'a>(
        &'a self,
        view: &'a crate::bridge::View,
        epoch: &'a NavigationEpoch,
    ) -> Result<(
        &'a crate::historical_message_contract::MessageCards,
        Option<&'a RasterPreview>,
    )> {
        let c = view.conversation.as_ref().ok_or_else(stale)?;
        let expected = &self.context.selection;
        let matches = c
            .active_messages
            .iter()
            .filter(|m| m.id == expected.message_id)
            .collect::<Vec<_>>();
        if !view.available
            || epoch.current() != self.context.navigation
            || c.chat.id != expected.chat_id
            || c.chat.version != expected.chat_version
            || c.active_leaf.as_deref() != Some(&expected.active_leaf)
            || matches.len() != 1
            || matches[0].status != self.context.status
            || !matches[0].persisted
            || matches[0].run_id.is_some()
        {
            return Err(stale());
        }
        Ok((&self.cards, self.preview.as_deref()))
    }
}
impl<T: Transport> Bridge<T> {
    /// Serialized worker-only read. One selected card page and at most one
    /// explicitly selected embedded raster; remote/file references stay dormant.
    pub fn historical_message(
        &mut self,
        message_id: &str,
        epoch: &NavigationEpoch,
        image_index: Option<usize>,
    ) -> Result<HistoricalMessage> {
        use crate::historical_message_bridge as cards;
        let lookup = self.review_message_source(message_id, epoch)?;
        let receipt = self.fetch_message_source(&lookup, epoch)?;
        let mut adapter = CardsAdapter {
            bridge: self,
            epoch,
            receipt: &receipt,
        };
        let review = cards::review(&mut adapter, message_id).map_err(|e| e.to_string())?;
        let page = cards::cards(&mut adapter, &review).map_err(|e| e.to_string())?;
        let preview = if let Some(index) = image_index {
            let review = page
                .image_review(&adapter, index)
                .map_err(|e| e.to_string())?;
            let image = cards::image(&mut adapter, &review).map_err(|e| e.to_string())?;
            let bytes = image.view(&adapter).map_err(|e| e.to_string())?;
            // The shared validator already decoded the complete static raster.
            // Bound this second decode too, then transfer only a <=1024px preview
            // to Slint. No temporary file, path loader or URL loader is involved.
            let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes.bytes))
                .with_guessed_format()
                .map_err(|_| "Cannot decode original image.")?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(8192);
            limits.max_image_height = Some(8192);
            limits.max_alloc = Some(64 * 1024 * 1024);
            reader.limits(limits);
            let pixels = reader
                .decode()
                .map_err(|_| "Cannot decode original image.")?
                .thumbnail(1024, 1024)
                .to_rgba8();
            Some(Arc::new(RasterPreview {
                index,
                width: pixels.width(),
                height: pixels.height(),
                rgba: pixels.into_raw(),
            }))
        } else {
            None
        };
        let value = page.view(&adapter).map_err(|e| e.to_string())?.clone();
        adapter.bridge.check_session()?;
        receipt.mapping(adapter.bridge, epoch)?;
        Ok(HistoricalMessage {
            context: receipt.context,
            cards: Arc::new(value),
            preview,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Chat, Conversation, Message};
    use serde_json::{json, Value};
    use std::sync::Mutex;
    struct State {
        reply: Value,
        reads: usize,
        rotate: bool,
        fail: bool,
        session: String,
    }
    struct Mock(Arc<Mutex<State>>);
    impl Transport for Mock {
        fn request(
            &mut self,
            method: &str,
            path: &str,
            session: Option<&str>,
            body: Option<Value>,
        ) -> Result<Value> {
            let mut state = self.0.lock().unwrap();
            assert_eq!(method, "GET");
            assert!(body.is_none());
            if path == "/api/profile/status" {
                return Ok(json!({"state":"unlocked","session":state.session}));
            }
            assert_eq!(session, Some("session-admission"));
            assert_eq!(path, "/api/legacy-import/message-source/message%2Fone?chat_id=chat%3Fone&chat_version=4&active_leaf=leaf%26one");
            state.reads += 1;
            if state.rotate {
                state.session = "rotated".into();
            }
            if state.fail {
                Err("Lost response".into())
            } else {
                Ok(state.reply.clone())
            }
        }
    }
    fn setup() -> (Bridge<Mock>, Arc<Mutex<State>>, NavigationEpoch) {
        let state = Arc::new(Mutex::new(State {
            reply: json!({
            "selection":{"chat_id":"chat?one","chat_version":4,"active_leaf":"leaf&one","message_id":"message/one"},
            "message_status":"imported_reference", "original_source":{
                "source":{"import_id":"import","source_schema":25,"table":"messages","row_number":91,"payload_sha256":"a".repeat(64)},
                "original_message_id":"message/one","original_session_id":"chat?one"}}),
            reads: 0,
            rotate: false,
            fail: false,
            session: "session-admission".into(),
        }));
        let mut bridge = Bridge::new(Mock(state.clone()));
        bridge.session = Some("session-admission".into());
        bridge.view.available = true;
        bridge.view.conversation = Some(Conversation {
            chat: Chat {
                id: "chat?one".into(),
                version: 4,
                ..Default::default()
            },
            active_leaf: Some("leaf&one".into()),
            active_messages: vec![Message {
                id: "message/one".into(),
                persisted: true,
                status: "imported_reference".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        (bridge, state, NavigationEpoch::default())
    }
    #[test]
    fn source_lookup_uses_header_and_percent_encoded_selection_with_shared_one_use_guard() {
        let (mut b, state, e) = setup();
        let lookup = b.review_message_source("message/one", &e).unwrap();
        let clone = lookup.clone();
        let receipt = b.fetch_message_source(&lookup, &e).unwrap();
        assert_eq!(receipt.mapping(&b, &e).unwrap().source.row_number, 91);
        assert!(b.fetch_message_source(&clone, &e).is_err());
        assert_eq!(state.lock().unwrap().reads, 1);
    }
    #[test]
    fn source_lookup_requires_unique_persisted_active_member_without_new_run() {
        for case in 0..5 {
            let (mut b, state, e) = setup();
            let c = b.view.conversation.as_mut().unwrap();
            match case {
                0 => c.active_messages.clear(),
                1 => c.active_messages.push(c.active_messages[0].clone()),
                2 => c.active_messages[0].persisted = false,
                3 => c.active_messages[0].run_id = Some("new-run".into()),
                _ => c.active_messages[0].status = "streaming".into(),
            }
            assert!(b.review_message_source("message/one", &e).is_err());
            assert_eq!(state.lock().unwrap().reads, 0);
        }
    }
    #[test]
    fn source_lookup_refuses_every_stale_display_dimension_before_read() {
        for case in 0..6 {
            let (mut b, state, mut e) = setup();
            let lookup = b.review_message_source("message/one", &e).unwrap();
            match case {
                0 => {
                    e.advance();
                }
                1 => b.view.conversation.as_mut().unwrap().chat.version += 1,
                2 => b.view.conversation.as_mut().unwrap().chat.id = "other".into(),
                3 => b.view.conversation.as_mut().unwrap().active_leaf = Some("other".into()),
                4 => {
                    b.view.conversation.as_mut().unwrap().active_messages[0].status =
                        "cancelled".into()
                }
                _ => b.revoke(),
            }
            assert!(b.fetch_message_source(&lookup, &e).is_err());
            assert_eq!(state.lock().unwrap().reads, 0);
        }
    }
    #[test]
    fn source_lookup_rejects_mismatched_receipts_and_unknown_fields() {
        for case in 0..8 {
            let (mut b, state, e) = setup();
            {
                let mut s = state.lock().unwrap();
                match case {
                    0 => s.reply["selection"]["chat_version"] = json!(5),
                    1 => s.reply["message_status"] = json!("completed"),
                    2 => s.reply["original_source"]["source"]["payload_sha256"] = json!("bad"),
                    3 => s.reply["original_source"]["original_message_id"] = json!("other"),
                    4 => s.reply["original_source"]["original_session_id"] = json!("other"),
                    5 => s.reply["original_source"]["source"]["table"] = json!("artifacts"),
                    6 => s.reply["extra"] = json!(true),
                    _ => s.reply["extra"] = json!("x".repeat(16385)),
                }
            }
            let lookup = b.review_message_source("message/one", &e).unwrap();
            assert!(b.fetch_message_source(&lookup, &e).is_err());
        }
    }
    #[test]
    fn source_lookup_lost_reply_consumes_review_without_retry() {
        let (mut b, state, e) = setup();
        state.lock().unwrap().fail = true;
        let lookup = b.review_message_source("message/one", &e).unwrap();
        assert!(b.fetch_message_source(&lookup, &e).is_err());
        state.lock().unwrap().fail = false;
        assert!(b.fetch_message_source(&lookup, &e).is_err());
        assert_eq!(state.lock().unwrap().reads, 1);
    }
    #[test]
    fn source_lookup_revoked_reply_clears_workspace_and_returns_no_mapping() {
        let (mut b, state, e) = setup();
        state.lock().unwrap().rotate = true;
        let lookup = b.review_message_source("message/one", &e).unwrap();
        assert!(b.fetch_message_source(&lookup, &e).is_err());
        assert!(!b.view.available);
        assert!(b.view.conversation.is_none());
    }
    #[test]
    fn source_receipt_expires_after_navigation_even_if_same_selection_returns() {
        let (mut b, _, mut e) = setup();
        let lookup = b.review_message_source("message/one", &e).unwrap();
        let receipt = b.fetch_message_source(&lookup, &e).unwrap();
        e.advance();
        e.advance();
        assert!(receipt.mapping(&b, &e).is_err());
        b.revoke();
        assert!(receipt.mapping(&b, &e).is_err());
    }
}
