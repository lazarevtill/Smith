use crate::{Error, PlanSummary, Result};
use serde_json::{json, Value};
use std::path::Path;
#[cfg(target_os = "macos")]
use std::{
    io::Read,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) fn parse_command(command: &str) -> Result<Vec<String>> {
    if command.is_empty()
        || command.len() > 8192
        || command
            .chars()
            .any(|c| c.is_control() || "$`;|&<>*?[]{}\\".contains(c))
    {
        return Err(Error::Invalid("shell expansion/operators are unsupported"));
    }
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut started = false;
    for ch in command.chars() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                token.push(ch);
            }
            started = true;
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                tokens.push(std::mem::take(&mut token));
                started = false;
            }
        } else {
            token.push(ch);
            started = true;
        }
    }
    if quote.is_some() {
        return Err(Error::Invalid("unclosed command quote"));
    }
    if started {
        tokens.push(token);
    }
    if tokens.is_empty() || tokens[0].is_empty() || tokens.len() > 64 {
        return Err(Error::Invalid("command argv"));
    }
    Ok(tokens)
}

#[cfg(target_os = "macos")]
pub(crate) struct ProcessSpec<'a> {
    pub executable: &'a Path,
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub read: &'a [&'a Path],
    pub write: &'a [&'a Path],
    pub env: &'a [(String, String)],
}
#[cfg(target_os = "macos")]
fn profile(spec: &ProcessSpec<'_>) -> Result<String> {
    fn path(p: &Path) -> Result<String> {
        serde_json::to_string(p.to_str().ok_or(Error::Invalid("path encoding"))?)
            .map_err(|_| Error::Io)
    }
    let mut text=format!("(version 1)(deny default)(allow process-exec (literal {}))(allow file-read-metadata)(allow sysctl-read)(allow file-read* (literal \"/\") (subpath \"/System/Library\") (subpath \"/usr/lib\") (literal \"/dev/null\") (literal \"/dev/urandom\") (literal {}))",path(spec.executable)?,path(spec.executable)?);
    text.push_str("(allow file-write* (literal \"/dev/null\"))");
    for p in spec.read {
        text.push_str(&format!("(allow file-read* (subpath {}))", path(p)?));
    }
    for p in spec.write {
        text.push_str(&format!(
            "(allow file-read* file-write* (subpath {}))",
            path(p)?
        ));
    }
    Ok(text)
}

#[cfg(target_os = "macos")]
pub(crate) fn run(
    spec: ProcessSpec<'_>,
    plan: &PlanSummary,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<Value> {
    use std::os::{
        fd::AsRawFd,
        unix::process::{CommandExt, ExitStatusExt},
    };
    check()?;
    let policy = profile(&spec)?;
    let mut descriptor_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut descriptor_limit) } != 0
        || descriptor_limit.rlim_cur > 1_048_576
    {
        return Err(Error::Unsupported(
            "cannot bound inherited descriptor cleanup",
        ));
    }
    let descriptor_ceiling = descriptor_limit.rlim_cur as i32;
    let mut cmd = Command::new("/usr/bin/sandbox-exec");
    cmd.args(["-p", &policy])
        .arg(spec.executable)
        .args(spec.argv)
        .current_dir(spec.cwd)
        .env_clear()
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("PATH", "")
        .envs(spec.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Only async-signal-safe libc operations between fork and exec. The child
    // owns its process group; the sandbox then denies all further forks/execs
    // except the exact approved executable, and all network/IPC by default.
    unsafe {
        cmd.pre_exec(move || {
            // Inherited credentials may be reachable through open descriptors
            // as well as environment variables. Preserve only the explicitly
            // connected stdin/stdout/stderr across exec. Marking CLOEXEC also
            // preserves Rust's error pipe until exec succeeds or reports error.
            for fd in 3..descriptor_ceiling {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for (resource, value) in [
                (libc::RLIMIT_CORE, 0),
                (libc::RLIMIT_FSIZE, 8 * 1024 * 1024),
                (libc::RLIMIT_NOFILE, 64),
                (libc::RLIMIT_CPU, 31),
            ] {
                let limit = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let mut owner = OwnedChild(Some(child));
    let result = (|| {
        let mut stdout = owner.0.as_mut().unwrap().stdout.take().ok_or(Error::Io)?;
        let mut stderr = owner.0.as_mut().unwrap().stderr.take().ok_or(Error::Io)?;
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                    return Err(Error::Io);
                }
            }
        }
        let started = Instant::now();
        let mut out = Vec::new();
        let mut err = Vec::new();
        loop {
            check()?;
            if started.elapsed() >= Duration::from_millis(plan.timeout_ms) {
                return Err(Error::TimedOut);
            }
            drain(&mut stdout, &mut out, plan.max_output_bytes)?;
            drain(
                &mut stderr,
                &mut err,
                plan.max_output_bytes.saturating_sub(out.len()),
            )?;
            if let Some(status) = owner.0.as_mut().unwrap().try_wait()? {
                let pid = owner.0.as_ref().unwrap().id();
                // Reaping relinquishes ownership immediately, before even a
                // fallible final pipe read. No later error may signal this PID.
                owner.0.take();
                drain(&mut stdout, &mut out, plan.max_output_bytes)?;
                drain(
                    &mut stderr,
                    &mut err,
                    plan.max_output_bytes.saturating_sub(out.len()),
                )?;
                return Ok(
                    json!({"exit_code":status.code().unwrap_or(-1),"signal":status.signal(),"stdout":String::from_utf8_lossy(&out),"stderr":String::from_utf8_lossy(&err),"sandbox":"macos-seatbelt","pid":pid,"reaped":true}),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if owner.0.is_some() {
        owner.stop()?;
    }
    result
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) struct ProcessSpec<'a> {
    pub executable: &'a Path,
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub read: &'a [&'a Path],
    pub write: &'a [&'a Path],
    pub env: &'a [(String, String)],
}
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn run(
    spec: ProcessSpec<'_>,
    _: &PlanSummary,
    _: &mut dyn FnMut() -> Result<()>,
) -> Result<Value> {
    let _ = (
        spec.executable,
        spec.argv,
        spec.cwd,
        spec.read,
        spec.write,
        spec.env,
    );
    Err(Error::Unsupported(
        "process effects require qualified macOS sandbox",
    ))
}

#[cfg(target_os = "macos")]
fn drain(reader: &mut impl Read, buffer: &mut Vec<u8>, cap: usize) -> Result<()> {
    if buffer.len() > cap {
        return Err(Error::Limit("process output"));
    }
    let mut chunk = [0u8; 4096];
    // Bounded iteration keeps cancellation responsive under endless output.
    for _ in 0..64 {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if buffer.len() + n > cap {
                    return Err(Error::Limit("process output"));
                }
                buffer.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Error::Io),
        }
    }
    Ok(())
}
#[cfg(target_os = "macos")]
struct OwnedChild(Option<Child>);
#[cfg(target_os = "macos")]
impl OwnedChild {
    fn stop(&mut self) -> Result<()> {
        if let Some(child) = self.0.as_mut() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            child.wait().map_err(|_| Error::Cleanup)?;
        }
        self.0.take();
        Ok(())
    }
}
#[cfg(target_os = "macos")]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_expansion_or_operators() {
        for s in [
            "echo $HOME",
            "echo $(id)",
            "echo hi; id",
            "echo a|cat",
            "echo >x",
            "echo `id`",
            "echo *",
            "echo a\nb",
            "echo \\\"x",
        ] {
            assert!(parse_command(s).is_err(), "{s}");
        }
        assert_eq!(
            parse_command("echo 'hello world' \"\"").unwrap(),
            vec!["echo", "hello world", ""]
        );
    }
}
