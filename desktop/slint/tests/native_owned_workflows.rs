#![cfg(all(feature = "native-ui", unix))]
#[path = "support/task_model_fixture.rs"]
mod task_model_fixture;
use slint::ComponentHandle;
use smith_native_bridge::{
    bridge::Bridge,
    native::{
        register_reviewed_fonts, Driver, MainWindow, NativeDocuments, NativeMlx, NativeSettings,
        NativeTasks,
    },
    owned_backend::OwnedBackend,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use task_model_fixture::{Step, TaskModelFixture, MODEL};
fn wait(w: &MainWindow, d: &Driver, condition: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(12);
    loop {
        d.pump(w);
        if condition() {
            return;
        }
        assert!(
            Instant::now() < end,
            "Native workflow timed out: {}",
            w.get_status_text()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}
fn snapshot(w: &MainWindow, name: &str) {
    use std::io::Write;
    let p = w.window().take_snapshot().unwrap();
    let path = std::env::temp_dir().join(name);
    let mut f = std::fs::File::create(path).unwrap();
    write!(f, "P6\n{} {}\n255\n", p.width(), p.height()).unwrap();
    for pixel in p.as_slice() {
        f.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
    }
}
#[test]
#[ignore = "explicit frozen normal backend and owned native software window"]
fn original_native_controls_connect_documents_tasks_and_passive_mlx() {
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
    let root = std::env::temp_dir().join(format!(
        "smith-native-controls-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let backend = OwnedBackend::launch(
        &PathBuf::from(std::env::var_os("SMITH_NATIVE_TEST_BACKEND").unwrap()),
        &root,
    )
    .unwrap();
    let w = MainWindow::new().unwrap();
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    let d = Driver::attach_transport(&w, backend.transport());
    w.show().unwrap();
    wait(&w, &d, || d.ready());
    w.set_active_view(2);
    w.set_settings_tab(4);
    wait(&w, &d, || w.global::<NativeSettings>().get_available());
    snapshot(&w, "smith-native-providers.ppm");
    w.set_settings_tab(3);
    snapshot(&w, "smith-native-inference.ppm");
    w.global::<NativeMlx>().invoke_refresh();
    wait(&w, &d, || w.global::<NativeMlx>().get_available());
    assert!(d.view().mlx["runtime"].is_null());
    w.invoke_new_session();
    wait(&w, &d, || d.view().conversation.is_some());
    w.set_active_view(6);
    w.set_activity_sub_view(1);
    let media = w.global::<NativeDocuments>();
    media.set_generator_open(true);
    media.set_name("Native controls.csv".into());
    media.set_headers("Name\tCount".into());
    media.set_rows("owned\t2".into());
    media.invoke_generate();
    wait(&w, &d, || {
        w.get_artifacts_viewer_open() && !w.global::<NativeDocuments>().get_busy()
    });
    assert!(w.get_artifacts_viewer_text().contains("owned"));
    snapshot(&w, "smith-native-artifacts.ppm");
    let id = d.view().documents.preview_id;
    w.invoke_pick_artifact(id.into());
    wait(&w, &d, || w.global::<NativeDocuments>().get_pending());
    w.global::<NativeDocuments>().invoke_attach_context();
    wait(&w, &d, || {
        d.view().draft.media.len() == 1 && !w.global::<NativeDocuments>().get_busy()
    });
    w.set_active_view(3);
    let tasks = w.global::<NativeTasks>();
    tasks.invoke_refresh();
    wait(&w, &d, || !w.global::<NativeTasks>().get_busy());
    tasks.set_label("Native reviewed workspace".into());
    tasks.set_read(true);
    tasks.set_list(true);
    tasks.set_write(true);
    tasks.set_control(true);
    tasks.invoke_review_workspace();
    wait(&w, &d, || {
        !w.global::<NativeTasks>().get_proposal().is_empty()
            && !w.global::<NativeTasks>().get_busy()
    });
    tasks.set_confirmed(true);
    tasks.invoke_provision();
    wait(&w, &d, || {
        w.global::<NativeTasks>().get_selected_workspace() && !w.global::<NativeTasks>().get_busy()
    });
    tasks.set_input("Create one reviewed local note".into());
    tasks.invoke_create();
    wait(&w, &d, || {
        w.global::<NativeTasks>().get_pending() && !w.global::<NativeTasks>().get_busy()
    });
    tasks.invoke_start();
    wait(&w, &d, || {
        w.global::<NativeTasks>().get_running() && !w.global::<NativeTasks>().get_busy()
    });
    tasks.set_path("note.txt".into());
    tasks.set_content("Owned native task note".into());
    tasks.invoke_write_file();
    wait(&w, &d, || {
        w.global::<NativeTasks>().get_approval() && !w.global::<NativeTasks>().get_busy()
    });
    assert!(tasks.get_call().contains("Owned native task note"));
    snapshot(&w, "smith-native-task-approval.ppm");
    tasks.set_approval_confirmed(true);
    tasks.invoke_approve();
    wait(&w, &d, || {
        w.global::<NativeTasks>().get_running() && !w.global::<NativeTasks>().get_busy()
    });
    tasks.invoke_read_file();
    wait(&w, &d, || {
        w.global::<NativeTasks>()
            .get_detail()
            .contains("Owned native task note")
            && !w.global::<NativeTasks>().get_busy()
    });
    tasks.invoke_cancel();
    wait(&w, &d, || {
        w.global::<NativeTasks>().get_terminal() && !w.global::<NativeTasks>().get_busy()
    });
    // Explicitly selected owned local provider; no real model or external API.
    let fixture =
        TaskModelFixture::launch(vec![Step::complete(true), Step::complete(true)]).unwrap();
    w.set_active_view(2);
    w.set_settings_tab(4);
    let settings = w.global::<NativeSettings>();
    settings.invoke_reload();
    wait(&w, &d, || !w.global::<NativeSettings>().get_busy());
    settings.set_provider_index(2);
    settings.set_endpoint(fixture.endpoint.clone().into());
    settings.invoke_save();
    wait(&w, &d, || {
        d.view().config.provider == "openai" && !settings.get_busy()
    });
    assert_eq!(
        fixture.counts(),
        (0, 0),
        "saving must not discover or infer"
    );
    settings.invoke_test();
    wait(&w, &d, || d.view().config.tested && !settings.get_busy());
    settings.invoke_select_model(MODEL.into());
    wait(&w, &d, || {
        d.view().config.model == MODEL && !settings.get_busy()
    });
    w.set_active_view(3);
    tasks.invoke_inspect_model();
    wait(&w, &d, || tasks.get_model_reviewed() && !tasks.get_busy());
    assert_eq!(fixture.counts().1, 0);
    tasks.set_show_setup(true);
    tasks.set_label("Native exact model workspace".into());
    tasks.set_use_model(true);
    tasks.invoke_review_workspace();
    wait(&w, &d, || {
        tasks.get_proposal().contains("ModelInference") && !tasks.get_busy()
    });
    tasks.set_confirmed(true);
    tasks.invoke_provision();
    wait(&w, &d, || {
        tasks
            .get_workspace_detail()
            .contains("Native exact model workspace")
            && !tasks.get_busy()
    });
    tasks.set_input("Finish this synthetic task".into());
    tasks.invoke_create();
    wait(&w, &d, || tasks.get_pending() && !tasks.get_busy());
    tasks.invoke_review_run();
    wait(&w, &d, || {
        !tasks.get_run_review().is_empty() && !tasks.get_busy()
    });
    tasks.set_run_confirmed(true);
    tasks.set_max_steps("2".into());
    d.pump(&w);
    assert!(
        !tasks.get_run_confirmed(),
        "changed step budget clears approval"
    );
    tasks.set_max_steps("1".into());
    tasks.invoke_review_run();
    wait(&w, &d, || !tasks.get_busy());
    tasks.set_run_confirmed(true);
    tasks.invoke_submit_run();
    wait(&w, &d, || tasks.get_model_active() && !tasks.get_busy());
    fixture.wait_entered(1, Duration::from_secs(3));
    // Navigation remains available while a separate bounded poll tracks the run.
    w.set_active_view(6);
    w.set_activity_sub_view(0);
    d.pump(&w);
    assert!(tasks.get_detail().contains("Status: Generating"));
    assert_eq!(w.get_activity_detail().status, "Generating");
    snapshot(&w, "smith-native-task-model-running.ppm");
    fixture.release();
    wait(&w, &d, || {
        tasks.get_operation().contains("Finished") && !tasks.get_model_active()
    });
    assert!(tasks.get_operation().contains("Task worker finished"));
    assert_eq!(fixture.counts().1, 1);
    tasks.set_input("Cancel this held synthetic task".into());
    tasks.invoke_create();
    wait(&w, &d, || tasks.get_pending() && !tasks.get_busy());
    tasks.invoke_review_run();
    wait(&w, &d, || {
        !tasks.get_run_review().is_empty() && !tasks.get_busy()
    });
    tasks.set_run_confirmed(true);
    tasks.invoke_submit_run();
    wait(&w, &d, || tasks.get_model_active() && !tasks.get_busy());
    fixture.wait_entered(2, Duration::from_secs(3));
    tasks.invoke_inspect_operation();
    wait(&w, &d, || {
        tasks.get_detail().contains("Generating") && !tasks.get_busy()
    });
    tasks.invoke_cancel();
    wait(&w, &d, || {
        tasks.get_terminal() && !tasks.get_model_active() && !tasks.get_busy()
    });
    assert!(tasks.get_operation().contains("Cancelled"));
    assert!(
        tasks.get_operation().contains("external server may still"),
        "external cancellation must be truthful: {}",
        tasks.get_operation()
    );
    snapshot(&w, "smith-native-task-model-cancelled.ppm");
    fixture.release();
    std::thread::sleep(Duration::from_millis(150));
    d.pump(&w);
    assert!(tasks.get_terminal());
    assert_eq!(fixture.counts().1, 2, "no model run was replayed");
    // A second owned client changes the credential revision after the visible
    // review. Rejection before POST must not strand the native panel in an
    // ambiguous-submission state or disable ordinary task navigation.
    tasks.set_input("Refuse stale model review before submission".into());
    tasks.invoke_create();
    wait(&w, &d, || tasks.get_pending() && !tasks.get_busy());
    tasks.invoke_review_run();
    wait(&w, &d, || {
        !tasks.get_run_review().is_empty() && !tasks.get_busy()
    });
    tasks.set_run_confirmed(true);
    let mut other = Bridge::new(backend.transport());
    other.initialize().unwrap();
    other
        .session_token(&other.view.config.clone(), Some("synthetic-token".into()))
        .unwrap();
    // Deliberately do not pump the native view between the revision change and
    // callback: the actor must classify its own fresh preflight refusal.
    tasks.invoke_submit_run();
    wait(&w, &d, || {
        tasks.get_run_review().is_empty() && !tasks.get_busy()
    });
    assert!(!tasks.get_model_uncertain());
    assert!(!tasks.get_model_active());
    assert_eq!(
        fixture.counts().1,
        2,
        "preflight refusal made no model call"
    );
    tasks.set_input("Controls recover after a known preflight refusal".into());
    tasks.invoke_create();
    wait(&w, &d, || {
        tasks.get_detail().contains("Controls recover") && !tasks.get_busy()
    });
    let cancelled_id = other
        .task_overview()
        .unwrap()
        .tasks
        .into_iter()
        .find(|task| task.input == "Cancel this held synthetic task")
        .unwrap()
        .id;
    w.invoke_activity_select_task(cancelled_id.into());
    wait(&w, &d, || {
        tasks
            .get_detail()
            .contains("Cancel this held synthetic task")
            && !tasks.get_busy()
    });
    assert!(!tasks.get_model_active());
    assert_eq!(fixture.counts().1, 2);
    fixture.assert_healthy();
    drop(fixture);
    d.finish(&w).unwrap();
    drop(d);
    backend.stop().unwrap();
    drop(backend);
    w.hide().unwrap();
    drop(w);
    std::fs::remove_dir_all(root).unwrap();
}
