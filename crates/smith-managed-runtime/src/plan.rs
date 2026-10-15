use crate::{Budgets, Cancellation, Failure, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio::io::AsyncReadExt;

/// Provenance is a user-reviewed immutable revision/identifier, never a URL to fetch.
#[derive(Clone, Debug, Serialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub source: String,
    pub source_revision: String,
}
#[derive(Clone, Copy, Debug)]
pub enum ArtifactKind {
    Executable,
    Model,
}

/// Hash an explicitly selected existing file. The source fields are declarations,
/// not a signature, publisher verification, or permission to execute that file.
pub async fn inspect_selected_artifact(
    path: &Path,
    kind: ArtifactKind,
    source: String,
    source_revision: String,
    cancel: &Cancellation,
) -> Result<Artifact> {
    let mut artifact = Artifact {
        path: path.to_path_buf(),
        sha256: "0".repeat(64),
        source,
        source_revision,
    };
    inspect_inner(
        &mut artifact,
        matches!(kind, ArtifactKind::Executable),
        false,
        cancel,
    )
    .await?;
    Ok(artifact)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Native,
    CompatibleFixture,
}
#[derive(Clone, Debug, Serialize)]
pub struct Manifest {
    pub model_id: String,
    pub executable: Artifact,
    pub model: Artifact,
    pub protocol: Protocol,
    pub execution_kind: ExecutionKind,
    pub declares_text_chat: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Requires --host *.sock, --offline, --no-agent, --no-webui, --no-mmproj,
    /// --fit off, fixed INFO/plain logging, /health, /v1/models, and text-only
    /// /v1/chat/completions. Task requests additionally require response_format
    /// with the fixed task JSON schema; there is no unconstrained retry.
    LlamaCppUnixHttpV1,
    /// Existing TCP-only versions cannot establish endpoint ownership here.
    LegacyTcp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendRequest {
    Cpu,
    /// Exact reviewed local device name, not a device probe or fallback list.
    Device {
        name: String,
        gpu_layers: u32,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct LoadConfig {
    pub backend: BackendRequest,
    pub context_tokens: u32,
    pub threads: u16,
    pub budgets: Budgets,
}
impl Default for LoadConfig {
    fn default() -> Self {
        Self {
            backend: BackendRequest::Cpu,
            context_tokens: 4096,
            threads: 4,
            budgets: Budgets::default(),
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct PlanSummary {
    pub id: String,
    pub manifest: Manifest,
    pub config: LoadConfig,
    pub transport: &'static str,
    pub cancellation_policy: &'static str,
    pub advanced: Option<crate::AdvancedSettings>,
    pub advanced_argv: Vec<String>,
    /// Fixed source-reviewed INFO logging, included in the approval hash.
    /// Raw retention still freezes before Ready; subsequent output is discarded.
    pub startup_logging_argv: Vec<String>,
}
#[derive(Clone)]
pub struct LoadPlan {
    pub(crate) summary: PlanSummary,
    stamps: [Stamp; 2],
}
/// Constructible only by approving the exact inspected plan identifier.
/// This is an in-process trust boundary; never deserialize it from HTTP input.
#[derive(Clone)]
pub struct ApprovedPlan(pub(crate) LoadPlan);
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl LoadPlan {
    /// Hashes exactly the selected two files. No executable is run or discovered.
    pub async fn prepare(
        manifest: Manifest,
        config: LoadConfig,
        cancel: &Cancellation,
    ) -> Result<Self> {
        Self::prepare_inner(manifest, config, None, Vec::new(), cancel).await
    }
    pub async fn prepare_with_options(
        manifest: Manifest,
        config: LoadConfig,
        advanced: crate::AdvancedSettings,
        compatibility: &crate::RuntimeCompatibility,
        cancel: &Cancellation,
    ) -> Result<Self> {
        if !compatibility.protocol_candidate() {
            return Err(Failure::UnsupportedProtocol);
        }
        let argv = compatibility.arguments(&manifest.executable, &advanced)?;
        Self::prepare_inner(manifest, config, Some(advanced), argv, cancel).await
    }
    async fn prepare_inner(
        mut manifest: Manifest,
        config: LoadConfig,
        advanced: Option<crate::AdvancedSettings>,
        advanced_argv: Vec<String>,
        cancel: &Cancellation,
    ) -> Result<Self> {
        validate(&manifest, &config)?;
        let first = inspect(&mut manifest.executable, true, cancel).await?;
        let second = inspect(&mut manifest.model, false, cancel).await?;
        if manifest.execution_kind == ExecutionKind::Native {
            let model_path = manifest.model.path.clone();
            tokio::task::spawn_blocking(move || crate::inspect_gguf(&model_path))
                .await
                .map_err(|_| Failure::Io)?
                .map_err(|_| Failure::InvalidManifest)?;
            if cancel.is_cancelled() {
                return Err(Failure::Cancelled);
            }
            if stamp(
                &tokio::fs::metadata(&manifest.model.path)
                    .await
                    .map_err(|_| Failure::ArtifactChanged)?,
            )? != second
            {
                return Err(Failure::ArtifactChanged);
            }
        }
        let mut summary = PlanSummary {
            id: String::new(),
            manifest,
            config,
            transport: "private_unix_socket_with_kernel_peer_pid",
            cancellation_policy: "terminate_and_reap_owned_child",
            advanced,
            advanced_argv,
            startup_logging_argv: [
                "--verbosity",
                "3",
                "--log-colors",
                "off",
                "--no-log-prefix",
                "--no-log-timestamps",
                "--no-log-jsonl",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        };
        summary.id = hex(&Sha256::digest(
            serde_json::to_vec(&summary).map_err(|_| Failure::InvalidManifest)?,
        ));
        Ok(Self {
            summary,
            stamps: [first, second],
        })
    }
    pub fn summary(&self) -> &PlanSummary {
        &self.summary
    }
    /// Call only after a trusted local control surface records explicit consent
    /// to source, paths, hashes, config, process launch and cancellation policy.
    pub fn approve(self, reviewed_plan_id: &str) -> Result<ApprovedPlan> {
        if self.summary.id != reviewed_plan_id {
            return Err(Failure::ConsentMismatch);
        }
        Ok(ApprovedPlan(self))
    }
}
impl ApprovedPlan {
    pub fn summary(&self) -> &PlanSummary {
        &self.0.summary
    }
    pub(crate) async fn revalidate(&self, cancel: &Cancellation) -> Result<()> {
        let p = &self.0;
        let mut executable = p.summary.manifest.executable.clone();
        let mut model = p.summary.manifest.model.clone();
        let stamps = [
            inspect(&mut executable, true, cancel).await?,
            inspect(&mut model, false, cancel).await?,
        ];
        if executable.path != p.summary.manifest.executable.path
            || model.path != p.summary.manifest.model.path
            || stamps != p.stamps
        {
            return Err(Failure::ArtifactChanged);
        }
        Ok(())
    }
    pub(crate) fn unchanged(&self) -> Result<()> {
        let p = &self.0;
        let paths = [
            &p.summary.manifest.executable.path,
            &p.summary.manifest.model.path,
        ];
        for (path, expected) in paths.into_iter().zip(&p.stamps) {
            if std::fs::canonicalize(path).map_err(|_| Failure::ArtifactChanged)? != *path
                || stamp(&std::fs::metadata(path).map_err(|_| Failure::ArtifactChanged)?)?
                    != *expected
            {
                return Err(Failure::ArtifactChanged);
            }
        }
        Ok(())
    }
}
fn validate(manifest: &Manifest, c: &LoadConfig) -> Result<()> {
    let id = &manifest.model_id;
    if id.is_empty()
        || id.len() > 160
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
    {
        return Err(Failure::InvalidManifest);
    }
    if c.context_tokens < 128 || c.context_tokens > 1_048_576 || c.threads == 0 || c.threads > 512 {
        return Err(Failure::InvalidConfig);
    }
    for d in [c.budgets.startup, c.budgets.generation, c.budgets.reap] {
        if d.is_zero() || d.as_secs() > 3600 {
            return Err(Failure::InvalidConfig);
        }
    }
    if let BackendRequest::Device { name, gpu_layers } = &c.backend {
        if *gpu_layers == 0
            || *gpu_layers > 10000
            || name.is_empty()
            || name.len() > 80
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(Failure::InvalidConfig);
        }
    }
    Ok(())
}
pub(crate) fn stamp(meta: &std::fs::Metadata) -> Result<Stamp> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    if !meta.is_file() {
        return Err(Failure::InvalidManifest);
    }
    Ok(Stamp {
        len: meta.len(),
        modified: meta.modified().map_err(|_| Failure::Io)?,
        #[cfg(unix)]
        dev: meta.dev(),
        #[cfg(unix)]
        ino: meta.ino(),
        #[cfg(unix)]
        changed: (meta.ctime(), meta.ctime_nsec()),
    })
}
pub(crate) async fn inspect(
    artifact: &mut Artifact,
    executable: bool,
    cancel: &Cancellation,
) -> Result<Stamp> {
    inspect_inner(artifact, executable, true, cancel).await
}
async fn inspect_inner(
    artifact: &mut Artifact,
    executable: bool,
    verify: bool,
    cancel: &Cancellation,
) -> Result<Stamp> {
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    if !artifact.path.is_absolute()
        || artifact.source.trim().is_empty()
        || artifact.source_revision.trim().is_empty()
        || artifact.source.len() > 2048
        || artifact.source_revision.len() > 256
        || artifact.sha256.len() != 64
        || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Failure::InvalidManifest);
    }
    artifact.path = tokio::fs::canonicalize(&artifact.path)
        .await
        .map_err(|_| Failure::InvalidManifest)?;
    // Reject special files before opening: opening a FIFO for read can block
    // before any cancellable read future exists. Nonblocking/no-follow flags
    // also prevent a final-component replacement from recreating that trap.
    let selected = tokio::fs::metadata(&artifact.path)
        .await
        .map_err(|_| Failure::InvalidManifest)?;
    stamp(&selected)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let mut file = options
        .open(&artifact.path)
        .await
        .map_err(|_| Failure::InvalidManifest)?;
    let metadata = file.metadata().await.map_err(|_| Failure::Io)?;
    let before = stamp(&metadata)?;
    if before.len == 0
        || before.len
            > if executable {
                512 * 1024 * 1024
            } else {
                1024 * 1024 * 1024 * 1024
            }
    {
        return Err(Failure::InvalidManifest);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if executable && metadata.permissions().mode() & 0o111 == 0 {
            return Err(Failure::InvalidManifest);
        }
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut first = true;
    loop {
        let n = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Failure::Cancelled),
            n = file.read(&mut buffer) => n.map_err(|_| Failure::Io)?,
        };
        if n == 0 {
            break;
        }
        if first {
            first = false;
            let native = n >= 4
                && matches!(
                    &buffer[..4],
                    b"\x7fELF"
                        | b"\xcf\xfa\xed\xfe"
                        | b"\xce\xfa\xed\xfe"
                        | b"\xfe\xed\xfa\xcf"
                        | b"\xca\xfe\xba\xbe"
                        | b"\xca\xfe\xba\xbf"
                );
            // A selected runtime is native code, never a script/interpreter command.
            if executable && !native {
                return Err(Failure::InvalidManifest);
            }
            if !executable && (n < 4 || &buffer[..4] != b"GGUF") {
                return Err(Failure::InvalidManifest);
            }
        }
        digest.update(&buffer[..n]);
    }
    if first
        || before != stamp(&file.metadata().await.map_err(|_| Failure::Io)?)?
        || before
            != stamp(
                &tokio::fs::metadata(&artifact.path)
                    .await
                    .map_err(|_| Failure::Io)?,
            )?
    {
        return Err(Failure::ArtifactChanged);
    }
    let computed = hex(&digest.finalize());
    if verify && computed != artifact.sha256.to_ascii_lowercase() {
        return Err(Failure::ArtifactChanged);
    }
    artifact.sha256 = computed;
    Ok(before)
}

pub(crate) fn platform_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}
pub(crate) fn socket_directory() -> Result<tempfile::TempDir> {
    // Explicit permissions are required: tempfile directories otherwise inherit
    // the process umask. The short path avoids Darwin's sockaddr_un limit.
    let mut builder = tempfile::Builder::new();
    builder.prefix("smith-native-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder
        .tempdir_in(Path::new("/tmp"))
        .map_err(|_| Failure::Io)
}

#[cfg(all(test, unix))]
mod directory_tests {
    #[test]
    fn runtime_and_probe_directory_is_created_private() {
        use std::os::unix::fs::MetadataExt;
        let directory = super::socket_directory().unwrap();
        let metadata = std::fs::metadata(directory.path()).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
