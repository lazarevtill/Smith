#[path = "support/settings_fixture.rs"]
mod support;
use serde_json::json;
use smith_native_bridge::{bridge::Bridge, settings::SelectedFile};
#[test]
fn explicit_connection_review_never_discovers_or_replays_implicitly() {
    let fixture = support::SettingsFixture::new();
    let control = fixture.control.clone();
    let mut b = Bridge::new(fixture);
    b.initialize().unwrap();
    assert_eq!(control.0.lock().unwrap().discoveries, 0);
    let old = b.view.config.clone();
    b.save_connection(&old, "openai".into(), "http://127.0.0.1:18777/v1".into())
        .unwrap();
    assert!(!b.view.config.tested);
    assert!(b.view.models.is_empty());
    assert_eq!(control.0.lock().unwrap().discoveries, 0);
    assert!(b.test_connection(&old).is_err());
    b.test_connection(&b.view.config.clone()).unwrap();
    assert_eq!(control.0.lock().unwrap().discoveries, 1);
    assert!(
        b.view.config.model.is_empty(),
        "model must not be silently selected"
    );
    b.select_model(&b.view.config.clone(), "fixture-model")
        .unwrap();
    let stale = b.view.config.clone();
    control.0.lock().unwrap().config["revision"] = json!("settings-99");
    let count = control.0.lock().unwrap().calls.len();
    assert!(b.save_connection(&stale, "mock".into(), "".into()).is_err());
    assert_eq!(control.0.lock().unwrap().calls.len(), count);
}
#[test]
fn selected_files_probe_and_load_keep_exact_revisions_and_one_use_plan() {
    let fixture = support::SettingsFixture::new();
    let control = fixture.control.clone();
    let mut b = Bridge::new(fixture);
    b.initialize().unwrap();
    b.refresh_settings().unwrap();
    let file = SelectedFile {
        path: "/synthetic/file".into(),
        source: "synthetic fixture".into(),
        source_revision: "fixture-v1".into(),
    };
    b.inspect_files(
        &b.settings_review().unwrap(),
        file.clone(),
        file,
        "synthetic-model".into(),
    )
    .unwrap();
    let old = b.settings_review().unwrap();
    control.0.lock().unwrap().setup["selection"]["id"] = json!("selection-2");
    b.refresh_settings().unwrap();
    assert!(b.prepare_plan(&old).is_err());
    b.prepare_plan(&b.settings_review().unwrap()).unwrap();
    let review = b.settings_review().unwrap();
    b.apply_plan(&review).unwrap();
    assert_eq!(control.0.lock().unwrap().loads, 1);
    assert!(b.apply_plan(&review).is_err());
    assert_eq!(control.0.lock().unwrap().loads, 1);
    let runtime = b.view.settings.native["runtime"].clone();
    b.unload(&b.view.config.clone(), &runtime).unwrap();
    assert!(b.view.settings.native["runtime"].is_null());
    let calls = &control.0.lock().unwrap().calls;
    let body = &calls
        .iter()
        .find(|c| c.0 == "/api/acceleration/plan")
        .unwrap()
        .1;
    assert_eq!(body["confirm_probe"], true);
    assert_eq!(body["probe_id"], "probe-1");
    assert!(body.get("arguments").is_none());
}

#[test]
fn catalog_apply_uses_actual_prepared_binding_and_rejects_recreated_plan() {
    let fixture = support::SettingsFixture::new();
    let control = fixture.control.clone();
    let mut b = Bridge::new(fixture);
    b.initialize().unwrap();
    control.0.lock().unwrap().setup["prepared"] =
        json!({"plan":{"id":"same-deterministic-plan"},"catalog":{"id":"a".repeat(64)}});
    b.refresh_settings().unwrap();
    let old = b.settings_review().unwrap();
    control.0.lock().unwrap().setup["prepared"]["catalog"]["id"] = json!("b".repeat(64));
    b.refresh_settings().unwrap();
    assert!(b.apply_plan(&old).is_err());
    assert_eq!(control.0.lock().unwrap().loads, 0);
    b.apply_plan(&b.settings_review().unwrap()).unwrap();
    let state = control.0.lock().unwrap();
    let body = &state
        .calls
        .iter()
        .find(|c| c.0 == "/api/acceleration/apply")
        .unwrap()
        .1;
    assert_eq!(body["catalog_review_id"], "b".repeat(64));
}
