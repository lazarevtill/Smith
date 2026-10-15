//! Explicit selected-code execution for setup, never discovery or adoption.
use crate::{plan, Artifact, Cancellation, Failure, Result, RuntimeCompatibility};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::oneshot,
    time,
};

const OUTPUT_LIMIT: usize = 128 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);
const REAP: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeProbeSummary {
    pub id: String,
    pub executable: Artifact,
    pub commands: [&'static str; 2],
    pub deadline_seconds_per_command: u64,
    pub max_output_bytes_per_stream: usize,
    pub execution_notice: &'static str,
    pub provenance: &'static str,
}
pub struct RuntimeProbePlan {
    summary: RuntimeProbeSummary,
    stamp: plan::Stamp,
    prepared: Instant,
}
/// Non-cloneable, non-deserializable explicit approval for this exact executable.
pub struct ApprovedRuntimeProbe(RuntimeProbePlan);
impl RuntimeProbePlan {
    pub async fn prepare(mut executable: Artifact, cancel: &Cancellation) -> Result<Self> {
        if !plan::platform_supported() {
            return Err(Failure::UnsupportedPlatform);
        }
        let stamp = plan::inspect(&mut executable, true, cancel).await?;
        let mut summary = RuntimeProbeSummary { id: String::new(), executable, commands: ["--help", "--list-devices"], deadline_seconds_per_command: DEADLINE.as_secs(), max_output_bytes_per_stream: OUTPUT_LIMIT, execution_notice: "Runs the selected installed native executable with fixed inspection arguments; selected code is trusted to honor them. No model path, credentials, inherited environment, or arbitrary flags are supplied.", provenance: "user_selected_local_file_hash_not_publisher_verification" };
        // Include a nonce: consent is one prepared operation, not reusable by hash.
        summary.id = Sha256::digest(
            serde_json::to_vec(&(&summary, uuid::Uuid::new_v4())).map_err(|_| Failure::Protocol)?,
        )
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
        Ok(Self {
            summary,
            stamp,
            prepared: Instant::now(),
        })
    }
    pub fn summary(&self) -> &RuntimeProbeSummary {
        &self.summary
    }
    pub fn approve(self, exact_id: &str) -> Result<ApprovedRuntimeProbe> {
        if self.summary.id != exact_id || self.prepared.elapsed() > Duration::from_secs(300) {
            return Err(Failure::ConsentMismatch);
        }
        Ok(ApprovedRuntimeProbe(self))
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct ProbeOutput {
    pub argument: &'static str,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub reaped: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceBackend {
    Cpu,
    Metal,
    Cuda,
    Hip,
    Sycl,
    Unknown,
}
#[derive(Clone, Debug, Serialize)]
pub struct DeviceObservation {
    pub name: String,
    pub description: String,
    pub backend: DeviceBackend,
    pub evidence: &'static str,
}
#[derive(Clone, Debug, Serialize)]
pub struct RuntimeProbeReport {
    pub probe_id: String,
    pub executable: Artifact,
    pub help: ProbeOutput,
    pub inventory: ProbeOutput,
    pub devices: Vec<DeviceObservation>,
    pub compatibility: RuntimeCompatibility,
    pub inference_verified: bool,
}
/// Owns a cancellable worker. Dropping a pending handle latches cancellation; the worker
/// retains and reaps the child. Hosts should register and await `wait` on shutdown.
pub struct RuntimeProbe {
    cancel: Cancellation,
    result: oneshot::Receiver<Result<RuntimeProbeReport>>,
    completed: bool,
}
impl RuntimeProbe {
    pub fn start(approved: ApprovedRuntimeProbe, cancel: Cancellation) -> Self {
        let (tx, result) = oneshot::channel();
        let owned_cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = tx.send(run(approved.0, owned_cancel).await);
        });
        Self {
            cancel,
            result,
            completed: false,
        }
    }
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
    pub async fn wait(&mut self) -> Result<RuntimeProbeReport> {
        let result = (&mut self.result).await;
        self.completed = true;
        result.map_err(|_| Failure::Cleanup)?
    }
}
impl Drop for RuntimeProbe {
    fn drop(&mut self) {
        if !self.completed {
            self.cancel.cancel();
        }
    }
}
async fn run(plan: RuntimeProbePlan, cancel: Cancellation) -> Result<RuntimeProbeReport> {
    if plan.prepared.elapsed() > Duration::from_secs(300) {
        return Err(Failure::ConsentMismatch);
    }
    let mut executable = plan.summary.executable.clone();
    if plan::inspect(&mut executable, true, &cancel).await? != plan.stamp {
        return Err(Failure::ArtifactChanged);
    }
    let help = command(&plan, "--help", &cancel).await?;
    let compatibility = RuntimeCompatibility::observed(
        &executable,
        &format!("{}\n{}", help.stdout, help.stderr),
        help.exit_code == Some(0),
    );
    let inventory = command(&plan, "--list-devices", &cancel).await?;
    let devices = if inventory.exit_code == Some(0) {
        observations(&format!("{}\n{}", inventory.stdout, inventory.stderr))
    } else {
        Vec::new()
    };
    Ok(RuntimeProbeReport {
        probe_id: plan.summary.id,
        executable,
        help,
        inventory,
        devices,
        compatibility,
        inference_verified: false,
    })
}
async fn read_output(mut input: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let n = input.read(&mut buffer).await.map_err(|_| Failure::Io)?;
        if n == 0 {
            return Ok(bytes);
        }
        if bytes.len() + n > OUTPUT_LIMIT {
            return Err(Failure::ResponseTooLarge);
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
}
async fn command(
    plan: &RuntimeProbePlan,
    argument: &'static str,
    cancel: &Cancellation,
) -> Result<ProbeOutput> {
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    let path = &plan.summary.executable.path;
    if std::fs::canonicalize(path).map_err(|_| Failure::ArtifactChanged)? != *path
        || plan::stamp(&std::fs::metadata(path).map_err(|_| Failure::ArtifactChanged)?)?
            != plan.stamp
    {
        return Err(Failure::ArtifactChanged);
    }
    let directory = plan::socket_directory()?;
    let mut child = Command::new(path)
        .arg(argument)
        .env_clear()
        .env("LANG", "C")
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Failure::Spawn)?;
    let stdout = child.stdout.take().ok_or(Failure::Spawn)?;
    let stderr = child.stderr.take().ok_or(Failure::Spawn)?;
    let outcome = {
        let combined = async {
            let (status, stdout, stderr) = tokio::try_join!(
                async { child.wait().await.map_err(|_| Failure::Cleanup) },
                read_output(stdout),
                read_output(stderr)
            )?;
            Ok::<_, Failure>((status, stdout, stderr))
        };
        tokio::select! { biased;
            _ = cancel.cancelled() => Err(Failure::Cancelled),
            result = time::timeout(DEADLINE, combined) => result.map_err(|_| Failure::StartupTimeout).and_then(|x| x),
        }
    };
    match outcome {
        Ok((status, stdout, stderr)) => {
            if plan::stamp(&std::fs::metadata(path).map_err(|_| Failure::ArtifactChanged)?)?
                != plan.stamp
            {
                return Err(Failure::ArtifactChanged);
            }
            #[cfg(unix)]
            let signal = {
                use std::os::unix::process::ExitStatusExt;
                status.signal()
            };
            #[cfg(not(unix))]
            let signal = None;
            let stdout = String::from_utf8(stdout).map_err(|_| Failure::Protocol)?;
            let stderr = String::from_utf8(stderr).map_err(|_| Failure::Protocol)?;
            Ok(ProbeOutput {
                argument,
                exit_code: status.code(),
                signal,
                stdout,
                stderr,
                reaped: true,
            })
        }
        Err(error) => {
            // Never return a successful cancellation/deadline until owned reap.
            let _ = child.start_kill();
            time::timeout(REAP, child.wait())
                .await
                .map_err(|_| Failure::Cleanup)?
                .map_err(|_| Failure::Cleanup)?;
            Err(error)
        }
    }
}
fn observations(text: &str) -> Vec<DeviceObservation> {
    let mut devices = Vec::new();
    let mut listing = false;
    for line in text.lines().take(2048) {
        if line.trim() == "Available devices:" {
            listing = true;
            continue;
        }
        if !listing {
            continue;
        }
        let Some((name, description)) = line.trim().split_once(':') else {
            continue;
        };
        if name.is_empty()
            || name.len() > 80
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || description.len() > 512
            || description.chars().any(char::is_control)
            || devices.iter().any(|d: &DeviceObservation| d.name == name)
        {
            continue;
        }
        let upper = name.to_ascii_uppercase();
        let backend = if upper.starts_with("CPU") {
            DeviceBackend::Cpu
        } else if upper.starts_with("METAL")
            || upper.strip_prefix("MTL").is_some_and(|index| {
                !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            DeviceBackend::Metal
        } else if upper.starts_with("CUDA") {
            DeviceBackend::Cuda
        } else if upper.starts_with("HIP") || upper.starts_with("ROCM") {
            DeviceBackend::Hip
        } else if upper.starts_with("SYCL") {
            DeviceBackend::Sycl
        } else {
            DeviceBackend::Unknown
        };
        devices.push(DeviceObservation {
            name: name.into(),
            description: description.trim().into(),
            backend,
            evidence: "selected_executable_list_devices_only_not_inference",
        });
        if devices.len() == 32 {
            break;
        }
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_metal_device_tokens_preserve_exact_identity_and_do_not_infer_support() {
        let devices = observations("Available devices:\n  MTL0: Apple M1 (10922 MiB, 10922 MiB free)\n  BLAS: Accelerate (0 MiB, 0 MiB free)\n  MTL: not an indexed device\n  MTLfoo: not an indexed device\n  MTL0_extra: not an indexed device");
        assert_eq!(devices.len(), 5);
        assert_eq!(devices[0].backend, DeviceBackend::Metal);
        assert_eq!(devices[0].name, "MTL0");
        assert_eq!(
            devices[0].description,
            "Apple M1 (10922 MiB, 10922 MiB free)"
        );
        assert_eq!(
            devices[0].evidence,
            "selected_executable_list_devices_only_not_inference"
        );
        assert!(devices[1..]
            .iter()
            .all(|device| device.backend == DeviceBackend::Unknown));
    }

    #[test]
    fn same_name_devices_and_replaced_indices_remain_distinct_observations() {
        let devices = observations("Available devices:\n  MTL0: Apple M1\n  MTL1: Apple M1");
        assert_eq!(
            devices.len(),
            2,
            "description ambiguity must remain visible to the host"
        );
        assert!(devices
            .iter()
            .all(|device| device.backend == DeviceBackend::Metal));
        assert_ne!(devices[0].name, devices[1].name);
        let replacement = observations("Available devices:\n  MTL2: Apple M1");
        assert_eq!(replacement.len(), 1);
        assert_eq!(
            replacement[0].name, "MTL2",
            "no cached ordinal or identity substitution"
        );
        assert!(
            observations("  MTL0: Apple M1").is_empty(),
            "only the explicit device table is admitted"
        );
    }
}
