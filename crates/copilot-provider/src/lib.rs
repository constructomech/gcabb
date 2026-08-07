#![allow(clippy::missing_errors_doc)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use diagnostics::{DiagnosticEvent, DiagnosticsSink};
use github_copilot_sdk::handler::DenyAllHandler;
use github_copilot_sdk::session::Session;
use github_copilot_sdk::{Client, ClientOptions, ResumeSessionConfig, SessionConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

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
    async fn disconnect(&self, sdk_session_id: &str) -> Result<()>;
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

    async fn register(&self, session: Session) -> ProviderSession {
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
        }
    }

    fn session_config(request: &SessionRequest) -> SessionConfig {
        let mut config = SessionConfig::default()
            .with_working_directory(&request.working_directory)
            .with_client_name("gcabb")
            .with_permission_handler(Arc::new(DenyAllHandler));
        config.model.clone_from(&request.model);
        config
    }

    fn resume_config(sdk_session_id: &str, request: &SessionRequest) -> ResumeSessionConfig {
        let mut config = ResumeSessionConfig::new(sdk_session_id.into())
            .with_working_directory(&request.working_directory)
            .with_client_name("gcabb")
            .with_permission_handler(Arc::new(DenyAllHandler));
        config.model.clone_from(&request.model);
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
        let session = self
            .client()
            .await?
            .create_session(Self::session_config(&request))
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
        Ok(self.register(session).await)
    }

    async fn resume_session(
        &self,
        sdk_session_id: &str,
        request: SessionRequest,
    ) -> Result<ProviderSession> {
        let started = Instant::now();
        let session = self
            .client()
            .await?
            .resume_session(Self::resume_config(sdk_session_id, &request))
            .await
            .map_err(|error| ProviderError::Sdk(error.to_string()))?;
        self.record(
            "resume_session",
            millis(started.elapsed().as_millis()),
            Some(sdk_session_id.to_owned()),
            true,
            json!({"workingDirectory": request.working_directory}),
        );
        Ok(self.register(session).await)
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
