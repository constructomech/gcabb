#![allow(clippy::missing_errors_doc)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use app_model::{InteractionRequest, InteractionResponse, SessionControls};
use async_trait::async_trait;
use copilot_provider::{
    AgentProvider, ProviderCompatibility, ProviderError, ProviderEvent, ProviderInteraction,
    ProviderSession, Result, SDK_CRATE_VERSION, SessionRequest,
};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

pub const GOLDEN_SESSION_EVENTS: &str = include_str!("../fixtures/session-events.json");

#[derive(Default)]
pub struct FakeProvider {
    started: AtomicBool,
    next_session: AtomicU64,
    script: Mutex<Vec<Value>>,
    history: Mutex<HashMap<String, Vec<Value>>>,
    live: Mutex<HashMap<String, mpsc::Sender<ProviderEvent>>>,
    interactions: Mutex<HashMap<String, mpsc::Sender<ProviderInteraction>>>,
    fail_resume: AtomicBool,
    fail_history: AtomicBool,
}

impl FakeProvider {
    #[must_use]
    pub fn with_script(script: Vec<Value>) -> Self {
        Self {
            script: Mutex::new(script),
            ..Self::default()
        }
    }

    pub fn fail_resumes(&self, fail: bool) {
        self.fail_resume.store(fail, Ordering::SeqCst);
    }

    pub fn fail_history(&self, fail: bool) {
        self.fail_history.store(fail, Ordering::SeqCst);
    }

    pub async fn close_stream(&self, sdk_session_id: &str) {
        self.live.lock().await.remove(sdk_session_id);
    }

    pub async fn active_sessions(&self) -> usize {
        self.live.lock().await.len()
    }

    pub async fn request_interaction(
        &self,
        sdk_session_id: &str,
        request: InteractionRequest,
    ) -> Result<oneshot::Receiver<InteractionResponse>> {
        let sender = self
            .interactions
            .lock()
            .await
            .get(sdk_session_id)
            .cloned()
            .ok_or_else(|| ProviderError::SessionNotFound(sdk_session_id.to_owned()))?;
        let (response, receiver) = oneshot::channel();
        sender
            .send(ProviderInteraction { request, response })
            .await
            .map_err(|_| ProviderError::Sdk("fake interaction receiver closed".to_owned()))?;
        Ok(receiver)
    }

    pub async fn emit(&self, sdk_session_id: &str, event: Value) -> Result<()> {
        self.history
            .lock()
            .await
            .entry(sdk_session_id.to_owned())
            .or_default()
            .push(event.clone());
        let sender = self
            .live
            .lock()
            .await
            .get(sdk_session_id)
            .cloned()
            .ok_or_else(|| ProviderError::SessionNotFound(sdk_session_id.to_owned()))?;
        sender
            .send(ProviderEvent::Event(event))
            .await
            .map_err(|_| ProviderError::Sdk("fake event receiver closed".to_owned()))
    }

    async fn connect(&self, sdk_session_id: String) -> ProviderSession {
        let (sender, events) = mpsc::channel(128);
        let (interaction_sender, interactions) = mpsc::channel(16);
        self.live
            .lock()
            .await
            .insert(sdk_session_id.clone(), sender);
        self.interactions
            .lock()
            .await
            .insert(sdk_session_id.clone(), interaction_sender);
        ProviderSession {
            sdk_session_id,
            events,
            interactions,
        }
    }
}

#[async_trait]
impl AgentProvider for FakeProvider {
    async fn start(&self) -> Result<ProviderCompatibility> {
        self.started.store(true, Ordering::SeqCst);
        Ok(ProviderCompatibility {
            sdk_crate_version: SDK_CRATE_VERSION.to_owned(),
            sdk_protocol_version: 3,
            negotiated_protocol_version: 3,
            process_id: None,
            startup: None,
            available_modes: Vec::new(),
            available_models: Vec::new(),
        })
    }

    async fn stop(&self) -> Result<()> {
        self.started.store(false, Ordering::SeqCst);
        self.live.lock().await.clear();
        self.interactions.lock().await.clear();
        Ok(())
    }

    async fn create_session(&self, _request: SessionRequest) -> Result<ProviderSession> {
        if !self.started.load(Ordering::SeqCst) {
            return Err(ProviderError::NotStarted);
        }
        let number = self.next_session.fetch_add(1, Ordering::SeqCst) + 1;
        let sdk_session_id = format!("fake-session-{number}");
        self.history
            .lock()
            .await
            .entry(sdk_session_id.clone())
            .or_default();
        Ok(self.connect(sdk_session_id).await)
    }

    async fn resume_session(
        &self,
        sdk_session_id: &str,
        _request: SessionRequest,
    ) -> Result<ProviderSession> {
        if self.fail_resume.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk("configured resume failure".to_owned()));
        }
        if !self.history.lock().await.contains_key(sdk_session_id) {
            return Err(ProviderError::SessionNotFound(sdk_session_id.to_owned()));
        }
        Ok(self.connect(sdk_session_id.to_owned()).await)
    }

    async fn send(&self, sdk_session_id: &str, _prompt: &str) -> Result<String> {
        let script = self.script.lock().await.clone();
        for event in script {
            self.emit(sdk_session_id, event).await?;
        }
        Ok(format!("message-{sdk_session_id}"))
    }

    async fn cancel(&self, sdk_session_id: &str) -> Result<()> {
        self.emit(
            sdk_session_id,
            serde_json::json!({
                "id": format!("abort-{sdk_session_id}"),
                "type": "abort",
                "data": {"reason": "user_initiated"}
            }),
        )
        .await
    }

    async fn history(&self, sdk_session_id: &str) -> Result<Vec<Value>> {
        if self.fail_history.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk("configured history failure".to_owned()));
        }
        self.history
            .lock()
            .await
            .get(sdk_session_id)
            .cloned()
            .ok_or_else(|| ProviderError::SessionNotFound(sdk_session_id.to_owned()))
    }

    async fn controls(&self, _sdk_session_id: &str) -> Result<SessionControls> {
        Ok(SessionControls::default())
    }

    async fn set_model(
        &self,
        _sdk_session_id: &str,
        _model: &str,
        _reasoning_effort: Option<&str>,
        _context_tier: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }

    async fn set_mode(&self, _sdk_session_id: &str, _mode: &str) -> Result<()> {
        Ok(())
    }

    async fn set_reasoning_effort(&self, _sdk_session_id: &str, _effort: &str) -> Result<()> {
        Ok(())
    }

    async fn disconnect(&self, sdk_session_id: &str) -> Result<()> {
        self.live
            .lock()
            .await
            .remove(sdk_session_id)
            .ok_or_else(|| ProviderError::SessionNotFound(sdk_session_id.to_owned()))?;
        self.interactions.lock().await.remove(sdk_session_id);
        Ok(())
    }
}

pub fn golden_events() -> serde_json::Result<Vec<Value>> {
    serde_json::from_str(GOLDEN_SESSION_EVENTS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_fixture_is_valid_and_ordered() {
        let events = golden_events().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["type"], "assistant.turn_start");
        assert_eq!(events[3]["type"], "session.idle");
    }
}
