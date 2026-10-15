//! Handwritten macOS bindings to public SDK APIs. No global event injection.
//! All CF references obey create/copy/retain/release ownership. The selected
//! application's retained AX window and element handles bound every operation.
use crate::*;
use std::{ffi::c_void, ptr::NonNull, time::Instant};

type Ref = *const c_void;
const UTF8: u32 = 0x08000100;
#[repr(C)]
#[derive(Default)]
struct Point {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Default)]
struct Size {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Default)]
struct Range {
    location: isize,
    length: isize,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn mach_host_self() -> u32;
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> Ref;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementGetPid(element: Ref, pid: *mut i32) -> i32;
    fn AXUIElementCopyAttributeValue(element: Ref, attribute: Ref, value: *mut Ref) -> i32;
    fn AXUIElementCopyActionNames(element: Ref, actions: *mut Ref) -> i32;
    fn AXUIElementIsAttributeSettable(element: Ref, attribute: Ref, settable: *mut u8) -> i32;
    fn AXUIElementSetAttributeValue(element: Ref, attribute: Ref, value: Ref) -> i32;
    fn AXUIElementPerformAction(element: Ref, action: Ref) -> i32;
    fn AXUIElementSetMessagingTimeout(element: Ref, timeout: f32) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: Ref, kind: i32, output: *mut c_void) -> u8;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(value: Ref) -> Ref;
    fn CFRelease(value: Ref);
    fn CFEqual(a: Ref, b: Ref) -> u8;
    fn CFGetTypeID(value: Ref) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithBytes(
        allocator: Ref,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> Ref;
    fn CFStringGetLength(value: Ref) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(value: Ref, buffer: *mut i8, length: isize, encoding: u32) -> u8;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(value: Ref) -> isize;
    fn CFArrayGetValueAtIndex(value: Ref, index: isize) -> Ref;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: Ref) -> u8;
}
unsafe extern "C" {
    fn mach_port_deallocate(task: u32, port: u32) -> i32;
    static mach_task_self_: u32;
}

struct Cf(NonNull<c_void>);
// AX/CF opaque references have no thread-affine application objects here.
// Ownership moves across threads, and PlatformBackend only operates via &mut;
// this type is deliberately not Sync. Calls are serialized by the host session.
unsafe impl Send for Cf {}
impl Cf {
    fn owned(value: Ref) -> Result<Self> {
        NonNull::new(value.cast_mut())
            .map(Self)
            .ok_or_else(|| Error::Backend("null CoreFoundation object".into()))
    }
    fn ptr(&self) -> Ref {
        self.0.as_ptr().cast_const()
    }
    fn same(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.ptr(), other.ptr()) != 0 }
    }
    fn retained(&self) -> Self {
        unsafe {
            CFRetain(self.ptr());
        }
        Self(self.0)
    }
}
impl Drop for Cf {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.ptr());
        }
    }
}
fn cf_string(value: &str) -> Result<Cf> {
    // Safe because the CF constructor copies the complete UTF-8 buffer.
    Cf::owned(unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            value.as_ptr(),
            value.len() as isize,
            UTF8,
            0,
        )
    })
}
fn string(value: &Cf) -> Option<String> {
    unsafe {
        if CFGetTypeID(value.ptr()) != CFStringGetTypeID() {
            return None;
        }
        let length = CFStringGetLength(value.ptr());
        if !(0..=2048).contains(&length) {
            return None;
        }
        let capacity = CFStringGetMaximumSizeForEncoding(length, UTF8).checked_add(1)?;
        if !(1..=8193).contains(&capacity) {
            return None;
        }
        let mut bytes = vec![0u8; capacity as usize];
        if CFStringGetCString(value.ptr(), bytes.as_mut_ptr().cast(), capacity, UTF8) == 0 {
            return None;
        }
        let end = bytes.iter().position(|b| *b == 0)?;
        if end > 2048 {
            return None;
        }
        String::from_utf8(bytes[..end].to_vec()).ok()
    }
}
fn attr(element: &Cf, name: &str) -> Result<Cf> {
    let name = cf_string(name)?;
    let mut value = std::ptr::null();
    let error = unsafe { AXUIElementCopyAttributeValue(element.ptr(), name.ptr(), &mut value) };
    if error != 0 {
        return Err(Error::Backend(format!(
            "selected-target accessibility attribute failed ({error})"
        )));
    }
    Cf::owned(value)
}
fn text_attr(element: &Cf, name: &str) -> Option<String> {
    attr(element, name).ok().and_then(|v| string(&v))
}
fn bool_attr(element: &Cf, name: &str) -> bool {
    attr(element, name).ok().is_some_and(|v| unsafe {
        CFGetTypeID(v.ptr()) == CFBooleanGetTypeID() && CFBooleanGetValue(v.ptr()) != 0
    })
}
fn array(value: &Cf, limit: usize) -> Result<Vec<Cf>> {
    unsafe {
        if CFGetTypeID(value.ptr()) != CFArrayGetTypeID() {
            return Err(Error::Backend("invalid accessibility array".into()));
        }
        let count = CFArrayGetCount(value.ptr());
        if count < 0 || count as usize > limit {
            return Err(Error::Backend("accessibility tree exceeds bound".into()));
        }
        (0..count)
            .map(|index| {
                let item = CFArrayGetValueAtIndex(value.ptr(), index);
                if item.is_null() {
                    return Err(Error::Backend("null accessibility array item".into()));
                }
                Cf::owned(CFRetain(item))
            })
            .collect()
    }
}
fn bounds(element: &Cf) -> Result<Bounds> {
    let position = attr(element, "AXPosition")?;
    let size = attr(element, "AXSize")?;
    let mut point = Point::default();
    let mut extent = Size::default();
    unsafe {
        if CFGetTypeID(position.ptr()) != AXValueGetTypeID()
            || CFGetTypeID(size.ptr()) != AXValueGetTypeID()
            || AXValueGetValue(position.ptr(), 1, (&mut point as *mut Point).cast()) == 0
            || AXValueGetValue(size.ptr(), 2, (&mut extent as *mut Size).cast()) == 0
        {
            return Err(Error::Backend("invalid accessibility bounds".into()));
        }
    }
    if ![point.x, point.y, extent.width, extent.height]
        .iter()
        .all(|n| n.is_finite())
        || point.x < f64::from(i32::MIN)
        || point.x > f64::from(i32::MAX)
        || point.y < f64::from(i32::MIN)
        || point.y > f64::from(i32::MAX)
        || extent.width < 1.
        || extent.height < 1.
        || extent.width > 65536.
        || extent.height > 65536.
    {
        return Err(Error::Backend("accessibility bounds out of range".into()));
    }
    Ok(Bounds {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
        width: extent.width.round() as u32,
        height: extent.height.round() as u32,
    })
}
fn selection(element: &Cf) -> Option<TextSelection> {
    let value = attr(element, "AXSelectedTextRange").ok()?;
    let mut range = Range::default();
    unsafe {
        if CFGetTypeID(value.ptr()) != AXValueGetTypeID()
            || AXValueGetValue(value.ptr(), 4, (&mut range as *mut Range).cast()) == 0
        {
            return None;
        }
    }
    if range.location < 0 || range.length < 0 {
        return None;
    }
    Some(TextSelection {
        location: range.location as u64,
        length: range.length as u64,
    })
}
fn settable(element: &Cf, name: &str) -> bool {
    let Ok(name) = cf_string(name) else {
        return false;
    };
    let mut yes = 0;
    unsafe { AXUIElementIsAttributeSettable(element.ptr(), name.ptr(), &mut yes) == 0 && yes != 0 }
}
fn pressable(element: &Cf) -> bool {
    let mut output = std::ptr::null();
    if unsafe { AXUIElementCopyActionNames(element.ptr(), &mut output) } != 0 {
        return false;
    }
    Cf::owned(output)
        .and_then(|a| array(&a, 128))
        .is_ok_and(|names| {
            names
                .iter()
                .any(|name| string(name).as_deref() == Some("AXPress"))
        })
}
fn trusted() -> Result<()> {
    // This never opens System Settings or prompts; only the user can grant TCC.
    if unsafe { AXIsProcessTrusted() } == 0 {
        Err(Error::PermissionRequired(
            "macOS Accessibility permission must be granted explicitly to the host application"
                .into(),
        ))
    } else {
        Ok(())
    }
}
struct Selected {
    identity: TargetIdentity,
    pid: i32,
    process_identity: String,
    app: Cf,
    window: Cf,
    elements: Vec<(String, Cf)>,
}
#[derive(Default)]
pub struct NativeBackend {
    selected: Option<Selected>,
}
impl Selected {
    fn validate(&self, target: &TargetIdentity) -> Result<()> {
        trusted()?;
        if &self.identity != target
            || super::targets::process_identity(self.pid)? != self.process_identity
            || !bool_attr(&self.app, "AXFrontmost")
        {
            return Err(Error::TargetChanged);
        }
        let focused = attr(&self.app, "AXFocusedWindow").map_err(|_| Error::TargetChanged)?;
        if !self.window.same(&focused) {
            return Err(Error::TargetChanged);
        }
        let mut pid = 0;
        if unsafe { AXUIElementGetPid(self.window.ptr(), &mut pid) } != 0 || pid != self.pid {
            return Err(Error::TargetChanged);
        }
        Ok(())
    }
}
impl PlatformBackend for NativeBackend {
    fn capabilities(&self) -> PlatformCapabilities {
        let permission = Support::RequiresUserPermission("explicit selected-process/window grant and existing macOS Accessibility permission; native UI behavior is not hardware-qualified".into());
        PlatformCapabilities { platform: "macos".into(), execution_kind: ExecutionKind::Native, system_monitor: Support::Available,
            screen_elements: permission.clone(), screen_click: permission.clone(), screen_type: permission,
            screenshot: Support::Unsupported("pixel capture is not implemented; no Screen Recording request is made".into()),
            notes: vec!["Click invokes AXPress on an exact retained element; it does not move the mouse".into(),
                "Text replaces the focused control's selected text through AXSelectedText; unsupported controls are rejected".into(),
                "250 elements, depth 24, five-second traversal budget; no background or global desktop scanning".into()] }
    }
    fn select(&mut self, request: TargetRequest) -> Result<TargetIdentity> {
        self.release();
        let (pid, expected_process) = match request {
            TargetRequest::MacProcess { pid } => (pid, None),
            TargetRequest::MacProcessIdentity {
                pid,
                process_identity,
            } => (pid, Some(process_identity)),
            _ => {
                return Err(Error::Unsupported(
                    "native backend requires an explicitly selected macOS process".into(),
                ))
            }
        };
        if pid <= 0 {
            return Err(Error::InvalidInput(
                "positive selected application pid required".into(),
            ));
        }
        let process_identity = super::targets::process_identity(pid)?;
        if expected_process
            .as_ref()
            .is_some_and(|expected| expected != &process_identity)
        {
            return Err(Error::TargetChanged);
        }
        trusted()?;
        let app = Cf::owned(unsafe { AXUIElementCreateApplication(pid) })?;
        if unsafe { AXUIElementSetMessagingTimeout(app.ptr(), 0.2) } != 0 {
            return Err(Error::Backend(
                "cannot set accessibility message timeout".into(),
            ));
        }
        if !bool_attr(&app, "AXFrontmost") {
            return Err(Error::Denied(
                "selected application must be frontmost when the user grants access".into(),
            ));
        }
        let window = attr(&app, "AXFocusedWindow")?;
        if unsafe { CFGetTypeID(window.ptr()) != AXUIElementGetTypeID() } {
            return Err(Error::Backend(
                "selected window is not an accessibility element".into(),
            ));
        }
        let identity = TargetIdentity {
            id: uuid::Uuid::new_v4().to_string(),
            label: format!("Selected application {pid} window"),
            execution_kind: ExecutionKind::Native,
        };
        self.selected = Some(Selected {
            identity: identity.clone(),
            pid,
            process_identity,
            app,
            window,
            elements: Vec::new(),
        });
        Ok(identity)
    }
    fn frame(&mut self, target: &TargetIdentity) -> Result<Frame> {
        let s = self.selected.as_mut().ok_or(Error::TargetChanged)?;
        s.validate(target)?;
        let started = Instant::now();
        let window_bounds = bounds(&s.window)?;
        let focus = attr(&s.app, "AXFocusedUIElement").ok();
        let mut queue = vec![(s.window.retained(), 0usize)];
        let mut visited: Vec<Cf> = Vec::new();
        let mut found = Vec::new();
        let mut handles = Vec::new();
        while let Some((element, depth)) = queue.pop() {
            if started.elapsed() >= Duration::from_secs(5) {
                return Err(Error::Backend(
                    "selected accessibility tree scan timed out".into(),
                ));
            }
            if visited.iter().any(|previous| previous.same(&element)) {
                continue;
            }
            if visited.len() >= 250 || depth > 24 {
                return Err(Error::Backend(
                    "selected accessibility tree exceeds bound".into(),
                ));
            }
            visited.push(element.retained());
            if unsafe { CFGetTypeID(element.ptr()) != AXUIElementGetTypeID() } {
                return Err(Error::Backend(
                    "invalid selected accessibility element".into(),
                ));
            }
            if unsafe { AXUIElementSetMessagingTimeout(element.ptr(), 0.2) } != 0 {
                return Err(Error::Backend(
                    "cannot bound selected accessibility element timeout".into(),
                ));
            }
            let mut pid = 0;
            if unsafe { AXUIElementGetPid(element.ptr(), &mut pid) } != 0 || pid != s.pid {
                return Err(Error::TargetChanged);
            }
            let secure = text_attr(&element, "AXSubrole").as_deref() == Some("AXSecureTextField");
            if let Ok(children) = if secure {
                Err(Error::Denied("secure field subtree".into()))
            } else {
                attr(&element, "AXChildren")
            } {
                for child in array(&children, 250)?.into_iter().rev() {
                    queue.push((child, depth + 1));
                }
                if queue.len() > 250 {
                    return Err(Error::Backend(
                        "selected accessibility tree exceeds bound".into(),
                    ));
                }
            }
            let Ok(rect) = bounds(&element) else {
                continue;
            };
            let role = text_attr(&element, "AXRole").unwrap_or_else(|| "AXUnknown".into());
            let focused = focus.as_ref().is_some_and(|f| f.same(&element));
            let text_selection = if focused && !secure {
                selection(&element)
            } else {
                None
            };
            let can_insert_text = focused
                && !secure
                && text_selection.is_some()
                && settable(&element, "AXSelectedText");
            let id = s
                .elements
                .iter()
                .find(|(_, old)| old.same(&element))
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            found.push(UiElement {
                id: id.clone(),
                role,
                name: if secure {
                    None
                } else {
                    text_attr(&element, "AXTitle")
                        .filter(|s| !s.is_empty())
                        .or_else(|| text_attr(&element, "AXDescription"))
                },
                bounds: rect,
                enabled: bool_attr(&element, "AXEnabled"),
                focused,
                can_press: !secure && pressable(&element),
                can_insert_text,
                text_selection,
            });
            handles.push((id, element));
        }
        s.validate(target)?;
        s.elements = handles;
        Ok(Frame {
            target: target.clone(),
            bounds: window_bounds,
            elements: found,
        })
    }
    fn act(&mut self, plan: &ActionPlan) -> Result<()> {
        let s = self.selected.as_ref().ok_or(Error::TargetChanged)?;
        s.validate(&plan.target)?;
        let element = &s
            .elements
            .iter()
            .find(|(id, _)| id == &plan.element.id)
            .ok_or(Error::SnapshotChanged)?
            .1;
        let containing_window = attr(element, "AXWindow").map_err(|_| Error::TargetChanged)?;
        if !containing_window.same(&s.window) {
            return Err(Error::TargetChanged);
        }
        // The retained AX element dispatch stays scoped even if focus changes
        // between this check and the OS call; no global event fallback exists.
        let error = match &plan.action {
            Action::Click { x, y } => {
                if !bounds(element)?.contains(*x, *y)
                    || !pressable(element)
                    || !bool_attr(element, "AXEnabled")
                {
                    return Err(Error::SnapshotChanged);
                }
                let press = cf_string("AXPress")?;
                unsafe { AXUIElementPerformAction(element.ptr(), press.ptr()) }
            }
            Action::TypeText { text } => {
                let focus = attr(&s.app, "AXFocusedUIElement").map_err(|_| Error::TargetChanged)?;
                if !focus.same(element)
                    || selection(element) != plan.element.text_selection
                    || !settable(element, "AXSelectedText")
                {
                    return Err(Error::SnapshotChanged);
                }
                let attribute = cf_string("AXSelectedText")?;
                let text = cf_string(text)?;
                unsafe { AXUIElementSetAttributeValue(element.ptr(), attribute.ptr(), text.ptr()) }
            }
        };
        if error != 0 {
            return Err(Error::UncertainSideEffect(format!("selected accessibility action returned {error}; inspect before preparing a new action")));
        }
        Ok(())
    }
    fn system_monitor(&mut self) -> Result<SystemSnapshot> {
        system_snapshot()
    }
    fn release(&mut self) {
        self.selected = None;
    }
}

struct HostPort(u32);
impl Drop for HostPort {
    fn drop(&mut self) {
        unsafe {
            mach_port_deallocate(mach_task_self_, self.0);
        }
    }
}
fn system_snapshot() -> Result<SystemSnapshot> {
    let port = HostPort(unsafe { mach_host_self() });
    let start = Instant::now();
    let before = cpu_ticks(port.0)?;
    std::thread::sleep(Duration::from_millis(250));
    let after = cpu_ticks(port.0)?;
    let ticks: Vec<u64> = after
        .iter()
        .zip(before)
        .map(|(a, b)| u64::from(a.wrapping_sub(b)))
        .collect();
    let total: u64 = ticks.iter().sum();
    let cpu_percent = if total == 0 {
        0.
    } else {
        ((total - ticks[libc::CPU_STATE_IDLE as usize]) as f64 / total as f64 * 1000.).round() / 10.
    };
    let mut memory_total_bytes = 0u64;
    let mut size = std::mem::size_of::<u64>();
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut memory_total_bytes as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size != std::mem::size_of::<u64>() || memory_total_bytes == 0 {
        return Err(Error::Backend("memory total sample failed".into()));
    }
    let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    if unsafe {
        libc::host_statistics64(
            port.0,
            libc::HOST_VM_INFO64,
            (&mut vm as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    } != 0
    {
        return Err(Error::Backend("memory usage sample failed".into()));
    }
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(Error::Backend("memory page size sample failed".into()));
    }
    // Darwin physical use: active + wired + compressor pages, excluding
    // reclaimable inactive/file-cache pages. The definition is documented.
    let used_pages =
        u64::from(vm.active_count) + u64::from(vm.wire_count) + u64::from(vm.compressor_page_count);
    let memory_used_bytes = used_pages
        .saturating_mul(page_size as u64)
        .min(memory_total_bytes);
    let mounts = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if !(0..=256).contains(&mounts) {
        return Err(Error::Backend(
            "disk inventory exceeds bound or failed".into(),
        ));
    }
    let mut stats: Vec<libc::statfs> = (0..mounts).map(|_| unsafe { std::mem::zeroed() }).collect();
    let count = unsafe {
        libc::getfsstat(
            stats.as_mut_ptr(),
            (stats.len() * std::mem::size_of::<libc::statfs>()) as i32,
            libc::MNT_NOWAIT,
        )
    };
    if count < 0 || count as usize > stats.len() {
        return Err(Error::Backend(
            "disk inventory changed during sample".into(),
        ));
    }
    let disks = stats
        .into_iter()
        .take(count as usize)
        .filter(|s| s.f_blocks > 0)
        .map(|s| {
            let name: Vec<u8> = s
                .f_mntonname
                .iter()
                .take_while(|b| **b != 0)
                .map(|b| *b as u8)
                .collect();
            DiskUsage {
                mount: String::from_utf8_lossy(&name).into_owned(),
                total_bytes: s.f_blocks.saturating_mul(u64::from(s.f_bsize)),
                available_bytes: s.f_bavail.saturating_mul(u64::from(s.f_bsize)),
            }
        })
        .collect();
    Ok(SystemSnapshot {
        cpu_percent,
        memory_used_bytes,
        memory_total_bytes,
        disks,
        execution_kind: ExecutionKind::Native,
        sample_ms: start.elapsed().as_millis() as u64,
    })
}
fn cpu_ticks(port: u32) -> Result<[u32; 4]> {
    let mut data: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    if unsafe {
        libc::host_statistics(
            port,
            libc::HOST_CPU_LOAD_INFO,
            (&mut data as *mut libc::host_cpu_load_info).cast(),
            &mut count,
        )
    } != 0
        || count != libc::HOST_CPU_LOAD_INFO_COUNT
    {
        return Err(Error::Backend("CPU sample failed".into()));
    }
    Ok(data.cpu_ticks)
}
