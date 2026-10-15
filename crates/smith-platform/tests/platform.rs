use smith_platform::{fixture::*, *};
use std::{sync::Arc, time::Duration};

fn fixture() -> (PlatformSession, FixtureControl, Arc<FixtureClock>) {
    let (backend, control) = FixtureBackend::new();
    let clock = Arc::new(FixtureClock::default());
    let mut session = PlatformSession::with_clock(
        "owner",
        Box::new(backend),
        clock.clone(),
        Duration::from_secs(15),
    )
    .unwrap();
    session
        .select_target(
            TargetRequest::SyntheticFixture,
            TargetPermissions {
                read_elements: true,
                interact: true,
            },
        )
        .unwrap();
    (session, control, clock)
}
#[test]
fn read_consent_and_interaction_consent_are_independent() {
    let (backend, control) = FixtureBackend::new();
    let mut s = PlatformSession::new("owner", Box::new(backend)).unwrap();
    assert!(matches!(s.screen_elements(), Err(Error::Denied(_))));
    assert!(matches!(
        s.select_target(
            TargetRequest::SyntheticFixture,
            TargetPermissions {
                read_elements: false,
                interact: true
            }
        ),
        Err(Error::Denied(_))
    ));
    assert_eq!(control.frame_reads(), 0);
    s.select_target(
        TargetRequest::SyntheticFixture,
        TargetPermissions {
            read_elements: true,
            interact: false,
        },
    )
    .unwrap();
    let snapshot = s.screen_elements().unwrap();
    assert!(matches!(
        s.prepare(&snapshot.id, Action::Click { x: 30, y: 30 }),
        Err(Error::Denied(_))
    ));
    assert!(control.actions().is_empty());
}
#[test]
fn exact_click_and_text_have_synthetic_receipts_and_invalidate_snapshot() {
    let (mut s, control, _) = fixture();
    for action in [
        Action::Click { x: 30, y: 30 },
        Action::TypeText {
            text: "fixture 🧪".into(),
        },
    ] {
        let snapshot = s.screen_elements().unwrap();
        assert!(snapshot.elements.contains("Fixture Save"));
        let plan = s.prepare(&snapshot.id, action.clone()).unwrap();
        let approval = s.approve(&plan.id).unwrap();
        let receipt = s.execute(approval, &mut || Ok(())).unwrap();
        assert!(receipt.ok);
        assert_eq!(receipt.execution_kind, ExecutionKind::SyntheticFixture);
        assert_eq!(control.actions().last().unwrap().action, action);
        assert!(matches!(
            s.prepare(&snapshot.id, action),
            Err(Error::SnapshotExpired)
        ));
    }
    assert_eq!(control.actions().len(), 2);
}
#[test]
fn out_of_window_and_non_actionable_coordinates_fail() {
    let (mut s, control, _) = fixture();
    let snapshot = s.screen_elements().unwrap();
    assert!(matches!(
        s.prepare(&snapshot.id, Action::Click { x: -1, y: 30 }),
        Err(Error::Denied(_))
    ));
    assert!(matches!(
        s.prepare(&snapshot.id, Action::Click { x: 800, y: 30 }),
        Err(Error::Denied(_))
    ));
    assert!(matches!(
        s.prepare(&snapshot.id, Action::Click { x: 400, y: 400 }),
        Err(Error::Unsupported(_))
    ));
    assert!(control.actions().is_empty());
}
#[test]
fn exact_expiry_boundary_and_wrong_snapshot_fail() {
    let (mut s, control, clock) = fixture();
    let snapshot = s.screen_elements().unwrap();
    assert!(matches!(
        s.prepare("not-this-snapshot", Action::Click { x: 30, y: 30 }),
        Err(Error::SnapshotExpired)
    ));
    let plan = s
        .prepare(&snapshot.id, Action::Click { x: 30, y: 30 })
        .unwrap();
    let approval = s.approve(&plan.id).unwrap();
    clock.advance(15000);
    assert!(matches!(
        s.execute(approval, &mut || Ok(())),
        Err(Error::SnapshotExpired)
    ));
    assert!(control.actions().is_empty());
}
#[test]
fn edited_plan_and_duplicate_approval_cannot_change_or_replay_action() {
    let (mut s, control, _) = fixture();
    let snapshot = s.screen_elements().unwrap();
    let plan = s
        .prepare(&snapshot.id, Action::Click { x: 30, y: 30 })
        .unwrap();
    let first = s.approve(&plan.id).unwrap();
    let duplicate = s.approve(&plan.id).unwrap();
    assert!(matches!(
        s.validate_plan_action(&plan.id, &Action::Click { x: 31, y: 30 }),
        Err(Error::ApprovalMismatch)
    ));
    s.execute(first, &mut || Ok(())).unwrap();
    assert!(matches!(
        s.execute(duplicate, &mut || Ok(())),
        Err(Error::ApprovalConsumed)
    ));
    assert_eq!(control.actions().len(), 1);
}
#[test]
fn reselect_revoke_and_new_session_invalidate_approval_and_binding() {
    let (mut s, control, _) = fixture();
    let snapshot = s.screen_elements().unwrap();
    let plan = s
        .prepare(&snapshot.id, Action::Click { x: 30, y: 30 })
        .unwrap();
    let approval = s.approve(&plan.id).unwrap();
    let binding = s.binding().to_owned();
    s.revoke();
    assert_ne!(binding, s.binding());
    assert!(matches!(
        s.execute(approval, &mut || Ok(())),
        Err(Error::ApprovalMismatch)
    ));
    assert!(control.actions().is_empty());
    let (mut other, _, _) = fixture();
    let snapshot = other.screen_elements().unwrap();
    let plan = other
        .prepare(&snapshot.id, Action::Click { x: 30, y: 30 })
        .unwrap();
    let foreign = other.approve(&plan.id).unwrap();
    assert!(matches!(
        s.execute(foreign, &mut || Ok(())),
        Err(Error::ApprovalMismatch)
    ));
}
#[test]
fn changed_tree_target_or_selection_rejects_dispatch() {
    for mode in 0..3 {
        let (mut s, control, _) = fixture();
        let snapshot = s.screen_elements().unwrap();
        let plan = s
            .prepare(
                &snapshot.id,
                Action::TypeText {
                    text: "fixture".into(),
                },
            )
            .unwrap();
        let approval = s.approve(&plan.id).unwrap();
        control.mutate_frame(|f| match mode {
            0 => f.elements[1].name = Some("Changed field".into()),
            1 => f.target.id = "different-target".into(),
            _ => {
                f.elements[1].text_selection = Some(TextSelection {
                    location: 2,
                    length: 0,
                })
            }
        });
        assert!(matches!(
            s.execute(approval, &mut || Ok(())),
            Err(Error::SnapshotChanged | Error::TargetChanged)
        ));
        assert!(control.actions().is_empty());
    }
}
#[test]
fn malformed_tree_and_text_are_bounded() {
    let (mut s, control, _) = fixture();
    let snapshot = s.screen_elements().unwrap();
    for text in [String::new(), "\0".into(), "a".repeat(16385)] {
        assert!(matches!(
            s.prepare(&snapshot.id, Action::TypeText { text }),
            Err(Error::InvalidInput(_))
        ));
    }
    control.mutate_frame(|f| f.elements.push(f.elements[0].clone()));
    assert!(matches!(s.screen_elements(), Err(Error::Backend(_))));
    assert!(control.actions().is_empty());
}
#[test]
fn fixture_system_contract_and_shutdown_are_explicit() {
    let (mut s, control, _) = fixture();
    let value = serde_json::to_value(s.system_monitor().unwrap()).unwrap();
    assert_eq!(value["cpu_percent"], 12.5);
    assert_eq!(value["execution_kind"], "synthetic_fixture");
    assert!(value["memory_total_bytes"].as_u64().unwrap() > 0);
    assert!(value["disks"][0]["total_bytes"].as_u64().unwrap() > 0);
    s.shutdown();
    assert!(matches!(s.screen_elements(), Err(Error::ShuttingDown)));
    assert!(matches!(s.system_monitor(), Err(Error::ShuttingDown)));
    assert!(control.actions().is_empty());
}
#[test]
fn bounds_arithmetic_does_not_wrap() {
    let bounds = Bounds {
        x: i32::MAX - 10,
        y: i32::MIN,
        width: 100,
        height: 20,
    };
    assert!(bounds.contains(i32::MAX, i32::MIN));
    assert!(!bounds.contains(i32::MIN, i32::MIN));
    assert_eq!(bounds.center().0, i32::MAX);
}
