#![allow(clippy::missing_errors_doc)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use app_model::{
    InteractionRequest, InteractionResponse, PromptAttachment, SessionControls,
    ToolCatalog, ToolClass, ToolDescriptor, ToolSource,
};
use async_trait::async_trait;
use copilot_provider::{
    AgentProvider, AgentProviderFactory, DeliveryReceipt, ProviderCompatibility, ProviderError,
    ProviderEvent, ProviderInteraction, ProviderSession, QueueDeliveryRequest, Result,
    SDK_CRATE_VERSION, SessionRequest,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

pub const GOLDEN_SESSION_EVENTS: &str = include_str!("../fixtures/session-events.json");

/// Deterministic workload for transcript, streaming-output, and restore tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LargeSessionConfig {
    pub turns: usize,
    pub output_chunks_per_turn: usize,
    pub output_chunk_bytes: usize,
}

impl Default for LargeSessionConfig {
    fn default() -> Self {
        Self {
            turns: 1_000,
            output_chunks_per_turn: 8,
            output_chunk_bytes: 1_024,
        }
    }
}

/// Build a repeatable mixed transcript/tool stream without provider access.
#[must_use]
pub fn large_session_events(config: LargeSessionConfig) -> Vec<Value> {
    let events_per_turn = 5usize.saturating_add(config.output_chunks_per_turn);
    let mut events = Vec::with_capacity(
        config
            .turns
            .saturating_mul(events_per_turn)
            .saturating_add(1),
    );
    let output_chunk = "x".repeat(config.output_chunk_bytes);

    for turn in 0..config.turns {
        let message_id = format!("message-{turn}");
        let call_id = format!("call-{turn}");
        let shell_id = format!("shell-{turn}");
        events.push(json!({
            "id": format!("user-{turn}"),
            "type": "user.message",
            "data": {"content": format!("Run deterministic workload {turn}")}
        }));
        events.push(json!({
            "id": format!("message-start-{turn}"),
            "type": "assistant.message_start",
            "data": {"messageId": message_id}
        }));
        events.push(json!({
            "id": format!("message-complete-{turn}"),
            "type": "assistant.message",
            "data": {
                "messageId": message_id,
                "content": format!("## Result {turn}\n\nCompleted deterministic workload.")
            }
        }));
        events.push(json!({
            "id": format!("tool-start-{turn}"),
            "type": "tool.execution_start",
            "data": {
                "toolCallId": call_id,
                "toolName": "bash",
                "arguments": {"command": "fixture", "shellId": shell_id},
                "shellToolInfo": {"displayCommand": "fixture"}
            }
        }));
        for chunk in 0..config.output_chunks_per_turn {
            events.push(json!({
                "id": format!("tool-output-{turn}-{chunk}"),
                "type": "tool.execution_partial_result",
                "data": {"toolCallId": call_id, "partialOutput": output_chunk}
            }));
        }
        events.push(json!({
            "id": format!("tool-complete-{turn}"),
            "type": "tool.execution_complete",
            "data": {
                "toolCallId": call_id,
                "success": true,
                "result": {
                    "content": "done",
                    "contents": [{
                        "type": "shell_exit",
                        "shellId": shell_id,
                        "exitCode": 0,
                        "cwd": "/fixture",
                        "outputPreview": ""
                    }]
                }
            }
        }));
    }
    events.push(json!({
        "id": "fixture-idle",
        "type": "session.idle",
        "data": {}
    }));
    events
}

#[derive(Default)]
pub struct FakeProvider {
    working_directory: PathBuf,
    started: AtomicBool,
    process_id: AtomicU64,
    next_session: AtomicU64,
    script: Mutex<Vec<Value>>,
    history: Mutex<HashMap<String, Vec<Value>>>,
    live: Mutex<HashMap<String, mpsc::Sender<ProviderEvent>>>,
    interactions: Mutex<HashMap<String, mpsc::Sender<ProviderInteraction>>>,
    fail_resume: AtomicBool,
    fail_history: AtomicBool,
    fail_tool_discovery: AtomicBool,
    fail_title_generation: AtomicBool,
    fail_start: AtomicBool,
    fail_send: AtomicBool,
    dirty_on_send_failure: AtomicBool,
    fail_mode: AtomicBool,
    fail_stop: AtomicBool,
    generated_title: Mutex<Option<String>>,
    extra_tools: Mutex<Vec<String>>,
    omit_tools: Mutex<Vec<String>>,
    sent_attachments: Mutex<Vec<Vec<PromptAttachment>>>,
    sent_prompts: Mutex<Vec<String>>,
    /// Follow-ups handed to the fake agent, in delivery order, keyed by session.
    delivered_queue: Mutex<HashMap<String, Vec<QueueDeliveryRequest>>>,
    fail_queue_delivery: AtomicBool,
}

impl FakeProvider {
    fn with_process_id(process_id: u64, working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            process_id: AtomicU64::new(process_id),
            next_session: AtomicU64::new(process_id.saturating_mul(1_000)),
            ..Self::default()
        }
    }

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

    pub fn fail_tool_discovery(&self, fail: bool) {
        self.fail_tool_discovery.store(fail, Ordering::SeqCst);
    }

    pub fn fail_title_generation(&self, fail: bool) {
        self.fail_title_generation.store(fail, Ordering::SeqCst);
    }

    pub async fn set_generated_title(&self, title: impl Into<String>) {
        *self.generated_title.lock().await = Some(title.into());
    }

    /// Add tools beyond the built-in set, e.g. to simulate GitHub MCP.
    pub async fn add_tools(&self, names: &[&str]) {
        let mut extra = self.extra_tools.lock().await;
        extra.extend(names.iter().map(|name| (*name).to_owned()));
    }

    /// Hide built-in tools, to simulate a runtime missing a capability.
    pub async fn omit_tools(&self, names: &[&str]) {
        let mut omit = self.omit_tools.lock().await;
        omit.extend(names.iter().map(|name| (*name).to_owned()));
    }

    /// Attachments carried by each send, in order.
    pub async fn sent_attachments(&self) -> Vec<Vec<PromptAttachment>> {
        self.sent_attachments.lock().await.clone()
    }

    /// Prompts carried by each send, in order.
    pub async fn sent_prompts(&self) -> Vec<String> {
        self.sent_prompts.lock().await.clone()
    }

    /// Fail every queue delivery.
    pub fn fail_queue_delivery(&self, fail: bool) {
        self.fail_queue_delivery.store(fail, Ordering::SeqCst);
    }

    /// Follow-ups handed to the fake agent, in delivery order.
    pub async fn delivered_queue(&self, sdk_session_id: &str) -> Vec<QueueDeliveryRequest> {
        self.delivered_queue
            .lock()
            .await
            .get(sdk_session_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn tool_names(&self) -> Vec<String> {
        let omit = self.omit_tools.lock().await.clone();
        let extra = self.extra_tools.lock().await.clone();
        FAKE_BUILTIN_TOOLS
            .iter()
            .map(|name| (*name).to_owned())
            .chain(extra)
            .filter(|name| !omit.contains(name))
            .collect()
    }

    pub async fn close_stream(&self, sdk_session_id: &str) {
        self.live.lock().await.remove(sdk_session_id);
    }

    pub async fn active_sessions(&self) -> usize {
        self.live.lock().await.len()
    }

    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        u32::try_from(self.process_id.load(Ordering::SeqCst))
            .ok()
            .filter(|process_id| *process_id != 0)
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
        if self.fail_start.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk("configured start failure".to_owned()));
        }
        self.started.store(true, Ordering::SeqCst);
        Ok(ProviderCompatibility {
            sdk_crate_version: SDK_CRATE_VERSION.to_owned(),
            sdk_protocol_version: 3,
            negotiated_protocol_version: 3,
            process_id: self.process_id(),
            startup: None,
            available_modes: Vec::new(),
            available_models: Vec::new(),
        })
    }

    async fn stop(&self) -> Result<()> {
        self.started.store(false, Ordering::SeqCst);
        self.live.lock().await.clear();
        self.interactions.lock().await.clear();
        if self.fail_stop.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk("configured stop failure".to_owned()));
        }
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

    async fn send(
        &self,
        sdk_session_id: &str,
        prompt: &str,
        attachments: &[PromptAttachment],
    ) -> Result<String> {
        self.sent_prompts.lock().await.push(prompt.to_owned());
        self.sent_attachments
            .lock()
            .await
            .push(attachments.to_vec());
        if self.fail_send.load(Ordering::SeqCst) {
            if self.dirty_on_send_failure.load(Ordering::SeqCst) {
                std::fs::write(
                    self.working_directory.join("unsaved-from-failed-send.txt"),
                    "preserve me\n",
                )
                .map_err(|error| ProviderError::Sdk(error.to_string()))?;
            }
            return Err(ProviderError::Sdk("configured send failure".to_owned()));
        }
        let script = self.script.lock().await.clone();
        for event in script {
            self.emit(sdk_session_id, event).await?;
        }
        Ok(format!("message-{sdk_session_id}"))
    }

    async fn deliver_queued(
        &self,
        sdk_session_id: &str,
        request: &QueueDeliveryRequest,
    ) -> Result<DeliveryReceipt> {
        if self.fail_queue_delivery.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk("configured queue failure".to_owned()));
        }
        self.delivered_queue
            .lock()
            .await
            .entry(sdk_session_id.to_owned())
            .or_default()
            .push(request.clone());
        let message_id = self.send(sdk_session_id, &request.prompt, &[]).await?;
        Ok(DeliveryReceipt {
            message_id: Some(message_id),
        })
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
        if self.fail_mode.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk("configured mode failure".to_owned()));
        }
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

    async fn discover_tools(&self, _model: Option<&str>) -> Result<ToolCatalog> {
        if self.fail_tool_discovery.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk("tool discovery unavailable".to_owned()));
        }
        Ok(ToolCatalog {
            tools: self
                .tool_names()
                .await
                .into_iter()
                .map(descriptor)
                .collect(),
            discovered_at: Some("fake".to_owned()),
            error: None,
        })
    }

    async fn generate_title(
        &self,
        prompt: &str,
        _model: Option<&str>,
        _working_directory: &std::path::Path,
    ) -> Result<String> {
        if self.fail_title_generation.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk(
                "configured title generation failure".to_owned(),
            ));
        }
        if let Some(title) = self.generated_title.lock().await.clone() {
            return Ok(title);
        }
        Ok(prompt
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join(" "))
    }
}

#[derive(Clone, Default)]
pub struct FakeProviderFactory {
    state: Arc<FakeProviderFactoryState>,
}

#[derive(Default)]
struct FakeProviderFactoryState {
    next_provider: AtomicU64,
    providers: StdMutex<Vec<Arc<FakeProvider>>>,
    fail_start: AtomicBool,
    fail_send: AtomicBool,
    dirty_on_send_failure: AtomicBool,
    fail_mode: AtomicBool,
    fail_stop: AtomicBool,
    fail_title_generation: AtomicBool,
    generated_title: StdMutex<Option<String>>,
}

impl FakeProviderFactory {
    #[must_use]
    pub fn providers(&self) -> Vec<Arc<FakeProvider>> {
        self.state
            .providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn fail_starts(&self, fail: bool) {
        self.state.fail_start.store(fail, Ordering::SeqCst);
    }

    pub fn fail_sends(&self, fail: bool) {
        self.state.fail_send.store(fail, Ordering::SeqCst);
    }

    pub fn dirty_on_send_failure(&self, dirty: bool) {
        self.state
            .dirty_on_send_failure
            .store(dirty, Ordering::SeqCst);
    }

    pub fn fail_modes(&self, fail: bool) {
        self.state.fail_mode.store(fail, Ordering::SeqCst);
    }

    pub fn fail_stops(&self, fail: bool) {
        self.state.fail_stop.store(fail, Ordering::SeqCst);
    }

    pub fn fail_title_generation(&self, fail: bool) {
        self.state
            .fail_title_generation
            .store(fail, Ordering::SeqCst);
    }

    pub fn set_generated_title(&self, title: impl Into<String>) {
        *self
            .state
            .generated_title
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(title.into());
    }
}

#[async_trait]
impl AgentProviderFactory for FakeProviderFactory {
    async fn compatibility(&self) -> Result<ProviderCompatibility> {
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

    fn create(&self, working_directory: &Path) -> Arc<dyn AgentProvider> {
        let process_id = self.state.next_provider.fetch_add(1, Ordering::SeqCst) + 1;
        let provider = Arc::new(FakeProvider::with_process_id(
            process_id,
            working_directory.to_owned(),
        ));
        provider.fail_start.store(
            self.state.fail_start.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        provider.fail_send.store(
            self.state.fail_send.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        provider.dirty_on_send_failure.store(
            self.state.dirty_on_send_failure.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        provider.fail_mode.store(
            self.state.fail_mode.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        provider.fail_stop.store(
            self.state.fail_stop.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.state
            .providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(provider.clone());
        provider
    }

    async fn generate_title(
        &self,
        prompt: &str,
        _model: Option<&str>,
        _working_directory: &Path,
    ) -> Result<String> {
        if self.state.fail_title_generation.load(Ordering::SeqCst) {
            return Err(ProviderError::Sdk(
                "configured title generation failure".to_owned(),
            ));
        }
        Ok(self
            .state
            .generated_title
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| {
                let title = prompt
                    .split_whitespace()
                    .take(4)
                    .collect::<Vec<_>>()
                    .join(" ");
                if title.is_empty() {
                    "Generated session title".to_owned()
                } else {
                    title
                }
            }))
    }
}

/// Model-facing tool names the runtime returns from `tools.list`, used so
/// deterministic tests exercise the same catalog shape as the live runtime.
///
/// These are deliberately the `tools.list` names, not the CLI's user-facing
/// aliases: file editing arrives as a single `str_replace_editor` tool.
pub const FAKE_BUILTIN_TOOLS: &[&str] = &[
    "str_replace_editor",
    "glob",
    "grep",
    "bash",
    "read_bash",
    "stop_bash",
    "list_bash",
    "web_fetch",
    "fetch_copilot_cli_documentation",
    "task",
    "read_agent",
    "write_agent",
    "list_agents",
    "ask_user",
    "skill",
];

fn descriptor(name: String) -> ToolDescriptor {
    let source = name
        .strip_prefix("github-mcp-server-")
        .map_or(ToolSource::Builtin, |_| ToolSource::Mcp {
            server: "github-mcp-server".to_owned(),
        });
    ToolDescriptor {
        class: ToolClass::classify(&name),
        namespaced_name: match &source {
            ToolSource::Mcp { server } => Some(format!("{server}/{name}")),
            _ => None,
        },
        description: format!("Fake {name} tool"),
        name,
        source,
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

    #[test]
    fn large_fixture_has_repeatable_transcript_and_output_shape() {
        let config = LargeSessionConfig {
            turns: 3,
            output_chunks_per_turn: 2,
            output_chunk_bytes: 16,
        };
        let first = large_session_events(config);
        let second = large_session_events(config);

        assert_eq!(first, second);
        assert_eq!(first.len(), 22);
        assert_eq!(first[0]["type"], "user.message");
        assert_eq!(
            first[4]["data"]["partialOutput"].as_str().unwrap().len(),
            16
        );
        assert_eq!(first.last().unwrap()["type"], "session.idle");
    }
}
