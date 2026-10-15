//! Offline attachment validation and managed ingestion. No process execution or network I/O.
//!
//! Parsing is synchronous: call from a bounded blocking worker pool, not an async executor.
//! Inputs, decoded dimensions, animation frames, document structure and context are bounded.
//! The disk store is available on macOS/Linux; pure validation is platform independent.
#![forbid(unsafe_code)]

mod document;
mod office;
mod raster;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod store;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt};

pub use office::{
    export_table, read_spreadsheet, read_workbook, write_workbook, Cell, CellValue, ExportFormat,
    GeneratedDocument, NumberFormat, Sheet, SheetVisibility, Table, Workbook,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use store::MediaStore;

pub type Result<T> = std::result::Result<T, Error>;

/// Stable error category. Details never include file contents or user filesystem paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidRequest,
    UnsupportedFormat,
    MimeMismatch,
    InvalidContent,
    LimitExceeded,
    UnsafePath,
    Integrity,
    Io,
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    message: &'static str,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        error(ErrorKind::Io, "attachment I/O failed")
    }
}
fn error(kind: ErrorKind, message: &'static str) -> Error {
    Error { kind, message }
}

/// Defaults are also hard ceilings. Callers may reduce budgets, never disable them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_input_bytes: usize,
    pub max_text_bytes: usize,
    pub max_context_chars: usize,
    pub max_csv_rows: usize,
    pub max_csv_columns: usize,
    pub max_csv_field_bytes: usize,
    pub max_json_depth: usize,
    pub max_json_nodes: usize,
    pub max_image_dimension: u32,
    pub max_image_pixels: u64,
    pub max_decoded_bytes: u64,
    pub max_frames: usize,
    pub max_total_frame_pixels: u64,
    pub max_zip_entries: usize,
    pub max_zip_part_bytes: usize,
    pub max_zip_total_bytes: usize,
    pub max_xml_nodes: u32,
    pub max_xml_depth: usize,
    pub max_sheets: usize,
    pub max_cells: usize,
    pub max_shared_strings: usize,
    pub max_workbook_text_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 8 * 1024 * 1024,
            max_text_bytes: 2 * 1024 * 1024,
            max_context_chars: 20_000,
            max_csv_rows: 1_000,
            max_csv_columns: 128,
            max_csv_field_bytes: 64 * 1024,
            max_json_depth: 32,
            max_json_nodes: 50_000,
            max_image_dimension: 8_192,
            max_image_pixels: 16_000_000,
            max_decoded_bytes: 64 * 1024 * 1024,
            max_frames: 32,
            max_total_frame_pixels: 32_000_000,
            max_zip_entries: 128,
            max_zip_part_bytes: 4 * 1024 * 1024,
            max_zip_total_bytes: 16 * 1024 * 1024,
            max_xml_nodes: 150_000,
            max_xml_depth: 32,
            max_sheets: 16,
            max_cells: 20_000,
            max_shared_strings: 20_000,
            max_workbook_text_bytes: 2 * 1024 * 1024,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        let ceiling = Self::default();
        macro_rules! check {
            ($($field:ident),+) => {$(
                if self.$field == 0 || self.$field > ceiling.$field {
                    return Err(error(ErrorKind::InvalidRequest, "invalid resource limits"));
                }
            )+};
        }
        check!(
            max_input_bytes,
            max_text_bytes,
            max_context_chars,
            max_csv_rows,
            max_csv_columns,
            max_csv_field_bytes,
            max_json_depth,
            max_json_nodes,
            max_image_dimension,
            max_image_pixels,
            max_decoded_bytes,
            max_frames,
            max_total_frame_pixels,
            max_zip_entries,
            max_zip_part_bytes,
            max_zip_total_bytes,
            max_xml_nodes,
            max_xml_depth,
            max_sheets,
            max_cells,
            max_shared_strings,
            max_workbook_text_bytes
        );
        Ok(())
    }
}

/// Untrusted display metadata. Filename is a single basename, never a destination path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRequest {
    pub filename: String,
    pub declared_mime: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}
impl AttachmentRequest {
    pub fn new(filename: impl Into<String>, mime: Option<&str>) -> Self {
        Self {
            filename: filename.into(),
            declared_mime: mime.map(str::to_owned),
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Text,
    Markdown,
    Csv,
    Tsv,
    Json,
    Xlsx,
    Xlsm,
    Xls,
    Ods,
    Pdf,
    Pptx,
    Png,
    Jpeg,
    Gif,
    Bmp,
    #[serde(rename = "webp")]
    WebP,
}
impl Format {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Text => "text/plain",
            Self::Markdown => "text/markdown",
            Self::Csv => "text/csv",
            Self::Tsv => "text/tab-separated-values",
            Self::Json => "application/json",
            Self::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Self::Xlsm => "application/vnd.ms-excel.sheet.macroenabled.12",
            Self::Xls => "application/vnd.ms-excel",
            Self::Ods => "application/vnd.oasis.opendocument.spreadsheet",
            Self::Pdf => "application/pdf",
            Self::Pptx => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Bmp => "image/bmp",
            Self::WebP => "image/webp",
        }
    }
    fn image(self) -> bool {
        matches!(
            self,
            Self::Png | Self::Jpeg | Self::Gif | Self::Bmp | Self::WebP
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Content {
    Document {
        /// UTF-8 context. CSV/TSV use JSON arrays of string cells, retaining cell boundaries.
        context: String,
        /// True exactly when the context omitted Unicode scalar values from the full rendering.
        truncated: bool,
        total_context_chars: usize,
        /// CSV/TSV only; includes the first row (no implicit header assumption).
        rows: Option<usize>,
        columns: Option<usize>,
    },
    Image {
        width: u32,
        height: u32,
        frames: usize,
        /// Consumers must send retained bytes to a supported vision model or show this gap.
        requires_vision: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub sha256: String,
    pub byte_len: u64,
    pub format: Format,
    pub detected_mime: String,
    pub request: AttachmentRequest,
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAttachment {
    /// Server-generated UUID; pass this to `MediaStore::read`/`read_bytes`.
    pub id: String,
    pub manifest: Manifest,
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Validate a complete bounded input. This does not write files or fetch linked resources.
pub fn inspect(request: AttachmentRequest, bytes: &[u8], limits: &Limits) -> Result<Manifest> {
    limits.validate()?;
    let format = validate_request(&request)?;
    if bytes.len() > limits.max_input_bytes {
        return Err(error(
            ErrorKind::LimitExceeded,
            "attachment exceeds byte limit",
        ));
    }
    let content = if matches!(
        format,
        Format::Xlsx | Format::Xlsm | Format::Xls | Format::Ods | Format::Pdf | Format::Pptx
    ) {
        office::inspect(format, bytes, limits)?
    } else if format.image() {
        raster::inspect(format, bytes, limits)?
    } else {
        document::inspect(format, bytes, limits)?
    };
    Ok(Manifest {
        version: 1,
        sha256: sha256(bytes),
        byte_len: bytes.len() as u64,
        format,
        detected_mime: format.mime().to_owned(),
        request,
        content,
    })
}

fn forbidden_char(c: char) -> bool {
    (c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}
fn safe_label(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() || forbidden_char(c))
}

fn validate_request(request: &AttachmentRequest) -> Result<Format> {
    let name = &request.filename;
    if name.is_empty()
        || name.len() > 255
        || name.trim() != name
        || name.contains(['/', '\\', ':'])
        || !safe_label(name)
        || name == "."
        || name == ".."
        || name.ends_with('.')
    {
        return Err(error(
            ErrorKind::InvalidRequest,
            "filename must be a safe display basename",
        ));
    }
    let mut metadata_bytes = 0usize;
    if request.metadata.len() > 32 {
        return Err(error(ErrorKind::LimitExceeded, "too many metadata fields"));
    }
    for (key, value) in &request.metadata {
        metadata_bytes += key.len() + value.len();
        if key.is_empty()
            || key.len() > 64
            || value.len() > 2048
            || metadata_bytes > 8192
            || !safe_label(key)
            || !safe_label(value)
        {
            return Err(error(
                ErrorKind::InvalidRequest,
                "invalid attachment metadata",
            ));
        }
    }
    let mime = request
        .declared_mime
        .as_deref()
        .map(parse_mime)
        .transpose()?;
    let extension = name.rsplit_once('.').map(|(_, s)| s.to_ascii_lowercase());
    let format = match extension.as_deref() {
        Some("txt") => Format::Text,
        Some("md" | "markdown") => Format::Markdown,
        Some("csv") => Format::Csv,
        Some("tsv") => Format::Tsv,
        Some("json") => Format::Json,
        Some("xlsx") => Format::Xlsx,
        Some("xlsm") => Format::Xlsm,
        Some("xls") => Format::Xls,
        Some("ods") => Format::Ods,
        Some("pdf") => Format::Pdf,
        Some("pptx") => Format::Pptx,
        Some("png") => Format::Png,
        Some("jpg" | "jpeg") => Format::Jpeg,
        Some("gif") => Format::Gif,
        Some("bmp") => Format::Bmp,
        Some("webp") => Format::WebP,
        None => match mime.as_deref() {
            Some("text/plain") => Format::Text,
            Some("text/markdown") => Format::Markdown,
            Some("text/csv") => Format::Csv,
            Some("text/tab-separated-values") => Format::Tsv,
            Some("application/json") => Format::Json,
            _ => {
                return Err(error(
                    ErrorKind::UnsupportedFormat,
                    "extensionless input requires a supported document MIME",
                ))
            }
        },
        _ => {
            return Err(error(
                ErrorKind::UnsupportedFormat,
                "attachment format is not supported",
            ))
        }
    };
    if let Some(mime) = mime {
        if mime != format.mime() {
            return Err(error(
                ErrorKind::MimeMismatch,
                "declared MIME does not match filename format",
            ));
        }
    }
    Ok(format)
}

fn parse_mime(mime: &str) -> Result<String> {
    if mime.len() > 128 || !mime.is_ascii() || !safe_label(mime) {
        return Err(error(ErrorKind::InvalidRequest, "invalid declared MIME"));
    }
    let mut parts = mime.split(';');
    let base = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    for parameter in parts {
        let parameter = parameter.trim().to_ascii_lowercase();
        if parameter != "charset=utf-8" && parameter != "charset=\"utf-8\"" {
            return Err(error(
                ErrorKind::InvalidRequest,
                "only UTF-8 charset MIME parameters are supported",
            ));
        }
        if !base.starts_with("text/") && base != "application/json" {
            return Err(error(
                ErrorKind::InvalidRequest,
                "image MIME parameters are unsupported",
            ));
        }
    }
    Ok(base)
}
