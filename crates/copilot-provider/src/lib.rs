#![allow(clippy::missing_errors_doc)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app_model::{
    AgentOption, ContextWindowOption, InteractionKind, InteractionRequest, InteractionResponse,
    ModelOption, PromptAttachment, SessionControls, ToolCatalog, ToolClass, ToolDescriptor,
    ToolSource, WorkspaceConfiguration, WorkspaceResource,
};
use async_trait::async_trait;
use diagnostics::{DiagnosticEvent, DiagnosticsSink};
use github_copilot_sdk::handler::{
    AutoModeSwitchHandler, AutoModeSwitchResponse, ElicitationHandler, ExitPlanModeHandler,
    ExitPlanModeResult, PermissionHandler, PermissionResult, UserInputHandler, UserInputResponse,
};
use github_copilot_sdk::rpc::{
    AgentInfo, AgentSelectRequest, AgentsDiscoverRequest, InstructionsDiscoverRequest,
    PermissionDecision, PermissionDecisionApproveForLocation,
    PermissionDecisionApproveForLocationApproval,
    PermissionDecisionApproveForLocationApprovalCommands,
    PermissionDecisionApproveForLocationApprovalRead,
    PermissionDecisionApproveForLocationApprovalWrite, PermissionDecisionApproveForLocationKind,
    PermissionDecisionApproveForSession, PermissionDecisionApproveForSessionApproval,
    PermissionDecisionApproveForSessionApprovalCommands,
    PermissionDecisionApproveForSessionApprovalRead,
    PermissionDecisionApproveForSessionApprovalWrite, SkillsDiscoverRequest, ToolsListRequest,
};
use github_copilot_sdk::session::Session;
use github_copilot_sdk::{
    Client, ClientMode, ClientOptions, DeliveryMode, ElicitationRequest, ElicitationResult,
    ExitPlanModeData, MessageOptions, PermissionRequestData, PermissionRequestKind, RequestId,
    ResumeSessionConfig, SessionConfig, SessionId, SystemMessageConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

pub const SDK_CRATE_VERSION: &str = "1.0.9";
pub const MINIMUM_PROTOCOL_VERSION: u32 = 3;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider has not been started")]
    NotStarted,
    #[error("provider session not found: {0}")]
    SessionNotFound(String),
    #[error("provider protocol {actual} is older than required protocol {minimum}")]
    IncompatibleProtocol { actual: u32, minimum: u32 },
    #[error("Copilot SDK operation failed: {0}")]
    Sdk(String),
    #[error("provider event serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ProviderError>;

#[derive(Clone, Debug, Default)]
pub struct SessionRequest {
    pub working_directory: PathBuf,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub context_tier: Option<String>,
    /// Approve tool permissions that stay inside an isolated, GCABB-owned
    /// worktree. Requests that reach outside it are still prompted for.
    pub auto_approve_tools: bool,
}

#[derive(Clone, Debug)]
pub enum ProviderEvent {
    Event(Value),
    Lagged(u64),
    Closed,
}

pub struct ProviderSession {
    pub sdk_session_id: String,
    pub events: mpsc::Receiver<ProviderEvent>,
    pub interactions: mpsc::Receiver<ProviderInteraction>,
}

pub struct ProviderInteraction {
    pub request: InteractionRequest,
    pub response: oneshot::Sender<InteractionResponse>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StartupBreakdown {
    pub program_resolve_ms: Option<u64>,
    pub process_spawn_ms: Option<u64>,
    pub transport_setup_ms: u64,
    pub handshake_ms: u64,
    pub total_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProviderCompatibility {
    pub sdk_crate_version: String,
    pub sdk_protocol_version: u32,
    pub negotiated_protocol_version: u32,
    pub process_id: Option<u32>,
    pub startup: Option<StartupBreakdown>,
    #[serde(default)]
    pub available_modes: Vec<String>,
    #[serde(default)]
    pub available_models: Vec<ModelOption>,
}

#[async_trait]
pub trait AgentProvider: Send + Sync {
    async fn start(&self) -> Result<ProviderCompatibility>;
    async fn stop(&self) -> Result<()>;
    async fn create_session(&self, request: SessionRequest) -> Result<ProviderSession>;
    async fn resume_session(
        &self,
        sdk_session_id: &str,
        request: SessionRequest,
    ) -> Result<ProviderSession>;
    async fn send(
        &self,
        sdk_session_id: &str,
        prompt: &str,
        attachments: &[PromptAttachment],
    ) -> Result<String>;
    async fn cancel(&self, sdk_session_id: &str) -> Result<()>;
    async fn history(&self, sdk_session_id: &str) -> Result<Vec<Value>>;
    async fn controls(&self, sdk_session_id: &str) -> Result<SessionControls>;
    async fn set_model(
        &self,
        sdk_session_id: &str,
        model: &str,
        reasoning_effort: Option<&str>,
        context_tier: Option<&str>,
    ) -> Result<()>;
    async fn set_mode(&self, sdk_session_id: &str, mode: &str) -> Result<()>;
    async fn set_agent(&self, sdk_session_id: &str, agent: Option<&str>) -> Result<()>;
    async fn set_reasoning_effort(&self, sdk_session_id: &str, effort: &str) -> Result<()>;
    async fn disconnect(&self, sdk_session_id: &str) -> Result<()>;
    /// Discover user and project custom agents. The full result includes
    /// subagent-only entries because the runtime uses them as its delegation roster.
    async fn discover_configuration(
        &self,
        project_paths: &[PathBuf],
    ) -> Result<WorkspaceConfiguration>;
    /// Discover the tools the runtime advertises for `model`.
    ///
    /// Phase 3 requires proving inherited capabilities through the SDK rather
    /// than hardcoding a tool list, so this is called at session start and
    /// whenever the model changes.
    async fn discover_tools(&self, model: Option<&str>) -> Result<ToolCatalog>;
    /// Generate a short title in an isolated model turn.
    async fn generate_title(
        &self,
        prompt: &str,
        model: Option<&str>,
        working_directory: &Path,
    ) -> Result<String>;
}

#[derive(Clone)]
struct InteractionBroker {
    sender: mpsc::Sender<ProviderInteraction>,
    /// Canonical worktree root whose contents may be approved without prompting.
    /// `None` prompts for everything.
    auto_approve_root: Option<PathBuf>,
    permission_location: String,
}

impl InteractionBroker {
    fn new(sender: mpsc::Sender<ProviderInteraction>, request: &SessionRequest) -> Self {
        Self {
            sender,
            auto_approve_root: request
                .auto_approve_tools
                .then(|| resolve_root(&request.working_directory)),
            permission_location: request.working_directory.to_string_lossy().into_owned(),
        }
    }

    async fn request(&self, request: InteractionRequest) -> Option<InteractionResponse> {
        let (response, receiver) = oneshot::channel();
        if self
            .sender
            .send(ProviderInteraction { request, response })
            .await
            .is_err()
        {
            return None;
        }
        receiver.await.ok()
    }
}

#[async_trait]
impl PermissionHandler for InteractionBroker {
    async fn handle(
        &self,
        session_id: SessionId,
        request_id: RequestId,
        data: PermissionRequestData,
    ) -> PermissionResult {
        if let Some(root) = self.auto_approve_root.as_deref()
            && !data.managed_settings_enabled
            && data.managed_approval_required != Some(true)
            && permission_stays_in_worktree(&data, root)
        {
            return PermissionResult::approve_once();
        }
        let request = InteractionRequest {
            id: request_id.to_string(),
            session_id: session_id.to_string(),
            kind: InteractionKind::Permission,
            title: "Permission required".to_owned(),
            message: permission_message(&data),
            choices: permission_choices(&data),
            allow_freeform: false,
            details: serde_json::to_value(&data).unwrap_or(Value::Null),
        };
        match self.request(request).await {
            Some(InteractionResponse::Approve) => PermissionResult::approve_once(),
            Some(InteractionResponse::ApproveForSession) => permission_for_session(&data)
                .map_or_else(PermissionResult::user_not_available, PermissionResult::from),
            Some(InteractionResponse::ApproveForLocation) => {
                permission_for_location(&data, &self.permission_location)
                    .map_or_else(PermissionResult::user_not_available, PermissionResult::from)
            }
            Some(InteractionResponse::ApprovePermanently) => permission_for_domain(&data)
                .map_or_else(PermissionResult::user_not_available, PermissionResult::from),
            Some(InteractionResponse::Reject { feedback }) => PermissionResult::reject(feedback),
            _ => PermissionResult::user_not_available(),
        }
    }
}

#[async_trait]
impl ElicitationHandler for InteractionBroker {
    async fn handle(
        &self,
        session_id: SessionId,
        request_id: RequestId,
        request: ElicitationRequest,
    ) -> ElicitationResult {
        let interaction = InteractionRequest {
            id: request_id.to_string(),
            session_id: session_id.to_string(),
            kind: InteractionKind::Elicitation,
            title: "Additional information requested".to_owned(),
            message: request.message.clone(),
            choices: Vec::new(),
            allow_freeform: true,
            details: serde_json::to_value(request).unwrap_or(Value::Null),
        };
        match self.request(interaction).await {
            Some(InteractionResponse::Submit { value, .. }) => ElicitationResult {
                action: "accept".to_owned(),
                content: Some(value),
            },
            Some(InteractionResponse::Reject { .. }) => ElicitationResult {
                action: "decline".to_owned(),
                content: None,
            },
            _ => ElicitationResult {
                action: "cancel".to_owned(),
                content: None,
            },
        }
    }
}

#[async_trait]
impl UserInputHandler for InteractionBroker {
    async fn handle(
        &self,
        session_id: SessionId,
        question: String,
        choices: Option<Vec<String>>,
        allow_freeform: Option<bool>,
    ) -> Option<UserInputResponse> {
        let interaction = InteractionRequest {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            kind: InteractionKind::UserInput,
            title: "Copilot needs your input".to_owned(),
            message: question,
            choices: choices.unwrap_or_default(),
            allow_freeform: allow_freeform.unwrap_or(true),
            details: Value::Null,
        };
        match self.request(interaction).await {
            Some(InteractionResponse::Submit { value, freeform }) => {
                value.as_str().map(|answer| UserInputResponse {
                    answer: answer.to_owned(),
                    was_freeform: freeform,
                })
            }
            _ => None,
        }
    }
}

#[async_trait]
impl ExitPlanModeHandler for InteractionBroker {
    async fn handle(&self, session_id: SessionId, data: ExitPlanModeData) -> ExitPlanModeResult {
        let interaction = InteractionRequest {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            kind: InteractionKind::ExitPlanMode,
            title: "Plan ready".to_owned(),
            message: data.summary.clone(),
            choices: data.actions.clone(),
            allow_freeform: true,
            details: serde_json::to_value(data).unwrap_or(Value::Null),
        };
        match self.request(interaction).await {
            Some(InteractionResponse::Approve) => ExitPlanModeResult::default(),
            Some(InteractionResponse::Submit { value, .. }) => ExitPlanModeResult {
                approved: true,
                selected_action: value.as_str().map(str::to_owned),
                feedback: None,
            },
            Some(InteractionResponse::Reject { feedback }) => ExitPlanModeResult {
                approved: false,
                selected_action: None,
                feedback,
            },
            _ => ExitPlanModeResult {
                approved: false,
                selected_action: None,
                feedback: None,
            },
        }
    }
}

#[async_trait]
impl AutoModeSwitchHandler for InteractionBroker {
    async fn handle(
        &self,
        session_id: SessionId,
        error_code: Option<String>,
        retry_after_seconds: Option<f64>,
    ) -> AutoModeSwitchResponse {
        let interaction = InteractionRequest {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            kind: InteractionKind::AutoModeSwitch,
            title: "Switch to Auto model?".to_owned(),
            message: "The selected model is unavailable or rate limited.".to_owned(),
            choices: vec![
                "Switch once".to_owned(),
                "Always switch".to_owned(),
                "Keep current model".to_owned(),
            ],
            allow_freeform: false,
            details: json!({
                "errorCode": error_code,
                "retryAfterSeconds": retry_after_seconds
            }),
        };
        match self.request(interaction).await {
            Some(InteractionResponse::Approve) => AutoModeSwitchResponse::Yes,
            Some(InteractionResponse::Submit { value, .. }) if value.as_str() == Some("always") => {
                AutoModeSwitchResponse::YesAlways
            }
            _ => AutoModeSwitchResponse::No,
        }
    }
}

pub struct CopilotProvider {
    root: PathBuf,
    client: Mutex<Option<Client>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    selected_agents: Mutex<HashMap<String, Option<String>>>,
    diagnostics: Arc<dyn DiagnosticsSink>,
}

impl CopilotProvider {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, diagnostics: Arc<dyn DiagnosticsSink>) -> Self {
        Self {
            root: root.into(),
            client: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            selected_agents: Mutex::new(HashMap::new()),
            diagnostics,
        }
    }

    async fn client(&self) -> Result<Client> {
        self.client
            .lock()
            .await
            .clone()
            .ok_or(ProviderError::NotStarted)
    }

    async fn session(&self, sdk_session_id: &str) -> Result<Arc<Session>> {
        self.sessions
            .lock()
            .await
            .get(sdk_session_id)
            .cloned()
            .ok_or_else(|| ProviderError::SessionNotFound(sdk_session_id.to_owned()))
    }

    async fn register(
        &self,
        session: Session,
        interactions: mpsc::Receiver<ProviderInteraction>,
    ) -> ProviderSession {
        let sdk_session_id = session.id().to_string();
        let session = Arc::new(session);
        let mut subscription = session.subscribe();
        let (event_tx, event_rx) = mpsc::channel(512);
        tokio::spawn(async move {
            loop {
                match subscription.recv().await {
                    Ok(event) => match serde_json::to_value(event) {
                        Ok(raw) => {
                            if event_tx.send(ProviderEvent::Event(raw)).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            tracing::error!(%error, "failed to serialize SDK event");
                            break;
                        }
                    },
                    Err(error) => {
                        let message = error.to_string();
                        if let Some(count) = parse_lag_count(&message) {
                            if event_tx.send(ProviderEvent::Lagged(count)).await.is_err() {
                                break;
                            }
                        } else {
                            let _ = event_tx.send(ProviderEvent::Closed).await;
                            break;
                        }
                    }
                }
            }
        });
        self.sessions
            .lock()
            .await
            .insert(sdk_session_id.clone(), session);
        self.selected_agents
            .lock()
            .await
            .insert(sdk_session_id.clone(), None);
        ProviderSession {
            sdk_session_id,
            events: event_rx,
            interactions,
        }
    }

    fn session_config(request: &SessionRequest, broker: Arc<InteractionBroker>) -> SessionConfig {
        let mut config = SessionConfig::default()
            .with_working_directory(&request.working_directory)
            .with_client_name("gcabb")
            .with_enable_config_discovery(true)
            .with_enable_on_demand_instruction_discovery(true)
            .with_enable_skills(true)
            .with_permission_handler(broker.clone())
            .with_elicitation_handler(broker.clone())
            .with_user_input_handler(broker.clone())
            .with_exit_plan_mode_handler(broker.clone())
            .with_auto_mode_switch_handler(broker);
        config.skill_directories = repository_skill_directories(&request.working_directory);
        config.model.clone_from(&request.model);
        config
            .reasoning_effort
            .clone_from(&request.reasoning_effort);
        config.context_tier.clone_from(&request.context_tier);
        config
    }

    fn resume_config(
        sdk_session_id: &str,
        request: &SessionRequest,
        broker: Arc<InteractionBroker>,
    ) -> ResumeSessionConfig {
        let mut config = ResumeSessionConfig::new(sdk_session_id.into())
            .with_working_directory(&request.working_directory)
            .with_client_name("gcabb")
            .with_enable_config_discovery(true)
            .with_enable_on_demand_instruction_discovery(true)
            .with_enable_skills(true)
            .with_permission_handler(broker.clone())
            .with_elicitation_handler(broker.clone())
            .with_user_input_handler(broker.clone())
            .with_exit_plan_mode_handler(broker.clone())
            .with_auto_mode_switch_handler(broker);
        config.skill_directories = repository_skill_directories(&request.working_directory);
        config.model.clone_from(&request.model);
        config
            .reasoning_effort
            .clone_from(&request.reasoning_effort);
        config.context_tier.clone_from(&request.context_tier);
        config
    }

    fn record(
        &self,
        operation: &str,
        elapsed_ms: Option<u64>,
        session_id: Option<String>,
        success: bool,
        details: Value,
    ) {
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "copilot_provider".to_owned(),
            operation: operation.to_owned(),
            elapsed_ms,
            session_id,
            success,
            details,
        });
    }
}

#[async_trait]
impl AgentProvider for CopilotProvider {
    async fn start(&self) -> Result<ProviderCompatibility> {
        if let Some(client) = self.client.lock().await.clone() {
            return compatibility(&client).await;
        }

        let started = Instant::now();
        let mut options = ClientOptions::default();
        options.working_directory.clone_from(&self.root);
        // Pin CopilotCli mode explicitly. `ClientMode::Empty` silently strips
        // the built-in file, search, and shell tools the self-hosting loop
        // depends on, and that regression would surface as an unexplained
        // model failure rather than a configuration error.
        options.mode = ClientMode::CopilotCli;
        let client = Client::start(options).await.map_err(|error| {
            self.record(
                "start",
                millis(started.elapsed().as_millis()),
                None,
                false,
                json!({"error": error.to_string()}),
            );
            ProviderError::Sdk(error.to_string())
        })?;
        let compatibility = compatibility(&client).await?;
        self.record(
            "start",
            millis(started.elapsed().as_millis()),
            None,
            true,
            serde_json::to_value(&compatibility).unwrap_or(Value::Null),
        );
        *self.client.lock().await = Some(client);
        Ok(compatibility)
    }

    async fn stop(&self) -> Result<()> {
        let sessions = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>()
        };
        self.selected_agents.lock().await.clear();
        let mut errors = Vec::new();
        for session in sessions {
            if let Err(error) = session.disconnect().await {
                errors.push(format!("session {}: {error}", session.id()));
            }
        }
        if let Some(client) = self.client.lock().await.take()
            && let Err(error) = client.stop().await
        {
            errors.push(format!("client stop: {error}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ProviderError::Sdk(errors.join("; ")))
        }
    }

    async fn create_session(&self, request: SessionRequest) -> Result<ProviderSession> {
        let started = Instant::now();
        let (interaction_tx, interactions) = mpsc::channel(16);
        let broker = Arc::new(InteractionBroker::new(interaction_tx, &request));
        let session = self
            .client()
            .await?
            .create_session(Self::session_config(&request, broker))
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        if let Err(error) = session.rpc().skills().ensure_loaded().await {
            if let Err(disconnect_error) = session.disconnect().await {
                tracing::warn!(%disconnect_error, "failed to disconnect session after skill loading failed");
            }
            return Err(ProviderError::Sdk(format!(
                "failed to load session skills: {error}"
            )));
        }
        let sdk_session_id = session.id().to_string();
        self.record(
            "create_session",
            millis(started.elapsed().as_millis()),
            Some(sdk_session_id),
            true,
            json!({"workingDirectory": request.working_directory}),
        );
        Ok(self.register(session, interactions).await)
    }

    async fn resume_session(
        &self,
        sdk_session_id: &str,
        request: SessionRequest,
    ) -> Result<ProviderSession> {
        let started = Instant::now();
        let (interaction_tx, interactions) = mpsc::channel(16);
        let broker = Arc::new(InteractionBroker::new(interaction_tx, &request));
        let session = self
            .client()
            .await?
            .resume_session(Self::resume_config(sdk_session_id, &request, broker))
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        // The SDK's automatic resume reload is best-effort and only logs errors.
        // Reload explicitly so a stale or missing skill registry blocks the session.
        if let Err(error) = session.rpc().skills().reload().await {
            if let Err(disconnect_error) = session.disconnect().await {
                tracing::warn!(%disconnect_error, "failed to disconnect session after skill reload failed");
            }
            return Err(ProviderError::Sdk(format!(
                "failed to reload session skills: {error}"
            )));
        }
        self.record(
            "resume_session",
            millis(started.elapsed().as_millis()),
            Some(sdk_session_id.to_owned()),
            true,
            json!({"workingDirectory": request.working_directory}),
        );
        Ok(self.register(session, interactions).await)
    }

    async fn send(
        &self,
        sdk_session_id: &str,
        prompt: &str,
        attachments: &[PromptAttachment],
    ) -> Result<String> {
        self.session(sdk_session_id)
            .await?
            .send(message_options(prompt, attachments))
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))
    }

    async fn cancel(&self, sdk_session_id: &str) -> Result<()> {
        self.session(sdk_session_id)
            .await?
            .abort()
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))
    }

    async fn history(&self, sdk_session_id: &str) -> Result<Vec<Value>> {
        self.session(sdk_session_id)
            .await?
            .get_events()
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?
            .into_iter()
            .map(|event| serde_json::to_value(event).map_err(ProviderError::from))
            .collect()
    }

    async fn controls(&self, sdk_session_id: &str) -> Result<SessionControls> {
        let session = self.session(sdk_session_id).await?;
        let current = session
            .rpc()
            .model()
            .get_current()
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        let mode = session
            .rpc()
            .mode()
            .get()
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        let models = session
            .rpc()
            .model()
            .list()
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        let agents = session
            .rpc()
            .agent()
            .list()
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        let agent = self
            .selected_agents
            .lock()
            .await
            .get(sdk_session_id)
            .cloned()
            .flatten();
        Ok(SessionControls {
            model: current.model_id,
            mode: serde_json::to_value(mode)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned)),
            agent,
            reasoning_effort: current.reasoning_effort,
            context_tier: current.context_tier.as_ref().and_then(context_tier_id),
            available_models: models.list.iter().filter_map(model_option).collect(),
            available_agents: agents.agents.iter().map(agent_option).collect(),
        })
    }

    async fn set_model(
        &self,
        sdk_session_id: &str,
        model: &str,
        reasoning_effort: Option<&str>,
        context_tier: Option<&str>,
    ) -> Result<()> {
        let mut options = github_copilot_sdk::types::SetModelOptions::default();
        let mut configured = false;
        if let Some(effort) = reasoning_effort {
            options = options.with_reasoning_effort(effort);
            configured = true;
        }
        if let Some(tier) = context_tier {
            options = options.with_context_tier(context_tier_value(tier)?);
            configured = true;
        }
        self.session(sdk_session_id)
            .await?
            .set_model(model, configured.then_some(options))
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))
    }

    async fn set_mode(&self, sdk_session_id: &str, mode: &str) -> Result<()> {
        let mode = match mode {
            "interactive" => github_copilot_sdk::session_events::SessionMode::Interactive,
            "plan" => github_copilot_sdk::session_events::SessionMode::Plan,
            "autopilot" => github_copilot_sdk::session_events::SessionMode::Autopilot,
            other => {
                return Err(ProviderError::Sdk(format!(
                    "unsupported session mode: {other}"
                )));
            }
        };
        self.session(sdk_session_id)
            .await?
            .rpc()
            .mode()
            .set(github_copilot_sdk::rpc::ModeSetRequest { mode })
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))
    }

    async fn set_agent(&self, sdk_session_id: &str, agent: Option<&str>) -> Result<()> {
        let session = self.session(sdk_session_id).await?;
        match agent {
            Some(agent) => {
                session
                    .rpc()
                    .agent()
                    .select(AgentSelectRequest {
                        name: agent.to_owned(),
                    })
                    .await
                    .map_err(|error| ProviderError::Sdk(error.to_string()))?;
            }
            None => session
                .rpc()
                .agent()
                .deselect()
                .await
                .map_err(|error| ProviderError::Sdk(error.to_string()))?,
        }
        self.selected_agents
            .lock()
            .await
            .insert(sdk_session_id.to_owned(), agent.map(str::to_owned));
        Ok(())
    }

    async fn set_reasoning_effort(&self, sdk_session_id: &str, effort: &str) -> Result<()> {
        self.session(sdk_session_id)
            .await?
            .rpc()
            .model()
            .set_reasoning_effort(github_copilot_sdk::rpc::ModelSetReasoningEffortRequest {
                reasoning_effort: effort.to_owned(),
            })
            .await
            .map(|_| ())
            .map_err(|error| ProviderError::Sdk(error.to_string()))
    }

    async fn disconnect(&self, sdk_session_id: &str) -> Result<()> {
        let session = self
            .sessions
            .lock()
            .await
            .remove(sdk_session_id)
            .ok_or_else(|| ProviderError::SessionNotFound(sdk_session_id.to_owned()))?;
        session
            .disconnect()
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        self.selected_agents.lock().await.remove(sdk_session_id);
        Ok(())
    }

    async fn discover_configuration(
        &self,
        project_paths: &[PathBuf],
    ) -> Result<WorkspaceConfiguration> {
        let client = self.client().await?;
        let project_paths = (!project_paths.is_empty()).then(|| {
            project_paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        });
        let rpc = client.rpc();
        let agents_rpc = rpc.agents();
        let skills_rpc = rpc.skills();
        let instructions_rpc = rpc.instructions();
        let (agents, skills, instructions) = tokio::try_join!(
            agents_rpc.discover(AgentsDiscoverRequest {
                exclude_host_agents: None,
                project_paths: project_paths.clone(),
            }),
            skills_rpc.discover(SkillsDiscoverRequest {
                exclude_host_skills: None,
                project_paths: project_paths.clone(),
                skill_directories: None,
            }),
            instructions_rpc.discover(InstructionsDiscoverRequest {
                exclude_host_instructions: None,
                project_paths,
            }),
        )
        .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        Ok(WorkspaceConfiguration {
            agents: agents.agents.iter().map(agent_option).collect(),
            skills: skills
                .skills
                .iter()
                .filter(|skill| skill.enabled)
                .map(|skill| WorkspaceResource {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    path: skill.path.clone(),
                })
                .collect(),
            instructions: instructions
                .sources
                .iter()
                .map(|instruction| WorkspaceResource {
                    name: instruction.label.clone(),
                    description: instruction.description.clone().unwrap_or_default(),
                    path: Some(instruction.source_path.clone()),
                })
                .collect(),
            errors: skills.errors.unwrap_or_default(),
        })
    }
    async fn discover_tools(&self, model: Option<&str>) -> Result<ToolCatalog> {
        let started = Instant::now();
        let request = ToolsListRequest {
            model: model.map(str::to_owned),
        };
        let listed = self
            .client()
            .await?
            .rpc()
            .tools()
            .list(request)
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;

        let tools: Vec<ToolDescriptor> = listed.tools.into_iter().map(tool_descriptor).collect();
        self.record(
            "discover_tools",
            millis(started.elapsed().as_millis()),
            None,
            true,
            json!({
                "model": model,
                "toolCount": tools.len(),
                "tools": tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>()
            }),
        );
        Ok(ToolCatalog {
            tools,
            discovered_at: Some(timestamp()),
            error: None,
        })
    }

    async fn generate_title(
        &self,
        prompt: &str,
        model: Option<&str>,
        working_directory: &Path,
    ) -> Result<String> {
        const SYSTEM_MESSAGE: &str = "Create a concise, human-readable title for the user's task. \
            Return only the title, with no quotation marks, markdown, or punctuation at the end. \
            Use 2 to 5 words and sentence case. Do not answer or perform the task.";

        let started = Instant::now();
        let client = self.client().await?;
        let mut config = SessionConfig::default()
            .with_working_directory(working_directory)
            .with_client_name("gcabb-session-namer")
            .with_streaming(false)
            .with_available_tools(std::iter::empty::<String>())
            .with_system_message(
                SystemMessageConfig::new()
                    .with_mode("replace")
                    .with_content(SYSTEM_MESSAGE),
            );
        config.model = model.map(str::to_owned);
        let session = client
            .create_session(config)
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        let session_id = session.id().clone();
        let response = session
            .send_and_wait(
                github_copilot_sdk::MessageOptions::new(prompt)
                    .with_wait_timeout(Duration::from_secs(20)),
            )
            .await;

        if let Err(error) = session.disconnect().await {
            tracing::warn!(%error, %session_id, "failed to disconnect title generation session");
        }
        if let Err(error) = client.delete_session(&session_id).await {
            tracing::warn!(%error, %session_id, "failed to delete title generation session");
        }

        let result = response
            .map_err(|error| ProviderError::Sdk(error.to_string()))?
            .and_then(|event| {
                event
                    .data
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|title| !title.trim().is_empty())
            .ok_or_else(|| ProviderError::Sdk("title generation returned no text".to_owned()));
        self.record(
            "generate_title",
            millis(started.elapsed().as_millis()),
            None,
            result.is_ok(),
            json!({"model": model}),
        );
        result
    }
}

/// Convert an SDK tool description into the app-owned descriptor.
///
/// MCP tools are identified by a `server/tool` namespaced name, which is the
/// only signal the wire type carries about tool origin.
fn tool_descriptor(tool: github_copilot_sdk::rpc::Tool) -> ToolDescriptor {
    let source = tool
        .namespaced_name
        .as_deref()
        .and_then(|namespaced| namespaced.split_once('/'))
        .map_or(ToolSource::Builtin, |(server, _)| ToolSource::Mcp {
            server: server.to_owned(),
        });
    ToolDescriptor {
        class: ToolClass::classify(&tool.name),
        name: tool.name,
        namespaced_name: tool.namespaced_name,
        description: tool.description,
        source,
    }
}

async fn compatibility(client: &Client) -> Result<ProviderCompatibility> {
    let negotiated = client
        .protocol_version()
        .ok_or_else(|| ProviderError::Sdk("protocol negotiation did not complete".to_owned()))?;
    if negotiated < MINIMUM_PROTOCOL_VERSION {
        return Err(ProviderError::IncompatibleProtocol {
            actual: negotiated,
            minimum: MINIMUM_PROTOCOL_VERSION,
        });
    }

    let startup = client.startup_timings().map(|timings| StartupBreakdown {
        program_resolve_ms: timings.program_resolve_ms,
        process_spawn_ms: timings.process_spawn_ms,
        transport_setup_ms: timings.transport_setup_ms,
        handshake_ms: timings.handshake_ms,
        total_ms: timings.total_ms,
    });
    let available_models = match client.rpc().models().list().await {
        Ok(models) => models
            .models
            .into_iter()
            .map(|model| ModelOption {
                id: model.id,
                name: model.name,
                supported_reasoning_efforts: model.supported_reasoning_efforts.unwrap_or_default(),
                context_windows: sdk_context_windows(
                    model.billing.as_ref(),
                    model.capabilities.limits.as_ref(),
                ),
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "failed to load authenticated Copilot model catalog");
            Vec::new()
        }
    };
    let session_modes = [
        github_copilot_sdk::session_events::SessionMode::Interactive,
        github_copilot_sdk::session_events::SessionMode::Plan,
        github_copilot_sdk::session_events::SessionMode::Autopilot,
    ]
    .into_iter()
    .filter_map(|mode| {
        serde_json::to_value(mode)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
    })
    .collect();
    Ok(ProviderCompatibility {
        sdk_crate_version: SDK_CRATE_VERSION.to_owned(),
        sdk_protocol_version: github_copilot_sdk::SDK_PROTOCOL_VERSION,
        negotiated_protocol_version: negotiated,
        process_id: client.pid(),
        startup,
        available_modes: session_modes,
        available_models,
    })
}

fn permission_message(data: &PermissionRequestData) -> String {
    for path in [
        &["description"][..],
        &["request", "description"][..],
        &["request", "command"][..],
        &["command"][..],
    ] {
        let mut value = &data.extra;
        for key in path {
            value = &value[*key];
        }
        if let Some(message) = value.as_str() {
            return message.to_owned();
        }
    }
    data.kind.as_ref().map_or_else(
        || "Copilot requested permission to use a tool.".to_owned(),
        |kind| format!("Copilot requested {kind:?} permission."),
    )
}

/// Whether a permission request is confined to the isolated worktree, which is
/// the only reason auto-approval is safe. Anything reaching outside it — another
/// directory, the network, an MCP server — is prompted for even in a worktree
/// session.
fn permission_stays_in_worktree(data: &PermissionRequestData, root: &Path) -> bool {
    match data.kind {
        Some(PermissionRequestKind::Read) => {
            permission_path(data, "path").is_some_and(|path| path_stays_in_worktree(&path, root))
        }
        Some(PermissionRequestKind::Write) => permission_path(data, "fileName")
            .is_some_and(|path| path_stays_in_worktree(&path, root)),
        Some(PermissionRequestKind::Shell) => shell_stays_in_worktree(data, root),
        _ => false,
    }
}

fn shell_stays_in_worktree(data: &PermissionRequestData, root: &Path) -> bool {
    // Running outside the sandbox is exactly the case a human should see.
    if permission_bool(data, "requestSandboxBypass") {
        return false;
    }
    // Network access is not bounded by the worktree.
    if permission_value(data, "possibleUrls")
        .and_then(Value::as_array)
        .is_some_and(|urls| !urls.is_empty())
    {
        return false;
    }
    // Without the CLI's path analysis there is nothing bounding the command.
    let Some(paths) = permission_value(data, "possiblePaths").and_then(Value::as_array) else {
        return false;
    };
    paths.iter().all(|value| {
        value
            .as_str()
            .is_some_and(|path| path_stays_in_worktree(path, root))
    })
}

fn path_stays_in_worktree(raw: &str, root: &Path) -> bool {
    // An unexpanded variable or `~` cannot be resolved here, so treat it as
    // outside rather than guessing what it expands to.
    if raw.contains('$') || raw.starts_with('~') {
        return false;
    }
    let candidate = Path::new(raw);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    resolve_path(&absolute).is_some_and(|path| path.starts_with(root))
}

/// Resolves a path for containment checks, following symlinks so that `/tmp` and
/// `/private/tmp` compare equal on macOS, and so `..` cannot walk out of the
/// worktree. Paths that do not exist yet resolve through their nearest existing
/// ancestor, which keeps new files checkable.
fn resolve_path(path: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = path.canonicalize() {
        return Some(resolved);
    }
    let parent = path.parent()?;
    let name = path.file_name()?;
    Some(resolve_path(parent)?.join(name))
}

fn resolve_root(path: &Path) -> PathBuf {
    resolve_path(path).unwrap_or_else(|| path.to_owned())
}

fn permission_path(data: &PermissionRequestData, key: &str) -> Option<String> {
    permission_string(data, &[key]).or_else(|| permission_string(data, &["request", key]))
}

fn permission_value<'a>(data: &'a PermissionRequestData, key: &str) -> Option<&'a Value> {
    data.extra.get(key).or_else(|| {
        data.extra
            .get("request")
            .and_then(|request| request.get(key))
    })
}

fn permission_bool(data: &PermissionRequestData, key: &str) -> bool {
    permission_value(data, key)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn permission_choices(data: &PermissionRequestData) -> Vec<String> {
    let mut choices = vec!["Allow once".to_owned()];
    if permission_for_session(data).is_some() {
        choices.push("Allow for this session".to_owned());
    }
    if permission_for_location(data, "location").is_some() {
        choices.push("Always allow for this project".to_owned());
    }
    if permission_for_domain(data).is_some() {
        choices.push("Always allow this domain".to_owned());
    }
    choices.push("Deny".to_owned());
    choices
}

fn permission_for_session(data: &PermissionRequestData) -> Option<PermissionDecision> {
    let approval = match data.kind? {
        PermissionRequestKind::Read => PermissionDecisionApproveForSessionApproval::Read(
            PermissionDecisionApproveForSessionApprovalRead::default(),
        ),
        PermissionRequestKind::Write => PermissionDecisionApproveForSessionApproval::Write(
            PermissionDecisionApproveForSessionApprovalWrite::default(),
        ),
        PermissionRequestKind::Shell => PermissionDecisionApproveForSessionApproval::Commands(
            PermissionDecisionApproveForSessionApprovalCommands {
                command_identifiers: vec![command_identifier(data)?],
                ..Default::default()
            },
        ),
        PermissionRequestKind::Url => {
            return Some(PermissionDecision::ApproveForSession(
                PermissionDecisionApproveForSession {
                    approval: None,
                    domain: Some(permission_domain(data)?),
                    ..Default::default()
                },
            ));
        }
        _ => return None,
    };
    Some(PermissionDecision::ApproveForSession(
        PermissionDecisionApproveForSession {
            approval: Some(approval),
            domain: None,
            ..Default::default()
        },
    ))
}

fn permission_for_location(
    data: &PermissionRequestData,
    location_key: &str,
) -> Option<PermissionDecision> {
    let approval = match data.kind? {
        PermissionRequestKind::Read => PermissionDecisionApproveForLocationApproval::Read(
            PermissionDecisionApproveForLocationApprovalRead::default(),
        ),
        PermissionRequestKind::Write => PermissionDecisionApproveForLocationApproval::Write(
            PermissionDecisionApproveForLocationApprovalWrite::default(),
        ),
        PermissionRequestKind::Shell => PermissionDecisionApproveForLocationApproval::Commands(
            PermissionDecisionApproveForLocationApprovalCommands {
                command_identifiers: vec![command_identifier(data)?],
                ..Default::default()
            },
        ),
        _ => return None,
    };
    Some(PermissionDecision::ApproveForLocation(
        PermissionDecisionApproveForLocation {
            approval,
            kind: PermissionDecisionApproveForLocationKind::default(),
            location_key: location_key.to_owned(),
        },
    ))
}

fn permission_for_domain(data: &PermissionRequestData) -> Option<PermissionDecision> {
    if data.kind != Some(PermissionRequestKind::Url) {
        return None;
    }
    Some(PermissionDecision::ApprovePermanently(
        github_copilot_sdk::rpc::PermissionDecisionApprovePermanently {
            domain: permission_domain(data)?,
            ..Default::default()
        },
    ))
}

fn command_identifier(data: &PermissionRequestData) -> Option<String> {
    permission_string(data, &["commandIdentifier"])
        .or_else(|| permission_string(data, &["request", "commandIdentifier"]))
        .or_else(|| permission_string(data, &["command_identifier"]))
}

fn permission_domain(data: &PermissionRequestData) -> Option<String> {
    let url = permission_string(data, &["url"])
        .or_else(|| permission_string(data, &["request", "url"]))?;
    let authority = url
        .split_once("://")
        .map_or(url.as_str(), |(_, value)| value)
        .split(['/', '?', '#'])
        .next()?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, value)| value);
    let host = host_without_port(host)?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Trims a trailing `:port`, keeping the brackets around an IPv6 literal so its
/// inner colons are not mistaken for a port separator.
fn host_without_port(authority: &str) -> Option<&str> {
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        let host = &authority[..=end];
        return (host.len() > 2).then_some(host);
    }
    Some(
        authority
            .split_once(':')
            .map_or(authority, |(host, _)| host),
    )
}

fn permission_string(data: &PermissionRequestData, path: &[&str]) -> Option<String> {
    let mut value = &data.extra;
    for key in path {
        value = value.get(*key)?;
    }
    value.as_str().map(str::to_owned)
}

fn model_option(value: &Value) -> Option<ModelOption> {
    let id = value
        .get("id")
        .or_else(|| value.get("modelId"))
        .and_then(Value::as_str)?
        .to_owned();
    let name = value
        .get("name")
        .or_else(|| value.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let supported_reasoning_efforts = value
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .map(|efforts| {
            efforts
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(ModelOption {
        id,
        name,
        supported_reasoning_efforts,
        context_windows: json_context_windows(value),
    })
}

fn agent_option(agent: &AgentInfo) -> AgentOption {
    AgentOption {
        id: agent.id.clone(),
        name: if agent.display_name.is_empty() {
            agent.name.clone()
        } else {
            agent.display_name.clone()
        },
        description: agent.description.clone(),
        model: agent.model.clone(),
        user_invocable: agent.user_invocable,
    }
}

/// Builds the selectable context-window tiers for a model. The default tier is
/// always reported when a size is known; the extended tier is only reported for
/// models whose billing metadata carries long-context pricing.
fn context_windows(
    default_prompt_tokens: Option<i64>,
    long_context_prompt_tokens: Option<i64>,
    max_output_tokens: Option<i64>,
    max_context_window_tokens: Option<i64>,
) -> Vec<ContextWindowOption> {
    let output = max_output_tokens.unwrap_or_default();
    let default_tokens = default_prompt_tokens
        .and_then(|prompt| token_count(prompt.saturating_add(output)))
        .or_else(|| max_context_window_tokens.and_then(token_count));
    let long_tokens =
        long_context_prompt_tokens.and_then(|prompt| token_count(prompt.saturating_add(output)));
    let mut windows = Vec::new();
    if default_tokens.is_some() || long_tokens.is_some() {
        windows.push(ContextWindowOption {
            tier: "default".to_owned(),
            max_tokens: default_tokens,
        });
    }
    if let Some(max_tokens) = long_tokens
        && Some(max_tokens) != default_tokens
    {
        windows.push(ContextWindowOption {
            tier: "long_context".to_owned(),
            max_tokens: Some(max_tokens),
        });
    }
    windows
}

fn sdk_context_windows(
    billing: Option<&github_copilot_sdk::types::ModelBilling>,
    limits: Option<&github_copilot_sdk::types::ModelCapabilitiesLimits>,
) -> Vec<ContextWindowOption> {
    let prices = billing.and_then(|billing| billing.token_prices.as_ref());
    context_windows(
        prices.and_then(|prices| prices.max_prompt_tokens),
        prices
            .and_then(|prices| prices.long_context.as_ref())
            .and_then(|long| long.max_prompt_tokens),
        limits.and_then(|limits| limits.max_output_tokens),
        limits.and_then(|limits| limits.max_context_window_tokens),
    )
}

fn json_context_windows(value: &Value) -> Vec<ContextWindowOption> {
    let prices = value.pointer("/billing/tokenPrices");
    let limits = value.pointer("/capabilities/limits");
    context_windows(
        prices.and_then(|prices| json_i64(prices, "maxPromptTokens", "max_prompt_tokens")),
        prices
            .and_then(|prices| {
                prices
                    .get("longContext")
                    .or_else(|| prices.get("long_context"))
            })
            .and_then(|long| json_i64(long, "maxPromptTokens", "max_prompt_tokens")),
        limits.and_then(|limits| json_i64(limits, "maxOutputTokens", "max_output_tokens")),
        limits.and_then(|limits| {
            json_i64(
                limits,
                "maxContextWindowTokens",
                "max_context_window_tokens",
            )
        }),
    )
}

fn json_i64(value: &Value, camel: &str, snake: &str) -> Option<i64> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .and_then(Value::as_i64)
}

fn token_count(value: i64) -> Option<u64> {
    (value > 0).then(|| u64::try_from(value).unwrap_or_default())
}

fn context_tier_id(tier: &github_copilot_sdk::types::ContextTier) -> Option<String> {
    match tier {
        github_copilot_sdk::types::ContextTier::Default => Some("default".to_owned()),
        github_copilot_sdk::types::ContextTier::LongContext => Some("long_context".to_owned()),
        github_copilot_sdk::types::ContextTier::Unknown => None,
    }
}

fn context_tier_value(tier: &str) -> Result<github_copilot_sdk::types::ContextTier> {
    match tier {
        "default" => Ok(github_copilot_sdk::types::ContextTier::Default),
        "long_context" => Ok(github_copilot_sdk::types::ContextTier::LongContext),
        other => Err(ProviderError::Sdk(format!(
            "unsupported context tier: {other}"
        ))),
    }
}

fn message_options(prompt: &str, attachments: &[PromptAttachment]) -> MessageOptions {
    // Immediate delivery is a normal turn while idle and steering input while busy.
    let mut options = MessageOptions::from(prompt.to_owned()).with_mode(DeliveryMode::Immediate);
    if attachments.is_empty() {
        return options;
    }
    // Paths rather than inlined bytes: the runtime reads the file itself, so a
    // large screenshot never crosses the RPC boundary.
    options = options.with_attachments(
        attachments
            .iter()
            .map(|attachment| match attachment {
                PromptAttachment::File { path, display_name } => {
                    github_copilot_sdk::Attachment::File {
                        path: PathBuf::from(path),
                        display_name: Some(display_name.clone()),
                        line_range: None,
                    }
                }
                // A pasted image has no file to point at, so the bytes themselves travel.
                PromptAttachment::Image {
                    data,
                    mime_type,
                    display_name,
                } => github_copilot_sdk::Attachment::Blob {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                    display_name: Some(display_name.clone()),
                },
            })
            .collect(),
    );
    options
}

fn parse_lag_count(message: &str) -> Option<u64> {
    message
        .split_whitespace()
        .find_map(|part| part.parse::<u64>().ok())
}

fn millis(value: u128) -> Option<u64> {
    u64::try_from(value).ok()
}

fn timestamp() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| "0".to_owned(),
        |duration| duration.as_millis().to_string(),
    )
}

fn repository_skill_directories(working_directory: &Path) -> Option<Vec<PathBuf>> {
    let directory = working_directory.join(".github").join("skills");
    directory.is_dir().then_some(vec![directory])
}

#[must_use]
pub fn default_database_path(root: &Path) -> PathBuf {
    root.join(".gcabb").join("gcabb.db")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use github_copilot_sdk::handler::{PermissionHandler, PermissionResult};
    use github_copilot_sdk::rpc::PermissionDecision;
    use github_copilot_sdk::{
        DeliveryMode, PermissionRequestData, PermissionRequestKind, RequestId, SessionId,
    };
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    use super::{
        CopilotProvider, InteractionBroker, InteractionResponse, ProviderInteraction,
        SessionRequest, message_options, model_option, permission_choices, permission_for_domain,
        permission_for_location, permission_for_session, permission_stays_in_worktree,
        resolve_root, sdk_context_windows,
    };

    fn interaction_broker() -> Arc<InteractionBroker> {
        let (sender, _interactions) = mpsc::channel(1);
        Arc::new(InteractionBroker {
            sender,
            auto_approve_root: None,
            permission_location: std::env::temp_dir().to_string_lossy().into_owned(),
        })
    }

    #[test]
    fn session_configs_load_repository_skills() {
        let working_directory =
            std::env::temp_dir().join(format!("gcabb-skill-test-{}", uuid::Uuid::new_v4()));
        let skill_directory = working_directory.join(".github").join("skills");
        std::fs::create_dir_all(&skill_directory).expect("create repository skill directory");
        let request = SessionRequest {
            working_directory: working_directory.clone(),
            ..SessionRequest::default()
        };

        let create = CopilotProvider::session_config(&request, interaction_broker());
        let resume = CopilotProvider::resume_config("session", &request, interaction_broker());

        assert_eq!(create.enable_skills, Some(true));
        assert_eq!(resume.enable_skills, Some(true));
        assert_eq!(
            create.skill_directories.as_deref(),
            Some(std::slice::from_ref(&skill_directory))
        );
        assert_eq!(
            resume.skill_directories.as_deref(),
            Some(std::slice::from_ref(&skill_directory))
        );

        std::fs::remove_dir_all(working_directory).expect("remove test repository");
    }

    #[test]
    fn session_configs_skip_missing_repository_skill_directory() {
        let request = SessionRequest {
            working_directory: std::env::temp_dir()
                .join(format!("gcabb-no-skills-{}", uuid::Uuid::new_v4())),
            ..SessionRequest::default()
        };

        let create = CopilotProvider::session_config(&request, interaction_broker());
        let resume = CopilotProvider::resume_config("session", &request, interaction_broker());

        assert_eq!(create.skill_directories, None);
        assert_eq!(resume.skill_directories, None);
    }

    #[test]
    fn user_messages_request_immediate_delivery_for_steering() {
        let options = message_options("change direction", &[]);

        assert_eq!(options.mode, Some(DeliveryMode::Immediate));
    }

    fn worktree_broker(root: &Path) -> (InteractionBroker, mpsc::Receiver<ProviderInteraction>) {
        let (sender, interactions) = mpsc::channel(1);
        let broker = InteractionBroker {
            sender,
            auto_approve_root: Some(resolve_root(root)),
            permission_location: root.to_string_lossy().into_owned(),
        };
        (broker, interactions)
    }

    async fn decide(broker: &InteractionBroker, data: PermissionRequestData) -> PermissionResult {
        broker
            .handle(
                SessionId::from("session"),
                RequestId::new("permission"),
                data,
            )
            .await
    }

    fn approved(result: &PermissionResult) -> bool {
        matches!(
            result,
            PermissionResult::Decision(PermissionDecision::ApproveOnce(_))
        )
    }

    #[tokio::test]
    async fn reads_inside_the_worktree_are_approved_without_prompting() {
        let worktree = tempfile::tempdir().expect("worktree");
        let file = worktree.path().join("src/main.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("create dir");
        std::fs::write(&file, "fn main() {}").expect("write file");
        let (broker, mut interactions) = worktree_broker(worktree.path());

        let result = decide(
            &broker,
            PermissionRequestData {
                kind: Some(PermissionRequestKind::Read),
                extra: json!({ "path": file.to_string_lossy() }),
                ..PermissionRequestData::default()
            },
        )
        .await;

        assert!(approved(&result));
        assert!(interactions.try_recv().is_err());
    }

    #[tokio::test]
    async fn reads_outside_the_worktree_still_prompt() {
        let worktree = tempfile::tempdir().expect("worktree");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let file = elsewhere.path().join("secrets.txt");
        std::fs::write(&file, "secret").expect("write file");
        let (broker, mut interactions) = worktree_broker(worktree.path());

        let task = tokio::spawn(async move {
            decide(
                &broker,
                PermissionRequestData {
                    kind: Some(PermissionRequestKind::Read),
                    extra: json!({ "path": file.to_string_lossy() }),
                    ..PermissionRequestData::default()
                },
            )
            .await
        });

        let interaction = interactions.recv().await.expect("permission prompt");
        interaction
            .response
            .send(InteractionResponse::Reject { feedback: None })
            .expect("send response");
        assert!(!approved(&task.await.expect("join")));
    }

    #[tokio::test]
    async fn shell_commands_confined_to_the_worktree_are_approved() {
        let worktree = tempfile::tempdir().expect("worktree");
        let (broker, mut interactions) = worktree_broker(worktree.path());

        let result = decide(
            &broker,
            PermissionRequestData {
                kind: Some(PermissionRequestKind::Shell),
                extra: json!({
                    "possiblePaths": [worktree.path().join("Cargo.toml").to_string_lossy()],
                    "possibleUrls": [],
                }),
                ..PermissionRequestData::default()
            },
        )
        .await;

        assert!(approved(&result));
        assert!(interactions.try_recv().is_err());
    }

    #[tokio::test]
    async fn shell_commands_without_detected_paths_are_approved() {
        let worktree = tempfile::tempdir().expect("worktree");
        let (broker, mut interactions) = worktree_broker(worktree.path());

        let result = decide(
            &broker,
            PermissionRequestData {
                kind: Some(PermissionRequestKind::Shell),
                extra: json!({ "possiblePaths": [], "possibleUrls": [] }),
                ..PermissionRequestData::default()
            },
        )
        .await;

        assert!(approved(&result));
        assert!(interactions.try_recv().is_err());
    }

    #[test]
    fn requests_reaching_outside_the_worktree_are_not_confined() {
        let worktree = tempfile::tempdir().expect("worktree");
        let root = resolve_root(worktree.path());
        let confined = |extra: Value, kind: PermissionRequestKind| {
            permission_stays_in_worktree(
                &PermissionRequestData {
                    kind: Some(kind),
                    extra,
                    ..PermissionRequestData::default()
                },
                &root,
            )
        };

        // The reported bug: a shell command reading the user's home directory.
        assert!(!confined(
            json!({ "possiblePaths": ["/Users/someone/Documents"], "possibleUrls": [] }),
            PermissionRequestKind::Shell,
        ));
        // An unexpanded variable cannot be resolved, so it is not confined.
        assert!(!confined(
            json!({ "possiblePaths": ["$HOME/Documents"], "possibleUrls": [] }),
            PermissionRequestKind::Shell,
        ));
        // `..` must not walk out of the worktree.
        assert!(!confined(
            json!({ "possiblePaths": ["../elsewhere"], "possibleUrls": [] }),
            PermissionRequestKind::Shell,
        ));
        // Network access is not bounded by the worktree.
        assert!(!confined(
            json!({ "possiblePaths": [], "possibleUrls": [{ "url": "https://example.com" }] }),
            PermissionRequestKind::Shell,
        ));
        // Escaping the sandbox is exactly what a human should see.
        assert!(!confined(
            json!({ "possiblePaths": [], "possibleUrls": [], "requestSandboxBypass": true }),
            PermissionRequestKind::Shell,
        ));
        // Missing path analysis leaves the command unbounded.
        assert!(!confined(json!({}), PermissionRequestKind::Shell));
        // Opening a URL is never confined to the worktree.
        assert!(!confined(
            json!({ "url": "https://example.com" }),
            PermissionRequestKind::Url,
        ));
    }

    #[tokio::test]
    async fn managed_permissions_still_require_explicit_approval() {
        // A read that is confined to the worktree, so managed policy is the only
        // reason this prompts.
        let worktree = tempfile::tempdir().expect("worktree");
        let file = worktree.path().join("main.rs");
        std::fs::write(&file, "fn main() {}").expect("write file");
        let (broker, mut interactions) = worktree_broker(worktree.path());
        let task = tokio::spawn(async move {
            decide(
                &broker,
                PermissionRequestData {
                    kind: Some(PermissionRequestKind::Read),
                    managed_approval_required: Some(true),
                    extra: json!({ "path": file.to_string_lossy() }),
                    ..PermissionRequestData::default()
                },
            )
            .await
        });

        let interaction = interactions.recv().await.expect("permission prompt");
        interaction
            .response
            .send(app_model::InteractionResponse::Approve)
            .expect("permission response accepted");
        assert!(matches!(
            task.await.expect("permission handler completed"),
            PermissionResult::Decision(PermissionDecision::ApproveOnce(_))
        ));
    }

    #[test]
    fn read_permissions_offer_session_and_project_scopes() {
        let request = PermissionRequestData {
            kind: Some(PermissionRequestKind::Read),
            ..PermissionRequestData::default()
        };

        assert_eq!(
            permission_choices(&request),
            vec![
                "Allow once",
                "Allow for this session",
                "Always allow for this project",
                "Deny",
            ]
        );
        assert!(matches!(
            permission_for_session(&request),
            Some(PermissionDecision::ApproveForSession(_))
        ));
        assert!(matches!(
            permission_for_location(&request, "C:/worktree"),
            Some(PermissionDecision::ApproveForLocation(_))
        ));
    }

    #[test]
    fn shell_permissions_require_a_command_identifier_for_remembered_scopes() {
        let request = PermissionRequestData {
            kind: Some(PermissionRequestKind::Shell),
            ..PermissionRequestData::default()
        };

        assert_eq!(permission_choices(&request), vec!["Allow once", "Deny"]);
        assert!(permission_for_session(&request).is_none());
        assert!(permission_for_location(&request, "C:/worktree").is_none());
    }
    #[test]
    fn url_permissions_normalise_the_domain() {
        let domain = |url: &str| {
            permission_for_domain(&PermissionRequestData {
                kind: Some(PermissionRequestKind::Url),
                extra: json!({ "url": url }),
                ..PermissionRequestData::default()
            })
        };

        assert!(matches!(
            domain("https://Example.COM:8443/path?q=1"),
            Some(PermissionDecision::ApprovePermanently(decision)) if decision.domain == "example.com"
        ));
        assert!(matches!(
            domain("https://user:pass@example.com/path"),
            Some(PermissionDecision::ApprovePermanently(decision)) if decision.domain == "example.com"
        ));
        assert!(matches!(
            domain("http://[::1]:8080/path"),
            Some(PermissionDecision::ApprovePermanently(decision)) if decision.domain == "[::1]"
        ));
        assert!(domain("https:///path").is_none());
    }

    #[test]
    fn model_metadata_reports_both_context_windows_when_long_context_is_priced() {
        let option = model_option(&json!({
            "id": "claude-sonnet-5",
            "name": "Claude Sonnet 5",
            "capabilities": {"limits": {"max_output_tokens": 64000}},
            "billing": {"tokenPrices": {
                "maxPromptTokens": 136_000,
                "longContext": {"maxPromptTokens": 936_000}
            }}
        }))
        .unwrap();
        let windows = option.context_windows;
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].tier, "default");
        assert_eq!(windows[0].max_tokens, Some(200_000));
        assert_eq!(windows[1].tier, "long_context");
        assert_eq!(windows[1].max_tokens, Some(1_000_000));
    }

    #[test]
    fn model_metadata_reports_a_single_window_without_long_context_pricing() {
        let option = model_option(&json!({
            "id": "gpt-5.6-sol",
            "name": "GPT-5.6 Sol",
            "capabilities": {"limits": {"max_context_window_tokens": 264_000}}
        }))
        .unwrap();
        assert_eq!(option.context_windows.len(), 1);
        assert_eq!(option.context_windows[0].tier, "default");
        assert_eq!(option.context_windows[0].max_tokens, Some(264_000));
    }

    #[test]
    fn models_without_token_limits_report_no_context_windows() {
        let option = model_option(&json!({"id": "byok/local", "name": "Local"})).unwrap();
        assert!(option.context_windows.is_empty());
        assert!(sdk_context_windows(None, None).is_empty());
    }
}
