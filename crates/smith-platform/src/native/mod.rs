//! Native construction/capability discovery do not query accessibility or ask
//! for OS permissions. Only explicit target selection can read the chosen app.
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::NativeBackend;
#[cfg(target_os = "macos")]
mod targets;
#[cfg(target_os = "macos")]
pub use targets::selectable_targets;
#[cfg(not(target_os = "macos"))]
pub fn selectable_targets() -> crate::Result<Vec<crate::ProcessTarget>> {
    Err(unsupported())
}

#[cfg(not(target_os = "macos"))]
pub struct NativeBackend;
#[cfg(not(target_os = "macos"))]
impl Default for NativeBackend {
    fn default() -> Self {
        Self
    }
}
#[cfg(not(target_os = "macos"))]
impl crate::PlatformBackend for NativeBackend {
    fn capabilities(&self) -> crate::PlatformCapabilities {
        let unavailable = crate::Support::Unsupported("native backend is implemented only for macOS; Windows UIAutomation/input and Linux desktop backends are not ported or qualified".into());
        crate::PlatformCapabilities {
            platform: std::env::consts::OS.into(),
            execution_kind: crate::ExecutionKind::Native,
            system_monitor: unavailable.clone(),
            screen_elements: unavailable.clone(),
            screen_click: unavailable.clone(),
            screen_type: unavailable.clone(),
            screenshot: unavailable,
            notes: vec![
                "No fallback to global input, shell automation, or another platform".into(),
            ],
        }
    }
    fn select(&mut self, _: crate::TargetRequest) -> crate::Result<crate::TargetIdentity> {
        Err(unsupported())
    }
    fn frame(&mut self, _: &crate::TargetIdentity) -> crate::Result<crate::Frame> {
        Err(unsupported())
    }
    fn act(&mut self, _: &crate::ActionPlan) -> crate::Result<()> {
        Err(unsupported())
    }
    fn system_monitor(&mut self) -> crate::Result<crate::SystemSnapshot> {
        Err(unsupported())
    }
    fn release(&mut self) {}
}
#[cfg(not(target_os = "macos"))]
fn unsupported() -> crate::Error {
    crate::Error::Unsupported("native platform is not implemented".into())
}
