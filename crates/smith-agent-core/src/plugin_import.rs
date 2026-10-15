//! Explicit, bounded reads of one declarative plugin directory. No discovery,
//! execution, workspace grants, or interpretation of declared tool names.
#[cfg(unix)]
use crate::arguments_sha256;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use serde_json::json;
use std::path::Path;
#[cfg(unix)]
use std::path::{Component, PathBuf};

#[cfg(unix)]
const MANIFEST_LIMIT: usize = 32 * 1024;
#[cfg(unix)]
const INSTRUCTION_LIMIT: usize = 8192;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclarativePluginManifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub instructions_file: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
}
#[cfg(unix)]
impl DeclarativePluginManifest {
    fn validate(&self) -> Result<()> {
        label(&self.id, 128)?;
        label(&self.name, 80)?;
        bound(&self.description, 2048)?;
        bound(&self.version, 128)?;
        if self.authors.len() > 16 || self.tools.len() > 64 {
            return Err(Error::LimitExceeded("Plugin metadata entries".into()));
        }
        for item in self.authors.iter().chain(&self.tools) {
            label(item, 128)?;
        }
        for (text, max) in [
            (&self.homepage, 2048),
            (&self.license, 256),
            (&self.instructions_file, 512),
            (&self.instructions, INSTRUCTION_LIMIT),
        ] {
            if let Some(text) = text {
                bound(text, max)?;
            }
        }
        Ok(())
    }
}
#[cfg(unix)]
fn label(text: &str, max: usize) -> Result<()> {
    if text.trim().is_empty() || text.len() > max || text.chars().any(char::is_control) {
        return Err(Error::InvalidInput(
            "Invalid plugin name or metadata label".into(),
        ));
    }
    Ok(())
}
#[cfg(unix)]
fn bound(text: &str, max: usize) -> Result<()> {
    if text.len() > max {
        return Err(Error::LimitExceeded("Plugin text or metadata".into()));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginFileEvidence {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
    pub device: u64,
    pub inode: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSourceEvidence {
    pub directory_name: String,
    pub directory_device: u64,
    pub directory_inode: u64,
    /// Hash of the exact selected absolute path, never a publisher signature.
    pub selection_sha256: String,
    pub files: Vec<PluginFileEvidence>,
    pub source_sha256: String,
}

/// Host-created read custody; it cannot be deserialized from a request/model.
/// Only exact immutable captured bytes are exposed as resolved instructions.
pub struct SelectedPlugin {
    #[cfg(unix)]
    path: PathBuf,
    manifest: DeclarativePluginManifest,
    evidence: PluginSourceEvidence,
    #[cfg(unix)]
    root: std::fs::File,
    #[cfg(unix)]
    files: Vec<ReadFile>,
}
impl SelectedPlugin {
    pub fn manifest(&self) -> &DeclarativePluginManifest {
        &self.manifest
    }
    pub fn evidence(&self) -> &PluginSourceEvidence {
        &self.evidence
    }
    #[cfg(not(unix))]
    pub fn inspect(_directory: &Path) -> Result<Self> {
        Err(Error::Unsupported(
            "Selected plugin-directory custody is not implemented on this platform".into(),
        ))
    }
    #[cfg(not(unix))]
    pub fn revalidate(&self) -> Result<()> {
        Err(Error::Unsupported(
            "Selected plugin-directory custody is not implemented on this platform".into(),
        ))
    }
    #[cfg(unix)]
    pub fn inspect(directory: &Path) -> Result<Self> {
        let root = open_directory(directory)?;
        let root_stat = rustix::fs::fstat(&root)?;
        let raw = read_file(&root, "plugin.json", MANIFEST_LIMIT)?;
        let mut manifest: DeclarativePluginManifest =
            serde_json::from_str(&raw.text).map_err(|_| {
                Error::InvalidInput("plugin.json has invalid or unsupported fields".into())
            })?;
        manifest.validate()?;
        let mut files = vec![raw];
        // Preserve original precedence: an explicit inline value (including an
        // empty string) means instructions_file remains inert metadata.
        if manifest.instructions.is_none() {
            if let Some(name) = &manifest.instructions_file {
                child_name(name)?;
                if name == "plugin.json" {
                    return Err(Error::InvalidInput(
                        "The instruction file must differ from plugin.json".into(),
                    ));
                }
                let file = read_file(&root, name, INSTRUCTION_LIMIT)?;
                manifest.instructions = Some(file.text.clone());
                files.push(file);
            }
        }
        manifest.validate()?;
        let directory_name = directory
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::InvalidInput("Select a named UTF-8 plugin directory".into()))?
            .to_owned();
        label(&directory_name, 200)?;
        let mut evidence = PluginSourceEvidence {
            directory_name,
            directory_device: root_stat.st_dev as u64,
            directory_inode: root_stat.st_ino,
            selection_sha256: arguments_sha256(&json!({"selected_directory":directory})),
            files: files.iter().map(|file| file.evidence.clone()).collect(),
            source_sha256: String::new(),
        };
        evidence.source_sha256 = arguments_sha256(
            &json!({"format":"smith_selected_plugin_v1","source":evidence,"resolved_manifest":manifest}),
        );
        let selected = Self {
            path: directory.to_owned(),
            manifest,
            evidence,
            root,
            files,
        };
        // Detect replacement of the selection or either file during review.
        selected.revalidate()?;
        Ok(selected)
    }
    #[cfg(unix)]
    pub fn revalidate(&self) -> Result<()> {
        let root = open_directory(&self.path)?;
        if !same_directory(&root, &self.root)? {
            return Err(Error::Conflict);
        }
        for prior in &self.files {
            let limit = if prior.evidence.name == "plugin.json" {
                MANIFEST_LIMIT
            } else {
                INSTRUCTION_LIMIT
            };
            let current = read_file(&root, &prior.evidence.name, limit)?;
            if current.text != prior.text || !same_file(&current.stat, &prior.stat) {
                return Err(Error::Conflict);
            }
        }
        if !same_directory(&open_directory(&self.path)?, &self.root)? {
            return Err(Error::Conflict);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn child_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 200
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
    {
        return Err(Error::InvalidInput(
            "Plugin instruction file must be a direct-child filename".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
struct ReadFile {
    text: String,
    stat: rustix::fs::Stat,
    evidence: PluginFileEvidence,
}
#[cfg(unix)]
fn open_directory(path: &Path) -> Result<std::fs::File> {
    use rustix::fs::{self, Mode, OFlags};
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path.components().count() > 128
        || path.to_str().is_none()
    {
        return Err(Error::InvalidInput(
            "Select one absolute UTF-8 plugin directory".into(),
        ));
    }
    // Walking from an already-open root avoids following aliases in ancestors,
    // and every returned descriptor stays bound even if an ancestor is renamed.
    let mut current = std::fs::File::from(fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = std::fs::File::from(fs::openat(
                    &current,
                    name,
                    OFlags::RDONLY
                        | OFlags::DIRECTORY
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC
                        | OFlags::NONBLOCK,
                    Mode::empty(),
                )?);
            }
            _ => {
                return Err(Error::InvalidInput(
                    "Plugin directory cannot contain traversal components".into(),
                ))
            }
        }
    }
    Ok(current)
}
#[cfg(unix)]
fn same_directory(a: &std::fs::File, b: &std::fs::File) -> Result<bool> {
    let a = rustix::fs::fstat(a)?;
    let b = rustix::fs::fstat(b)?;
    Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
}
#[cfg(unix)]
fn same_file(a: &rustix::fs::Stat, b: &rustix::fs::Stat) -> bool {
    a.st_dev == b.st_dev
        && a.st_ino == b.st_ino
        && a.st_mode == b.st_mode
        && a.st_nlink == b.st_nlink
        && a.st_size == b.st_size
        && a.st_mtime == b.st_mtime
        && a.st_mtime_nsec == b.st_mtime_nsec
        && a.st_ctime == b.st_ctime
        && a.st_ctime_nsec == b.st_ctime_nsec
}
#[cfg(unix)]
fn read_file(root: &std::fs::File, name: &str, limit: usize) -> Result<ReadFile> {
    use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
    use std::io::Read;
    child_name(name)?;
    let mut file = std::fs::File::from(fs::openat(
        root,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let before = fs::fstat(&file)?;
    if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile || before.st_nlink != 1 {
        return Err(Error::InvalidInput(
            "Plugin files must be regular files with one hard link".into(),
        ));
    }
    if before.st_size < 0 || before.st_size as u64 > limit as u64 {
        return Err(Error::LimitExceeded("Selected plugin source file".into()));
    }
    let mut bytes = Vec::with_capacity(before.st_size as usize);
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(Error::LimitExceeded("Selected plugin source file".into()));
    }
    let after = fs::fstat(&file)?;
    let entry = fs::statat(root, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if bytes.len() as u64 != before.st_size as u64
        || !same_file(&before, &after)
        || !same_file(&after, &entry)
    {
        return Err(Error::Conflict);
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::InvalidInput("Plugin source must be valid UTF-8".into()))?;
    let sha256 = crate::workspace::sha256(&text);
    Ok(ReadFile {
        evidence: PluginFileEvidence {
            name: name.into(),
            bytes: text.len() as u64,
            sha256,
            device: before.st_dev as u64,
            inode: before.st_ino,
        },
        text,
        stat: before,
    })
}
