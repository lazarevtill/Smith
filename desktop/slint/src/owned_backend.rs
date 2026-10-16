//! Unix sidecar custody: parent retains the listening socket, launches one
//! exact backend, verifies private handshake plus authenticated HTTP readiness,
//! and reaps that child. No existing endpoint/process can be adopted.
use crate::{
    bridge::{Result, Transport},
    http,
};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read},
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
struct ChildState {
    child: Option<Child>,
    exit: Option<bool>,
}
struct Owner {
    listener: TcpListener,
    address: SocketAddr,
    secret: String,
    pid: u32,
    state: Mutex<ChildState>,
    readers: Mutex<Vec<JoinHandle<()>>>,
}
impl Owner {
    fn alive(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let Some(child) = state.child.as_mut() else {
            return Err("Owned backend has stopped.".into());
        };
        if let Some(status) = child
            .try_wait()
            .map_err(|_| "Cannot observe owned backend.")?
        {
            state.child.take();
            state.exit = Some(status.success());
            return Err("Owned backend exited. Native content must be cleared.".into());
        }
        Ok(())
    }
    fn stop(&self, timeout: Duration) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some(child) = state.child.as_mut() {
            if let Some(status) = child
                .try_wait()
                .map_err(|_| "Cannot observe owned backend before stopping.")?
            {
                state.child.take();
                state.exit = Some(status.success());
            }
        }
        if let Some(child) = state.child.as_mut() {
            #[cfg(unix)]
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGINT);
            }
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|_| "Cannot wait for backend shutdown.")?
                {
                    state.child.take();
                    state.exit = Some(status.success());
                    break;
                }
                if Instant::now() > deadline {
                    return Err("Owned backend has not finished draining. Its process and listener remain owned.".into());
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
        let clean = state.exit == Some(true);
        drop(state);
        for reader in self.readers.lock().unwrap().drain(..) {
            let _ = reader.join();
        }
        if clean {
            Ok(())
        } else {
            Err(
                "Owned backend exited unsuccessfully; clean storage shutdown is not confirmed."
                    .into(),
            )
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        // Fallback is deliberately not reported as a clean shutdown.
        let state = self.state.get_mut().unwrap();
        if let Some(mut child) = state.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        for reader in self.readers.get_mut().unwrap().drain(..) {
            let _ = reader.join();
        }
    }
}
pub struct OwnedBackend {
    owner: Arc<Owner>,
}
impl OwnedBackend {
    /// executable/profile are selected by the desktop host, not HTTP/tool
    /// arguments. Tests pass only a canonical disposable directory.
    #[cfg(unix)]
    pub fn launch(executable: &Path, profile: &Path) -> Result<Self> {
        use std::os::fd::OwnedFd;
        let executable = executable
            .canonicalize()
            .map_err(|_| "Selected backend executable is missing.")?;
        if !executable.is_file() {
            return Err("Selected backend is not a regular executable.".into());
        }
        let profile = profile
            .canonicalize()
            .map_err(|_| "Select an existing owned workspace directory.")?;
        if !profile.is_dir() {
            return Err("Workspace directory is invalid.".into());
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| "Cannot create parent-owned listener.")?;
        let address = listener
            .local_addr()
            .map_err(|_| "Cannot inspect owned listener.")?;
        let inherited: OwnedFd = listener
            .try_clone()
            .map_err(|_| "Cannot duplicate owned listener.")?
            .into();
        let mut entropy = [0; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut entropy))
            .map_err(|_| "Operating-system randomness unavailable.")?;
        let secret: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("TMPDIR", "/tmp")
            .env("SMITH_NEXT_PORT", "0")
            .env("SMITH_NEXT_DATA_DIR", &profile)
            .env("SMITH_NEXT_DESKTOP_OWNER", &secret)
            .env("SMITH_NEXT_DESKTOP_LISTENER_FD", "0")
            .stdin(Stdio::from(inherited))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|_| "Cannot launch the selected owned backend.")?;
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let owner = Arc::new(Owner {
            listener,
            address,
            secret,
            pid,
            state: Mutex::new(ChildState {
                child: Some(child),
                exit: None,
            }),
            readers: Mutex::new(vec![]),
        });
        let (ready, handshake) = mpsc::channel();
        let output = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut bytes = Vec::new();
            let result = reader
                .by_ref()
                .take(65537)
                .read_until(b'\n', &mut bytes)
                .map_err(|_| "Cannot read private backend handshake.".to_owned())
                .and_then(|n| {
                    if n == 0 || n > 65536 {
                        Err("Backend readiness output exceeded its bound.".into())
                    } else {
                        serde_json::from_slice::<Value>(&bytes)
                            .map_err(|_| "Invalid private backend handshake.".into())
                    }
                });
            let _ = ready.send(result);
            let mut discard = [0; 8192];
            while reader.read(&mut discard).is_ok_and(|n| n > 0) {}
        });
        let errors = thread::spawn(move || {
            let mut reader = stderr;
            let mut discard = [0; 8192];
            while reader.read(&mut discard).is_ok_and(|n| n > 0) {}
        });
        owner.readers.lock().unwrap().extend([output, errors]);
        let value = handshake
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| "Owned backend did not become ready before the deadline.")??;
        if value["smith_desktop"] != 1
            || value["pid"] != pid
            || value["owner"] != owner.secret
            || value["url"] != format!("http://{address}")
        {
            return Err(
                "Backend handshake does not match its exact process, owner and retained listener."
                    .into(),
            );
        }
        owner.alive()?;
        let identity = http::json(
            address,
            &owner.secret,
            "GET",
            "/api/desktop/ready",
            None,
            None,
        )?;
        if identity["smith_desktop"] != 1
            || identity["pid"] != pid
            || identity["owner"] != owner.secret
        {
            return Err("Owned listener did not authenticate its exact backend.".into());
        }
        owner.alive()?;
        Ok(Self { owner })
    }
    #[cfg(not(unix))]
    pub fn launch(_: &Path, _: &Path) -> Result<Self> {
        Err("Native sidecar custody has only been implemented for Unix; this platform is unqualified.".into())
    }
    pub fn transport(&self) -> OwnedTransport {
        OwnedTransport {
            owner: self.owner.clone(),
        }
    }
    pub fn stop(&self) -> Result<()> {
        self.owner.stop(Duration::from_secs(20))
    }
    pub fn pid(&self) -> u32 {
        self.owner.pid
    }
    pub fn reserved_address(&self) -> SocketAddr {
        self.owner.listener.local_addr().expect("retained listener")
    }
}
pub struct OwnedTransport {
    owner: Arc<Owner>,
}
impl Transport for OwnedTransport {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        self.owner.alive()?;
        let result = http::json(
            self.owner.address,
            &self.owner.secret,
            method,
            path,
            session,
            body,
        )?;
        self.owner.alive()?;
        Ok(result)
    }
    fn catalog_mutation(
        &mut self,
        method: &str,
        path: &str,
        session: &str,
        body: Value,
    ) -> std::result::Result<Value, http::CatalogMutationError> {
        use http::CatalogMutationError;
        self.owner
            .alive()
            .map_err(CatalogMutationError::NotSubmitted)?;
        let value = http::catalog_mutation(
            self.owner.address,
            &self.owner.secret,
            method,
            path,
            session,
            body,
        )?;
        self.owner
            .alive()
            .map_err(CatalogMutationError::OutcomeUnknown)?;
        Ok(value)
    }
    fn transcription_start(
        &mut self,
        session: &str,
        body: Value,
    ) -> std::result::Result<Value, crate::transcription::TranscriptionSubmitError> {
        use crate::transcription::TranscriptionSubmitError;
        self.owner
            .alive()
            .map_err(TranscriptionSubmitError::NotSubmitted)?;
        let result =
            http::transcription_start(self.owner.address, &self.owner.secret, session, body)?;
        self.owner
            .alive()
            .map_err(TranscriptionSubmitError::OutcomeUnknown)?;
        Ok(result)
    }
    fn transcription_file_start(
        &mut self,
        session: &str,
        body: Value,
    ) -> std::result::Result<Value, crate::transcription::TranscriptionSubmitError> {
        use crate::transcription::TranscriptionSubmitError;
        self.owner
            .alive()
            .map_err(TranscriptionSubmitError::NotSubmitted)?;
        let result =
            http::transcription_file_start(self.owner.address, &self.owner.secret, session, body)?;
        self.owner
            .alive()
            .map_err(TranscriptionSubmitError::OutcomeUnknown)?;
        Ok(result)
    }
    fn sensitive_request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: crate::profile_bridge::SensitiveBody,
    ) -> Result<Value> {
        self.owner.alive()?;
        let result = http::sensitive_json(
            self.owner.address,
            &self.owner.secret,
            method,
            path,
            session,
            body.value(),
        )?;
        drop(body);
        self.owner.alive()?;
        Ok(result)
    }
    fn upload(&mut self, session: &str, metadata: Value, bytes: &[u8]) -> Result<Value> {
        self.owner.alive()?;
        let result = http::upload(
            self.owner.address,
            &self.owner.secret,
            session,
            metadata,
            bytes,
        )?;
        self.owner.alive()?;
        Ok(result)
    }
    fn download(&mut self, path: &str, session: &str) -> Result<Vec<u8>> {
        self.owner.alive()?;
        let result = http::download(self.owner.address, &self.owner.secret, path, session)?;
        self.owner.alive()?;
        Ok(result)
    }
    fn historical_cards(
        &mut self,
        request: &crate::historical_message_bridge::CardsRequest,
        body: &mut crate::historical_message_bridge::ResponseBody,
    ) -> Result<crate::historical_message_bridge::Headers> {
        self.owner.alive()?;
        let result = http::historical_read(
            self.owner.address,
            &self.owner.secret,
            &request.path(),
            request.session(),
            crate::historical_message_contract::MAX_CARD_BYTES,
            body,
        );
        self.owner.alive()?;
        result
    }
    fn historical_image(
        &mut self,
        request: &crate::historical_message_bridge::ImageRequest,
        body: &mut crate::historical_message_bridge::ResponseBody,
    ) -> Result<crate::historical_message_bridge::Headers> {
        self.owner.alive()?;
        let result = http::historical_read(
            self.owner.address,
            &self.owner.secret,
            &request.path(),
            request.session(),
            crate::historical_message_contract::MAX_IMAGE_BYTES,
            body,
        );
        self.owner.alive()?;
        result
    }
    fn event_snapshot(&mut self, path: &str, session: &str) -> Result<Option<Value>> {
        self.owner.alive()?;
        let result = http::event(self.owner.address, &self.owner.secret, path, session)?;
        self.owner.alive()?;
        Ok(Some(result))
    }
}
