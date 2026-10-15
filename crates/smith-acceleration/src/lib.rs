//! Pure local planning. Inputs describe host-qualified facts; this crate does
//! not discover/install adapters, authorize execution, or claim residency.
mod planner;
mod types;
mod validate;

pub use planner::{plan, preview, revalidate};
pub use types::*;
pub use validate::ValidationError;

use serde::Serialize;
use sha2::{Digest, Sha256};

fn hash(value: &impl Serialize) -> Result<String, ValidationError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ValidationError::new("serialization"))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
