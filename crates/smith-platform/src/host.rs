use crate::*;
use serde_json::{json, Value};
use smith_agent_core::{
    Availability, Capability, ExecutionGrant, HostToolAdapter, HostToolContext, ResourceKind,
    ToolCall, ToolDescriptor,
};
use std::sync::{Arc, Mutex, MutexGuard};

pub struct PlatformToolAdapter {
    session: Arc<Mutex<PlatformSession>>,
    approved: Mutex<Option<ApprovedCall>>,
}
struct ApprovedCall {
    task_id: String,
    call: ToolCall,
    approval: ActionApproval,
}
impl PlatformToolAdapter {
    pub fn new(session: Arc<Mutex<PlatformSession>>) -> Self {
        Self {
            session,
            approved: Mutex::new(None),
        }
    }
    /// Explicit authenticated host action only, after showing the exact plan,
    /// task and call. Does not grant TaskEngine approval; both checks apply.
    pub fn approve_for_call(&self, plan_id: &str, task_id: &str, call: &ToolCall) -> Result<()> {
        if task_id.is_empty() || task_id.len() > 128 || call.id.is_empty() || call.id.len() > 128 {
            return Err(Error::InvalidInput("task/call identity".into()));
        }
        let mut session = self.lock()?;
        let Some((requested, action, review)) = parse_action(call)? else {
            return Err(Error::InvalidInput(
                "only screen actions can be approved".into(),
            ));
        };
        if requested != plan_id {
            return Err(Error::ApprovalMismatch);
        }
        session.validate_plan_action(plan_id, &action)?;
        validate_review(&session, &review)?;
        let approval = session.approve(plan_id)?;
        *self.approved.lock().map_err(|_| poisoned())? = Some(ApprovedCall {
            task_id: task_id.into(),
            call: call.clone(),
            approval,
        });
        Ok(())
    }
    pub fn revoke_approval(&self) -> Result<()> {
        *self.approved.lock().map_err(|_| poisoned())? = None;
        Ok(())
    }
    fn lock(&self) -> Result<MutexGuard<'_, PlatformSession>> {
        self.session.lock().map_err(|_| poisoned())
    }
}
impl HostToolAdapter for PlatformToolAdapter {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let capabilities = self.session.lock().ok().map(|s| s.capabilities());
        [
            ("system_monitor", "system", Capability::SystemMonitor, false, json!({}), vec![], capabilities.as_ref().map(|c| &c.system_monitor)),
            ("screen_elements", "computer-use", Capability::ComputerUse, false, json!({}), vec![], capabilities.as_ref().map(|c| &c.screen_elements)),
            ("screen_click", "computer-use", Capability::ComputerUse, true, json!({"x":{"type":"integer","minimum":i32::MIN,"maximum":i32::MAX},"y":{"type":"integer","minimum":i32::MIN,"maximum":i32::MAX},"plan_id":{"type":"string"},"review":{"type":"object"}}), vec!["x", "y", "plan_id", "review"], capabilities.as_ref().map(|c| &c.screen_click)),
            ("screen_type", "computer-use", Capability::ComputerUse, true, json!({"text":{"type":"string","minLength":1,"maxLength":16384},"plan_id":{"type":"string"},"review":{"type":"object"}}), vec!["text", "plan_id", "review"], capabilities.as_ref().map(|c| &c.screen_type)),
        ].into_iter().map(|(name, source, capability, approval, properties, required, support)| ToolDescriptor {
            name: name.into(), source: format!("builtin:{source}"), parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            availability: match support { Some(Support::Unsupported(why)) => Availability::Unsupported(why.clone()), None => Availability::Unsupported("platform session lock failed".into()), _ => Availability::Implemented },
            required_capability: Some(capability), requires_approval: approval,
        }).collect()
    }
    fn validate(&self, grant: &ExecutionGrant, call: &ToolCall) -> smith_agent_core::Result<()> {
        let session = self.lock().map_err(core_error)?;
        validate(&session, grant, call).map_err(core_error)
    }
    fn execute(
        &self,
        context: &HostToolContext<'_>,
        check_active: &mut dyn FnMut() -> smith_agent_core::Result<()>,
    ) -> smith_agent_core::Result<Value> {
        let mut session = self.lock().map_err(core_error)?;
        validate(&session, context.grant, context.call).map_err(core_error)?;
        check_active()?;
        match context.call.name.as_str() {
            "system_monitor" => serde_json::to_value(session.system_monitor().map_err(core_error)?),
            "screen_elements" => serde_json::to_value(session.screen_elements().map_err(core_error)?),
            "screen_click" | "screen_type" => {
                let mut approved = self.approved.lock().map_err(|_| core_error(poisoned()))?;
                if !approved.as_ref().is_some_and(|a| a.task_id == context.task_id && a.call == *context.call) {
                    return Err(smith_agent_core::Error::Denied("explicit application-user approval for this exact platform plan, task and call is required".into()));
                }
                let approved = approved.take().unwrap();
                let receipt = session.execute(approved.approval, &mut || check_active().map_err(|_| Error::Cancelled)).map_err(core_error)?;
                serde_json::to_value(receipt)
            }
            _ => return Err(smith_agent_core::Error::Unsupported("unknown platform tool".into())),
        }.map_err(|_| smith_agent_core::Error::UncertainSideEffect("platform result serialization failed".into()))
    }
}
fn validate(session: &PlatformSession, grant: &ExecutionGrant, call: &ToolCall) -> Result<()> {
    let capability = match call.name.as_str() {
        "system_monitor" => Capability::SystemMonitor,
        "screen_elements" | "screen_click" | "screen_type" => Capability::ComputerUse,
        _ => return Err(Error::Unsupported("unknown platform tool".into())),
    };
    if grant.principal().id() != session.principal()
        || !grant.permits(capability)
        || !grant.permits_resource(ResourceKind::EffectBinding, session.binding())
    {
        return Err(Error::Denied(
            "principal, platform capability, or exact selected session binding mismatch".into(),
        ));
    }
    if let Some((plan_id, action, review)) = parse_action(call)? {
        session.validate_plan_action(&plan_id, &action)?;
        validate_review(session, &review)?;
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Click {
    x: i32,
    y: i32,
    plan_id: String,
    review: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeText {
    text: String,
    plan_id: String,
    review: Value,
}
fn parse_action(call: &ToolCall) -> Result<Option<(String, Action, Value)>> {
    fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T> {
        serde_json::from_value(value.clone()).map_err(|e| Error::InvalidInput(e.to_string()))
    }
    match call.name.as_str() {
        "system_monitor" | "screen_elements" => {
            parse::<Empty>(&call.arguments)?;
            Ok(None)
        }
        "screen_click" => {
            let a = parse::<Click>(&call.arguments)?;
            Ok(Some((
                a.plan_id,
                Action::Click { x: a.x, y: a.y },
                a.review,
            )))
        }
        "screen_type" => {
            let a = parse::<TypeText>(&call.arguments)?;
            Ok(Some((
                a.plan_id,
                Action::TypeText { text: a.text },
                a.review,
            )))
        }
        _ => Err(Error::Unsupported("unknown platform tool".into())),
    }
}
fn validate_review(session: &PlatformSession, review: &Value) -> Result<()> {
    let expected = serde_json::to_value(session.pending_plan())
        .map_err(|_| Error::Backend("plan review serialization".into()))?;
    if !review.is_object() || review != &expected {
        return Err(Error::ApprovalMismatch);
    }
    Ok(())
}
fn poisoned() -> Error {
    Error::Backend("platform authority lock failed; recreate the session".into())
}
fn core_error(error: Error) -> smith_agent_core::Error {
    match error {
        Error::InvalidInput(message) => smith_agent_core::Error::InvalidInput(message),
        Error::Unsupported(message) => smith_agent_core::Error::Unsupported(message),
        Error::UncertainSideEffect(message) => {
            smith_agent_core::Error::UncertainSideEffect(message)
        }
        other => smith_agent_core::Error::Denied(format!("platform: {other}")),
    }
}
