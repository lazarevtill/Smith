//! Restricted HTTP/1.1 over a numeric loopback socket retained by our owner.
//! No DNS, proxy, redirects, cookies, ambient credentials, or arbitrary URL.
use crate::bridge::Result;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};
fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}
pub(crate) fn wipe_string(value: &mut String) {
    // Zero is valid UTF-8. No allocation or formatting of the secret occurs.
    unsafe {
        wipe(value.as_mut_vec());
    }
    value.clear();
}
pub(crate) fn wipe_json(value: &mut Value) {
    match value {
        Value::String(s) => wipe_string(s),
        Value::Array(values) => values.iter_mut().for_each(wipe_json),
        Value::Object(values) => {
            for (mut key, mut value) in std::mem::take(values) {
                wipe_string(&mut key);
                wipe_json(&mut value);
            }
        }
        _ => {}
    }
}
struct SensitiveBytes(Vec<u8>);
impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}
struct SensitiveReply(Value);
impl Drop for SensitiveReply {
    fn drop(&mut self) {
        wipe_json(&mut self.0);
    }
}
const MAX: usize = 2 * 1024 * 1024;
pub(crate) fn json(
    address: SocketAddr,
    owner: &str,
    method: &str,
    path: &str,
    session: Option<&str>,
    body: Option<Value>,
) -> Result<Value> {
    let bytes = body
        .map(|v| serde_json::to_vec(&v).unwrap())
        .unwrap_or_default();
    let mut response = start(
        address,
        owner,
        method,
        path,
        session,
        &bytes,
        "application/json",
        if method == "POST" && path == "/api/media/transcription/start" {
            3 * 1024 * 1024
        } else if method == "POST" && path == "/api/media/transcription/start-file" {
            12 * 1024 * 1024
        } else {
            MAX
        },
        MAX,
        Duration::from_secs(5),
    )?;
    let code = response.code;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk()? {
        if bytes.len() + chunk.len() > MAX {
            return Err("Backend response exceeded the native limit.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Backend returned invalid JSON.")?;
    if !(200..300).contains(&code) {
        return Err(format!(
            "HTTP {code}: {}",
            value["error"]
                .as_str()
                .unwrap_or("Backend request rejected.")
        ));
    }
    Ok(value)
}
/// An authoritative catalog 4xx means this call did not admit/commit new work.
/// It says nothing about a previous lost attempt with the same request ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogMutationError {
    NotSubmitted(String),
    OutcomeUnknown(String),
}
pub(crate) fn catalog_mutation(
    address: SocketAddr,
    owner: &str,
    method: &str,
    path: &str,
    session: &str,
    body: Value,
) -> std::result::Result<Value, CatalogMutationError> {
    use CatalogMutationError::{NotSubmitted, OutcomeUnknown};
    let entry = path.strip_prefix("/api/local-catalog/entries/");
    let allowed = matches!(
        (method, path),
        ("POST", "/api/local-catalog/entries")
            | ("POST", "/api/local-catalog/select")
            | ("POST", "/api/local-catalog/prepare")
            | ("POST", "/api/local-catalog/setup")
    ) || entry.is_some_and(|s| {
        let (id, suffix) = s.split_once('/').unwrap_or((s, ""));
        let id_ok = id.strip_prefix("local-").is_some_and(|s| {
            s.len() == 16
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        id_ok
            && matches!(
                (method, suffix),
                ("PUT", "") | ("DELETE", "") | ("POST", "rename") | ("POST", "restore")
            )
    });
    if !allowed {
        return Err(NotSubmitted("This route is not a catalog mutation.".into()));
    }
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| NotSubmitted("Cannot encode catalog request.".into()))?;
    if bytes.len() > 256 * 1024 {
        return Err(NotSubmitted(
            "Catalog request exceeds its bounded input size.".into(),
        ));
    }
    let mut response = start(
        address,
        owner,
        method,
        path,
        Some(session),
        &bytes,
        "application/json",
        256 * 1024,
        MAX,
        Duration::from_secs(5),
    )
    .map_err(OutcomeUnknown)?;
    let code = response.code;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().map_err(OutcomeUnknown)? {
        if bytes.len() + chunk.len() > MAX {
            return Err(OutcomeUnknown(
                "Catalog response exceeded its native limit.".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| OutcomeUnknown("Catalog response was not valid JSON.".into()))?;
    if (200..300).contains(&code) {
        return Ok(value);
    }
    if (400..500).contains(&code) {
        if let Some(error) = value["error"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096)
        {
            return Err(NotSubmitted(error.into()));
        }
    }
    Err(OutcomeUnknown(
        "Catalog outcome is unconfirmed. Inspect the retained request ID.".into(),
    ))
}
/// Transcription has a durable client submission ID. Only a validated typed
/// backend pre-admission rejection may discard that recovery handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptionStartError {
    NotSubmitted(String),
    OutcomeUnknown(String),
}
impl std::fmt::Display for TranscriptionStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSubmitted(s) | Self::OutcomeUnknown(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for TranscriptionStartError {}
pub(crate) fn transcription_start(
    address: SocketAddr,
    owner: &str,
    session: &str,
    body: Value,
) -> std::result::Result<Value, TranscriptionStartError> {
    transcription_start_at(address, owner, session, body, false)
}
pub(crate) fn transcription_file_start(
    address: SocketAddr,
    owner: &str,
    session: &str,
    body: Value,
) -> std::result::Result<Value, TranscriptionStartError> {
    transcription_start_at(address, owner, session, body, true)
}
fn transcription_start_at(
    address: SocketAddr,
    owner: &str,
    session: &str,
    body: Value,
    normalize: bool,
) -> std::result::Result<Value, TranscriptionStartError> {
    let cap = if normalize {
        12 * 1024 * 1024
    } else {
        3 * 1024 * 1024
    };
    let path = if normalize {
        "/api/media/transcription/start-file"
    } else {
        "/api/media/transcription/start"
    };
    use TranscriptionStartError::{NotSubmitted, OutcomeUnknown};
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| NotSubmitted("Cannot encode transcription request.".into()))?;
    if bytes.len() > cap {
        return Err(NotSubmitted(
            "Transcription request exceeds the native limit.".into(),
        ));
    }
    let mut response = start(
        address,
        owner,
        "POST",
        path,
        Some(session),
        &bytes,
        "application/json",
        cap,
        MAX,
        Duration::from_secs(5),
    )
    .map_err(OutcomeUnknown)?;
    let code = response.code;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().map_err(OutcomeUnknown)? {
        if bytes.len() + chunk.len() > MAX {
            return Err(OutcomeUnknown(
                "Backend response exceeded the native limit.".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| OutcomeUnknown("Backend returned invalid transcription JSON.".into()))?;
    if (200..300).contains(&code) {
        return Ok(value);
    }
    if matches!(code, 400 | 409 | 413 | 415)
        && matches!(
            value["code"].as_str(),
            Some(
                "transcription_invalid"
                    | "transcription_audio_unsupported"
                    | "transcription_language_unsupported"
                    | "transcription_limit"
                    | "transcription_busy"
                    | "transcription_normalizer_unavailable"
                    | "transcription_unavailable"
                    | "transcription_request_conflict"
                    | "transcription_cancelled"
            )
        )
    {
        return Err(NotSubmitted(
            value["error"]
                .as_str()
                .unwrap_or("Transcription was rejected before admission.")
                .to_owned(),
        ));
    }
    Err(OutcomeUnknown("Transcription admission was not confirmed. Inspect the retained request before taking another action.".into()))
}
/// Owned key request buffers are scrubbed on every return path. Backend errors
/// are intentionally static here: never reflect a submitted credential.
pub(crate) fn sensitive_json(
    address: SocketAddr,
    owner: &str,
    method: &str,
    path: &str,
    session: Option<&str>,
    body: &Value,
) -> Result<Value> {
    let bytes =
        SensitiveBytes(serde_json::to_vec(body).map_err(|_| "Cannot encode sensitive request.")?);
    let mut response = start(
        address,
        owner,
        method,
        path,
        session,
        &bytes.0,
        "application/json",
        MAX,
        MAX,
        Duration::from_secs(5),
    )?;
    drop(bytes);
    if !(200..300).contains(&response.code) {
        return Err("The sensitive operation was rejected. Check workspace status.".into());
    }
    let mut bytes = SensitiveBytes(Vec::new());
    while let Some(chunk) = response.chunk()? {
        let chunk = SensitiveBytes(chunk);
        if bytes.0.len() + chunk.0.len() > MAX {
            return Err("Backend response exceeded the native limit.".into());
        }
        bytes.0.extend_from_slice(&chunk.0);
    }
    let value = SensitiveReply(
        serde_json::from_slice(&bytes.0).map_err(|_| "Backend returned invalid JSON.")?,
    );
    // A successful import operation only acknowledges a public operation ID.
    let id = value
        .0
        .get("operation_id")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .ok_or("Invalid sensitive-operation acknowledgement.")?;
    Ok(serde_json::json!({"operation_id":id}))
}
pub(crate) fn event(address: SocketAddr, owner: &str, path: &str, session: &str) -> Result<Value> {
    let mut response = start(
        address,
        owner,
        "GET",
        path,
        Some(session),
        &[],
        "application/json",
        MAX,
        MAX,
        Duration::from_secs(5),
    )?;
    if response.code != 200 {
        return Err(format!(
            "Response status {} while reading the current answer.",
            response.code
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk()? {
        if bytes.len() + chunk.len() > MAX {
            return Err("Answer snapshot exceeded the native limit.".into());
        }
        bytes.extend_from_slice(&chunk);
        while let Some(n) = bytes.windows(2).position(|w| w == b"\n\n") {
            let frame = String::from_utf8(bytes.drain(..n + 2).collect())
                .map_err(|_| "Invalid answer encoding.")?;
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                return serde_json::from_str(&data).map_err(|_| "Invalid answer snapshot.".into());
            }
        }
    }
    Err("Answer stream ended without a snapshot.".into())
}
struct DeadlineSocket {
    socket: TcpStream,
    deadline: Instant,
}
impl DeadlineSocket {
    fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "whole request deadline")
            })
    }
}
impl Read for DeadlineSocket {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(bytes)
    }
}
impl Write for DeadlineSocket {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}
struct Response {
    security_headers: std::collections::BTreeMap<String, String>,
    reader: BufReader<DeadlineSocket>,
    code: u16,
    remaining: usize,
    chunked: bool,
    finished: bool,
    total: usize,
    deadline: Instant,
    limit: usize,
}
fn line(reader: &mut BufReader<DeadlineSocket>, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    let n = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "Backend header read failed.")?;
    if n == 0 || n > limit || !bytes.ends_with(b"\r\n") {
        return Err("Malformed or oversized HTTP header.".into());
    }
    String::from_utf8(bytes[..n - 2].to_vec()).map_err(|_| "Invalid HTTP header encoding.".into())
}
#[allow(clippy::too_many_arguments)]
fn start(
    address: SocketAddr,
    owner: &str,
    method: &str,
    path: &str,
    session: Option<&str>,
    bytes: &[u8],
    content_type: &str,
    request_limit: usize,
    response_limit: usize,
    timeout: Duration,
) -> Result<Response> {
    if !address.ip().is_loopback()
        || !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE")
        || !path.starts_with("/api/")
        || path.bytes().any(|b| b.is_ascii_control() || b == b' ')
        || session.is_some_and(|s| s.bytes().any(|b| b.is_ascii_control()))
    {
        return Err("Invalid owned-backend request.".into());
    }
    if content_type.bytes().any(|b| b.is_ascii_control()) || bytes.len() > request_limit {
        return Err("Native request limit exceeded.".into());
    }
    let deadline = Instant::now() + timeout;
    let socket = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|_| "Owned backend listener is unavailable.")?;
    let mut stream = DeadlineSocket { socket, deadline };
    let session = session
        .map(|s| format!("X-Smith-Profile-Session: {s}\r\n"))
        .unwrap_or_default();
    let head=format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nX-Smith-Desktop-Owner: {owner}\r\n{session}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len());
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(bytes))
        .map_err(|_| "Owned backend request failed.")?;
    let mut reader = BufReader::new(stream);
    let status = line(&mut reader, 4096)?;
    if !status.starts_with("HTTP/1.1 ") {
        return Err("Unsupported backend HTTP version.".into());
    }
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("Malformed backend status.")?;
    let mut security_headers = std::collections::BTreeMap::new();
    let mut total = status.len();
    let mut length = None;
    let mut chunked = false;
    loop {
        let value = line(&mut reader, 8192)?;
        total += value.len();
        if total > 16384 {
            return Err("Backend headers exceeded the limit.".into());
        }
        if value.is_empty() {
            break;
        }
        let (name, value) = value.split_once(':').ok_or("Malformed backend header.")?;
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "content-type" | "cache-control" | "x-content-type-options" | "x-smith-content-sha256"
        ) && security_headers
            .insert(lower, value.trim().to_owned())
            .is_some()
        {
            return Err("Duplicate backend security header.".into());
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err("Duplicate backend content length.".into());
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "Invalid response size.")?,
            );
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.trim().eq_ignore_ascii_case("chunked") {
                return Err("Unsupported response transfer encoding.".into());
            }
            chunked = true;
        }
        if name.eq_ignore_ascii_case("content-encoding")
            && !value.trim().eq_ignore_ascii_case("identity")
        {
            return Err("Compressed responses are not accepted by the native bridge.".into());
        }
    }
    if (chunked && length.is_some())
        || (!chunked && length.is_none())
        || length.is_some_and(|n| n > response_limit)
    {
        return Err("Unbounded or ambiguous backend response.".into());
    }
    Ok(Response {
        security_headers,
        reader,
        code,
        remaining: length.unwrap_or(0),
        chunked,
        finished: false,
        total: 0,
        deadline,
        limit: response_limit,
    })
}
impl Response {
    fn chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.finished {
            return Ok(None);
        }
        if Instant::now() > self.deadline {
            return Err("Backend response deadline exceeded.".into());
        }
        let n = if self.chunked {
            let value = line(&mut self.reader, 128)?;
            if value.contains(';') {
                return Err("Chunk extensions are not accepted.".into());
            }
            usize::from_str_radix(&value, 16).map_err(|_| "Invalid HTTP chunk size.")?
        } else {
            self.remaining
        };
        if n == 0 {
            self.finished = true;
            if self.chunked && !line(&mut self.reader, 8192)?.is_empty() {
                return Err("HTTP trailers are not accepted.".into());
            }
            return Ok(None);
        }
        if n > self.limit || self.total + n > self.limit {
            return Err("Backend response limit exceeded.".into());
        }
        let mut bytes = vec![0; n];
        self.reader
            .read_exact(&mut bytes)
            .map_err(|_| "Backend response ended early.")?;
        self.total += n;
        if self.chunked {
            let mut crlf = [0; 2];
            self.reader
                .read_exact(&mut crlf)
                .map_err(|_| "Truncated chunk boundary.")?;
            if crlf != *b"\r\n" {
                return Err("Invalid chunk boundary.".into());
            }
        } else {
            self.remaining = 0;
            self.finished = true;
        }
        Ok(Some(bytes))
    }
}

/// Binary routes retain the same owner/session and exact numeric listener.
/// Upload/download has a finite 15-second whole-operation bound and 8 MiB cap.
pub(crate) fn upload(
    address: SocketAddr,
    owner: &str,
    session: &str,
    metadata: Value,
    bytes: &[u8],
) -> Result<Value> {
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("Document exceeds 8 MiB.".into());
    }
    let request_id = metadata["request_id"]
        .as_str()
        .filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .ok_or("Invalid document request identity.")?;
    let boundary = format!("smith-native-{request_id}");
    let metadata = serde_json::to_vec(&metadata).map_err(|_| "Invalid upload metadata.")?;
    if metadata.len() > 16 * 1024 {
        return Err("Upload metadata exceeds its limit.".into());
    }
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"\r\nContent-Type: application/json\r\n\r\n").into_bytes();
    body.extend(metadata);
    body.extend(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"selected-file\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
    body.extend(bytes);
    body.extend(format!("\r\n--{boundary}--\r\n").as_bytes());
    let mut response = start(
        address,
        owner,
        "POST",
        "/api/media/uploads",
        Some(session),
        &body,
        &format!("multipart/form-data; boundary={boundary}"),
        8 * 1024 * 1024 + 32 * 1024,
        MAX,
        Duration::from_secs(15),
    )?;
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk()? {
        data.extend(chunk);
    }
    let value: Value = serde_json::from_slice(&data).map_err(|_| "Invalid upload response.")?;
    if !(200..300).contains(&response.code) {
        return Err(format!(
            "HTTP {}: {}",
            response.code,
            value["error"].as_str().unwrap_or("Upload rejected.")
        ));
    }
    Ok(value)
}
pub(crate) fn download(
    address: SocketAddr,
    owner: &str,
    path: &str,
    session: &str,
) -> Result<Vec<u8>> {
    let parts = path.split('/').collect::<Vec<_>>();
    let original_import = matches!(parts.as_slice(),
        ["", "api", "legacy-import", "artifacts", id, "bytes"]
        if id.len() == 36 && id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'));
    if !original_import && (!path.starts_with("/api/media/") || !path.ends_with("/original")) {
        return Err("Unsupported document download route.".into());
    }
    let mut response = start(
        address,
        owner,
        "GET",
        path,
        Some(session),
        &[],
        "application/json",
        0,
        8 * 1024 * 1024,
        Duration::from_secs(15),
    )?;
    if response.code != 200 {
        return Err(format!(
            "HTTP {}: document download rejected.",
            response.code
        ));
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk()? {
        data.extend(chunk);
    }
    Ok(data)
}

// Only called by OwnedTransport using paths minted by private CardsRequest or
// ImageRequest constructors. This is not the generic document download route.
pub(crate) fn historical_read(
    address: SocketAddr,
    owner: &str,
    path: &str,
    session: &str,
    limit: usize,
    body: &mut crate::historical_message_bridge::ResponseBody,
) -> Result<crate::historical_message_bridge::Headers> {
    let mut response = start(
        address,
        owner,
        "GET",
        path,
        Some(session),
        &[],
        "application/json",
        0,
        limit,
        Duration::from_secs(15),
    )?;
    if response.code != 200 {
        return Err(format!(
            "HTTP {}: original-message read rejected.",
            response.code
        ));
    }
    while let Some(chunk) = response.chunk()? {
        body.write_all(&chunk)
            .map_err(|_| "Original-message response exceeds its limit.")?;
    }
    let mut headers = response.security_headers;
    Ok(crate::historical_message_bridge::Headers {
        status: response.code,
        content_type: headers.remove("content-type").unwrap_or_default(),
        content_sha256: headers.remove("x-smith-content-sha256"),
        cache_control: headers.remove("cache-control").unwrap_or_default(),
        content_type_options: headers.remove("x-content-type-options").unwrap_or_default(),
    })
}
