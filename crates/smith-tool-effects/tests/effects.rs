#![cfg(feature = "fixtures")]
use serde_json::{json, Value};
use smith_tool_effects::{
    DestinationGrant, EffectHost, Error, ExecutableGrant, NetworkOperation, WorkspaceGrant,
};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

fn prepared(
    host: &EffectHost,
    id: &str,
    tool: &str,
    args: Value,
) -> smith_tool_effects::ApprovedPlan {
    let revision = host.binding(id).unwrap().revision;
    let plan = host.prepare(id, revision, tool, args).unwrap();
    host.approve(&plan.summary().id, revision).unwrap()
}
fn execute(host: &EffectHost, id: &str, tool: &str, args: Value) -> Value {
    host.execute(prepared(host, id, tool, args), &mut || Ok(()))
        .unwrap()
}
fn fixture() -> ExecutableGrant {
    let path = fs::canonicalize(env!("CARGO_BIN_EXE_smith-effects-fixture")).unwrap();
    ExecutableGrant::inspect("fixture", &path).unwrap()
}
fn workspace(host: &EffectHost) -> (String, PathBuf) {
    let work = WorkspaceGrant::temporary().unwrap();
    let path = work.host_path().to_owned();
    let binding = host.bind_workspace(work, vec![fixture()], None).unwrap();
    (binding.id, path)
}
#[cfg(target_os = "macos")]
fn git_workspace(host: &EffectHost) -> (String, PathBuf) {
    let work = WorkspaceGrant::temporary().unwrap();
    let path = work.host_path().to_owned();
    let git = ExecutableGrant::inspect(
        "git",
        std::path::Path::new("/Applications/Xcode.app/Contents/Developer/usr/bin/git"),
    )
    .unwrap();
    let binding = host
        .bind_workspace(work, vec![fixture()], Some(git))
        .unwrap();
    (binding.id, path)
}
fn assert_ok(out: &Value) {
    assert_eq!(out["exit_code"], 0, "{out}");
    assert_eq!(out["reaped"], true);
}

#[test]
fn descriptors_match_nine_original_names() {
    let names: Vec<_> = smith_tool_effects::descriptors()
        .into_iter()
        .map(|v| v["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names.len(), 9);
    assert!(names.contains(&"send_webhook".to_string()));
}

#[cfg(target_os = "macos")]
#[test]
fn host_prefix_is_reviewed_immutable_and_cannot_be_overridden_by_command_args() {
    let host = EffectHost::new();
    let work = WorkspaceGrant::temporary().unwrap();
    let root = work.host_path().to_owned();
    let binary = fs::canonicalize(env!("CARGO_BIN_EXE_smith-effects-fixture")).unwrap();
    let executable =
        ExecutableGrant::inspect_with_prefix("echo", &binary, vec!["echo".into()]).unwrap();
    let bound = host.bind_workspace(work, vec![executable], None).unwrap();
    let plan = host
        .prepare(
            &bound.id,
            bound.revision,
            "run_shell",
            json!({"command":"echo write unexpected.txt changed"}),
        )
        .unwrap();
    assert_eq!(
        plan.summary().policy["executables"][0]["argv_prefix"],
        json!(["echo"])
    );
    let approved = host.approve(&plan.summary().id, bound.revision).unwrap();
    let output = host.execute(approved, &mut || Ok(())).unwrap();
    assert_ok(&output);
    assert_eq!(output["signal"], Value::Null);
    assert_eq!(output["stdout"], "write unexpected.txt changed\n");
    assert!(!root.join("unexpected.txt").exists());
    assert!(host
        .prepare(
            &bound.id,
            host.binding(&bound.id).unwrap().revision,
            "run_shell",
            json!({"command":"echo test","argv_prefix":[]})
        )
        .is_err());
    assert!(
        ExecutableGrant::inspect_with_prefix("bad", &binary, vec!["bad\0prefix".into()]).is_err()
    );
    assert!(ExecutableGrant::inspect_with_prefix("bad", &binary, vec!["x".repeat(4097)]).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn signal_termination_is_reported_and_owned_child_is_reaped() {
    let host = EffectHost::new();
    let (id, _) = workspace(&host);
    let output = execute(&host, &id, "run_shell", json!({"command":"fixture abort"}));
    assert_eq!(output["exit_code"], -1);
    assert_eq!(output["signal"], libc::SIGABRT);
    assert_eq!(output["reaped"], true);
    assert_eq!(
        unsafe { libc::kill(output["pid"].as_i64().unwrap() as i32, 0) },
        -1
    );
}

#[cfg(target_os = "macos")]
#[test]
fn reviewed_command_real_workspace_and_environment_are_scoped() {
    let host = EffectHost::new();
    let (id, path) = workspace(&host);
    let out = execute(
        &host,
        &id,
        "run_shell",
        json!({"command":"fixture write greeting.txt 'hello effects'"}),
    );
    assert_ok(&out);
    assert_eq!(
        fs::read_to_string(path.join("greeting.txt")).unwrap(),
        "hello effects"
    );
    let out = execute(
        &host,
        &id,
        "run_shell",
        json!({"command":"fixture read greeting.txt"}),
    );
    assert_ok(&out);
    assert_eq!(out["stdout"], "hello effects\n");
    let out = execute(&host, &id, "run_shell", json!({"command":"fixture env"}));
    assert_ok(&out);
    let env = out["stdout"].as_str().unwrap();
    assert!(!env.contains("SSH_AUTH_SOCK="));
    assert!(!env.contains("CODEX_HOME="));
    assert!(!env.contains("HTTP_PROXY="));
    assert!(env.contains("PATH=\n"));
    // Deliberately create a non-CLOEXEC descriptor carrying only a synthetic
    // credential canary. The child must not inherit it even if a host library
    // forgot to request close-on-exec.
    use std::os::fd::{AsRawFd, FromRawFd};
    let fake = tempfile::tempfile().unwrap();
    let fd = unsafe { libc::fcntl(fake.as_raw_fd(), libc::F_DUPFD, 57) };
    assert!(fd >= 57);
    let _owned = unsafe { fs::File::from_raw_fd(fd) };
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
    let out = execute(
        &host,
        &id,
        "run_shell",
        json!({"command":format!("fixture fd {fd}")}),
    );
    assert_ok(&out);
    assert_eq!(out["stdout"], "fd_open=false\n");
    assert!(host
        .prepare(
            &id,
            host.binding(&id).unwrap().revision,
            "run_shell",
            json!({"command":"sh -c echo"})
        )
        .is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_blocks_ungranted_file_symlink_network_and_fork() {
    let outside = tempfile::tempdir().unwrap();
    let file = fs::canonicalize(outside.path()).unwrap().join("canary.txt");
    fs::write(&file, "OUTSIDE-CANARY").unwrap();
    let host = EffectHost::new();
    let (id, path) = workspace(&host);
    let out = execute(
        &host,
        &id,
        "run_shell",
        json!({"command":format!("fixture read {}",file.display())}),
    );
    assert_eq!(out["exit_code"], 23);
    assert!(!out.to_string().contains("OUTSIDE-CANARY"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let out = execute(
        &host,
        &id,
        "run_shell",
        json!({"command":format!("fixture network {address}")}),
    );
    assert_ok(&out);
    assert_eq!(out["stdout"], "connected=false\n");
    assert!(listener.accept().is_err());
    let out = execute(&host, &id, "run_shell", json!({"command":"fixture fork"}));
    assert_ok(&out);
    assert_eq!(out["stdout"], "fork=-1\n");
    let out = execute(
        &host,
        &id,
        "run_shell",
        json!({"command":format!("fixture symlink-read {}",file.display())}),
    );
    assert_eq!(out["exit_code"], 23);
    assert!(!out.to_string().contains("OUTSIDE-CANARY"));
    assert!(host
        .prepare(
            &id,
            host.binding(&id).unwrap().revision,
            "run_shell",
            json!({"command":"fixture echo test"})
        )
        .is_err());
    fs::remove_file(path.join("link")).unwrap();
    let out = execute(
        &host,
        &id,
        "run_shell",
        json!({"command":format!("fixture write {} overwritten",file.display())}),
    );
    assert_ne!(out["exit_code"], 0);
    assert_eq!(fs::read_to_string(file).unwrap(), "OUTSIDE-CANARY");
}

#[cfg(target_os = "macos")]
#[test]
fn cancellation_timeout_and_output_limits_reap_owned_child() {
    let host = EffectHost::new();
    let (id, path) = workspace(&host);
    let plan = prepared(&host, &id, "run_shell", json!({"command":"fixture sleep"}));
    let start = Instant::now();
    let mut checks = 0;
    let result = host.execute(plan, &mut || {
        checks += 1;
        if path.join("child.pid").exists() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    });
    assert_eq!(result.unwrap_err(), Error::Cancelled);
    assert!(checks > 1 && start.elapsed() < Duration::from_secs(3));
    let pid: i32 = fs::read_to_string(path.join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    fs::remove_file(path.join("child.pid")).unwrap();
    let result = host.execute(
        prepared(
            &host,
            &id,
            "run_shell",
            json!({"command":"fixture sleep","timeout_secs":1}),
        ),
        &mut || Ok(()),
    );
    assert_eq!(result.unwrap_err(), Error::TimedOut);
    let pid: i32 = fs::read_to_string(path.join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    let result = host.execute(
        prepared(&host, &id, "run_shell", json!({"command":"fixture spam"})),
        &mut || Ok(()),
    );
    assert_eq!(result.unwrap_err(), Error::Limit("process output"));
}

#[cfg(target_os = "macos")]
#[test]
fn approval_is_exact_single_use_and_stale_workspace_is_denied() {
    let host = EffectHost::new();
    let (id, path) = workspace(&host);
    let p = host
        .prepare(
            &id,
            0,
            "run_shell",
            json!({"command":"fixture echo approved"}),
        )
        .unwrap();
    assert!(host.approve("not-the-plan", 0).is_err());
    assert!(host.approve(&p.summary().id, 1).is_err());
    let approved = host.approve(&p.summary().id, 0).unwrap();
    assert!(host.approve(&p.summary().id, 0).is_err());
    fs::write(path.join("changed"), "after review").unwrap();
    assert_eq!(
        host.execute(approved, &mut || Ok(())).unwrap_err(),
        Error::Stale
    );
    let a = prepared(
        &host,
        &id,
        "run_shell",
        json!({"command":"fixture echo same"}),
    );
    let other = EffectHost::new();
    assert!(other.execute(a, &mut || Ok(())).is_err());
    let a = prepared(
        &host,
        &id,
        "run_shell",
        json!({"command":"fixture echo before-revoke"}),
    );
    host.revoke(&id).unwrap();
    assert!(host.execute(a, &mut || Ok(())).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn git_init_commit_diff_blame_status_log_and_granted_clone_are_real() {
    let host = EffectHost::new();
    let (id, path) = git_workspace(&host);
    assert_ok(&execute(&host, &id, "git_init", json!({})));
    fs::write(path.join("hello.txt"), "line one\nline two\n").unwrap();
    let out = execute(&host, &id, "git_status", json!({}));
    assert_ok(&out);
    assert!(out["stdout"].as_str().unwrap().contains("?? hello.txt"));
    let out = execute(
        &host,
        &id,
        "git_commit",
        json!({"message":"first controlled commit"}),
    );
    assert_ok(&out);
    let out = execute(&host, &id, "git_log", json!({"max_count":1}));
    assert_ok(&out);
    assert!(out["stdout"]
        .as_str()
        .unwrap()
        .contains("first controlled commit"));
    let out = execute(&host, &id, "git_blame", json!({"path":"hello.txt"}));
    assert_ok(&out);
    assert!(out["stdout"]
        .as_str()
        .unwrap()
        .contains("Smith scoped task"));
    fs::write(path.join("hello.txt"), "changed\nline two\n").unwrap();
    let out = execute(&host, &id, "git_diff", json!({"path":"hello.txt"}));
    assert_ok(&out);
    assert!(out["stdout"].as_str().unwrap().contains("+changed"));
    assert_ok(&execute(
        &host,
        &id,
        "git_commit",
        json!({"message":"second controlled commit"}),
    ));
    let (dest, destpath) = git_workspace(&host);
    let out = execute(
        &host,
        &dest,
        "git_clone",
        json!({"url":format!("owned://{id}"),"directory":"."}),
    );
    assert_ok(&out);
    assert_eq!(
        fs::read_to_string(destpath.join("hello.txt")).unwrap(),
        "changed\nline two\n"
    );
    let out = execute(&host, &dest, "git_log", json!({}));
    assert_ok(&out);
    assert!(out["stdout"]
        .as_str()
        .unwrap()
        .contains("first controlled commit"));
    assert!(host
        .prepare(
            &dest,
            host.binding(&dest).unwrap().revision,
            "git_clone",
            json!({"url":"https://example.com/unapproved.git"})
        )
        .is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn clone_rejects_source_changed_after_review() {
    let host = EffectHost::new();
    let (source, path) = git_workspace(&host);
    assert_ok(&execute(&host, &source, "git_init", json!({})));
    fs::write(path.join("one"), "one").unwrap();
    assert_ok(&execute(
        &host,
        &source,
        "git_commit",
        json!({"message":"initial"}),
    ));
    let (dest, destpath) = git_workspace(&host);
    let plan = prepared(
        &host,
        &dest,
        "git_clone",
        json!({"url":format!("owned://{source}")}),
    );
    fs::write(path.join("one"), "two").unwrap();
    assert_ok(&execute(
        &host,
        &source,
        "git_commit",
        json!({"message":"later"}),
    ));
    assert!(host.execute(plan, &mut || Ok(())).is_err());
    assert!(fs::read_dir(destpath).unwrap().next().is_none());
}

fn server(
    response: Vec<u8>,
) -> (
    String,
    SocketAddr,
    mpsc::Receiver<String>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let (tx, rx) = mpsc::channel();
    let join = std::thread::spawn(move || {
        let start = Instant::now();
        loop {
            if let Ok((mut stream, _)) = listener.accept() {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length = header
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length: ")
                                    .and_then(|s| s.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&bytes).to_string());
                let _ = stream.write_all(&response);
                break;
            }
            if start.elapsed() > Duration::from_secs(3) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    (
        format!("http://{address}/exact?reviewed=1"),
        address,
        rx,
        join,
    )
}
#[test]
fn exact_get_and_post_pins_are_separate_and_real() {
    let host = EffectHost::new();
    let (url, address, rx, join) = server(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello".to_vec(),
    );
    let binding = host
        .bind_destination(
            DestinationGrant::loopback_fixture(&url, address, NetworkOperation::WebFetch).unwrap(),
        )
        .unwrap();
    assert!(host
        .prepare(
            &binding.id,
            0,
            "web_fetch",
            json!({"url":format!("{url}&extra=2")})
        )
        .is_err());
    assert!(host
        .prepare(
            &binding.id,
            0,
            "send_webhook",
            json!({"url":url,"message":"no"})
        )
        .is_err());
    let out = execute(&host, &binding.id, "web_fetch", json!({"url":url}));
    assert_eq!(out["status"], 200);
    assert!(out["content"]
        .as_str()
        .unwrap()
        .contains("untrusted web content"));
    let request = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(request.starts_with("GET /exact?reviewed=1 HTTP/1.1"));
    assert!(!request.to_lowercase().contains("authorization:"));
    join.join().unwrap();
    let (url, address, rx, join) =
        server(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n".to_vec());
    let binding = host
        .bind_destination(
            DestinationGrant::loopback_fixture(&url, address, NetworkOperation::SendWebhook)
                .unwrap(),
        )
        .unwrap();
    let out = execute(
        &host,
        &binding.id,
        "send_webhook",
        json!({"url":url,"message":"reviewed body","title":"heading"}),
    );
    assert_eq!(out["status"], 201);
    let request = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(request.starts_with("POST /exact?reviewed=1 HTTP/1.1"));
    assert!(request.contains("\"text\":\"reviewed body\""));
    assert!(request.contains("\"title\":\"heading\""));
    join.join().unwrap();
}
#[test]
fn redirects_content_type_size_and_cancellation_are_bounded() {
    for (response,expected) in [
        (b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/unapproved\r\nContent-Length: 0\r\n\r\n".to_vec(),None),
        (b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 1\r\n\r\nx".to_vec(),Some(Error::Denied("response content type"))),
        (b"HTTP/1.1 200 OK\r\nContent-Length: 300000\r\n\r\n".to_vec(),Some(Error::Limit("HTTP response"))),
    ] {let host=EffectHost::new();let(url,address,_,join)=server(response);let binding=host.bind_destination(DestinationGrant::loopback_fixture(&url,address,NetworkOperation::WebFetch).unwrap()).unwrap();let result=host.execute(prepared(&host,&binding.id,"web_fetch",json!({"url":url})),&mut||Ok(()));if let Some(error)=expected{assert_eq!(result.unwrap_err(),error);}else{assert_eq!(result.unwrap()["status"],302);}join.join().unwrap();}
    let host = EffectHost::new();
    let (url, address, rx, join) = server(Vec::new());
    let binding = host
        .bind_destination(
            DestinationGrant::loopback_fixture(&url, address, NetworkOperation::WebFetch).unwrap(),
        )
        .unwrap();
    let result = host.execute(
        prepared(&host, &binding.id, "web_fetch", json!({"url":url})),
        &mut || Err(Error::Cancelled),
    );
    assert_eq!(result.unwrap_err(), Error::Cancelled);
    join.join().unwrap();
    assert!(rx.try_recv().is_err());
}

#[test]
fn cumulative_http_limit_and_inflight_cancellation_close_owned_requests() {
    let mut response =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n".to_vec();
    response.extend(vec![b'x'; 300_000]);
    let host = EffectHost::new();
    let (url, address, _, join) = server(response);
    let binding = host
        .bind_destination(
            DestinationGrant::loopback_fixture(&url, address, NetworkOperation::WebFetch).unwrap(),
        )
        .unwrap();
    assert_eq!(
        host.execute(
            prepared(&host, &binding.id, "web_fetch", json!({"url":url})),
            &mut || Ok(())
        )
        .unwrap_err(),
        Error::Limit("HTTP response")
    );
    join.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (seen_tx, seen_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let join = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buffer = [0; 8192];
        assert!(stream.read(&mut buffer).unwrap() > 0);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\n\r\nx",
            )
            .unwrap();
        seen_tx.send(()).unwrap();
        let closed = match stream.read(&mut buffer) {
            Ok(0) => true,
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            _ => false,
        };
        closed_tx.send(closed).unwrap();
    });
    let url = format!("http://{address}/slow");
    let host = EffectHost::new();
    let binding = host
        .bind_destination(
            DestinationGrant::loopback_fixture(&url, address, NetworkOperation::WebFetch).unwrap(),
        )
        .unwrap();
    let start = Instant::now();
    let mut seen = false;
    let result = host.execute(
        prepared(&host, &binding.id, "web_fetch", json!({"url":url})),
        &mut || {
            seen |= seen_rx.try_recv().is_ok();
            if seen {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(result.unwrap_err(), Error::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(closed_rx.recv_timeout(Duration::from_secs(3)).unwrap());
    join.join().unwrap();
    assert_eq!(host.binding(&binding.id).unwrap().revision, 1);
}

#[cfg(target_os = "macos")]
#[test]
fn system_volume_alias_cannot_read_an_outside_fixture() {
    let outside = tempfile::tempdir().unwrap();
    let path = fs::canonicalize(outside.path())
        .unwrap()
        .join("sentinel.txt");
    fs::write(&path, "OUTSIDE-ALIAS-CANARY").unwrap();
    let alias = PathBuf::from("/System/Volumes/Data").join(path.strip_prefix("/").unwrap());
    assert!(alias.exists());
    let host = EffectHost::new();
    let (id, _) = workspace(&host);
    let out = execute(
        &host,
        &id,
        "run_shell",
        json!({"command":format!("fixture read {}",alias.display())}),
    );
    assert_eq!(out["exit_code"], 23);
    assert!(!out.to_string().contains("OUTSIDE-ALIAS-CANARY"));
}
