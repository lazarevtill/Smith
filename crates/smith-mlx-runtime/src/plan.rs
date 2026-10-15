use crate::*;
use sha2::{Digest, Sha256};
use smith_managed_runtime::{inspect_selected_artifact, Artifact, ArtifactKind};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};
use tokio::io::AsyncReadExt;

#[derive(Clone, Debug)]
pub struct SelectedWorker {
    pub path: PathBuf,
    pub source: String,
    pub source_revision: String,
    pub execution_kind: ExecutionKind,
}
#[derive(Clone, Debug, Serialize)]
pub struct ModelSelection {
    pub root: PathBuf,
    pub files: Vec<FileRecord>,
    pub package_sha256: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct PlanSummary {
    pub id: String,
    pub worker: Artifact,
    pub runtime_files: Vec<FileRecord>,
    pub runtime_bundle_sha256: String,
    pub model: ModelSelection,
    pub budgets: Budgets,
    pub backend: &'static str,
    pub execution_kind: ExecutionKind,
    pub execution_notice: &'static str,
}
pub struct LaunchPlan {
    pub(crate) summary: PlanSummary,
    worker_stamp: Stamp,
    model_stamp: Stamp,
    prepared: Instant,
}
pub struct ApprovedPlan(pub(crate) LaunchPlan);
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
pub(crate) fn stamp(m: &std::fs::Metadata) -> Result<Stamp> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Stamp {
        len: m.len(),
        modified: m.modified().map_err(|_| Failure::Io)?,
        #[cfg(unix)]
        identity: (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()),
    })
}
fn map_artifact(e: smith_managed_runtime::Failure) -> Failure {
    if e == smith_managed_runtime::Failure::Cancelled {
        Failure::Cancelled
    } else {
        Failure::InvalidSelection
    }
}
impl LaunchPlan {
    pub async fn prepare(
        worker: SelectedWorker,
        model_root: &Path,
        budgets: Budgets,
        cancel: &Cancellation,
    ) -> Result<Self> {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            return Err(Failure::UnsupportedPlatform);
        }
        for (value, max) in [
            (budgets.hello, 10),
            (budgets.operation, 180),
            (budgets.generation, 120),
            (budgets.drain, 5),
            (budgets.reap, 5),
        ] {
            if value.is_zero() || value > Duration::from_secs(max) {
                return Err(Failure::InvalidRequest);
            }
        }
        if worker.execution_kind == ExecutionKind::Native && !cfg!(target_os = "macos") {
            return Err(Failure::UnsupportedPlatform);
        }
        let execution_kind = worker.execution_kind;
        let worker = inspect_selected_artifact(
            &worker.path,
            ArtifactKind::Executable,
            worker.source,
            worker.source_revision,
            cancel,
        )
        .await
        .map_err(map_artifact)?;
        let worker_stamp =
            stamp(&std::fs::metadata(&worker.path).map_err(|_| Failure::InvalidSelection)?)?;
        let mut runtime_files = vec![FileRecord {
            name: "smith-mlx-worker".into(),
            bytes: worker_stamp.len,
            sha256: worker.sha256.clone(),
        }];
        if execution_kind == ExecutionKind::Native {
            runtime_files.extend(crate::bundle::inspect(&worker.path, cancel).await?);
        }
        runtime_files.sort_by(|a, b| a.name.cmp(&b.name));
        let runtime_bundle_sha256 = package_hash(&runtime_files);
        let (model, model_stamp) = inspect_model(model_root, cancel).await?;
        let mut summary=PlanSummary{id:String::new(),worker,runtime_files,runtime_bundle_sha256,model,budgets,backend:"mlx_swift_metal_only",execution_kind,execution_notice:"Native mode snapshots the selected worker and six required colocated resources into private read-only files before fixed JSONL execution. Fixture mode executes the explicit synthetic worker in place. No Python, network configuration, or backend fallback is supplied. Hashes identify local bytes, not publisher trust or system-library certification."};
        summary.id = hex(&Sha256::digest(
            serde_json::to_vec(&(&summary, uuid::Uuid::new_v4())).map_err(|_| Failure::Protocol)?,
        ));
        Ok(Self {
            summary,
            worker_stamp,
            model_stamp,
            prepared: Instant::now(),
        })
    }
    pub fn summary(&self) -> &PlanSummary {
        &self.summary
    }
    pub fn approve(self, exact_id: &str) -> Result<ApprovedPlan> {
        if self.summary.id != exact_id || self.prepared.elapsed() > Duration::from_secs(300) {
            Err(Failure::ConsentMismatch)
        } else {
            Ok(ApprovedPlan(self))
        }
    }
    pub(crate) async fn revalidate(&self, cancel: &Cancellation) -> Result<()> {
        if self.prepared.elapsed() > Duration::from_secs(300) {
            return Err(Failure::ConsentMismatch);
        }
        let w = &self.summary.worker;
        let current = inspect_selected_artifact(
            &w.path,
            ArtifactKind::Executable,
            w.source.clone(),
            w.source_revision.clone(),
            cancel,
        )
        .await
        .map_err(map_artifact)?;
        let (model, stamp_model) = inspect_model(&self.summary.model.root, cancel).await?;
        if self.summary.execution_kind == ExecutionKind::Native {
            let resources = crate::bundle::inspect(&w.path, cancel).await?;
            let expected: Vec<_> = self
                .summary
                .runtime_files
                .iter()
                .filter(|f| f.name != "smith-mlx-worker")
                .cloned()
                .collect();
            if resources != expected {
                return Err(Failure::SelectionChanged);
            }
        }
        if current.sha256 != w.sha256
            || current.path != w.path
            || self.worker_stamp
                != stamp(&std::fs::metadata(&w.path).map_err(|_| Failure::SelectionChanged)?)?
            || stamp_model != self.model_stamp
            || model.files != self.summary.model.files
            || model.package_sha256 != self.summary.model.package_sha256
        {
            return Err(Failure::SelectionChanged);
        }
        self.unchanged_worker()
    }
    pub(crate) fn unchanged_worker(&self) -> Result<()> {
        if self.prepared.elapsed() > Duration::from_secs(300) {
            return Err(Failure::ConsentMismatch);
        }
        if self.worker_stamp
            != stamp(
                &std::fs::metadata(&self.summary.worker.path)
                    .map_err(|_| Failure::SelectionChanged)?,
            )?
        {
            return Err(Failure::SelectionChanged);
        }
        Ok(())
    }
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn package_hash(files: &[FileRecord]) -> String {
    let mut h = Sha256::new();
    for f in files {
        h.update(format!("{}\0{}\0{}\n", f.name, f.bytes, f.sha256).as_bytes());
    }
    hex(&h.finalize())
}

#[cfg(unix)]
async fn inspect_model(root: &Path, cancel: &Cancellation) -> Result<(ModelSelection, Stamp)> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::fs::MetadataExt,
        },
    };
    let raw = root.to_str().ok_or(Failure::InvalidSelection)?;
    if !root.is_absolute()
        || raw.len() > 4096
        || raw
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
        || std::fs::canonicalize(root).map_err(|_| Failure::InvalidSelection)? != root
    {
        return Err(Failure::InvalidSelection);
    }
    let mut fd = unsafe {
        OwnedFd::from_raw_fd({
            let n = libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            if n < 0 {
                return Err(Failure::Io);
            }
            n
        })
    };
    for part in root.components().skip(1) {
        let std::path::Component::Normal(part) = part else {
            return Err(Failure::InvalidSelection);
        };
        let name = CString::new(part.as_encoded_bytes()).map_err(|_| Failure::InvalidSelection)?;
        let next = unsafe {
            libc::openat(
                fd.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 {
            return Err(Failure::InvalidSelection);
        }
        fd = unsafe { OwnedFd::from_raw_fd(next) };
    }
    let dir = std::fs::File::from(fd);
    let before = stamp(&dir.metadata().map_err(|_| Failure::Io)?)?;
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|_| Failure::Io)? {
        if names.len() >= 128 {
            return Err(Failure::InvalidSelection);
        }
        names.push(
            entry
                .map_err(|_| Failure::Io)?
                .file_name()
                .into_string()
                .map_err(|_| Failure::InvalidSelection)?,
        )
    }
    names.sort();
    let mut files = Vec::new();
    let mut total = 0u64;
    for name in names {
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if name.is_empty() || name.len() > 200 || name.contains(['/', '\\', '\0']) {
            return Err(Failure::InvalidSelection);
        }
        let ignored = name.ends_with(".md")
            || ["LICENSE", "LICENSE.txt", ".gitattributes", ".DS_Store"].contains(&name.as_str());
        let weight = name.ends_with(".safetensors");
        if !ignored
            && !weight
            && ![
                "config.json",
                "tokenizer.json",
                "tokenizer_config.json",
                "generation_config.json",
                "model.safetensors.index.json",
                "special_tokens_map.json",
                "added_tokens.json",
                "chat_template.jinja",
            ]
            .contains(&name.as_str())
        {
            return Err(Failure::InvalidSelection);
        }
        let c = CString::new(name.as_str()).map_err(|_| Failure::InvalidSelection)?;
        let n = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if n < 0 {
            return Err(Failure::InvalidSelection);
        }
        let file = unsafe { std::fs::File::from_raw_fd(n) };
        let metadata = file.metadata().map_err(|_| Failure::Io)?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(Failure::InvalidSelection);
        }
        if ignored {
            continue;
        }
        let cap = if weight {
            16 * 1024 * 1024 * 1024
        } else if name == "tokenizer.json" {
            32 * 1024 * 1024
        } else {
            1024 * 1024
        };
        total = total
            .checked_add(metadata.len())
            .ok_or(Failure::InvalidSelection)?;
        if metadata.len() > cap || total > 16 * 1024 * 1024 * 1024 {
            return Err(Failure::InvalidSelection);
        }
        let initial = stamp(&metadata)?;
        let mut file = tokio::fs::File::from_std(file);
        let mut buffer = vec![0; 1024 * 1024];
        let mut hash = Sha256::new();
        let mut count = 0u64;
        loop {
            let read = tokio::select! {biased;_ = cancel.cancelled()=>return Err(Failure::Cancelled),n=file.read(&mut buffer)=>n.map_err(|_|Failure::Io)?};
            if read == 0 {
                break;
            }
            count += read as u64;
            if count > metadata.len() {
                return Err(Failure::SelectionChanged);
            }
            hash.update(&buffer[..read]);
        }
        if count != metadata.len()
            || initial != stamp(&file.metadata().await.map_err(|_| Failure::Io)?)?
        {
            return Err(Failure::SelectionChanged);
        }
        files.push(FileRecord {
            name,
            bytes: count,
            sha256: hex(&hash.finalize()),
        });
    }
    if before != stamp(&dir.metadata().map_err(|_| Failure::Io)?)?
        || before != stamp(&std::fs::metadata(root).map_err(|_| Failure::SelectionChanged)?)?
    {
        return Err(Failure::SelectionChanged);
    }
    if !["config.json", "tokenizer.json", "tokenizer_config.json"]
        .iter()
        .all(|name| files.iter().any(|f| f.name == *name))
        || !files.iter().any(|f| f.name.ends_with(".safetensors"))
    {
        return Err(Failure::InvalidSelection);
    }
    let package_sha256 = package_hash(&files);
    Ok((
        ModelSelection {
            root: root.to_path_buf(),
            files,
            package_sha256,
        },
        before,
    ))
}
#[cfg(not(unix))]
async fn inspect_model(_root: &Path, _cancel: &Cancellation) -> Result<(ModelSelection, Stamp)> {
    Err(Failure::UnsupportedPlatform)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn final_pre_spawn_check_rejects_a_plan_that_expired_after_hashing() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for name in [
            "config.json",
            "tokenizer.json",
            "tokenizer_config.json",
            "model.safetensors",
        ] {
            std::fs::write(root.join(name), b"synthetic").unwrap();
        }
        let mut plan = LaunchPlan::prepare(
            SelectedWorker {
                path: std::env::current_exe().unwrap(),
                source: "unit-test executable".into(),
                source_revision: "fixture".into(),
                execution_kind: ExecutionKind::CompatibleFixture,
            },
            &root,
            Budgets::default(),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        plan.revalidate(&Cancellation::new()).await.unwrap();
        plan.prepared = Instant::now() - Duration::from_secs(301);
        assert_eq!(plan.unchanged_worker(), Err(Failure::ConsentMismatch));
    }
}
