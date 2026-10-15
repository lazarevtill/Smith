#![cfg(all(feature = "native-ui", unix))]
//! Explicit owned normal backend, original renderer and synthetic local data only.
use slint::{ComponentHandle, Model};
use smith_native_bridge::{
    bridge::{Result, Transport},
    native::{
        register_reviewed_fonts, Driver, MainWindow, NativeAgents, NativeExtensions,
        NativeHistorical, NativeProfile, NativeTasks, NativeTranscription,
    },
    owned_backend::{OwnedBackend, OwnedTransport},
    task_bridge::new_task_request_id,
};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Key(String);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            for b in self.0.as_mut_vec() {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}
struct LostImport {
    inner: OwnedTransport,
    imports: Arc<AtomicUsize>,
}
impl Transport for LostImport {
    fn request(
        &mut self,
        m: &str,
        p: &str,
        s: Option<&str>,
        b: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let r = self.inner.request(m, p, s, b)?;
        if m == "POST"
            && p == "/api/agent/extensions/plugin-import"
            && self.imports.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err("Synthetic lost committed import acknowledgement".into());
        }
        Ok(r)
    }
    fn sensitive_request(
        &mut self,
        m: &str,
        p: &str,
        s: Option<&str>,
        b: smith_native_bridge::profile_bridge::SensitiveBody,
    ) -> Result<serde_json::Value> {
        self.inner.sensitive_request(m, p, s, b)
    }
    fn transcription_start(
        &mut self,
        s: &str,
        b: serde_json::Value,
    ) -> std::result::Result<
        serde_json::Value,
        smith_native_bridge::transcription::TranscriptionSubmitError,
    > {
        self.inner.transcription_start(s, b)
    }
    fn transcription_file_start(
        &mut self,
        s: &str,
        b: serde_json::Value,
    ) -> std::result::Result<
        serde_json::Value,
        smith_native_bridge::transcription::TranscriptionSubmitError,
    > {
        self.inner.transcription_file_start(s, b)
    }
    fn event_snapshot(&mut self, p: &str, s: &str) -> Result<Option<serde_json::Value>> {
        self.inner.event_snapshot(p, s)
    }
}
fn wait(w: &MainWindow, d: &Driver, c: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        d.pump(w);
        if c() {
            return;
        }
        assert!(
            Instant::now() < until,
            "Native extensions timed out: {}",
            w.get_status_text()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}
fn snapshot(w: &MainWindow, name: &str) {
    let Some(root) = std::env::var_os("SMITH_NATIVE_TEST_CAPTURES") else {
        return;
    };
    std::fs::create_dir_all(&root).unwrap();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    let p = w.window().take_snapshot().unwrap();
    let mut f = std::fs::File::create(Path::new(&root).join(format!("{name}.ppm"))).unwrap();
    write!(f, "P6\n{} {}\n255\n", p.width(), p.height()).unwrap();
    for p in p.as_slice() {
        f.write_all(&[p.r, p.g, p.b]).unwrap();
    }
}
#[test]
#[ignore = "Explicit frozen backend with extensions, historical-memory and normalized transcription routes"]
fn native_extensions_capture_import_historical_memory_and_normalization() {
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("software".into()),
            mock_time: true,
            threading: false,
            ..Default::default()
        },
    )))
    .unwrap();
    register_reviewed_fonts().unwrap();
    let root = Temp(std::env::temp_dir().canonicalize().unwrap().join(format!(
        "smith-native-instructions-{}",
        new_task_request_id().unwrap()
    )));
    std::fs::create_dir(&root.0).unwrap();
    let host = root.0.join("host");
    std::fs::create_dir(&host).unwrap();
    let backend = OwnedBackend::launch(
        &PathBuf::from(std::env::var_os("SMITH_NATIVE_TEST_BACKEND").unwrap()),
        &host,
    )
    .unwrap();
    let w = MainWindow::new().unwrap();
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    let imports = Arc::new(AtomicUsize::new(0));
    let d = Driver::attach_transport_with_profile(
        &w,
        LostImport {
            inner: backend.transport(),
            imports: imports.clone(),
        },
        &host,
    )
    .unwrap();
    w.show().unwrap();
    wait(&w, &d, || d.ready());
    w.set_active_view(3);
    w.set_autonomy_sub_view(4);
    let x = w.global::<NativeExtensions>();
    x.invoke_refresh();
    wait(&w, &d, || !x.get_busy());
    w.set_skill_name("Captured local skill".into());
    w.set_skill_desc("Synthetic instruction capture".into());
    w.set_skill_instructions("Use the blue notebook.".into());
    w.set_skill_enabled(true);
    w.invoke_add_skill();
    wait(&w, &d, || w.get_skills().row_count() == 1 && !x.get_busy());
    let skill = w.get_skills().row_data(0).unwrap().id;
    w.invoke_add_skill();
    wait(&w, &d, || !x.get_busy());
    assert!(w
        .get_skills()
        .row_data(0)
        .unwrap()
        .detail
        .contains("Revision 1"));
    snapshot(&w, "native-extensions-skill");
    let agents = w.global::<NativeAgents>();
    w.set_autonomy_sub_view(0);
    agents.set_tasks_mode(true);
    let tasks = w.global::<NativeTasks>();
    tasks.invoke_refresh();
    wait(&w, &d, || !tasks.get_busy());
    tasks.set_label("Instruction capture workspace".into());
    tasks.set_read(true);
    tasks.set_control(true);
    tasks.invoke_review_workspace();
    wait(&w, &d, || {
        !tasks.get_busy() && !tasks.get_proposal().is_empty()
    });
    tasks.set_confirmed(true);
    tasks.invoke_provision();
    wait(&w, &d, || {
        !tasks.get_busy() && tasks.get_workspaces().row_count() == 1
    });
    tasks.set_input("Capture enabled instructions only.".into());
    tasks.invoke_create();
    wait(&w, &d, || !tasks.get_busy() && tasks.get_selected_task());
    assert!(tasks.get_detail().contains("Captured local skill"));
    let captured = tasks.get_detail();
    w.set_autonomy_sub_view(4);
    x.invoke_refresh();
    wait(&w, &d, || !x.get_busy());
    x.invoke_edit(skill.clone());
    wait(&w, &d, || {
        !x.get_busy() && w.get_skill_name() == "Captured local skill"
    });
    w.set_skill_instructions("Use the green notebook.".into());
    w.invoke_add_skill();
    wait(&w, &d, || {
        !x.get_busy()
            && w.get_skills()
                .row_data(0)
                .is_some_and(|r| r.detail.contains("Revision 2"))
    });
    w.invoke_remove_skill(skill.clone());
    wait(&w, &d, || !x.get_busy() && w.get_skills().row_count() == 0);
    x.set_show_trash(true);
    d.pump(&w);
    assert_eq!(w.get_skills().row_count(), 1);
    w.invoke_remove_skill(skill.clone());
    wait(&w, &d, || !x.get_busy() && w.get_skills().row_count() == 0);
    x.set_show_trash(false);
    d.pump(&w);
    assert!(w.get_skills().row_data(0).unwrap().enabled);
    assert_eq!(tasks.get_detail(), captured);
    let mut inspector = smith_native_bridge::bridge::Bridge::new(backend.transport());
    inspector.initialize().unwrap();
    let task_id = inspector.task_overview().unwrap().tasks[0].id.clone();
    let captured_task = inspector.task_snapshot(&task_id).unwrap();
    assert_eq!(
        captured_task.task.extensions.as_ref().unwrap()["items"][0]["instructions"],
        "Use the blue notebook."
    );
    assert_eq!(
        captured_task.task.extensions.as_ref().unwrap()["items"][0]["revision"],
        1
    );
    // A lost import acknowledgement is recovered only by the user's same-request retry.
    let plugin = root.0.join("selected-plugin");
    std::fs::create_dir(&plugin).unwrap();
    std::fs::write(plugin.join("plugin.json"),br#"{"id":"native.selected","name":"Selected native plugin","description":"Synthetic local selection","version":"1","authors":[],"instructions_file":"instructions.md","tools":["read_file"]}"#).unwrap();
    std::fs::write(
        plugin.join("instructions.md"),
        "Read the selected notebook only.",
    )
    .unwrap();
    x.set_import_open(true);
    x.set_import_directory(plugin.to_str().unwrap().into());
    x.invoke_review_plugin();
    wait(&w, &d, || !x.get_busy() && x.get_import_ready());
    assert!(x.get_import_instructions().contains("selected notebook"));
    snapshot(&w, "native-plugin-reviewed");
    x.invoke_import_reviewed();
    wait(&w, &d, || !x.get_busy() && x.get_import_uncertain());
    x.invoke_import_reviewed();
    wait(&w, &d, || {
        !x.get_busy() && !x.get_import_ready() && x.get_plugins().row_count() == 1
    });
    assert_eq!(imports.load(Ordering::SeqCst), 2);
    assert!(!x.get_plugins().row_data(0).unwrap().enabled);
    x.set_import_open(false);
    snapshot(&w, "native-plugin-imported-disabled");
    // Saved-agent exact selection is retained across an unrelated edit.
    w.set_autonomy_sub_view(0);
    agents.set_tasks_mode(false);
    agents.invoke_refresh();
    wait(&w, &d, || {
        !agents.get_busy() && agents.get_workspaces().row_count() == 1
    });
    w.invoke_new_agent();
    wait(&w, &d, || !agents.get_busy());
    agents.invoke_select_workspace(0);
    for (i, c) in agents.get_capabilities().iter().enumerate() {
        if c.label == "read" || c.label == "control" {
            agents.invoke_toggle_capability(i as i32);
        }
    }
    agents.set_instructions_open(true);
    agents.set_instructions_mode(2);
    let chosen = agents
        .get_instructions()
        .iter()
        .position(|v| v.label.contains("Captured local skill"))
        .unwrap();
    agents.invoke_toggle_instruction(chosen as i32);
    w.set_agent_name("Exact instruction agent".into());
    w.set_agent_prompt("Use captured instructions.".into());
    w.invoke_save_agent();
    wait(&w, &d, || {
        !agents.get_busy() && !w.get_editing_agent().is_empty()
    });
    assert_eq!(agents.get_instructions_mode(), 2);
    w.set_agent_soul("Concise.".into());
    w.invoke_save_agent();
    wait(&w, &d, || !agents.get_busy());
    assert_eq!(agents.get_instructions_mode(), 2);
    assert_eq!(
        agents
            .get_instructions()
            .iter()
            .filter(|r| r.selected)
            .count(),
        1
    );
    snapshot(&w, "native-agent-exact-instructions");
    // Import a generated historical profile, never an existing user profile.
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    let key = Key(bytes.iter().map(|b| format!("{b:02x}")).collect());
    let source = root.0.join("historical-copy");
    let mut child = Command::new(std::env::var_os("SMITH_NATIVE_TEST_HISTORICAL_FIXTURE").unwrap())
        .arg(&source)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(key.0.as_bytes())
        .unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            assert!(s.success());
            break;
        }
        if Instant::now() > until {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Synthetic historical fixture timeout")
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    w.set_active_view(2);
    w.set_settings_tab(8);
    d.pump(&w);
    let profile = w.global::<NativeProfile>();
    profile.set_source(source.to_str().unwrap().into());
    profile.set_managed(source.join("artifacts").to_str().unwrap().into());
    profile.set_attested(true);
    profile.set_master(key.0.clone().into());
    profile.invoke_inspect();
    assert!(profile.get_master().is_empty());
    wait(&w, &d, || profile.get_reviewed() && !profile.get_busy());
    profile.set_master(key.0.clone().into());
    profile.set_confirmed(true);
    profile.invoke_import();
    assert!(profile.get_master().is_empty());
    wait(&w, &d, || {
        profile.get_can_open() && !profile.get_busy() && profile.get_status().contains("succeeded")
    });
    profile.invoke_open();
    wait(&w, &d, || d.ready());
    w.set_active_view(6);
    w.set_activity_sub_view(3);
    let h = w.global::<NativeHistorical>();
    h.set_mode(1);
    h.invoke_scope_changed();
    h.invoke_search();
    wait(&w, &d, || !h.get_busy() && h.get_rows().row_count() == 3);
    let episode = h
        .get_rows()
        .iter()
        .find(|r| r.summary == "Synthetic agent episode")
        .unwrap();
    assert!(episode.metadata.contains("Pinned"));
    h.invoke_inspect(episode.id.clone());
    wait(&w, &d, || !h.get_busy() && !h.get_details().is_empty());
    assert!(h.get_details().contains("original_extra"));
    snapshot(&w, "native-imported-episodes");
    h.invoke_remove(episode.id.clone());
    wait(&w, &d, || {
        !h.get_busy() && h.get_rows().row_count() == 2 && h.get_can_undo()
    });
    h.invoke_undo();
    wait(&w, &d, || !h.get_busy() && h.get_rows().row_count() == 3);
    h.set_mode(2);
    h.invoke_scope_changed();
    h.invoke_search();
    wait(&w, &d, || !h.get_busy() && h.get_rows().row_count() == 3);
    assert!(h
        .get_rows()
        .iter()
        .any(|r| r.metadata.contains("Unrecognized polarity")));
    snapshot(&w, "native-imported-lessons");
    h.set_query("caution".into());
    h.invoke_scope_changed();
    h.invoke_search();
    wait(&w, &d, || !h.get_busy() && h.get_rows().row_count() == 1);
    assert!(h
        .get_rows()
        .row_data(0)
        .unwrap()
        .metadata
        .contains("9223372036854775807"));
    // Original extension rows are reviewed before creating a disabled active copy.
    w.set_active_view(3);
    w.set_autonomy_sub_view(4);
    x.invoke_refresh();
    wait(&w, &d, || !x.get_busy());
    x.set_import_open(true);
    x.set_original_kind(0);
    x.invoke_browse_original();
    wait(&w, &d, || {
        !x.get_busy() && x.get_original_rows().row_count() > 0
    });
    x.invoke_review_original(0);
    wait(&w, &d, || !x.get_busy() && x.get_import_ready());
    x.invoke_import_reviewed();
    wait(&w, &d, || {
        !x.get_busy() && !x.get_import_ready() && w.get_skills().row_count() == 1
    });
    assert!(!w.get_skills().row_data(0).unwrap().enabled);
    // The selected normalization route runs the reviewed system converter then
    // the synthetic transcriber. This is format/ownership evidence, not ASR.
    let assets = PathBuf::from(std::env::var_os("SMITH_NATIVE_TEST_TRANSCRIPTION").unwrap());
    w.set_active_view(6);
    w.set_activity_sub_view(2);
    let tr = w.global::<NativeTranscription>();
    tr.set_executable(
        assets
            .join("synthetic-transcriber")
            .to_str()
            .unwrap()
            .into(),
    );
    tr.set_model(
        assets
            .join("synthetic-header-only-model.bin")
            .to_str()
            .unwrap()
            .into(),
    );
    tr.invoke_review();
    wait(&w, &d, || tr.get_reviewed() && !tr.get_busy());
    assert!(tr.get_normalization_available());
    tr.set_normalize(true);
    w.set_transcribe_source(assets.join("synthetic.wav").to_str().unwrap().into());
    w.set_transcribe_lang("English".into());
    tr.set_confirmed(true);
    w.invoke_transcribe_start();
    wait(&w, &d, || {
        w.get_transcribe_has_result() && !w.get_transcribe_busy()
    });
    assert_eq!(w.get_transcribe_out_text().as_str(), "Hello & Привет.");
    snapshot(&w, "native-normalized-transcript");
    d.finish(&w).unwrap();
    w.hide().unwrap();
    backend.stop().unwrap();
}
