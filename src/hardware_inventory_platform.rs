//! Read-only Linux proc/sysfs and Windows Win32/DXGI inventory groundwork.
//!
//! This module is deliberately independent of the macOS provider. `observe`
//! returns its existing observation contract plus explicitly scoped evidence.
//! No model, compute device/context, command queue, shell, registry, package or
//! driver installation is involved. Call on a blocking worker. A two-second
//! deadline and process-wide single-flight gate bound work; an OS call that
//! stalls cannot be forcibly interrupted, and no timed-out result is reused.
//!
//! Native Linux/Windows runtime qualification is still required. Synthetic
//! mapping/ABI/parser tests on macOS do not establish native OS support.
#![cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]

use crate::hardware_inventory::{IdentityScope, InventoryError, Observation};
use serde::Serialize;
use sha2::{Digest, Sha256};
use smith_acceleration::{
    Architecture, Device, DeviceClass, HardwareInventory, MemoryPool, OperatingSystem, Vendor,
};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, InventoryError>;
const MAX_BYTES: u64 = (1 << 53) - 1;
const MAX_GPUS: usize = 31;
#[cfg(any(target_os = "linux", test))]
const MAX_CPU_COUNT: usize = 1024;
const TIMEOUT: Duration = Duration::from_secs(2);
static IN_FLIGHT: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static PROCESS_SALT: OnceLock<String> = OnceLock::new();

#[derive(Clone, Debug, Serialize)]
pub struct PlatformObservation {
    #[serde(flatten)]
    pub observation: Observation,
    pub host_memory: HostMemoryEvidence,
    pub gpu_devices: Vec<GpuEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostMemoryEvidence {
    pub source: &'static str,
    pub total_physical_bytes: u64,
    /// OS-wide snapshot, not this process's cgroup/job/address-space allowance.
    pub available_physical_bytes: Option<u64>,
    /// Linux MemAvailable includes a kernel estimate of reclaimable memory.
    pub availability_is_os_estimate: bool,
    pub process_memory_limit_observed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VideoMemoryBudget {
    pub source: &'static str,
    pub node_index: u32,
    /// QueryVideoMemoryInfo budget for *this querying process*, not a future
    /// child process and not physical GPU capacity/free VRAM.
    pub budget_bytes: u64,
    pub current_usage_bytes: u64,
    pub available_for_reservation_bytes: u64,
    pub current_reservation_bytes: u64,
    /// Saturating remaining process budget. No allocation/reservation is made.
    pub remaining_process_budget_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GpuEvidence {
    pub device_id: String,
    pub source: &'static str,
    pub name: Option<String>,
    pub vendor_id: Option<u32>,
    pub hardware_device_id: Option<u32>,
    pub driver_name: Option<String>,
    pub driver_version: Option<String>,
    pub reported_dedicated_video_bytes: Option<u64>,
    pub reported_dedicated_system_bytes: Option<u64>,
    /// DXGI maximum shared-system usage, never extra RAM or current free RAM.
    pub reported_shared_system_limit_bytes: Option<u64>,
    pub reported_vram_used_bytes: Option<u64>,
    pub local_process_budget: Option<VideoMemoryBudget>,
    pub non_local_process_budget: Option<VideoMemoryBudget>,
    /// Neither PCI vendor nor DXGI shared-memory capacity proves UMA topology.
    pub unified_memory: Option<bool>,
}

/// No configurable filesystem root, helper path, environment override or API
/// endpoint. macOS continues to use hardware_inventory::observe().
pub fn observe() -> Result<PlatformObservation> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        let gate = IN_FLIGHT
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone();
        bounded(gate, TIMEOUT, || {
            #[cfg(target_os = "linux")]
            let raw = linux::read()?;
            #[cfg(target_os = "windows")]
            let raw = windows::read()?;
            assemble(raw)
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    Err(InventoryError::UnsupportedPlatform)
}

fn bounded<T: Send + 'static>(
    gate: Arc<AtomicBool>,
    timeout: Duration,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    if gate
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(InventoryError::Busy);
    }
    struct Reset(Arc<AtomicBool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let reset = Reset(gate);
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("smith-platform-inventory".into())
        .spawn(move || {
            let result = work();
            drop(reset);
            let _ = sender.send(result);
        })
        .map_err(|_| InventoryError::NativeUnavailable("cannot start inventory worker"))?;
    match receiver.recv_timeout(timeout) {
        Ok(value) => value,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(InventoryError::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(InventoryError::NativeUnavailable(
            "inventory worker ended without result",
        )),
    }
}

fn now() -> Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| InventoryError::InvalidObservation("system clock"))?
            .as_millis(),
    )
    .map_err(|_| InventoryError::InvalidObservation("system clock overflow"))
}
fn process_salt() -> String {
    PROCESS_SALT
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}
fn opaque(namespace: &str, salt: &str, identity: &str) -> String {
    let mut hash = Sha256::new();
    for part in ["smith.platform-inventory.v1", namespace, salt, identity] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("{namespace}:{digest}")
}
fn text_valid(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}
fn bytes_valid(bytes: u64) -> bool {
    bytes <= MAX_BYTES
}
fn vendor(id: Option<u32>) -> Vendor {
    match id {
        Some(0x10de) => Vendor::Nvidia,
        Some(0x1002 | 0x1022) => Vendor::Amd,
        Some(0x8086) => Vendor::Intel,
        Some(0x106b) => Vendor::Apple,
        Some(0x17cb | 0x5143) => Vendor::Qualcomm,
        Some(_) => Vendor::Other,
        None => Vendor::Unknown,
    }
}

struct RawGpu {
    identity: String,
    evidence: GpuEvidence,
}
struct RawInventory {
    os: OperatingSystem,
    architecture: Architecture,
    os_version: String,
    logical: u16,
    physical: Option<u16>,
    active: Option<u16>,
    cpu_vendor: Vendor,
    salt: String,
    identity_scope: IdentityScope,
    memory: HostMemoryEvidence,
    gpus: Vec<RawGpu>,
    limitations: Vec<&'static str>,
}

fn assemble(raw: RawInventory) -> Result<PlatformObservation> {
    if !(1..=1024).contains(&raw.logical)
        || !text_valid(&raw.os_version, 128)
        || !text_valid(&raw.salt, 256)
        || raw.gpus.len() > MAX_GPUS
        || raw.physical.is_some_and(|n| n == 0 || n > raw.logical)
        || raw.active.is_some_and(|n| n == 0 || n > raw.logical)
        || raw.memory.total_physical_bytes == 0
        || !bytes_valid(raw.memory.total_physical_bytes)
        || raw
            .memory
            .available_physical_bytes
            .is_some_and(|n| n > raw.memory.total_physical_bytes)
    {
        return Err(InventoryError::InvalidObservation(
            "platform inventory bounds",
        ));
    }
    let host = opaque("host-memory", &raw.salt, "0");
    let mut pools = vec![MemoryPool {
        id: host.clone(),
        total_bytes: Some(raw.memory.total_physical_bytes),
        available_bytes: None,
    }];
    let mut devices = vec![Device {
        id: opaque("cpu", &raw.salt, "0"),
        class: DeviceClass::Cpu,
        vendor: raw.cpu_vendor,
        memory_pool_id: host.clone(),
        driver_version: None,
    }];
    let mut evidence = Vec::new();
    let mut unique = BTreeSet::new();
    for gpu in raw.gpus {
        if !text_valid(&gpu.identity, 4096) || !unique.insert(gpu.identity.clone()) {
            return Err(InventoryError::InvalidObservation(
                "GPU identity duplicate or bounds",
            ));
        }
        let mut item = gpu.evidence;
        if item.name.as_ref().is_some_and(|v| !text_valid(v, 512))
            || item
                .driver_name
                .as_ref()
                .is_some_and(|v| !text_valid(v, 128))
            || item
                .driver_version
                .as_ref()
                .is_some_and(|v| !text_valid(v, 128))
            || [
                item.reported_dedicated_video_bytes,
                item.reported_dedicated_system_bytes,
                item.reported_shared_system_limit_bytes,
                item.reported_vram_used_bytes,
            ]
            .into_iter()
            .flatten()
            .any(|n| !bytes_valid(n))
            || item.unified_memory.is_some()
        {
            return Err(InventoryError::InvalidObservation(
                "GPU evidence bounds or unproven topology",
            ));
        }
        for budget in [&item.local_process_budget, &item.non_local_process_budget]
            .into_iter()
            .flatten()
        {
            if [
                budget.budget_bytes,
                budget.current_usage_bytes,
                budget.available_for_reservation_bytes,
                budget.current_reservation_bytes,
            ]
            .into_iter()
            .any(|n| !bytes_valid(n))
                || budget.node_index != 0
                || budget.remaining_process_budget_bytes
                    != budget
                        .budget_bytes
                        .saturating_sub(budget.current_usage_bytes)
            {
                return Err(InventoryError::InvalidObservation("DXGI budget bounds"));
            }
        }
        item.device_id = opaque("gpu", &raw.salt, &gpu.identity);
        let pool = opaque("unknown-gpu-memory", &raw.salt, &gpu.identity);
        // This is an unknown topology placeholder, not an additional measured
        // physical pool. Both values remain unknown so a strict fit must block.
        pools.push(MemoryPool {
            id: pool.clone(),
            total_bytes: None,
            available_bytes: None,
        });
        devices.push(Device {
            id: item.device_id.clone(),
            class: DeviceClass::Gpu,
            vendor: vendor(item.vendor_id),
            memory_pool_id: pool,
            driver_version: item.driver_version.clone(),
        });
        evidence.push(item);
    }
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    pools.sort_by(|a, b| a.id.cmp(&b.id));
    evidence.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    let inventory = HardwareInventory {
        generation: uuid::Uuid::new_v4().to_string(),
        os: raw.os,
        architecture: raw.architecture,
        os_version: raw.os_version,
        logical_cpu_count: raw.logical,
        host_memory_pool_id: host,
        memory_pools: pools,
        devices,
    };
    inventory
        .validate()
        .map_err(|_| InventoryError::InvalidObservation("planner inventory contract"))?;
    let mut limitations = raw.limitations;
    limitations.extend([
        "Hardware enumeration is not installed-adapter, compute, inference, placement or performance qualification.",
        "GPU physical-pool topology is unknown; dedicated/shared counters and process budgets are evidence only, never added to host RAM.",
        "Host availability is an OS-wide snapshot; process/container/job limits are not measured, so planner availability remains unknown.",
        "Native Linux/Windows runtime qualification has not been performed for this implementation.",
    ]);
    if evidence.is_empty() {
        limitations.push("No GPU was visible through the selected enumeration API; this does not prove no accelerator exists.");
    }
    Ok(PlatformObservation {
        observation: Observation {
            platform_evidence: None,
            schema_version: 1,
            source: match raw.os {
                OperatingSystem::Linux => "native_linux_proc_sysfs",
                OperatingSystem::Windows => "native_windows_win32_dxgi",
                _ => return Err(InventoryError::UnsupportedPlatform),
            },
            observed_unix_ms: now()?,
            inventory,
            process_architecture: std::env::consts::ARCH,
            physical_cpu_count: raw.physical,
            active_cpu_count: raw.active,
            identity_scope: raw.identity_scope,
            metal_devices: Vec::new(),
            memory_sample: None,
            limitations,
            backend_runtime_tested: false,
            model_placement_observed: false,
        },
        host_memory: raw.memory,
        gpu_devices: evidence,
    })
}

fn empty_gpu(source: &'static str) -> GpuEvidence {
    GpuEvidence {
        device_id: String::new(),
        source,
        name: None,
        vendor_id: None,
        hardware_device_id: None,
        driver_name: None,
        driver_version: None,
        reported_dedicated_video_bytes: None,
        reported_dedicated_system_bytes: None,
        reported_shared_system_limit_bytes: None,
        reported_vram_used_bytes: None,
        local_process_budget: None,
        non_local_process_budget: None,
        unified_memory: None,
    }
}

#[cfg(any(target_os = "linux", test))]
fn meminfo(text: &str) -> Result<HostMemoryEvidence> {
    if text.len() > 65536 {
        return Err(InventoryError::InvalidObservation("meminfo length"));
    }
    let (mut total, mut available) = (None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let destination = match key {
            "MemTotal" => &mut total,
            "MemAvailable" => &mut available,
            _ => continue,
        };
        let fields: Vec<_> = value.split_ascii_whitespace().collect();
        if destination.is_some()
            || fields.len() != 2
            || fields[1] != "kB"
            || !fields[0].bytes().all(|b| b.is_ascii_digit())
        {
            return Err(InventoryError::InvalidObservation("meminfo fields"));
        }
        *destination = fields[0]
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(1024))
            .filter(|n| bytes_valid(*n));
        if destination.is_none() {
            return Err(InventoryError::InvalidObservation("meminfo byte overflow"));
        }
    }
    let total = total
        .filter(|n| *n > 0)
        .ok_or(InventoryError::InvalidObservation("missing MemTotal"))?;
    if available.is_some_and(|n| n > total) {
        return Err(InventoryError::InvalidObservation(
            "MemAvailable exceeds total",
        ));
    }
    Ok(HostMemoryEvidence {
        source: "proc_meminfo",
        total_physical_bytes: total,
        available_physical_bytes: available,
        availability_is_os_estimate: true,
        process_memory_limit_observed: false,
    })
}

#[cfg(any(target_os = "linux", test))]
fn cpu_list(text: &str) -> Result<Vec<u32>> {
    let text = text.trim();
    if text.is_empty() || text.len() > 8192 {
        return Err(InventoryError::InvalidObservation("CPU list length"));
    }
    let mut ids = BTreeSet::new();
    for component in text.split(',') {
        let mut fields = component.split('-');
        let parse = |s: &str| {
            s.parse::<u32>().ok().filter(|n| {
                *n <= 1_048_575 && !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
            })
        };
        let start = fields
            .next()
            .and_then(parse)
            .ok_or(InventoryError::InvalidObservation("CPU list syntax"))?;
        let end = match fields.next() {
            Some(v) => parse(v).ok_or(InventoryError::InvalidObservation("CPU range syntax"))?,
            None => start,
        };
        if fields.next().is_some() || end < start || (end - start) as usize >= MAX_CPU_COUNT {
            return Err(InventoryError::InvalidObservation("CPU range bounds"));
        }
        for id in start..=end {
            if !ids.insert(id) || ids.len() > MAX_CPU_COUNT {
                return Err(InventoryError::InvalidObservation(
                    "CPU list overlap or bound",
                ));
            }
        }
    }
    Ok(ids.into_iter().collect())
}

#[cfg(any(target_os = "linux", test))]
fn hex_id(text: &str) -> Result<u32> {
    let digits = text
        .trim()
        .strip_prefix("0x")
        .ok_or(InventoryError::InvalidObservation("sysfs hex prefix"))?;
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(InventoryError::InvalidObservation("sysfs hex ID"));
    }
    u32::from_str_radix(digits, 16)
        .map_err(|_| InventoryError::InvalidObservation("sysfs hex ID overflow"))
}
#[cfg(any(target_os = "linux", test))]
fn decimal_bytes(text: &str) -> Result<u64> {
    let text = text.trim();
    text.parse::<u64>()
        .ok()
        .filter(|n| bytes_valid(*n) && !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .ok_or(InventoryError::InvalidObservation("sysfs byte counter"))
}
#[cfg(any(target_os = "linux", test))]
fn pci_address(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && b.iter()
            .enumerate()
            .all(|(i, c)| [4, 7, 10].contains(&i) || c.is_ascii_hexdigit())
        && b[11] <= b'7'
}
#[cfg(any(target_os = "linux", test))]
fn drm_card(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|n| !n.is_empty() && n.len() <= 6 && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        ffi::CStr,
        fs::{self, OpenOptions},
        io::Read,
        os::{
            fd::{AsRawFd, IntoRawFd},
            unix::fs::OpenOptionsExt,
        },
        path::{Path, PathBuf},
    };
    const PROC: libc::c_long = 0x9fa0;
    const SYS: libc::c_long = 0x62656572;
    struct Reads {
        bytes: usize,
        files: usize,
    }
    impl Reads {
        fn text(&mut self, path: &Path, cap: usize, optional: bool) -> Result<Option<String>> {
            self.files += 1;
            if self.files > 4096 || self.bytes > 8 * 1024 * 1024 {
                return Err(InventoryError::InvalidObservation("inventory read budget"));
            }
            let expected = if path.starts_with("/proc") {
                PROC
            } else if path.starts_with("/sys") {
                SYS
            } else {
                return Err(InventoryError::InvalidObservation("inventory fixed root"));
            };
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)
            {
                Ok(file) => file,
                Err(e)
                    if optional
                        && matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                        ) =>
                {
                    return Ok(None)
                }
                Err(_) => return Err(InventoryError::NativeUnavailable("fixed proc/sysfs read")),
            };
            let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
            if !file
                .metadata()
                .map_err(|_| InventoryError::NativeUnavailable("proc/sysfs metadata"))?
                .is_file()
                || unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0
                || stat.f_type != expected
            {
                return Err(InventoryError::InvalidObservation(
                    "unexpected proc/sysfs filesystem or file type",
                ));
            }
            let remaining = (8 * 1024 * 1024usize).saturating_sub(self.bytes);
            let cap = cap.min(remaining);
            let mut bytes = Vec::new();
            file.take((cap + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| InventoryError::NativeUnavailable("bounded proc/sysfs read"))?;
            self.bytes += bytes.len();
            if bytes.len() > cap {
                return Err(InventoryError::InvalidObservation("proc/sysfs file size"));
            }
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| InventoryError::InvalidObservation("proc/sysfs text encoding"))
        }
        fn required(&mut self, path: &str, cap: usize) -> Result<String> {
            self.text(Path::new(path), cap, false)?
                .ok_or(InventoryError::NativeUnavailable(
                    "required proc/sysfs field",
                ))
        }
    }
    fn entries(path: &str) -> Result<Vec<String>> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| InventoryError::NativeUnavailable("sysfs enumeration"))?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(dir.as_raw_fd(), &mut stat) } != 0 || stat.f_type != SYS {
            return Err(InventoryError::InvalidObservation(
                "sysfs enumeration filesystem",
            ));
        }
        // Enumerate the same descriptor whose filesystem was checked above.
        // Do not reopen a path between admission and directory iteration.
        let fd = dir.into_raw_fd();
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(InventoryError::NativeUnavailable("sysfs directory stream"));
        }
        struct Directory(*mut libc::DIR);
        impl Drop for Directory {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let stream = Directory(stream);
        let mut names = Vec::new();
        loop {
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if unsafe { *libc::__errno_location() } != 0 {
                    return Err(InventoryError::NativeUnavailable(
                        "sysfs directory iteration",
                    ));
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                .to_str()
                .map_err(|_| InventoryError::InvalidObservation("sysfs entry encoding"))?;
            if matches!(name, "." | "..") {
                continue;
            }
            if name.len() > 256 || names.len() == 4096 {
                return Err(InventoryError::InvalidObservation("sysfs entry length"));
            }
            names.push(name.to_owned());
        }
        names.sort();
        Ok(names)
    }
    fn device_path(path: &Path) -> Result<PathBuf> {
        let path = fs::canonicalize(path).map_err(|_| {
            InventoryError::NativeUnavailable("sysfs device changed during enumeration")
        })?;
        if !path.starts_with("/sys/devices") || path.as_os_str().len() > 4096 {
            return Err(InventoryError::InvalidObservation(
                "sysfs device outside kernel tree",
            ));
        }
        Ok(path)
    }
    pub(super) fn read() -> Result<RawInventory> {
        let mut reads = Reads { bytes: 0, files: 0 };
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut uts) } != 0 {
            return Err(InventoryError::NativeUnavailable("uname"));
        }
        let machine = unsafe { CStr::from_ptr(uts.machine.as_ptr()) }
            .to_str()
            .map_err(|_| InventoryError::InvalidObservation("uname encoding"))?;
        let architecture = match machine {
            "x86_64" => Architecture::X86_64,
            "aarch64" => Architecture::Aarch64,
            _ => return Err(InventoryError::UnsupportedPlatform),
        };
        let os_version = reads
            .required("/proc/sys/kernel/osrelease", 128)?
            .trim()
            .to_owned();
        let memory = meminfo(&reads.required("/proc/meminfo", 65536)?)?;
        let cpus = cpu_list(&reads.required("/sys/devices/system/cpu/online", 8192)?)?;
        let mut cores = BTreeSet::new();
        let mut complete = true;
        for cpu in &cpus {
            let root = PathBuf::from(format!("/sys/devices/system/cpu/cpu{cpu}/topology"));
            let package = reads.text(&root.join("physical_package_id"), 32, true)?;
            let core = reads.text(&root.join("core_id"), 32, true)?;
            match (package, core) {
                (Some(p), Some(c)) => {
                    let p = p.trim().parse::<u32>().ok();
                    let c = c.trim().parse::<u32>().ok();
                    if let (Some(p), Some(c)) = (p, c) {
                        cores.insert((p, c));
                    } else {
                        complete = false;
                    }
                }
                _ => complete = false,
            }
        }
        // Do not read /proc/cpuinfo just to infer vendor: on some platforms
        // that broad record also contains serials. Topology and architecture
        // are sufficient here; CPU vendor remains explicitly unknown.
        let cpu_vendor = Vendor::Unknown;
        let boot = reads
            .text(Path::new("/proc/sys/kernel/random/boot_id"), 64, true)?
            .and_then(|s| uuid::Uuid::parse_str(s.trim()).ok())
            .map(|u| u.to_string());
        let (salt, identity_scope) = match boot {
            Some(v) => (v, IdentityScope::OsBootRegistryLifetime),
            None => (process_salt(), IdentityScope::ProcessLifetime),
        };
        let mut device_paths = BTreeSet::new();
        let mut limitations = vec!["Linux PCI display-class and DRM-card enumeration does not enumerate every possible NPU/compute accelerator.",
            "Linux device IDs identify a kernel path within the current boot; a hot replacement can reuse a path, so fresh observation generations must invalidate plans.",
            "Linux sysfs module version can be absent; kernel version is not substituted for GPU driver version."];
        match entries("/sys/bus/pci/devices") {
            Ok(names) => {
                for name in names {
                    if !pci_address(&name) {
                        return Err(InventoryError::InvalidObservation("PCI address syntax"));
                    }
                    let path = device_path(&Path::new("/sys/bus/pci/devices").join(name))?;
                    let class = reads.text(&path.join("class"), 32, true)?;
                    if class
                        .as_deref()
                        .map(hex_id)
                        .transpose()?
                        .is_some_and(|c| c >> 16 == 3)
                    {
                        device_paths.insert(path);
                    }
                }
            }
            Err(InventoryError::NativeUnavailable(_)) => limitations
                .push("PCI sysfs inventory was unavailable; only visible DRM cards are included."),
            Err(e) => return Err(e),
        }
        match entries("/sys/class/drm") {
            Ok(names) => for name in names.into_iter().filter(|n| drm_card(n)) {
                device_paths.insert(device_path(&Path::new("/sys/class/drm").join(name).join("device"))?);
            },
            Err(InventoryError::NativeUnavailable(_)) => limitations.push("DRM sysfs inventory was unavailable; only visible PCI display devices are included."),
            Err(e) => return Err(e),
        }
        if device_paths.len() > MAX_GPUS {
            return Err(InventoryError::InvalidObservation("GPU count"));
        }
        let mut gpus = Vec::new();
        for path in device_paths {
            let mut evidence = empty_gpu("linux_sysfs_pci_drm");
            evidence.vendor_id = reads
                .text(&path.join("vendor"), 32, true)?
                .as_deref()
                .map(hex_id)
                .transpose()?;
            evidence.hardware_device_id = reads
                .text(&path.join("device"), 32, true)?
                .as_deref()
                .map(hex_id)
                .transpose()?;
            if let Ok(link) = fs::read_link(path.join("driver")) {
                evidence.driver_name = link
                    .file_name()
                    .and_then(|n| n.to_str())
                    .filter(|n| text_valid(n, 128))
                    .map(str::to_owned);
            }
            if let Ok(module) = fs::canonicalize(path.join("driver/module")) {
                if !module.starts_with("/sys/module") {
                    return Err(InventoryError::InvalidObservation(
                        "sysfs driver module root",
                    ));
                }
                evidence.driver_version = reads
                    .text(&module.join("version"), 128, true)?
                    .map(|s| s.trim().to_owned());
            }
            if evidence.driver_name.as_deref() == Some("amdgpu") {
                evidence.reported_dedicated_video_bytes = reads
                    .text(&path.join("mem_info_vram_total"), 32, true)?
                    .as_deref()
                    .map(decimal_bytes)
                    .transpose()?;
                evidence.reported_vram_used_bytes = reads
                    .text(&path.join("mem_info_vram_used"), 32, true)?
                    .as_deref()
                    .map(decimal_bytes)
                    .transpose()?;
            }
            let identity = path
                .to_str()
                .ok_or(InventoryError::InvalidObservation(
                    "sysfs device path encoding",
                ))?
                .to_owned();
            gpus.push(RawGpu { identity, evidence });
        }
        // Detect CPU hotplug crossing this non-atomic observation.
        if cpus != cpu_list(&reads.required("/sys/devices/system/cpu/online", 8192)?)? {
            return Err(InventoryError::NativeUnavailable(
                "CPU topology changed during observation",
            ));
        }
        Ok(RawInventory {
            os: OperatingSystem::Linux,
            architecture,
            os_version,
            logical: cpus.len() as u16,
            physical: complete.then_some(cores.len() as u16),
            active: Some(cpus.len() as u16),
            cpu_vendor,
            salt,
            identity_scope,
            memory,
            gpus,
            limitations,
        })
    }
}

// These ABI declarations and parsers also compile under synthetic tests on a
// 64-bit Mac. OS calls themselves are cfg-gated and never invoked there.
#[cfg(any(target_os = "windows", test))]
mod windows {
    use super::*;
    use std::ffi::c_void;
    #[repr(C)]
    struct Guid {
        a: u32,
        b: u16,
        c: u16,
        d: [u8; 8],
    }
    #[repr(C)]
    struct SystemInfo {
        architecture: u16,
        reserved: u16,
        page_size: u32,
        min_address: *mut c_void,
        max_address: *mut c_void,
        active_mask: usize,
        processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        level: u16,
        revision: u16,
    }
    #[repr(C)]
    #[derive(Default)]
    struct MemoryStatus {
        length: u32,
        load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended: u64,
    }
    #[repr(C)]
    struct Version {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    #[repr(C)]
    struct Desc1 {
        description: [u16; 128],
        vendor: u32,
        device: u32,
        subsystem: u32,
        revision: u32,
        dedicated_video: usize,
        dedicated_system: usize,
        shared_system: usize,
        luid_low: u32,
        luid_high: i32,
        flags: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Budget {
        budget: u64,
        usage: u64,
        reservable: u64,
        reservation: u64,
    }
    #[repr(C)]
    struct UnknownVtable {
        query: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }
    #[repr(C)]
    struct FactoryVtable {
        unknown: UnknownVtable,
        object_and_factory: [usize; 9],
        enum_adapters: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
        is_current: unsafe extern "system" fn(*mut c_void) -> i32,
    }
    #[repr(C)]
    struct AdapterVtable {
        unknown: UnknownVtable,
        object_and_adapter: [usize; 6],
        check_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut i64) -> i32,
        desc1: unsafe extern "system" fn(*mut c_void, *mut Desc1) -> i32,
    }
    #[repr(C)]
    struct Adapter3Vtable {
        adapter: AdapterVtable,
        desc2_and_events: [usize; 3],
        query_memory: unsafe extern "system" fn(*mut c_void, u32, i32, *mut Budget) -> i32,
    }
    const FACTORY: Guid = Guid {
        a: 0x770aae78,
        b: 0xf26f,
        c: 0x4dba,
        d: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
    };
    const ADAPTER3: Guid = Guid {
        a: 0x645967a4,
        b: 0x1392,
        c: 0x4310,
        d: [0xa7, 0x98, 0x80, 0x53, 0xce, 0x3e, 0x93, 0xfd],
    };
    const DXGI_DEVICE: Guid = Guid {
        a: 0x54ec77fa,
        b: 0x1377,
        c: 0x44e6,
        d: [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c],
    };

    fn budget(raw: Budget) -> Result<VideoMemoryBudget> {
        if [raw.budget, raw.usage, raw.reservable, raw.reservation]
            .into_iter()
            .any(|n| !bytes_valid(n))
        {
            return Err(InventoryError::InvalidObservation(
                "DXGI budget integer range",
            ));
        }
        Ok(VideoMemoryBudget {
            source: "dxgi_adapter3_query_video_memory_info",
            node_index: 0,
            budget_bytes: raw.budget,
            current_usage_bytes: raw.usage,
            available_for_reservation_bytes: raw.reservable,
            current_reservation_bytes: raw.reservation,
            remaining_process_budget_bytes: raw.budget.saturating_sub(raw.usage),
        })
    }
    fn description(raw: &Desc1) -> Result<Option<RawGpu>> {
        // DXGI_ADAPTER_FLAG_SOFTWARE/REMOTE are not local hardware GPUs.
        if raw.flags & 3 != 0 {
            return Ok(None);
        }
        let end = raw
            .description
            .iter()
            .position(|v| *v == 0)
            .ok_or(InventoryError::InvalidObservation("DXGI name terminator"))?;
        let name = String::from_utf16(&raw.description[..end])
            .map_err(|_| InventoryError::InvalidObservation("DXGI name UTF16"))?;
        let mut evidence = empty_gpu("windows_dxgi_adapter1");
        evidence.name = (!name.is_empty()).then_some(name);
        evidence.vendor_id = Some(raw.vendor);
        evidence.hardware_device_id = Some(raw.device);
        evidence.reported_dedicated_video_bytes = Some(raw.dedicated_video as u64);
        evidence.reported_dedicated_system_bytes = Some(raw.dedicated_system as u64);
        evidence.reported_shared_system_limit_bytes = Some(raw.shared_system as u64);
        Ok(Some(RawGpu {
            identity: format!(
                "dxgi-luid:{:08x}:{:08x}",
                raw.luid_high as u32, raw.luid_low
            ),
            evidence,
        }))
    }
    fn physical_cores(buffer: &[u8]) -> Result<u16> {
        if buffer.is_empty() || buffer.len() > 1_048_576 {
            return Err(InventoryError::InvalidObservation(
                "processor topology length",
            ));
        }
        let mut offset = 0;
        let mut count = 0u16;
        while offset < buffer.len() {
            let header =
                buffer
                    .get(offset..offset + 8)
                    .ok_or(InventoryError::InvalidObservation(
                        "processor topology header",
                    ))?;
            let relation = u32::from_le_bytes(header[..4].try_into().unwrap());
            let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            let record =
                buffer
                    .get(
                        offset
                            ..offset.checked_add(size).ok_or(
                                InventoryError::InvalidObservation("processor topology overflow"),
                            )?,
                    )
                    .filter(|v| v.len() >= 48)
                    .ok_or(InventoryError::InvalidObservation(
                        "processor topology record",
                    ))?;
            let groups = u16::from_le_bytes(record[30..32].try_into().unwrap()) as usize;
            if relation != 0
                || groups == 0
                || groups > 64
                || size != 32 + groups * 16
                || count >= 1024
            {
                return Err(InventoryError::InvalidObservation(
                    "processor core relationship",
                ));
            }
            count += 1;
            offset += size;
        }
        Ok(count)
    }

    #[cfg(target_os = "windows")]
    mod calls {
        use super::*;
        use std::ptr;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetNativeSystemInfo(value: *mut SystemInfo);
            fn GetActiveProcessorCount(group: u16) -> u32;
            fn GlobalMemoryStatusEx(value: *mut MemoryStatus) -> i32;
            fn GetLogicalProcessorInformationEx(
                relationship: i32,
                buffer: *mut c_void,
                length: *mut u32,
            ) -> i32;
            fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
            fn FreeLibrary(module: *mut c_void) -> i32;
        }
        #[link(name = "ntdll")]
        extern "system" {
            fn RtlGetVersion(value: *mut Version) -> i32;
        }
        struct Library(*mut c_void);
        impl Drop for Library {
            fn drop(&mut self) {
                unsafe {
                    FreeLibrary(self.0);
                }
            }
        }
        struct Com(*mut c_void);
        impl Com {
            unsafe fn vtable<T>(&self) -> &T {
                &**(self.0 as *const *const T)
            }
        }
        impl Drop for Com {
            fn drop(&mut self) {
                unsafe {
                    (self.vtable::<UnknownVtable>().release)(self.0);
                }
            }
        }
        fn cores() -> Option<u16> {
            let mut size = 0u32;
            unsafe {
                GetLogicalProcessorInformationEx(0, ptr::null_mut(), &mut size);
            }
            if !(48..=1_048_576).contains(&size) {
                return None;
            }
            // u64 allocation provides native struct alignment.
            let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
            let capacity = size;
            if unsafe { GetLogicalProcessorInformationEx(0, buffer.as_mut_ptr().cast(), &mut size) }
                == 0
                || size > capacity
            {
                return None;
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size as usize) };
            physical_cores(bytes).ok()
        }
        pub(super) fn read() -> Result<RawInventory> {
            if std::mem::size_of::<usize>() != 8 {
                return Err(InventoryError::UnsupportedPlatform);
            }
            let mut system: SystemInfo = unsafe { std::mem::zeroed() };
            unsafe {
                GetNativeSystemInfo(&mut system);
            }
            let architecture = match system.architecture {
                9 => Architecture::X86_64,
                12 => Architecture::Aarch64,
                _ => return Err(InventoryError::UnsupportedPlatform),
            };
            let logical = unsafe { GetActiveProcessorCount(0xffff) };
            if !(1..=1024).contains(&logical) {
                return Err(InventoryError::InvalidObservation("active processor count"));
            }
            let mut memory = MemoryStatus {
                length: std::mem::size_of::<MemoryStatus>() as u32,
                ..Default::default()
            };
            if unsafe { GlobalMemoryStatusEx(&mut memory) } == 0 {
                return Err(InventoryError::NativeUnavailable("GlobalMemoryStatusEx"));
            }
            let mut version: Version = unsafe { std::mem::zeroed() };
            version.size = std::mem::size_of::<Version>() as u32;
            if unsafe { RtlGetVersion(&mut version) } < 0 {
                return Err(InventoryError::NativeUnavailable("RtlGetVersion"));
            }
            let mut gpus = Vec::new();
            let mut limitations=vec!["Windows adapter identity is process-scoped, not stable across application restart.",
                "DXGI budgets refer to this querying process and node zero only; they are not a child-runtime budget or free VRAM.",
                "DXGI excludes non-display compute adapters and may hide devices in restricted sessions; driver versions are DXGI UMD observations only."];
            // Restrict the sole optional DLL to System32; never search PATH,
            // current directory, package plugins or a user-selected library.
            let dll: Vec<u16> = "dxgi.dll\0".encode_utf16().collect();
            let module = unsafe { LoadLibraryExW(dll.as_ptr(), ptr::null_mut(), 0x800) };
            if module.is_null() {
                limitations
                    .push("System DXGI unavailable; this observation contains CPU/RAM only.");
            } else {
                let library = Library(module);
                let address =
                    unsafe { GetProcAddress(library.0, c"CreateDXGIFactory1".as_ptr().cast()) };
                if address.is_null() {
                    limitations.push(
                        "CreateDXGIFactory1 unavailable; this observation contains CPU/RAM only.",
                    );
                } else {
                    let create: unsafe extern "system" fn(*const Guid, *mut *mut c_void) -> i32 =
                        unsafe { std::mem::transmute(address) };
                    let mut factory = ptr::null_mut();
                    let status = unsafe { create(&FACTORY, &mut factory) };
                    if status < 0 || factory.is_null() {
                        return Err(InventoryError::NativeUnavailable("CreateDXGIFactory1"));
                    }
                    let factory = Com(factory);
                    let mut ended = false;
                    for index in 0..=64u32 {
                        let mut pointer = ptr::null_mut();
                        let status = unsafe {
                            (factory.vtable::<FactoryVtable>().enum_adapters)(
                                factory.0,
                                index,
                                &mut pointer,
                            )
                        };
                        if status as u32 == 0x887a0002 {
                            ended = true;
                            break;
                        }
                        if status < 0 || pointer.is_null() {
                            return Err(InventoryError::NativeUnavailable("EnumAdapters1"));
                        }
                        let adapter = Com(pointer);
                        if index == 64 {
                            return Err(InventoryError::InvalidObservation(
                                "DXGI adapter enumeration limit",
                            ));
                        }
                        let mut desc: Desc1 = unsafe { std::mem::zeroed() };
                        if unsafe {
                            (adapter.vtable::<AdapterVtable>().desc1)(adapter.0, &mut desc)
                        } < 0
                        {
                            return Err(InventoryError::NativeUnavailable("GetDesc1"));
                        }
                        let Some(mut gpu) = description(&desc)? else {
                            continue;
                        };
                        if gpus.len() == MAX_GPUS {
                            return Err(InventoryError::InvalidObservation(
                                "DXGI hardware adapter limit",
                            ));
                        }
                        let mut driver = 0i64;
                        if unsafe {
                            (adapter.vtable::<AdapterVtable>().check_interface)(
                                adapter.0,
                                &DXGI_DEVICE,
                                &mut driver,
                            )
                        } >= 0
                        {
                            let d = driver as u64;
                            gpu.evidence.driver_version = Some(format!(
                                "{}.{}.{}.{}",
                                d >> 48,
                                (d >> 32) & 0xffff,
                                (d >> 16) & 0xffff,
                                d & 0xffff
                            ));
                        }
                        let mut newer = ptr::null_mut();
                        if unsafe {
                            (adapter.vtable::<UnknownVtable>().query)(
                                adapter.0, &ADAPTER3, &mut newer,
                            )
                        } >= 0
                            && !newer.is_null()
                        {
                            let newer = Com(newer);
                            for segment in 0..=1 {
                                let mut info = Budget::default();
                                if unsafe {
                                    (newer.vtable::<Adapter3Vtable>().query_memory)(
                                        newer.0, 0, segment, &mut info,
                                    )
                                } >= 0
                                {
                                    let value = Some(budget(info)?);
                                    if segment == 0 {
                                        gpu.evidence.local_process_budget = value;
                                    } else {
                                        gpu.evidence.non_local_process_budget = value;
                                    }
                                }
                            }
                        }
                        gpus.push(gpu);
                    }
                    if !ended {
                        return Err(InventoryError::InvalidObservation(
                            "incomplete DXGI enumeration",
                        ));
                    }
                    if unsafe { (factory.vtable::<FactoryVtable>().is_current)(factory.0) } == 0 {
                        return Err(InventoryError::NativeUnavailable(
                            "DXGI topology changed during observation",
                        ));
                    }
                }
            }
            Ok(RawInventory {
                os: OperatingSystem::Windows,
                architecture,
                os_version: format!("{}.{}.{}", version.major, version.minor, version.build),
                logical: logical as u16,
                physical: cores(),
                active: Some(logical as u16),
                cpu_vendor: Vendor::Unknown,
                salt: process_salt(),
                identity_scope: IdentityScope::ProcessLifetime,
                memory: HostMemoryEvidence {
                    source: "win32_global_memory_status_ex",
                    total_physical_bytes: memory.total_phys,
                    available_physical_bytes: Some(memory.avail_phys),
                    availability_is_os_estimate: false,
                    process_memory_limit_observed: false,
                },
                gpus,
                limitations,
            })
        }
    }
    #[cfg(target_os = "windows")]
    pub(super) fn read() -> Result<RawInventory> {
        calls::read()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn abi_layouts_match_64_bit_public_sdk() {
            assert_eq!(std::mem::size_of::<SystemInfo>(), 48);
            assert_eq!(std::mem::size_of::<MemoryStatus>(), 64);
            assert_eq!(std::mem::size_of::<Version>(), 276);
            assert_eq!(std::mem::size_of::<Desc1>(), 312);
            assert_eq!(std::mem::offset_of!(Desc1, dedicated_video), 272);
            assert_eq!(std::mem::offset_of!(FactoryVtable, enum_adapters), 12 * 8);
            assert_eq!(std::mem::offset_of!(AdapterVtable, check_interface), 9 * 8);
            assert_eq!(std::mem::offset_of!(AdapterVtable, desc1), 10 * 8);
            assert_eq!(std::mem::offset_of!(Adapter3Vtable, query_memory), 14 * 8);
        }
        #[test]
        fn dxgi_budget_is_process_scoped_and_never_underflows() {
            let value = budget(Budget {
                budget: 10,
                usage: 20,
                reservable: 900,
                reservation: 30,
            })
            .unwrap();
            assert_eq!(value.remaining_process_budget_bytes, 0);
            assert_eq!(value.available_for_reservation_bytes, 900);
            assert!(budget(Budget {
                budget: u64::MAX,
                ..Default::default()
            })
            .is_err());
        }
        #[test]
        fn software_adapter_and_bad_utf16_are_not_hardware() {
            let mut desc: Desc1 = unsafe { std::mem::zeroed() };
            desc.flags = 2;
            assert!(description(&desc).unwrap().is_none());
            desc.flags = 0;
            desc.description[0] = 0xd800;
            assert!(description(&desc).is_err());
            desc.description[0] = b'A' as u16;
            desc.shared_system = 8 * 1024 * 1024 * 1024;
            desc.vendor = 0x8086;
            let item = description(&desc).unwrap().unwrap();
            assert!(item.evidence.unified_memory.is_none());
            assert_eq!(
                item.evidence.reported_shared_system_limit_bytes,
                Some(8 * 1024 * 1024 * 1024)
            );
        }
        #[test]
        fn variable_cpu_topology_records_are_bounded() {
            let mut one = vec![0u8; 48];
            one[4..8].copy_from_slice(&48u32.to_le_bytes());
            one[30..32].copy_from_slice(&1u16.to_le_bytes());
            assert_eq!(physical_cores(&one).unwrap(), 1);
            let mut two = one.clone();
            two.extend(&one);
            assert_eq!(physical_cores(&two).unwrap(), 2);
            two[4..8].fill(0);
            assert!(physical_cores(&two).is_err());
            assert!(physical_cores(&one[..47]).is_err());
            one[30..32].copy_from_slice(&64u16.to_le_bytes());
            assert!(physical_cores(&one).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> RawInventory {
        RawInventory {
            os: OperatingSystem::Linux,
            architecture: Architecture::X86_64,
            os_version: "synthetic-1".into(),
            logical: 8,
            physical: Some(4),
            active: Some(8),
            cpu_vendor: Vendor::Intel,
            salt: "synthetic-boot".into(),
            identity_scope: IdentityScope::OsBootRegistryLifetime,
            memory: meminfo("MemTotal: 16777216 kB\nMemAvailable: 8388608 kB\n").unwrap(),
            gpus: Vec::new(),
            limitations: Vec::new(),
        }
    }
    #[test]
    fn linux_meminfo_preserves_unknowns_rejects_duplicates_units_overflow() {
        assert_eq!(
            meminfo("MemTotal: 16 kB\nMemFree: 8 kB\n")
                .unwrap()
                .available_physical_bytes,
            None
        );
        for s in [
            "MemTotal: 1 MB",
            "MemTotal: 1 kB\nMemTotal: 1 kB",
            "MemTotal: 1 kB\nMemAvailable: 2 kB",
            "MemTotal: 18446744073709551615 kB",
            "MemTotal: +12 kB",
            "MemTotal: 0 kB",
        ] {
            assert!(meminfo(s).is_err(), "{s}");
        }
    }
    #[test]
    fn cpu_ranges_and_device_names_cannot_expand_unbounded_or_traverse() {
        assert_eq!(cpu_list("0-2,7\n").unwrap(), vec![0, 1, 2, 7]);
        for s in [
            "0-1048575",
            "1-0",
            "1,1",
            "1-3,2",
            "../4",
            "0-1-2",
            "+1",
            "",
        ] {
            assert!(cpu_list(s).is_err());
        }
        assert!(pci_address("0000:01:00.0"));
        assert!(!pci_address("0000:01:00.8"));
        assert!(!pci_address("../0000:01:00.0"));
        assert!(drm_card("card12"));
        assert!(!drm_card("card0-DP-1"));
        assert!(!drm_card("renderD128"));
        assert!(!drm_card("card../x"));
        assert_eq!(hex_id("0x10de\n").unwrap(), 0x10de);
        assert!(hex_id("0x100000000").is_err());
        assert!(decimal_bytes("-1").is_err());
    }
    #[test]
    fn gpu_counters_never_create_extra_ram_or_infer_uma_or_runtime() {
        let mut raw = fixture();
        let mut gpu = empty_gpu("synthetic");
        gpu.vendor_id = Some(0x8086);
        gpu.reported_shared_system_limit_bytes = Some(8 << 30);
        gpu.reported_dedicated_video_bytes = Some(4 << 30);
        raw.gpus.push(RawGpu {
            identity: "pci:synthetic-device".into(),
            evidence: gpu,
        });
        let output = assemble(raw).unwrap();
        let inventory = &output.observation.inventory;
        assert_eq!(
            inventory
                .memory_pools
                .iter()
                .filter_map(|p| p.total_bytes)
                .sum::<u64>(),
            16 << 30
        );
        assert!(inventory
            .memory_pools
            .iter()
            .all(|p| p.available_bytes.is_none()));
        assert!(!output.observation.backend_runtime_tested);
        assert!(!output.observation.model_placement_observed);
        assert_eq!(output.host_memory.available_physical_bytes, Some(8 << 30));
        assert!(output.gpu_devices[0].unified_memory.is_none());
    }
    #[test]
    fn identity_is_opaque_stable_per_scope_and_generation_fresh() {
        let first = assemble(fixture()).unwrap();
        let second = assemble(fixture()).unwrap();
        assert_eq!(
            first.observation.inventory.devices,
            second.observation.inventory.devices
        );
        assert_ne!(
            first.observation.inventory.generation,
            second.observation.inventory.generation
        );
        let mut different = fixture();
        different.salt = "new-boot".into();
        assert_ne!(
            first.observation.inventory.devices,
            assemble(different).unwrap().observation.inventory.devices
        );
        assert!(!serde_json::to_string(&first)
            .unwrap()
            .contains("synthetic-boot"));
    }
    #[test]
    fn duplicate_device_and_bad_memory_fail_closed() {
        let mut raw = fixture();
        raw.memory.available_physical_bytes = Some(u64::MAX);
        assert!(assemble(raw).is_err());
        let mut raw = fixture();
        for _ in 0..2 {
            raw.gpus.push(RawGpu {
                identity: "same".into(),
                evidence: empty_gpu("synthetic"),
            });
        }
        assert!(assemble(raw).is_err());
        assert_eq!(vendor(Some(0x10de)), Vendor::Nvidia);
        assert_eq!(vendor(Some(0x1002)), Vendor::Amd);
        assert_eq!(vendor(None), Vendor::Unknown);
    }
    #[test]
    fn singleflight_timeout_does_not_release_a_live_worker() {
        let gate = Arc::new(AtomicBool::new(false));
        let worker = gate.clone();
        let (tx, rx) = mpsc::channel();
        assert_eq!(
            bounded(worker, Duration::from_millis(10), move || {
                rx.recv().unwrap();
                Ok(())
            })
            .unwrap_err(),
            InventoryError::TimedOut
        );
        assert_eq!(
            bounded(gate.clone(), Duration::from_millis(10), || Ok(())).unwrap_err(),
            InventoryError::Busy
        );
        tx.send(()).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while gate.load(Ordering::Acquire) && std::time::Instant::now() < until {
            std::thread::yield_now();
        }
        assert!(!gate.load(Ordering::Acquire));
        assert!(bounded(gate, Duration::from_secs(1), || Ok(())).is_ok());
    }
    #[test]
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    fn native_entrypoint_does_not_probe_on_other_platforms() {
        assert_eq!(observe().unwrap_err(), InventoryError::UnsupportedPlatform);
    }
}
