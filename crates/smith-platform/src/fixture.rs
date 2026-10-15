//! Deterministic owned data only. This backend never reads or controls the OS.
use crate::*;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct FixtureControl(Arc<Mutex<State>>);
struct State {
    frame: Frame,
    actions: Vec<ActionPlan>,
    fail_next: bool,
    reads: usize,
    selected: bool,
}
pub struct FixtureBackend {
    control: FixtureControl,
}
impl FixtureBackend {
    pub fn new() -> (Self, FixtureControl) {
        let frame = Frame {
            target: TargetIdentity {
                id: uuid::Uuid::new_v4().to_string(),
                label: "Synthetic fixture window".into(),
                execution_kind: ExecutionKind::SyntheticFixture,
            },
            bounds: Bounds {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
            elements: vec![
                UiElement {
                    id: "fixture-button".into(),
                    role: "Button".into(),
                    name: Some("Fixture Save".into()),
                    bounds: Bounds {
                        x: 20,
                        y: 20,
                        width: 120,
                        height: 40,
                    },
                    enabled: true,
                    focused: false,
                    can_press: true,
                    can_insert_text: false,
                    text_selection: None,
                },
                UiElement {
                    id: "fixture-field".into(),
                    role: "TextField".into(),
                    name: Some("Fixture draft".into()),
                    bounds: Bounds {
                        x: 20,
                        y: 80,
                        width: 400,
                        height: 60,
                    },
                    enabled: true,
                    focused: true,
                    can_press: false,
                    can_insert_text: true,
                    text_selection: Some(TextSelection {
                        location: 0,
                        length: 0,
                    }),
                },
            ],
        };
        let control = FixtureControl(Arc::new(Mutex::new(State {
            frame,
            actions: Vec::new(),
            fail_next: false,
            reads: 0,
            selected: false,
        })));
        (
            Self {
                control: control.clone(),
            },
            control,
        )
    }
}
impl FixtureControl {
    pub fn actions(&self) -> Vec<ActionPlan> {
        self.0.lock().unwrap().actions.clone()
    }
    pub fn frame_reads(&self) -> usize {
        self.0.lock().unwrap().reads
    }
    pub fn mutate_frame(&self, mutation: impl FnOnce(&mut Frame)) {
        mutation(&mut self.0.lock().unwrap().frame);
    }
    pub fn fail_next_action(&self) {
        self.0.lock().unwrap().fail_next = true;
    }
}
impl PlatformBackend for FixtureBackend {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            platform: "fixture".into(),
            execution_kind: ExecutionKind::SyntheticFixture,
            system_monitor: Support::Available,
            screen_elements: Support::Available,
            screen_click: Support::Available,
            screen_type: Support::Available,
            screenshot: Support::Unsupported("fixture has no pixels or desktop capture".into()),
            notes: vec!["SYNTHETIC FIXTURE: operations change owned in-memory data only".into()],
        }
    }
    fn select(&mut self, request: TargetRequest) -> Result<TargetIdentity> {
        if !matches!(request, TargetRequest::SyntheticFixture) {
            return Err(Error::Unsupported(
                "fixture cannot select a native target".into(),
            ));
        }
        let mut s = self.control.0.lock().unwrap();
        s.selected = true;
        Ok(s.frame.target.clone())
    }
    fn frame(&mut self, target: &TargetIdentity) -> Result<Frame> {
        let mut s = self.control.0.lock().unwrap();
        if !s.selected || &s.frame.target != target {
            return Err(Error::TargetChanged);
        }
        s.reads += 1;
        Ok(s.frame.clone())
    }
    fn act(&mut self, plan: &ActionPlan) -> Result<()> {
        let mut s = self.control.0.lock().unwrap();
        if !s.selected || plan.target != s.frame.target || !s.frame.elements.contains(&plan.element)
        {
            return Err(Error::TargetChanged);
        }
        if s.fail_next {
            s.fail_next = false;
            return Err(Error::UncertainSideEffect(
                "synthetic dispatch failure".into(),
            ));
        }
        s.actions.push(plan.clone());
        Ok(())
    }
    fn system_monitor(&mut self) -> Result<SystemSnapshot> {
        Ok(SystemSnapshot {
            cpu_percent: 12.5,
            memory_used_bytes: 2 * 1024 * 1024 * 1024,
            memory_total_bytes: 8 * 1024 * 1024 * 1024,
            disks: vec![DiskUsage {
                mount: "/synthetic".into(),
                total_bytes: 64 * 1024 * 1024 * 1024,
                available_bytes: 32 * 1024 * 1024 * 1024,
            }],
            execution_kind: ExecutionKind::SyntheticFixture,
            sample_ms: 250,
        })
    }
    fn release(&mut self) {
        self.control.0.lock().unwrap().selected = false;
    }
}

#[derive(Default)]
pub struct FixtureClock(std::sync::atomic::AtomicU64);
impl FixtureClock {
    pub fn advance(&self, millis: u64) {
        self.0
            .fetch_add(millis, std::sync::atomic::Ordering::SeqCst);
    }
}
impl Clock for FixtureClock {
    fn elapsed(&self) -> Duration {
        Duration::from_millis(self.0.load(std::sync::atomic::Ordering::SeqCst))
    }
}
