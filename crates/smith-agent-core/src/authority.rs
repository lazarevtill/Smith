use crate::{Error, Result, Workspace, WorkspaceIdentity};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

/// An application-authenticated identity, distinct from the selected persona.
/// This crate does not authenticate users or implement the original pairing protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal(String);

impl Principal {
    pub fn new(authenticated_id: impl Into<String>) -> Result<Self> {
        let id = authenticated_id.into();
        if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
            return Err(Error::InvalidInput("principal id".into()));
        }
        Ok(Self(id))
    }
    pub fn id(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadWorkspace,
    ListWorkspace,
    WriteWorkspace,
    SearchWorkspace,
    EditWorkspace,
    TaskControl,
    ReadConversations,
    ExportConversations,
    SaveArtifact,
    ReadMemory,
    WriteMemory,
    RenderChart,
    ReadStatus,
    ReadDocument,
    WriteDocument,
    EmbedText,
    GenerateImage,
    GitRead,
    GitWrite,
    WebFetch,
    SendWebhook,
    SystemMonitor,
    ModelInference,
    Shell,
    ComputerUse,
    Network,
    Mcp,
}

/// Exact host-selected resource IDs. An audit record alone is never authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Conversation,
    MemoryNamespace,
    Document,
    ArtifactCollection,
    ProviderBinding,
    EffectBinding,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceScope {
    pub kind: ResourceKind,
    pub id: String,
}

/// Audit data only. Deserializing a manifest does not create an execution grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantManifest {
    pub principal: String,
    pub workspace: WorkspaceIdentity,
    pub persona: String,
    pub capabilities: BTreeSet<Capability>,
    #[serde(default)]
    pub resources: BTreeSet<ResourceScope>,
}

/// Opaque, non-deserializable host authority. Never construct this from a prompt,
/// tool output, or an untrusted request's principal/workspace fields.
#[derive(Clone)]
pub struct ExecutionGrant {
    principal: Principal,
    workspace: Arc<Workspace>,
    persona: String,
    capabilities: BTreeSet<Capability>,
    resources: BTreeSet<ResourceScope>,
}

impl ExecutionGrant {
    /// Explicit trusted-host boundary; all capabilities must be supplied.
    pub fn new(
        principal: Principal,
        workspace: Arc<Workspace>,
        persona: impl Into<String>,
        capabilities: impl IntoIterator<Item = Capability>,
    ) -> Result<Self> {
        Self::new_scoped(principal, workspace, persona, capabilities, [])
    }

    /// Only a trusted host may create these exact scopes after explicit selection.
    pub fn new_scoped(
        principal: Principal,
        workspace: Arc<Workspace>,
        persona: impl Into<String>,
        capabilities: impl IntoIterator<Item = Capability>,
        resources: impl IntoIterator<Item = ResourceScope>,
    ) -> Result<Self> {
        let persona = persona.into();
        if persona.is_empty() || persona.len() > 128 {
            return Err(Error::InvalidInput("persona".into()));
        }
        let resources: BTreeSet<_> = resources.into_iter().collect();
        if resources.len() > 128
            || resources
                .iter()
                .any(|r| r.id.is_empty() || r.id.len() > 256 || r.id.chars().any(char::is_control))
        {
            return Err(Error::InvalidInput("resource scope IDs or count".into()));
        }
        Ok(Self {
            principal,
            workspace,
            persona,
            capabilities: capabilities.into_iter().collect(),
            resources,
        })
    }

    /// A child can borrow a persona and remove capabilities, never change its
    /// effective principal or workspace, or acquire a capability from a target.
    pub fn delegate(
        &self,
        persona: impl Into<String>,
        requested: impl IntoIterator<Item = Capability>,
    ) -> Result<Self> {
        let capabilities = requested
            .into_iter()
            .filter(|c| self.capabilities.contains(c));
        Self::new_scoped(
            self.principal.clone(),
            self.workspace.clone(),
            persona,
            capabilities,
            self.resources.iter().cloned(),
        )
    }

    pub fn delegate_scoped(
        &self,
        persona: impl Into<String>,
        requested: impl IntoIterator<Item = Capability>,
        resources: impl IntoIterator<Item = ResourceScope>,
    ) -> Result<Self> {
        Self::new_scoped(
            self.principal.clone(),
            self.workspace.clone(),
            persona,
            requested
                .into_iter()
                .filter(|c| self.capabilities.contains(c)),
            resources.into_iter().filter(|r| self.resources.contains(r)),
        )
    }

    pub fn permits_resource(&self, kind: ResourceKind, id: &str) -> bool {
        self.resources.contains(&ResourceScope {
            kind,
            id: id.into(),
        })
    }
    pub fn resources(&self) -> &BTreeSet<ResourceScope> {
        &self.resources
    }
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn permits(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
    pub fn manifest(&self) -> GrantManifest {
        GrantManifest {
            principal: self.principal.id().into(),
            workspace: self.workspace.identity().clone(),
            persona: self.persona.clone(),
            capabilities: self.capabilities.clone(),
            resources: self.resources.clone(),
        }
    }
    pub(crate) fn workspace(&self) -> &Workspace {
        &self.workspace
    }
    pub(crate) fn validate_task(&self, original: &GrantManifest) -> Result<()> {
        if self.principal.id() != original.principal
            || self.workspace.identity() != &original.workspace
            || !self.capabilities.is_subset(&original.capabilities)
            || !self.resources.is_subset(&original.resources)
        {
            return Err(Error::Denied(
                "task identity, workspace, or capability binding mismatch".into(),
            ));
        }
        Ok(())
    }
}
