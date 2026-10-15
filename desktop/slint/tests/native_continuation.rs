#![cfg(all(feature = "native-ui", unix))]
use slint::{ComponentHandle, Model};
use smith_native_bridge::{
    bridge::{Result, Transport},
    native::{
        register_reviewed_fonts, Driver, MainWindow, NativeAgents, NativeBatch, NativeMemory,
        NativeProfile, NativeTasks, NativeTranscription,
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
// Commit the first selected transcript save, then lose only its acknowledgement.
// This exercises the actual actor's GET-only recovery without repeating the effect.
struct LostSaveReply {
    inner: OwnedTransport,
    saves: Arc<AtomicUsize>,
}
impl Transport for LostSaveReply {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let response = self.inner.request(method, path, session, body)?;
        if method == "POST"
            && path.starts_with("/api/media/transcription/jobs/")
            && path.ends_with("/save")
            && self.saves.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(
                "Synthetic lost save acknowledgement; inspect the exact submitted job.".into(),
            );
        }
        Ok(response)
    }
    fn transcription_start(
        &mut self,
        session: &str,
        body: serde_json::Value,
    ) -> std::result::Result<
        serde_json::Value,
        smith_native_bridge::transcription::TranscriptionSubmitError,
    > {
        self.inner.transcription_start(session, body)
    }
    fn sensitive_request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: smith_native_bridge::profile_bridge::SensitiveBody,
    ) -> Result<serde_json::Value> {
        self.inner.sensitive_request(method, path, session, body)
    }
    fn upload(
        &mut self,
        session: &str,
        metadata: serde_json::Value,
        bytes: &[u8],
    ) -> Result<serde_json::Value> {
        self.inner.upload(session, metadata, bytes)
    }
    fn download(&mut self, path: &str, session: &str) -> Result<Vec<u8>> {
        self.inner.download(path, session)
    }
    fn event_snapshot(&mut self, path: &str, session: &str) -> Result<Option<serde_json::Value>> {
        self.inner.event_snapshot(path, session)
    }
}
fn wait(w: &MainWindow, d: &Driver, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        d.pump(w);
        if condition() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Native continuation timed out: {}",
            w.get_status_text()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}
fn snapshot(w: &MainWindow, name: &str) {
    let Some(root) = std::env::var_os("SMITH_NATIVE_TEST_CAPTURES") else {
        return;
    };
    let p = w.window().take_snapshot().unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let mut f = std::fs::File::create(Path::new(&root).join(format!("{name}.ppm"))).unwrap();
    write!(f, "P6\n{} {}\n255\n", p.width(), p.height()).unwrap();
    for pixel in p.as_slice() {
        f.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
    }
}
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
            for byte in self.0.as_mut_vec() {
                std::ptr::write_volatile(byte, 0);
            }
        }
    }
}
#[test]
#[ignore = "Explicit frozen backend, synthetic original-copy fixture, owned native renderer only"]
fn saved_agents_feedback_batch_and_original_copy_import_use_native_controls() {
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
        "smith-native-continuation-{}",
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
    let saves = Arc::new(AtomicUsize::new(0));
    let d = Driver::attach_transport_with_profile(
        &w,
        LostSaveReply {
            inner: backend.transport(),
            saves: saves.clone(),
        },
        &host,
    )
    .unwrap();
    w.show().unwrap();
    wait(&w, &d, || d.ready());
    w.invoke_new_session();
    wait(&w, &d, || d.view().conversation.is_some());
    let first = d.view().conversation.unwrap().chat.id;
    w.set_composer_text("### Native heading\n\n```rust\nlet reviewed = true;\n```".into());
    w.invoke_send(w.get_composer_text());
    wait(&w, &d, || {
        d.view().conversation.as_ref().is_some_and(|c| {
            c.active_messages.len() == 2
                && !c.active_messages.iter().any(|m| m.status == "streaming")
        })
    });
    assert!(w
        .get_messages()
        .iter()
        .any(|m| m.segments.iter().any(|s| s.kind == "code")));
    let answer = d.view().conversation.unwrap().active_messages[1].id.clone();
    w.invoke_message_feedback(answer.clone().into(), "thumbs_up".into());
    wait(&w, &d, || {
        d.view()
            .conversation
            .as_ref()
            .is_some_and(|c| c.active_messages[1].feedback == "thumbs_up")
    });
    w.invoke_message_feedback(answer.into(), "thumbs_up".into());
    wait(&w, &d, || {
        d.view().conversation.as_ref().is_some_and(|c| {
            c.active_messages[1].feedback.is_empty() && c.active_messages[1].feedback_revision == 2
        })
    });
    snapshot(&w, "native-rich-answer");
    w.invoke_new_session();
    wait(&w, &d, || {
        d.view()
            .conversation
            .as_ref()
            .is_some_and(|c| c.chat.id != first)
    });
    let batch = w.global::<NativeBatch>();
    batch.invoke_toggle_mode();
    batch.invoke_select_visible();
    d.pump(&w);
    assert_eq!(batch.get_count(), 2);
    batch.invoke_apply();
    wait(&w, &d, || {
        d.view().chats.is_empty() && d.view().trash.len() == 2 && !batch.get_busy()
    });
    w.invoke_native_toggle_trash();
    batch.invoke_select_visible();
    d.pump(&w);
    assert_eq!(batch.get_count(), 2);
    batch.invoke_apply();
    wait(&w, &d, || {
        d.view().chats.len() == 2 && d.view().trash.is_empty()
    });
    w.invoke_native_toggle_trash();
    batch.invoke_toggle_mode();
    w.set_active_view(3);
    let agents = w.global::<NativeAgents>();
    agents.set_tasks_mode(true);
    let tasks = w.global::<NativeTasks>();
    tasks.invoke_refresh();
    wait(&w, &d, || !tasks.get_busy());
    tasks.set_label("Native persona workspace".into());
    tasks.set_read(true);
    tasks.set_write(true);
    tasks.set_control(true);
    tasks.invoke_review_workspace();
    wait(&w, &d, || {
        tasks.get_proposal().contains("Native persona workspace") && !tasks.get_busy()
    });
    tasks.set_confirmed(true);
    tasks.invoke_provision();
    wait(&w, &d, || {
        tasks.get_workspaces().row_count() == 1 && !tasks.get_busy()
    });
    agents.set_tasks_mode(false);
    agents.invoke_refresh();
    wait(&w, &d, || {
        agents.get_workspaces().row_count() == 1 && !agents.get_busy()
    });
    w.invoke_new_agent();
    wait(&w, &d, || !agents.get_busy());
    agents.invoke_select_workspace(0);
    let caps = agents.get_capabilities();
    for (i, item) in caps.iter().enumerate() {
        if item.label == "read" || item.label == "control" {
            agents.invoke_toggle_capability(i as i32);
        }
    }
    w.set_agent_name("Saved native reader".into());
    w.set_agent_prompt("Read the selected workspace only.".into());
    w.set_agent_soul("Use concise wording.".into());
    w.invoke_save_agent();
    wait(&w, &d, || {
        !w.get_editing_agent().is_empty() && !agents.get_busy()
    });
    let saved = w.get_editing_agent();
    w.invoke_save_agent();
    wait(&w, &d, || !agents.get_busy());
    assert_eq!(w.get_editing_agent(), saved);
    w.set_agent_name("Renamed native reader".into());
    w.invoke_save_agent();
    wait(&w, &d, || {
        w.get_agents()
            .iter()
            .any(|r| r.name == "Renamed native reader")
            && !agents.get_busy()
    });
    snapshot(&w, "native-saved-agent");
    w.set_agent_run_input("Read only this captured task scope".into());
    w.invoke_run_agent_foreground();
    wait(&w, &d, || {
        w.get_active_view() == 6 && tasks.get_selected_task() && !agents.get_busy()
    });
    assert!(tasks.get_pending());
    assert!(tasks.get_can_read());
    assert!(!tasks.get_can_write());
    assert!(!tasks.get_can_model());
    snapshot(&w, "native-agent-pending-task");
    w.set_active_view(3);
    w.invoke_delete_agent(saved.clone());
    wait(&w, &d, || {
        w.get_agents().row_count() == 0 && !agents.get_busy()
    });
    agents.set_show_deleted(true);
    agents.invoke_refresh();
    wait(&w, &d, || {
        w.get_agents().row_count() == 1 && !agents.get_busy()
    });
    w.invoke_select_agent(saved.clone());
    wait(&w, &d, || agents.get_deleted() && !agents.get_busy());
    agents.invoke_restore();
    wait(&w, &d, || !agents.get_deleted() && !agents.get_busy());
    agents.set_show_deleted(false);
    d.pump(&w);
    assert!(w.get_agents().iter().any(|r| r.id == saved));
    w.set_active_view(6);
    w.set_activity_sub_view(3);
    let memory = w.global::<NativeMemory>();
    memory.set_namespace("native-curation".into());
    w.invoke_browse_memory();
    wait(&w, &d, || memory.get_can_create() && !memory.get_busy());
    memory.invoke_new_memory();
    memory.set_fact("The reviewed local example uses a blue notebook.".into());
    memory.set_tags("native\ncurated, example".into());
    memory.invoke_save();
    wait(&w, &d, || {
        w.get_memory_rows().row_count() == 1 && !memory.get_busy()
    });
    let memory_id = w.get_memory_rows().row_data(0).unwrap().id;
    w.invoke_toggle_pin_memory(memory_id.clone());
    wait(&w, &d, || {
        w.get_memory_rows().row_data(0).is_some_and(|r| r.pinned) && !memory.get_busy()
    });
    memory.invoke_edit(memory_id.clone());
    assert_eq!(memory.get_tags().as_str(), "native\ncurated, example");
    memory.set_fact("The reviewed local example uses a green notebook.".into());
    memory.invoke_save();
    wait(&w, &d, || {
        w.get_memory_rows()
            .row_data(0)
            .is_some_and(|r| r.summary.contains("green"))
            && !memory.get_busy()
    });
    snapshot(&w, "native-memory-curation");
    w.invoke_delete_memory(memory_id.clone());
    wait(&w, &d, || {
        w.get_memory_rows().row_count() == 0 && memory.get_can_undo() && !memory.get_busy()
    });
    memory.invoke_undo_trash();
    wait(&w, &d, || {
        w.get_memory_rows().row_count() == 1 && !memory.get_busy()
    });
    w.invoke_delete_memory(memory_id.clone());
    wait(&w, &d, || {
        w.get_memory_rows().row_count() == 0 && !memory.get_busy()
    });
    memory.set_deleted(true);
    w.invoke_browse_memory();
    wait(&w, &d, || {
        w.get_memory_rows().row_count() == 1 && !memory.get_busy()
    });
    w.invoke_delete_memory(memory_id);
    wait(&w, &d, || {
        w.get_memory_rows().row_count() == 0 && !memory.get_busy()
    });
    memory.set_deleted(false);
    w.set_memory_query("GREEN".into());
    w.invoke_search_memory();
    wait(&w, &d, || {
        w.get_memory_rows().row_count() == 1 && !memory.get_busy()
    });
    memory.set_namespace("another-scope".into());
    d.pump(&w);
    assert_eq!(w.get_memory_rows().row_count(), 0);
    assert!(!memory.get_can_create());
    // This owned C fixture emits predetermined text; it is not an ASR model.
    let transcript_fixture =
        PathBuf::from(std::env::var_os("SMITH_NATIVE_TEST_TRANSCRIPTION").unwrap())
            .canonicalize()
            .unwrap();
    w.set_activity_sub_view(2);
    let transcribe = w.global::<NativeTranscription>();
    transcribe.set_executable(
        transcript_fixture
            .join("synthetic-transcriber")
            .to_str()
            .unwrap()
            .into(),
    );
    transcribe.set_model(
        transcript_fixture
            .join("synthetic-header-only-model.bin")
            .to_str()
            .unwrap()
            .into(),
    );
    transcribe.invoke_review();
    wait(&w, &d, || {
        transcribe.get_reviewed() && !transcribe.get_busy()
    });
    transcribe.set_confirmed(true);
    transcribe.set_setup(false);
    w.set_transcribe_source(
        transcript_fixture
            .join("synthetic.wav")
            .to_str()
            .unwrap()
            .into(),
    );
    w.set_transcribe_lang("English".into());
    w.set_transcribe_format("text".into());
    w.invoke_transcribe_start();
    wait(&w, &d, || {
        w.get_transcribe_has_result() && !w.get_transcribe_busy()
    });
    assert_eq!(w.get_transcribe_out_text().as_str(), "Hello & Привет.");
    w.invoke_transcribe_save_artifact();
    wait(&w, &d, || {
        transcribe.get_save_uncertain() && !transcribe.get_busy()
    });
    assert!(!transcribe.get_can_start());
    assert_eq!(saves.load(Ordering::SeqCst), 1);
    transcribe.invoke_inspect();
    wait(&w, &d, || {
        w.get_transcribe_artifacts().row_count() == 1
            && !transcribe.get_busy()
            && !transcribe.get_save_uncertain()
    });
    assert_eq!(
        saves.load(Ordering::SeqCst),
        1,
        "Inspect must not repeat the save POST"
    );
    let transcript = w.get_transcribe_artifacts().row_data(0).unwrap().id;
    w.invoke_transcribe_load_artifact(transcript);
    wait(&w, &d, || {
        w.get_transcribe_has_result() && !transcribe.get_busy()
    });
    assert_eq!(w.get_transcribe_out_text().as_str(), "Hello & Привет.");
    snapshot(&w, "native-selected-file-transcription");
    w.set_transcribe_source(
        transcript_fixture
            .join("synthetic-hold.wav")
            .to_str()
            .unwrap()
            .into(),
    );
    w.invoke_transcribe_start();
    wait(&w, &d, || {
        transcribe.get_running() && !transcribe.get_busy()
    });
    transcribe.invoke_cancel();
    wait(&w, &d, || {
        w.get_transcribe_status().starts_with("cancelled") && !w.get_transcribe_busy()
    });
    // Key is generated and held only in test memory; the fixture receives it on stdin.
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    let key = Key(bytes.iter().map(|v| format!("{v:02x}")).collect());
    let source = root.0.join("stopped-copy");
    let mut child = Command::new(std::env::var_os("SMITH_NATIVE_TEST_LEGACY_FIXTURE").unwrap())
        .arg(&source)
        .arg("25")
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
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            assert!(exit.success());
            break;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Fixture timeout");
        }
        std::thread::sleep(Duration::from_millis(25));
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
    assert!(!d.ready());
    assert!(profile.get_report().contains("Verified schema 25"));
    snapshot(&w, "native-import-reviewed-copy");
    profile.set_master(key.0.clone().into());
    profile.set_confirmed(true);
    profile.invoke_import();
    assert!(profile.get_master().is_empty());
    wait(&w, &d, || {
        profile.get_can_open()
            && !profile.get_busy()
            && profile.get_status().contains("Copy import · succeeded")
    });
    profile.invoke_open();
    wait(&w, &d, || {
        d.ready() && d.view().chats.iter().any(|c| c.id == "session")
    });
    assert!(d.view().chats.iter().all(|c| c.id != first));
    w.set_active_view(0);
    let index = w
        .get_sessions()
        .iter()
        .position(|v| v.id == "session")
        .unwrap();
    w.invoke_select_session(index as i32);
    wait(&w, &d, || {
        d.view()
            .conversation
            .as_ref()
            .is_some_and(|c| c.chat.id == "session")
    });
    snapshot(&w, "native-import-opened-chat");
    let original = "11223344-5566-7788-99aa-bbccddeeff00";
    w.set_active_view(6);
    w.set_activity_sub_view(1);
    w.invoke_artifacts_open(original.into());
    wait(&w, &d, || d.view().documents.preview_id == original);
    assert_eq!(
        d.view().documents.preview_path,
        format!("/api/legacy-import/artifacts/{original}/bytes")
    );
    assert!(d
        .view()
        .documents
        .preview_text
        .contains("authenticated original"));
    snapshot(&w, "native-import-original-artifact");
    let mut exporter = smith_native_bridge::bridge::Bridge::new(backend.transport());
    exporter.initialize().unwrap();
    exporter.open_artifact(original).unwrap();
    exporter
        .download_document(
            &exporter.download_review().unwrap(),
            &root.0.join("original-export.txt"),
        )
        .unwrap();
    assert_eq!(
        std::fs::read(root.0.join("original-export.txt")).unwrap(),
        b"Synthetic original managed artifact. Exact retained bytes.\n"
    );
    d.finish(&w).unwrap();
    drop(d);
    backend.stop().unwrap();
    w.hide().unwrap();
}
