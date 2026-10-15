use crate::{
    plan::{hex, stamp},
    *,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) const RESOURCES: [(&str, u64); 6] = [
    ("mlx.metallib", 64 * 1024 * 1024),
    ("mlx-swift_Cmlx.bundle/Contents/Info.plist", 64 * 1024),
    (
        "mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib",
        64 * 1024 * 1024,
    ),
    (
        "swift-transformers_Hub.bundle/gpt2_tokenizer_config.json",
        1024 * 1024,
    ),
    (
        "swift-transformers_Hub.bundle/t5_tokenizer_config.json",
        1024 * 1024,
    ),
    (
        "swift-crypto_Crypto.bundle/PrivacyInfo.xcprivacy",
        64 * 1024,
    ),
];

#[cfg(unix)]
fn open(path: &Path, directory: bool) -> Result<std::fs::File> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };
    if !path.is_absolute() {
        return Err(Failure::InvalidSelection);
    }
    let n = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if n < 0 {
        return Err(Failure::Io);
    }
    let mut fd = unsafe { OwnedFd::from_raw_fd(n) };
    let parts: Vec<_> = path.components().skip(1).collect();
    for (i, part) in parts.iter().enumerate() {
        let std::path::Component::Normal(name) = part else {
            return Err(Failure::InvalidSelection);
        };
        let name = CString::new(name.as_encoded_bytes()).map_err(|_| Failure::InvalidSelection)?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory || i + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        let next = unsafe { libc::openat(fd.as_raw_fd(), name.as_ptr(), flags) };
        if next < 0 {
            return Err(Failure::InvalidSelection);
        }
        fd = unsafe { OwnedFd::from_raw_fd(next) };
    }
    Ok(std::fs::File::from(fd))
}
#[cfg(not(unix))]
fn open(_path: &Path, _directory: bool) -> Result<std::fs::File> {
    Err(Failure::UnsupportedPlatform)
}

async fn hash(path: &Path, cap: u64, cancel: &Cancellation) -> Result<(u64, String)> {
    let file = open(path, false)?;
    let metadata = file.metadata().map_err(|_| Failure::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Failure::InvalidSelection);
        }
    }
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > cap {
        return Err(Failure::InvalidSelection);
    }
    let initial = stamp(&metadata)?;
    let mut file = tokio::fs::File::from_std(file);
    let mut bytes = 0u64;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = tokio::select! {biased;_ = cancel.cancelled()=>return Err(Failure::Cancelled),n=file.read(&mut buffer)=>n.map_err(|_|Failure::Io)?};
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > cap {
            return Err(Failure::SelectionChanged);
        }
        digest.update(&buffer[..n]);
    }
    if bytes != metadata.len()
        || initial != stamp(&file.metadata().await.map_err(|_| Failure::Io)?)?
    {
        return Err(Failure::SelectionChanged);
    }
    Ok((bytes, hex(&digest.finalize())))
}
pub(crate) async fn inspect(worker: &Path, cancel: &Cancellation) -> Result<Vec<FileRecord>> {
    let parent = worker.parent().ok_or(Failure::InvalidSelection)?;
    let mut files = Vec::new();
    for (name, cap) in RESOURCES {
        let (bytes, sha256) = hash(&parent.join(name), cap, cancel).await?;
        files.push(FileRecord {
            name: name.into(),
            bytes,
            sha256,
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let library = files.iter().find(|f| f.name == "mlx.metallib").unwrap();
    let bundle = files
        .iter()
        .find(|f| f.name.ends_with("default.metallib"))
        .unwrap();
    if library.sha256 != bundle.sha256 {
        return Err(Failure::InvalidSelection);
    }
    Ok(files)
}

pub(crate) async fn snapshot(
    plan: &LaunchPlan,
    scratch: &Path,
    cancel: &Cancellation,
) -> Result<PathBuf> {
    if plan.summary.execution_kind == ExecutionKind::CompatibleFixture {
        return Ok(plan.summary.worker.path.clone());
    }
    let root = scratch.join("runtime");
    std::fs::create_dir(&root).map_err(|_| Failure::Io)?;
    let mut dirs = BTreeSet::from([root.clone()]);
    for record in &plan.summary.runtime_files {
        let source = if record.name == "smith-mlx-worker" {
            plan.summary.worker.path.clone()
        } else {
            plan.summary
                .worker
                .path
                .parent()
                .ok_or(Failure::InvalidSelection)?
                .join(&record.name)
        };
        let destination = root.join(&record.name);
        let parent = destination.parent().ok_or(Failure::Io)?;
        std::fs::create_dir_all(parent).map_err(|_| Failure::Io)?;
        let mut next = parent;
        while next.starts_with(&root) {
            dirs.insert(next.to_path_buf());
            let Some(p) = next.parent() else { break };
            next = p;
        }
        copy(&source, &destination, record, cancel).await?;
    }
    for dir in dirs.iter().rev() {
        set_mode(dir, 0o500)?;
    }
    Ok(root.join("smith-mlx-worker"))
}
async fn copy(
    source: &Path,
    destination: &Path,
    record: &FileRecord,
    cancel: &Cancellation,
) -> Result<()> {
    let source = open(source, false)?;
    let metadata = source.metadata().map_err(|_| Failure::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Failure::InvalidSelection);
        }
    }
    if !metadata.is_file() || metadata.len() != record.bytes {
        return Err(Failure::SelectionChanged);
    }
    let initial = stamp(&metadata)?;
    let mut input = tokio::fs::File::from_std(source);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut output = tokio::fs::File::from_std(options.open(destination).map_err(|_| Failure::Io)?);
    let mut buffer = vec![0; 1024 * 1024];
    let mut bytes = 0u64;
    let mut digest = Sha256::new();
    loop {
        let n = tokio::select! {biased;_ = cancel.cancelled()=>return Err(Failure::Cancelled),n=input.read(&mut buffer)=>n.map_err(|_|Failure::Io)?};
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > record.bytes {
            return Err(Failure::SelectionChanged);
        }
        digest.update(&buffer[..n]);
        output
            .write_all(&buffer[..n])
            .await
            .map_err(|_| Failure::Io)?;
    }
    output.flush().await.map_err(|_| Failure::Io)?;
    if bytes != record.bytes
        || hex(&digest.finalize()) != record.sha256
        || initial != stamp(&input.metadata().await.map_err(|_| Failure::Io)?)?
    {
        return Err(Failure::SelectionChanged);
    }
    drop(output);
    set_mode(
        destination,
        if record.name == "smith-mlx-worker" {
            0o500
        } else {
            0o400
        },
    )?;
    let copied = hash(destination, record.bytes, cancel).await?;
    if copied != (record.bytes, record.sha256.clone()) {
        return Err(Failure::SelectionChanged);
    }
    Ok(())
}
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|_| Failure::Io)
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Err(Failure::UnsupportedPlatform)
}

// Only the known read-only runtime directories are made removable. Unknown
// worker-created trees are not traversed or granted additional permissions.
pub(crate) fn make_removable(scratch: &Path) -> Result<()> {
    let root = scratch.join("runtime");
    if !root.exists() {
        return Ok(());
    }
    let mut dirs = BTreeSet::from([root.clone()]);
    for (resource, _) in RESOURCES {
        let path = root.join(resource);
        let mut parent = path.parent();
        while let Some(p) = parent {
            if !p.starts_with(&root) {
                break;
            }
            dirs.insert(p.to_path_buf());
            parent = p.parent();
        }
    }
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        let file = open(&dir, true)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o700) } != 0 {
                return Err(Failure::Io);
            }
        }
    }
    Ok(())
}
