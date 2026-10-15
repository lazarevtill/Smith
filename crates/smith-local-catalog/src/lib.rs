//! Passive catalog records are not execution permissions. Browse never touches
//! selected files. Explicit capture/prepare hashes only the supplied paths; the
//! host must still use its native adapter's consent and final load revalidation.
mod catalog;
mod files;
mod types;
mod validate;

pub use catalog::{Catalog, PreparedSelection, SelectionReview};
pub use files::{capture, ReadControl};
pub use types::*;

use serde::Serialize;
use sha2::{Digest, Sha256};

pub const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 128;
pub const MAX_FILES: usize = 128;
pub const MAX_SELECTED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub const MAX_REVISION: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidRecord,
    UnsupportedVersion,
    LimitExceeded,
    Conflict,
    MissingEntry,
    EntryTrashed,
    WrongSelection,
    StaleReview,
    InvalidSelection,
    UnsafeFile,
    FileChanged,
    Io,
    Cancelled,
    Deadline,
    UnsupportedPlatform,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidRecord => "The saved catalog is invalid; it was not reset.",
            Self::UnsupportedVersion => "The catalog format is unsupported.",
            Self::LimitExceeded => "The catalog or selected files exceed a bounded limit.",
            Self::Conflict => "The catalog changed; reload before saving.",
            Self::MissingEntry => "The selected catalog entry does not exist.",
            Self::EntryTrashed => "Restore this catalog entry before selecting or preparing it.",
            Self::WrongSelection => "This entry is not selected for review.",
            Self::StaleReview => "The selection or current context changed; review again.",
            Self::InvalidSelection => "The explicit file selection is invalid.",
            Self::UnsafeFile => "A selected path is not a permitted regular file.",
            Self::FileChanged => "A selected file changed; no replacement was selected.",
            Self::Io => "A selected file could not be read.",
            Self::Cancelled => "File validation was cancelled.",
            Self::Deadline => "File validation exceeded its deadline.",
            Self::UnsupportedPlatform => "Secure file validation is unavailable on this platform.",
        })
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(value: &impl Serialize) -> Result<String> {
    Ok(hex(&Sha256::digest(
        serde_json::to_vec(value).map_err(|_| Error::InvalidRecord)?,
    )))
}
