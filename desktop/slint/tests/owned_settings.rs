#![cfg(unix)]
use serde_json::json;
use smith_native_bridge::{bridge::Bridge, owned_backend::OwnedBackend};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
#[ignore = "requires explicit frozen normal setup backend and owned loopback execution"]
fn reviewed_connection_and_passive_settings_survive_owned_restart_without_discovery() {
    let executable = PathBuf::from(
        std::env::var_os("SMITH_NATIVE_TEST_BACKEND").expect("explicit reviewed setup backend"),
    );
    let root = std::env::temp_dir().join(format!(
        "smith-native-settings-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let temp = Temp(root.clone());
    let backend = OwnedBackend::launch(&executable, &root).unwrap();
    let mut b = Bridge::new(backend.transport());
    b.initialize().unwrap();
    b.refresh_settings().unwrap();
    let initial = b.view.config.clone();
    b.save_connection(
        &initial,
        "openai".into(),
        "http://127.0.0.1:65530/v1".into(),
    )
    .unwrap();
    assert!(!b.view.config.tested);
    assert!(b.view.models.is_empty());
    assert!(b.view.config.model.is_empty());
    // Saving and session-token custody do not contact this deliberately unused
    // loopback destination. No provider test or generation is requested.
    let before_token = b.view.config.clone();
    b.session_token(&before_token, Some("synthetic-native-test-token".into()))
        .unwrap();
    assert!(b.session_token(&before_token, None).is_err());
    b.session_token(&b.view.config.clone(), None).unwrap();
    b.save_connection(&b.view.config.clone(), "mock".into(), "".into())
        .unwrap();
    b.test_connection(&b.view.config.clone()).unwrap();
    b.refresh_settings().unwrap();
    let old = b.settings_review().unwrap();
    let mut intent = b.view.settings.setup["preferences"]["intent"].clone();
    intent["preference"] = json!("quiet");
    intent["overrides"]["threads"] = json!(2);
    b.save_preferences(&old, intent.clone()).unwrap();
    assert!(b.save_preferences(&old, intent.clone()).is_err());
    assert_eq!(b.view.settings.setup["preferences"]["intent"], intent);
    assert!(
        b.view.settings.native["runtime"].is_null(),
        "passive settings must not start runtime"
    );
    drop(b);
    backend.stop().unwrap();
    drop(backend);
    let backend = OwnedBackend::launch(&executable, &root).unwrap();
    let mut b = Bridge::new(backend.transport());
    b.initialize().unwrap();
    b.refresh_settings().unwrap();
    assert_eq!(b.view.config.provider, "mock");
    assert_eq!(b.view.settings.setup["preferences"]["intent"], intent);
    assert!(b.view.settings.native["runtime"].is_null());
    drop(b);
    backend.stop().unwrap();
    drop(backend);
    drop(temp);
}
