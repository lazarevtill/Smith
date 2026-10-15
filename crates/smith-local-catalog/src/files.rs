use crate::*;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// Cooperative cancellation and finite whole-inspection budget. A regular-file
/// OS read may itself block; expiry is checked before/after every bounded read.
/// The host must retain and drain the worker rather than dropping it as "done".
#[derive(Clone)]
pub struct ReadControl {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}
impl ReadControl {
    pub fn new(timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(Error::InvalidSelection);
        }
        Ok(Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + timeout,
        })
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    /// Check again while holding the host's cancellation/publication lock.
    /// This atomic flag alone does not serialize a host database transaction.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(Error::Deadline);
        }
        Ok(())
    }
}

/// Inspect only explicitly enumerated files. No directory listing, model parse,
/// executable invocation, network access, publisher verification or load occurs.
pub fn capture(request: CaptureRequest, control: &ReadControl) -> Result<CapturedAssets> {
    validate::request(&request)?;
    control.check()?;
    let result = inspect(&request, control)?;
    validate::manifest(&result)?;
    Ok(CapturedAssets(result))
}
pub(crate) fn revalidate(expected: &ManifestRecord, control: &ReadControl) -> Result<()> {
    validate::manifest(expected)?;
    let package = |p: &PackageRecord| PackageSelection {
        root: p.root.clone(),
        source: p.source.clone(),
        files: p
            .files
            .iter()
            .map(|f| SelectedFile {
                name: f.name.clone(),
                role: f.role,
            })
            .collect(),
    };
    let request = CaptureRequest {
        description: expected.description.clone(),
        runtime: package(&expected.runtime),
        model: package(&expected.model),
    };
    let actual = inspect(&request, control)?;
    if actual != *expected {
        return Err(Error::FileChanged);
    }
    Ok(())
}

#[cfg(not(unix))]
fn inspect(_request: &CaptureRequest, _control: &ReadControl) -> Result<ManifestRecord> {
    Err(Error::UnsupportedPlatform)
}

#[cfg(unix)]
mod unix {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{
        ffi::CString,
        fs::{File, Metadata},
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::fs::MetadataExt,
        },
        path::{Path, PathBuf},
    };

    #[derive(PartialEq, Eq)]
    struct Stamp {
        dev: u64,
        ino: u64,
        len: u64,
        modified: (i64, i64),
        changed: (i64, i64),
        mode: u32,
        nlink: u64,
    }
    fn stamp(m: &Metadata) -> Stamp {
        Stamp {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
            mode: m.mode(),
            nlink: m.nlink(),
        }
    }
    struct HeldFile {
        file: File,
        path: PathBuf,
        before: Stamp,
        role: FileRole,
    }
    fn open(path: &Path, role: FileRole) -> Result<File> {
        let raw = path.to_str().ok_or(Error::InvalidSelection)?;
        if !raw.starts_with('/') {
            return Err(Error::UnsupportedPlatform);
        }
        // Components were validated before joining; reject raw aliases again so
        // Path::components cannot erase a symlink followed by `.` or `..`.
        if raw
            .split('/')
            .skip(1)
            .any(|p| p.is_empty() || p == "." || p == "..")
        {
            return Err(Error::InvalidSelection);
        }
        let root = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if root < 0 {
            return Err(Error::Io);
        }
        let mut parent = unsafe { OwnedFd::from_raw_fd(root) };
        let parts: Vec<_> = raw.split('/').skip(1).collect();
        for (index, part) in parts.iter().enumerate() {
            let name = CString::new(*part).map_err(|_| Error::InvalidSelection)?;
            let last = index + 1 == parts.len();
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if last { 0 } else { libc::O_DIRECTORY };
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(Error::UnsafeFile);
            }
            parent = unsafe { OwnedFd::from_raw_fd(fd) };
        }
        let file = File::from(parent);
        let m = file.metadata().map_err(|_| Error::Io)?;
        if !m.is_file() || m.nlink() != 1 || (role == FileRole::Executable && m.mode() & 0o111 == 0)
        {
            return Err(Error::UnsafeFile);
        }
        Ok(file)
    }
    pub(super) fn inspect(
        request: &CaptureRequest,
        control: &ReadControl,
    ) -> Result<ManifestRecord> {
        control.check()?;
        let mut held = Vec::new();
        let mut identities = std::collections::BTreeSet::new();
        let mut total = 0u64;
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut package = |selection: &PackageSelection| -> Result<PackageRecord> {
            let mut selected = selection.files.clone();
            selected.sort_by(|a, b| a.name.cmp(&b.name));
            let mut records = Vec::new();
            for selected in selected {
                control.check()?;
                let path = selection.root.join(&selected.name);
                let mut file = open(&path, selected.role)?;
                let before = stamp(&file.metadata().map_err(|_| Error::Io)?);
                if !identities.insert((before.dev, before.ino)) {
                    return Err(Error::InvalidSelection);
                }
                if before.len == 0 || before.len > validate::role_cap(selected.role) {
                    return Err(Error::LimitExceeded);
                }
                total = total.checked_add(before.len).ok_or(Error::LimitExceeded)?;
                if total > MAX_SELECTED_BYTES {
                    return Err(Error::LimitExceeded);
                }
                let mut hash = Sha256::new();
                let mut read = 0u64;
                loop {
                    control.check()?;
                    let n = file.read(&mut buffer).map_err(|_| Error::Io)?;
                    control.check()?;
                    if n == 0 {
                        break;
                    }
                    read = read.checked_add(n as u64).ok_or(Error::LimitExceeded)?;
                    if read > before.len {
                        return Err(Error::FileChanged);
                    }
                    hash.update(&buffer[..n]);
                }
                if read != before.len || stamp(&file.metadata().map_err(|_| Error::Io)?) != before {
                    return Err(Error::FileChanged);
                }
                records.push(FileRecord {
                    name: selected.name,
                    role: selected.role,
                    bytes: read,
                    sha256: hex(&hash.finalize()),
                });
                held.push(HeldFile {
                    file,
                    path,
                    before,
                    role: selected.role,
                });
            }
            Ok(PackageRecord {
                root: selection.root.clone(),
                source: selection.source.clone(),
                completeness: Completeness::SelectedFilesOnly,
                files: records,
            })
        };
        let runtime = package(&request.runtime)?;
        let model = package(&request.model)?;
        // Catch changes to files inspected early while later files were hashed,
        // and path replacement after open. Keep exact descriptors until here.
        for item in &held {
            control.check()?;
            let reopened = open(&item.path, item.role)?;
            if stamp(&item.file.metadata().map_err(|_| Error::Io)?) != item.before
                || stamp(&reopened.metadata().map_err(|_| Error::Io)?) != item.before
            {
                return Err(Error::FileChanged);
            }
        }
        control.check()?;
        let mut manifest = ManifestRecord {
            description: request.description.clone(),
            runtime,
            model,
            sha256: String::new(),
        };
        manifest.sha256 = validate::manifest_hash(&manifest)?;
        Ok(manifest)
    }
}
#[cfg(unix)]
use unix::inspect;
