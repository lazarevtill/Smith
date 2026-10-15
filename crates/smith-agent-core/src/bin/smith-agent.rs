#![forbid(unsafe_code)]

#[cfg(unix)]
mod unix_cli {
    use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};
    use smith_agent_core::*;
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs::File,
        io::{Read, Write},
        path::PathBuf,
        sync::Arc,
    };

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct HostPolicy {
        version: u32,
        host_id: String,
        principal: String,
        workspace: WorkspaceIdentity,
        capabilities: BTreeSet<Capability>,
    }

    struct Host {
        _directory: File,
        path: PathBuf,
        grant: ExecutionGrant,
    }

    pub fn run() -> Result<Value> {
        let mut input = std::env::args().skip(1);
        let command = input.next().unwrap_or_else(|| "help".into());
        if command == "help" || command == "--help" {
            return Ok(
                json!({"usage":"smith-agent COMMAND --host HOST_ID --principal PRINCIPAL [options]", "commands":["init --principal ID --capabilities read,list,write,search,edit,control (choose explicitly, or none)","create --input TEXT", "start --task ID", "status --task ID", "trace --task ID", "tools", "read --task ID --path NAME", "list --task ID", "write --task ID --path NAME --contents TEXT", "search --task ID --query LITERAL [--max-results N] [--max-bytes N]", "edit --task ID --path NAME --old LITERAL --new TEXT (--expected-contents TEXT | --expected-sha256 HASH)", "approve|deny --task ID --revision N", "plan --task ID --steps JSON_ARRAY", "clarify --task ID --question TEXT", "resume --task ID --revision N --answer TEXT", "complete --task ID --summary TEXT", "cancel --task ID --reason TEXT", "interrupt --task ID --revision N (old worker must have stopped)", "recover --task ID --revision N --decision continue|retry", "call --task ID --tool NAME --arguments JSON", "fixture"], "scope":"Private temporary workspaces; direct-child files only. No shell, network, MCP, or computer-use execution."}),
            );
        }
        let mut options = parse_options(input)?;
        if command == "fixture" {
            no_extra(&options)?;
            return serde_json::to_value(fixture::run_deterministic_fixture()?)
                .map_err(serialization);
        }
        if command == "init" {
            let principal = take(&mut options, "principal")?;
            let capabilities = parse_capabilities(&take(&mut options, "capabilities")?)?;
            no_extra(&options)?;
            return initialize(&principal, capabilities);
        }
        let host_id = take(&mut options, "host")?;
        let principal = take(&mut options, "principal")?;
        let host = open_host(&host_id, &principal)?;
        let engine = TaskEngine::new(
            SqliteTraceStore::open(host.path.join("trace.sqlite"))?,
            ToolRegistry::default(),
        );
        if command == "tools" {
            no_extra(&options)?;
            return Ok(
                json!({"inventory":engine.registry().all_tools(),"granted":engine.registry().available_tools(&host.grant)}),
            );
        }
        if command == "create" {
            let input = take(&mut options, "input")?;
            no_extra(&options)?;
            return Ok(json!({"task":engine.create(&host.grant, input)?}));
        }
        let id = take(&mut options, "task")?;
        let grant = &host.grant;
        // Verify ownership before either loading raw trace records or executing.
        engine.get(&id, grant)?;
        let outcome = match command.as_str() {
            "status" => {
                no_extra(&options)?;
                return Ok(json!({"task":engine.get(&id, grant)?}));
            }
            "trace" => {
                no_extra(&options)?;
                return Ok(json!({"trace":engine.store().events(&id)?}));
            }
            "start" => {
                no_extra(&options)?;
                engine.start(&id, grant)?;
                None
            }
            "cancel" => {
                let reason = take(&mut options, "reason")?;
                no_extra(&options)?;
                engine.cancel(&id, grant, reason)?;
                None
            }
            "approve" | "deny" => {
                let revision = revision(&mut options)?;
                no_extra(&options)?;
                Some(engine.decide_approval(
                    &id,
                    revision,
                    grant,
                    if command == "approve" {
                        ApprovalDecision::Approve
                    } else {
                        ApprovalDecision::Deny
                    },
                )?)
            }
            "resume" => {
                let revision = revision(&mut options)?;
                let answer = take(&mut options, "answer")?;
                no_extra(&options)?;
                engine.resume_clarification(&id, revision, grant, answer)?;
                None
            }
            "interrupt" => {
                let revision = revision(&mut options)?;
                no_extra(&options)?;
                engine.mark_interrupted(&id, revision, grant)?;
                None
            }
            "recover" => {
                let revision = revision(&mut options)?;
                let decision = match take(&mut options, "decision")?.as_str() {
                    "continue" => RecoveryDecision::ContinueWithoutReplay,
                    "retry" => RecoveryDecision::RetryUncertainCall,
                    _ => return Err(invalid("decision must be continue or retry")),
                };
                no_extra(&options)?;
                Some(engine.resume_interrupted(&id, revision, grant, decision)?)
            }
            _ => {
                let (tool, args) = match command.as_str() {
                    "read" => (
                        "read_file".into(),
                        json!({"path":take(&mut options,"path")?}),
                    ),
                    "write" => (
                        "write_file".into(),
                        json!({"path":take(&mut options,"path")?,"contents":take(&mut options,"contents")?}),
                    ),
                    "list" => ("list_directory".into(), json!({})),
                    "search" => {
                        let mut args = json!({"query":take(&mut options, "query")?});
                        for (flag, field) in
                            [("max-results", "max_results"), ("max-bytes", "max_bytes")]
                        {
                            if let Some(value) = options.remove(flag) {
                                let value: usize = value.parse().map_err(|_| {
                                    invalid("search limits must be unsigned integers")
                                })?;
                                args[field] = json!(value);
                            }
                        }
                        ("search".into(), args)
                    }
                    "edit" => {
                        let mut args = json!({"path":take(&mut options,"path")?,"old_string":take(&mut options,"old")?,"new_string":take(&mut options,"new")?});
                        for (flag, field) in [
                            ("expected-contents", "expected_contents"),
                            ("expected-sha256", "expected_sha256"),
                        ] {
                            if let Some(value) = options.remove(flag) {
                                args[field] = json!(value);
                            }
                        }
                        ("edit_file".into(), args)
                    }
                    "plan" => (
                        "update_plan".into(),
                        json!({"steps":parse_json(&take(&mut options,"steps")?)?}),
                    ),
                    "clarify" => (
                        "ask_clarification".into(),
                        json!({"question":take(&mut options,"question")?}),
                    ),
                    "complete" => (
                        "task_complete".into(),
                        json!({"summary":take(&mut options,"summary")?}),
                    ),
                    "call" => (
                        take(&mut options, "tool")?,
                        parse_json(&take(&mut options, "arguments")?)?,
                    ),
                    _ => return Err(invalid("unknown command; run smith-agent help")),
                };
                no_extra(&options)?;
                Some(engine.call(&id, grant, ToolCall::new(tool, args))?)
            }
        };
        Ok(json!({"task":engine.get(&id, grant)?,"outcome":outcome}))
    }

    fn initialize(principal: &str, capabilities: BTreeSet<Capability>) -> Result<Value> {
        use std::os::unix::fs::PermissionsExt;
        let principal = Principal::new(principal)?;
        let directory = tempfile::Builder::new()
            .prefix("smith-agent-host-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let root = File::open(directory.path())?;
        let stat = fs::fstat(&root)?;
        let name = directory
            .path()
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| invalid("host directory name"))?;
        // Device/inode bind a saved host ID to the actual provisioned directory.
        let host_id = format!("{}@{}-{}", name, stat.st_dev, stat.st_ino);
        let workspace = Workspace::create_temporary()?;
        let result = (|| {
            let policy = HostPolicy {
                version: 1,
                host_id: host_id.clone(),
                principal: principal.id().into(),
                workspace: workspace.identity().clone(),
                capabilities,
            };
            let fd = fs::openat(
                &root,
                "policy.json",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?;
            let mut file = File::from(fd);
            file.write_all(&serde_json::to_vec(&policy).map_err(serialization)?)?;
            file.sync_all()?;
            let db = fs::openat(
                &root,
                "trace.sqlite",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?;
            File::from(db).sync_all()?;
            SqliteTraceStore::open(directory.path().join("trace.sqlite"))?;
            root.sync_all()?;
            Ok(
                json!({"host_id":host_id,"principal":principal.id(),"capabilities":policy.capabilities,"workspace":workspace.identity(),"workspace_path":workspace.host_path(),"host_path":directory.path()}),
            )
        })();
        if result.is_ok() {
            let _ = directory.keep();
        } else {
            let _ = std::fs::remove_dir_all(workspace.host_path());
        }
        result
    }

    fn open_host(host_id: &str, principal: &str) -> Result<Host> {
        let (name, identity) = host_id
            .split_once('@')
            .ok_or_else(|| invalid("malformed host ID"))?;
        if !name.starts_with("smith-agent-host-")
            || name.len() > 100
            || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err(Error::UnsafePath("invalid host directory name".into()));
        }
        let (device, inode) = identity
            .split_once('-')
            .ok_or_else(|| invalid("host identity missing"))?;
        let device: u64 = device.parse().map_err(|_| invalid("host device"))?;
        let inode: u64 = inode.parse().map_err(|_| invalid("host inode"))?;
        let temp = File::open(std::env::temp_dir())?;
        let root = fs::openat(
            &temp,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fs::fstat(&root)?;
        if stat.st_dev as u64 != device
            || stat.st_ino as u64 != inode
            || stat.st_mode & 0o777 != 0o700
            || stat.st_uid != rustix::process::geteuid().as_raw()
        {
            return Err(Error::Denied(
                "host directory identity, owner, or permissions mismatch".into(),
            ));
        }
        let fd = fs::openat(
            &root,
            "policy.json",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        check_private_file(&fs::fstat(&fd)?)?;
        let mut bytes = Vec::new();
        File::from(fd).take(16385).read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err(Error::LimitExceeded("host policy bytes".into()));
        }
        let policy: HostPolicy = serde_json::from_slice(&bytes).map_err(serialization)?;
        if policy.version != 1 || policy.host_id != host_id || policy.principal != principal {
            return Err(Error::Denied("host policy or principal mismatch".into()));
        }
        // The DB and auxiliary files are host-owned and outside tool authority.
        // Refuse redirection before opening SQLite. Same-UID malicious host code
        // is outside the security boundary of this local CLI.
        for name in [
            "trace.sqlite",
            "trace.sqlite-wal",
            "trace.sqlite-shm",
            "trace.sqlite-journal",
        ] {
            match fs::statat(&root, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => check_private_file(&stat)?,
                Err(rustix::io::Errno::NOENT) if name != "trace.sqlite" => (),
                Err(error) => return Err(error.into()),
            }
        }
        let workspace = Arc::new(Workspace::reopen_temporary(&policy.workspace)?);
        let grant = ExecutionGrant::new(
            Principal::new(principal)?,
            workspace,
            "local-cli-agent",
            policy.capabilities,
        )?;
        Ok(Host {
            _directory: File::from(root),
            path: std::env::temp_dir().join(name),
            grant,
        })
    }

    fn check_private_file(stat: &fs::Stat) -> Result<()> {
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_nlink != 1
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o777 != 0o600
        {
            return Err(Error::Denied(
                "host file must be private, owned, singly linked, and regular".into(),
            ));
        }
        Ok(())
    }
    fn parse_options(input: impl Iterator<Item = String>) -> Result<BTreeMap<String, String>> {
        let mut input = input;
        let mut map = BTreeMap::new();
        while let Some(key) = input.next() {
            let name = key
                .strip_prefix("--")
                .ok_or_else(|| invalid("options must use --name value"))?;
            let value = input
                .next()
                .ok_or_else(|| invalid("missing option value"))?;
            if value.len() > 128 * 1024 || map.len() >= 16 {
                return Err(Error::LimitExceeded("CLI arguments".into()));
            }
            if map.insert(name.into(), value).is_some() {
                return Err(invalid("duplicate option"));
            }
        }
        Ok(map)
    }
    fn parse_capabilities(text: &str) -> Result<BTreeSet<Capability>> {
        if text == "none" {
            return Ok(BTreeSet::new());
        }
        text.split(',')
            .map(|item| match item {
                "read" => Ok(Capability::ReadWorkspace),
                "list" => Ok(Capability::ListWorkspace),
                "write" => Ok(Capability::WriteWorkspace),
                "search" => Ok(Capability::SearchWorkspace),
                "edit" => Ok(Capability::EditWorkspace),
                "control" => Ok(Capability::TaskControl),
                _ => Err(invalid(
                    "capabilities must be explicit read,list,write,search,edit,control or none",
                )),
            })
            .collect()
    }
    fn take(options: &mut BTreeMap<String, String>, key: &str) -> Result<String> {
        options
            .remove(key)
            .ok_or_else(|| invalid(&format!("missing --{key}")))
    }
    fn revision(options: &mut BTreeMap<String, String>) -> Result<u64> {
        take(options, "revision")?
            .parse()
            .map_err(|_| invalid("revision must be an unsigned integer"))
    }
    fn no_extra(options: &BTreeMap<String, String>) -> Result<()> {
        if options.is_empty() {
            Ok(())
        } else {
            Err(invalid("unknown option"))
        }
    }
    fn parse_json(value: &str) -> Result<Value> {
        serde_json::from_str(value).map_err(serialization)
    }
    fn serialization(error: serde_json::Error) -> Error {
        Error::InvalidInput(error.to_string())
    }
    fn invalid(message: &str) -> Error {
        Error::InvalidInput(message.into())
    }
}

fn main() {
    #[cfg(unix)]
    let result = unix_cli::run();
    #[cfg(not(unix))]
    let result: smith_agent_core::Result<serde_json::Value> =
        Err(smith_agent_core::Error::Unsupported(
            "confined CLI requires Unix; Windows adapter pending".into(),
        ));
    match result {
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("JSON value serializes")
        ),
        Err(error) => {
            eprintln!("{}", serde_json::json!({"error":error}));
            std::process::exit(1);
        }
    }
}
