#![allow(clippy::missing_errors_doc)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use app_model::{ApplyOutcome, DomainEvent, SessionMetadata, SessionSnapshot, SessionStatus};
use copilot_provider::{
    AgentProvider, ProviderCompatibility, ProviderError, ProviderEvent, ProviderSession,
    SessionRequest,
};
use diagnostics::{DiagnosticEvent, DiagnosticsSink};
use serde_json::{Value, json};
use storage::{Storage, StorageError};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

const SNAPSHOT_INTERVAL: u64 = 50;

#[derive(Debug, Error)]
pub enum SessionManagerError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("session actor is no longer running")]
    ActorClosed,
    #[error("session not found: {0}")]
    SessionNotFound(String),
}

pub type Result<T> = std::result::Result<T, SessionManagerError>;

#[derive(Clone, Debug)]
pub struct CreateSessionRequest {
    pub project_path: PathBuf,
    pub title: String,
    pub model: Option<String>,
    pub mode: Option<String>,
}

#[derive(Clone)]
pub struct SessionHandle {
    id: String,
    command_tx: mpsc::Sender<SessionCommand>,
    snapshots: watch::Receiver<Arc<SessionSnapshot>>,
}

impl SessionHandle {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<SessionSnapshot> {
        self.snapshots.borrow().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<SessionSnapshot>> {
        self.snapshots.clone()
    }

    pub async fn send(&self, prompt: impl Into<String>) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(SessionCommand::Send {
                prompt: prompt.into(),
                response: response_tx,
            })
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?
    }

    pub async fn cancel(&self) -> Result<()> {
        self.command(SessionCommandKind::Cancel).await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.command(SessionCommandKind::Disconnect).await
    }

    async fn command(&self, kind: SessionCommandKind) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(SessionCommand::Lifecycle {
                kind,
                response: response_tx,
            })
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?
    }
}

#[derive(Default)]
pub struct RestoreReport {
    pub restored: Vec<SessionHandle>,
    pub failed: Vec<RestoreFailure>,
}

pub struct RestoreFailure {
    pub app_session_id: String,
    pub sdk_session_id: String,
    pub error: String,
}

pub struct SessionManager {
    provider: Arc<dyn AgentProvider>,
    storage: Arc<Storage>,
    diagnostics: Arc<dyn DiagnosticsSink>,
    sessions: Mutex<HashMap<String, SessionHandle>>,
}

impl SessionManager {
    #[must_use]
    pub fn new(
        provider: Arc<dyn AgentProvider>,
        storage: Arc<Storage>,
        diagnostics: Arc<dyn DiagnosticsSink>,
    ) -> Self {
        Self {
            provider,
            storage,
            diagnostics,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn start(&self) -> Result<(ProviderCompatibility, RestoreReport)> {
        let compatibility = self.provider.start().await?;
        let report = self.restore_sessions().await?;
        Ok((compatibility, report))
    }

    pub async fn stop(&self) -> Result<()> {
        let handles = self
            .sessions
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for handle in handles {
            let _ = handle.disconnect().await;
        }
        self.sessions.lock().await.clear();
        self.provider.stop().await?;
        Ok(())
    }

    pub async fn create_session(&self, request: CreateSessionRequest) -> Result<SessionHandle> {
        let provider_session = self
            .provider
            .create_session(SessionRequest {
                working_directory: request.project_path.clone(),
                model: request.model.clone(),
                mode: request.mode.clone(),
            })
            .await?;
        let now = timestamp();
        let metadata = SessionMetadata {
            id: Uuid::new_v4().to_string(),
            sdk_session_id: provider_session.sdk_session_id.clone(),
            project_path: request.project_path.to_string_lossy().into_owned(),
            title: request.title,
            model: request.model,
            mode: request.mode,
            created_at: now.clone(),
            updated_at: now,
        };
        self.storage.upsert_session(&metadata)?;
        let state = SessionSnapshot::new(metadata);
        let handle = self.spawn_actor(state, provider_session);
        self.sessions
            .lock()
            .await
            .insert(handle.id.clone(), handle.clone());
        Ok(handle)
    }

    pub async fn session(&self, app_session_id: &str) -> Result<SessionHandle> {
        self.sessions
            .lock()
            .await
            .get(app_session_id)
            .cloned()
            .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))
    }

    pub async fn sessions(&self) -> Vec<SessionHandle> {
        self.sessions.lock().await.values().cloned().collect()
    }

    async fn restore_sessions(&self) -> Result<RestoreReport> {
        let mut report = RestoreReport::default();
        for metadata in self.storage.list_sessions()? {
            match self.restore_session(metadata.clone()).await {
                Ok(handle) => {
                    self.sessions
                        .lock()
                        .await
                        .insert(handle.id.clone(), handle.clone());
                    report.restored.push(handle);
                }
                Err(error) => {
                    self.record_restore_failure(&metadata, &error);
                    report.failed.push(RestoreFailure {
                        app_session_id: metadata.id,
                        sdk_session_id: metadata.sdk_session_id,
                        error: error.to_string(),
                    });
                }
            }
        }
        Ok(report)
    }

    async fn restore_session(&self, metadata: SessionMetadata) -> Result<SessionHandle> {
        let recovered = self.storage.recover_session(&metadata.id)?;
        let mut state = recovered.state;
        state.status = SessionStatus::Recovering;
        let provider_session = self
            .provider
            .resume_session(
                &metadata.sdk_session_id,
                SessionRequest {
                    working_directory: PathBuf::from(&metadata.project_path),
                    model: metadata.model.clone(),
                    mode: metadata.mode.clone(),
                },
            )
            .await?;
        let history = match self.provider.history(&metadata.sdk_session_id).await {
            Ok(history) => history,
            Err(error) => {
                if let Err(cleanup_error) = self.provider.disconnect(&metadata.sdk_session_id).await
                {
                    self.record_restore_cleanup_failure(
                        &metadata,
                        &error.to_string(),
                        &cleanup_error.to_string(),
                    );
                }
                return Err(error.into());
            }
        };
        reconcile_history(&self.storage, &mut state, history)?;
        state.status = SessionStatus::Idle;
        self.storage.write_snapshot(&state)?;
        Ok(self.spawn_actor(state, provider_session))
    }

    fn spawn_actor(
        &self,
        state: SessionSnapshot,
        provider_session: ProviderSession,
    ) -> SessionHandle {
        let id = state.metadata.id.clone();
        let (command_tx, command_rx) = mpsc::channel(32);
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(state.clone()));
        let actor = SessionActor {
            provider: self.provider.clone(),
            storage: self.storage.clone(),
            diagnostics: self.diagnostics.clone(),
            state,
            sdk_session_id: provider_session.sdk_session_id,
            provider_events: provider_session.events,
            commands: command_rx,
            snapshots: snapshot_tx,
        };
        tokio::spawn(actor.run());
        SessionHandle {
            id,
            command_tx,
            snapshots: snapshot_rx,
        }
    }

    fn record_restore_failure(&self, metadata: &SessionMetadata, error: &SessionManagerError) {
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "restore_session".to_owned(),
            elapsed_ms: None,
            session_id: Some(metadata.id.clone()),
            success: false,
            details: json!({
                "sdkSessionId": metadata.sdk_session_id,
                "error": error.to_string()
            }),
        });
    }

    fn record_restore_cleanup_failure(
        &self,
        metadata: &SessionMetadata,
        original_error: &str,
        cleanup_error: &str,
    ) {
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "restore_cleanup".to_owned(),
            elapsed_ms: None,
            session_id: Some(metadata.id.clone()),
            success: false,
            details: json!({
                "sdkSessionId": metadata.sdk_session_id,
                "originalError": original_error,
                "cleanupError": cleanup_error
            }),
        });
    }
}

enum SessionCommand {
    Send {
        prompt: String,
        response: oneshot::Sender<Result<String>>,
    },
    Lifecycle {
        kind: SessionCommandKind,
        response: oneshot::Sender<Result<()>>,
    },
}

enum SessionCommandKind {
    Cancel,
    Disconnect,
}

struct SessionActor {
    provider: Arc<dyn AgentProvider>,
    storage: Arc<Storage>,
    diagnostics: Arc<dyn DiagnosticsSink>,
    state: SessionSnapshot,
    sdk_session_id: String,
    provider_events: mpsc::Receiver<ProviderEvent>,
    commands: mpsc::Receiver<SessionCommand>,
    snapshots: watch::Sender<Arc<SessionSnapshot>>,
}

impl SessionActor {
    async fn run(mut self) {
        loop {
            tokio::select! {
                event = self.provider_events.recv() => {
                    match event {
                        Some(ProviderEvent::Event(raw)) => self.apply_raw(&raw),
                        Some(ProviderEvent::Lagged(count)) => self.apply_raw(&json!({
                            "id": format!("lagged-{}-{count}", self.state.last_sequence + 1),
                            "type": "session.warning",
                            "data": {"message": format!("provider subscriber skipped {count} events")}
                        })),
                        Some(ProviderEvent::Closed) | None => {
                            self.handle_provider_closed().await;
                            break;
                        }
                    }
                }
                command = self.commands.recv() => {
                    match command {
                        Some(SessionCommand::Send { prompt, response }) => {
                            let result = self.provider
                                .send(&self.sdk_session_id, &prompt)
                                .await
                                .map_err(SessionManagerError::from);
                            let _ = response.send(result);
                        }
                        Some(SessionCommand::Lifecycle {
                            kind: SessionCommandKind::Cancel,
                            response,
                        }) => {
                            let result = self.provider
                                .cancel(&self.sdk_session_id)
                                .await
                                .map_err(SessionManagerError::from);
                            let _ = response.send(result);
                        }
                        Some(SessionCommand::Lifecycle {
                            kind: SessionCommandKind::Disconnect,
                            response,
                        }) => {
                            let result = self.disconnect().await;
                            let _ = response.send(result);
                            break;
                        }
                        None => {
                            let _ = self.disconnect().await;
                            break;
                        }
                    }
                }
            }
        }
    }

    fn apply_raw(&mut self, raw: &Value) {
        let sequence = self.state.last_sequence + 1;
        let event = DomainEvent::from_sdk_event_for(&self.state.metadata.id, sequence, raw);
        match self.storage.append_event(&event) {
            Ok(true) => {
                if self.state.apply(event) == ApplyOutcome::Applied {
                    let force_snapshot = self.state.status == SessionStatus::Idle
                        || self.state.status == SessionStatus::Failed
                        || self.state.last_sequence.is_multiple_of(SNAPSHOT_INTERVAL);
                    self.publish(force_snapshot);
                }
            }
            Ok(false) => tracing::debug!(event_id = event.id, "duplicate event ignored"),
            Err(error) => self.record_actor_error("append_event", &error.to_string()),
        }
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.storage.write_snapshot(&self.state)?;
        self.provider.disconnect(&self.sdk_session_id).await?;
        self.state.status = SessionStatus::Disconnected;
        self.publish(false);
        Ok(())
    }

    async fn handle_provider_closed(&mut self) {
        let message = "provider event stream closed unexpectedly";
        self.state.status = SessionStatus::Disconnected;
        self.state.last_error = Some(message.to_owned());
        self.record_actor_error("provider_stream_closed", message);
        if let Err(error) = self.provider.disconnect(&self.sdk_session_id).await {
            self.record_actor_error("provider_stream_cleanup", &error.to_string());
        }
        self.publish(true);
    }

    fn publish(&self, persist: bool) {
        if persist && let Err(error) = self.storage.write_snapshot(&self.state) {
            self.record_actor_error("write_snapshot", &error.to_string());
        }
        self.snapshots.send_replace(Arc::new(self.state.clone()));
    }

    fn record_actor_error(&self, operation: &str, error: &str) {
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_actor".to_owned(),
            operation: operation.to_owned(),
            elapsed_ms: None,
            session_id: Some(self.state.metadata.id.clone()),
            success: false,
            details: json!({"error": error}),
        });
    }
}

fn reconcile_history(
    storage: &Storage,
    state: &mut SessionSnapshot,
    history: Vec<Value>,
) -> Result<()> {
    let mut seen = state
        .activities
        .iter()
        .map(|event| event.id.clone())
        .collect::<HashSet<_>>();
    for raw in history {
        let Some(event_id) = raw.get("id").and_then(Value::as_str) else {
            continue;
        };
        if seen.contains(event_id) {
            continue;
        }
        let event =
            DomainEvent::from_sdk_event_for(&state.metadata.id, state.last_sequence + 1, &raw);
        if storage.append_event(&event)? {
            seen.insert(event.id.clone());
            let _ = state.apply(event);
        }
    }
    Ok(())
}

fn timestamp() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| "0".to_owned(),
        |duration| duration.as_millis().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use diagnostics::MemoryDiagnostics;
    use tempfile::tempdir;
    use test_harness::{FakeProvider, golden_events};

    use super::*;

    fn request(path: PathBuf) -> CreateSessionRequest {
        CreateSessionRequest {
            project_path: path,
            title: "Foundation test".to_owned(),
            model: None,
            mode: Some("interactive".to_owned()),
        }
    }

    #[tokio::test]
    async fn actor_serializes_events_and_publishes_idle_snapshot() {
        let provider = Arc::new(FakeProvider::with_script(golden_events().unwrap()));
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage, diagnostics);
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(std::env::temp_dir()))
            .await
            .unwrap();
        let mut snapshots = handle.subscribe();

        handle.send("run fixture").await.unwrap();
        snapshots
            .wait_for(|snapshot| snapshot.status == SessionStatus::Idle)
            .await
            .unwrap();

        let snapshot = snapshots.borrow().clone();
        assert_eq!(snapshot.activities.len(), 4);
        assert_eq!(snapshot.last_sequence, 4);
        assert_eq!(snapshot.status, SessionStatus::Idle);
    }

    #[tokio::test]
    async fn restart_resumes_and_reconciles_without_duplicates() {
        let directory = tempdir().unwrap();
        let database_path = directory.path().join("gcabb.db");
        let provider = Arc::new(FakeProvider::with_script(golden_events().unwrap()));
        let diagnostics = Arc::new(MemoryDiagnostics::default());

        let storage = Arc::new(Storage::open(&database_path).unwrap());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics.clone());
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        let app_session_id = handle.id().to_owned();
        let mut snapshots = handle.subscribe();
        handle.send("run fixture").await.unwrap();
        snapshots
            .wait_for(|snapshot| snapshot.status == SessionStatus::Idle)
            .await
            .unwrap();
        handle.disconnect().await.unwrap();
        drop(manager);

        let reopened = Arc::new(Storage::open(&database_path).unwrap());
        let restored_manager = SessionManager::new(provider, reopened, diagnostics);
        let (_, report) = restored_manager.start().await.unwrap();

        assert!(report.failed.is_empty());
        assert_eq!(report.restored.len(), 1);
        let restored = restored_manager.session(&app_session_id).await.unwrap();
        assert_eq!(restored.snapshot().activities.len(), 4);
        assert_eq!(restored.snapshot().last_sequence, 4);
    }

    #[tokio::test]
    async fn resume_failure_is_reported_without_hiding_other_metadata() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        provider.start().await.unwrap();
        provider.fail_resumes(true);
        let metadata = SessionMetadata {
            id: "app-session".to_owned(),
            sdk_session_id: "missing-sdk-session".to_owned(),
            project_path: std::env::temp_dir().to_string_lossy().into_owned(),
            title: "Broken restore".to_owned(),
            model: None,
            mode: None,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        storage.upsert_session(&metadata).unwrap();
        let manager = SessionManager::new(provider, storage, diagnostics.clone());

        let (_, report) = manager.start().await.unwrap();

        assert!(report.restored.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(diagnostics.events().len(), 1);
    }

    #[tokio::test]
    async fn history_failure_after_resume_disconnects_orphaned_session() {
        let directory = tempdir().unwrap();
        let database_path = directory.path().join("gcabb.db");
        let provider = Arc::new(FakeProvider::default());
        let diagnostics = Arc::new(MemoryDiagnostics::default());

        let storage = Arc::new(Storage::open(&database_path).unwrap());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics.clone());
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        handle.disconnect().await.unwrap();
        assert_eq!(provider.active_sessions().await, 0);
        provider.fail_history(true);

        let reopened = Arc::new(Storage::open(&database_path).unwrap());
        let restored_manager = SessionManager::new(provider.clone(), reopened, diagnostics);
        let (_, report) = restored_manager.start().await.unwrap();

        assert_eq!(report.failed.len(), 1);
        assert_eq!(provider.active_sessions().await, 0);
    }

    #[tokio::test]
    async fn unexpected_provider_close_is_visible_in_snapshot_and_diagnostics() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics.clone());
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(std::env::temp_dir()))
            .await
            .unwrap();
        let sdk_session_id = handle.snapshot().metadata.sdk_session_id.clone();
        let mut snapshots = handle.subscribe();

        provider.close_stream(&sdk_session_id).await;
        snapshots
            .wait_for(|snapshot| snapshot.status == SessionStatus::Disconnected)
            .await
            .unwrap();

        assert_eq!(
            snapshots.borrow().last_error.as_deref(),
            Some("provider event stream closed unexpectedly")
        );
        assert!(!diagnostics.events().is_empty());
    }
}
