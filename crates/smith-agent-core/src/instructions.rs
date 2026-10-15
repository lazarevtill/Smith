//! Captured extension text is immutable task data, never execution authority.
use crate::{arguments_sha256, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionKind {
    Skill,
    Method,
    Plugin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionRef {
    pub id: String,
    pub revision: u64,
}
impl InstructionRef {
    pub fn validate(&self) -> Result<()> {
        if !uuid::Uuid::parse_str(&self.id).is_ok_and(|id| id.to_string() == self.id)
            || self.revision == 0
            || self.revision >= i64::MAX as u64
        {
            return Err(Error::InvalidInput("extension identity or revision".into()));
        }
        Ok(())
    }
}

/// The host resolves this preference when creating a task. It never resolves
/// it again for an existing task, even after a source definition changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionSelection {
    #[default]
    Enabled,
    Explicit {
        references: Vec<InstructionRef>,
    },
}
impl InstructionSelection {
    pub fn validate(&self) -> Result<()> {
        if let Self::Explicit { references } = self {
            if references.len() > 12 {
                return Err(Error::LimitExceeded("selected extensions (12)".into()));
            }
            let mut ids = BTreeSet::new();
            for reference in references {
                reference.validate()?;
                if !ids.insert(&reference.id) {
                    return Err(Error::InvalidInput("duplicate selected extension".into()));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedInstruction {
    pub id: String,
    pub revision: u64,
    pub kind: InstructionKind,
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub content_sha256: String,
}
impl CapturedInstruction {
    pub fn new(
        reference: InstructionRef,
        kind: InstructionKind,
        name: String,
        description: String,
        instructions: String,
    ) -> Result<Self> {
        let mut value = Self {
            id: reference.id,
            revision: reference.revision,
            kind,
            name,
            description,
            instructions,
            content_sha256: String::new(),
        };
        value.content_sha256 = value.digest();
        value.validate()?;
        Ok(value)
    }
    fn digest(&self) -> String {
        arguments_sha256(
            &json!({"id":self.id,"revision":self.revision,"kind":self.kind,"name":self.name,"description":self.description,"instructions":self.instructions}),
        )
    }
    fn validate(&self) -> Result<()> {
        InstructionRef {
            id: self.id.clone(),
            revision: self.revision,
        }
        .validate()?;
        if self.name.trim().is_empty()
            || self.name.len() > 80
            || self.name.chars().any(char::is_control)
        {
            return Err(Error::InvalidInput("extension name".into()));
        }
        if self.description.len() > 2048 || self.instructions.len() > 8192 {
            return Err(Error::LimitExceeded("extension instruction text".into()));
        }
        if self.content_sha256 != self.digest() {
            return Err(Error::InvalidInput(
                "extension snapshot digest mismatch".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedInstructions {
    pub selection: InstructionSelection,
    pub items: Vec<CapturedInstruction>,
}
impl Default for CapturedInstructions {
    /// Old snapshots and standalone callers do not silently adopt host settings.
    fn default() -> Self {
        Self {
            selection: InstructionSelection::Explicit { references: vec![] },
            items: vec![],
        }
    }
}
impl CapturedInstructions {
    pub fn new(selection: InstructionSelection, items: Vec<CapturedInstruction>) -> Result<Self> {
        let captured = Self { selection, items };
        captured.validate()?;
        Ok(captured)
    }
    pub fn validate(&self) -> Result<()> {
        self.selection.validate()?;
        let mut ids = BTreeSet::new();
        for item in &self.items {
            item.validate()?;
            if !ids.insert(&item.id) {
                return Err(Error::InvalidInput("duplicate captured extension".into()));
            }
        }
        for kind in [
            InstructionKind::Skill,
            InstructionKind::Method,
            InstructionKind::Plugin,
        ] {
            if self.items.iter().filter(|i| i.kind == kind).count() > 4 {
                return Err(Error::LimitExceeded("at most four enabled instructions of each kind can be captured; disable some extensions or choose a saved subset".into()));
            }
        }
        if let InstructionSelection::Explicit { references } = &self.selection {
            if references.len() != self.items.len()
                || references
                    .iter()
                    .zip(&self.items)
                    .any(|(r, i)| r.id != i.id || r.revision != i.revision)
            {
                return Err(Error::InvalidInput(
                    "extension selection does not match captured records".into(),
                ));
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| Error::Persistence("extension snapshot encoding".into()))?
            .len()
            > 16 * 1024
        {
            return Err(Error::LimitExceeded("captured extension snapshot exceeds 16 KiB; shorten instructions or choose a saved subset".into()));
        }
        Ok(())
    }
}
