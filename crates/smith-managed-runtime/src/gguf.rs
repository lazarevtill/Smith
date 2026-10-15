//! Read-only, bounded GGUF metadata inspection. Tensor data is never read here.
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

const MAX_METADATA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_VALUES: u64 = 1_000_000;
const MAX_KEYS: u64 = 8192;
const MAX_STRING: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GgufFailure {
    Io,
    InvalidHeader,
    UnsupportedVersion,
    InvalidMetadata,
    LimitExceeded,
    SplitModelUnsupported,
    FileChanged,
}
impl std::fmt::Display for GgufFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for GgufFailure {}
type Result<T> = std::result::Result<T, GgufFailure>;

#[derive(Clone, Debug, Serialize)]
pub struct KvDimensions {
    pub layers: u32,
    pub heads: u32,
    pub key_head_length: u32,
    pub value_head_length: u32,
    /// Exact arithmetic for a conventional full attention KV cache, before
    /// runtime padding/allocator overhead. Not an allocated-memory measurement.
    pub elements_per_token: u64,
    pub evidence: &'static str,
}
#[derive(Clone, Debug, Serialize)]
pub struct GgufMetadata {
    pub version: u32,
    pub file_bytes: u64,
    pub tensor_count: u64,
    pub metadata_entries: u64,
    pub inspected_bytes: u64,
    pub architecture: Option<String>,
    pub name: Option<String>,
    pub size_label: Option<String>,
    pub context_tokens: Option<u32>,
    pub layers: Option<u32>,
    pub embedding_length: Option<u32>,
    pub attention_heads: Option<u32>,
    pub kv_heads: Option<u32>,
    pub key_head_length: Option<u32>,
    pub value_head_length: Option<u32>,
    pub kv: Option<KvDimensions>,
    pub warnings: Vec<&'static str>,
}

#[derive(Clone)]
enum Value {
    Number(u64),
    Text(String),
    Other,
}
struct Parser<R> {
    input: R,
    position: u64,
    file_len: u64,
    values: u64,
}
impl<R: Read> Parser<R> {
    fn bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        let end = self
            .position
            .checked_add(n as u64)
            .ok_or(GgufFailure::LimitExceeded)?;
        if end > MAX_METADATA_BYTES {
            return Err(GgufFailure::LimitExceeded);
        }
        if end > self.file_len {
            return Err(GgufFailure::InvalidMetadata);
        }
        let mut bytes = vec![0; n];
        self.input
            .read_exact(&mut bytes)
            .map_err(|_| GgufFailure::InvalidMetadata)?;
        self.position = end;
        Ok(bytes)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn string(&mut self, max: u64) -> Result<String> {
        let n = self.u64()?;
        if n > max {
            return Err(GgufFailure::LimitExceeded);
        }
        String::from_utf8(self.bytes(n as usize)?).map_err(|_| GgufFailure::InvalidMetadata)
    }
    fn value(&mut self, kind: u32, depth: u8) -> Result<Value> {
        self.values += 1;
        if self.values > MAX_VALUES || depth > 4 {
            return Err(GgufFailure::LimitExceeded);
        }
        Ok(match kind {
            0 => Value::Number(self.bytes(1)?[0] as u64),
            1 => signed(self.bytes(1)?[0] as i8 as i64),
            2 => Value::Number(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()) as u64),
            3 => signed(i16::from_le_bytes(self.bytes(2)?.try_into().unwrap()) as i64),
            4 => Value::Number(self.u32()? as u64),
            5 => signed(self.u32()? as i32 as i64),
            6 => {
                self.bytes(4)?;
                Value::Other
            }
            7 => {
                if self.bytes(1)?[0] > 1 {
                    return Err(GgufFailure::InvalidMetadata);
                }
                Value::Other
            }
            8 => Value::Text(self.string(MAX_STRING)?),
            9 => {
                let element = self.u32()?;
                if element > 12 {
                    return Err(GgufFailure::InvalidMetadata);
                }
                let count = self.u64()?;
                if count > MAX_VALUES - self.values {
                    return Err(GgufFailure::LimitExceeded);
                }
                for _ in 0..count {
                    self.value(element, depth + 1)?;
                }
                Value::Other
            }
            10 => Value::Number(self.u64()?),
            11 => signed(self.u64()? as i64),
            12 => {
                self.bytes(8)?;
                Value::Other
            }
            _ => return Err(GgufFailure::InvalidMetadata),
        })
    }
}
fn signed(n: i64) -> Value {
    if n >= 0 {
        Value::Number(n as u64)
    } else {
        Value::Other
    }
}
fn number(values: &BTreeMap<String, Value>, key: &str) -> Option<u64> {
    match values.get(key) {
        Some(Value::Number(n)) => Some(*n),
        _ => None,
    }
}
fn text(values: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    match values.get(key) {
        Some(Value::Text(s)) if s.len() <= 512 && !s.chars().any(char::is_control) => {
            Some(s.clone())
        }
        _ => None,
    }
}

/// Parses only the header and metadata. Does not verify tensor layout, tensor
/// contents, model support, or inference. Limits are intentionally below the
/// format's theoretical maxima. Only little-endian GGUF v2/v3 are accepted.
pub fn inspect_gguf_reader(input: impl Read, file_bytes: u64) -> Result<GgufMetadata> {
    let mut parser = Parser {
        input,
        file_len: file_bytes,
        position: 0,
        values: 0,
    };
    if parser.bytes(4)? != b"GGUF" {
        return Err(GgufFailure::InvalidHeader);
    }
    let version = parser.u32()?;
    if !matches!(version, 2 | 3) {
        return Err(GgufFailure::UnsupportedVersion);
    }
    let tensor_count = parser.u64()?;
    let metadata_entries = parser.u64()?;
    if tensor_count > 1_000_000 || metadata_entries > MAX_KEYS {
        return Err(GgufFailure::LimitExceeded);
    }
    let mut values = BTreeMap::new();
    let mut keys = BTreeSet::new();
    for _ in 0..metadata_entries {
        let key = parser.string(1024)?;
        if key.is_empty()
            || key.split('.').any(|s| {
                s.is_empty()
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            || !keys.insert(key.clone())
        {
            return Err(GgufFailure::InvalidMetadata);
        }
        let kind = parser.u32()?;
        let value = parser.value(kind, 0)?;
        // Retain only small planning fields, never tokenizer/chat template data.
        if key.starts_with("split.")
            || key.starts_with("general.")
            || key.ends_with(".context_length")
            || key.ends_with(".block_count")
            || key.ends_with(".embedding_length")
            || key.contains(".attention.")
        {
            if let Value::Text(s) = &value {
                if s.len() > 512 {
                    continue;
                }
            }
            values.insert(key, value);
        }
    }
    for key in ["split.count", "split.no", "split.tensors.count"] {
        if keys.contains(key) && number(&values, key).is_none() {
            return Err(GgufFailure::InvalidMetadata);
        }
    }
    if number(&values, "split.count").is_some_and(|n| n != 1)
        || number(&values, "split.no").is_some_and(|n| n != 0)
        || number(&values, "split.tensors.count").is_some_and(|n| n != tensor_count)
    {
        return Err(GgufFailure::SplitModelUnsupported);
    }
    let architecture = text(&values, "general.architecture").filter(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    });
    let dim = |suffix: &str| {
        architecture
            .as_ref()
            .and_then(|a| number(&values, &format!("{a}.{suffix}")))
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let context_tokens = dim("context_length");
    let layers = dim("block_count");
    let embedding_length = dim("embedding_length");
    let attention_heads = dim("attention.head_count");
    let kv_heads = dim("attention.head_count_kv");
    let key_head_length = dim("attention.key_length");
    let value_head_length = dim("attention.value_length");
    // Restrict fallback arithmetic to conventional full-attention architectures.
    // Hybrid/recurrent/sliding-window/per-layer array layouts remain unknown.
    let scalar_or_absent = |suffix: &str| {
        architecture.as_ref().is_some_and(|a| {
            let key = format!("{a}.{suffix}");
            !keys.contains(&key) || number(&values, &key).is_some_and(|n| n > 0)
        })
    };
    let supported = matches!(architecture.as_deref(), Some("llama" | "qwen2" | "qwen3"))
        && [
            "attention.head_count_kv",
            "attention.key_length",
            "attention.value_length",
        ]
        .into_iter()
        .all(scalar_or_absent)
        && architecture.as_ref().is_some_and(|a| {
            let key = format!("{a}.attention.sliding_window");
            !keys.contains(&key) || number(&values, &key) == Some(0)
        });
    let kv = if supported {
        (|| {
            let layers = layers?;
            let heads = kv_heads.or(attention_heads)?;
            let default_length = embedding_length
                .zip(attention_heads)
                .filter(|(e, h)| e % h == 0)
                .map(|(e, h)| e / h);
            let key_head_length = key_head_length.or(default_length)?;
            let value_head_length = value_head_length.or(default_length)?;
            if let Some(all_heads) = attention_heads {
                if heads > all_heads || all_heads % heads != 0 {
                    return None;
                }
            }
            let elements_per_token = u64::from(layers)
                .checked_mul(u64::from(heads))?
                .checked_mul(u64::from(key_head_length) + u64::from(value_head_length))?;
            Some(KvDimensions {
                layers,
                heads,
                key_head_length,
                value_head_length,
                elements_per_token,
                evidence: "metadata_scalar_full_attention_estimate",
            })
        })()
    } else {
        None
    };
    let mut warnings = vec![
        "Tensor layout and model compatibility have not been validated.",
        "Metadata describes the file and is not a successful load or memory measurement.",
    ];
    if kv.is_none() {
        warnings.push("KV cache dimensions are unknown for this metadata/layout.");
    }
    Ok(GgufMetadata {
        version,
        file_bytes,
        tensor_count,
        metadata_entries,
        inspected_bytes: parser.position,
        architecture,
        name: text(&values, "general.name"),
        size_label: text(&values, "general.size_label"),
        context_tokens,
        layers,
        embedding_length,
        attention_heads,
        kv_heads,
        key_head_length,
        value_head_length,
        kv,
        warnings,
    })
}

/// Inspects exactly one selected local regular file; no adjacent shard discovery.
pub fn inspect_gguf(path: &Path) -> Result<GgufMetadata> {
    if !path.is_absolute() {
        return Err(GgufFailure::Io);
    }
    let path = std::fs::canonicalize(path).map_err(|_| GgufFailure::Io)?;
    if path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(shard_name)
    {
        return Err(GgufFailure::SplitModelUnsupported);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(&path).map_err(|_| GgufFailure::Io)?;
    let before = file.metadata().map_err(|_| GgufFailure::Io)?;
    if !before.is_file() {
        return Err(GgufFailure::InvalidHeader);
    }
    let result = inspect_gguf_reader(&mut file, before.len())?;
    let after = file.metadata().map_err(|_| GgufFailure::Io)?;
    let current = std::fs::metadata(path).map_err(|_| GgufFailure::FileChanged)?;
    if !same_file(&before, &after) || !same_file(&before, &current) {
        return Err(GgufFailure::FileChanged);
    }
    Ok(result)
}
fn shard_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".gguf") else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() >= 15 && {
        let tail = &bytes[bytes.len() - 15..];
        tail[0] == b'-'
            && tail[1..6].iter().all(u8::is_ascii_digit)
            && &tail[6..10] == b"-of-"
            && tail[10..].iter().all(u8::is_ascii_digit)
    }
}
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() != b.dev()
            || a.ino() != b.ino()
            || a.ctime() != b.ctime()
            || a.ctime_nsec() != b.ctime_nsec()
        {
            return false;
        }
    }
    a.is_file() && b.is_file() && a.len() == b.len() && a.modified().ok() == b.modified().ok()
}
