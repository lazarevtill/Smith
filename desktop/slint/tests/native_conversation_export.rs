#![cfg(all(feature = "native-ui", unix))]
//! Actual original callback and owned backend; only the native save dialog is
//! replaced with explicit destinations inside this test's new temporary root.
use serde_json::Value;
use slint::ComponentHandle;
use smith_native_bridge::{
    bridge::{encode_component, Bridge, Transport},
    native::{register_reviewed_fonts, Driver, MainWindow},
    owned_backend::OwnedBackend,
    task_bridge::new_task_request_id,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
enum Choice {
    Cancel,
    File(PathBuf),
    Rename(PathBuf),
    Navigate(PathBuf),
}
fn wait(w: &MainWindow, d: &Driver, condition: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        d.pump(w);
        if condition() {
            return;
        }
        assert!(
            Instant::now() < until,
            "Export callback did not settle: {}",
            w.get_status_text()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}
#[test]
#[ignore = "Requires an explicit immutable normal backend and coordinated original-window build"]
fn original_export_callback_raw_branch_cancel_stale_navigation_and_exclusive_write() {
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
        "smith-native-export-{}",
        new_task_request_id().unwrap()
    )));
    std::fs::create_dir(&root.0).unwrap();
    let profile = root.0.join("profile");
    std::fs::create_dir(&profile).unwrap();
    let backend = OwnedBackend::launch(
        &PathBuf::from(
            std::env::var_os("SMITH_NATIVE_TEST_BACKEND").expect("explicit frozen backend"),
        ),
        &profile,
    )
    .unwrap();
    let w = MainWindow::new().unwrap();
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    let choices = Rc::new(RefCell::new(VecDeque::new()));
    let calls = Rc::new(Cell::new(0));
    let mut other = Bridge::new(backend.transport());
    other.initialize().unwrap();
    let other = RefCell::new(other);
    let selected = Rc::new(RefCell::new(String::new()));
    let d = Driver::attach_transport_with_export_picker(&w, backend.transport(), {
        let choices = choices.clone();
        let calls = calls.clone();
        let selected = selected.clone();
        let weak = w.as_weak();
        move || {
            calls.set(calls.get() + 1);
            let choice = choices
                .borrow_mut()
                .pop_front()
                .expect("Only an explicit queued test destination may be chosen");
            let path = match choice {
                Choice::Cancel => return Ok(None),
                Choice::File(path) => path,
                Choice::Rename(path) => {
                    let mut b = other.borrow_mut();
                    b.open(&selected.borrow())?;
                    let chat = b.view.conversation.as_ref().unwrap().chat.clone();
                    let config = b.view.config.clone();
                    b.rename(&chat, &config, "Changed while save picker was open")?;
                    path
                }
                Choice::Navigate(path) => {
                    weak.upgrade().unwrap().invoke_new_session();
                    path
                }
            };
            Ok(Some(path.to_str().unwrap().into()))
        }
    });
    w.show().unwrap();
    wait(&w, &d, || d.ready());
    w.invoke_new_session();
    wait(&w, &d, || {
        d.view().conversation.is_some() && !w.get_session_loading()
    });
    *selected.borrow_mut() = d.view().conversation.unwrap().chat.id;
    let raw = "<think>\nUser text stays literal\n</think>\n# Raw question\r\n```text\nПривет\n```";
    w.set_composer_text(raw.into());
    w.invoke_send(raw.into());
    wait(&w, &d, || w.get_streaming());
    w.invoke_export_session(w.get_selected_session());
    assert_eq!(
        calls.get(),
        0,
        "Streaming export must not even open the dialog"
    );
    wait(&w, &d, || {
        !w.get_streaming()
            && d.view()
                .conversation
                .as_ref()
                .unwrap()
                .active_messages
                .len()
                == 2
    });
    let abandoned = d.view().conversation.unwrap().active_leaf.unwrap();
    w.invoke_regenerate(abandoned.clone().into());
    wait(&w, &d, || {
        d.view()
            .conversation
            .as_ref()
            .unwrap()
            .active_leaf
            .as_deref()
            != Some(&abandoned)
    });
    wait(&w, &d, || !w.get_streaming());
    let leaf = d.view().conversation.unwrap().active_leaf.unwrap();
    assert_ne!(leaf, abandoned);
    let json_path = root.0.join("branch.json");
    choices
        .borrow_mut()
        .push_back(Choice::File(json_path.clone()));
    w.invoke_export_session(w.get_selected_session());
    wait(&w, &d, || {
        !w.get_export_session_saving() && json_path.exists()
    });
    assert_eq!(w.get_status_text(), "Conversation exported.");
    let saved: Value = serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
    let mut transport = backend.transport();
    let status = transport
        .request("GET", "/api/profile/status", None, None)
        .unwrap();
    let snapshot = transport
        .request(
            "GET",
            &format!("/api/chats/{}", encode_component(&selected.borrow())),
            status["session"].as_str(),
            None,
        )
        .unwrap();
    assert_eq!(saved, snapshot["active_messages"]);
    assert_eq!(saved[0]["content"], raw);
    assert_eq!(saved[1]["id"], leaf);
    assert!(!saved
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == abandoned));
    assert_eq!(snapshot["messages"].as_array().unwrap().len(), 3);
    let md_path = root.0.join("branch.md");
    choices
        .borrow_mut()
        .push_back(Choice::File(md_path.clone()));
    w.invoke_export_session(w.get_selected_session());
    wait(&w, &d, || {
        !w.get_export_session_saving() && md_path.exists()
    });
    let markdown = std::fs::read_to_string(&md_path).unwrap();
    assert!(markdown.contains(raw));
    assert!(markdown.contains(saved[1]["content"].as_str().unwrap()));
    let protected = root.0.join("protected.md");
    std::fs::write(&protected, b"preserve exactly").unwrap();
    choices
        .borrow_mut()
        .push_back(Choice::File(protected.clone()));
    w.invoke_export_session(w.get_selected_session());
    wait(&w, &d, || !w.get_export_session_saving());
    assert_eq!(std::fs::read(&protected).unwrap(), b"preserve exactly");
    assert!(w.get_status_text().contains("not overwritten"));
    choices.borrow_mut().push_back(Choice::Cancel);
    w.invoke_export_session(w.get_selected_session());
    assert!(!w.get_export_session_saving());
    assert!(choices.borrow().is_empty());
    let stale = root.0.join("stale.md");
    choices
        .borrow_mut()
        .push_back(Choice::Rename(stale.clone()));
    w.invoke_export_session(w.get_selected_session());
    wait(&w, &d, || !w.get_export_session_saving());
    assert!(!stale.exists());
    assert!(w.get_status_text().contains("changed"));
    // A fresh view is needed after the refused snapshot; no automatic export retry.
    w.invoke_select_session(w.get_selected_session());
    wait(&w, &d, || {
        !w.get_session_loading()
            && d.view().conversation.as_ref().unwrap().chat.title
                == "Changed while save picker was open"
    });
    let navigated = root.0.join("old-navigation.json");
    choices
        .borrow_mut()
        .push_back(Choice::Navigate(navigated.clone()));
    w.invoke_export_session(w.get_selected_session());
    wait(&w, &d, || {
        d.view().conversation.as_ref().unwrap().chat.id != *selected.borrow()
    });
    assert!(!navigated.exists());
    assert!(!w.get_export_session_saving());
    assert!(choices.borrow().is_empty());
    assert_eq!(calls.get(), 6);
    d.finish(&w).unwrap();
    drop(d);
    w.hide().unwrap();
    drop(w);
    backend.stop().unwrap();
}
