use crate::*;
use std::collections::BTreeSet;

pub(crate) const MAX_BYTES: u64 = 9_007_199_254_740_991;
pub(crate) const MAX_CONTEXT: u32 = 1_048_576;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    pub field: &'static str,
}
impl ValidationError {
    pub(crate) fn new(field: &'static str) -> Self {
        Self { field }
    }
}
impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid acceleration planning input: {}", self.field)
    }
}
impl std::error::Error for ValidationError {}
type Result<T> = std::result::Result<T, ValidationError>;
fn require(ok: bool, field: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ValidationError::new(field))
    }
}
fn id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 160
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
fn text(s: &str) -> bool {
    !s.is_empty() && s.len() <= 500 && !s.chars().any(char::is_control)
}
fn sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn unique<T: Ord>(values: impl IntoIterator<Item = T>) -> bool {
    let mut seen = BTreeSet::new();
    values.into_iter().all(|v| seen.insert(v))
}
impl HardwareInventory {
    pub fn validate(&self) -> Result<()> {
        require(id(&self.generation), "inventory.generation")?;
        require(text(&self.os_version), "inventory.os_version")?;
        require(
            (1..=1024).contains(&self.logical_cpu_count),
            "inventory.logical_cpu_count",
        )?;
        require(
            !self.memory_pools.is_empty() && self.memory_pools.len() <= 32,
            "inventory.memory_pools",
        )?;
        require(self.devices.len() <= 32, "inventory.devices")?;
        require(
            unique(self.memory_pools.iter().map(|v| &v.id)),
            "inventory.duplicate_pool",
        )?;
        require(
            unique(self.devices.iter().map(|v| &v.id)),
            "inventory.duplicate_device",
        )?;
        require(
            self.memory_pools
                .iter()
                .any(|p| p.id == self.host_memory_pool_id),
            "inventory.host_pool",
        )?;
        for p in &self.memory_pools {
            require(id(&p.id), "pool.id")?;
            require(
                p.total_bytes.is_none_or(|n| n <= MAX_BYTES)
                    && p.available_bytes.is_none_or(|n| n <= MAX_BYTES),
                "pool.bytes",
            )?;
            require(
                !matches!((p.total_bytes,p.available_bytes), (Some(t),Some(a)) if a > t),
                "pool.available_exceeds_total",
            )?;
        }
        for d in &self.devices {
            require(id(&d.id), "device.id")?;
            require(
                d.driver_version.as_deref().is_none_or(text),
                "device.driver_version",
            )?;
            require(
                self.memory_pools.iter().any(|p| p.id == d.memory_pool_id),
                "device.memory_pool",
            )?;
            require(
                d.class != DeviceClass::Cpu || d.memory_pool_id == self.host_memory_pool_id,
                "device.cpu_pool",
            )?;
        }
        Ok(())
    }
    /// Hash all observations, including available memory and generation. Device
    /// and pool enumeration order is irrelevant; identities must be unique.
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical.devices.sort_by(|a, b| a.id.cmp(&b.id));
        canonical.memory_pools.sort_by(|a, b| a.id.cmp(&b.id));
        crate::hash(&canonical)
    }
}
impl ModelDescriptor {
    pub fn validate(&self) -> Result<()> {
        require(id(&self.id) && sha(&self.bundle_sha256), "model.identity")?;
        require(
            id(&self.architecture) && id(&self.quantization),
            "model.format_metadata",
        )?;
        require(
            (128..=MAX_CONTEXT).contains(&self.native_context_limit),
            "model.native_context_limit",
        )?;
        require(
            self.capabilities.len() <= 6 && unique(self.capabilities.iter()),
            "model.capabilities",
        )
    }
}
impl AdapterDescriptor {
    pub fn validate(&self) -> Result<()> {
        require(id(&self.id) && text(&self.version), "adapter.identity")?;
        require(
            self.bundle_sha256.as_deref().is_none_or(sha),
            "adapter.bundle_sha256",
        )?;
        require(
            self.selected_executable_sha256.as_deref().is_none_or(sha),
            "adapter.selected_executable_sha256",
        )?;
        require(
            self.availability != Availability::Qualified || self.bundle_sha256.is_some(),
            "adapter.qualified_bundle",
        )?;
        require(
            self.capabilities.len() <= 6 && unique(self.capabilities.iter()),
            "adapter.capabilities",
        )?;
        require(
            self.qualified_devices.len() <= 32
                && unique(self.qualified_devices.iter().map(|d| &d.device_id)),
            "adapter.qualified_devices",
        )?;
        for d in &self.qualified_devices {
            require(
                id(&d.device_id)
                    && text(&d.os_version)
                    && d.driver_version.as_deref().is_none_or(text),
                "adapter.device_qualification",
            )?;
        }
        let l = &self.settings;
        require(
            (128..=MAX_CONTEXT).contains(&l.max_context_tokens),
            "limits.context",
        )?;
        require(
            (1..=MAX_CONTEXT).contains(&l.max_output_tokens),
            "limits.output",
        )?;
        require((1..=1024).contains(&l.max_threads), "limits.threads")?;
        require(
            (1..=65536).contains(&l.max_batch_tokens)
                && (1..=65536).contains(&l.max_microbatch_tokens),
            "limits.batch",
        )?;
        require(self.models.len() <= 128, "adapter.model_profiles")?;
        let mut profile_ids = BTreeSet::new();
        for p in &self.models {
            require(
                sha(&p.model_bundle_sha256) && id(&p.architecture) && id(&p.quantization),
                "profile.model_identity",
            )?;
            require(
                p.gpu_layers.is_none_or(|n| n <= 10000),
                "profile.gpu_layers",
            )?;
            require(
                profile_ids.insert((&p.model_bundle_sha256, p.kv_type, p.gpu_layers)),
                "profile.duplicate",
            )?;
            if let Some(m) = &p.memory {
                require(text(&m.provenance), "memory.provenance")?;
                require(
                    !m.allocations.is_empty()
                        && m.allocations.len() <= 64
                        && unique(m.allocations.iter().map(|a| &a.id)),
                    "memory.allocations",
                )?;
                require(
                    m.allocations
                        .iter()
                        .any(|a| a.locations.contains(&MemoryLocation::Host)),
                    "memory.host_overhead_missing",
                )?;
                for a in &m.allocations {
                    require(
                        id(&a.id)
                            && !a.locations.is_empty()
                            && a.locations.len() <= 2
                            && unique(a.locations.iter()),
                        "memory.allocation_identity",
                    )?;
                    require(
                        a.base_bytes != 0
                            || a.bytes_per_context_token != 0
                            || a.bytes_per_batch_token != 0
                            || a.bytes_per_thread != 0,
                        "memory.zero_allocation",
                    )?;
                }
            }
        }
        Ok(())
    }
}
impl OptimizationIntent {
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        crate::hash(self)
    }
    pub fn validate(&self) -> Result<()> {
        let o = &self.overrides;
        require(
            self.mode != Mode::Manual || (o.backend.is_some() && o.device_id.is_some()),
            "manual.requires_backend_and_device",
        )?;
        require(o.device_id.as_deref().is_none_or(id), "override.device_id")?;
        require(
            o.context_tokens
                .is_none_or(|v| (128..=MAX_CONTEXT).contains(&v)),
            "override.context",
        )?;
        require(
            o.output_tokens
                .is_none_or(|v| (1..=MAX_CONTEXT).contains(&v)),
            "override.output",
        )?;
        require(
            o.threads.is_none_or(|v| (1..=1024).contains(&v))
                && o.prefill_threads.is_none_or(|v| (1..=1024).contains(&v)),
            "override.threads",
        )?;
        require(
            o.batch_tokens.is_none_or(|v| (1..=65536).contains(&v))
                && o.microbatch_tokens.is_none_or(|v| (1..=65536).contains(&v)),
            "override.batch",
        )?;
        require(
            o.gpu_layers.is_none_or(|v| v <= 10000),
            "override.gpu_layers",
        )?;
        require(
            [o.host_budget_bytes, o.device_budget_bytes, o.headroom_bytes]
                .into_iter()
                .flatten()
                .all(|v| v <= MAX_BYTES),
            "override.memory",
        )?;
        Ok(())
    }
}
