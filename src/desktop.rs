//! Ephemeral ownership boundary for the packaged desktop window.
use crate::*;
use std::{io::Write, net::SocketAddr};

// Deliberately no Debug/Serialize; only the owned child's private stdout pipe
// receives the launch handshake. This secret is never a URL/query parameter.
#[derive(Clone)]
pub(crate) struct Owner {
    secret: Arc<String>,
    parent: u32,
}
impl Owner {
    pub(crate) fn from_environment() -> Result<Option<Self>, String> {
        match std::env::var("SMITH_NEXT_DESKTOP_OWNER") {
            Err(std::env::VarError::NotPresent) => Ok(None),
            Ok(value) if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) => {
                // Do not propagate desktop custody into later model/tool children.
                std::env::remove_var("SMITH_NEXT_DESKTOP_OWNER");
                if std::env::var("SMITH_NEXT_DESKTOP_LISTENER_FD").as_deref() != Ok("0") {
                    return Err("Desktop launch requires its parent-owned listening socket.".into());
                }
                std::env::remove_var("SMITH_NEXT_DESKTOP_LISTENER_FD");
                #[cfg(unix)]
                let parent = unsafe { libc::getppid() } as u32;
                #[cfg(not(unix))]
                let parent = 0;
                if parent <= 1 {
                    return Err("Desktop launch parent is unavailable on this platform.".into());
                }
                Ok(Some(Self {
                    secret: Arc::new(value),
                    parent,
                }))
            }
            _ => Err("Invalid desktop launch ownership.".into()),
        }
    }
    /// Parent keeps its own copy open until the native view is destroyed and
    /// this child is reaped. A dead child's TCP port cannot be taken over.
    pub(crate) fn listener(&self) -> Result<tokio::net::TcpListener, String> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            // Foundation Process maps the reviewed listener to child stdin.
            let socket = unsafe { std::net::TcpListener::from_raw_fd(0) };
            let fd = socket.as_raw_fd();
            for (option, expected) in [(libc::SO_TYPE, libc::SOCK_STREAM), (libc::SO_REUSEPORT, 0)]
            {
                let mut value: libc::c_int = 0;
                let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
                let result = unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        option,
                        (&mut value as *mut libc::c_int).cast(),
                        &mut len,
                    )
                };
                if result != 0 || value != expected {
                    return Err("Desktop listener is not an inherited listening TCP socket.".into());
                }
            }
            let address = socket
                .local_addr()
                .map_err(|_| "Cannot inspect inherited desktop listener.".to_owned())?;
            if address.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                || address.port() == 0
            {
                return Err("Desktop listener must use an assigned numeric loopback port.".into());
            }
            socket
                .set_nonblocking(true)
                .map_err(|_| "Cannot configure desktop listener.".to_owned())?;
            // SO_ACCEPTCONN is unavailable on macOS. A bounded nonblocking
            // accept validates listening state; no client is admitted before
            // the private startup handshake. Discard a queued pre-ready peer.
            let probe = unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) };
            if probe >= 0 {
                unsafe {
                    libc::close(probe);
                }
            } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
                return Err("Inherited desktop socket is not listening.".into());
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err("Cannot confine desktop listener ownership.".into());
            }
            tokio::net::TcpListener::from_std(socket)
                .map_err(|_| "Cannot activate inherited desktop listener.".into())
        }
        #[cfg(not(unix))]
        {
            Err("Inherited desktop listeners are not qualified on this platform.".into())
        }
    }
    fn matches(&self, candidate: &str) -> bool {
        candidate.len() == self.secret.len()
            && candidate
                .bytes()
                .zip(self.secret.bytes())
                .fold(0u8, |v, (a, b)| v | (a ^ b))
                == 0
    }
    pub(crate) fn announce(&self, address: SocketAddr) -> Result<(), String> {
        let message = json!({"smith_desktop":1,"pid":std::process::id(),"owner":self.secret.as_str(),"url":format!("http://{address}")});
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{message}")
            .and_then(|_| stdout.flush())
            .map_err(|_| "Desktop launch handshake could not be delivered.".into())
    }
}
/// Parent death closes the local application through the normal drain path.
/// PID reuse cannot hide reparenting: the child checks its current actual parent.
pub(crate) async fn shutdown_signal(owner: Option<Owner>) {
    let parent_gone = async move {
        if let Some(owner) = owner {
            loop {
                #[cfg(unix)]
                if unsafe { libc::getppid() } as u32 != owner.parent {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } else {
            std::future::pending::<()>().await
        }
    };
    #[cfg(unix)]
    {
        let terminated = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminated=>{},_=parent_gone=>{}}
    }
    #[cfg(not(unix))]
    {
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=parent_gone=>{}}
    }
}

#[derive(Clone)]
struct DesktopState {
    owner: Owner,
    host: String,
}
async fn guard(State(state): State<DesktopState>, req: Request, next: Next) -> Response {
    if req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        != Some(state.host.as_str())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let headers = req
        .headers()
        .get_all("x-smith-desktop-owner")
        .iter()
        .collect::<Vec<_>>();
    let cookies = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|s| s.trim().split_once('='))
        .filter(|(name, _)| *name == "smith_desktop_owner")
        .map(|(_, v)| v)
        .collect::<Vec<_>>();
    // Reject ambiguity rather than preferring one of two conflicting owners.
    if headers.len() > 1
        || cookies.len() > 1
        || (headers.is_empty() && cookies.is_empty())
        || headers
            .iter()
            .any(|h| h.to_str().map_or(true, |v| !state.owner.matches(v)))
        || cookies.iter().any(|v| !state.owner.matches(v))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}
pub(crate) fn routes(router: Router, owner: Option<Owner>, address: SocketAddr) -> Router {
    let Some(owner) = owner else {
        return router;
    };
    let ready_owner = owner.clone();
    router.route("/api/desktop/ready", get(move || {
        let owner = ready_owner.clone();
        async move { ([(header::CACHE_CONTROL,"no-store")], Json(json!({"smith_desktop":1,"pid":std::process::id(),"owner":owner.secret.as_str()}))) }
    })).layer(middleware::from_fn_with_state(DesktopState { owner, host: address.to_string() }, guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test]
    async fn desktop_requires_exact_launch_owner_for_every_resource() {
        let secret = "a".repeat(64);
        let app = routes(
            Router::new().route("/", get(|| async { "local" })),
            Some(Owner {
                secret: Arc::new(secret.clone()),
                parent: 0,
            }),
            "127.0.0.1:18471".parse().unwrap(),
        );
        for (host, header, cookie, expected) in [
            ("127.0.0.1:18471", None, None, 403),
            ("localhost:18471", Some(secret.as_str()), None, 403),
            ("127.0.0.1:18472", Some(secret.as_str()), None, 403),
            ("127.0.0.1:18471", Some("wrong"), None, 403),
            ("127.0.0.1:18471", Some(secret.as_str()), None, 200),
            (
                "127.0.0.1:18471",
                Some(secret.as_str()),
                Some("smith_desktop_owner=wrong"),
                403,
            ),
        ] {
            let mut req = axum::http::Request::builder()
                .uri("/")
                .header(header::HOST, host);
            if let Some(v) = header {
                req = req.header("x-smith-desktop-owner", v);
            }
            if let Some(v) = cookie {
                req = req.header(header::COOKIE, v);
            }
            assert_eq!(
                app.clone()
                    .oneshot(req.body(axum::body::Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
                    .as_u16(),
                expected
            );
        }
        let req = axum::http::Request::builder()
            .uri("/api/desktop/ready")
            .header(header::HOST, "127.0.0.1:18471")
            .header(header::COOKIE, format!("smith_desktop_owner={secret}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}
