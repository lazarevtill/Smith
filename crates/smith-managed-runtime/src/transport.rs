use crate::{Completion, ExecutionKind, Failure, Result, Target};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

/// Private transport authority; never exposed as an arbitrary URL/client.
#[derive(Clone)]
pub(crate) struct Endpoint {
    pub path: PathBuf,
    pub pid: u32,
    pub key: String,
    pub model: String,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn request(
    ep: &Endpoint,
    path: &'static str,
    body: Option<Vec<u8>>,
) -> Result<(u16, Vec<u8>)> {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::{client::conn::http1, Request};
    use hyper_util::rt::TokioIo;
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(&ep.path)
        .await
        .map_err(|_| Failure::Transport)?;
    verify_peer(&stream, ep.pid)?;
    // Ownership is checked on THIS connected stream before hyper sees it. No
    // request bytes (including health/auth) were written and no reconnect exists.
    let (mut sender, connection) = http1::Builder::new()
        .max_headers(64)
        .max_buf_size(32 * 1024)
        .handshake(TokioIo::new(stream))
        .await
        .map_err(|_| Failure::Transport)?;
    let mut auth = hyper::header::HeaderValue::from_str(&format!("Bearer {}", ep.key))
        .map_err(|_| Failure::Protocol)?;
    auth.set_sensitive(true);
    let req = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("Host", "localhost")
        .header("Connection", "close")
        .header("Authorization", auth)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.unwrap_or_default())))
        .map_err(|_| Failure::Protocol)?;
    let response = async {
        let mut response = sender
            .send_request(req)
            .await
            .map_err(|_| Failure::Transport)?;
        let status = response.status().as_u16();
        if response
            .headers()
            .get("content-length")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .is_some_and(|n| n > 2 * 1024 * 1024)
        {
            return Err(Failure::ResponseTooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(frame) = response.body_mut().frame().await {
            if let Ok(data) = frame.map_err(|_| Failure::Transport)?.into_data() {
                if bytes.len() + data.len() > 2 * 1024 * 1024 {
                    return Err(Failure::ResponseTooLarge);
                }
                bytes.extend_from_slice(&data);
            }
        }
        Ok((status, bytes))
    };
    tokio::pin!(response);
    tokio::pin!(connection);
    // Both futures live in the caller's task; cancellation drops the connection
    // immediately. There is no detached HTTP pump, pool, proxy, DNS, or redirect.
    tokio::select! {
        biased;
        result = &mut response => result,
        _ = &mut connection => {
            // The body/parser is authoritative. Darwin may report ENOTCONN
            // when hyper shuts down an already-closed peer after receiving the
            // complete response. Truncated/invalid responses still fail here.
            response.await
        }
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn request(_: &Endpoint, _: &'static str, _: Option<Vec<u8>>) -> Result<(u16, Vec<u8>)> {
    Err(Failure::UnsupportedPlatform)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn verify_peer(stream: &tokio::net::UnixStream, expected_pid: u32) -> Result<()> {
    let creds = stream.peer_cred().map_err(|_| Failure::EndpointOwnership)?;
    // The child is not reaped while any connection task is alive. This keeps its
    // PID reserved even if it exits between connect and this kernel query.
    if creds.pid().and_then(|p| u32::try_from(p).ok()) != Some(expected_pid)
        || creds.uid() != unsafe { libc::geteuid() }
    {
        return Err(Failure::EndpointOwnership);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        // SAFETY: fd is the live stream; buffer/length match pid_t; getsockopt
        // cannot retain pointers. Check real peer PID as well as Tokio's EPID.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        if rc != 0
            || len as usize != std::mem::size_of_val(&pid)
            || u32::try_from(pid).ok() != Some(expected_pid)
        {
            return Err(Failure::EndpointOwnership);
        }
    }
    Ok(())
}

pub(crate) async fn ready(ep: Endpoint) -> Result<()> {
    loop {
        match request(&ep, "/health", None).await {
            Ok((200, body)) => {
                let value: Value = serde_json::from_slice(&body).map_err(|_| Failure::Protocol)?;
                if value.get("status").and_then(Value::as_str) != Some("ok") {
                    return Err(Failure::Protocol);
                }
                let (status, body) = request(&ep, "/v1/models", None).await?;
                if status != 200 {
                    return Err(Failure::UnsupportedProtocol);
                }
                let value: Value = serde_json::from_slice(&body).map_err(|_| Failure::Protocol)?;
                let models = value
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or(Failure::Protocol)?;
                if models.len() != 1
                    || models[0].get("id").and_then(Value::as_str) != Some(ep.model.as_str())
                {
                    return Err(Failure::Protocol);
                }
                return Ok(());
            }
            Ok((503, _)) | Err(Failure::Transport) => {}
            Ok(_) => return Err(Failure::UnsupportedProtocol),
            Err(e) => return Err(e),
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

pub(crate) async fn generate(
    ep: Endpoint,
    body: Vec<u8>,
    target: Target,
    run_id: Uuid,
    execution_kind: ExecutionKind,
) -> Result<Completion> {
    let (status, bytes) = request(&ep, "/v1/chat/completions", Some(body)).await?;
    if status != 200 {
        return Err(Failure::Protocol);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Protocol)?;
    if value.get("model").and_then(Value::as_str) != Some(ep.model.as_str()) {
        return Err(Failure::Protocol);
    }
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or(Failure::Protocol)?;
    if choices.len() != 1 {
        return Err(Failure::UnsupportedCapability);
    }
    let message = &choices[0]["message"];
    if message.get("tool_calls").is_some_and(|v| !v.is_null())
        || message
            .get("reasoning_content")
            .is_some_and(|v| !v.is_null() && v != "")
    {
        return Err(Failure::UnsupportedCapability);
    }
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .ok_or(Failure::Protocol)?
        .to_owned();
    let finish_reason = choices[0]
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or(Failure::Protocol)?
        .to_owned();
    if !matches!(finish_reason.as_str(), "stop" | "length") {
        return Err(Failure::UnsupportedCapability);
    }
    let usage = value
        .get("usage")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|_| Failure::Protocol)?;
    Ok(Completion {
        target,
        run_id,
        text,
        finish_reason,
        usage,
        execution_kind,
    })
}

/// Observe a child's exit without releasing its PID while transport is active.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn exited_without_reaping(pid: u32) -> bool {
    // SAFETY: zeroed siginfo_t is a valid output buffer; WNOWAIT retains the zombie
    // and PID reservation until the actor aborts/joins all transport and waits.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0
            && info.si_pid() != 0
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn exited_without_reaping(_: u32) -> bool {
    false
}
