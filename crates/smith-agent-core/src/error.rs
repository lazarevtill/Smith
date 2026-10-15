use serde::{Deserialize, Serialize};
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Error {
    InvalidInput(String),
    Denied(String),
    Unsupported(String),
    InvalidTransition(String),
    Conflict,
    NotFound,
    LimitExceeded(String),
    UnsafePath(String),
    Io(String),
    UncertainSideEffect(String),
    Persistence(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}
#[cfg(feature = "standalone-sqlite")]
impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::Persistence(value.to_string())
    }
}

pub(crate) fn bounded_text(value: &str, maximum: usize, label: &str) -> Result<()> {
    if value.len() > maximum {
        Err(Error::LimitExceeded(label.into()))
    } else {
        Ok(())
    }
}
