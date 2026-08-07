#![allow(clippy::missing_errors_doc)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use app_model::{
    InteractionKind, InteractionRequest, InteractionResponse, ModelOption, SessionControls,
};
use async_trait::async_trait;
use diagnostics::{DiagnosticEvent, DiagnosticsSink};
use github_copilot_sdk::handler::{
    AutoModeSwitchHandler, AutoModeSwitchResponse, ElicitationHandler, ExitPlanModeHandler,
    ExitPlanModeResult, PermissionHandler, PermissionResult, UserInputHandler, UserInputResponse,
};
use github_copilot_sdk::session::Session;
use github_copilot_sdk::{
    Client, ClientOptions, ElicitationRequest, ElicitationResult, ExitPlanModeData,
    PermissionRequestData, RequestId, ResumeSessionConfig, SessionConfig, SessionId,
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
    async fn send(&self, sdk_session_id: &str, prompt: &str) -> Result<String>;
    async fn cancel(&self, sdk_session_id: &str) -> Result<()>;
    async fn history(&self, sdk_session_id: &str) -> Result<Vec<Value>>;
    async fn controls(&self, sdk_session_id: &str) -> Result<SessionControls>;
    async fn set_model(&self, sdk_session_id: &str, model: &str) -> Result<()>;
    async fn set_mode(&self, sdk_session_id: &str, mode: &str) -> Result<()>;
    async fn set_reasoning_effort(&self, sdk_session_id: &str, effort: &str) -> Result<()>;
    async fn disconnect(&self, sdk_session_id: &str) -> Result<()>;
}

#[derive(Clone)]
struct InteractionBroker {
    sender: mpsc::Sender<ProviderInteraction>,
}

impl InteractionBroker {
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
        let request = InteractionRequest {
            id: request_id.to_string(),
            session_id: session_id.to_string(),
            kind: InteractionKind::Permission,
            title: "Permission required".to_owned(),
            message: permission_message(&data),
            choices: vec!["Allow once".to_owned(), "Deny".to_owned()],
            allow_freeform: false,
            details: serde_json::to_value(&data).unwrap_or(Value::Null),
        };
        match self.request(request).await {
            Some(InteractionResponse::Approve) => PermissionResult::approve_once(),
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
    diagnostics: Arc<dyn DiagnosticsSink>,
}

impl CopilotProvider {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, diagnostics: Arc<dyn DiagnosticsSink>) -> Self {
        Self {
            root: root.into(),
            client: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
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
            .with_permission_handler(broker.clone())
            .with_elicitation_handler(broker.clone())
            .with_user_input_handler(broker.clone())
            .with_exit_plan_mode_handler(broker.clone())
            .with_auto_mode_switch_handler(broker);
        config.model.clone_from(&request.model);
        config
            .reasoning_effort
            .clone_from(&request.reasoning_effort);
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
            .with_permission_handler(broker.clone())
            .with_elicitation_handler(broker.clone())
            .with_user_input_handler(broker.clone())
            .with_exit_plan_mode_handler(broker.clone())
            .with_auto_mode_switch_handler(broker);
        config.model.clone_from(&request.model);
        config
            .reasoning_effort
            .clone_from(&request.reasoning_effort);
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
            return compatibility(&client);
        }

        let started = Instant::now();
        let mut options = ClientOptions::default();
        options.working_directory.clone_from(&self.root);
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
        let compatibility = compatibility(&client)?;
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
        let broker = Arc::new(InteractionBroker {
            sender: interaction_tx,
        });
        let session = self
            .client()
            .await?
            .create_session(Self::session_config(&request, broker))
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
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
        let broker = Arc::new(InteractionBroker {
            sender: interaction_tx,
        });
        let session = self
            .client()
            .await?
            .resume_session(Self::resume_config(sdk_session_id, &request, broker))
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        self.record(
            "resume_session",
            millis(started.elapsed().as_millis()),
            Some(sdk_session_id.to_owned()),
            true,
            json!({"workingDirectory": request.working_directory}),
        );
        Ok(self.register(session, interactions).await)
    }

    async fn send(&self, sdk_session_id: &str, prompt: &str) -> Result<String> {
        self.session(sdk_session_id)
            .await?
            .send(prompt)
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
        Ok(SessionControls {
            model: current.model_id,
            mode: serde_json::to_value(mode)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned)),
            reasoning_effort: current.reasoning_effort,
            available_models: models.list.iter().filter_map(model_option).collect(),
        })
    }

    async fn set_model(&self, sdk_session_id: &str, model: &str) -> Result<()> {
        self.session(sdk_session_id)
            .await?
            .set_model(model, None)
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
            .map_err(|error| ProviderError::Sdk(error.to_string()))
    }
}

fn compatibility(client: &Client) -> Result<ProviderCompatibility> {
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
    Ok(ProviderCompatibility {
        sdk_crate_version: SDK_CRATE_VERSION.to_owned(),
        sdk_protocol_version: github_copilot_sdk::SDK_PROTOCOL_VERSION,
        negotiated_protocol_version: negotiated,
        process_id: client.pid(),
        startup,
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
    })
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

#[must_use]
pub fn default_database_path(root: &Path) -> PathBuf {
    root.join(".gcabb").join("gcabb.db")
}
