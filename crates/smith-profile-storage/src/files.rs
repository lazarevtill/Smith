use crate::{require, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use tempfile::{Builder, TempDir};
use zeroize::Zeroizing;

pub(crate) const LIMIT: u64 = 256 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FileRecord {
    pub bytes: u64,
    pub sha256: String,
    pub mode: u32,
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub(crate) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}
pub(crate) fn safe_root(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut cursor = PathBuf::new();
    for part in path.components() {
        require(
            !matches!(part, Component::ParentDir),
            "Parent traversal is not accepted.",
        )?;
        cursor.push(part);
        require(
            !fs::symlink_metadata(&cursor)?.file_type().is_symlink(),
            "Symlink profile paths are not accepted.",
        )?;
    }
    require(path.is_dir(), "Selected profile root must be a directory.")?;
    Ok(fs::canonicalize(path)?)
}
pub(crate) fn safe_relative(value: &str) -> Result<PathBuf> {
    require(
        !value.is_empty() && !value.contains(['\\', ':', '\0']),
        "Invalid preserved file path.",
    )?;
    let path = Path::new(value);
    require(
        path.components().all(|p| matches!(p, Component::Normal(_))),
        "Invalid preserved file path.",
    )?;
    Ok(path.to_owned())
}
pub(crate) fn mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        if meta.permissions().readonly() {
            0o400
        } else {
            0o600
        }
    }
}
pub(crate) fn read_file(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let meta = fs::symlink_metadata(path)?;
    require(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Only independent regular copied files are accepted.",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        require(
            meta.nlink() == 1,
            "Hard-linked files are not independent copies.",
        )?;
    }
    require(
        meta.len() <= LIMIT,
        "A profile file exceeds the 256 MiB limit.",
    )?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let mut bytes = Zeroizing::new(Vec::new());
    Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= LIMIT,
        "A profile file exceeds the 256 MiB limit.",
    )?;
    Ok(bytes)
}
fn regular_reader(path: &Path) -> Result<File> {
    let meta = fs::symlink_metadata(path)?;
    require(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Only independent regular profile files are accepted.",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        require(
            meta.nlink() == 1,
            "Hard-linked profile files are not accepted.",
        )?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}
pub(crate) fn read_header(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(512);
    regular_reader(path)?.take(512).read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn streaming_record(path: &Path) -> Result<FileRecord> {
    let mut file = regular_reader(path)?;
    let mut hash = Sha256::new();
    let mut buffer = Zeroizing::new(vec![0u8; 64 * 1024]);
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        bytes += n as u64;
    }
    Ok(FileRecord {
        bytes,
        sha256: hex(&hash.finalize()),
        mode: mode(&file.metadata()?),
    })
}
pub(crate) fn copy_encrypted_database(
    source: &Path,
    name: &str,
    stage: &Path,
    output: &str,
) -> Result<()> {
    let header = read_header(&source.join(name))?;
    require(
        header.len() >= 512 && !header.starts_with(b"SQLite format 3\0"),
        "Encrypted database header is invalid.",
    )?;
    require(
        !source.join(format!("{name}-journal")).try_exists()?,
        "A rollback journal requires offline recovery before unlocking.",
    )?;
    for suffix in ["", "-wal"] {
        let path = source.join(format!("{name}{suffix}"));
        if !path.try_exists()? {
            continue;
        }
        let mut input = regular_reader(&path)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut output = options.open(stage.join(format!("{output}{suffix}")))?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
    }
    Ok(())
}
pub(crate) fn record(path: &Path) -> Result<FileRecord> {
    let bytes = read_file(path)?;
    Ok(FileRecord {
        bytes: bytes.len() as u64,
        sha256: digest(&bytes),
        mode: mode(&fs::symlink_metadata(path)?),
    })
}
pub(crate) fn inventory(root: &Path) -> Result<BTreeMap<String, FileRecord>> {
    fn walk(
        root: &Path,
        path: &Path,
        out: &mut BTreeMap<String, FileRecord>,
        depth: usize,
        entries: &mut usize,
        bytes: &mut u64,
    ) -> Result<()> {
        require(
            depth <= 32,
            "Copied profile exceeds the 32-directory depth limit.",
        )?;
        for entry in fs::read_dir(path)? {
            *entries += 1;
            require(
                *entries <= 10_000,
                "Copied profile exceeds the 10000-entry limit.",
            )?;
            let path = entry?.path();
            let meta = fs::symlink_metadata(&path)?;
            require(
                !meta.file_type().is_symlink(),
                "Symlink entries are not accepted.",
            )?;
            if meta.is_dir() {
                walk(root, &path, out, depth + 1, entries, bytes)?;
            } else {
                *bytes = bytes
                    .checked_add(meta.len())
                    .ok_or(Error("Copied profile size overflow."))?;
                require(
                    *bytes <= LIMIT,
                    "Copied profile exceeds the 256 MiB cumulative archive limit.",
                )?;
                let rel = path
                    .strip_prefix(root)
                    .map_err(|_| Error("Invalid copied path."))?
                    .to_str()
                    .ok_or(Error("Non-UTF-8 copied path."))?
                    .to_string();
                safe_relative(&rel)?;
                let record = record(&path)?;
                require(
                    record.bytes == meta.len(),
                    "Copied file changed during inventory.",
                )?;
                out.insert(rel, record);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out, 0, &mut 0, &mut 0)?;
    Ok(out)
}
pub(crate) fn database_inventory(root: &Path, name: &str) -> Result<BTreeMap<String, FileRecord>> {
    let mut out = BTreeMap::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let name = format!("{name}{suffix}");
        let path = root.join(&name);
        if path.try_exists()? {
            out.insert(name, streaming_record(&path)?);
        }
    }
    Ok(out)
}
pub(crate) fn private_temp(prefix: &str, parent: Option<&Path>) -> Result<TempDir> {
    let mut builder = Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    Ok(match parent {
        Some(p) => builder.tempdir_in(p)?,
        None => builder.tempdir()?,
    })
}
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub(crate) fn copy_database(
    source: &Path,
    name: &str,
    stage: &Path,
    output: &str,
    plaintext: bool,
) -> Result<()> {
    let bytes = read_file(&source.join(name))?;
    require(
        bytes.len() >= 512 && bytes.starts_with(b"SQLite format 3\0") == plaintext,
        "Database format does not match the selected operation.",
    )?;
    require(
        !source.join(format!("{name}-journal")).try_exists()?,
        "A rollback journal requires offline recovery before migration.",
    )?;
    write_new(&stage.join(output), &bytes)?;
    let wal = source.join(format!("{name}-wal"));
    if wal.try_exists()? {
        write_new(&stage.join(format!("{output}-wal")), &read_file(&wal)?)?;
    }
    // SHM is a volatile index. Reconstruct only beside the private snapshot.
    Ok(())
}
pub(crate) fn destination(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let parent = safe_root(
        path.parent()
            .ok_or(Error("Destination needs an existing parent."))?,
    )?;
    let name = path.file_name().ok_or(Error("Invalid destination name."))?;
    let destination = parent.join(name);
    require(
        fs::symlink_metadata(&destination).is_err(),
        "Destination already exists; it will never be overwritten.",
    )?;
    Ok((parent, destination))
}
pub(crate) fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if path.is_dir() {
            sync_tree(&path)?;
        } else {
            File::open(&path)?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}
pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().ok_or(Error("Destination has no parent."))?).and_then(|p| p.sync_all())
        .map_err(|_| Error("Destination was published, but durability could not be confirmed; retain and verify it before retrying."))
}
pub(crate) fn rename_exclusive(from: &Path, to: &Path) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(from.as_os_str().as_bytes())
            .map_err(|_| Error("Invalid stage path."))?;
        let to = std::ffi::CString::new(to.as_os_str().as_bytes())
            .map_err(|_| Error("Invalid destination path."))?;
        #[cfg(target_os = "macos")]
        let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        let rc = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        require(
            rc == 0,
            "Atomic publication failed; existing destination was not replaced.",
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (from, to);
        Err(Error(
            "Atomic publication currently requires macOS or Linux.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn competing_publication_never_replaces_file_or_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for directory in [false, true] {
            let stage = root.join(if directory { "stage-dir" } else { "stage-file" });
            let target = root.join(if directory {
                "target-dir"
            } else {
                "target-file"
            });
            if directory {
                fs::create_dir(&stage).unwrap();
                fs::create_dir(&target).unwrap();
                write_new(&target.join("keep"), b"retained").unwrap();
            } else {
                write_new(&stage, b"new").unwrap();
                write_new(&target, b"retained").unwrap();
            }
            assert!(rename_exclusive(&stage, &target).is_err());
            assert!(stage.exists());
            assert_eq!(
                fs::read(if directory {
                    target.join("keep")
                } else {
                    target
                })
                .unwrap(),
                b"retained"
            );
        }
    }
}
