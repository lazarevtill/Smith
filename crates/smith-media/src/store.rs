use crate::{
    error, inspect, validate_request, AttachmentRequest, ErrorKind, Limits, Manifest, Result,
    StoredAttachment,
};
use rustix::fs::{self as fd, AtFlags, Mode, OFlags, RenameFlags};
use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::AsFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path},
};
use uuid::Uuid;

const MAX_MANIFEST_BYTES: usize = 512 * 1024;

/// A capability to an application-owned directory. The root must already exist and
/// be private (mode 0700). Ancestors of the explicitly selected root are trusted;
/// all paths beneath it are traversed via directory descriptors with NOFOLLOW.
/// No APIs open paths supplied in metadata or return unchecked payload paths.
pub struct MediaStore {
    staging: File,
    attachments: File,
    limits: Limits,
}

impl MediaStore {
    pub fn open(root: impl AsRef<Path>, limits: Limits) -> Result<Self> {
        limits.validate()?;
        let root = open_root(root.as_ref())?;
        require_private(&root)?;
        let staging = create_private_dir(&root, ".staging")?;
        let attachments = create_private_dir(&root, "attachments")?;
        root.sync_all()?;
        Ok(Self {
            staging,
            attachments,
            limits,
        })
    }

    /// Validate all bytes before starting staging. Original bytes and display metadata
    /// survive unchanged. Two identical inputs have equal SHA-256, independent record IDs.
    pub fn ingest_bytes(
        &self,
        request: AttachmentRequest,
        bytes: &[u8],
    ) -> Result<StoredAttachment> {
        let manifest = inspect(request, bytes, &self.limits)?;
        let id = Uuid::new_v4().to_string();
        let manifest_bytes = serde_json::to_vec(&manifest)
            .map_err(|_| error(ErrorKind::InvalidContent, "manifest serialization failed"))?;
        if manifest_bytes.len() > MAX_MANIFEST_BYTES {
            return Err(error(
                ErrorKind::LimitExceeded,
                "manifest exceeds storage limit",
            ));
        }
        // mkdirat is exclusive: a collision fails instead of reusing an existing stage.
        fd::mkdirat(
            &self.staging,
            id.as_str(),
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
        )
        .map_err(io_error)?;
        let stage = match open_dir(&self.staging, &id) {
            Ok(stage) => stage,
            Err(e) => {
                let _ = fd::unlinkat(&self.staging, id.as_str(), AtFlags::REMOVEDIR);
                return Err(e);
            }
        };
        let mut cleanup = StagingGuard {
            parent: &self.staging,
            dir: &stage,
            id: &id,
            promoted: false,
        };
        write_new(&stage, "original", bytes)?;
        write_new(&stage, "manifest.json", &manifest_bytes)?;
        stage.sync_all()?;
        // Exclusive atomic directory promotion: readers see both files or no record.
        fd::renameat_with(
            &self.staging,
            id.as_str(),
            &self.attachments,
            id.as_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(io_error)?;
        cleanup.promoted = true;
        // A durability error here is returned as I/O; a complete record may exist. No
        // partially published record can exist. Caller must not assume exactly-once.
        self.attachments.sync_all()?;
        self.staging.sync_all()?;
        drop(cleanup);
        Ok(StoredAttachment { id, manifest })
    }

    /// Bounded reader, e.g. an already-owned upload buffer. Caller controls I/O deadlines;
    /// this method reads at most max_input_bytes + 1 bytes, including unknown-length input.
    pub fn ingest_reader(
        &self,
        request: AttachmentRequest,
        reader: impl Read,
    ) -> Result<StoredAttachment> {
        validate_request(&request)?;
        let bytes = read_bounded(reader, self.limits.max_input_bytes)?;
        self.ingest_bytes(request, &bytes)
    }

    /// Generate a table and publish it through exactly the same validated, private,
    /// atomic store as uploaded attachments. The request's format must agree.
    pub fn generate_table(
        &self,
        request: AttachmentRequest,
        table: &crate::Table,
        format: crate::ExportFormat,
    ) -> Result<StoredAttachment> {
        let requested = validate_request(&request)?;
        let expected = match format {
            crate::ExportFormat::Csv => crate::Format::Csv,
            crate::ExportFormat::Markdown => crate::Format::Markdown,
            crate::ExportFormat::Xlsx => crate::Format::Xlsx,
            crate::ExportFormat::Pdf => crate::Format::Pdf,
            crate::ExportFormat::Pptx => crate::Format::Pptx,
        };
        if requested != expected {
            return Err(error(
                ErrorKind::MimeMismatch,
                "generated format does not match requested filename",
            ));
        }
        let generated = crate::export_table(table, format, &self.limits)?;
        self.ingest_bytes(request, &generated.bytes)
    }

    /// Generate a multi-sheet XLSX and atomically publish original bytes + manifest.
    pub fn generate_workbook(
        &self,
        request: AttachmentRequest,
        workbook: &crate::Workbook,
    ) -> Result<StoredAttachment> {
        if validate_request(&request)? != crate::Format::Xlsx {
            return Err(error(
                ErrorKind::MimeMismatch,
                "workbook export requires XLSX filename and MIME",
            ));
        }
        let bytes = crate::write_workbook(workbook, &self.limits)?;
        self.ingest_bytes(request, &bytes)
    }

    /// Copy a regular, singly-linked file beneath an explicitly authorized source root.
    /// Paths must be normal relative components. Rejects symlinks, hard links, devices,
    /// FIFOs, sockets, absolute paths, dot components and alternate Windows separators.
    pub fn ingest_file(
        &self,
        source_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
        request: AttachmentRequest,
    ) -> Result<StoredAttachment> {
        validate_request(&request)?;
        let root = open_root(source_root.as_ref())?;
        let file = open_confined(&root, relative.as_ref())?;
        self.ingest_reader(request, file)
    }

    /// Read and verify a manifest plus its payload. Revalidation detects corrupted
    /// hashes, MIME, derived content, sizes and image metadata, not just JSON syntax.
    pub fn read(&self, id: &str) -> Result<StoredAttachment> {
        let (manifest, _) = self.read_verified(id)?;
        Ok(StoredAttachment {
            id: id.to_owned(),
            manifest,
        })
    }

    /// Return original bytes only after revalidating the stored manifest and content.
    pub fn read_bytes(&self, id: &str) -> Result<Vec<u8>> {
        self.read_verified(id).map(|(_, bytes)| bytes)
    }

    fn read_verified(&self, id: &str) -> Result<(Manifest, Vec<u8>)> {
        if Uuid::parse_str(id).is_err()
            || id.len() != 36
            || Uuid::parse_str(id).unwrap().to_string() != id
        {
            return Err(error(ErrorKind::UnsafePath, "invalid attachment ID"));
        }
        let dir = open_dir(&self.attachments, id)?;
        require_private(&dir)?;
        let manifest_bytes =
            read_bounded(open_regular(&dir, "manifest.json")?, MAX_MANIFEST_BYTES)?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| error(ErrorKind::Integrity, "stored manifest is invalid"))?;
        let bytes = read_bounded(open_regular(&dir, "original")?, self.limits.max_input_bytes)?;
        let checked = inspect(manifest.request.clone(), &bytes, &self.limits).map_err(|_| {
            error(
                ErrorKind::Integrity,
                "stored attachment failed content validation",
            )
        })?;
        if checked != manifest {
            return Err(error(
                ErrorKind::Integrity,
                "stored attachment failed manifest integrity check",
            ));
        }
        Ok((manifest, bytes))
    }
}

fn io_error(_: rustix::io::Errno) -> crate::Error {
    error(ErrorKind::Io, "confined attachment I/O failed")
}
fn path_error(_: rustix::io::Errno) -> crate::Error {
    error(
        ErrorKind::UnsafePath,
        "attachment path is unavailable or unsafe",
    )
}

fn open_root(path: &Path) -> Result<File> {
    // O_NOFOLLOW applies to the final component. A trailing slash or dot can
    // make the selected directory symlink an ancestor instead, notably on macOS.
    // Inspect raw spelling: Path::components() normalizes away these distinctions.
    let raw = path.as_os_str().as_bytes();
    let components = raw.strip_prefix(b"/").unwrap_or(raw);
    if raw.is_empty()
        || (raw != b"/"
            && components
                .split(|&b| b == b'/')
                .any(|part| part.is_empty() || part == b"." || part == b".."))
    {
        return Err(error(
            ErrorKind::UnsafePath,
            "authorized root has ambiguous path components",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC).bits() as i32)
        .open(path)
        .map_err(|_| {
            error(
                ErrorKind::UnsafePath,
                "authorized root is unavailable or unsafe",
            )
        })?;
    Ok(file)
}
fn require_private(file: &File) -> Result<()> {
    if file.metadata()?.mode() & 0o077 != 0 {
        return Err(error(
            ErrorKind::UnsafePath,
            "managed attachment directories must be private (0700)",
        ));
    }
    Ok(())
}
fn open_dir(parent: impl AsFd, name: &str) -> Result<File> {
    fd::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(path_error)
}
fn create_private_dir(parent: &File, name: &str) -> Result<File> {
    match fd::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => (),
        Err(e) => return Err(io_error(e)),
    }
    let file = open_dir(parent, name)?;
    require_private(&file)?;
    Ok(file)
}
fn open_regular(parent: impl AsFd, name: &str) -> Result<File> {
    // NONBLOCK prevents an attacker-controlled FIFO from stalling before fstat rejects it.
    let file = fd::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(path_error)?;
    let stat = file.metadata()?;
    if !stat.is_file() || stat.nlink() != 1 {
        return Err(error(
            ErrorKind::UnsafePath,
            "attachment must be a regular singly-linked file",
        ));
    }
    Ok(file)
}
fn open_confined(root: &File, path: &Path) -> Result<File> {
    let string = path
        .to_str()
        .ok_or_else(|| error(ErrorKind::UnsafePath, "non-UTF-8 attachment path"))?;
    if string.is_empty()
        || string.len() > 4096
        || string.contains(['\\', ':'])
        || string
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(error(
            ErrorKind::UnsafePath,
            "attachment path must use normal relative components",
        ));
    }
    let components: Vec<_> = path.components().collect();
    if components.len() > 64
        || components
            .iter()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(error(
            ErrorKind::UnsafePath,
            "attachment path must be bounded and relative",
        ));
    }
    let mut dir = root.try_clone()?;
    for part in components.iter().take(components.len() - 1) {
        dir = open_dir(&dir, part.as_os_str().to_str().unwrap())?;
    }
    open_regular(
        &dir,
        components.last().unwrap().as_os_str().to_str().unwrap(),
    )
}
fn write_new(dir: &File, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = File::from(
        fd::openat(
            dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(io_error)?,
    );
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn read_bounded(reader: impl Read, max_bytes: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(error(
            ErrorKind::LimitExceeded,
            "attachment exceeds byte limit",
        ));
    }
    Ok(bytes)
}

struct StagingGuard<'a> {
    parent: &'a File,
    dir: &'a File,
    id: &'a str,
    promoted: bool,
}
impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        if !self.promoted {
            let _ = fd::unlinkat(self.dir, "original", AtFlags::empty());
            let _ = fd::unlinkat(self.dir, "manifest.json", AtFlags::empty());
            let _ = fd::unlinkat(self.parent, self.id, AtFlags::REMOVEDIR);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn failed_atomic_promotion_cleans_its_stage() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = MediaStore::open(root.path(), Limits::default()).unwrap();
        // Deterministic destination-failure injection, including when tests run as root.
        // A regular-file descriptor cannot be the directory target of renameat.
        store.attachments = File::create(root.path().join("unavailable-destination")).unwrap();
        let result = store.ingest_bytes(AttachmentRequest::new("notes.txt", None), b"valid text");
        assert_eq!(result.unwrap_err().kind, ErrorKind::Io);
        assert_eq!(
            std::fs::read_dir(root.path().join("attachments"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(root.path().join(".staging"))
                .unwrap()
                .count(),
            0
        );
    }
}
