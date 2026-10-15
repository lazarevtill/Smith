#![cfg(all(feature = "native-ui", unix))]
//! Original Models callbacks against a freshly owned normal backend. Paths are
//! synthetic selections; no executable/model is launched by this workflow.
use serde_json::Value;
use slint::{ComponentHandle, Model};
use smith_native_bridge::{
    bridge::{Bridge, Result, Transport},
    catalog_bridge::CatalogMutationError,
    native::{register_reviewed_fonts, Driver, MainWindow, NativeCatalog},
    owned_backend::{OwnedBackend, OwnedTransport},
    task_bridge::new_task_request_id,
};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
struct LostReply {
    inner: OwnedTransport,
    lose: Arc<AtomicBool>,
}
impl Transport for LostReply {
    fn request(&mut self, m: &str, p: &str, s: Option<&str>, b: Option<Value>) -> Result<Value> {
        self.inner.request(m, p, s, b)
    }
    fn catalog_mutation(
        &mut self,
        m: &str,
        p: &str,
        s: &str,
        b: Value,
    ) -> std::result::Result<Value, CatalogMutationError> {
        let value = self.inner.catalog_mutation(m, p, s, b)?;
        if self.lose.swap(false, Ordering::AcqRel) {
            return Err(CatalogMutationError::OutcomeUnknown(
                "Synthetic loss after owned backend admission".into(),
            ));
        }
        Ok(value)
    }
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn wait(w: &MainWindow, d: &Driver, p: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(25);
    loop {
        d.pump(w);
        if p() {
            return;
        }
        assert!(
            Instant::now() < end,
            "Catalog did not settle: {} / {}",
            w.get_status_text(),
            w.global::<NativeCatalog>().get_status()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}
#[test]
#[ignore = "Requires coordinated native build and explicit frozen catalog backend"]
fn original_models_save_lost_receipt_select_rename_trash_restore_never_loads() {
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
        "smith-native-catalog-{}",
        new_task_request_id().unwrap()
    )));
    std::fs::create_dir(&root.0).unwrap();
    let profile = root.0.join("profile");
    std::fs::create_dir(&profile).unwrap();
    let selected = root.0.join("selected");
    std::fs::create_dir(&selected).unwrap();
    let executable = selected.join("synthetic-runtime");
    let model = selected.join("synthetic-model.gguf");
    std::fs::write(
        &executable,
        b"Explicit catalog bytes only; never executable inference",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o500)).unwrap();
    std::fs::write(&model, b"GGUFsynthetic selected-byte catalog fixture").unwrap();
    let backend = OwnedBackend::launch(
        &PathBuf::from(
            std::env::var_os("SMITH_NATIVE_TEST_BACKEND").expect("explicit frozen backend"),
        ),
        &profile,
    )
    .unwrap();
    let mut observer = Bridge::new(backend.transport());
    observer.initialize().unwrap();
    let initial = observer.view.config.clone();
    let lose = Arc::new(AtomicBool::new(true));
    let w = MainWindow::new().unwrap();
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    let d = Driver::attach_transport(
        &w,
        LostReply {
            inner: backend.transport(),
            lose,
        },
    );
    w.show().unwrap();
    wait(&w, &d, || !w.get_session_loading());
    w.set_active_view(1);
    wait(&w, &d, || {
        w.global::<NativeCatalog>()
            .get_current()
            .as_str()
            .contains("Demo")
            && !w.global::<NativeCatalog>().get_busy()
    });
    let g = w.global::<NativeCatalog>();
    g.invoke_refresh();
    wait(&w, &d, || !g.get_busy());
    g.set_editor_open(true);
    g.set_label("Synthetic saved model".into());
    g.set_executable(executable.to_str().unwrap().into());
    g.set_model(model.to_str().unwrap().into());
    g.invoke_save_gguf();
    wait(&w, &d, || g.get_can_inspect() && !g.get_busy());
    assert!(g.get_blocked());
    assert!(!g.get_pending_id().is_empty());
    g.invoke_inspect();
    wait(&w, &d, || {
        g.get_rows().row_count() == 1 && !g.get_blocked() && !g.get_busy()
    });
    let id = g.get_rows().row_data(0).unwrap().id;
    g.invoke_select(id.clone());
    wait(&w, &d, || {
        g.get_rows().row_data(0).is_some_and(|r| r.selected) && !g.get_busy()
    });
    // First-use preparation before an explicit inventory observation is a
    // known pre-admission refusal, not a permanently uncertain operation.
    g.invoke_prepare(id.clone());
    wait(&w, &d, || !g.get_busy());
    assert!(!g.get_blocked());
    assert!(!g.get_can_inspect());
    g.invoke_details_for(id.clone());
    wait(&w, &d, || g.get_has_details() && !g.get_busy());
    assert!(g.get_details().contains("external_user_selected"));
    g.set_rename_label("Renamed local selection".into());
    g.invoke_rename();
    wait(&w, &d, || {
        g.get_rows()
            .row_data(0)
            .is_some_and(|r| r.name == "Renamed local selection")
            && !g.get_busy()
    });
    g.invoke_trash_entry(id.clone());
    wait(&w, &d, || g.get_rows().row_count() == 0 && !g.get_busy());
    g.set_trash(true);
    g.invoke_scope_changed();
    wait(&w, &d, || g.get_rows().row_count() == 1 && !g.get_busy());
    g.invoke_restore_entry(id);
    wait(&w, &d, || g.get_rows().row_count() == 0 && !g.get_busy());
    g.set_trash(false);
    g.invoke_scope_changed();
    wait(&w, &d, || g.get_rows().row_count() == 1 && !g.get_busy());
    assert!(!g.get_rows().row_data(0).unwrap().selected);
    observer.refresh_configuration().unwrap();
    assert_eq!(observer.view.config, initial);
    assert_eq!(
        std::fs::read(&model).unwrap(),
        b"GGUFsynthetic selected-byte catalog fixture"
    );
    assert_eq!(
        std::fs::read(&executable).unwrap(),
        b"Explicit catalog bytes only; never executable inference"
    );
    let catalog = observer.catalog_list(false).unwrap();
    assert!(catalog.selected.is_none());
    assert!(catalog.prepared.is_none());
    assert_eq!(catalog.entries[0].label, "Renamed local selection");
    w.hide().unwrap();
    drop(d);
    drop(observer);
    backend.stop().unwrap();
    println!("NATIVE_CATALOG_CALLBACKS_PASS passive save/recovery/select/rename/Trash/restore; no runtime load");
}
