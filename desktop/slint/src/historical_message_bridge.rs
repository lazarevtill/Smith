//! No filesystem, process, URL fetch, tool dispatch, or raw-ID review constructor.
//! The adapter is trusted to expose only its admitted, currently displayed branch.
use crate::historical_memory_contract::lower_hash;
pub use crate::historical_message_contract::SourceMapping;
use crate::historical_message_contract::*;
use serde::{de, Deserialize};
use std::{
    collections::BTreeSet,
    fmt,
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
fn stale() -> Error {
    Error("The displayed branch or workspace changed. Refresh the original cards.")
}
fn invalid() -> Error {
    Error("The original card receipt is invalid. Exact archived values remain retained.")
}
fn identity(s: &str) -> bool {
    !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
}

/// Adapter input, not an HTTP DTO or a UI command. It contains only the selected
/// persisted member of the currently displayed active_messages collection.
/// The native adapter must reject duplicate IDs and missing/ambiguous mappings.
/// Do not derive Debug/Serialize: profile_session is an ephemeral admission value.
#[derive(Clone, PartialEq, Eq)]
pub struct DisplayContext {
    pub profile_session: String,
    pub navigation_epoch: u64,
    pub chat_id: String,
    pub chat_version: i64,
    pub active_leaf: Option<String>,
    pub message_id: String,
    pub persisted: bool,
    pub run_id: Option<String>,
    pub message_status: String,
    pub source: Option<SourceMapping>,
}
impl DisplayContext {
    fn valid(&self) -> bool {
        identity(&self.profile_session)
            && identity(&self.chat_id)
            && self.chat_version >= 0
            && self.active_leaf.as_deref().is_some_and(identity)
            && identity(&self.message_id)
            && self.persisted
            // Original imported messages have no new inference run. Do not
            // attach historical cards to a newly generated/replaced message.
            && self.run_id.is_none()
            && identity(&self.message_status)
            && self.source.as_ref().is_some_and(|m| {
                m.source.valid()
                    && identity(&m.original_message_id)
                    && identity(&m.original_session_id)
            })
    }
}

/// Implement only in the owned native adapter. Every request uses the retained
/// listener, exact owner header and profile-session HEADER, never a query token.
/// Check child ownership before/after; enforce finite deadlines/header limits;
/// accept neither redirects nor arbitrary paths. Streaming into ResponseBody
/// enforces the candidate's cap before extending its owned allocation.
pub trait OwnedMessageTransport {
    fn displayed(&self, message_id: &str) -> Option<DisplayContext>;
    fn check_profile_session(&mut self, expected: &str) -> Result<()>;
    fn read_cards(&mut self, request: &CardsRequest, body: &mut ResponseBody) -> Result<Headers>;
    fn read_image(&mut self, request: &ImageRequest, body: &mut ResponseBody) -> Result<Headers>;
}

/// Exact header values from one successful response, with duplicate security
/// headers refused by the transport. Dimensions are revalidated from the bytes.
pub struct Headers {
    pub status: u16,
    pub content_type: String,
    pub content_sha256: Option<String>,
    pub cache_control: String,
    pub content_type_options: String,
}
impl Headers {
    fn valid(&self, mime: &str) -> bool {
        self.status == 200
            && self.content_type == mime
            && self.cache_control == "no-store"
            && self.content_type_options == "nosniff"
    }
}

/// Created only by the bridge with its fixed response budget. A transport may
/// write chunks, but cannot change the cap or extract an unreviewed response.
pub struct ResponseBody {
    bytes: Vec<u8>,
    limit: usize,
    overflow: bool,
}
impl ResponseBody {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            overflow: false,
        }
    }
    fn finish(self) -> Result<Vec<u8>> {
        if self.overflow {
            Err(invalid())
        } else {
            Ok(self.bytes)
        }
    }
}
impl Write for ResponseBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.overflow || bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.overflow = true;
            return Err(io::Error::other("historical response byte bound"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct CardsRequest {
    context: DisplayContext,
}
impl CardsRequest {
    pub fn session(&self) -> &str {
        &self.context.profile_session
    }
    pub fn source(&self) -> &MessageSource {
        &self
            .context
            .source
            .as_ref()
            .expect("validated context")
            .source
    }
    pub fn path(&self) -> String {
        format!(
            "/api/legacy-import/message-cards/{}",
            self.source().row_number
        )
    }
}
pub struct ImageRequest {
    cards: CardsRequest,
    index: usize,
}
impl ImageRequest {
    pub fn session(&self) -> &str {
        self.cards.session()
    }
    pub fn source(&self) -> &MessageSource {
        self.cards.source()
    }
    pub fn index(&self) -> usize {
        self.index
    }
    pub fn path(&self) -> String {
        let source = self.source();
        format!(
            "/api/legacy-import/message-cards/{}/images/{}/bytes?import_id={}&source_schema=25&payload_sha256={}",
            source.row_number, self.index, component(&source.import_id), source.payload_sha256,
        )
    }
}
fn component(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Shared consume guard prevents clones from retrying an uncertain request.
#[derive(Clone)]
pub struct MessageReview {
    context: DisplayContext,
    used: Arc<AtomicBool>,
}
#[derive(Clone)]
pub struct ImageReview {
    context: DisplayContext,
    image: ImageReference,
    used: Arc<AtomicBool>,
}
pub struct CardPage {
    context: DisplayContext,
    cards: MessageCards,
    images: Vec<ImageReview>,
}
impl CardPage {
    /// Literal details only: no execute/retry/follow-URL method exists.
    pub fn view<'a, T: OwnedMessageTransport>(
        &'a self,
        transport: &'a T,
    ) -> Result<&'a MessageCards> {
        same_display(transport, &self.context)?;
        Ok(&self.cards)
    }
    pub fn image_review<T: OwnedMessageTransport>(
        &self,
        transport: &T,
        index: usize,
    ) -> Result<ImageReview> {
        same_display(transport, &self.context)?;
        self.images
            .iter()
            .find(|r| r.image.index == index)
            .cloned()
            .ok_or(Error(
                "This reference has no embedded static image preview.",
            ))
    }
}

pub struct ValidatedPreview {
    context: DisplayContext,
    image: ImageReference,
    bytes: Vec<u8>,
}
pub struct ImageView<'a> {
    pub bytes: &'a [u8],
    pub metadata: &'a ImageReference,
}
impl ValidatedPreview {
    /// Recheck local selection on each adoption/render boundary. The controller
    /// must also drop previews on profile revocation or navigation, including
    /// leaving and returning to the same message (epoch prevents that ABA).
    pub fn view<'a, T: OwnedMessageTransport>(&'a self, transport: &'a T) -> Result<ImageView<'a>> {
        same_display(transport, &self.context)?;
        Ok(ImageView {
            bytes: &self.bytes,
            metadata: &self.image,
        })
    }
}
fn same_display<T: OwnedMessageTransport>(transport: &T, expected: &DisplayContext) -> Result<()> {
    if transport.displayed(&expected.message_id).as_ref() != Some(expected) {
        Err(stale())
    } else {
        Ok(())
    }
}
fn current<T: OwnedMessageTransport>(transport: &mut T, expected: &DisplayContext) -> Result<()> {
    same_display(transport, expected)?;
    transport.check_profile_session(&expected.profile_session)?;
    same_display(transport, expected)
}
fn consume(used: &AtomicBool) -> Result<()> {
    if used.swap(true, Ordering::SeqCst) {
        Err(Error(
            "This original review was already used. Refresh before another request.",
        ))
    } else {
        Ok(())
    }
}

/// Takes a current UI message ID only. The source is obtained from the owned
/// adapter's admitted active branch, never supplied by the UI command.
pub fn review<T: OwnedMessageTransport>(
    transport: &mut T,
    message_id: &str,
) -> Result<MessageReview> {
    let context = transport.displayed(message_id).ok_or_else(stale)?;
    if !context.valid() || context.message_id != message_id {
        return Err(invalid());
    }
    current(transport, &context)?;
    Ok(MessageReview {
        context,
        used: Arc::new(AtomicBool::new(false)),
    })
}
pub fn cards<T: OwnedMessageTransport>(
    transport: &mut T,
    review: &MessageReview,
) -> Result<CardPage> {
    consume(&review.used)?;
    current(transport, &review.context)?;
    let request = CardsRequest {
        context: review.context.clone(),
    };
    let mut body = ResponseBody::new(MAX_CARD_BYTES);
    let response = transport.read_cards(&request, &mut body);
    current(transport, &review.context)?;
    let headers = response?;
    if !headers.valid("application/json") {
        return Err(invalid());
    }
    let cards: WireCards = serde_json::from_slice(&body.finish()?).map_err(|_| invalid())?;
    let cards = cards.into_cards();
    validate_cards(&cards, &review.context)?;
    let images = cards
        .images
        .iter()
        .filter(|image| matches!(image.state, ImageState::Embedded { .. }))
        .map(|image| ImageReview {
            context: review.context.clone(),
            image: image.clone(),
            used: Arc::new(AtomicBool::new(false)),
        })
        .collect();
    Ok(CardPage {
        context: review.context.clone(),
        cards,
        images,
    })
}

/// Explicit bounded read only. The reviewed card determines both request and
/// validation metadata. Run this on the serialized, owned blocking adapter, not
/// the UI thread; shutdown must retain/drain it rather than abandon the read.
pub fn image<T: OwnedMessageTransport>(
    transport: &mut T,
    review: &ImageReview,
) -> Result<ValidatedPreview> {
    consume(&review.used)?;
    current(transport, &review.context)?;
    let ImageState::Embedded {
        mime,
        decoded_bytes,
        decoded_sha256,
        width,
        height,
    } = &review.image.state
    else {
        return Err(invalid());
    };
    let request = ImageRequest {
        cards: CardsRequest {
            context: review.context.clone(),
        },
        index: review.image.index,
    };
    let mut body = ResponseBody::new(MAX_IMAGE_BYTES);
    let response = transport.read_image(&request, &mut body);
    current(transport, &review.context)?;
    let headers = response?;
    if !headers.valid(mime) || headers.content_sha256.as_ref() != Some(decoded_sha256) {
        return Err(invalid());
    }
    let bytes = body.finish()?;
    if bytes.len() as u64 != *decoded_bytes || digest(&bytes) != *decoded_sha256 {
        return Err(invalid());
    }
    let filename = image_filename(mime).ok_or_else(invalid)?;
    let limits = smith_media::Limits {
        max_frames: 1,
        ..smith_media::Limits::default()
    };
    let manifest = smith_media::inspect(
        smith_media::AttachmentRequest::new(filename, Some(mime)),
        &bytes,
        &limits,
    )
    .map_err(|_| Error("The original image is not a valid bounded static raster."))?;
    if manifest.sha256 != *decoded_sha256
        || !matches!(manifest.content,
        smith_media::Content::Image { width: w, height: h, frames: 1, .. } if w == *width && h == *height)
    {
        return Err(invalid());
    }
    current(transport, &review.context)?;
    Ok(ValidatedPreview {
        context: review.context.clone(),
        image: review.image.clone(),
        bytes,
    })
}
fn image_filename(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("original.png"),
        "image/jpeg" => Some("original.jpg"),
        "image/webp" => Some("original.webp"),
        _ => None,
    }
}
fn digest(bytes: &[u8]) -> String {
    smith_media::sha256(bytes)
}

fn validate_cards(cards: &MessageCards, context: &DisplayContext) -> Result<()> {
    let mapping = context.source.as_ref().ok_or_else(invalid)?;
    if cards.source != mapping.source
        || cards.original_id.as_ref().and_then(ExactValue::text)
            != Some(mapping.original_message_id.as_str())
        || cards.session_id.as_ref().and_then(ExactValue::text)
            != Some(mapping.original_session_id.as_str())
        || !count_matches(cards.image_count, cards.images.len())
        || !count_matches(cards.call_count, cards.calls.len())
    {
        return Err(invalid());
    }
    for (n, image) in cards.images.iter().enumerate() {
        if image.index != n
            || !image
                .source_string_sha256
                .as_deref()
                .is_some_and(lower_hash)
            || image
                .source_string_bytes
                .is_none_or(|n| n > MAX_IMAGE_FIELD as u64)
        {
            return Err(invalid());
        }
        match &image.state {
            ImageState::Embedded {
                mime,
                decoded_bytes,
                decoded_sha256,
                width,
                height,
            } => {
                if image_filename(mime).is_none()
                    || *decoded_bytes == 0
                    || *decoded_bytes > MAX_IMAGE_BYTES as u64
                    || !lower_hash(decoded_sha256)
                    || *width == 0
                    || *height == 0
                    || *width > 8192
                    || *height > 8192
                    || u64::from(*width) * u64::from(*height) > 16_000_000
                {
                    return Err(invalid());
                }
            }
            ImageState::RemoteUnresolved { scheme } if scheme != "http" && scheme != "https" => {
                return Err(invalid())
            }
            _ => (),
        }
    }
    for (n, call) in cards.calls.iter().enumerate() {
        if call.index != n
            || call.id.as_deref().is_some_and(|s| !identity(s))
            || call.name.as_deref().is_some_and(|s| !identity(s))
            || !names_valid(&call.unknown_fields, 32, true)
        {
            return Err(invalid());
        }
        if let Some(p) = &call.arguments {
            if p.text.len() > MAX_ARGUMENT_PREVIEW
                || p.original_bytes > MAX_CALL_FIELD as u64
                || !lower_hash(&p.original_sha256)
                || p.original_bytes < p.text.len() as u64
                || p.truncated != (p.original_bytes > p.text.len() as u64)
                || (!p.truncated && digest(p.text.as_bytes()) != p.original_sha256)
            {
                return Err(invalid());
            }
        }
    }
    if !names_valid(&cards.unknown_columns, 512, false) {
        return Err(invalid());
    }
    match &cards.result_link {
        ResultLink::Unique { call_id, call }
            if !identity(call_id)
                || !identity(&call.original_message_id)
                || !call.message.valid()
                || call.message.import_id != cards.source.import_id
                || call.message == cards.source
                || call.call_index >= MAX_CALLS =>
        {
            return Err(invalid())
        }
        ResultLink::Ambiguous {
            call_id,
            candidates,
        } if !identity(call_id) || *candidates < 2 || *candidates > MAX_ANCESTORS * MAX_CALLS => {
            return Err(invalid())
        }
        ResultLink::Unmatched { call_id } if !identity(call_id) => return Err(invalid()),
        _ => (),
    }
    // A unique earlier-ancestor link is displayed as a literal receipt. It never
    // mints a review for that other message; navigation requires a fresh admitted
    // conversation mapping and selection.
    Ok(())
}
fn count_matches(count: Option<usize>, length: usize) -> bool {
    match count {
        Some(count) => count == length,
        None => length == 0,
    }
}
fn names_valid(names: &[String], limit: usize, allow_empty: bool) -> bool {
    let mut seen = BTreeSet::new();
    names.len() <= limit
        && names
            .iter()
            // JSON object keys may be empty; original SQL column names may not.
            .all(|s| (allow_empty || !s.is_empty()) && s.len() <= 256 && seen.insert(s))
}

// Shared DTOs remain authoritative. This local wire adapter only adds collection
// admission ceilings before allocation, then produces the shared MessageCards.
// Refuse the first excess item without decoding it or its nested values.
struct Bounded<T, const N: usize>(Vec<T>);
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for Bounded<T, N> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor<T, const N: usize>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> de::Visitor<'de> for Visitor<T, N> {
            type Value = Bounded<T, N>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded historical cards")
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while values.len() < N {
                    match seq.next_element()? {
                        Some(v) => values.push(v),
                        None => return Ok(Bounded(values)),
                    }
                }
                struct Reject;
                impl<'de> de::DeserializeSeed<'de> for Reject {
                    type Value = ();
                    fn deserialize<D: serde::Deserializer<'de>>(
                        self,
                        _: D,
                    ) -> std::result::Result<(), D::Error> {
                        Err(de::Error::custom("historical collection limit"))
                    }
                }
                seq.next_element_seed(Reject)?;
                Ok(Bounded(values))
            }
        }
        d.deserialize_seq(Visitor::<T, N>(std::marker::PhantomData))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCall {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<TextPreview>,
    arguments_valid_json: Option<bool>,
    unknown_fields: Bounded<String, 32>,
    issue: Option<Reason>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCards {
    source: MessageSource,
    original_id: Option<ExactValue>,
    session_id: Option<ExactValue>,
    role: Option<ExactValue>,
    original_status: Option<ExactValue>,
    original_seq: Option<ExactValue>,
    parent_id: Option<ExactValue>,
    tool_call_id: Option<ExactValue>,
    images: Bounded<ImageReference, MAX_IMAGES>,
    image_count: Option<usize>,
    calls: Bounded<WireCall, MAX_CALLS>,
    call_count: Option<usize>,
    result_link: ResultLink,
    issues: Bounded<RetainedIssue, 176>,
    unknown_columns: Bounded<String, 512>,
    runtime_activation: Dormant,
}
impl WireCards {
    fn into_cards(self) -> MessageCards {
        MessageCards {
            source: self.source,
            original_id: self.original_id,
            session_id: self.session_id,
            role: self.role,
            original_status: self.original_status,
            original_seq: self.original_seq,
            parent_id: self.parent_id,
            tool_call_id: self.tool_call_id,
            images: self.images.0,
            image_count: self.image_count,
            calls: self
                .calls
                .0
                .into_iter()
                .map(|c| ToolCallCard {
                    index: c.index,
                    id: c.id,
                    name: c.name,
                    arguments: c.arguments,
                    arguments_valid_json: c.arguments_valid_json,
                    unknown_fields: c.unknown_fields.0,
                    issue: c.issue,
                })
                .collect(),
            call_count: self.call_count,
            result_link: self.result_link,
            issues: self.issues.0,
            unknown_columns: self.unknown_columns.0,
            runtime_activation: self.runtime_activation,
        }
    }
}

#[cfg(test)]
mod visitor_tests {
    use super::*;
    #[test]
    fn bounded_visitor_refuses_excess_before_deserializing_it() {
        let err =
            serde_json::from_str::<Bounded<String, 2>>(r#"["one","two",{invalid nested data}]"#)
                .err()
                .expect("excess element must be rejected");
        assert!(err.to_string().contains("historical collection limit"));
    }
}
