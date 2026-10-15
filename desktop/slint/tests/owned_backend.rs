#![cfg(unix)]
use smith_native_bridge::{
    bridge::{Bridge, Transport},
    owned_backend::OwnedBackend,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
#[ignore = "requires explicit normal backend path and owned loopback execution"]
fn normal_owned_backend_opens_directly_chats_streams_reopens_and_reaps() {
    let executable = PathBuf::from(
        std::env::var_os("SMITH_NATIVE_TEST_BACKEND").expect("explicit reviewed backend required"),
    );
    let path = std::env::temp_dir().join(format!(
        "smith-native-owned-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&path).unwrap();
    let temporary = Temp(path.clone());
    let backend = OwnedBackend::launch(&executable, &path).unwrap();
    let address = backend.reserved_address();
    let mut intruder = TcpStream::connect(address).unwrap();
    intruder
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    intruder
        .write_all(
            format!(
                "GET /api/desktop/ready HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut reply = String::new();
    intruder.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 403"));
    let mut bridge = Bridge::new(backend.transport());
    bridge.initialize().unwrap();
    assert!(bridge.view.available);
    assert!(!bridge.view.synthetic);
    assert_eq!(bridge.view.config.provider, "mock");
    bridge.new_chat().unwrap();
    let chat = bridge.view.conversation.as_ref().unwrap().chat.id.clone();
    bridge
        .send(
            &bridge.intent().unwrap(),
            "Synthetic native transport qualification",
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while bridge.streaming() {
        bridge.poll().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(25));
    }
    let answer = bridge
        .view
        .conversation
        .as_ref()
        .unwrap()
        .active_messages
        .last()
        .unwrap();
    assert_eq!(answer.status, "completed");
    assert!(!answer.content.is_empty());
    bridge
        .change_draft(
            bridge.view.draft.identity,
            "Durable native draft through edit and Trash".into(),
        )
        .unwrap();
    bridge.save_draft().unwrap();
    let old_user = bridge.view.conversation.as_ref().unwrap().active_messages[0]
        .id
        .clone();
    bridge
        .edit(
            &bridge.intent().unwrap(),
            &old_user,
            "Edited from the native bridge",
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while bridge.streaming() {
        bridge.poll().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(bridge.view.conversation.as_ref().unwrap().messages.len(), 4);
    assert!(bridge
        .view
        .conversation
        .as_ref()
        .unwrap()
        .messages
        .iter()
        .any(|m| m.id == old_user));
    let selected = bridge.view.conversation.as_ref().unwrap().chat.clone();
    let previous_title = selected.title.clone();
    bridge
        .rename(
            &selected,
            &bridge.view.config.clone(),
            "Native reversible title",
        )
        .unwrap();
    bridge.reverse(&bridge.view.actions.clone(), false).unwrap();
    assert_eq!(
        bridge.view.conversation.as_ref().unwrap().chat.title,
        previous_title
    );
    bridge.reverse(&bridge.view.actions.clone(), true).unwrap();
    assert_eq!(
        bridge.view.conversation.as_ref().unwrap().chat.title,
        "Native reversible title"
    );
    let selected = bridge.view.conversation.as_ref().unwrap().chat.clone();
    bridge
        .trash(&[selected], &bridge.view.actions.clone(), false)
        .unwrap();
    assert!(bridge.view.conversation.is_none());
    assert!(bridge.view.chats.iter().all(|c| c.id != chat));
    bridge
        .trash(
            &bridge.view.trash.clone(),
            &bridge.view.actions.clone(),
            true,
        )
        .unwrap();
    bridge.open(&chat).unwrap();
    assert_eq!(
        bridge.view.draft.text,
        "Durable native draft through edit and Trash"
    );
    let before = bridge.view.conversation.clone();
    let mut stale = backend.transport();
    backend.stop().unwrap();
    assert!(stale
        .request("GET", "/api/profile/status", None, None)
        .is_err());
    assert!(
        TcpListener::bind(address).is_err(),
        "parent listener stays reserved through child exit"
    );
    assert!(bridge.poll().is_err());
    assert!(!bridge.view.available);
    drop(bridge);
    drop(stale);
    drop(backend);
    let restarted = OwnedBackend::launch(&executable, &path).unwrap();
    let mut reopened = Bridge::new(restarted.transport());
    reopened.initialize().unwrap();
    reopened.open(&chat).unwrap();
    assert_eq!(before, reopened.view.conversation);
    assert_eq!(
        reopened.view.draft.text,
        "Durable native draft through edit and Trash"
    );
    drop(reopened);
    restarted.stop().unwrap();
    drop(restarted);
    drop(temporary);
}
