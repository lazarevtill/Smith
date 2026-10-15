//! Passive runtime preferences in the already-keyed application database.
//!
//! No saved value is an execution grant. The host must invalidate pending plans
//! after every successful write, replan/revalidate against current facts, and
//! explicitly load again. Success helpers participate in the caller's config
//! transaction; callers must roll that transaction back on any helper error.
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use smith_acceleration::{
    Backend, Capability, EffectivePlan, EffectiveSettings, LocalFallback, Mode, OptimizationIntent,
    Overrides, PoolEstimate, Preference,
};
use std::collections::BTreeSet;

pub const SETTINGS_KEY: &str = "runtime_preferences";
const FORMAT_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 65_536;
const MAX_REVISION: u64 = 9_007_199_254_740_991;
const MAX_MEMORY_BYTES: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Database,
    Malformed,
    UnsupportedVersion,
    InvalidIntent,
    InvalidMetadata,
    Conflict,
    RevisionExhausted,
    NoRollbackCandidate,
    PlanMismatch,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Database => "Runtime preferences could not be read or saved.",
            Self::Malformed => "Saved runtime preferences are malformed; they were not reset.",
            Self::UnsupportedVersion => "Saved runtime preferences use an unsupported format.",
            Self::InvalidIntent => "The requested runtime preferences are invalid.",
            Self::InvalidMetadata => "The runtime success metadata is invalid.",
            Self::Conflict => "Runtime preferences changed; refresh and review again.",
            Self::RevisionExhausted => "The runtime preferences revision cannot advance.",
            Self::NoRollbackCandidate => "No different successful runtime intent is retained.",
            Self::PlanMismatch => "The successful plan does not match the reviewed preferences.",
        })
    }
}
impl std::error::Error for Error {}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub format_version: u32,
    pub revision: u64,
    pub intent: OptimizationIntent,
    #[serde(deserialize_with = "present_option")]
    pub last_successful: Option<SuccessfulSettings>,
    #[serde(deserialize_with = "present_option")]
    pub previous_successful: Option<SuccessfulSettings>,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            revision: 0,
            intent: OptimizationIntent {
                mode: Mode::Auto,
                preference: Preference::Balanced,
                capability: Capability::TextChat,
                local_fallback: LocalFallback::Ask,
                overrides: Overrides::default(),
            },
            last_successful: None,
            previous_successful: None,
        }
    }
}
impl Preferences {
    /// A candidate for restoring desired settings only. The associated runtime
    /// and model may no longer exist or be qualified, so a fresh plan is needed.
    pub fn rollback_candidate(&self) -> Option<&SuccessfulSettings> {
        self.last_successful
            .iter()
            .chain(self.previous_successful.iter())
            .find(|record| record.intent != self.intent)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuccessfulSettings {
    /// Revision assigned to the atomic successful config/settings commit.
    pub settings_revision: u64,
    pub intent: OptimizationIntent,
    pub metadata: WorkingMetadata,
}

/// Deserializable historical evidence, deliberately not an EffectivePlan,
/// ApprovedPlan, command line, file path, endpoint, or execution permission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "details",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkingMetadata {
    QualifiedPlan(EffectivePlanMetadata),
    SelectedRuntime(SelectedSuccessRecord),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivePlanMetadata {
    pub plan_id: String,
    pub inventory_generation: String,
    /// Settings revision against which the successful plan was computed.
    pub config_revision: u64,
    pub adapter_id: String,
    pub adapter_bundle_sha256: String,
    pub model_bundle_sha256: String,
    pub backend: Backend,
    pub device_id: String,
    pub settings: EffectiveSettings,
    pub memory: Vec<PoolEstimate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectedBackend {
    Cpu,
    Device,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectedEvidence {
    RuntimeReadyPlacementUnknown,
}

/// The explicitly selected runtime became ready. Its requested backend/device
/// and basic settings do not establish placement or acceleration qualification.
/// Advanced settings remain in the strict intent; absent basic metadata is not
/// filled with guessed runtime defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedSuccessRecord {
    pub model_sha256: String,
    pub executable_sha256: String,
    pub requested_backend: SelectedBackend,
    pub requested_device: Option<String>,
    pub context_tokens: u32,
    pub output_tokens: u32,
    pub threads: u16,
    pub gpu_layers: Option<u32>,
    pub evidence: SelectedEvidence,
}

/// Missing preferences return safe defaults without writing a row. Malformed,
/// oversized, non-text, unknown-version, or inconsistent records fail closed.
pub fn read(db: &Connection) -> Result<Preferences> {
    Ok(read_snapshot(db)?.preferences)
}

/// One conditional SQL write provides compare-and-swap, including first save.
/// The monotonically increasing revision prevents a change/revert ABA match.
/// When called inside a transaction, this does not commit that transaction.
pub fn update(
    db: &Connection,
    expected_revision: u64,
    intent: OptimizationIntent,
) -> Result<Preferences> {
    intent.validate().map_err(|_| Error::InvalidIntent)?;
    let snapshot = expected_snapshot(db, expected_revision)?;
    let mut next = snapshot.preferences.clone();
    next.intent = intent;
    write_next(db, snapshot, next)
}

/// Reset overrides and return to Auto (Manual requires backend/device overrides).
/// Preserve preference, capability, fallback, working records and every other
/// settings row, including model/source records and the current managed config.
pub fn reset_overrides(db: &Connection, expected_revision: u64) -> Result<Preferences> {
    let snapshot = expected_snapshot(db, expected_revision)?;
    let mut next = snapshot.preferences.clone();
    next.intent.mode = Mode::Auto;
    next.intent.overrides = Overrides::default();
    write_next(db, snapshot, next)
}

/// Restore a retained different successful intent without changing managed
/// config, consuming historical records, granting execution or spawning work.
pub fn rollback_intent(db: &Connection, expected_revision: u64) -> Result<Preferences> {
    let snapshot = expected_snapshot(db, expected_revision)?;
    let mut next = snapshot.preferences.clone();
    next.intent = next
        .rollback_candidate()
        .ok_or(Error::NoRollbackCandidate)?
        .intent
        .clone();
    write_next(db, snapshot, next)
}

/// Call only after owned runtime readiness and exact plan revalidation. Persist
/// this record in the same transaction as the new managed config. This helper
/// does not commit; any error must abort the caller's entire transaction.
#[cfg_attr(not(test), allow(dead_code))] // Qualified catalog admission is separate from selected-file previews.
pub fn commit_success(
    tx: &Transaction<'_>,
    expected_revision: u64,
    planned_intent: &OptimizationIntent,
    plan: &EffectivePlan,
) -> Result<Preferences> {
    let fingerprint = planned_intent
        .fingerprint()
        .map_err(|_| Error::InvalidIntent)?;
    if plan.config_revision() != expected_revision || plan.intent_sha256() != fingerprint {
        return Err(Error::PlanMismatch);
    }
    let metadata = EffectivePlanMetadata {
        plan_id: plan.id().into(),
        inventory_generation: plan.inventory_generation().into(),
        config_revision: plan.config_revision(),
        adapter_id: plan.adapter_id().into(),
        adapter_bundle_sha256: plan.adapter_bundle_sha256().into(),
        model_bundle_sha256: plan.model_bundle_sha256().into(),
        backend: plan.backend(),
        device_id: plan.device_id().into(),
        settings: plan.settings().clone(),
        memory: plan.memory().to_vec(),
    };
    save_success(
        tx,
        expected_revision,
        planned_intent,
        WorkingMetadata::QualifiedPlan(metadata),
    )
}

/// For an explicitly reviewed selected runtime that became ready without full
/// acceleration qualification. The caller supplies owned-runtime observations;
/// no HTTP-deserialized record is evidence that an operation actually succeeded.
pub fn commit_selected_success(
    tx: &Transaction<'_>,
    expected_revision: u64,
    planned_intent: &OptimizationIntent,
    record: &SelectedSuccessRecord,
) -> Result<Preferences> {
    save_success(
        tx,
        expected_revision,
        planned_intent,
        WorkingMetadata::SelectedRuntime(record.clone()),
    )
}

struct Snapshot {
    raw: Option<String>,
    preferences: Preferences,
}

fn read_snapshot(db: &Connection) -> Result<Snapshot> {
    // SQL bounds the value before rusqlite allocates/copies it. Blob length is
    // used because SQLite length(TEXT) counts characters, not UTF-8 bytes.
    let row: Option<Option<String>> = db
        .query_row(
            "SELECT CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB))<=?2 \
             THEN value ELSE NULL END FROM settings WHERE key=?1",
            params![SETTINGS_KEY, MAX_RECORD_BYTES as i64],
            |row| row.get(0),
        )
        .optional()?;
    let raw = match row {
        None => {
            return Ok(Snapshot {
                raw: None,
                preferences: Preferences::default(),
            })
        }
        Some(Some(raw)) => raw,
        Some(None) => return Err(Error::Malformed),
    };
    let preferences: Preferences = serde_json::from_str(&raw).map_err(|_| Error::Malformed)?;
    if preferences.format_version != FORMAT_VERSION {
        return Err(Error::UnsupportedVersion);
    }
    validate_preferences(&preferences).map_err(|_| Error::Malformed)?;
    Ok(Snapshot {
        raw: Some(raw),
        preferences,
    })
}

fn expected_snapshot(db: &Connection, revision: u64) -> Result<Snapshot> {
    let snapshot = read_snapshot(db)?;
    if snapshot.preferences.revision != revision {
        return Err(Error::Conflict);
    }
    Ok(snapshot)
}

fn write_next(db: &Connection, snapshot: Snapshot, mut next: Preferences) -> Result<Preferences> {
    next.revision = next_revision(snapshot.preferences.revision)?;
    validate_preferences(&next)?;
    let value = serde_json::to_string(&next).map_err(|_| Error::InvalidMetadata)?;
    if value.len() > MAX_RECORD_BYTES {
        return Err(Error::InvalidMetadata);
    }
    let changed = match snapshot.raw {
        Some(old) => db.execute(
            "UPDATE settings SET value=?2 WHERE key=?1 AND typeof(value)='text' \
             AND value=?3 COLLATE BINARY",
            params![SETTINGS_KEY, value, old],
        )?,
        None => db.execute(
            "INSERT INTO settings(key,value) SELECT ?1,?2 \
             WHERE NOT EXISTS(SELECT 1 FROM settings WHERE key=?1)",
            params![SETTINGS_KEY, value],
        )?,
    };
    if changed != 1 {
        return Err(Error::Conflict);
    }
    Ok(next)
}

fn next_revision(revision: u64) -> Result<u64> {
    revision
        .checked_add(1)
        .filter(|r| *r <= MAX_REVISION)
        .ok_or(Error::RevisionExhausted)
}

fn save_success(
    tx: &Transaction<'_>,
    expected_revision: u64,
    planned_intent: &OptimizationIntent,
    metadata: WorkingMetadata,
) -> Result<Preferences> {
    planned_intent
        .validate()
        .map_err(|_| Error::InvalidIntent)?;
    let snapshot = expected_snapshot(tx, expected_revision)?;
    if snapshot.preferences.intent != *planned_intent {
        return Err(Error::PlanMismatch);
    }
    let mut next = snapshot.preferences.clone();
    let successful = SuccessfulSettings {
        settings_revision: next_revision(expected_revision)?,
        intent: planned_intent.clone(),
        metadata,
    };
    validate_success(&successful)?;
    // Repeated success of the same intent must not erase the last different
    // working intent, which is the useful candidate for settings rollback.
    if next
        .last_successful
        .as_ref()
        .is_some_and(|last| last.intent != successful.intent)
    {
        next.previous_successful = next.last_successful.clone();
    }
    next.last_successful = Some(successful);
    write_next(tx, snapshot, next)
}

fn validate_preferences(p: &Preferences) -> Result<()> {
    if p.format_version != FORMAT_VERSION || p.revision == 0 || p.revision > MAX_REVISION {
        return Err(Error::InvalidMetadata);
    }
    p.intent.validate().map_err(|_| Error::InvalidIntent)?;
    for record in p.last_successful.iter().chain(p.previous_successful.iter()) {
        if record.settings_revision > p.revision {
            return Err(Error::InvalidMetadata);
        }
        validate_success(record)?;
    }
    if let Some(previous) = &p.previous_successful {
        let last = p.last_successful.as_ref().ok_or(Error::InvalidMetadata)?;
        if previous.settings_revision >= last.settings_revision || previous.intent == last.intent {
            return Err(Error::InvalidMetadata);
        }
    }
    Ok(())
}

fn validate_success(record: &SuccessfulSettings) -> Result<()> {
    if record.settings_revision == 0 || record.settings_revision > MAX_REVISION {
        return Err(Error::InvalidMetadata);
    }
    record.intent.validate().map_err(|_| Error::InvalidIntent)?;
    match &record.metadata {
        WorkingMetadata::QualifiedPlan(p) => {
            let s = &p.settings;
            let mut pools = BTreeSet::new();
            if !sha(&p.plan_id)
                || !identifier(&p.inventory_generation)
                || !identifier(&p.adapter_id)
                || !identifier(&p.device_id)
                || !sha(&p.adapter_bundle_sha256)
                || !sha(&p.model_bundle_sha256)
                || p.config_revision.checked_add(1) != Some(record.settings_revision)
                || !(128..=1_048_576).contains(&s.context_tokens)
                || !(1..=s.context_tokens).contains(&s.output_tokens)
                || !(1..=1024).contains(&s.threads)
                || !(1..=1024).contains(&s.prefill_threads)
                || !(1..=65_536.min(s.context_tokens)).contains(&s.batch_tokens)
                || !(1..=s.batch_tokens).contains(&s.microbatch_tokens)
                || s.gpu_layers.is_some_and(|n| n > 10_000)
                || s.headroom_bytes > MAX_MEMORY_BYTES
                || p.memory.is_empty()
                || p.memory.len() > 32
            {
                return Err(Error::InvalidMetadata);
            }
            for pool in &p.memory {
                if !identifier(&pool.pool_id)
                    || !pools.insert(&pool.pool_id)
                    || pool.headroom_bytes != s.headroom_bytes
                    || pool.available_bytes > MAX_MEMORY_BYTES
                    || pool.permitted_bytes > pool.available_bytes
                    || pool
                        .required_bytes
                        .checked_add(pool.headroom_bytes)
                        .is_none_or(|sum| sum > pool.permitted_bytes)
                {
                    return Err(Error::InvalidMetadata);
                }
            }
        }
        WorkingMetadata::SelectedRuntime(p) => {
            if !sha(&p.model_sha256)
                || !sha(&p.executable_sha256)
                || !(128..=1_048_576).contains(&p.context_tokens)
                || !(1..=p.context_tokens).contains(&p.output_tokens)
                || !(1..=512).contains(&p.threads)
                || record.intent.capability != Capability::TextChat
                || record
                    .intent
                    .overrides
                    .context_tokens
                    .is_some_and(|n| n != p.context_tokens)
                || record
                    .intent
                    .overrides
                    .output_tokens
                    .is_some_and(|n| n != p.output_tokens)
                || record
                    .intent
                    .overrides
                    .threads
                    .is_some_and(|n| n != p.threads)
                || record
                    .intent
                    .overrides
                    .gpu_layers
                    .is_some_and(|n| Some(n) != p.gpu_layers)
                || match p.requested_backend {
                    SelectedBackend::Cpu => p.requested_device.is_some() || p.gpu_layers != Some(0),
                    SelectedBackend::Device => {
                        !p.requested_device.as_deref().is_some_and(device_token)
                            || !p.gpu_layers.is_some_and(|n| (1..=10_000).contains(&n))
                    }
                }
            {
                return Err(Error::InvalidMetadata);
            }
        }
    }
    Ok(())
}

fn sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
fn device_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

// A versioned record must explicitly contain nullable history fields. An
// accidentally missing field must not silently discard retained history.
fn present_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smith_acceleration::*;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);")
            .unwrap();
        db
    }
    fn selected() -> SelectedSuccessRecord {
        SelectedSuccessRecord {
            model_sha256: "a".repeat(64),
            executable_sha256: "b".repeat(64),
            requested_backend: SelectedBackend::Cpu,
            requested_device: None,
            context_tokens: 2048,
            output_tokens: 256,
            threads: 4,
            gpu_layers: Some(0),
            evidence: SelectedEvidence::RuntimeReadyPlacementUnknown,
        }
    }
    fn success(db: &mut Connection, expected: u64, intent: &OptimizationIntent) -> Preferences {
        let tx = db.transaction().unwrap();
        let result = commit_selected_success(&tx, expected, intent, &selected()).unwrap();
        tx.commit().unwrap();
        result
    }
    fn plan(intent: &OptimizationIntent, revision: u64) -> EffectivePlan {
        let inventory = HardwareInventory {
            generation: "synthetic-1".into(),
            os: OperatingSystem::Linux,
            architecture: Architecture::X86_64,
            os_version: "synthetic".into(),
            logical_cpu_count: 8,
            host_memory_pool_id: "ram".into(),
            memory_pools: vec![MemoryPool {
                id: "ram".into(),
                total_bytes: Some(8 << 30),
                available_bytes: Some(8 << 30),
            }],
            devices: vec![Device {
                id: "cpu".into(),
                class: DeviceClass::Cpu,
                vendor: Vendor::Intel,
                memory_pool_id: "ram".into(),
                driver_version: None,
            }],
        };
        let model = ModelDescriptor {
            id: "synthetic-model".into(),
            bundle_sha256: "a".repeat(64),
            format: ModelFormat::GgufBundle,
            architecture: "synthetic".into(),
            quantization: "q4".into(),
            capabilities: vec![Capability::TextChat],
            native_context_limit: 4096,
        };
        let adapter = AdapterDescriptor {
            id: "synthetic-adapter".into(),
            backend: Backend::LlamaCpu,
            os: inventory.os,
            architecture: inventory.architecture,
            version: "synthetic-1".into(),
            bundle_sha256: Some("b".repeat(64)),
            selected_executable_sha256: None,
            availability: Availability::Qualified,
            capabilities: vec![Capability::TextChat],
            qualified_devices: vec![QualifiedDevice {
                device_id: "cpu".into(),
                os_version: inventory.os_version.clone(),
                driver_version: None,
            }],
            settings: SettingsLimits {
                max_context_tokens: 4096,
                max_output_tokens: 1024,
                max_threads: 8,
                max_batch_tokens: 512,
                max_microbatch_tokens: 256,
                supports_flash_attention: false,
                supports_mmap: false,
                supports_mlock: false,
            },
            models: vec![ModelProfile {
                model_bundle_sha256: model.bundle_sha256.clone(),
                format: model.format,
                architecture: model.architecture.clone(),
                quantization: model.quantization.clone(),
                kv_type: KvType::F16,
                gpu_layers: Some(0),
                memory: Some(MemoryModel {
                    provenance: "synthetic bounded test allocation".into(),
                    allocations: vec![MemoryAllocation {
                        id: "resident".into(),
                        locations: vec![MemoryLocation::Host],
                        base_bytes: 1 << 20,
                        bytes_per_context_token: 1024,
                        bytes_per_batch_token: 1024,
                        bytes_per_thread: 1024,
                    }],
                }),
            }],
        };
        match smith_acceleration::plan(&inventory, &[adapter], &model, intent, revision).unwrap() {
            PlanningOutcome::Candidate { plan, .. } => plan,
            other => panic!("synthetic qualified plan unexpectedly blocked: {other:?}"),
        }
    }

    #[test]
    fn missing_row_is_safe_and_read_does_not_write() {
        let db = db();
        let p = read(&db).unwrap();
        assert_eq!(p, Preferences::default());
        assert_eq!(p.intent.local_fallback, LocalFallback::Ask);
        assert_eq!(p.rollback_candidate(), None);
        assert_eq!(db.total_changes(), 0);
        assert_eq!(rollback_intent(&db, 0), Err(Error::NoRollbackCandidate));
        assert_eq!(db.total_changes(), 0);
    }

    #[test]
    fn strict_malformed_records_are_never_reset() {
        let db = db();
        let good = update(&db, 0, Preferences::default().intent).unwrap();
        let baseline = serde_json::to_value(&good).unwrap();
        let mut bad_rows = vec!["{".to_owned()];
        for (field, value) in [
            ("format_version", serde_json::json!(2)),
            ("revision", serde_json::json!(0)),
            ("authority", serde_json::json!(true)),
            (
                "last_successful",
                serde_json::json!({"command":"never run"}),
            ),
        ] {
            let mut bad = baseline.clone();
            bad[field] = value;
            bad_rows.push(bad.to_string());
        }
        let mut invalid_intent = baseline.clone();
        invalid_intent["intent"]["overrides"]["threads"] = serde_json::json!(0);
        bad_rows.push(invalid_intent.to_string());
        for field in ["last_successful", "previous_successful"] {
            let mut missing = baseline.clone();
            missing.as_object_mut().unwrap().remove(field);
            bad_rows.push(missing.to_string());
        }
        bad_rows.push("x".repeat(MAX_RECORD_BYTES + 1));
        for raw in bad_rows {
            db.execute(
                "UPDATE settings SET value=?1 WHERE key=?2",
                params![raw, SETTINGS_KEY],
            )
            .unwrap();
            let changes = db.total_changes();
            assert!(read(&db).is_err());
            assert!(reset_overrides(&db, 1).is_err());
            assert!(update(&db, 1, good.intent.clone()).is_err());
            assert_eq!(db.total_changes(), changes);
            let retained: String = db
                .query_row(
                    "SELECT value FROM settings WHERE key=?1",
                    [SETTINGS_KEY],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(retained, raw);
        }
        db.execute(
            "UPDATE settings SET value=x'7b7d' WHERE key=?1",
            [SETTINGS_KEY],
        )
        .unwrap();
        assert_eq!(read(&db), Err(Error::Malformed));
    }

    #[test]
    fn encrypted_restart_and_stale_writes_preserve_latest_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.sqlite3");
        let open = || {
            let db = Connection::open(&path).unwrap();
            db.pragma_update(None, "key", "synthetic-runtime-preferences-test-key")
                .unwrap();
            let version: String = db
                .query_row("PRAGMA cipher_version", [], |r| r.get(0))
                .unwrap();
            assert!(version.starts_with('4'));
            db
        };
        let a = open();
        a.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            .unwrap();
        let b = open();
        let initial = read(&b).unwrap();
        let mut desired = initial.intent.clone();
        desired.preference = Preference::Quiet;
        let latest = update(&a, 0, desired).unwrap();
        assert_eq!(
            update(&b, initial.revision, initial.intent),
            Err(Error::Conflict)
        );
        drop(a);
        drop(b);
        let reopened = open();
        assert_eq!(read(&reopened).unwrap(), latest);
        assert!(
            reopened
                .query_row("SELECT count(*) FROM settings", [], |r| r.get::<_, i64>(0))
                .unwrap()
                == 1
        );
        drop(reopened);
        let bytes = std::fs::read(path).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3"));
        assert!(!bytes
            .windows(SETTINGS_KEY.len())
            .any(|w| w == SETTINGS_KEY.as_bytes()));
    }

    #[test]
    fn conditional_write_rejects_intervening_creation_or_change() {
        let db = db();
        let absent = expected_snapshot(&db, 0).unwrap();
        let first = update(&db, 0, Preferences::default().intent).unwrap();
        assert_eq!(write_next(&db, absent, first.clone()), Err(Error::Conflict));
        let stale = expected_snapshot(&db, 1).unwrap();
        let second = reset_overrides(&db, 1).unwrap();
        assert_eq!(write_next(&db, stale, first), Err(Error::Conflict));
        assert_eq!(read(&db).unwrap(), second);
    }

    #[test]
    fn reset_preserves_config_sources_models_and_success_history() {
        let mut db = db();
        db.execute_batch("INSERT INTO settings VALUES('config','managed-original'),('models','model-original'),('sources','source-original')").unwrap();
        let mut intent = Preferences::default().intent;
        intent.mode = Mode::Manual;
        intent.preference = Preference::Quiet;
        intent.overrides.backend = Some(Backend::LlamaCpu);
        intent.overrides.device_id = Some("cpu".into());
        update(&db, 0, intent.clone()).unwrap();
        let working = success(&mut db, 1, &intent);
        let reset = reset_overrides(&db, 2).unwrap();
        assert_eq!(reset.intent.mode, Mode::Auto);
        assert_eq!(reset.intent.overrides, Overrides::default());
        assert_eq!(reset.intent.preference, Preference::Quiet);
        assert_eq!(reset.last_successful, working.last_successful);
        for (key, expected) in [
            ("config", "managed-original"),
            ("models", "model-original"),
            ("sources", "source-original"),
        ] {
            let value: String = db
                .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(value, expected);
        }
    }

    #[test]
    fn rollback_restores_intent_without_loading_or_changing_config() {
        let mut db = db();
        let first = Preferences::default().intent;
        success(&mut db, 0, &first);
        let mut second = first.clone();
        second.preference = Preference::Fast;
        update(&db, 1, second.clone()).unwrap();
        let mut working = success(&mut db, 2, &second);
        // Same-intent successful reload retains the older distinct candidate.
        working = success(&mut db, working.revision, &second);
        assert_eq!(working.previous_successful.as_ref().unwrap().intent, first);
        db.execute(
            "INSERT INTO settings VALUES('config','current-owned-config')",
            [],
        )
        .unwrap();
        let restored = rollback_intent(&db, working.revision).unwrap();
        assert_eq!(restored.intent, first);
        assert_eq!(restored.last_successful, working.last_successful);
        assert_eq!(restored.previous_successful, working.previous_successful);
        assert_eq!(restored.revision, working.revision + 1);
        assert_eq!(restored.rollback_candidate().unwrap().intent, second);
        let config: String = db
            .query_row("SELECT value FROM settings WHERE key='config'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(config, "current-owned-config");
    }

    #[test]
    fn failed_commit_rolls_back_managed_config_and_success_together() {
        let mut db = db();
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE deferred_child(parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED); INSERT INTO settings VALUES('config','old-config')").unwrap();
        let before = read(&db).unwrap();
        let tx = db.transaction().unwrap();
        tx.execute(
            "UPDATE settings SET value='new-config' WHERE key='config'",
            [],
        )
        .unwrap();
        commit_selected_success(&tx, 0, &before.intent, &selected()).unwrap();
        tx.execute("INSERT INTO deferred_child VALUES(404)", [])
            .unwrap();
        assert!(tx.commit().is_err());
        assert_eq!(read(&db).unwrap(), before);
        let config: String = db
            .query_row("SELECT value FROM settings WHERE key='config'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(config, "old-config");
    }

    #[test]
    fn qualified_success_is_bound_to_exact_intent_and_settings_revision() {
        let mut db = db();
        let intent = Preferences::default().intent;
        let qualified = plan(&intent, 0);
        let tx = db.transaction().unwrap();
        let mut different = intent.clone();
        different.preference = Preference::Quiet;
        assert_eq!(
            commit_success(&tx, 0, &different, &qualified),
            Err(Error::PlanMismatch)
        );
        assert_eq!(
            commit_success(&tx, 1, &intent, &qualified),
            Err(Error::PlanMismatch)
        );
        let success = commit_success(&tx, 0, &intent, &qualified).unwrap();
        tx.execute(
            "INSERT INTO settings VALUES('config','atomic-managed-config')",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(read(&db).unwrap(), success);
        let metadata = &success.last_successful.unwrap().metadata;
        assert!(
            matches!(metadata, WorkingMetadata::QualifiedPlan(p) if p.plan_id == qualified.id())
        );
        let raw = serde_json::to_string(metadata).unwrap();
        for absent in [
            "executable_path",
            "endpoint",
            "argv",
            "approved_plan",
            "inventory_sha256",
        ] {
            assert!(!raw.contains(absent));
        }
    }

    #[test]
    fn selected_metadata_cannot_claim_placement_or_hide_paths() {
        let mut db = db();
        let intent = Preferences::default().intent;
        let tx = db.transaction().unwrap();
        let mut invalid = selected();
        invalid.executable_sha256 = "/synthetic/private/executable".into();
        assert_eq!(
            commit_selected_success(&tx, 0, &intent, &invalid),
            Err(Error::InvalidMetadata)
        );
        invalid = selected();
        invalid.requested_backend = SelectedBackend::Device;
        invalid.requested_device = Some("../not-a-device".into());
        assert_eq!(
            commit_selected_success(&tx, 0, &intent, &invalid),
            Err(Error::InvalidMetadata)
        );
        let mut json = serde_json::to_value(selected()).unwrap();
        json["evidence"] = serde_json::json!("gpu_placement_proven");
        assert!(serde_json::from_value::<SelectedSuccessRecord>(json).is_err());
        assert_eq!(read(&tx).unwrap().revision, 0);
    }

    #[test]
    fn selected_basic_settings_match_intent_and_accept_managed_core_bounds() {
        let mut db = db();
        let mut intent = Preferences::default().intent;
        intent.overrides.context_tokens = Some(1_048_576);
        intent.overrides.output_tokens = Some(1_048_576);
        intent.overrides.threads = Some(512);
        update(&db, 0, intent.clone()).unwrap();
        let tx = db.transaction().unwrap();
        assert_eq!(
            commit_selected_success(&tx, 1, &intent, &selected()),
            Err(Error::InvalidMetadata)
        );
        assert_eq!(read(&tx).unwrap().revision, 1);
        let mut actual = selected();
        actual.context_tokens = 1_048_576;
        actual.output_tokens = 1_048_576;
        actual.threads = 512;
        let successful = commit_selected_success(&tx, 1, &intent, &actual).unwrap();
        tx.commit().unwrap();
        assert_eq!(read(&db).unwrap(), successful);
    }

    #[test]
    fn invalid_intent_and_exhausted_revision_are_not_persisted() {
        let db = db();
        let mut intent = Preferences::default().intent;
        intent.mode = Mode::Manual;
        assert_eq!(update(&db, 0, intent), Err(Error::InvalidIntent));
        assert_eq!(db.total_changes(), 0);
        let exhausted = Preferences {
            revision: MAX_REVISION,
            ..Preferences::default()
        };
        db.execute(
            "INSERT INTO settings VALUES(?1,?2)",
            params![SETTINGS_KEY, serde_json::to_string(&exhausted).unwrap()],
        )
        .unwrap();
        assert_eq!(
            reset_overrides(&db, MAX_REVISION),
            Err(Error::RevisionExhausted)
        );
        assert_eq!(read(&db).unwrap(), exhausted);
    }
}
