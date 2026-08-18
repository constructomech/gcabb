//! Delivery of queued prompts to the runtime.
//!
//! GCABB owns the queue; this module is only the last mile that hands an item
//! to the agent. Every `session.queue.*` import in the workspace lives here, so
//! if that surface changes or disappears the damage is contained to this file.
//!
//! Two transports implement the same port:
//!
//! - [`NativeQueueTransport`] mirrors the queue into the runtime, which gives
//!   the runtime's own drain loop, visibility of items queued by other clients,
//!   and promotion of a queued item into an in-flight turn.
//! - [`SendOnIdleTransport`] uses only `session.send`, which carries no
//!   experimental marker. GCABB drives delivery itself, and steering still
//!   works because immediate delivery interrupts a running turn.

use app_model::QueueDelivery;
use async_trait::async_trait;
use github_copilot_sdk::rpc::{
    QueueInsertAtRequest, QueueInsertMessage, QueueMoveItemRequest, QueueRemoveAtRequest,
    QueueSendNowRequest, QueueSetDrainPausedRequest, QueueUpdateTextRequest,
};
use github_copilot_sdk::session::Session;
use github_copilot_sdk::{DeliveryMode, MessageOptions};

use crate::{ProviderError, Result};

/// Which delivery strategy a session is using.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueTransportKind {
    /// The runtime holds a mirror of the queue and drains it itself.
    Native,
    /// GCABB holds the queue alone and sends items as it decides.
    SendOnIdle,
}

impl QueueTransportKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Native => "Runtime queue",
            Self::SendOnIdle => "Send on idle",
        }
    }
}

/// One item as the runtime reports it.
///
/// Deliberately not [`app_model::QueueItem`]: this describes what the runtime
/// currently holds, which may include items queued by other clients and never
/// carries GCABB's identifiers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeQueueItem {
    pub runtime_id: String,
    pub display_text: String,
    /// Whether this is a slash command rather than a user prompt. Commands are
    /// not GCABB's to manage even when they appear in the same queue.
    pub is_command: bool,
    pub agent_mode: Option<String>,
}

/// What the runtime is currently holding.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeQueue {
    pub items: Vec<RuntimeQueueItem>,
    /// Messages waiting to interrupt the active turn.
    pub steering: Vec<String>,
}

/// A request to hand one prompt to the agent.
#[derive(Clone, Debug)]
pub struct QueueDeliveryRequest {
    pub prompt: String,
    pub display_prompt: Option<String>,
    pub delivery: QueueDelivery,
    pub agent_mode: Option<String>,
}

/// What a delivery produced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeliveryReceipt {
    /// Identifier the runtime assigned, when the item entered the runtime
    /// queue. Absent when the prompt was sent directly as a turn.
    pub runtime_id: Option<String>,
    /// Message identifier, when the prompt was sent as a turn.
    pub message_id: Option<String>,
}

/// The last mile between GCABB's queue and the agent.
#[async_trait]
pub trait QueueTransport: Send + Sync {
    fn kind(&self) -> QueueTransportKind;

    /// Whether this transport works against the given session.
    ///
    /// Called once per session so a runtime that has dropped the experimental
    /// queue surface degrades to the fallback instead of failing every edit.
    async fn probe(&self, session: &Session) -> bool;

    /// What the runtime is holding, for reconciling against GCABB's queue.
    async fn pending(&self, session: &Session) -> Result<RuntimeQueue>;

    /// Hand one prompt to the agent.
    async fn deliver(
        &self,
        session: &Session,
        request: &QueueDeliveryRequest,
    ) -> Result<DeliveryReceipt>;

    /// Edit an item the runtime is holding.
    async fn update_text(
        &self,
        session: &Session,
        runtime_id: &str,
        request: &QueueDeliveryRequest,
    ) -> Result<bool>;

    /// Move an item the runtime is holding to a new zero-based position.
    async fn move_to(&self, session: &Session, runtime_id: &str, position: i64) -> Result<bool>;

    /// Promote an item the runtime is holding into the active turn.
    async fn steer(&self, session: &Session, runtime_id: &str) -> Result<bool>;

    /// Withdraw an item the runtime is holding.
    async fn withdraw(&self, session: &Session, runtime_id: &str) -> Result<bool>;

    /// Suspend or resume the runtime's own draining.
    async fn set_paused(&self, session: &Session, paused: bool) -> Result<()>;

    /// Drop everything the runtime is holding, leaving GCABB's queue intact.
    async fn clear(&self, session: &Session) -> Result<()>;
}

fn sdk_error(error: &github_copilot_sdk::Error) -> ProviderError {
    ProviderError::Sdk(error.to_string())
}

fn message_options(request: &QueueDeliveryRequest, mode: DeliveryMode) -> MessageOptions {
    let mut options = MessageOptions::from(request.prompt.clone()).with_mode(mode);
    if let Some(display) = request.display_prompt.clone() {
        options.display_prompt = Some(display);
    }
    options
}

/// Mirrors GCABB's queue into the runtime using `session.queue.*`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeQueueTransport;

#[async_trait]
impl QueueTransport for NativeQueueTransport {
    fn kind(&self) -> QueueTransportKind {
        QueueTransportKind::Native
    }

    async fn probe(&self, session: &Session) -> bool {
        session.rpc().queue().pending_items().await.is_ok()
    }

    async fn pending(&self, session: &Session) -> Result<RuntimeQueue> {
        let pending = session
            .rpc()
            .queue()
            .pending_items()
            .await
            .map_err(|error| sdk_error(&error))?;
        Ok(RuntimeQueue {
            items: pending
                .items
                .into_iter()
                .map(|item| RuntimeQueueItem {
                    runtime_id: item.id,
                    display_text: item.display_text,
                    is_command: matches!(
                        item.kind,
                        github_copilot_sdk::rpc::QueuePendingItemsKind::Command
                    ),
                    agent_mode: agent_mode_label(&item.agent_mode),
                })
                .collect(),
            steering: pending.steering_messages,
        })
    }

    async fn deliver(
        &self,
        session: &Session,
        request: &QueueDeliveryRequest,
    ) -> Result<DeliveryReceipt> {
        // Steering has to interrupt the running turn, which the runtime queue
        // cannot express for an item it is not already holding.
        if request.delivery == QueueDelivery::Steer {
            let message_id = session
                .send(message_options(request, DeliveryMode::Immediate))
                .await
                .map_err(|error| sdk_error(&error))?;
            return Ok(DeliveryReceipt {
                runtime_id: None,
                message_id: Some(message_id),
            });
        }

        // Appending: the runtime rejects a position past the end, and GCABB's
        // own positions are strided rather than contiguous, so the runtime's
        // current length is the only valid append point.
        let position = i64::try_from(self.pending(session).await?.items.len()).unwrap_or(i64::MAX);
        let inserted = session
            .rpc()
            .queue()
            .insert_at(QueueInsertAtRequest {
                message: QueueInsertMessage {
                    prompt: request.prompt.clone(),
                    display_prompt: request.display_prompt.clone(),
                    ..QueueInsertMessage::default()
                },
                position,
            })
            .await
            .map_err(|error| sdk_error(&error))?;
        Ok(DeliveryReceipt {
            runtime_id: Some(inserted.id),
            message_id: None,
        })
    }

    async fn update_text(
        &self,
        session: &Session,
        runtime_id: &str,
        request: &QueueDeliveryRequest,
    ) -> Result<bool> {
        let updated = session
            .rpc()
            .queue()
            .update_text(QueueUpdateTextRequest {
                id: runtime_id.to_owned(),
                prompt: request.prompt.clone(),
                display_prompt: request.display_prompt.clone(),
            })
            .await
            .map_err(|error| sdk_error(&error))?;
        Ok(updated.updated)
    }

    async fn move_to(&self, session: &Session, runtime_id: &str, position: i64) -> Result<bool> {
        let moved = session
            .rpc()
            .queue()
            .move_item(QueueMoveItemRequest {
                id: runtime_id.to_owned(),
                to_position: position,
            })
            .await
            .map_err(|error| sdk_error(&error))?;
        Ok(moved.changed)
    }

    async fn steer(&self, session: &Session, runtime_id: &str) -> Result<bool> {
        let steered = session
            .rpc()
            .queue()
            .send_now(QueueSendNowRequest {
                id: runtime_id.to_owned(),
            })
            .await
            .map_err(|error| sdk_error(&error))?;
        Ok(steered.steered)
    }

    async fn withdraw(&self, session: &Session, runtime_id: &str) -> Result<bool> {
        let removed = session
            .rpc()
            .queue()
            .remove_at(QueueRemoveAtRequest {
                id: runtime_id.to_owned(),
            })
            .await
            .map_err(|error| sdk_error(&error))?;
        Ok(removed.removed)
    }

    async fn set_paused(&self, session: &Session, paused: bool) -> Result<()> {
        session
            .rpc()
            .queue()
            .set_drain_paused(QueueSetDrainPausedRequest { paused })
            .await
            .map_err(|error| sdk_error(&error))
    }

    async fn clear(&self, session: &Session) -> Result<()> {
        session
            .rpc()
            .queue()
            .clear()
            .await
            .map_err(|error| sdk_error(&error))
    }
}

fn agent_mode_label(mode: &github_copilot_sdk::rpc::SendAgentMode) -> Option<String> {
    use github_copilot_sdk::rpc::SendAgentMode;
    match mode {
        SendAgentMode::Interactive => Some("interactive".to_owned()),
        SendAgentMode::Plan => Some("plan".to_owned()),
        SendAgentMode::Autopilot => Some("autopilot".to_owned()),
        SendAgentMode::Shell => Some("shell".to_owned()),
        SendAgentMode::Unknown => None,
    }
}

/// Delivers through `session.send` alone, with GCABB deciding when.
///
/// The runtime never holds a queue under this transport, so the operations
/// that address a runtime-held item report that there was nothing to act on
/// rather than failing. GCABB's own queue remains fully editable.
#[derive(Clone, Copy, Debug, Default)]
pub struct SendOnIdleTransport;

#[async_trait]
impl QueueTransport for SendOnIdleTransport {
    fn kind(&self) -> QueueTransportKind {
        QueueTransportKind::SendOnIdle
    }

    async fn probe(&self, _session: &Session) -> bool {
        // Built only on `session.send`, which is not experimental.
        true
    }

    async fn pending(&self, _session: &Session) -> Result<RuntimeQueue> {
        Ok(RuntimeQueue::default())
    }

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
            runtime_id: None,
            message_id: Some(message_id),
        })
    }

    async fn update_text(
        &self,
        _session: &Session,
        _runtime_id: &str,
        _request: &QueueDeliveryRequest,
    ) -> Result<bool> {
        Ok(false)
    }

    async fn move_to(&self, _session: &Session, _runtime_id: &str, _position: i64) -> Result<bool> {
        Ok(false)
    }

    async fn steer(&self, _session: &Session, _runtime_id: &str) -> Result<bool> {
        Ok(false)
    }

    async fn withdraw(&self, _session: &Session, _runtime_id: &str) -> Result<bool> {
        Ok(false)
    }

    async fn set_paused(&self, _session: &Session, _paused: bool) -> Result<()> {
        Ok(())
    }

    async fn clear(&self, _session: &Session) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_kinds_are_distinguishable() {
        assert_eq!(NativeQueueTransport.kind(), QueueTransportKind::Native);
        assert_eq!(SendOnIdleTransport.kind(), QueueTransportKind::SendOnIdle);
        assert_ne!(
            QueueTransportKind::Native.label(),
            QueueTransportKind::SendOnIdle.label()
        );
    }

    #[test]
    fn display_prompt_rides_along_when_present() {
        let request = QueueDeliveryRequest {
            prompt: "raw".to_owned(),
            display_prompt: Some("shown".to_owned()),
            delivery: QueueDelivery::WhenIdle,
            agent_mode: None,
        };
        let options = message_options(&request, DeliveryMode::Enqueue);
        assert_eq!(options.prompt, "raw");
        assert_eq!(options.display_prompt.as_deref(), Some("shown"));
    }

    #[test]
    fn steering_uses_immediate_delivery_and_queueing_does_not() {
        let steer = QueueDeliveryRequest {
            prompt: "p".to_owned(),
            display_prompt: None,
            delivery: QueueDelivery::Steer,
            agent_mode: None,
        };
        assert_eq!(
            message_options(&steer, DeliveryMode::Immediate).mode,
            Some(DeliveryMode::Immediate)
        );
        assert_eq!(
            message_options(&steer, DeliveryMode::Enqueue).mode,
            Some(DeliveryMode::Enqueue)
        );
    }

    #[test]
    fn unknown_agent_modes_are_dropped_rather_than_guessed() {
        use github_copilot_sdk::rpc::SendAgentMode;
        assert_eq!(
            agent_mode_label(&SendAgentMode::Interactive).as_deref(),
            Some("interactive")
        );
        assert_eq!(
            agent_mode_label(&SendAgentMode::Plan).as_deref(),
            Some("plan")
        );
        assert!(agent_mode_label(&SendAgentMode::Unknown).is_none());
    }
}
