//! Read-only hardware inventory through public macOS APIs linked into this
//! executable. No helper discovery, environment override, model, GPU command
//! queue, kernel, driver installation, profile or personal file is involved.
//!
//! Call `observe` from a blocking worker, not the UI/event-loop thread. Its
//! caller deadline is two seconds. A process-wide single-flight gate bounds
//! native work to one thread even if an OS API stalls; an in-process OS call
//! cannot be forcibly interrupted. A timed-out observation is never reused.

use serde::Serialize;
use sha2::{Digest, Sha256};
use smith_acceleration::{
    Architecture, Device, DeviceClass, HardwareInventory, MemoryPool, OperatingSystem, Vendor,
};
use std::{
    collections::BTreeSet,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_METAL_DEVICES: usize = 31; // The planner's 32-device bound includes CPU.
const MAX_JSON_INTEGER: u64 = (1 << 53) - 1;
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(2);
static IN_FLIGHT: OnceLock<Arc<AtomicBool>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum InventoryError {
    #[cfg_attr(target_os = "macos", allow(dead_code))] // Constructed on other OSes.
    UnsupportedPlatform,
    Busy,
    TimedOut,
    NativeUnavailable(&'static str),
    InvalidObservation(&'static str),
}
impl fmt::Display for InventoryError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                output.write_str("Native inventory is not implemented on this platform.")
            }
            Self::Busy => output.write_str("A native inventory query is already in progress."),
            Self::TimedOut => {
                output.write_str("Native inventory exceeded its two-second observation deadline.")
            }
            Self::NativeUnavailable(detail) => {
                write!(output, "Native inventory unavailable: {detail}")
            }
            Self::InvalidObservation(detail) => {
                write!(output, "Native inventory returned invalid data: {detail}")
            }
        }
    }
}
impl std::error::Error for InventoryError {}
type Result<T> = std::result::Result<T, InventoryError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityScope {
    /// Hash of the current boot session and Metal's cross-task registry ID.
    /// Neither input is exposed. No persistence across reboot/hot replacement
    /// is promised, and enumeration order is never used as identity.
    OsBootRegistryLifetime,
    /// If boot-session information is unavailable, use a process-local nonce.
    ProcessLifetime,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetalEvidence {
    pub device_id: String,
    pub name: Option<String>,
    pub unified_memory: Option<bool>,
    pub low_power: Option<bool>,
    pub removable: Option<bool>,
    /// Performance guidance from Metal, never free/available/physical VRAM.
    pub recommended_working_set_bytes: Option<u64>,
    pub maximum_single_buffer_bytes: Option<u64>,
    pub apple_gpu_family: Option<bool>,
    pub mac_gpu_family_2: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VmMemorySample {
    pub observed_unix_ms: u64,
    pub source: &'static str,
    pub page_size_bytes: u64,
    /// Darwin free_count includes speculative cached pages; these overlap.
    pub free_including_speculative_bytes: u64,
    pub speculative_bytes: u64,
    pub inactive_bytes: u64,
    pub purgeable_bytes: u64,
    pub compressed_bytes: u64,
    /// Only unused, non-speculative pages at this instant. This is a
    /// conservative planning input, not a reservation or OOM guarantee.
    pub conservative_available_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Observation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_evidence: Option<serde_json::Value>,
    pub schema_version: u8,
    pub source: &'static str,
    pub observed_unix_ms: u64,
    pub inventory: HardwareInventory,
    pub process_architecture: &'static str,
    pub physical_cpu_count: Option<u16>,
    pub active_cpu_count: Option<u16>,
    pub identity_scope: IdentityScope,
    pub metal_devices: Vec<MetalEvidence>,
    pub memory_sample: Option<VmMemorySample>,
    pub limitations: Vec<&'static str>,
    pub backend_runtime_tested: bool,
    pub model_placement_observed: bool,
}

/// One fresh, bounded OS observation. On unsupported platforms returns an
/// explicit error rather than inventing a zero-device hardware inventory.
pub fn observe() -> Result<Observation> {
    #[cfg(target_os = "macos")]
    {
        let gate = IN_FLIGHT.get_or_init(|| Arc::new(AtomicBool::new(false)));
        bounded(gate.clone(), OBSERVATION_TIMEOUT, || {
            assemble(native::read()?)
        })
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        let observed = crate::hardware_inventory_platform::observe()?;
        let mut observation = observed.observation;
        observation.platform_evidence = Some(serde_json::json!({
            "host_memory": observed.host_memory, "gpu_devices": observed.gpu_devices,
            "native_platform_qualification": "pending"
        }));
        Ok(observation)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(InventoryError::UnsupportedPlatform)
    }
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
        .name("smith-hardware-inventory".into())
        .spawn(move || {
            let result = work();
            // Native work has ended before publishing the result. A caller
            // may immediately request another fresh observation.
            drop(reset);
            let _ = sender.send(result);
        })
        .map_err(|_| InventoryError::NativeUnavailable("cannot start bounded inventory worker"))?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(InventoryError::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(InventoryError::NativeUnavailable(
            "inventory worker did not return a result",
        )),
    }
}

fn unix_ms() -> Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| InventoryError::InvalidObservation("system clock"))?
            .as_millis(),
    )
    .map_err(|_| InventoryError::InvalidObservation("system clock overflow"))
}

struct RawMetal {
    registry_id: u64,
    name: Option<String>,
    unified: Option<bool>,
    low_power: Option<bool>,
    removable: Option<bool>,
    working_set: Option<u64>,
    max_buffer: Option<u64>,
    apple_family: Option<bool>,
    mac_family: Option<bool>,
}
struct RawInventory {
    architecture: Architecture,
    os_version: String,
    logical_cpu_count: u32,
    physical_cpu_count: Option<u32>,
    active_cpu_count: Option<u32>,
    cpu_vendor: Vendor,
    physical_memory: u64,
    identity_salt: String,
    identity_scope: IdentityScope,
    metal: Vec<RawMetal>,
    memory_sample: Option<VmMemorySample>,
}

fn opaque_id(namespace: &str, salt: &str, registry_id: u64) -> String {
    let mut hash = Sha256::new();
    hash.update(b"smith.native-hardware.v1\0");
    hash.update(namespace.as_bytes());
    hash.update(b"\0");
    hash.update(salt.as_bytes());
    hash.update(b"\0");
    hash.update(registry_id.to_be_bytes());
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{namespace}:{digest}")
}
fn valid_count(value: u32) -> Option<u16> {
    (1..=1024).contains(&value).then_some(value as u16)
}
fn finite_bytes(value: Option<u64>) -> Option<u64> {
    value.filter(|value| *value > 0 && *value <= MAX_JSON_INTEGER)
}
fn assemble(raw: RawInventory) -> Result<Observation> {
    let logical = valid_count(raw.logical_cpu_count)
        .ok_or(InventoryError::InvalidObservation("logical CPU count"))?;
    if raw.os_version.is_empty()
        || raw.os_version.len() > 128
        || raw.os_version.chars().any(char::is_control)
        || raw.identity_salt.is_empty()
        || raw.identity_salt.len() > 256
        || finite_bytes(Some(raw.physical_memory)).is_none()
        || raw.metal.len() > MAX_METAL_DEVICES
    {
        return Err(InventoryError::InvalidObservation("inventory bounds"));
    }
    let host_pool = opaque_id("host-memory", &raw.identity_salt, 0);
    let mut pools = vec![MemoryPool {
        id: host_pool.clone(),
        total_bytes: Some(raw.physical_memory),
        // No public macOS API queried here establishes globally allocatable
        // bytes. Keep VM free-page evidence separate from this quantity.
        available_bytes: None,
    }];
    let mut devices = vec![Device {
        id: opaque_id("cpu", &raw.identity_salt, 0),
        class: DeviceClass::Cpu,
        vendor: raw.cpu_vendor,
        memory_pool_id: host_pool.clone(),
        driver_version: None,
    }];
    let mut metadata = Vec::with_capacity(raw.metal.len());
    let mut unique = BTreeSet::new();
    for metal in raw.metal {
        if metal.registry_id == 0 || !unique.insert(metal.registry_id) {
            return Err(InventoryError::InvalidObservation(
                "Metal registry identity",
            ));
        }
        let id = opaque_id("metal", &raw.identity_salt, metal.registry_id);
        let pool = if metal.unified == Some(true) {
            host_pool.clone()
        } else {
            let pool = opaque_id("device-memory", &raw.identity_salt, metal.registry_id);
            pools.push(MemoryPool {
                id: pool.clone(),
                total_bytes: None,
                available_bytes: None,
            });
            pool
        };
        devices.push(Device {
            id: id.clone(),
            class: DeviceClass::Gpu,
            vendor: if metal.apple_family == Some(true) {
                Vendor::Apple
            } else {
                Vendor::Unknown
            },
            memory_pool_id: pool,
            // OS version is not silently relabelled as a GPU driver version.
            driver_version: None,
        });
        metadata.push(MetalEvidence {
            device_id: id,
            name: metal.name.filter(|name| {
                !name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)
            }),
            unified_memory: metal.unified,
            low_power: metal.low_power,
            removable: metal.removable,
            recommended_working_set_bytes: finite_bytes(metal.working_set),
            maximum_single_buffer_bytes: finite_bytes(metal.max_buffer),
            apple_gpu_family: metal.apple_family,
            mac_gpu_family_2: metal.mac_family,
        });
    }
    // Stable identity/order is independent of Metal's enumeration ordinal.
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    pools.sort_by(|a, b| a.id.cmp(&b.id));
    metadata.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    let mut limitations = vec![
        "OS enumeration is not runtime installation, inference, placement or performance evidence.",
        "Available pool memory and GPU driver versions are unknown; Metal working-set guidance is not free memory.",
        "VM free-page evidence is instantaneous, unreserved and not an allocation or OOM guarantee.",
    ];
    if metadata.is_empty() {
        limitations.push("No Metal devices were visible to this process; sandbox restrictions can hide hardware.");
    }
    if raw.memory_sample.is_none() {
        limitations.push("Native VM memory counters were unavailable or inconsistent.");
    }
    if raw.identity_scope == IdentityScope::ProcessLifetime {
        limitations.push("Boot-session identity was unavailable; opaque device IDs are stable only in this process.");
    }
    Ok(Observation {
        platform_evidence: None,
        schema_version: 1,
        source: "native_macos_sysctl_metal_vm",
        observed_unix_ms: unix_ms()?,
        inventory: HardwareInventory {
            generation: uuid::Uuid::new_v4().to_string(),
            os: OperatingSystem::Macos,
            architecture: raw.architecture,
            os_version: raw.os_version,
            logical_cpu_count: logical,
            host_memory_pool_id: host_pool,
            memory_pools: pools,
            devices,
        },
        process_architecture: std::env::consts::ARCH,
        physical_cpu_count: raw
            .physical_cpu_count
            .and_then(valid_count)
            .filter(|value| *value <= logical),
        active_cpu_count: raw
            .active_cpu_count
            .and_then(valid_count)
            .filter(|value| *value <= logical),
        identity_scope: raw.identity_scope,
        metal_devices: metadata,
        memory_sample: raw.memory_sample,
        limitations,
        backend_runtime_tested: false,
        model_placement_observed: false,
    })
}

fn memory_sample(
    page_size: u64,
    counts: [u64; 5],
    total_bytes: u64,
    observed_unix_ms: u64,
) -> Option<VmMemorySample> {
    if !page_size.is_power_of_two() || !(4096..=65536).contains(&page_size) {
        return None;
    }
    let [free, speculative, ..] = counts;
    let unused = free.checked_sub(speculative)?.checked_mul(page_size)?;
    let bytes: Vec<u64> = counts
        .into_iter()
        .map(|pages| {
            pages
                .checked_mul(page_size)
                .filter(|bytes| *bytes <= total_bytes)
        })
        .collect::<Option<_>>()?;
    Some(VmMemorySample {
        observed_unix_ms,
        source: "host_statistics64.free_count_minus_speculative_count",
        page_size_bytes: page_size,
        free_including_speculative_bytes: bytes[0],
        speculative_bytes: bytes[1],
        inactive_bytes: bytes[2],
        purgeable_bytes: bytes[3],
        compressed_bytes: bytes[4],
        conservative_available_bytes: unused,
    })
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::{
        ffi::{c_void, CStr},
        ptr,
    };
    type Object = *mut c_void;
    type Selector = *const c_void;

    #[link(name = "Metal", kind = "framework")]
    extern "C" {
        fn MTLCopyAllDevices() -> Object;
    }
    #[link(name = "Foundation", kind = "framework")]
    extern "C" {}
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringGetLength(value: Object) -> isize;
        fn CFStringGetCString(value: Object, buffer: *mut i8, capacity: isize, encoding: u32)
            -> u8;
    }
    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const i8) -> Object;
        fn sel_registerName(name: *const i8) -> Selector;
        fn objc_msgSend();
    }
    extern "C" {
        fn mach_host_self() -> libc::mach_port_t;
        fn host_page_size(host: libc::mach_port_t, output: *mut libc::vm_size_t) -> i32;
        fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> i32;
        static mach_task_self_: libc::mach_port_t;
    }

    // Every selector is a compile-time public SDK selector. These small casts
    // reproduce its exact scalar/pointer ABI; there are no struct/float calls.
    unsafe fn selector(name: &'static CStr) -> Selector {
        sel_registerName(name.as_ptr())
    }
    unsafe fn pointer(object: Object, name: &'static CStr) -> Object {
        let call: unsafe extern "C" fn(Object, Selector) -> Object =
            std::mem::transmute(objc_msgSend as *const ());
        call(object, selector(name))
    }
    unsafe fn number(object: Object, name: &'static CStr) -> u64 {
        let call: unsafe extern "C" fn(Object, Selector) -> u64 =
            std::mem::transmute(objc_msgSend as *const ());
        call(object, selector(name))
    }
    unsafe fn boolean(object: Object, name: &'static CStr) -> bool {
        let call: unsafe extern "C" fn(Object, Selector) -> u8 =
            std::mem::transmute(objc_msgSend as *const ());
        call(object, selector(name)) != 0
    }
    unsafe fn responds(object: Object, name: &'static CStr) -> bool {
        let call: unsafe extern "C" fn(Object, Selector, Selector) -> u8 =
            std::mem::transmute(objc_msgSend as *const ());
        call(object, selector(c"respondsToSelector:"), selector(name)) != 0
    }
    unsafe fn optional_bool(object: Object, name: &'static CStr) -> Option<bool> {
        responds(object, name).then(|| boolean(object, name))
    }
    unsafe fn optional_number(object: Object, name: &'static CStr) -> Option<u64> {
        responds(object, name).then(|| number(object, name))
    }
    unsafe fn family(object: Object, value: usize) -> Option<bool> {
        if !responds(object, c"supportsFamily:") {
            return None;
        }
        let call: unsafe extern "C" fn(Object, Selector, usize) -> u8 =
            std::mem::transmute(objc_msgSend as *const ());
        Some(call(object, selector(c"supportsFamily:"), value) != 0)
    }
    struct Owned(Object);
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe {
                let call: unsafe extern "C" fn(Object, Selector) =
                    std::mem::transmute(objc_msgSend as *const ());
                call(self.0, selector(c"release"));
            }
        }
    }
    struct Pool(Object);
    impl Drop for Pool {
        fn drop(&mut self) {
            unsafe {
                let call: unsafe extern "C" fn(Object, Selector) =
                    std::mem::transmute(objc_msgSend as *const ());
                call(self.0, selector(c"drain"));
            }
        }
    }
    unsafe fn name(object: Object) -> Option<String> {
        let string = pointer(object, c"name");
        if string.is_null() || !(1..=256).contains(&CFStringGetLength(string)) {
            return None;
        }
        // NSString and CFString are toll-free bridged. This fixed stack buffer
        // never scans an unbounded UTF8String pointer returned by an object.
        let mut buffer = [0u8; 513];
        if CFStringGetCString(
            string,
            buffer.as_mut_ptr().cast(),
            buffer.len() as isize,
            0x08000100,
        ) == 0
        {
            return None;
        }
        let end = buffer.iter().position(|byte| *byte == 0)?;
        std::str::from_utf8(&buffer[..end]).ok().map(str::to_owned)
    }

    fn text(name: &'static CStr) -> Option<String> {
        let mut bytes = [0u8; 256];
        let mut count = bytes.len();
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut count,
                ptr::null_mut(),
                0,
            )
        } != 0
            || count == 0
            || count > bytes.len()
        {
            return None;
        }
        let end = bytes[..count].iter().position(|byte| *byte == 0)?;
        std::str::from_utf8(&bytes[..end]).ok().map(str::to_owned)
    }
    fn u32_value(name: &'static CStr) -> Option<u32> {
        let mut output = 0u32;
        let mut size = std::mem::size_of_val(&output);
        let result = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut output as *mut u32).cast(),
                &mut size,
                ptr::null_mut(),
                0,
            )
        };
        (result == 0 && size == std::mem::size_of_val(&output)).then_some(output)
    }
    fn u64_value(name: &'static CStr) -> Option<u64> {
        let mut output = 0u64;
        let mut size = std::mem::size_of_val(&output);
        let result = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut output as *mut u64).cast(),
                &mut size,
                ptr::null_mut(),
                0,
            )
        };
        (result == 0 && size == std::mem::size_of_val(&output)).then_some(output)
    }
    fn vm_sample(total: u64) -> Option<VmMemorySample> {
        struct HostPort(libc::mach_port_t);
        impl Drop for HostPort {
            fn drop(&mut self) {
                unsafe {
                    mach_port_deallocate(mach_task_self_, self.0);
                }
            }
        }
        let host = HostPort(unsafe { mach_host_self() });
        if host.0 == 0 {
            return None;
        }
        let mut page_size: libc::vm_size_t = 0;
        if unsafe { host_page_size(host.0, &mut page_size) } != 0 {
            return None;
        }
        let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // libc may describe a newer Darwin revision than the running kernel.
        // Require only the complete prefix through the last field we read.
        let required_count = ((std::mem::offset_of!(libc::vm_statistics64, compressor_page_count)
            + std::mem::size_of::<libc::natural_t>())
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        if unsafe {
            libc::host_statistics64(
                host.0,
                libc::HOST_VM_INFO64,
                (&mut vm as *mut libc::vm_statistics64).cast(),
                &mut count,
            )
        } != 0
            || count < required_count
        {
            return None;
        }
        memory_sample(
            page_size as u64,
            [
                vm.free_count.into(),
                vm.speculative_count.into(),
                vm.inactive_count.into(),
                vm.purgeable_count.into(),
                vm.compressor_page_count.into(),
            ],
            total,
            unix_ms().ok()?,
        )
    }
    pub(super) fn read() -> Result<RawInventory> {
        let architecture = if u32_value(c"hw.optional.arm64") == Some(1) {
            Architecture::Aarch64
        } else {
            match text(c"hw.machine").as_deref() {
                Some("arm64") => Architecture::Aarch64,
                Some("x86_64") => Architecture::X86_64,
                _ => return Err(InventoryError::NativeUnavailable("host architecture")),
            }
        };
        let logical_cpu_count = u32_value(c"hw.logicalcpu")
            .ok_or(InventoryError::NativeUnavailable("logical CPU count"))?;
        let physical_memory =
            u64_value(c"hw.memsize").ok_or(InventoryError::NativeUnavailable("physical memory"))?;
        let os_version = text(c"kern.osproductversion")
            .ok_or(InventoryError::NativeUnavailable("macOS version"))?;
        let (identity_salt, identity_scope) = if let Some(boot) =
            text(c"kern.bootsessionuuid").filter(|value| uuid::Uuid::parse_str(value).is_ok())
        {
            (boot, IdentityScope::OsBootRegistryLifetime)
        } else {
            static PROCESS: OnceLock<String> = OnceLock::new();
            (
                PROCESS
                    .get_or_init(|| uuid::Uuid::new_v4().to_string())
                    .clone(),
                IdentityScope::ProcessLifetime,
            )
        };
        let mut metal = Vec::new();
        unsafe {
            let class = objc_getClass(c"NSAutoreleasePool".as_ptr());
            if class.is_null() {
                return Err(InventoryError::NativeUnavailable(
                    "Foundation autorelease pool",
                ));
            }
            let pool = Pool(pointer(class, c"new"));
            if pool.0.is_null() {
                return Err(InventoryError::NativeUnavailable(
                    "Foundation autorelease pool",
                ));
            }
            let devices = Owned(MTLCopyAllDevices());
            if devices.0.is_null() {
                return Err(InventoryError::NativeUnavailable(
                    "Metal device enumeration",
                ));
            }
            let count = number(devices.0, c"count");
            if count > MAX_METAL_DEVICES as u64 {
                return Err(InventoryError::InvalidObservation("too many Metal devices"));
            }
            let object_at: unsafe extern "C" fn(Object, Selector, usize) -> Object =
                std::mem::transmute(objc_msgSend as *const ());
            for index in 0..count as usize {
                let device = object_at(devices.0, selector(c"objectAtIndex:"), index);
                if device.is_null() {
                    return Err(InventoryError::NativeUnavailable(
                        "Metal returned a null device",
                    ));
                }
                metal.push(RawMetal {
                    registry_id: optional_number(device, c"registryID").unwrap_or(0),
                    name: name(device),
                    unified: optional_bool(device, c"hasUnifiedMemory"),
                    low_power: optional_bool(device, c"isLowPower"),
                    removable: optional_bool(device, c"isRemovable"),
                    working_set: optional_number(device, c"recommendedMaxWorkingSetSize"),
                    max_buffer: optional_number(device, c"maxBufferLength"),
                    apple_family: family(device, 1001), // MTLGPUFamilyApple1
                    mac_family: family(device, 2002),   // MTLGPUFamilyMac2
                });
            }
        }
        Ok(RawInventory {
            architecture,
            os_version,
            logical_cpu_count,
            physical_cpu_count: u32_value(c"hw.physicalcpu"),
            active_cpu_count: u32_value(c"hw.activecpu"),
            cpu_vendor: if architecture == Architecture::Aarch64 {
                Vendor::Apple
            } else {
                match text(c"machdep.cpu.vendor").as_deref() {
                    Some("GenuineIntel") => Vendor::Intel,
                    Some("AuthenticAMD") => Vendor::Amd,
                    _ => Vendor::Unknown,
                }
            },
            physical_memory,
            identity_salt,
            identity_scope,
            metal,
            memory_sample: vm_sample(physical_memory),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(registry_id: u64, unified: bool) -> RawMetal {
        RawMetal {
            registry_id,
            name: Some("Synthetic GPU".into()),
            unified: Some(unified),
            low_power: Some(true),
            removable: Some(false),
            working_set: Some(8 << 30),
            max_buffer: Some(4 << 30),
            apple_family: Some(unified),
            mac_family: Some(true),
        }
    }
    fn fixture(metal: Vec<RawMetal>) -> RawInventory {
        RawInventory {
            architecture: Architecture::Aarch64,
            os_version: "15.0.0".into(),
            logical_cpu_count: 8,
            physical_cpu_count: Some(8),
            active_cpu_count: Some(8),
            cpu_vendor: Vendor::Apple,
            physical_memory: 16 << 30,
            identity_salt: "synthetic-boot".into(),
            identity_scope: IdentityScope::OsBootRegistryLifetime,
            metal,
            memory_sample: None,
        }
    }
    #[test]
    fn unified_ram_is_counted_once_and_working_set_is_not_available() {
        let observation = assemble(fixture(vec![gpu(7, true)])).unwrap();
        assert_eq!(observation.inventory.memory_pools.len(), 1);
        assert!(observation
            .inventory
            .devices
            .iter()
            .all(|device| device.memory_pool_id == observation.inventory.host_memory_pool_id));
        assert_eq!(
            observation.inventory.memory_pools[0].total_bytes,
            Some(16 << 30)
        );
        assert_eq!(observation.inventory.memory_pools[0].available_bytes, None);
        assert_eq!(
            observation.metal_devices[0].recommended_working_set_bytes,
            Some(8 << 30)
        );
        assert!(!observation.backend_runtime_tested && !observation.model_placement_observed);
    }
    #[test]
    fn discrete_vram_and_driver_remain_unknown() {
        let observation = assemble(fixture(vec![gpu(8, false)])).unwrap();
        let gpu = observation
            .inventory
            .devices
            .iter()
            .find(|device| device.class == DeviceClass::Gpu)
            .unwrap();
        let pool = observation
            .inventory
            .memory_pools
            .iter()
            .find(|pool| pool.id == gpu.memory_pool_id)
            .unwrap();
        assert_ne!(pool.id, observation.inventory.host_memory_pool_id);
        assert_eq!((pool.total_bytes, pool.available_bytes), (None, None));
        assert_eq!(gpu.driver_version, None);
        assert_eq!(gpu.vendor, Vendor::Unknown);
    }
    #[test]
    fn identity_is_opaque_order_independent_and_changes_with_boot_or_device() {
        let a = assemble(fixture(vec![gpu(11, false), gpu(22, true)])).unwrap();
        let b = assemble(fixture(vec![gpu(22, true), gpu(11, false)])).unwrap();
        assert_eq!(a.inventory.devices, b.inventory.devices);
        assert_ne!(a.inventory.generation, b.inventory.generation);
        assert_ne!(
            opaque_id("metal", "boot-a", 11),
            opaque_id("metal", "boot-b", 11)
        );
        assert_ne!(
            opaque_id("metal", "boot-a", 11),
            opaque_id("metal", "boot-a", 12)
        );
        let output = serde_json::to_string(&a).unwrap();
        assert!(!output.contains("registry_id") && !output.contains("synthetic-boot"));
    }
    #[test]
    fn bounds_and_duplicate_identities_fail_closed() {
        assert!(assemble(fixture(vec![gpu(1, true), gpu(1, true)])).is_err());
        assert!(assemble(fixture(vec![gpu(0, true)])).is_err());
        assert!(assemble(fixture((1..=32).map(|id| gpu(id, true)).collect())).is_err());
        let mut invalid = fixture(vec![]);
        invalid.logical_cpu_count = 0;
        assert!(assemble(invalid).is_err());
        let observation = assemble(fixture(vec![])).unwrap();
        assert!(observation
            .limitations
            .iter()
            .any(|reason| reason.contains("sandbox")));
    }
    #[test]
    fn free_memory_excludes_speculative_and_all_other_reclaimable_counters() {
        let sample = memory_sample(16384, [10, 3, 20, 5, 8], 1 << 30, 123).unwrap();
        assert_eq!(sample.conservative_available_bytes, 7 * 16384);
        assert_eq!(sample.observed_unix_ms, 123);
        assert!(memory_sample(4096, [1, 2, 0, 0, 0], 1 << 30, 0).is_none());
        assert!(memory_sample(4096, [u64::MAX, 0, 0, 0, 0], u64::MAX, 0).is_none());
        assert!(memory_sample(4096, [100, 0, 0, 0, 0], 10, 0).is_none());
        assert!(memory_sample(3, [0; 5], 100, 0).is_none());
        assert_eq!(
            memory_sample(4096, [0; 5], 1 << 30, 0)
                .unwrap()
                .conservative_available_bytes,
            0
        );
    }
    #[test]
    fn timed_out_native_work_keeps_single_flight_until_it_finishes() {
        let gate = Arc::new(AtomicBool::new(false));
        let (release, wait) = mpsc::channel();
        assert_eq!(
            bounded(gate.clone(), Duration::from_millis(10), move || {
                wait.recv().unwrap();
                Ok(1)
            }),
            Err(InventoryError::TimedOut)
        );
        assert_eq!(
            bounded(gate.clone(), Duration::from_millis(10), || Ok(2)),
            Err(InventoryError::Busy)
        );
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while gate.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(bounded(gate, Duration::from_secs(1), || Ok(3)), Ok(3));
    }
}
