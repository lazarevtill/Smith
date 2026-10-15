#![cfg(unix)]
use serde_json::json;
use smith_agent_core::{Error, SelectedPlugin};
use std::{fs, path::PathBuf};

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("plugin");
        fs::create_dir(&path).unwrap();
        let f = Self { _dir: dir, path };
        f.manifest(json!({"id":"fixture.plugin","name":"Fixture","instructions_file":"SKILL.md","tools":["run_shell"]}));
        fs::write(f.path.join("SKILL.md"), "Synthetic instructions 🐟").unwrap();
        f
    }
    fn manifest(&self, value: serde_json::Value) {
        fs::write(self.path.join("plugin.json"), value.to_string()).unwrap();
    }
}

#[test]
fn explicit_directory_resolves_one_file_and_keeps_reviewed_bytes_and_provenance() {
    let f = Fixture::new();
    fs::write(f.path.join("ignored-private-file"), "never read").unwrap();
    let selected = SelectedPlugin::inspect(&f.path).unwrap();
    assert_eq!(
        selected.manifest().instructions.as_deref(),
        Some("Synthetic instructions 🐟")
    );
    assert_eq!(selected.manifest().tools, vec!["run_shell"]);
    assert_eq!(selected.evidence().files.len(), 2);
    assert_eq!(selected.evidence().source_sha256.len(), 64);
    assert_eq!(
        selected.evidence().files[1].bytes,
        "Synthetic instructions 🐟".len() as u64
    );
    selected.revalidate().unwrap();
    fs::write(f.path.join("SKILL.md"), "Changed source").unwrap();
    assert!(matches!(selected.revalidate(), Err(Error::Conflict)));
    assert_eq!(
        selected.manifest().instructions.as_deref(),
        Some("Synthetic instructions 🐟")
    );
}

#[test]
fn inline_precedence_never_opens_the_inert_file_reference_even_when_empty() {
    let f = Fixture::new();
    for inline in ["", "Inline stays exact\n"] {
        f.manifest(json!({"id":"fixture.inline","name":"Inline","instructions":inline,"instructions_file":"../../outside"}));
        let s = SelectedPlugin::inspect(&f.path).unwrap();
        assert_eq!(s.manifest().instructions.as_deref(), Some(inline));
        assert_eq!(s.evidence().files.len(), 1);
    }
}

#[test]
fn traversal_absolute_nested_and_special_instruction_sources_are_refused() {
    let f = Fixture::new();
    for name in [
        "../secret",
        "/etc/passwd",
        "folder/file",
        "folder\\file",
        ".",
        "..",
        "plugin.json",
        "file:stream",
    ] {
        f.manifest(json!({"id":"fixture","name":"Fixture","instructions_file":name}));
        assert!(SelectedPlugin::inspect(&f.path).is_err(), "{name}");
    }
    f.manifest(json!({"id":"fixture","name":"Fixture","instructions_file":"fifo"}));
    // Fixture construction only; no production path starts any process.
    assert!(std::process::Command::new("/usr/bin/mkfifo")
        .arg(f.path.join("fifo"))
        .status()
        .unwrap()
        .success());
    assert!(SelectedPlugin::inspect(&f.path).is_err());
    assert!(SelectedPlugin::inspect(&f.path.join("..").join("plugin")).is_err());
}

#[test]
fn symlink_ancestors_manifests_files_and_hardlinks_cannot_supply_content() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let parent = f.path.parent().unwrap();
    fs::write(parent.join("outside"), "OUTSIDE_SENTINEL").unwrap();
    symlink(&f.path, parent.join("alias")).unwrap();
    assert!(SelectedPlugin::inspect(&parent.join("alias")).is_err());
    fs::remove_file(f.path.join("SKILL.md")).unwrap();
    symlink(parent.join("outside"), f.path.join("SKILL.md")).unwrap();
    assert!(SelectedPlugin::inspect(&f.path).is_err());
    fs::remove_file(f.path.join("SKILL.md")).unwrap();
    fs::hard_link(parent.join("outside"), f.path.join("SKILL.md")).unwrap();
    assert!(SelectedPlugin::inspect(&f.path).is_err());
    fs::remove_file(f.path.join("plugin.json")).unwrap();
    symlink(parent.join("outside"), f.path.join("plugin.json")).unwrap();
    assert!(SelectedPlugin::inspect(&f.path).is_err());
}

#[test]
fn replacement_of_selection_or_same_bytes_file_invalidates_the_review() {
    let f = Fixture::new();
    let selected = SelectedPlugin::inspect(&f.path).unwrap();
    let text = fs::read(f.path.join("SKILL.md")).unwrap();
    fs::write(f.path.join("replacement"), text).unwrap();
    fs::rename(f.path.join("replacement"), f.path.join("SKILL.md")).unwrap();
    assert!(matches!(selected.revalidate(), Err(Error::Conflict)));
    let selected = SelectedPlugin::inspect(&f.path).unwrap();
    fs::rename(&f.path, f.path.with_file_name("moved")).unwrap();
    fs::create_dir(&f.path).unwrap();
    assert!(matches!(selected.revalidate(), Err(Error::Conflict)));
}

#[test]
fn exact_instruction_cap_utf8_manifest_bounds_and_strict_fields_are_enforced() {
    let f = Fixture::new();
    fs::write(f.path.join("SKILL.md"), "x".repeat(8192)).unwrap();
    assert_eq!(
        SelectedPlugin::inspect(&f.path)
            .unwrap()
            .manifest()
            .instructions
            .as_ref()
            .unwrap()
            .len(),
        8192
    );
    fs::write(f.path.join("SKILL.md"), "x".repeat(8193)).unwrap();
    assert!(matches!(
        SelectedPlugin::inspect(&f.path),
        Err(Error::LimitExceeded(_))
    ));
    fs::write(f.path.join("SKILL.md"), [0xff]).unwrap();
    assert!(matches!(
        SelectedPlugin::inspect(&f.path),
        Err(Error::InvalidInput(_))
    ));
    f.manifest(json!({"id":"fixture","name":"Fixture","execute":"malicious"}));
    assert!(matches!(
        SelectedPlugin::inspect(&f.path),
        Err(Error::InvalidInput(_))
    ));
    fs::write(f.path.join("plugin.json"), " ".repeat(32769)).unwrap();
    assert!(matches!(
        SelectedPlugin::inspect(&f.path),
        Err(Error::LimitExceeded(_))
    ));
    fs::write(
        f.path.join("plugin.json"),
        r#"{"id":"one","id":"two","name":"Fixture"}"#,
    )
    .unwrap();
    assert!(SelectedPlugin::inspect(&f.path).is_err());
}

#[test]
fn concurrent_symlink_swaps_never_capture_external_instruction_bytes() {
    use std::os::unix::fs::symlink;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let f = Fixture::new();
    let outside = f.path.parent().unwrap().join("outside");
    fs::write(&outside, "OUTSIDE_SENTINEL").unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let path = f.path.clone();
    let writer = std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            let _ = symlink(&outside, path.join("swap"));
            let _ = fs::rename(path.join("swap"), path.join("SKILL.md"));
            let _ = fs::write(path.join("safe"), "INSIDE");
            let _ = fs::rename(path.join("safe"), path.join("SKILL.md"));
        }
    });
    for _ in 0..100 {
        if let Ok(s) = SelectedPlugin::inspect(&f.path) {
            assert_ne!(
                s.manifest().instructions.as_deref(),
                Some("OUTSIDE_SENTINEL")
            );
        }
    }
    done.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}
