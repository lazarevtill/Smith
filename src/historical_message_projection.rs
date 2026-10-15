//! Isolated read-only projection. No network, process, filesystem, or profile
//! mutation capability is used here. Call raster work in a bounded blocking pool.
#[path = "historical_message_contract.rs"]
pub mod contract;
use base64::Engine;
pub use contract::*;
use serde::{
    de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
    Deserialize,
};
use std::{cell::Cell, collections::BTreeSet, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
pub fn digest(bytes: &[u8]) -> String {
    smith_media::sha256(bytes)
}
fn malformed() -> Error {
    Error("Original message encoding is invalid; archive bytes remain retained.")
}
fn hex(s: &str) -> bool {
    s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn short_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
}

pub struct ArchivedMessage {
    source: MessageSource,
    original: ExactRow,
    payload_bytes: usize,
}
impl ArchivedMessage {
    pub fn parse(import_id: &str, row_number: i64, raw: &str) -> Result<Self> {
        if raw.len() > MAX_ARCHIVE_ROW {
            return Err(Error(
                "Original message exceeds the bounded view; archive bytes remain retained.",
            ));
        }
        let source = MessageSource {
            import_id: import_id.into(),
            source_schema: 25,
            table: "messages".into(),
            row_number,
            payload_sha256: digest(raw.as_bytes()),
        };
        if !source.valid() {
            return Err(malformed());
        }
        let bounded: BoundedRow = serde_json::from_str(raw).map_err(|_| malformed())?;
        let original = ExactRow {
            columns: bounded.columns,
            values: bounded.values,
        };
        if original.columns.len() != original.values.len() || original.columns.len() > 512 {
            return Err(malformed());
        }
        let mut names = BTreeSet::new();
        for (name, value) in original.columns.iter().zip(&original.values) {
            if name.is_empty() || name.len() > 256 || !names.insert(name) {
                return Err(malformed());
            }
            match value {
                ExactValue::Integer(v) if v.parse::<i64>().is_err() => return Err(malformed()),
                ExactValue::RealBits(v) if v.len() != 16 || !hex(v) => return Err(malformed()),
                ExactValue::Blob(v) | ExactValue::TextBytes(v) if !hex(v) => {
                    return Err(malformed())
                }
                _ => (),
            }
        }
        Ok(Self {
            source,
            original,
            payload_bytes: raw.len(),
        })
    }
    pub fn source(&self) -> &MessageSource {
        &self.source
    }
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }
    #[cfg(test)]
    pub fn original(&self) -> &ExactRow {
        &self.original
    }
    pub fn field(&self, name: &str) -> Option<&ExactValue> {
        self.original
            .columns
            .iter()
            .position(|v| v == name)
            .map(|n| &self.original.values[n])
    }
    pub fn text(&self, name: &str) -> Option<&str> {
        self.field(name).and_then(ExactValue::text)
    }
    pub fn id(&self) -> Option<&str> {
        self.text("id").filter(|s| short_id(s))
    }
    pub fn session(&self) -> Option<&str> {
        self.text("session_id").filter(|s| short_id(s))
    }
    pub fn sequence(&self) -> Option<i64> {
        match self.field("seq") {
            Some(ExactValue::Integer(v)) => v.parse().ok(),
            _ => None,
        }
    }
    pub fn parent(&self) -> std::result::Result<Option<&str>, Reason> {
        match self.field("parent_id") {
            Some(ExactValue::Null) => Ok(None),
            Some(ExactValue::Text(v)) if short_id(v) => Ok(Some(v)),
            _ => Err(Reason::InvalidIdentity),
        }
    }
    fn metadata(&self, name: &str) -> Result<Option<ExactValue>> {
        if let Some(v) = self.field(name) {
            if matches!(v, ExactValue::Text(s)|ExactValue::TextBytes(s)|ExactValue::Blob(s) if s.len()>4096)
                || serde_json::to_vec(v).map_err(|_| malformed())?.len() > 4096
            {
                return Err(Error("Original message metadata exceeds its display bound; exact bytes remain retained."));
            }
        }
        Ok(self.field(name).cloned())
    }
    pub fn cards(&self, ancestry: &Ancestry) -> Result<MessageCards> {
        let mut issues = Vec::new();
        let images = self.images(&mut issues);
        let calls = parse_calls(self.field("tool_calls"));
        issues.extend(calls.issues);
        let known = [
            "rowid",
            "id",
            "session_id",
            "seq",
            "role",
            "content",
            "reasoning",
            "tool_calls",
            "tool_call_id",
            "images",
            "created_at",
            "parent_id",
            "status",
        ];
        let cards = MessageCards {
            source: self.source.clone(),
            original_id: self.metadata("id")?,
            session_id: self.metadata("session_id")?,
            role: self.metadata("role")?,
            original_status: self.metadata("status")?,
            original_seq: self.metadata("seq")?,
            parent_id: self.metadata("parent_id")?,
            tool_call_id: self.metadata("tool_call_id")?,
            image_count: images.as_ref().map(Vec::len),
            images: images.unwrap_or_default(),
            calls: calls.cards,
            call_count: calls.count,
            result_link: resolve_result(self, ancestry),
            issues,
            unknown_columns: self
                .original
                .columns
                .iter()
                .filter(|n| !known.contains(&n.as_str()))
                .cloned()
                .collect(),
            runtime_activation: Dormant::Dormant,
        };
        encode_cards(&cards)?;
        Ok(cards)
    }
    fn image_strings(&self) -> std::result::Result<Vec<String>, Reason> {
        let raw = field_text(self.field("images"), MAX_IMAGE_FIELD)?;
        bounded_array(raw, MAX_IMAGES)
    }
    fn images(&self, issues: &mut Vec<RetainedIssue>) -> Option<Vec<ImageReference>> {
        let strings = match self.image_strings() {
            Ok(v) => v,
            Err(reason) => {
                issues.push(issue(
                    "images",
                    None,
                    reason,
                    self.text("images").map(str::len),
                ));
                return None;
            }
        };
        Some(
            strings
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let state = match classify_image(value) {
                        Ok(ImageClassification::Remote(scheme)) => {
                            ImageState::RemoteUnresolved { scheme }
                        }
                        Ok(ImageClassification::Embedded(v)) => ImageState::Embedded {
                            mime: v.mime,
                            decoded_bytes: v.bytes.len() as u64,
                            decoded_sha256: v.sha256,
                            width: v.width,
                            height: v.height,
                        },
                        Err(reason) => {
                            issues.push(issue("images", Some(index), reason, Some(value.len())));
                            ImageState::Retained { reason }
                        }
                    };
                    ImageReference {
                        index,
                        source_string_bytes: Some(value.len() as u64),
                        source_string_sha256: Some(digest(value.as_bytes())),
                        state,
                        fetched: NotFetched,
                    }
                })
                .collect(),
        )
    }
    pub fn embedded_image(&self, expected: &MessageSource, index: usize) -> Result<ValidatedImage> {
        if self.source != *expected {
            return Err(Error(
                "Original image source changed; refresh before opening it.",
            ));
        }
        let values = self
            .image_strings()
            .map_err(|_| Error("Original image list is retained but cannot be previewed."))?;
        let value = values
            .get(index)
            .ok_or(Error("Original image index is absent."))?;
        match classify_image(value) {
            Ok(ImageClassification::Embedded(image)) => Ok(image),
            _ => Err(Error("This original image is an unresolved or unsupported reference; no resource was opened.")),
        }
    }
}
/// Encode into a hard-capped writer; escaping cannot allocate an oversized response.
pub fn encode_cards(cards: &MessageCards) -> Result<Vec<u8>> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_CARD_BYTES.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("historical card bound"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded(Vec::new());
    serde_json::to_writer(&mut writer, cards).map_err(|_| {
        Error(
            "Original message cards exceed the display limit; exact archive bytes remain retained.",
        )
    })?;
    Ok(writer.0)
}
fn issue(field: &str, index: Option<usize>, reason: Reason, bytes: Option<usize>) -> RetainedIssue {
    RetainedIssue {
        field: field.into(),
        index,
        reason,
        retained_bytes: bytes.map(|n| n as u64),
    }
}
fn field_text(value: Option<&ExactValue>, limit: usize) -> std::result::Result<&str, Reason> {
    match value {
        Some(ExactValue::Text(v)) if v.len() <= limit => Ok(v),
        Some(ExactValue::Text(_)) => Err(Reason::OversizedField),
        Some(_) => Err(Reason::InvalidFieldType),
        None => Err(Reason::MissingField),
    }
}
/// The seed is invoked only when an excess item exists. It refuses before the
/// item is deserialized, including huge nested values and strings.
struct RejectExtra<'a>(Option<&'a Cell<bool>>);
impl<'de> DeserializeSeed<'de> for RejectExtra<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> std::result::Result<(), D::Error> {
        if let Some(overflow) = self.0 {
            overflow.set(true);
        }
        Err(de::Error::custom("historical collection entry bound"))
    }
}
struct Array<'a, T> {
    limit: usize,
    overflow: Option<&'a Cell<bool>>,
    marker: std::marker::PhantomData<T>,
}
impl<'de, T: Deserialize<'de>> Visitor<'de> for Array<'_, T> {
    type Value = Vec<T>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded original array")
    }
    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut result = Vec::new();
        while result.len() < self.limit {
            match seq.next_element()? {
                Some(v) => result.push(v),
                None => return Ok(result),
            }
        }
        seq.next_element_seed(RejectExtra(self.overflow))?;
        Ok(result)
    }
}
fn bounded_items<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
    limit: usize,
    overflow: Option<&Cell<bool>>,
) -> std::result::Result<Vec<T>, D::Error> {
    deserializer.deserialize_seq(Array {
        limit,
        overflow,
        marker: std::marker::PhantomData,
    })
}
fn bounded_array<'de, T: Deserialize<'de>>(
    raw: &'de str,
    limit: usize,
) -> std::result::Result<Vec<T>, Reason> {
    let overflow = Cell::new(false);
    let mut de = serde_json::Deserializer::from_str(raw);
    let result = bounded_items(&mut de, limit, Some(&overflow));
    match result {
        Ok(v) if de.end().is_ok() => Ok(v),
        _ if overflow.get() => Err(Reason::TooManyEntries),
        _ => Err(Reason::InvalidJson),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundedRow {
    #[serde(deserialize_with = "row_columns")]
    columns: Vec<String>,
    #[serde(deserialize_with = "row_values")]
    values: Vec<ExactValue>,
}
fn row_columns<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<String>, D::Error> {
    bounded_items(d, 512, None)
}
fn row_values<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<ExactValue>, D::Error> {
    bounded_items(d, 512, None)
}
pub struct ValidatedImage {
    pub mime: String,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}
enum ImageClassification {
    Remote(String),
    Embedded(ValidatedImage),
}
fn classify_image(value: &str) -> std::result::Result<ImageClassification, Reason> {
    let scheme = value
        .split_once(':')
        .map(|v| v.0.to_ascii_lowercase())
        .ok_or(Reason::UnsupportedScheme)?;
    if scheme == "http" || scheme == "https" {
        let parsed = url::Url::parse(value).map_err(|_| Reason::InvalidEntry)?;
        if parsed.host_str().is_none() {
            return Err(Reason::InvalidEntry);
        }
        return Ok(ImageClassification::Remote(scheme));
    }
    if scheme != "data" {
        return Err(Reason::UnsupportedScheme);
    }
    let (head, encoded) = value
        .split_once(',')
        .ok_or(Reason::UnsupportedDataEncoding)?;
    let (mime, filename) = match head {
        "data:image/png;base64" => ("image/png", "original.png"),
        "data:image/jpeg;base64" => ("image/jpeg", "original.jpg"),
        "data:image/webp;base64" => ("image/webp", "original.webp"),
        _ => return Err(Reason::UnsupportedDataEncoding),
    };
    if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(Reason::ImageTooLarge);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Reason::InvalidBase64)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(Reason::ImageTooLarge);
    }
    let manifest = smith_media::inspect(
        smith_media::AttachmentRequest::new(filename, Some(mime)),
        &bytes,
        &smith_media::Limits::default(),
    )
    .map_err(|e| match e.kind {
        smith_media::ErrorKind::LimitExceeded => Reason::ImageTooLarge,
        smith_media::ErrorKind::MimeMismatch => Reason::MimeMismatch,
        smith_media::ErrorKind::UnsupportedFormat => Reason::UnsupportedImageFormat,
        _ => Reason::InvalidImage,
    })?;
    let smith_media::Content::Image {
        width,
        height,
        frames: 1,
        ..
    } = manifest.content
    else {
        return Err(Reason::UnsupportedImageFormat);
    };
    Ok(ImageClassification::Embedded(ValidatedImage {
        mime: mime.into(),
        sha256: manifest.sha256,
        width,
        height,
        bytes,
    }))
}

// Only literal strings need materialization. Unknown nested values stay in the
// exact archive and are skipped, not expanded into a temporary Value tree.
enum CallValue {
    Text(String),
    Other,
}
impl CallValue {
    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            Self::Other => None,
        }
    }
}
struct CallVisitor;
impl<'de> Visitor<'de> for CallVisitor {
    type Value = CallValue;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("literal original call value")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
        Ok(CallValue::Text(v.into()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
        Ok(CallValue::Text(v))
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
        Ok(CallValue::Other)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
        Ok(CallValue::Other)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
        Ok(CallValue::Other)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
        Ok(CallValue::Other)
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
        Ok(CallValue::Other)
    }
    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(CallValue::Other)
    }
    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(CallValue::Other)
    }
}
impl<'de> Deserialize<'de> for CallValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        d.deserialize_any(CallVisitor)
    }
}
struct Pairs(Vec<(String, CallValue)>);
enum CallEntry {
    Object(Pairs),
    Other,
}
impl<'de> Deserialize<'de> for CallEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Entry;
        impl<'de> Visitor<'de> for Entry {
            type Value = CallEntry;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded original tool call")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut pairs = Vec::new();
                while pairs.len() < 32 {
                    match map.next_entry()? {
                        Some(pair) => pairs.push(pair),
                        None => return Ok(CallEntry::Object(Pairs(pairs))),
                    }
                }
                map.next_key_seed(RejectExtra(None))?;
                Ok(CallEntry::Object(Pairs(pairs)))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(CallEntry::Other)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(CallEntry::Other)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(CallEntry::Other)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(CallEntry::Other)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(CallEntry::Other)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(CallEntry::Other)
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(CallEntry::Other)
            }
        }
        d.deserialize_any(Entry)
    }
}
struct Calls {
    cards: Vec<ToolCallCard>,
    count: Option<usize>,
    issues: Vec<RetainedIssue>,
    complete: bool,
}
fn preview(s: &str) -> TextPreview {
    let mut n = s.len().min(MAX_ARGUMENT_PREVIEW);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    TextPreview {
        text: s[..n].into(),
        original_bytes: s.len() as u64,
        original_sha256: digest(s.as_bytes()),
        truncated: n < s.len(),
    }
}
fn parse_calls(field: Option<&ExactValue>) -> Calls {
    let input =
        field_text(field, MAX_CALL_FIELD).and_then(|s| bounded_array::<CallEntry>(s, MAX_CALLS));
    let entries = match input {
        Ok(v) => v,
        Err(reason) => {
            return Calls {
                cards: vec![],
                count: None,
                issues: vec![issue(
                    "tool_calls",
                    None,
                    reason,
                    field.and_then(ExactValue::text).map(str::len),
                )],
                complete: false,
            }
        }
    };
    let mut result = Calls {
        cards: vec![],
        count: Some(entries.len()),
        issues: vec![],
        complete: true,
    };
    for (index, entry) in entries.into_iter().enumerate() {
        let mut card = ToolCallCard {
            index,
            id: None,
            name: None,
            arguments: None,
            arguments_valid_json: None,
            unknown_fields: vec![],
            issue: None,
        };
        match entry {
            CallEntry::Object(Pairs(pairs)) => {
                let mut names = BTreeSet::new();
                if pairs.len() > 32 || pairs.iter().any(|(n, _)| n.len() > 256) {
                    card.issue = Some(Reason::InvalidEntry);
                } else if pairs.iter().any(|(n, _)| !names.insert(n)) {
                    card.issue = Some(Reason::DuplicateCallField);
                } else {
                    let text = |name: &str| {
                        pairs
                            .iter()
                            .find(|(n, _)| n == name)
                            .and_then(|(_, v)| v.text())
                    };
                    card.id = text("id").filter(|s| short_id(s)).map(str::to_owned);
                    card.name = text("name").filter(|s| short_id(s)).map(str::to_owned);
                    card.arguments = text("arguments").map(preview);
                    card.arguments_valid_json =
                        text("arguments").map(|s| serde_json::from_str::<IgnoredAny>(s).is_ok());
                    card.unknown_fields = pairs
                        .iter()
                        .filter(|(n, _)| !["id", "name", "arguments"].contains(&n.as_str()))
                        .map(|(n, _)| n.clone())
                        .collect();
                    if card.id.is_none() || card.name.is_none() || card.arguments.is_none() {
                        card.issue = Some(Reason::InvalidEntry);
                    }
                }
            }
            CallEntry::Other => {
                card.issue = Some(Reason::InvalidEntry);
            }
        }
        if let Some(reason) = card.issue {
            result.complete = false;
            result
                .issues
                .push(issue("tool_calls", Some(index), reason, None));
        }
        result.cards.push(card);
    }
    result
}

/// Ordered immediate-parent-first chain. `end` must explicitly describe whether
/// traversal reached SQL NULL; an incomplete chain never earns a unique link.
pub struct Ancestry {
    pub nodes: Vec<ArchivedMessage>,
    pub end: std::result::Result<(), Reason>,
}
pub fn resolve_result(message: &ArchivedMessage, ancestry: &Ancestry) -> ResultLink {
    if message.text("role") != Some("tool") {
        return ResultLink::NotToolResult;
    }
    let Some(call_id) = message.text("tool_call_id").filter(|s| short_id(s)) else {
        return ResultLink::MissingId;
    };
    let unresolved = |reason| ResultLink::Unresolved { reason };
    let (Some(id), Some(session), Some(mut sequence)) =
        (message.id(), message.session(), message.sequence())
    else {
        return unresolved(Reason::InvalidIdentity);
    };
    let mut parent = match message.parent() {
        Ok(v) => v,
        Err(r) => return unresolved(r),
    };
    let mut seen = BTreeSet::from([id]);
    let mut bytes = 0usize;
    let mut matches = Vec::new();
    if ancestry.nodes.len() > MAX_ANCESTORS {
        return unresolved(Reason::AncestryLimit);
    }
    for node in &ancestry.nodes {
        bytes = match bytes.checked_add(node.payload_bytes) {
            Some(n) if n <= MAX_ANCESTOR_BYTES => n,
            _ => return unresolved(Reason::AncestryLimit),
        };
        let (Some(id), Some(seq)) = (node.id(), node.sequence()) else {
            return unresolved(Reason::InvalidIdentity);
        };
        if parent != Some(id) || node.source.import_id != message.source.import_id {
            return unresolved(Reason::InvalidIdentity);
        }
        if !seen.insert(id) {
            return unresolved(Reason::Cycle);
        }
        if node.session() != Some(session) {
            return unresolved(Reason::CrossSessionAncestor);
        }
        if seq >= sequence {
            return unresolved(Reason::NonEarlierAncestor);
        }
        sequence = seq;
        let calls = parse_calls(node.field("tool_calls"));
        if !calls.complete {
            return unresolved(Reason::IncompleteCalls);
        }
        if node.text("role") == Some("assistant") {
            for call in calls
                .cards
                .iter()
                .filter(|c| c.id.as_deref() == Some(call_id))
            {
                matches.push(CallLocation {
                    message: node.source.clone(),
                    original_message_id: id.into(),
                    call_index: call.index,
                });
            }
        } else if !calls.cards.is_empty() {
            return unresolved(Reason::IncompleteCalls);
        }
        parent = match node.parent() {
            Ok(v) => v,
            Err(r) => return unresolved(r),
        };
    }
    if let Err(r) = ancestry.end {
        return unresolved(r);
    }
    if parent.is_some() {
        return unresolved(Reason::MissingAncestor);
    }
    match matches.len() {
        0 => ResultLink::Unmatched {
            call_id: call_id.into(),
        },
        1 => ResultLink::Unique {
            call_id: call_id.into(),
            call: matches.remove(0),
        },
        n => ResultLink::Ambiguous {
            call_id: call_id.into(),
            candidates: n,
        },
    }
}

#[cfg(test)]
#[path = "../tests/support/historical_message_projection.rs"]
mod tests;
