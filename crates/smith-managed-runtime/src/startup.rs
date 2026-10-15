//! Bounded startup output custody. This module is not a placement proof.
//!
//! The supervisor freezes retention before publishing Ready. Readers continue
//! draining afterward, but no later output enters diagnostics or counters.
use crate::{ExecutionKind, Target};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    task::JoinHandle,
};

const BYTE_LIMIT: usize = 64 * 1024;
const LINE_LIMIT: usize = 4096;
const LINE_COUNT_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupStream {
    Stdout,
    Stderr,
}
impl StartupStream {
    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StartupIdentity {
    pub target: Target,
    pub plan_id: String,
    pub executable_sha256: String,
    pub execution_kind: ExecutionKind,
    pub pid: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct StartupStreamSummary {
    pub inspected_bytes: usize,
    pub retained_lines: usize,
    pub truncated: bool,
    pub invalid_text: bool,
    pub incomplete_line: bool,
    pub read_failed: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupUnknownReason {
    NoSupportedRecords,
    IncompleteOutput,
    ConflictingOrMalformedRecords,
    UnsupportedBufferLabel,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedBufferKind {
    Cpu,
    CpuMapped,
    MetalShared,
    MetalMapped,
    MetalPrivate,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReportedOffload {
    pub offloaded_layers: u32,
    pub total_layers: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReportedModelBuffer {
    pub kind: ReportedBufferKind,
    /// Runtime-local index, never an OS inventory/device ownership proof.
    pub runtime_device_index: Option<u16>,
    /// Rounded units printed by the runtime, not exact allocated byte counts.
    pub size_mib_hundredths: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartupPlacement {
    Unknown {
        reason: StartupUnknownReason,
    },
    /// Claims from this exact selected child, not measured kernel execution,
    /// resident pages, total VRAM use, guaranteed fit, or model correctness.
    RuntimeReported {
        format: &'static str,
        offload: ReportedOffload,
        model_buffers: Vec<ReportedModelBuffer>,
    },
}

/// Safe bounded metadata only. Raw output is deliberately a separate type.
#[derive(Clone, Debug, Serialize)]
pub struct StartupObservation {
    pub identity: StartupIdentity,
    pub source: &'static str,
    pub retention_closed: bool,
    pub max_bytes_per_stream: usize,
    pub max_line_bytes: usize,
    pub max_lines_per_stream: usize,
    pub stdout: StartupStreamSummary,
    pub stderr: StartupStreamSummary,
    /// Parsed source-compatible process statements, never a hardware profiler.
    pub placement: StartupPlacement,
    pub kernel_execution_verified: bool,
}

/// Private-startup text for explicitly trusted local diagnostics. Never expose
/// this through an HTTP snapshot, logs, telemetry or a general Debug formatter.
/// Key bytes are redacted, but model paths and runtime messages may be private.
/// This type intentionally implements neither Serialize nor Debug.
pub struct StartupDiagnosticLine {
    stream: StartupStream,
    text: String,
}
impl StartupDiagnosticLine {
    pub fn stream(&self) -> StartupStream {
        self.stream
    }
    pub fn text(&self) -> &str {
        &self.text
    }
}
/// Explicit trusted diagnostics only, with no Serialize/Debug implementation.
pub struct StartupDiagnostics {
    pub observation: StartupObservation,
    lines: Vec<StartupDiagnosticLine>,
}
impl StartupDiagnostics {
    pub fn lines(&self) -> &[StartupDiagnosticLine] {
        &self.lines
    }
}

#[derive(Default)]
struct StreamState {
    summary: StartupStreamSummary,
    pending: Vec<u8>,
    dropping_line: bool,
    lines: Vec<String>,
}
struct State {
    identity: StartupIdentity,
    closed: bool,
    key: String,
    streams: [StreamState; 2],
}
#[derive(Clone)]
pub(crate) struct StartupCapture(Arc<Mutex<State>>);
impl StartupCapture {
    pub(crate) fn new(identity: StartupIdentity, key: String) -> Self {
        Self(Arc::new(Mutex::new(State {
            identity,
            closed: false,
            key,
            streams: Default::default(),
        })))
    }
    pub(crate) fn reader<R>(&self, mut reader: R, stream: StartupStream) -> JoinHandle<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let capture = self.clone();
        tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => {
                        capture.end(stream, false);
                        break;
                    }
                    Ok(count) => capture.push(stream, &buffer[..count]),
                    Err(_) => {
                        capture.end(stream, true);
                        break;
                    }
                }
            }
        })
    }
    fn push(&self, stream: StartupStream, bytes: &[u8]) {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return;
        }
        // Borrow the immutable redaction key separately from stream assembly.
        let State { key, streams, .. } = &mut *state;
        let value = &mut streams[stream.index()];
        for byte in bytes {
            if value.summary.inspected_bytes >= BYTE_LIMIT {
                value.summary.truncated = true;
                value.pending.clear();
                return;
            }
            value.summary.inspected_bytes += 1;
            if *byte == b'\n' {
                if !value.dropping_line {
                    finish_line(value, key);
                }
                value.pending.clear();
                value.dropping_line = false;
            } else if !value.dropping_line {
                if value.pending.len() == LINE_LIMIT {
                    value.summary.truncated = true;
                    value.pending.clear();
                    value.dropping_line = true;
                } else {
                    value.pending.push(*byte);
                }
            }
        }
    }
    fn end(&self, stream: StartupStream, failed: bool) {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return;
        }
        let value = &mut state.streams[stream.index()];
        value.summary.read_failed |= failed;
        value.summary.incomplete_line |= !value.pending.is_empty() || value.dropping_line;
        value.pending.clear();
    }
    /// Must happen before the actor publishes Ready or admits any generation.
    pub(crate) fn freeze(&self) {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return;
        }
        state.closed = true;
        for value in &mut state.streams {
            value.summary.incomplete_line |= !value.pending.is_empty() || value.dropping_line;
            value.pending.fill(0);
            value.pending.clear();
        }
        // The endpoint still owns its live key. Retention no longer needs a copy.
        let mut key = std::mem::take(&mut state.key).into_bytes();
        key.fill(0);
    }
    pub(crate) fn observation(&self) -> Option<StartupObservation> {
        let state = self.0.lock().unwrap();
        state.closed.then(|| observation(&state))
    }
    pub(crate) fn diagnostics(&self) -> Option<StartupDiagnostics> {
        let state = self.0.lock().unwrap();
        if !state.closed {
            return None;
        }
        let lines = [StartupStream::Stdout, StartupStream::Stderr]
            .into_iter()
            .flat_map(|stream| {
                state.streams[stream.index()]
                    .lines
                    .iter()
                    .map(move |text| StartupDiagnosticLine {
                        stream,
                        text: text.clone(),
                    })
            })
            .collect();
        Some(StartupDiagnostics {
            observation: observation(&state),
            lines,
        })
    }
}
fn observation(state: &State) -> StartupObservation {
    StartupObservation {
        identity: state.identity.clone(),
        source: "owned_process_startup_output",
        retention_closed: true,
        max_bytes_per_stream: BYTE_LIMIT,
        max_line_bytes: LINE_LIMIT,
        max_lines_per_stream: LINE_COUNT_LIMIT,
        stdout: state.streams[0].summary.clone(),
        stderr: state.streams[1].summary.clone(),
        placement: crate::startup_parse::parse(
            state
                .streams
                .iter()
                .flat_map(|v| v.lines.iter().map(String::as_str)),
            state.streams.iter().any(|v| {
                v.summary.truncated
                    || v.summary.invalid_text
                    || v.summary.incomplete_line
                    || v.summary.read_failed
            }),
        ),
        kernel_execution_verified: false,
    }
}
fn finish_line(value: &mut StreamState, key: &str) {
    if value.lines.len() == LINE_COUNT_LIMIT {
        value.summary.truncated = true;
        return;
    }
    let Ok(text) = std::str::from_utf8(&value.pending) else {
        value.summary.invalid_text = true;
        return;
    };
    let text = text.trim_end_matches('\r');
    if text.chars().any(|c| c.is_control() && c != '\t') {
        value.summary.invalid_text = true;
        return;
    }
    if text.is_empty() {
        return;
    }
    let text = if key.is_empty() {
        text.to_owned()
    } else {
        text.replace(key, if key.len() >= 10 { "[redacted]" } else { "*" })
    };
    value.lines.push(text);
    value.summary.retained_lines = value.lines.len();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    fn capture() -> StartupCapture {
        StartupCapture::new(
            StartupIdentity {
                target: Target {
                    owner: uuid::Uuid::new_v4(),
                    generation: 1,
                },
                plan_id: "p".repeat(64),
                executable_sha256: "e".repeat(64),
                execution_kind: ExecutionKind::CompatibleFixture,
                pid: 42,
            },
            "synthetic-secret".into(),
        )
    }
    #[test]
    fn split_secret_is_redacted_and_freeze_excludes_later_prompt_text() {
        let capture = capture();
        capture.push(StartupStream::Stderr, b"key=synthetic-");
        capture.push(StartupStream::Stderr, b"secret\nallocation record\n");
        assert!(capture.diagnostics().is_none());
        capture.freeze();
        let before = capture.diagnostics().unwrap();
        capture.push(StartupStream::Stderr, b"synthetic private prompt\n");
        capture.end(StartupStream::Stderr, true);
        let after = capture.diagnostics().unwrap();
        assert_eq!(before.observation.stderr, after.observation.stderr);
        assert_eq!(after.lines().len(), 2);
        assert_eq!(after.lines()[0].text(), "key=[redacted]");
        let public = serde_json::to_string(&after.observation).unwrap();
        assert!(!public.contains("allocation record"));
        assert!(!public.contains("synthetic-secret"));
        assert!(matches!(
            after.observation.placement,
            StartupPlacement::Unknown { .. }
        ));
    }
    #[test]
    fn byte_line_count_and_text_bounds_fail_closed() {
        let capture = capture();
        capture.push(StartupStream::Stdout, &vec![b'x'; BYTE_LIMIT * 2]);
        capture.push(StartupStream::Stderr, &b"x\n".repeat(LINE_COUNT_LIMIT + 1));
        capture.freeze();
        let result = capture.diagnostics().unwrap();
        assert_eq!(result.observation.stdout.inspected_bytes, BYTE_LIMIT);
        assert!(result.observation.stdout.truncated);
        assert!(result.observation.stderr.truncated);
        assert_eq!(result.lines().len(), LINE_COUNT_LIMIT);
        let capture = self::capture();
        capture.push(StartupStream::Stdout, b"bad\xff\ncontrol\0\npartial");
        capture.freeze();
        let result = capture.diagnostics().unwrap();
        assert!(result.observation.stdout.invalid_text);
        assert!(result.observation.stdout.incomplete_line);
        assert!(result.lines().is_empty());
    }
    #[tokio::test]
    async fn readers_keep_draining_after_bounds_and_after_retention_closes() {
        let capture = capture();
        let (mut writer, reader) = tokio::io::duplex(1024);
        let task = capture.reader(reader, StartupStream::Stderr);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            writer.write_all(&vec![b'x'; BYTE_LIMIT * 2]).await.unwrap();
            capture.freeze();
            writer.write_all(&vec![b'y'; BYTE_LIMIT * 2]).await.unwrap();
            drop(writer);
            task.await.unwrap();
        })
        .await
        .unwrap();
        let result = capture.diagnostics().unwrap();
        assert_eq!(result.observation.stderr.inspected_bytes, BYTE_LIMIT);
        assert!(result.observation.stderr.truncated);
        assert!(result.lines().is_empty());
    }
}
