use smith_profile_migrate::{reconcile, run, verify, Error, Keys, Operation, Request, Result};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{IsTerminal, Read, Write},
    path::Path,
};
use zeroize::Zeroizing;

const HELP: &str = r#"smith-profile-migrate — offline encrypted preservation only

  preflight --source-copy DIR --managed-copy DIR --destination NEW_DIR --offline-copy [KEY_INPUT]
  migrate   --source-copy DIR --managed-copy DIR --destination NEW_DIR --offline-copy [KEY_INPUT]
  verify    --profile-copy DIR --offline-copy [KEY_INPUT]
  reconcile --baseline-copy DIR --current-copy DIR --offline-copy [KEY_INPUT]

KEY_INPUT: --key-mode master|derived (default master)
           --key-source prompt|stdin (default prompt), OR --key-fd NUMBER

master: one line containing 64 hex characters of the existing identity secret.
derived: first line is exact SQLCipher passphrase; second is the 64-hex blob key.
Never put secret values in arguments, environment variables, commands or reports.
stdin must be a pipe/file; prompt disables terminal echo. Files are never activated.
Outputs must not exist; source copies must be quiesced, regular files, without links.
Preflight writes an encrypted inventory. Migration writes encrypted DB/blobs/vault.
Reconcile reports divergence; exit 2 means rollback may discard later changes.
"#;
#[cfg(unix)]
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(unix)]
extern "C" fn interrupt_secret_input(_: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(unix)]
struct SignalRestore(Vec<(libc::c_int, libc::sigaction)>);
#[cfg(unix)]
impl Drop for SignalRestore {
    fn drop(&mut self) {
        for (signal, action) in &self.0 {
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
    }
}
#[cfg(unix)]
fn protect_prompt() -> Result<SignalRestore> {
    let mut restore = SignalRestore(Vec::new());
    INTERRUPTED.store(false, std::sync::atomic::Ordering::Relaxed);
    for signal in [
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGTSTP,
    ] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = interrupt_secret_input as *const () as usize;
        let mut old = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        if unsafe { libc::sigaction(signal, &action, old.as_mut_ptr()) } != 0 {
            return Err(Error("cannot protect secret prompt interruption"));
        }
        restore.0.push((signal, unsafe { old.assume_init() }));
    }
    Ok(restore)
}
fn input_line(reader: &mut impl Read) -> Result<Zeroizing<String>> {
    let mut bytes = Zeroizing::new(Vec::new());
    loop {
        #[cfg(unix)]
        if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error("secret input interrupted"));
        }
        let mut b = [0];
        if reader
            .read(&mut b)
            .map_err(|_| Error("secret input failed"))?
            == 0
        {
            break;
        }
        if b[0] == b'\n' {
            break;
        }
        if bytes.len() >= 4096 {
            return Err(Error("secret input exceeds limit"));
        }
        bytes.push(b[0]);
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let value = std::str::from_utf8(&bytes).map_err(|_| Error("secret input is not UTF-8"))?;
    Ok(Zeroizing::new(value.to_owned()))
}
#[cfg(unix)]
struct TerminalInput(libc::c_int);
#[cfg(unix)]
impl Read for TerminalInput {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            let mut poll = libc::pollfd {
                fd: self.0,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 200) };
            if ready < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if ready == 0 {
                continue;
            }
            // Use one raw read so standard-library EINTR retries cannot hide a
            // termination signal while terminal echo is disabled.
            let n = unsafe { libc::read(self.0, bytes.as_mut_ptr().cast(), bytes.len()) };
            return if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            };
        }
    }
}
#[cfg(unix)]
fn prompt_keys(derived: bool) -> Result<Keys> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .or_else(|error| {
            // Some desktop sandboxes expose an inherited PTY while denying /dev/tty.
            // This fallback remains interactive: a pipe/file is never accepted here.
            if !std::io::stdin().is_terminal() {
                return Err(error);
            }
            let duplicate = unsafe { libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3) };
            if duplicate < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(unsafe { File::from_raw_fd(duplicate) })
        })
        .map_err(|_| Error("no terminal; choose stdin or an inherited secret descriptor"))?;
    let _signals = protect_prompt()?;
    let fd = tty.as_raw_fd();
    let mut old = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, old.as_mut_ptr()) } != 0 {
        return Err(Error("cannot disable terminal echo"));
    }
    let old = unsafe { old.assume_init() };
    let mut settings = old;
    settings.c_lflag &= !(libc::ECHO | libc::ECHONL);
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &settings) } != 0 {
        return Err(Error("cannot disable terminal echo"));
    }
    struct Restore(i32, libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                libc::tcflush(self.0, libc::TCIFLUSH);
                libc::tcsetattr(self.0, libc::TCSANOW, &self.1);
            }
        }
    }
    let _restore = Restore(fd, old);
    tty.write_all(if derived {
        b"Existing SQLCipher passphrase: "
    } else {
        b"Existing master secret (64 hex): "
    })?;
    tty.flush()?;
    let first = input_line(&mut TerminalInput(fd))?;
    tty.write_all(b"\n")?;
    if derived {
        tty.write_all(b"Existing artifact key (64 hex): ")?;
        tty.flush()?;
        let second = input_line(&mut TerminalInput(fd))?;
        tty.write_all(b"\n")?;
        Keys::from_derived(first, &second)
    } else {
        Keys::from_master_hex(&first)
    }
}
fn keys(options: &BTreeMap<String, String>) -> Result<Keys> {
    let derived = match options
        .get("--key-mode")
        .map(String::as_str)
        .unwrap_or("master")
    {
        "master" => false,
        "derived" => true,
        _ => return Err(Error("key mode must be master or derived")),
    };
    if options.contains_key("--key-source") && options.contains_key("--key-fd") {
        return Err(Error("choose one secret input mechanism"));
    }
    if let Some(fd) = options.get("--key-fd") {
        #[cfg(unix)]
        {
            use std::os::fd::FromRawFd;
            let fd = fd
                .parse::<i32>()
                .map_err(|_| Error("invalid secret descriptor number"))?;
            if fd < 3 {
                return Err(Error("secret descriptor must be at least 3"));
            }
            let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            if duplicate < 0 {
                return Err(Error("secret descriptor is unavailable"));
            }
            let mut reader = unsafe { File::from_raw_fd(duplicate) };
            if reader.is_terminal() {
                return Err(Error("secret descriptor cannot be a terminal; use prompt"));
            }
            let first = input_line(&mut reader)?;
            return if derived {
                Keys::from_derived(first, &input_line(&mut reader)?)
            } else {
                Keys::from_master_hex(&first)
            };
        }
        #[cfg(not(unix))]
        {
            let _ = fd;
            return Err(Error("inherited descriptor input currently requires Unix"));
        }
    }
    match options
        .get("--key-source")
        .map(String::as_str)
        .unwrap_or("prompt")
    {
        "stdin" => {
            if std::io::stdin().is_terminal() {
                return Err(Error("stdin is a terminal; use prompt to suppress echo"));
            }
            let mut reader = std::io::stdin().lock();
            let first = input_line(&mut reader)?;
            if derived {
                Keys::from_derived(first, &input_line(&mut reader)?)
            } else {
                Keys::from_master_hex(&first)
            }
        }
        "prompt" => {
            #[cfg(unix)]
            {
                prompt_keys(derived)
            }
            #[cfg(not(unix))]
            {
                Err(Error(
                    "interactive input currently requires Unix; use a secret stdin pipe",
                ))
            }
        }
        _ => Err(Error("unknown secret input mechanism")),
    }
}
fn execute() -> Result<i32> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        print!("{HELP}");
        return Ok(0);
    };
    if command == "--help" || command == "help" {
        print!("{HELP}");
        return Ok(0);
    }
    if !matches!(
        command.as_str(),
        "preflight" | "migrate" | "verify" | "reconcile"
    ) {
        return Err(Error("unknown operation; see --help"));
    }
    let mut options = BTreeMap::new();
    let mut offline = false;
    while let Some(flag) = args.next() {
        if flag == "--offline-copy" {
            if offline {
                return Err(Error("duplicate offline-copy flag"));
            }
            offline = true;
            continue;
        }
        let allowed = matches!(flag.as_str(), "--key-mode" | "--key-source" | "--key-fd")
            || match command.as_str() {
                "preflight" | "migrate" => matches!(
                    flag.as_str(),
                    "--source-copy" | "--managed-copy" | "--destination"
                ),
                "verify" => flag == "--profile-copy",
                "reconcile" => matches!(flag.as_str(), "--baseline-copy" | "--current-copy"),
                _ => false,
            };
        if !allowed {
            return Err(Error(
                "unknown option; secret values must never be arguments",
            ));
        }
        let value = args.next().ok_or(Error("missing option value"))?;
        if options.insert(flag, value).is_some() {
            return Err(Error("duplicate option"));
        }
    }
    if !offline {
        return Err(Error(
            "--offline-copy attests every selected input is an independent, quiesced copy",
        ));
    }
    let path = |name: &str| -> Result<&Path> {
        options
            .get(name)
            .map(Path::new)
            .ok_or(Error("required path option is missing; see --help"))
    };
    let required = match command.as_str() {
        "preflight" | "migrate" => vec!["--source-copy", "--managed-copy", "--destination"],
        "verify" => vec!["--profile-copy"],
        _ => vec!["--baseline-copy", "--current-copy"],
    };
    for name in required {
        path(name)?;
    }
    let keys = keys(&options)?;
    if command == "reconcile" {
        let r = reconcile(path("--baseline-copy")?, path("--current-copy")?, &keys)?;
        // Only counts are public. Table names can themselves contain sensitive text.
        println!("{{\"exact\":{},\"changed_tables\":{},\"added_tables\":{},\"removed_tables\":{},\"schema_changed\":{},\"changed_files\":{},\"rollback_safe_without_losing_writes\":{}}}",r.exact,r.changed_tables.len(),r.added_tables.len(),r.removed_tables.len(),r.schema_changed,r.changed_files,r.rollback_safe_without_losing_writes);
        return Ok(if r.exact { 0 } else { 2 });
    }
    let m = if command == "verify" {
        verify(path("--profile-copy")?, &keys)?
    } else {
        run(
            &Request {
                source_copy: path("--source-copy")?,
                managed_copy: path("--managed-copy")?,
                destination: path("--destination")?,
                offline_copy_attested: true,
            },
            &keys,
            if command == "migrate" {
                Operation::Migrate
            } else {
                Operation::Preflight
            },
        )?
    };
    println!("{{\"ok\":true,\"schema\":25,\"tables\":{},\"verified_blobs\":{},\"external_stores_reference_only\":{},\"ancillary_files\":{},\"runtime_enabled\":false}}",m.database.tables.len(),m.verified_blobs,m.external_stores,m.ancillary_files);
    Ok(0)
}
fn main() {
    match execute() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("Migration refused: {e}");
            std::process::exit(1);
        }
    }
}
