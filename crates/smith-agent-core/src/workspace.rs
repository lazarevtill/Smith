use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};

/// A restart binding, not a grant. The host must explicitly reopen and grant it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceIdentity {
    pub name: String,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: EntryKind,
}

pub(crate) const MAX_SEARCH_BYTES: usize = 4 * 1024 * 1024;

pub(crate) enum EditExpectation {
    Contents(String),
    Sha256(String),
}

pub(crate) struct EditRequest {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    pub expected: EditExpectation,
}

#[derive(Serialize)]
pub(crate) struct EditResult {
    pub path: String,
    pub replaced: usize,
    pub bytes_written: usize,
    pub before_sha256: String,
    pub after_sha256: String,
}

#[derive(Serialize)]
pub(crate) struct SearchResult {
    pub matches: Vec<String>,
    pub scanned_files: usize,
    pub bytes_read: usize,
    pub truncated: bool,
    pub skipped: Vec<SearchSkip>,
}

#[derive(Serialize)]
pub(crate) struct SearchSkip {
    pub path: String,
    pub reason: Error,
}

#[cfg(unix)]
struct FileSnapshot {
    contents: String,
    stat: fs::Stat,
}

pub(crate) fn sha256(contents: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in Sha256::digest(contents.as_bytes()) {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 15) as usize] as char);
    }
    output
}

/// A private, host-provisioned temporary workspace. Only direct child files are
/// supported in this first adapter. No arbitrary host-folder grant API exists.
/// The directory persists across process exit; the host owns its cleanup policy.
pub struct Workspace {
    root: File,
    path: PathBuf,
    identity: WorkspaceIdentity,
}

impl Workspace {
    #[cfg(unix)]
    pub fn create_temporary() -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::Builder::new()
            .prefix("smith-agent-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let name = temp
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| Error::UnsafePath("temporary workspace name".into()))?
            .to_owned();
        let root = open_root(&name)?;
        let stat = fs::fstat(&root)?;
        let identity = WorkspaceIdentity {
            name,
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        };
        let path = temp.keep();
        Ok(Self {
            root,
            path,
            identity,
        })
    }

    #[cfg(not(unix))]
    pub fn create_temporary() -> Result<Self> {
        Err(Error::Unsupported(
            "confined filesystem adapter requires Unix; Windows adapter pending".into(),
        ))
    }

    /// Trusted-host restart operation. Resolves only the recorded temporary
    /// basename and verifies the pinned directory's device/inode identity.
    #[cfg(unix)]
    pub fn reopen_temporary(identity: &WorkspaceIdentity) -> Result<Self> {
        validate_name(&identity.name)?;
        if !identity.name.starts_with("smith-agent-") {
            return Err(Error::UnsafePath("not an agent temporary workspace".into()));
        }
        let root = open_root(&identity.name)?;
        let stat = fs::fstat(&root)?;
        if stat.st_dev as u64 != identity.device || stat.st_ino as u64 != identity.inode {
            return Err(Error::Denied("temporary workspace was replaced".into()));
        }
        Ok(Self {
            root,
            path: std::env::temp_dir().join(&identity.name),
            identity: identity.clone(),
        })
    }

    #[cfg(not(unix))]
    pub fn reopen_temporary(_: &WorkspaceIdentity) -> Result<Self> {
        Err(Error::Unsupported(
            "confined filesystem adapter requires Unix".into(),
        ))
    }

    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }
    /// Host administration only; never used to resolve a tool's file arguments.
    pub fn host_path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    pub(crate) fn read(&self, path: &str, max_bytes: usize) -> Result<String> {
        self.read_snapshot(path, max_bytes)
            .0
            .map(|snapshot| snapshot.contents)
    }

    #[cfg(unix)]
    fn read_snapshot(&self, path: &str, max_bytes: usize) -> (Result<FileSnapshot>, usize) {
        let mut bytes_read = 0;
        let result = (|| {
            validate_name(path)?;
            let fd = fs::openat(
                &self.root,
                path,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )?;
            let before = fs::fstat(&fd)?;
            validate_regular(&before)?;
            let file = File::from(fd);
            let mut bytes = Vec::new();
            let read = (&file).take(max_bytes as u64 + 1).read_to_end(&mut bytes);
            bytes_read = bytes.len();
            read?;
            if bytes.len() > max_bytes {
                return Err(Error::LimitExceeded("file read bytes".into()));
            }
            let after = fs::fstat(&file)?;
            validate_regular(&after)?;
            if !same_version(&before, &after) {
                return Err(Error::Conflict);
            }
            let contents = String::from_utf8(bytes)
                .map_err(|_| Error::InvalidInput("file is not UTF-8".into()))?;
            Ok(FileSnapshot {
                contents,
                stat: after,
            })
        })();
        (result, bytes_read)
    }

    #[cfg(unix)]
    pub(crate) fn search(
        &self,
        query: &str,
        max_results: usize,
        max_bytes: usize,
        max_file_bytes: usize,
        max_entries: usize,
        check_active: &mut dyn FnMut() -> Result<()>,
    ) -> Result<SearchResult> {
        let entries = self.list(".", max_entries)?;
        let mut result = SearchResult {
            matches: vec![],
            scanned_files: 0,
            bytes_read: 0,
            truncated: false,
            skipped: vec![],
        };
        for entry in entries {
            check_active()?;
            if entry.kind != EntryKind::File {
                result.skipped.push(SearchSkip {
                    path: entry.name,
                    reason: Error::UnsafePath(
                        "search only reads direct-child regular files".into(),
                    ),
                });
                continue;
            }
            let remaining = max_bytes - result.bytes_read;
            if remaining == 0 {
                result.truncated = true;
                break;
            }
            // Reserve the extra byte used to detect an oversized/growing file;
            // failed reads also count toward the aggregate I/O budget.
            let per_file_limit = max_file_bytes.min(remaining - 1);
            let (snapshot, bytes_read) = self.read_snapshot(&entry.name, per_file_limit);
            result.bytes_read += bytes_read;
            result.scanned_files += 1;
            match snapshot {
                Ok(snapshot) if snapshot.contents.contains(query) => {
                    if result.matches.len() == max_results {
                        result.truncated = true;
                        break;
                    }
                    result.matches.push(entry.name);
                }
                Ok(_) => (),
                Err(error) => {
                    if matches!(error, Error::LimitExceeded(_)) {
                        result.truncated = true;
                    }
                    result.skipped.push(SearchSkip {
                        path: entry.name,
                        reason: error,
                    });
                }
            }
        }
        Ok(result)
    }

    #[cfg(unix)]
    pub(crate) fn edit_atomic(
        &self,
        request: &EditRequest,
        max_bytes: usize,
        check_active: &mut dyn FnMut() -> Result<()>,
    ) -> Result<EditResult> {
        validate_name(&request.path)?;
        let _lock = self.lock_writes()?;
        check_active()?;
        let original = self.read_snapshot(&request.path, max_bytes).0?;
        let before_sha256 = sha256(&original.contents);
        let matches_expected = match &request.expected {
            EditExpectation::Contents(expected) => expected == &original.contents,
            EditExpectation::Sha256(expected) => expected == &before_sha256,
        };
        if !matches_expected {
            return Err(Error::Conflict);
        }
        let start = original
            .contents
            .find(&request.old_string)
            .ok_or_else(|| Error::InvalidInput("old_string was not found".into()))?;
        // Advance one character, not the full match, to reject overlapping
        // alternatives too (e.g. replacing "aa" inside "aaa" is ambiguous).
        let next = start
            + original.contents[start..]
                .chars()
                .next()
                .expect("nonempty old_string")
                .len_utf8();
        if original.contents[next..].contains(&request.old_string) {
            return Err(Error::InvalidInput(
                "old_string must occur exactly once".into(),
            ));
        }
        let updated_len = original
            .contents
            .len()
            .checked_sub(request.old_string.len())
            .and_then(|n| n.checked_add(request.new_string.len()))
            .ok_or_else(|| Error::LimitExceeded("edited file bytes".into()))?;
        if updated_len > max_bytes {
            return Err(Error::LimitExceeded("edited file bytes".into()));
        }
        let mut updated = String::with_capacity(updated_len);
        updated.push_str(&original.contents[..start]);
        updated.push_str(&request.new_string);
        updated.push_str(&original.contents[start + request.old_string.len()..]);
        let after_sha256 = sha256(&updated);
        self.replace_staged(&request.path, &updated, &mut || {
            // A cancellation/persistence failure or a competing replacement
            // seen before rename leaves the complete existing file untouched.
            check_active()?;
            let current = self.read_snapshot(&request.path, max_bytes).0?;
            if !same_version(&original.stat, &current.stat) || current.contents != original.contents
            {
                return Err(Error::Conflict);
            }
            Ok(())
        })?;
        Ok(EditResult {
            path: request.path.clone(),
            replaced: 1,
            bytes_written: updated.len(),
            before_sha256,
            after_sha256,
        })
    }

    #[cfg(unix)]
    pub(crate) fn list(&self, path: &str, max_entries: usize) -> Result<Vec<DirectoryEntry>> {
        if path != "." {
            return Err(Error::UnsafePath(
                "only the workspace root can be listed".into(),
            ));
        }
        let mut entries = Vec::new();
        let mut scanned = 0;
        for entry in fs::Dir::read_from(&self.root)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| Error::InvalidInput("directory entry is not UTF-8".into()))?;
            if name == "." || name == ".." {
                continue;
            }
            scanned += 1;
            if scanned > max_entries {
                return Err(Error::LimitExceeded("directory entries".into()));
            }
            if name.starts_with(".smith-") {
                continue;
            }
            let kind = match entry.file_type() {
                FileType::RegularFile => EntryKind::File,
                FileType::Directory => EntryKind::Directory,
                FileType::Symlink => EntryKind::Symlink,
                _ => EntryKind::Other,
            };
            entries.push(DirectoryEntry {
                name: name.into(),
                kind,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    #[cfg(unix)]
    pub(crate) fn write_atomic(
        &self,
        path: &str,
        contents: &str,
        max_bytes: usize,
        max_entries: usize,
    ) -> Result<()> {
        validate_name(path)?;
        crate::error::bounded_text(contents, max_bytes, "file write bytes")?;
        // Independent open description + nonblocking lock serialize cooperating
        // writers across threads and reopened workspaces without an unbounded
        // wait. This also makes the file-count quota effective under concurrency.
        let _write_lock = self.lock_writes()?;
        match fs::statat(&self.root, path, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => validate_regular(&stat)?,
            Err(rustix::io::Errno::NOENT) => {
                if self.list(".", max_entries)?.len() >= max_entries {
                    return Err(Error::LimitExceeded("workspace file count".into()));
                }
            }
            Err(e) => return Err(e.into()),
        }
        self.replace_staged(path, contents, &mut || Ok(()))
    }

    #[cfg(unix)]
    fn lock_writes(&self) -> Result<File> {
        let write_lock = fs::openat(
            &self.root,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        fs::flock(&write_lock, fs::FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                Error::Conflict
            } else {
                e.into()
            }
        })?;
        Ok(File::from(write_lock))
    }

    #[cfg(unix)]
    fn replace_staged(
        &self,
        path: &str,
        contents: &str,
        before_replace: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let scratch = format!(".smith-write-{}", uuid::Uuid::new_v4());
        let fd = fs::openat(
            &self.root,
            scratch.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let result = (|| {
            let mut file = File::from(fd);
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            before_replace()?;
            // Rename replaces a raced-in final symlink itself; it never follows
            // that symlink. Both names are resolved by this same pinned root fd.
            fs::renameat(&self.root, scratch.as_str(), &self.root, path)?;
            self.root.sync_all().map_err(|error| {
                Error::UncertainSideEffect(format!(
                    "replacement occurred but directory durability failed: {error}"
                ))
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::unlinkat(&self.root, scratch.as_str(), AtFlags::empty());
        }
        result
    }

    #[cfg(not(unix))]
    pub(crate) fn read(&self, _: &str, _: usize) -> Result<String> {
        Err(Error::Unsupported("filesystem adapter".into()))
    }
    #[cfg(not(unix))]
    pub(crate) fn list(&self, _: &str, _: usize) -> Result<Vec<DirectoryEntry>> {
        Err(Error::Unsupported("filesystem adapter".into()))
    }
    #[cfg(not(unix))]
    pub(crate) fn write_atomic(&self, _: &str, _: &str, _: usize, _: usize) -> Result<()> {
        Err(Error::Unsupported("filesystem adapter".into()))
    }
    #[cfg(not(unix))]
    pub(crate) fn search(
        &self,
        _: &str,
        _: usize,
        _: usize,
        _: usize,
        _: usize,
        _: &mut dyn FnMut() -> Result<()>,
    ) -> Result<SearchResult> {
        Err(Error::Unsupported("filesystem adapter".into()))
    }
    #[cfg(not(unix))]
    pub(crate) fn edit_atomic(
        &self,
        _: &EditRequest,
        _: usize,
        _: &mut dyn FnMut() -> Result<()>,
    ) -> Result<EditResult> {
        Err(Error::Unsupported("filesystem adapter".into()))
    }
}

#[cfg(unix)]
fn same_version(left: &fs::Stat, right: &fs::Stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
        && left.st_mode == right.st_mode
        && left.st_nlink == right.st_nlink
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 200
        || name == "."
        || name == ".."
        || name.starts_with(".smith-")
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
    {
        return Err(Error::UnsafePath(
            "expected a direct-child filename, no traversal, separators, or reserved prefix".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_root(name: &str) -> Result<File> {
    validate_name(name)?;
    let temp_root = File::open(std::env::temp_dir())?;
    let root = fs::openat(
        &temp_root,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = fs::fstat(&root)?;
    if stat.st_mode & 0o777 != 0o700 || stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(Error::Denied("workspace must be private (0700)".into()));
    }
    Ok(File::from(root))
}

#[cfg(unix)]
fn validate_regular(stat: &fs::Stat) -> Result<()> {
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(Error::UnsafePath(
            "expected a regular file with exactly one hard link".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
impl From<rustix::io::Errno> for Error {
    fn from(error: rustix::io::Errno) -> Self {
        if error == rustix::io::Errno::LOOP {
            Self::UnsafePath("symlink refused".into())
        } else {
            Self::Io(error.to_string())
        }
    }
}
