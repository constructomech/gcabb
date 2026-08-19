//! Delivery of queued follow-ups to the runtime.
//!
//! GCABB owns the queue; this module is only the last mile that hands one item
//! to the agent. Delivery uses `session.send` alone, so the runtime never holds
//! a copy of the queue: GCABB decides what runs next and when, and a follow-up
//! stays editable right up to the moment it is dispatched.
//!
//! The runtime does have its own `session.queue.*` surface, but mirroring into
//! it would buy nothing here. GCABB releases one item per idle event either
//! way, so the mirror would only add a second source of truth whose identifiers
//! are reissued per session and whose contents are lost on disconnect.

use app_model::QueueDelivery;
use async_trait::async_trait;
use github_copilot_sdk::session::Session;
use github_copilot_sdk::{AgentMode, DeliveryMode, MessageOptions};

use crate::{ProviderError, Result};

/// A request to hand one prompt to the agent.
#[derive(Clone, Debug)]
pub struct QueueDeliveryRequest {
    pub prompt: String,
    pub display_prompt: Option<String>,
    pub delivery: QueueDelivery,
    /// Mode the follow-up was queued under, when it should not simply inherit
    /// whatever mode the session happens to be in at delivery time.
    pub agent_mode: Option<String>,
}

/// What a delivery produced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeliveryReceipt {
    /// Identifier of the message the runtime accepted.
    pub message_id: Option<String>,
}

/// The last mile between GCABB's queue and the agent.
#[async_trait]
pub trait QueueTransport: Send + Sync {
    /// Hand one prompt to the agent.
    async fn deliver(
        &self,
        session: &Session,
        request: &QueueDeliveryRequest,
    ) -> Result<DeliveryReceipt>;
}

fn sdk_error(error: &github_copilot_sdk::Error) -> ProviderError {
    ProviderError::Sdk(error.to_string())
}

/// Map a GCABB mode name onto the SDK's per-message mode.
///
/// An unrecognised name is dropped rather than guessed, which leaves the
/// session's current mode in force instead of silently running the follow-up
/// somewhere the developer did not ask for.
fn agent_mode(mode: Option<&str>) -> Option<AgentMode> {
    match mode? {
        "interactive" => Some(AgentMode::Interactive),
        "plan" => Some(AgentMode::Plan),
        "autopilot" => Some(AgentMode::Autopilot),
        "shell" => Some(AgentMode::Shell),
        _ => None,
    }
}

fn message_options(request: &QueueDeliveryRequest, mode: DeliveryMode) -> MessageOptions {
    let mut options = MessageOptions::from(request.prompt.clone()).with_mode(mode);
    if let Some(display) = request.display_prompt.clone() {
        options.display_prompt = Some(display);
    }
    options.agent_mode = agent_mode(request.agent_mode.as_deref());
    options
}

/// Delivers through `session.send`, with GCABB deciding when.
#[derive(Clone, Copy, Debug, Default)]
pub struct SendOnIdleTransport;

#[async_trait]
impl QueueTransport for SendOnIdleTransport {
    async fn deliver(
        &self,
        session: &Session,
        request: &QueueDeliveryRequest,
    ) -> Result<DeliveryReceipt> {
        // Enqueue is still correct for a non-steering item: GCABB only calls
        // this once it has decided the item should run, and enqueue delivery
        // keeps it from cutting into a turn that started in the meantime.
        let mode = match request.delivery {
            QueueDelivery::WhenIdle => DeliveryMode::Enqueue,
            QueueDelivery::Steer => DeliveryMode::Immediate,
        };
        let message_id = session
            .send(message_options(request, mode))
            .await
            .map_err(|error| sdk_error(&error))?;
        Ok(DeliveryReceipt {
            message_id: Some(message_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(delivery: QueueDelivery) -> QueueDeliveryRequest {
        QueueDeliveryRequest {
            prompt: "p".to_owned(),
            display_prompt: None,
            delivery,
            agent_mode: None,
        }
    }

    #[test]
    fn display_prompt_rides_along_when_present() {
        let mut request = request(QueueDelivery::WhenIdle);
        request.prompt = "raw".to_owned();
        request.display_prompt = Some("shown".to_owned());
        let options = message_options(&request, DeliveryMode::Enqueue);
        assert_eq!(options.prompt, "raw");
        assert_eq!(options.display_prompt.as_deref(), Some("shown"));
    }

    #[test]
    fn steering_uses_immediate_delivery_and_queueing_does_not() {
        assert_eq!(
            message_options(&request(QueueDelivery::Steer), DeliveryMode::Immediate).mode,
            Some(DeliveryMode::Immediate)
        );
        assert_eq!(
            message_options(&request(QueueDelivery::WhenIdle), DeliveryMode::Enqueue).mode,
            Some(DeliveryMode::Enqueue)
        );
    }

    #[test]
    fn a_follow_up_runs_in_the_mode_it_was_queued_under() {
        let mut queued = request(QueueDelivery::WhenIdle);
        queued.agent_mode = Some("plan".to_owned());
        assert_eq!(
            message_options(&queued, DeliveryMode::Enqueue).agent_mode,
            Some(AgentMode::Plan)
        );
    }

    #[test]
    fn unknown_modes_leave_the_session_mode_in_force() {
        let mut queued = request(QueueDelivery::WhenIdle);
        queued.agent_mode = Some("teleportation".to_owned());
        assert!(message_options(&queued, DeliveryMode::Enqueue).agent_mode.is_none());
        assert!(message_options(&request(QueueDelivery::WhenIdle), DeliveryMode::Enqueue)
            .agent_mode
            .is_none());
    }
}
