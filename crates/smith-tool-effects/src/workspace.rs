use crate::{
    hash,
    process::{self, ProcessSpec},
    Error, GitAction, PlanSummary, Result,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};
use tempfile::TempDir;

/// Explicit trusted-host file identity. No PATH search, auto-discovery, scripts,
/// environment inheritance or arbitrary runtime read grants.
pub struct ExecutableGrant {
    name: String,
    path: PathBuf,
    sha256: String,
    source: PathBuf,
    argv_prefix: Vec<String>,
}
impl ExecutableGrant {
    pub fn inspect(name: impl Into<String>, path: &Path) -> Result<Self> {
        Self::inspect_with_prefix(name, path, Vec::new())
    }
    /// Host-owned entry point selection for a reviewed native executable.
    /// The prefix is immutable authority, included in every reviewed plan;
    /// never populate it from a model/tool call or browser request.
    pub fn inspect_with_prefix(
        name: impl Into<String>,
        path: &Path,
        argv_prefix: Vec<String>,
    ) -> Result<Self> {
        if argv_prefix.len() > 16
            || argv_prefix.iter().map(String::len).sum::<usize>() > 4096
            || argv_prefix
                .iter()
                .any(|arg| arg.chars().any(char::is_control))
        {
            return Err(Error::Invalid("host executable argv prefix"));
        }
        let name = name.into();
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(Error::Invalid("executable name"));
        }
        let path = canonical(path)?;
        let bytes = bounded_file(&path, 128 * 1024 * 1024)?;
        let magic: [u8; 4] = bytes
            .get(..4)
            .ok_or(Error::Invalid("executable file"))?
            .try_into()
            .map_err(|_| Error::Invalid("executable magic"))?;
        if ![
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xbe, 0xba, 0xfe, 0xca],
            [0x7f, b'E', b'L', b'F'],
        ]
        .contains(&magic)
        {
            return Err(Error::Unsupported(
                "executable must be a native binary, not a shell/script",
            ));
        }
        let sha256 = hash(&bytes);
        Ok(Self {
            name,
            source: path.clone(),
            path,
            sha256,
            argv_prefix,
        })
    }
    pub fn summary(&self) -> Value {
        json!({"name":self.name,"path":self.source,"sha256":self.sha256,"argv_prefix":self.argv_prefix,"execution":"private immutable snapshot"})
    }
    fn verify(&self) -> Result<()> {
        if canonical(&self.path)? != self.path
            || hash(&bounded_file(&self.path, 128 * 1024 * 1024)?) != self.sha256
        {
            return Err(Error::Stale);
        }
        Ok(())
    }
    fn materialize(mut self, parent: &Path) -> Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let bytes = bounded_file(&self.path, 128 * 1024 * 1024)?;
        if hash(&bytes) != self.sha256 {
            return Err(Error::Stale);
        }
        let path = parent.join(&self.name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o500)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        file.set_permissions(fs::Permissions::from_mode(0o500))?;
        self.path = path;
        Ok(self)
    }
}
pub struct WorkspaceGrant {
    root: File,
    path: PathBuf,
    device: u64,
    inode: u64,
    _temporary: Option<TempDir>,
}
impl WorkspaceGrant {
    #[cfg(unix)]
    pub fn temporary() -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .prefix("smith-effects-")
            .tempdir()?;
        let path = fs::canonicalize(temp.path())?;
        let mut grant = Self::owned_temporary(&path, None)?;
        grant._temporary = Some(temp);
        Ok(grant)
    }
    #[cfg(not(unix))]
    pub fn temporary() -> Result<Self> {
        Err(Error::Unsupported("owned workspace requires Unix"))
    }
    /// Host-only adoption of a core-created temporary workspace. The path must
    /// be an existing direct child of the canonical OS temp directory. Never
    /// pass a browser-supplied path or reopen a repository from a user's home.
    #[cfg(unix)]
    pub fn owned_temporary(path: &Path, expected: Option<(u64, u64)>) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let path = canonical(path)?;
        let temp = fs::canonicalize(std::env::temp_dir())?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or(Error::Invalid("workspace name"))?;
        if path.parent() != Some(temp.as_path())
            || !(name.starts_with("smith-agent-") || name.starts_with("smith-effects-"))
        {
            return Err(Error::Denied("not an owned temporary workspace"));
        }
        let root = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(&path)?;
        let meta = root.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(Error::Denied("workspace ownership/permissions"));
        }
        if expected.is_some_and(|id| id != (meta.dev(), meta.ino())) {
            return Err(Error::Stale);
        }
        Ok(Self {
            root,
            path,
            device: meta.dev(),
            inode: meta.ino(),
            _temporary: None,
        })
    }
    #[cfg(not(unix))]
    pub fn owned_temporary(_: &Path, _: Option<(u64, u64)>) -> Result<Self> {
        Err(Error::Unsupported("owned workspace requires Unix"))
    }
    pub fn host_path(&self) -> &Path {
        &self.path
    }
    fn verify(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::symlink_metadata(&self.path)?;
            let pinned = self.root.metadata()?;
            if !meta.is_dir()
                || meta.dev() != self.device
                || meta.ino() != self.inode
                || pinned.ino() != meta.ino()
            {
                return Err(Error::Stale);
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(Error::Unsupported("workspace"))
        }
    }
}
pub(crate) struct WorkspaceBinding {
    workspace: WorkspaceGrant,
    private: TempDir,
    gitdir: PathBuf,
    executables: BTreeMap<String, ExecutableGrant>,
    git: Option<ExecutableGrant>,
}
impl WorkspaceBinding {
    pub fn new(
        workspace: WorkspaceGrant,
        executables: Vec<ExecutableGrant>,
        git: Option<ExecutableGrant>,
    ) -> Result<Self> {
        if executables.len() > 16 {
            return Err(Error::Limit("executables"));
        }
        let private = tempfile::Builder::new()
            .permissions({
                use std::os::unix::fs::PermissionsExt;
                fs::Permissions::from_mode(0o700)
            })
            .prefix("smith-effects-private-")
            .tempdir()?;
        let private_path = fs::canonicalize(private.path())?;
        for name in ["home", "hooks", "template", "executables", "git-executable"] {
            fs::create_dir(private_path.join(name))?;
        }
        let mut map = BTreeMap::new();
        for exe in executables {
            let exe = exe.materialize(&private_path.join("executables"))?;
            if map.insert(exe.name.clone(), exe).is_some() {
                return Err(Error::Invalid("duplicate executable name"));
            }
        }
        let git = git
            .map(|exe| exe.materialize(&private_path.join("git-executable")))
            .transpose()?;
        let binding = Self {
            workspace,
            gitdir: private_path.join("repo"),
            private,
            executables: map,
            git,
        };
        binding.digest()?;
        Ok(binding)
    }
    pub fn policy(&self) -> Value {
        json!({"kind":"owned_temporary_workspace","path":self.workspace.path,"device":self.workspace.device,"inode":self.workspace.inode,"executables":self.executables.values().map(ExecutableGrant::summary).collect::<Vec<_>>(),"git":self.git.as_ref().map(ExecutableGrant::summary),"sandbox":"macos-seatbelt-required","shell_expansion":false,"network":false,"fork":false,"commit_identity":{"name":"Smith scoped task","email":"smith@localhost"}})
    }
    pub fn digest(&self) -> Result<String> {
        self.workspace.verify()?;
        let mut entries = 0;
        let mut bytes = 0;
        let mut data = Vec::new();
        tree(
            &self.workspace.path,
            &self.workspace.path,
            &mut entries,
            &mut bytes,
            &mut data,
        )?;
        data.extend_from_slice(b"PRIVATE-GIT");
        if self.gitdir.exists() {
            tree(
                &self.gitdir,
                &self.gitdir,
                &mut entries,
                &mut bytes,
                &mut data,
            )?;
        }
        Ok(hash(&data))
    }
    pub fn verify_executables(&self) -> Result<()> {
        for exe in self.executables.values().chain(self.git.iter()) {
            exe.verify()?;
        }
        Ok(())
    }
    pub fn resolve_command(&self, name: &str) -> Result<String> {
        if self.executables.contains_key(name) {
            Ok(name.into())
        } else {
            Err(Error::Denied("command executable was not granted"))
        }
    }
    pub fn require_git(&self) -> Result<()> {
        self.git
            .as_ref()
            .ok_or(Error::Denied("git executable"))?
            .verify()
    }
    pub fn require_repository(&self) -> Result<()> {
        self.require_git()?;
        if !self.gitdir.join("HEAD").is_file() {
            return Err(Error::Denied(
                "initialize or clone an owned repository first",
            ));
        }
        Ok(())
    }
    pub fn shell(
        &self,
        executable: &str,
        argv: &[String],
        plan: &PlanSummary,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        let exe = self
            .executables
            .get(executable)
            .ok_or(Error::Denied("executable"))?;
        exe.verify()?;
        let mut arguments = exe.argv_prefix.clone();
        arguments.extend_from_slice(argv);
        let env = vec![
            (
                "HOME".into(),
                self.workspace.path.to_string_lossy().into_owned(),
            ),
            (
                "TMPDIR".into(),
                self.workspace.path.to_string_lossy().into_owned(),
            ),
        ];
        process::run(
            ProcessSpec {
                executable: &exe.path,
                argv: &arguments,
                cwd: &self.workspace.path,
                read: &[&self.workspace.path],
                write: &[&self.workspace.path],
                env: &env,
            },
            plan,
            check,
        )
    }
    fn config(&self) -> Result<()> {
        if !self.gitdir.exists() {
            return Ok(());
        }
        let hooks = self.private.path().join("hooks");
        let hooks = fs::canonicalize(hooks)?;
        let text=format!("[core]\n repositoryformatversion = 0\n bare = false\n filemode = false\n symlinks = false\n hooksPath = {}\n fsmonitor = false\n[gc]\n auto = 0\n[maintenance]\n auto = false\n[commit]\n gpgsign = false\n[tag]\n gpgsign = false\n[credential]\n helper =\n[protocol]\n allow = never\n[submodule]\n recurse = false\n",serde_json::to_string(hooks.to_str().ok_or(Error::Io)?).map_err(|_|Error::Io)?);
        fs::write(self.gitdir.join("config"), text)?;
        Ok(())
    }
    fn invoke(
        &self,
        args: Vec<String>,
        extra_read: &[&Path],
        plan: &PlanSummary,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        self.require_git()?;
        self.config()?;
        let git = self.git.as_ref().unwrap();
        let private = fs::canonicalize(self.private.path())?;
        let mut argv = git.argv_prefix.clone();
        argv.extend([
            format!("--git-dir={}", self.gitdir.display()),
            format!("--work-tree={}", self.workspace.path.display()),
            "--no-pager".into(),
            "-c".into(),
            "core.fsmonitor=false".into(),
            "-c".into(),
            "gc.auto=0".into(),
            "-c".into(),
            "maintenance.auto=false".into(),
        ]);
        argv.extend(args);
        let mut read = vec![self.workspace.path.as_path(), private.as_path()];
        read.extend_from_slice(extra_read);
        let env = vec![
            ("HOME".into(), private.join("home").display().to_string()),
            ("TMPDIR".into(), private.join("home").display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
            ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
            ("GIT_AUTHOR_NAME".into(), "Smith scoped task".into()),
            ("GIT_AUTHOR_EMAIL".into(), "smith@localhost".into()),
            ("GIT_COMMITTER_NAME".into(), "Smith scoped task".into()),
            ("GIT_COMMITTER_EMAIL".into(), "smith@localhost".into()),
        ];
        process::run(
            ProcessSpec {
                executable: &git.path,
                argv: &argv,
                cwd: &self.workspace.path,
                read: &read,
                write: &[&self.workspace.path, &self.gitdir, &private.join("home")],
                env: &env,
            },
            plan,
            check,
        )
    }
    pub fn git(
        &self,
        action: GitAction,
        plan: &PlanSummary,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        let args = match action {
            GitAction::Init => {
                if self.gitdir.exists() {
                    return Err(Error::Denied("repository already initialized"));
                }
                vec![
                    "init".into(),
                    "--quiet".into(),
                    format!(
                        "--template={}",
                        self.private.path().join("template").display()
                    ),
                ]
            }
            GitAction::Commit(message) => {
                let stage = self.invoke(
                    vec!["add".into(), "-A".into(), "--".into(), ".".into()],
                    &[],
                    plan,
                    check,
                )?;
                if stage["exit_code"] != 0 {
                    return Ok(stage);
                }
                vec![
                    "commit".into(),
                    "--no-gpg-sign".into(),
                    "--no-verify".into(),
                    "-m".into(),
                    message,
                ]
            }
            GitAction::Diff(path) => {
                let mut v = vec![
                    "diff".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                ];
                if let Some(path) = path {
                    v.extend(["--".into(), path]);
                }
                v
            }
            GitAction::Blame(path) => {
                vec!["blame".into(), "--no-textconv".into(), "--".into(), path]
            }
            GitAction::Status => vec![
                "status".into(),
                "--porcelain=v1".into(),
                "--untracked-files=all".into(),
            ],
            GitAction::Log(n) => vec![
                "log".into(),
                "--no-show-signature".into(),
                "--oneline".into(),
                "-n".into(),
                n.to_string(),
            ],
            GitAction::Clone { .. } => return Err(Error::Invalid("clone dispatch")),
        };
        self.invoke(args, &[], plan, check)
    }
    pub fn clone_from(
        &self,
        source: &Self,
        plan: &PlanSummary,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        source.require_repository()?;
        source.digest()?;
        if self.gitdir.exists() || fs::read_dir(&self.workspace.path)?.next().is_some() {
            return Err(Error::Denied("clone destination must be empty"));
        }
        // Copy only the reviewed owned repository snapshot using descriptor-
        // bounded reads. No Git transport/helper/shell is needed and no inode
        // is shared with the source. Checkout still uses real sandboxed Git.
        let mut snapshot = Vec::new();
        tree(
            &source.gitdir,
            &source.gitdir,
            &mut 0,
            &mut 0,
            &mut snapshot,
        )?;
        check()?;
        fs::create_dir(&self.gitdir)?;
        copy_snapshot(&snapshot, &self.gitdir, check)?;
        self.config()?;
        let mut result = self.invoke(
            vec!["reset".into(), "--hard".into(), "HEAD".into()],
            &[],
            plan,
            check,
        )?;
        result["clone_transport"] =
            json!("reviewed owned repository byte copy and sandboxed checkout");
        Ok(result)
    }
}
fn canonical(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(Error::Invalid("absolute canonical path required"));
    }
    let actual = fs::canonicalize(path)?;
    if actual != path {
        return Err(Error::Denied("symlink/noncanonical path"));
    }
    Ok(actual)
}
pub(crate) fn relative(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 1024
        || path.contains('\0')
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || path.split('/').any(|p| p.eq_ignore_ascii_case(".git"))
    {
        return Err(Error::Invalid("relative file path"));
    }
    Ok(())
}
fn bounded_file(path: &Path, cap: usize) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    bounded_descriptor(file, cap)
}
fn bounded_descriptor(mut file: File, cap: usize) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    let before = file.metadata()?;
    if !before.is_file() || before.len() > cap as u64 || before.nlink() != 1 {
        return Err(Error::Denied("regular bounded single-link file required"));
    }
    let mut bytes = Vec::new();
    file.by_ref().take(cap as u64 + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > cap {
        return Err(Error::Limit("file bytes"));
    }
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(Error::Stale);
    }
    Ok(bytes)
}
fn tree(
    root: &Path,
    path: &Path,
    entries: &mut usize,
    bytes: &mut usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)?;
    scan_directory(
        &directory,
        path.strip_prefix(root).map_err(|_| Error::Io)?,
        entries,
        bytes,
        out,
    )
}
fn scan_directory(
    directory: &File,
    prefix: &Path,
    entries: &mut usize,
    bytes: &mut usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    use std::{
        ffi::{CStr, CString, OsString},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStringExt,
        },
    };
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let duplicated = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicated < 0 {
        return Err(Error::Io);
    }
    let pointer = unsafe { libc::fdopendir(duplicated) };
    if pointer.is_null() {
        unsafe {
            libc::close(duplicated);
        }
        return Err(Error::Io);
    }
    let iterator = Directory(pointer);
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(iterator.0) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
            if names.len() + *entries > 2048 {
                return Err(Error::Limit("workspace entries"));
            }
        }
    }
    names.sort();
    for name in names {
        *entries += 1;
        if *entries > 2048 {
            return Err(Error::Limit("workspace entries"));
        }
        let relative_path = prefix.join(OsString::from_vec(name.clone()));
        let display = relative_path
            .to_str()
            .ok_or(Error::Invalid("path encoding"))?;
        relative(display)?;
        let c_name = CString::new(name).map_err(|_| Error::Invalid("name"))?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(Error::Denied("unsafe workspace entry"));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        out.extend_from_slice(&(display.len() as u64).to_be_bytes());
        out.extend_from_slice(display.as_bytes());
        if meta.is_dir() {
            out.push(b'd');
            scan_directory(&file, &relative_path, entries, bytes, out)?;
        } else {
            let data = bounded_descriptor(file, (8 * 1024 * 1024usize).saturating_sub(*bytes))?;
            *bytes += data.len();
            out.push(b'f');
            out.extend_from_slice(&(data.len() as u64).to_be_bytes());
            out.extend_from_slice(&data);
        }
    }
    Ok(())
}

fn copy_snapshot(
    mut data: &[u8],
    destination: &Path,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    fn length(data: &mut &[u8]) -> Result<usize> {
        let bytes = data.get(..8).ok_or(Error::Io)?;
        let n = u64::from_be_bytes(bytes.try_into().map_err(|_| Error::Io)?);
        *data = &data[8..];
        usize::try_from(n).map_err(|_| Error::Io)
    }
    while !data.is_empty() {
        check()?;
        let size = length(&mut data)?;
        let name =
            std::str::from_utf8(data.get(..size).ok_or(Error::Io)?).map_err(|_| Error::Io)?;
        relative(name)?;
        data = &data[size..];
        let kind = *data.first().ok_or(Error::Io)?;
        data = &data[1..];
        let path = destination.join(name);
        if kind == b'd' {
            fs::create_dir(&path)?;
        } else if kind == b'f' {
            let size = length(&mut data)?;
            let bytes = data.get(..size).ok_or(Error::Io)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            file.write_all(bytes)?;
            data = &data[size..];
        } else {
            return Err(Error::Io);
        }
    }
    Ok(())
}
