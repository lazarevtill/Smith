//! Independent synthetic review: no native backend is constructed or queried.
use smith_platform::{fixture::*, *};
use std::{sync::Arc, time::Duration};

struct SlowRefresh {
    inner: FixtureBackend,
    clock: Arc<FixtureClock>,
    reads: usize,
}
impl PlatformBackend for SlowRefresh {
    fn capabilities(&self) -> PlatformCapabilities {
        self.inner.capabilities()
    }
    fn select(&mut self, request: TargetRequest) -> Result<TargetIdentity> {
        self.inner.select(request)
    }
    fn frame(&mut self, target: &TargetIdentity) -> Result<Frame> {
        self.reads += 1;
        let frame = self.inner.frame(target)?;
        if self.reads > 1 {
            self.clock.advance(15_000);
        }
        Ok(frame)
    }
    fn act(&mut self, plan: &ActionPlan) -> Result<()> {
        self.inner.act(plan)
    }
    fn system_monitor(&mut self) -> Result<SystemSnapshot> {
        self.inner.system_monitor()
    }
    fn release(&mut self) {
        self.inner.release();
    }
}

fn selected(session: &mut PlatformSession) -> ActionPlan {
    session
        .select_target(
            TargetRequest::SyntheticFixture,
            TargetPermissions {
                read_elements: true,
                interact: true,
            },
        )
        .unwrap();
    let snapshot = session.screen_elements().unwrap();
    session
        .prepare(&snapshot.id, Action::Click { x: 30, y: 30 })
        .unwrap()
}

#[test]
fn snapshot_expiring_during_revalidation_cannot_authorize_action() {
    let (backend, control) = FixtureBackend::new();
    let clock = Arc::new(FixtureClock::default());
    let backend = SlowRefresh {
        inner: backend,
        clock: clock.clone(),
        reads: 0,
    };
    let mut session =
        PlatformSession::with_clock("review", Box::new(backend), clock, Duration::from_secs(15))
            .unwrap();
    let plan = selected(&mut session);
    let approval = session.approve(&plan.id).unwrap();
    let result = session.execute(approval, &mut || Ok(()));
    assert!(
        matches!(result, Err(Error::SnapshotExpired)),
        "refresh elapsed beyond review deadline: {result:?}"
    );
    assert!(
        control.actions().is_empty(),
        "expired approval caused an effect"
    );
}

#[test]
fn revocation_after_refresh_prevents_action_and_consumes_attempt() {
    let (backend, control) = FixtureBackend::new();
    let mut session = PlatformSession::new("review", Box::new(backend)).unwrap();
    let plan = selected(&mut session);
    let approval = session.approve(&plan.id).unwrap();
    let second_approval = session.approve(&plan.id).unwrap();
    let mut checks = 0;
    let result = session.execute(approval, &mut || {
        checks += 1;
        if checks == 2 {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    });
    assert_eq!(result, Err(Error::Cancelled));
    assert_eq!(control.frame_reads(), 2);
    assert!(control.actions().is_empty());
    assert_eq!(
        session.execute(second_approval, &mut || Ok(())),
        Err(Error::ApprovalConsumed)
    );
}

#[test]
fn uncertain_native_dispatch_is_never_automatically_replayed() {
    let (backend, control) = FixtureBackend::new();
    let mut session = PlatformSession::new("review", Box::new(backend)).unwrap();
    let plan = selected(&mut session);
    let approval = session.approve(&plan.id).unwrap();
    let second_approval = session.approve(&plan.id).unwrap();
    control.fail_next_action();
    assert!(matches!(
        session.execute(approval, &mut || Ok(())),
        Err(Error::UncertainSideEffect(_))
    ));
    assert_eq!(
        session.execute(second_approval, &mut || Ok(())),
        Err(Error::ApprovalConsumed)
    );
    assert!(session.pending_plan().is_none());
    assert!(control.actions().is_empty());
}

#[test]
fn construction_discovery_and_rejected_cross_session_authority_have_no_effect() {
    let (backend, control) = FixtureBackend::new();
    let mut first = PlatformSession::new("review", Box::new(backend)).unwrap();
    assert_eq!(
        first.capabilities().execution_kind,
        ExecutionKind::SyntheticFixture
    );
    assert_eq!(control.frame_reads(), 0);
    assert!(control.actions().is_empty());
    let plan = selected(&mut first);
    let approval = first.approve(&plan.id).unwrap();
    let (other, other_control) = FixtureBackend::new();
    let mut second = PlatformSession::new("review", Box::new(other)).unwrap();
    selected(&mut second);
    assert_eq!(
        second.execute(approval, &mut || Ok(())),
        Err(Error::ApprovalMismatch)
    );
    assert!(other_control.actions().is_empty());
    assert!(control.actions().is_empty());
    first.shutdown();
    assert_eq!(first.screen_elements(), Err(Error::ShuttingDown));
}
