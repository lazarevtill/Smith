use crate::*;
use std::{collections::BTreeSet, sync::Arc};
use uuid::Uuid;

/// Opaque, single-use application-user authority. Never serialize or persist.
pub struct ActionApproval {
    owner: String,
    plan: ActionPlan,
}

pub struct PlatformSession {
    owner: String,
    principal: String,
    binding: String,
    backend: Box<dyn PlatformBackend>,
    clock: Arc<dyn Clock>,
    ttl: Duration,
    generation: u64,
    selected: Option<(TargetIdentity, TargetPermissions)>,
    snapshot: Option<(Snapshot, Duration)>,
    plan: Option<ActionPlan>,
    consumed: BTreeSet<String>,
    stopped: bool,
}
impl PlatformSession {
    pub fn new(principal: impl Into<String>, backend: Box<dyn PlatformBackend>) -> Result<Self> {
        Self::with_clock(
            principal,
            backend,
            Arc::new(MonotonicClock::default()),
            Duration::from_secs(15),
        )
    }
    pub fn with_clock(
        principal: impl Into<String>,
        backend: Box<dyn PlatformBackend>,
        clock: Arc<dyn Clock>,
        ttl: Duration,
    ) -> Result<Self> {
        let principal = principal.into();
        if principal.is_empty()
            || principal.len() > 128
            || principal.chars().any(char::is_control)
            || ttl.is_zero()
            || ttl > Duration::from_secs(30)
        {
            return Err(Error::InvalidInput("principal or snapshot TTL".into()));
        }
        Ok(Self {
            owner: Uuid::new_v4().to_string(),
            binding: Uuid::new_v4().to_string(),
            principal,
            backend,
            clock,
            ttl,
            generation: 0,
            selected: None,
            snapshot: None,
            plan: None,
            consumed: BTreeSet::new(),
            stopped: false,
        })
    }
    pub fn principal(&self) -> &str {
        &self.principal
    }
    pub fn binding(&self) -> &str {
        &self.binding
    }
    pub fn capabilities(&self) -> PlatformCapabilities {
        self.backend.capabilities()
    }
    pub fn selected_target(&self) -> Option<&TargetIdentity> {
        self.selected.as_ref().map(|(target, _)| target)
    }
    pub fn pending_plan(&self) -> Option<&ActionPlan> {
        self.plan.as_ref()
    }

    /// Trusted host boundary, invoked only by an authenticated explicit user
    /// selection. Never invoke as a side effect of tool/model content.
    pub fn select_target(
        &mut self,
        request: TargetRequest,
        permissions: TargetPermissions,
    ) -> Result<TargetIdentity> {
        self.active()?;
        if !permissions.read_elements {
            return Err(Error::Denied(
                "selecting a target requires explicit element-read consent".into(),
            ));
        }
        self.revoke();
        let target = self.backend.select(request)?;
        self.selected = Some((target.clone(), permissions));
        Ok(target)
    }
    pub fn screen_elements(&mut self) -> Result<Snapshot> {
        self.active()?;
        let target = self
            .selected
            .as_ref()
            .filter(|(_, p)| p.read_elements)
            .map(|(t, _)| t.clone())
            .ok_or_else(|| Error::Denied("select a target and grant element access".into()))?;
        self.invalidate();
        let frame = self.backend.frame(&target)?;
        validate_frame(&frame, &target)?;
        let elements = frame
            .elements
            .iter()
            .filter_map(|e| {
                e.name.as_ref().map(|name| {
                    let (x, y) = e.bounds.center();
                    format!(
                        "[{}] {} {} @ ({x},{y})",
                        e.id,
                        e.role,
                        serde_json::to_string(name).unwrap_or_default()
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
        let snapshot = Snapshot {
            id: Uuid::new_v4().to_string(),
            generation: self.generation,
            valid_for_ms: self.ttl.as_millis() as u64,
            frame,
            elements,
        };
        if serde_json::to_vec(&snapshot)
            .map_err(|_| Error::Backend("snapshot serialization".into()))?
            .len()
            > 192 * 1024
        {
            return Err(Error::Backend(
                "selected accessibility snapshot exceeds byte budget".into(),
            ));
        }
        self.snapshot = Some((snapshot.clone(), self.clock.elapsed()));
        Ok(snapshot)
    }
    pub fn prepare(&mut self, snapshot_id: &str, action: Action) -> Result<ActionPlan> {
        self.active()?;
        if !self.selected.as_ref().is_some_and(|(_, p)| p.interact) {
            return Err(Error::Denied("explicit interaction grant required".into()));
        }
        let snapshot = self.fresh_snapshot(snapshot_id)?;
        let element = match &action {
            Action::Click { x, y } => {
                if !snapshot.frame.bounds.contains(*x, *y) {
                    return Err(Error::Denied("click outside selected window".into()));
                }
                snapshot
                    .frame
                    .elements
                    .iter()
                    .filter(|e| e.enabled && e.can_press && e.bounds.contains(*x, *y))
                    .min_by_key(|e| u64::from(e.bounds.width) * u64::from(e.bounds.height))
                    .cloned()
                    .ok_or_else(|| {
                        Error::Unsupported(
                            "no pressable selected-window element at these coordinates".into(),
                        )
                    })?
            }
            Action::TypeText { text } => {
                if text.is_empty() || text.len() > 16 * 1024 || text.contains('\0') {
                    return Err(Error::InvalidInput(
                        "text must contain 1–16384 bytes without NUL".into(),
                    ));
                }
                let mut focused = snapshot
                    .frame
                    .elements
                    .iter()
                    .filter(|e| e.enabled && e.focused && e.can_insert_text);
                let target = focused.next().cloned().ok_or_else(|| {
                    Error::Unsupported(
                        "focused control does not support scoped text insertion".into(),
                    )
                })?;
                if focused.next().is_some() {
                    return Err(Error::Denied("ambiguous focused control".into()));
                }
                target
            }
        };
        let plan = ActionPlan {
            id: Uuid::new_v4().to_string(),
            binding: self.binding.clone(),
            snapshot_id: snapshot.id.clone(),
            target: snapshot.frame.target.clone(),
            element,
            action,
            generation: self.generation,
        };
        self.plan = Some(plan.clone());
        Ok(plan)
    }
    /// Call only after the application user confirms this exact displayed plan.
    /// Task approval or a model argument alone must never call this function.
    pub fn approve(&mut self, plan_id: &str) -> Result<ActionApproval> {
        self.active()?;
        let plan = self
            .plan
            .as_ref()
            .filter(|p| p.id == plan_id)
            .ok_or(Error::ApprovalMismatch)?;
        self.fresh_snapshot(&plan.snapshot_id)?;
        Ok(ActionApproval {
            owner: self.owner.clone(),
            plan: plan.clone(),
        })
    }
    pub fn validate_plan_action(&self, plan_id: &str, action: &Action) -> Result<()> {
        self.active()?;
        let plan = self
            .plan
            .as_ref()
            .filter(|p| p.id == plan_id && &p.action == action)
            .ok_or(Error::ApprovalMismatch)?;
        self.fresh_snapshot(&plan.snapshot_id)?;
        Ok(())
    }
    pub fn execute(
        &mut self,
        approval: ActionApproval,
        check_active: &mut dyn FnMut() -> Result<()>,
    ) -> Result<ActionReceipt> {
        self.active()?;
        if self.consumed.contains(&approval.plan.id) {
            return Err(Error::ApprovalConsumed);
        }
        if approval.owner != self.owner
            || self.plan.as_ref() != Some(&approval.plan)
            || approval.plan.generation != self.generation
            || approval.plan.binding != self.binding
        {
            return Err(Error::ApprovalMismatch);
        }
        let expected_frame = self
            .fresh_snapshot(&approval.plan.snapshot_id)?
            .frame
            .clone();
        let snapshot_at = self.snapshot.as_ref().ok_or(Error::SnapshotExpired)?.1;
        check_active()?;
        // Consume before the native dispatch, including error/uncertain outcomes.
        self.consumed.insert(approval.plan.id.clone());
        if self.consumed.len() > 1024 {
            self.consumed.clear();
            self.consumed.insert(approval.plan.id.clone());
        }
        let observed = self.backend.frame(&approval.plan.target);
        self.invalidate();
        let observed = observed?;
        validate_frame(&observed, &approval.plan.target)?;
        if observed != expected_frame {
            return Err(Error::SnapshotChanged);
        }
        check_active()?;
        if self
            .clock
            .elapsed()
            .checked_sub(snapshot_at)
            .is_none_or(|elapsed| elapsed >= self.ttl)
        {
            return Err(Error::SnapshotExpired);
        }
        self.backend.act(&approval.plan)?;
        Ok(ActionReceipt {
            ok: true,
            plan_id: approval.plan.id,
            execution_kind: approval.plan.target.execution_kind,
            target: approval.plan.target,
        })
    }
    /// Resource sampling is called only after a host SystemMonitor grant check.
    pub fn system_monitor(&mut self) -> Result<SystemSnapshot> {
        self.active()?;
        self.backend.system_monitor()
    }
    pub fn revoke(&mut self) {
        self.selected = None;
        self.binding = Uuid::new_v4().to_string();
        self.invalidate();
        self.backend.release();
    }
    pub fn shutdown(&mut self) {
        self.revoke();
        self.stopped = true;
    }
    fn active(&self) -> Result<()> {
        if self.stopped {
            Err(Error::ShuttingDown)
        } else {
            Ok(())
        }
    }
    fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.snapshot = None;
        self.plan = None;
    }
    fn fresh_snapshot(&self, id: &str) -> Result<&Snapshot> {
        let (snapshot, time) = self
            .snapshot
            .as_ref()
            .filter(|(s, _)| s.id == id)
            .ok_or(Error::SnapshotExpired)?;
        if self
            .clock
            .elapsed()
            .checked_sub(*time)
            .is_none_or(|elapsed| elapsed >= self.ttl)
        {
            return Err(Error::SnapshotExpired);
        }
        if self.selected.as_ref().map(|(t, _)| t) != Some(&snapshot.frame.target) {
            return Err(Error::TargetChanged);
        }
        Ok(snapshot)
    }
}
impl Drop for PlatformSession {
    fn drop(&mut self) {
        self.backend.release();
    }
}

fn validate_frame(frame: &Frame, target: &TargetIdentity) -> Result<()> {
    if &frame.target != target {
        return Err(Error::TargetChanged);
    }
    if frame.elements.len() > 250 || frame.bounds.width == 0 || frame.bounds.height == 0 {
        return Err(Error::Backend("invalid or oversized window tree".into()));
    }
    let mut ids = BTreeSet::new();
    for e in &frame.elements {
        if e.id.is_empty()
            || e.id.len() > 256
            || !ids.insert(&e.id)
            || e.role.len() > 128
            || e.name.as_ref().is_some_and(|s| s.len() > 2048)
            || e.bounds.width == 0
            || e.bounds.height == 0
        {
            return Err(Error::Backend("invalid element tree".into()));
        }
    }
    Ok(())
}
