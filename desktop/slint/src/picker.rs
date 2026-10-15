//! User-invoked native file selection. Selecting a path neither reads the file
//! nor grants execution; later inspection/review uses the owned backend API.
use crate::bridge::Result;
fn checked(path: String) -> Result<String> {
    if path.len() > 4096
        || path.chars().any(char::is_control)
        || !std::path::Path::new(&path).is_absolute()
    {
        return Err("The selected file path is invalid or too long.".into());
    }
    Ok(path)
}
#[cfg(target_os = "macos")]
pub fn choose(prompt: &str, directory: bool) -> Result<Option<String>> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};
    use objc2_foundation::NSString;
    let thread =
        MainThreadMarker::new().ok_or("The file picker must open on the native UI thread.")?;
    let panel = NSOpenPanel::openPanel(thread);
    panel.setCanChooseFiles(!directory);
    panel.setCanChooseDirectories(directory);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(false);
    panel.setMessage(Some(&NSString::from_str(prompt)));
    if panel.runModal() != NSModalResponseOK {
        return Ok(None);
    }
    let url = panel
        .URL()
        .ok_or("The file picker did not return a file.")?;
    if !url.isFileURL() {
        return Err("Choose an existing local file.".into());
    }
    let path = url.path().ok_or("The selected file has no local path.")?;
    checked(path.to_string()).map(Some)
}
#[cfg(not(target_os = "macos"))]
pub fn choose(_prompt: &str, _directory: bool) -> Result<Option<String>> {
    Err("Native file selection is not implemented on this platform; enter an explicit existing path.".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_paths_require_bounded_absolute_local_identity() {
        assert!(checked("relative/file".into()).is_err());
        assert!(checked("/tmp/file\nother".into()).is_err());
        assert_eq!(
            checked("/tmp/user selected/file.gguf".into()).unwrap(),
            "/tmp/user selected/file.gguf"
        );
    }
}

pub fn file(prompt: &str) -> Result<Option<String>> {
    choose(prompt, false)
}
#[cfg(target_os = "macos")]
pub fn save(name: &str) -> Result<Option<String>> {
    save_with_message(
        name,
        "Choose a new filename. Existing files will not be overwritten.",
    )
}
#[cfg(target_os = "macos")]
fn save_with_message(name: &str, message: &str) -> Result<Option<String>> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSSavePanel};
    use objc2_foundation::NSString;
    let thread = MainThreadMarker::new().ok_or("Export picker requires the UI thread.")?;
    let panel = NSSavePanel::savePanel(thread);
    panel.setCanCreateDirectories(false);
    panel.setMessage(Some(&NSString::from_str(message)));
    let basename = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    panel.setNameFieldStringValue(&NSString::from_str(basename));
    if panel.runModal() != NSModalResponseOK {
        return Ok(None);
    }
    let url = panel.URL().ok_or("Export destination missing.")?;
    if !url.isFileURL() {
        return Err("Choose a local destination.".into());
    }
    checked(url.path().ok_or("Export path missing.")?.to_string()).map(Some)
}
#[cfg(not(target_os = "macos"))]
pub fn save(_name: &str) -> Result<Option<String>> {
    Err("Native export picker is not yet implemented on this platform.".into())
}
/// The original conversation Export action chooses its format by extension.
/// The writer independently rejects other extensions and existing paths.
pub fn save_conversation() -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        save_with_message("conversation.md", "Export the selected saved branch. Use .md for Markdown or .json for message data. Choose a new filename; existing files will not be overwritten.")
    }
    #[cfg(not(target_os = "macos"))]
    {
        save("conversation.md")
    }
}

/// Explicit user Copy action only; never reads the system clipboard.
#[cfg(target_os = "macos")]
pub fn copy_text(text: &str) -> Result<()> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;
    if text.len() > 1024 * 1024 {
        return Err("Clipboard text exceeds 1 MiB.".into());
    }
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    if board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString }) {
        Ok(())
    } else {
        Err("The clipboard did not accept the text.".into())
    }
}
#[cfg(not(target_os = "macos"))]
pub fn copy_text(_text: &str) -> Result<()> {
    Err("Native clipboard export is not implemented on this platform.".into())
}
