//! Explicit user-invoked application picker only. Lists regular application
//! names/PIDs via NSWorkspace; no accessibility, windows, documents or values.
use crate::*;
use std::ffi::{c_void, CStr};
type Obj = *const c_void;
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const i8) -> Obj;
    fn sel_registerName(name: *const i8) -> Obj;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}
struct Pool(*mut c_void);
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            objc_autoreleasePoolPop(self.0);
        }
    }
}

/// The host calls this only after the user opens its native target picker.
/// Returned IDs bind PID + kernel process start time, not model-supplied names.
pub fn selectable_targets() -> Result<Vec<ProcessTarget>> {
    let _pool = Pool(unsafe { objc_autoreleasePoolPush() });
    unsafe {
        let object: unsafe extern "C" fn(Obj, Obj) -> Obj =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let integer: unsafe extern "C" fn(Obj, Obj) -> isize =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let pid: unsafe extern "C" fn(Obj, Obj) -> i32 =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let at: unsafe extern "C" fn(Obj, Obj, usize) -> Obj =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let utf8: unsafe extern "C" fn(Obj, Obj) -> *const i8 =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let workspace = object(
            objc_getClass(c"NSWorkspace".as_ptr()),
            sel_registerName(c"sharedWorkspace".as_ptr()),
        );
        if workspace.is_null() {
            return Err(Error::Backend(
                "native application picker unavailable".into(),
            ));
        }
        let applications = object(workspace, sel_registerName(c"runningApplications".as_ptr()));
        if applications.is_null() {
            return Err(Error::Backend("native application list unavailable".into()));
        }
        let count = integer(applications, sel_registerName(c"count".as_ptr()));
        if !(0..=512).contains(&count) {
            return Err(Error::Backend("application picker exceeds bound".into()));
        }
        let mut result = Vec::new();
        for index in 0..count {
            let app = at(
                applications,
                sel_registerName(c"objectAtIndex:".as_ptr()),
                index as usize,
            );
            if app.is_null() || integer(app, sel_registerName(c"activationPolicy".as_ptr())) != 0 {
                continue;
            }
            let process_id = pid(app, sel_registerName(c"processIdentifier".as_ptr()));
            let Ok(id) = process_identity(process_id) else {
                continue;
            };
            let name = object(app, sel_registerName(c"localizedName".as_ptr()));
            let pointer = if name.is_null() {
                std::ptr::null()
            } else {
                utf8(name, sel_registerName(c"UTF8String".as_ptr()))
            };
            let label = if pointer.is_null() {
                format!("Application {process_id}")
            } else {
                CStr::from_ptr(pointer)
                    .to_string_lossy()
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(128)
                    .collect()
            };
            result.push(ProcessTarget {
                id,
                pid: process_id,
                label,
            });
        }
        result.sort_by(|a, b| a.label.cmp(&b.label).then(a.pid.cmp(&b.pid)));
        Ok(result)
    }
}
pub(super) fn process_identity(pid: i32) -> Result<String> {
    if pid <= 0 {
        return Err(Error::TargetChanged);
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    if unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    } != size
        || info.pbi_pid != pid as u32
    {
        return Err(Error::TargetChanged);
    }
    Ok(format!(
        "{pid}:{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}
