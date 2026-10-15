//! Explicitly app-managed local key custody. This relies on OS account access,
//! not a user password or keychain. Existing/manual profiles are never opened.
use crate::{files, require, Error, Result, Secret, DB_FILE};
use std::{
    fs,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;
pub const LOCAL_DIRECTORY: &str = "local-workspace";
pub const LOCAL_KEY_FILE: &str = "managed.key";

/// Create a separate owned key/database pair atomically, or validate its presence.
/// An incomplete existing directory is never repaired by creating a new key.
pub fn prepare_local_workspace(parent: &Path) -> Result<PathBuf> {
    let parent = files::safe_root(parent)?;
    let destination = parent.join(LOCAL_DIRECTORY);
    if fs::symlink_metadata(&destination).is_ok() {
        let _ = local_workspace_secret(&destination)?;
        require(crate::probe(&destination.join(DB_FILE))?==crate::ProfileKind::EncryptedCandidate,
            "The local encrypted workspace is missing or damaged. Existing files were retained; no key or database was regenerated.")?;
        return Ok(destination);
    }
    let stage = files::private_temp(".local-workspace-", Some(&parent))?;
    // UUIDv4 uses OS randomness; two independent values provide 244 random bits.
    let key = Secret::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ))?;
    files::write_new(&stage.path().join(LOCAL_KEY_FILE), key.0.as_bytes())?;
    let db = crate::create(&stage.path().join(DB_FILE), &key)?;
    crate::close(db).map_err(|failure| failure.into_parts().1)?;
    files::sync_tree(stage.path())?;
    files::rename_exclusive(stage.path(), &destination)?;
    files::sync_parent(&destination).map_err(|_|Error("The local workspace was published, but durability could not be confirmed. Retain it and verify before retrying."))?;
    Ok(destination)
}

/// Read the existing app-managed key without exposing or serializing it.
/// Key loss, broad permissions, aliases and partial creation fail closed.
pub fn local_workspace_secret(root: &Path) -> Result<Secret> {
    let root = files::safe_root(root)?;
    let path = root.join(LOCAL_KEY_FILE);
    let meta=fs::symlink_metadata(&path).map_err(|_|Error("The local workspace key is unavailable. Existing encrypted data was retained; no replacement key was generated."))?;
    require(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() == 64,
        "The local workspace key is invalid; existing data was retained.",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        require(meta.nlink()==1&&meta.uid()==unsafe{libc::geteuid()}&&files::mode(&meta)==0o600&&files::mode(&fs::metadata(&root)?)==0o700,"The local workspace requires an owned private directory and mode-0600 key file; existing data was retained.")?;
    }
    let bytes = files::read_file(&path)?;
    require(
        bytes.len() == 64 && bytes.iter().all(u8::is_ascii_hexdigit),
        "The local workspace key is malformed; existing data was retained.",
    )?;
    let text = Zeroizing::new(
        String::from_utf8(bytes.to_vec())
            .map_err(|_| Error("The local workspace key is malformed."))?,
    );
    Secret::new(text.to_string())
}
