#![allow(clippy::missing_errors_doc)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app_model::{
    ApplyOutcome, CapabilityId, CapabilityReport, CapabilityStatus, DomainEvent,
    InteractionResponse, OutputStreamKind, ProjectMetadata, PromptAttachment, QueueDelivery,
    QueueItem, QueueItemState, SessionKind, SessionMetadata, SessionSnapshot, SessionStatus,
    TitleSource, ToolCatalog,
};
use copilot_provider::{
    AgentProvider, AgentProviderFactory, ProviderCompatibility, ProviderError, ProviderEvent,
    ProviderInteraction, ProviderSession, QueueDeliveryRequest, SessionRequest,
};
use diagnostics::{DiagnosticEvent, DiagnosticsSink};
use git_service::GitService;
use serde_json::{Value, json};
use storage::{OutputRange, Storage, StorageError};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use uuid::Uuid;

const SNAPSHOT_INTERVAL: u64 = 50;
const BASE_REF_REFRESH_TTL: Duration = Duration::from_mins(5);
/// Where an archived patch is dropped when it cannot be re-applied, so
/// unarchiving never destroys the work it was holding.
const ARCHIVED_PATCH_FILE: &str = "gcabb-archived-changes.patch";

#[derive(Debug, Error)]
pub enum SessionManagerError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Git(#[from] git_service::GitError),
    #[error("session actor is no longer running")]
    ActorClosed,
    #[error("background task failed: {0}")]
    BackgroundTask(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error(
        "archived session {id} could not be restored: {error}. \
         It is still archived, and its saved work is intact."
    )]
    ArchiveRestoreFailed { id: String, error: String },
    #[error("session is already being restored: {0}")]
    SessionRestoreInProgress(String),
    #[error(
        "saved session working directory does not exist or cannot be accessed: {0}. \
         Restore the directory or delete this session."
    )]
    WorkingDirectoryUnavailable(PathBuf),
    #[error("session runtime failed: {error}; runtime cleanup failed: {cleanup_error}")]
    RuntimeCleanup {
        error: String,
        cleanup_error: String,
    },
}

pub type Result<T> = std::result::Result<T, SessionManagerError>;

/// What happened to a session's worktree when the session was deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeOutcome {
    /// The worktree and, when it was merged, its branch were removed.
    Removed {
        path: PathBuf,
        branch: Option<String>,
        branch_removed: bool,
    },
    /// The worktree still held uncommitted work and was left on disk.
    PreservedWithChanges { path: PathBuf },
    /// The directory was already gone; only the record needed cleaning up.
    AlreadyGone,
    /// Git refused to remove the worktree.
    RemovalFailed { path: PathBuf, error: String },
}

impl WorktreeOutcome {
    /// A message worth showing the user, when there is one.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        match self {
            Self::Removed { .. } | Self::AlreadyGone => None,
            Self::PreservedWithChanges { path } => Some(format!(
                "Session deleted. Its worktree has uncommitted changes and was kept at {}.",
                path.display()
            )),
            Self::RemovalFailed { path, error } => Some(format!(
                "Session deleted, but its worktree at {} could not be removed: {error}",
                path.display()
            )),
        }
    }
}

/// Result of deleting a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDeletion {
    pub id: String,
    pub worktree: Option<WorktreeOutcome>,
    /// Attachment files removed with the session.
    pub attachments_removed: usize,
    /// Whether the runtime's own state directory was removed.
    pub runtime_state_removed: bool,
}

/// What happened to a session's worktree when the session was archived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveOutcome {
    /// The worktree was captured and removed; the branch was kept.
    Captured {
        path: PathBuf,
        branch: String,
        /// Whether uncommitted work was saved as a patch.
        patch_saved: bool,
    },
    /// The directory was already gone; only the record needed writing.
    AlreadyGone,
    /// The worktree could not be reduced to a branch plus a patch, so it was
    /// left alone. The session is archived; its checkout stays on disk.
    Preserved { path: PathBuf, reason: String },
}

impl ArchiveOutcome {
    /// A message worth showing the user, when there is one.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        match self {
            Self::Captured { .. } | Self::AlreadyGone => None,
            Self::Preserved { path, reason } => Some(format!(
                "Session archived, but its worktree at {} was kept: {reason}",
                path.display()
            )),
        }
    }
}

/// Result of archiving a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionArchival {
    pub metadata: SessionMetadata,
    pub worktree: Option<ArchiveOutcome>,
}

/// What happened to a session's worktree when the session was unarchived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The worktree was rebuilt from its branch.
    Recreated {
        path: PathBuf,
        branch: String,
        /// Whether the saved patch was re-applied on top.
        patch_applied: bool,
    },
    /// The worktree was still on disk and was left as it was.
    AlreadyPresent { path: PathBuf },
    /// The worktree could not be rebuilt. The session is back, read-only,
    /// until its working directory is restored or relocated.
    Failed {
        path: PathBuf,
        error: String,
        /// Whether nothing was consumed, so the session can stay archived and
        /// the attempt be repeated.
        recoverable: bool,
    },
}

impl RestoreOutcome {
    /// A message worth showing the user, when there is one.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        match self {
            Self::AlreadyPresent { .. } => None,
            Self::Recreated {
                path,
                patch_applied,
                ..
            } => patch_applied.then(|| {
                format!(
                    "Session unarchived. Its uncommitted work was restored to {} and is staged.",
                    path.display()
                )
            }),
            Self::Failed { path, error, .. } => Some(format!(
                "Session unarchived, but its worktree at {} could not be rebuilt: {error}",
                path.display()
            )),
        }
    }
}

/// Result of unarchiving a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRestoration {
    pub metadata: SessionMetadata,
    pub worktree: Option<RestoreOutcome>,
}

/// Directories a session's files may live under.
///
/// Passed in rather than discovered so nothing is ever deleted from a location
/// the caller did not name.
#[derive(Clone, Debug, Default)]
pub struct SessionRoots {
    /// Where GCABB creates worktrees.
    pub worktrees: Option<PathBuf>,
    /// Where GCABB writes pasted images.
    pub attachments: Option<PathBuf>,
    /// Where the runtime keeps its per-session state.
    pub runtime_state: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct CreateSessionRequest {
    pub project_path: PathBuf,
    pub title: String,
    pub title_source: TitleSource,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub context_tier: Option<String>,
    /// Git ref the changes view compares against, e.g. `main`.
    pub base_ref: Option<String>,
    /// Repository the session belongs to, used to group sessions by project.
    pub repository_root: Option<String>,
    /// Whether this is a project session or a standalone chat.
    pub kind: SessionKind,
}

#[derive(Clone)]
pub struct SessionHandle {
    id: String,
    command_tx: mpsc::Sender<SessionCommand>,
    snapshots: watch::Receiver<Arc<SessionSnapshot>>,
}

impl SessionHandle {
    fn read_only(state: SessionSnapshot) -> Self {
        let id = state.metadata.id.clone();
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (_snapshot_tx, snapshots) = watch::channel(Arc::new(state));
        Self {
            id,
            command_tx,
            snapshots,
        }
    }

    /// Build a detached handle around a fixed snapshot, for UI tests.
    ///
    /// The command channel has no actor behind it, so lifecycle calls fail
    /// rather than block. This exists so view-level tests can render real
    /// session rows without starting a provider.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn for_test(snapshot: SessionSnapshot) -> Self {
        Self::read_only(snapshot)
    }

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
        self.send_with_attachments(prompt, Vec::new()).await
    }

    /// Send a prompt with files attached.
    pub async fn send_with_attachments(
        &self,
        prompt: impl Into<String>,
        attachments: Vec<PromptAttachment>,
    ) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(SessionCommand::Send {
                prompt: prompt.into(),
                attachments,
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

    pub async fn respond(
        &self,
        interaction_id: impl Into<String>,
        answer: InteractionResponse,
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(SessionCommand::Respond {
                interaction_id: interaction_id.into(),
                answer,
                response: response_tx,
            })
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?
    }

    /// Prepend an older persisted output window to the currently loaded one.
    pub async fn load_output_before(
        &self,
        kind: OutputStreamKind,
        identity: impl Into<String>,
        before_chunk: u64,
        max_chunks: u64,
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(SessionCommand::LoadOutput {
                kind,
                identity: identity.into(),
                before_chunk,
                max_chunks,
                response: response_tx,
            })
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?
    }

    /// Rename the session, updating the published snapshot and storage.
    pub async fn rename(&self, title: impl Into<String>) -> Result<()> {
        self.control(SessionControlCommand::Rename {
            title: title.into(),
            source: TitleSource::Manual,
        })
        .await
    }

    async fn apply_generated_title(&self, title: String) -> Result<()> {
        self.control(SessionControlCommand::Rename {
            title,
            source: TitleSource::Generated,
        })
        .await
    }

    pub async fn set_model(&self, model: impl Into<String>) -> Result<()> {
        self.control(SessionControlCommand::Model {
            model: model.into(),
            reasoning_effort: ReasoningEffortUpdate::Preserve,
            context_tier: None,
        })
        .await
    }

    pub async fn set_model_with_reasoning_effort(
        &self,
        model: impl Into<String>,
        reasoning_effort: Option<String>,
    ) -> Result<()> {
        self.control(SessionControlCommand::Model {
            model: model.into(),
            reasoning_effort: ReasoningEffortUpdate::Set(reasoning_effort),
            context_tier: None,
        })
        .await
    }

    pub async fn set_model_with_options(
        &self,
        model: impl Into<String>,
        reasoning_effort: Option<String>,
        context_tier: Option<String>,
    ) -> Result<()> {
        self.control(SessionControlCommand::Model {
            model: model.into(),
            reasoning_effort: ReasoningEffortUpdate::Set(reasoning_effort),
            context_tier,
        })
        .await
    }

    pub async fn set_context_tier(&self, tier: impl Into<String>) -> Result<()> {
        self.control(SessionControlCommand::ContextTier(tier.into()))
            .await
    }

    pub async fn set_mode(&self, mode: impl Into<String>) -> Result<()> {
        self.control(SessionControlCommand::Mode(mode.into())).await
    }

    pub async fn set_reasoning_effort(&self, effort: impl Into<String>) -> Result<()> {
        self.control(SessionControlCommand::ReasoningEffort(effort.into()))
            .await
    }

    pub async fn set_base_ref(&self, base_ref: impl Into<String>) -> Result<()> {
        self.control(SessionControlCommand::BaseRef(base_ref.into()))
            .await
    }

    pub async fn refresh_changes(&self, force: bool) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(SessionCommand::RefreshChanges {
                force,
                response: response_tx,
            })
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?
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

    async fn control(&self, control: SessionControlCommand) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(SessionCommand::Control {
                control,
                response: response_tx,
            })
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?
    }

    async fn queue(&self, command: QueueCommand) -> Result<Option<String>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(SessionCommand::Queue {
                command,
                response: response_tx,
            })
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::ActorClosed)?
    }

    /// Add a prompt to the end of the queue, returning its identifier.
    pub async fn enqueue(&self, prompt: impl Into<String>) -> Result<String> {
        self.enqueue_with(prompt, None, QueueDelivery::WhenIdle)
            .await
    }

    /// Add a prompt to the queue with an explicit delivery mode.
    pub async fn enqueue_with(
        &self,
        prompt: impl Into<String>,
        display_prompt: Option<String>,
        delivery: QueueDelivery,
    ) -> Result<String> {
        self.queue(QueueCommand::Enqueue {
            prompt: prompt.into(),
            display_prompt,
            delivery,
        })
        .await?
        .ok_or(SessionManagerError::ActorClosed)
    }

    /// Edit a queued prompt that has not been delivered.
    pub async fn update_queued(
        &self,
        id: impl Into<String>,
        prompt: impl Into<String>,
        display_prompt: Option<String>,
    ) -> Result<()> {
        self.queue(QueueCommand::UpdateText {
            id: id.into(),
            prompt: prompt.into(),
            display_prompt,
        })
        .await
        .map(|_| ())
    }

    /// Remove a queued prompt.
    pub async fn remove_queued(&self, id: impl Into<String>) -> Result<()> {
        self.queue(QueueCommand::Remove { id: id.into() })
            .await
            .map(|_| ())
    }

    /// Reorder the queue to match the given identifiers.
    pub async fn reorder_queue(&self, ordered_ids: Vec<String>) -> Result<()> {
        self.queue(QueueCommand::Reorder { ordered_ids })
            .await
            .map(|_| ())
    }

    /// Suspend or resume draining.
    pub async fn set_queue_paused(&self, paused: bool) -> Result<()> {
        self.queue(QueueCommand::SetPaused { paused })
            .await
            .map(|_| ())
    }

    /// Drop every pending item from the queue.
    pub async fn clear_queue(&self) -> Result<()> {
        self.queue(QueueCommand::Clear).await.map(|_| ())
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

#[derive(Clone)]
struct SessionRuntime {
    handle: SessionHandle,
    provider: Option<Arc<dyn AgentProvider>>,
    isolated: bool,
}

struct RestoreTiming {
    started: Instant,
    recovery_ms: u64,
    replayed_events: usize,
}

pub struct SessionManager {
    provider_factory: Arc<dyn AgentProviderFactory>,
    storage: Arc<Storage>,
    diagnostics: Arc<dyn DiagnosticsSink>,
    lifecycle: Mutex<()>,
    restoring: Mutex<HashSet<String>>,
    sessions: Mutex<HashMap<String, SessionRuntime>>,
    roots: SessionRoots,
}

impl SessionManager {
    #[must_use]
    pub fn new<F>(
        provider_factory: F,
        storage: Arc<Storage>,
        diagnostics: Arc<dyn DiagnosticsSink>,
    ) -> Self
    where
        F: AgentProviderFactory + 'static,
    {
        Self {
            provider_factory: Arc::new(provider_factory),
            storage,
            diagnostics,
            lifecycle: Mutex::new(()),
            restoring: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
            roots: SessionRoots::default(),
        }
    }

    #[must_use]
    pub fn with_session_roots(mut self, roots: SessionRoots) -> Self {
        self.roots = roots;
        self
    }

    pub async fn start(&self) -> Result<(ProviderCompatibility, RestoreReport)> {
        let selected_session = self.selected_session()?;
        self.start_with_restore_updates(selected_session.as_deref(), |_| {})
            .await
    }

    /// Start the provider and restore sessions, publishing each usable handle
    /// as soon as it is ready. The preferred session is restored first.
    pub async fn start_with_restore_updates(
        &self,
        preferred_session: Option<&str>,
        mut on_restored: impl FnMut(SessionHandle),
    ) -> Result<(ProviderCompatibility, RestoreReport)> {
        let started = Instant::now();
        let provider_started = Instant::now();
        let compatibility = self.provider_factory.compatibility().await?;
        let provider_ms = elapsed_ms(provider_started);
        let restore_started = Instant::now();
        let report = self
            .restore_sessions(preferred_session, &mut on_restored)
            .await?;
        let restore_ms = elapsed_ms(restore_started);
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "startup".to_owned(),
            elapsed_ms: Some(elapsed_ms(started)),
            session_id: None,
            success: true,
            details: json!({
                "providerMs": provider_ms,
                "restoreMs": restore_ms,
                "restoredSessions": report.restored.len(),
                "failedSessions": report.failed.len()
            }),
        });
        Ok((compatibility, report))
    }

    /// Start the provider and restore only the preferred session.
    ///
    /// Remaining metadata is returned so the caller can hydrate it in a
    /// background task without delaying command handling for the selected
    /// session.
    pub async fn start_preferred_session(
        &self,
        preferred_session: Option<&str>,
        mut on_restored: impl FnMut(SessionHandle),
    ) -> Result<(ProviderCompatibility, RestoreReport, Vec<SessionMetadata>)> {
        let compatibility = self.provider_factory.compatibility().await?;
        let mut sessions = self.storage.list_sessions()?;
        let preferred = preferred_session.and_then(|id| {
            sessions
                .iter()
                .position(|metadata| metadata.id == id)
                .map(|index| sessions.remove(index))
        });
        let report = match preferred {
            Some(metadata) => {
                self.restore_metadata_sessions(vec![metadata], &mut on_restored)
                    .await
            }
            None => RestoreReport::default(),
        };
        Ok((compatibility, report, sessions))
    }

    /// Restore a known metadata set, publishing each usable handle immediately.
    pub async fn restore_remaining_sessions(
        &self,
        sessions: Vec<SessionMetadata>,
        mut on_restored: impl FnMut(SessionHandle),
    ) -> RestoreReport {
        self.restore_metadata_sessions(sessions, &mut on_restored)
            .await
    }

    pub async fn stop(&self) -> Result<()> {
        let runtimes = self
            .sessions
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut stop_error = None;
        for runtime in runtimes {
            let disconnect_result = if matches!(
                runtime.handle.snapshot().status,
                SessionStatus::Disconnected | SessionStatus::Unavailable
            ) {
                Ok(())
            } else {
                runtime.handle.disconnect().await
            };
            if let Err(error) = disconnect_result
                && stop_error.is_none()
            {
                stop_error = Some(ProviderError::Sdk(error.to_string()));
            }
            if let Some(provider) = runtime.provider
                && runtime.isolated
                && let Err(error) = provider.stop().await
                && stop_error.is_none()
            {
                stop_error = Some(error);
            }
        }
        self.sessions.lock().await.clear();
        if let Err(error) = self.provider_factory.shutdown().await
            && stop_error.is_none()
        {
            stop_error = Some(error);
        }
        stop_error.map_or(Ok(()), |error| Err(error.into()))
    }

    pub async fn create_session(&self, request: CreateSessionRequest) -> Result<SessionHandle> {
        let app_session_id = Uuid::new_v4().to_string();
        let auto_approve_tools = is_gcabb_worktree(
            request.kind,
            &request.project_path,
            request.repository_root.as_deref(),
        );
        let provider = self.provider_factory.create(&request.project_path);
        let isolated = self.provider_factory.isolates_session_runtimes();
        let compatibility = match provider.start().await {
            Ok(compatibility) => compatibility,
            Err(error) => {
                if isolated && let Err(cleanup_error) = provider.stop().await {
                    return Err(SessionManagerError::RuntimeCleanup {
                        error: error.to_string(),
                        cleanup_error: cleanup_error.to_string(),
                    });
                }
                return Err(error.into());
            }
        };
        self.record_runtime_start(&app_session_id, &compatibility);
        let result: Result<SessionHandle> = async {
            let provider_session = provider
                .create_session(SessionRequest {
                    working_directory: request.project_path.clone(),
                    model: request.model.clone(),
                    mode: request.mode.clone(),
                    reasoning_effort: request.reasoning_effort.clone(),
                    context_tier: request.context_tier.clone(),
                    auto_approve_tools,
                })
                .await?;
            let controls = match provider.controls(&provider_session.sdk_session_id).await {
                Ok(controls) => controls,
                Err(error) => {
                    let _ = provider.disconnect(&provider_session.sdk_session_id).await;
                    return Err(error.into());
                }
            };
            let now = timestamp();
            let metadata = SessionMetadata {
                id: app_session_id.clone(),
                sdk_session_id: provider_session.sdk_session_id.clone(),
                project_path: request.project_path.to_string_lossy().into_owned(),
                repository_root: request.repository_root,
                title: request.title,
                title_source: request.title_source,
                kind: request.kind,
                model: request.model,
                mode: request.mode,
                base_ref: request.base_ref,
                created_at: now.clone(),
                updated_at: now,
            };
            if let Err(error) = self.storage.upsert_session(&metadata) {
                let _ = provider.disconnect(&provider_session.sdk_session_id).await;
                return Err(error.into());
            }
            let mut state = SessionSnapshot::new(metadata);
            state.controls = controls;
            if request.reasoning_effort.is_some() {
                state.controls.reasoning_effort = request.reasoning_effort;
            }
            if request.context_tier.is_some() {
                state.controls.context_tier = request.context_tier;
            }
            // Prove inherited tool capabilities through the SDK before the first
            // prompt, so a runtime that is missing file or shell tools is visible
            // as capability state rather than as an unexplained model failure.
            self.populate_capabilities(&provider, &mut state).await;
            refresh_changes_on_start(&mut state).await;
            Ok(self.spawn_actor(provider.clone(), isolated, state, provider_session))
        }
        .await;
        match result {
            Ok(handle) => {
                self.sessions.lock().await.insert(
                    handle.id.clone(),
                    SessionRuntime {
                        handle: handle.clone(),
                        provider: Some(provider),
                        isolated,
                    },
                );
                Ok(handle)
            }
            Err(error) => {
                if isolated && let Err(cleanup_error) = provider.stop().await {
                    return Err(SessionManagerError::RuntimeCleanup {
                        error: error.to_string(),
                        cleanup_error: cleanup_error.to_string(),
                    });
                }
                Err(error)
            }
        }
    }

    pub async fn session(&self, app_session_id: &str) -> Result<SessionHandle> {
        self.sessions
            .lock()
            .await
            .get(app_session_id)
            .map(|runtime| runtime.handle.clone())
            .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))
    }

    pub async fn sessions(&self) -> Vec<SessionHandle> {
        self.sessions
            .lock()
            .await
            .values()
            .map(|runtime| runtime.handle.clone())
            .collect()
    }

    pub async fn close_session(&self, app_session_id: &str) -> Result<()> {
        let runtime = self
            .sessions
            .lock()
            .await
            .get(app_session_id)
            .cloned()
            .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))?;
        let disconnect_result = if matches!(
            runtime.handle.snapshot().status,
            SessionStatus::Disconnected | SessionStatus::Unavailable
        ) {
            Ok(())
        } else {
            runtime.handle.disconnect().await
        };
        let stop_result = if let Some(provider) = runtime.provider
            && runtime.isolated
        {
            provider.stop().await.map_err(Into::into)
        } else {
            Ok(())
        };
        self.sessions.lock().await.remove(app_session_id);
        disconnect_result.and(stop_result)
    }

    pub async fn resume_closed_session(&self, app_session_id: &str) -> Result<SessionHandle> {
        self.resume_closed_session_from_worktrees_root(
            app_session_id,
            self.roots.worktrees.as_deref(),
        )
        .await
    }

    /// Resume a session while allowing the caller to identify the managed root
    /// that owns a missing worktree.
    pub async fn resume_closed_session_from_worktrees_root(
        &self,
        app_session_id: &str,
        worktrees_root: Option<&Path>,
    ) -> Result<SessionHandle> {
        let existing = {
            let sessions = self.sessions.lock().await;
            sessions.get(app_session_id).cloned()
        };
        if let Some(runtime) = existing {
            if !matches!(
                runtime.handle.snapshot().status,
                SessionStatus::Disconnected | SessionStatus::Unavailable
            ) {
                return Ok(runtime.handle);
            }
            self.sessions.lock().await.remove(app_session_id);
            if let Some(provider) = runtime.provider
                && runtime.isolated
                && let Err(error) = provider.stop().await
            {
                tracing::warn!(%error, %app_session_id, "failed to finish stopping disconnected provider before resume");
            }
        }
        let metadata = self
            .storage
            .list_sessions()?
            .into_iter()
            .find(|metadata| metadata.id == app_session_id)
            .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))?;
        if !self.begin_restore(&metadata.id).await {
            return Err(SessionManagerError::SessionRestoreInProgress(
                app_session_id.to_owned(),
            ));
        }
        let result = async {
            let runtime = self
                .restore_session_from_worktrees_root(metadata.clone(), worktrees_root)
                .await?;
            self.install_restored_runtime(&metadata, runtime, |_| {})
                .await?
                .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))
        }
        .await;
        self.finish_restore(&metadata.id).await;
        result
    }

    /// Point a read-only session at a replacement working directory and retry it.
    pub async fn relocate_session(
        &self,
        app_session_id: &str,
        working_directory: &Path,
    ) -> Result<SessionHandle> {
        if !working_directory.is_dir() {
            return Err(SessionManagerError::WorkingDirectoryUnavailable(
                working_directory.to_owned(),
            ));
        }
        if !self.begin_restore(app_session_id).await {
            return Err(SessionManagerError::SessionRestoreInProgress(
                app_session_id.to_owned(),
            ));
        }
        let result = self
            .relocate_session_inner(app_session_id, working_directory)
            .await;
        self.finish_restore(app_session_id).await;
        result
    }

    async fn relocate_session_inner(
        &self,
        app_session_id: &str,
        working_directory: &Path,
    ) -> Result<SessionHandle> {
        let mut metadata = self
            .storage
            .list_sessions()?
            .into_iter()
            .find(|metadata| metadata.id == app_session_id)
            .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))?;
        let previous_path = metadata.project_path.clone();
        metadata.project_path = working_directory
            .canonicalize()
            .unwrap_or_else(|_| working_directory.to_owned())
            .to_string_lossy()
            .into_owned();
        self.storage.upsert_session(&metadata)?;
        let previous_runtime = self.sessions.lock().await.remove(app_session_id);
        let result = match self.restore_session(metadata.clone()).await {
            Ok(runtime) => match self
                .install_restored_runtime(&metadata, runtime, |_| {})
                .await
            {
                Ok(Some(handle)) => Ok(handle),
                Ok(None) => Err(SessionManagerError::SessionNotFound(
                    app_session_id.to_owned(),
                )),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        if result.is_err() {
            if let Some(runtime) = previous_runtime {
                self.sessions
                    .lock()
                    .await
                    .insert(app_session_id.to_owned(), runtime);
            }
            metadata.project_path = previous_path;
            self.storage.upsert_session(&metadata)?;
        }
        result
    }

    pub fn register_project(&self, project: &ProjectMetadata) -> Result<()> {
        self.storage.upsert_project(project)?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<ProjectMetadata>> {
        self.storage.list_projects().map_err(Into::into)
    }

    pub fn session_metadata(&self) -> Result<Vec<SessionMetadata>> {
        self.storage.list_sessions().map_err(Into::into)
    }

    /// Rename a session.
    ///
    /// The title is app-owned, so this updates GCABB's record without
    /// disturbing the CLI runtime's own session state.
    pub async fn rename_session(&self, app_session_id: &str, title: &str) -> Result<()> {
        let handle = self.session(app_session_id).await.ok();
        if let Some(handle) = handle {
            handle.rename(title.to_owned()).await?;
            return Ok(());
        }
        // Closed sessions have no actor, so update storage directly.
        let mut metadata = self
            .storage
            .list_sessions()?
            .into_iter()
            .find(|metadata| metadata.id == app_session_id)
            .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))?;
        title.clone_into(&mut metadata.title);
        metadata.title_source = TitleSource::Manual;
        metadata.updated_at = timestamp();
        self.storage.upsert_session(&metadata)?;
        Ok(())
    }

    /// Replace a new session's fallback title with an isolated model-generated one.
    ///
    /// The actor checks ownership again when applying the result, so a manual
    /// rename that races with generation always wins.
    pub async fn generate_session_title(&self, app_session_id: &str, prompt: &str) -> Result<()> {
        let handle = self.session(app_session_id).await?;
        let snapshot = handle.snapshot();
        if snapshot.metadata.title_source != TitleSource::Fallback {
            return Ok(());
        }
        if let Some(title) = self
            .generate_task_title(
                prompt,
                snapshot.metadata.model.as_deref(),
                Path::new(&snapshot.metadata.project_path),
            )
            .await?
        {
            handle.apply_generated_title(title).await?;
        }
        Ok(())
    }

    /// Generate the concise task title used by a new session and its branch.
    pub async fn generate_task_title(
        &self,
        prompt: &str,
        model: Option<&str>,
        working_directory: &Path,
    ) -> Result<Option<String>> {
        let generated = self
            .provider_factory
            .generate_title(prompt, model, working_directory)
            .await?;
        Ok(normalize_generated_title(&generated))
    }

    /// Delete a session, disconnecting it first when it is still live.
    ///
    /// When the session ran in a worktree GCABB created under `worktrees_root`,
    /// that worktree is removed too so deleting a session does not leave
    /// checkouts and branches behind. A worktree that still holds uncommitted
    /// work is deliberately preserved; the outcome says so rather than
    /// silently discarding it.
    pub async fn delete_session(
        &self,
        app_session_id: &str,
        roots: &SessionRoots,
    ) -> Result<SessionDeletion> {
        let _lifecycle = self.lifecycle.lock().await;
        // Archived sessions are hidden from `list_sessions`, so this looks the
        // session up directly; deleting one from the archive must still work.
        let metadata = self.storage.session_metadata(app_session_id)?;

        // Read the attachment paths before the rows are deleted, since the
        // snapshot is the only record of which files this session referenced.
        let attachments = self
            .storage
            .recover_session(app_session_id)
            .ok()
            .map(|recovered| attachment_paths(&recovered.state))
            .unwrap_or_default();

        let runtime = self.sessions.lock().await.remove(app_session_id);
        if let Some(runtime) = runtime {
            let _ = runtime.handle.disconnect().await;
            if let Some(provider) = runtime.provider
                && runtime.isolated
            {
                let _ = provider.stop().await;
            }
        }
        if self.selected_session()?.as_deref() == Some(app_session_id) {
            self.set_selected_session(None)?;
        }
        self.storage.delete_session(app_session_id)?;
        // Space is not returned to the filesystem otherwise, which is how a
        // database of deleted sessions stays as large as it ever was.
        if let Err(error) = self.storage.vacuum() {
            self.diagnostics.record(DiagnosticEvent {
                timestamp: timestamp(),
                category: "storage".to_owned(),
                operation: "vacuum".to_owned(),
                elapsed_ms: None,
                session_id: Some(app_session_id.to_owned()),
                success: false,
                details: serde_json::json!({ "error": error.to_string() }),
            });
        }

        let attachments_removed = remove_attachments(&attachments, roots.attachments.as_deref());
        let runtime_state_removed = metadata.as_ref().is_some_and(|metadata| {
            remove_runtime_state(&metadata.sdk_session_id, roots.runtime_state.as_deref())
        });

        let worktree = metadata
            .as_ref()
            .and_then(|metadata| Self::reclaim_worktree(metadata, roots.worktrees.as_deref()));
        Ok(SessionDeletion {
            id: app_session_id.to_owned(),
            worktree,
            attachments_removed,
            runtime_state_removed,
        })
    }

    /// Archive a session: keep everything it recorded, throw away its worktree.
    ///
    /// The session's events, snapshots, and output stay exactly where they
    /// were; only its visibility changes, so nothing has to be copied out and
    /// back. The worktree is reducible to a branch plus a patch of whatever was
    /// never committed, so it is captured and removed. The branch is kept --
    /// deleting it would make the archive unrestorable.
    ///
    /// A worktree that cannot be reduced that way (no branch, a detached
    /// `HEAD`, a checkout GCABB did not create) is left on disk and reported
    /// rather than destroyed. Git-ignored files are not captured and go with
    /// the directory, as they do when a session is deleted.
    pub async fn archive_session(
        &self,
        app_session_id: &str,
        roots: &SessionRoots,
    ) -> Result<SessionArchival> {
        // Restore recreates a missing managed worktree from its branch, so an
        // archive racing one could see its own removal undone. Holding the
        // restore guard keeps the two apart.
        if !self.begin_restore(app_session_id).await {
            return Err(SessionManagerError::SessionRestoreInProgress(
                app_session_id.to_owned(),
            ));
        }
        let result = self.archive_session_inner(app_session_id, roots).await;
        self.finish_restore(app_session_id).await;
        result
    }

    async fn archive_session_inner(
        &self,
        app_session_id: &str,
        roots: &SessionRoots,
    ) -> Result<SessionArchival> {
        let _lifecycle = self.lifecycle.lock().await;
        let metadata = self
            .storage
            .session_metadata(app_session_id)?
            .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))?;

        // Archiving twice must be a no-op. A second pass would find the
        // worktree already gone and overwrite the record -- and the patch that
        // record holds is the only copy of the session's uncommitted work.
        if self.storage.is_session_archived(app_session_id)? {
            return Ok(SessionArchival {
                metadata,
                worktree: None,
            });
        }

        // Disconnect first so the agent cannot write into the worktree between
        // the patch being captured and the directory being removed.
        let runtime = self.sessions.lock().await.remove(app_session_id);
        if let Some(runtime) = runtime {
            let _ = runtime.handle.disconnect().await;
            if let Some(provider) = runtime.provider
                && runtime.isolated
            {
                let _ = provider.stop().await;
            }
        }
        if self.selected_session()?.as_deref() == Some(app_session_id) {
            self.set_selected_session(None)?;
        }

        let (mut outcome, record, removable) =
            Self::capture_worktree(&metadata, roots.worktrees.as_deref());
        // The record holds the only copy of work that was never committed, so
        // it is committed to storage *before* the checkout is destroyed. If
        // this fails the worktree is still there and nothing has been lost.
        self.storage.archive_session(&record)?;
        if let Some((repository, worktree, branch)) = removable {
            // Forced because the patch in the record above already preserved
            // anything the worktree still held.
            if let Err(error) = GitService::new(&repository).force_remove_worktree(&worktree) {
                outcome = Some(ArchiveOutcome::Preserved {
                    path: worktree,
                    reason: format!("git refused to remove it: {error}"),
                });
            } else {
                outcome = Some(ArchiveOutcome::Captured {
                    path: worktree,
                    branch,
                    patch_saved: record.patch.is_some(),
                });
            }
        }
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "archive_session".to_owned(),
            elapsed_ms: None,
            session_id: Some(metadata.id.clone()),
            success: true,
            details: json!({
                "branch": record.branch,
                "patchBytes": record.patch.as_ref().map_or(0, String::len),
                "worktree": outcome.as_ref().map(|outcome| format!("{outcome:?}")),
            }),
        });
        Ok(SessionArchival {
            metadata,
            worktree: outcome,
        })
    }

    /// Decide what archiving this session's worktree entails.
    ///
    /// Returns the outcome for cases that are already settled, the record to
    /// store, and -- when the worktree really can be rebuilt later -- the
    /// repository, path, and branch needed to remove it. Nothing is deleted
    /// here; the caller removes the checkout only once the record is durable.
    fn capture_worktree(
        metadata: &SessionMetadata,
        worktrees_root: Option<&Path>,
    ) -> (
        Option<ArchiveOutcome>,
        storage::SessionArchiveRecord,
        Option<(PathBuf, PathBuf, String)>,
    ) {
        let record = |branch, head_commit, patch| storage::SessionArchiveRecord {
            session_id: metadata.id.clone(),
            archived_at: timestamp(),
            project_path: metadata.project_path.clone(),
            repository_root: metadata.repository_root.clone(),
            branch,
            head_commit,
            patch,
        };
        let bare = || (None, record(None, None, None), None);
        // Chats have no repository, and a session running in the project
        // directory is using the developer's own checkout.
        if metadata.is_chat() {
            return bare();
        }
        let Some(repository) = metadata.repository_root.as_ref() else {
            return bare();
        };
        let worktree = PathBuf::from(&metadata.project_path);
        if Path::new(repository) == worktree {
            return bare();
        }
        // Only ever remove worktrees GCABB created. Anything outside the
        // managed root belongs to the developer.
        let Some(root) = worktrees_root else {
            return bare();
        };
        if !worktree.starts_with(root) {
            return bare();
        }
        if !worktree.exists() {
            return (
                Some(ArchiveOutcome::AlreadyGone),
                record(None, None, None),
                None,
            );
        }

        let session_git = GitService::new(&worktree);
        let preserved = |reason: &str, branch, head_commit, patch| {
            (
                Some(ArchiveOutcome::Preserved {
                    path: worktree.clone(),
                    reason: reason.to_owned(),
                }),
                record(branch, head_commit, patch),
                None,
            )
        };
        let Ok(branch) = session_git.current_branch() else {
            return preserved("its branch could not be determined", None, None, None);
        };
        // A detached HEAD has no branch to rebuild the worktree from, and the
        // commits it holds would become unreachable.
        if branch == "HEAD" || branch.is_empty() {
            return preserved(
                "it is on a detached HEAD with no branch to restore from",
                None,
                None,
                None,
            );
        }
        let head_commit = session_git.head_commit().ok();
        let patch = match session_git.capture_uncommitted_patch() {
            Ok(patch) => patch,
            Err(error) => {
                return preserved(
                    &format!("its uncommitted work could not be captured: {error}"),
                    Some(branch),
                    head_commit,
                    None,
                );
            }
        };
        (
            None,
            record(Some(branch.clone()), head_commit, patch),
            Some((PathBuf::from(repository), worktree, branch)),
        )
    }

    /// Bring an archived session back and rebuild the worktree it ran in.
    ///
    /// The session data was never moved, so it becomes visible again as soon
    /// as the archive record is cleared. The worktree is recreated from the
    /// branch that was kept, and the patch taken at archive time is applied on
    /// top so uncommitted work comes back with it.
    ///
    /// The record holds the only copy of that uncommitted work, so it is
    /// cleared last. A rebuild that fails outright leaves the session archived
    /// and the record intact, so the attempt can be repeated rather than
    /// costing the user their work.
    pub async fn unarchive_session(&self, app_session_id: &str) -> Result<SessionRestoration> {
        let _lifecycle = self.lifecycle.lock().await;
        let metadata = self
            .storage
            .session_metadata(app_session_id)?
            .ok_or_else(|| SessionManagerError::SessionNotFound(app_session_id.to_owned()))?;
        let Some(record) = self.storage.session_archive(app_session_id)? else {
            // Already visible; nothing to restore.
            return Ok(SessionRestoration {
                metadata,
                worktree: None,
            });
        };
        let worktree = Self::rebuild_worktree(&record);
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "unarchive_session".to_owned(),
            elapsed_ms: None,
            session_id: Some(metadata.id.clone()),
            success: !matches!(worktree, Some(RestoreOutcome::Failed { .. })),
            details: json!({
                "branch": record.branch,
                "worktree": worktree.as_ref().map(|outcome| format!("{outcome:?}")),
            }),
        });
        match worktree {
            // Nothing was rebuilt and the patch is still the only copy, so the
            // session stays archived and the user can try again.
            Some(RestoreOutcome::Failed {
                error,
                recoverable: true,
                ..
            }) => Err(SessionManagerError::ArchiveRestoreFailed {
                id: metadata.id,
                error,
            }),
            worktree => {
                self.storage.clear_session_archive(app_session_id)?;
                Ok(SessionRestoration { metadata, worktree })
            }
        }
    }

    /// Recreate an archived worktree from its branch and saved patch.
    fn rebuild_worktree(record: &storage::SessionArchiveRecord) -> Option<RestoreOutcome> {
        let worktree = PathBuf::from(&record.project_path);
        let branch = record.branch.clone()?;
        let repository = record.repository_root.as_ref()?;
        // Something is already at the path -- most likely the checkout was
        // rebuilt behind our back. Applying the patch could conflict with
        // whatever is there, so it is written out instead of thrown away.
        if worktree.exists() {
            let Some(patch) = record.patch.as_deref() else {
                return Some(RestoreOutcome::AlreadyPresent { path: worktree });
            };
            return Some(Self::rescue_patch(
                worktree,
                patch,
                "a directory is already at the worktree path, so the saved \
                 uncommitted work was not re-applied",
            ));
        }
        let repository_git = GitService::new(repository);
        if let Err(error) = repository_git.recreate_worktree(&worktree, &branch) {
            // Nothing has been consumed, so the caller can leave the session
            // archived and the patch on record.
            return Some(RestoreOutcome::Failed {
                path: worktree,
                error: error.to_string(),
                recoverable: true,
            });
        }
        let Some(patch) = record.patch.as_deref() else {
            return Some(RestoreOutcome::Recreated {
                path: worktree,
                branch,
                patch_applied: false,
            });
        };
        if let Err(error) = GitService::new(&worktree).apply_patch(patch) {
            return Some(Self::rescue_patch(
                worktree,
                patch,
                &format!("saved uncommitted work could not be re-applied ({error})"),
            ));
        }
        Some(RestoreOutcome::Recreated {
            path: worktree,
            branch,
            patch_applied: true,
        })
    }

    /// Write a patch that could not be applied into the checkout it belongs to.
    ///
    /// The archive record is about to be cleared, so a patch that is not
    /// applied and not written out is a patch that is gone.
    fn rescue_patch(worktree: PathBuf, patch: &str, reason: &str) -> RestoreOutcome {
        let rescue = worktree.join(ARCHIVED_PATCH_FILE);
        let written = std::fs::write(&rescue, patch).is_ok();
        let error = if written {
            format!("{reason}; it was written to {}", rescue.display())
        } else {
            format!("{reason}, and it could not be written to disk either")
        };
        RestoreOutcome::Failed {
            path: worktree,
            error,
            // A rescue that did not land leaves the archive record as the only
            // copy of the work, so it must not be cleared.
            recoverable: !written,
        }
    }

    /// Archived sessions, for the settings surface that unarchives them.
    pub fn archived_sessions(&self) -> Result<Vec<storage::ArchivedSession>> {
        self.storage.list_archived_sessions().map_err(Into::into)
    }

    /// Remove the session's worktree when GCABB created it.    ///
    /// Returns what happened, or `None` when the session had no managed
    /// worktree to reclaim.
    fn reclaim_worktree(
        metadata: &SessionMetadata,
        worktrees_root: Option<&Path>,
    ) -> Option<WorktreeOutcome> {
        // Chats have no repository, and a session running in the project
        // directory is using the developer's own checkout.
        if metadata.is_chat() {
            return None;
        }
        let repository = metadata.repository_root.as_ref()?;
        let worktree = PathBuf::from(&metadata.project_path);
        if Path::new(repository) == worktree {
            return None;
        }
        // Only ever remove worktrees GCABB created. Anything outside the
        // managed root belongs to the developer.
        let root = worktrees_root?;
        if !worktree.starts_with(root) {
            return None;
        }
        if !worktree.exists() {
            return Some(WorktreeOutcome::AlreadyGone);
        }

        let session_git = GitService::new(&worktree);
        let branch = session_git.current_branch().ok();
        if !session_git.is_clean() {
            return Some(WorktreeOutcome::PreservedWithChanges { path: worktree });
        }

        let repository_git = GitService::new(repository);
        if let Err(error) = repository_git.remove_worktree(&worktree) {
            return Some(WorktreeOutcome::RemovalFailed {
                path: worktree,
                error: error.to_string(),
            });
        }
        let branch_removed = branch.as_deref().is_some_and(|branch| {
            repository_git
                .delete_branch_if_merged(branch)
                .unwrap_or(false)
        });
        Some(WorktreeOutcome::Removed {
            path: worktree,
            branch,
            branch_removed,
        })
    }

    /// Remove a project. Sessions are retained; they are associated by
    /// `repository_root` and reappear if the project is added again.
    pub fn remove_project(&self, project_id: &str) -> Result<()> {
        self.storage.remove_project(project_id)?;
        Ok(())
    }

    /// Associate sessions and projects recorded before repositories were
    /// tracked with the repository they belong to.
    ///
    /// Earlier builds registered one project per worktree, so a repository with
    /// several session worktrees appeared as several unrelated projects named
    /// after generated branch directories. `resolve` maps a worktree path to
    /// its repository root; sessions are backfilled and project rows that were
    /// really worktrees are removed.
    ///
    /// Returns the number of sessions updated.
    pub fn adopt_repository_roots(
        &self,
        resolve: impl Fn(&str) -> Option<String>,
    ) -> Result<usize> {
        let mut updated = 0;
        for mut metadata in self.storage.list_sessions()? {
            if metadata.repository_root.is_some() {
                continue;
            }
            let Some(root) = resolve(&metadata.project_path) else {
                continue;
            };
            metadata.repository_root = Some(root);
            self.storage.upsert_session(&metadata)?;
            updated += 1;
        }

        for project in self.storage.list_projects()? {
            let is_repository_root = resolve(&project.path).is_none_or(|root| root == project.path);
            if !is_repository_root {
                self.storage.remove_project(&project.id)?;
            }
        }
        Ok(updated)
    }

    pub fn set_selected_session(&self, session_id: Option<&str>) -> Result<()> {
        self.storage.set_selected_session(session_id)?;
        Ok(())
    }

    pub fn selected_session(&self) -> Result<Option<String>> {
        self.storage.selected_session().map_err(Into::into)
    }

    async fn restore_sessions(
        &self,
        preferred_session: Option<&str>,
        on_restored: &mut impl FnMut(SessionHandle),
    ) -> Result<RestoreReport> {
        let mut sessions = self.storage.list_sessions()?;
        if let Some(preferred_session) = preferred_session
            && let Some(index) = sessions
                .iter()
                .position(|metadata| metadata.id == preferred_session)
        {
            sessions.swap(0, index);
        }
        Ok(self.restore_metadata_sessions(sessions, on_restored).await)
    }

    async fn restore_metadata_sessions(
        &self,
        sessions: Vec<SessionMetadata>,
        on_restored: &mut impl FnMut(SessionHandle),
    ) -> RestoreReport {
        let mut report = RestoreReport::default();
        for metadata in sessions {
            let started = Instant::now();
            if !self.begin_restore(&metadata.id).await {
                continue;
            }
            let result = self.restore_session(metadata.clone()).await;
            match result {
                Ok(runtime) => match self
                    .install_restored_runtime(&metadata, runtime, |handle| {
                        on_restored(handle.clone());
                    })
                    .await
                {
                    Ok(Some(handle)) => {
                        report.restored.push(handle);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.record_restore_failure(&metadata, &error, elapsed_ms(started));
                        report.failed.push(RestoreFailure {
                            app_session_id: metadata.id.clone(),
                            sdk_session_id: metadata.sdk_session_id,
                            error: error.to_string(),
                        });
                    }
                },
                Err(error) => {
                    self.record_restore_failure(&metadata, &error, elapsed_ms(started));
                    report.failed.push(RestoreFailure {
                        app_session_id: metadata.id.clone(),
                        sdk_session_id: metadata.sdk_session_id,
                        error: error.to_string(),
                    });
                }
            }
            self.finish_restore(&metadata.id).await;
        }
        report
    }

    async fn begin_restore(&self, app_session_id: &str) -> bool {
        self.restoring
            .lock()
            .await
            .insert(app_session_id.to_owned())
    }

    async fn finish_restore(&self, app_session_id: &str) {
        self.restoring.lock().await.remove(app_session_id);
    }

    async fn install_restored_runtime(
        &self,
        metadata: &SessionMetadata,
        runtime: SessionRuntime,
        on_installed: impl FnOnce(&SessionHandle),
    ) -> Result<Option<SessionHandle>> {
        let (installed, keeps_runtime): (Result<Option<SessionHandle>>, bool) = {
            let _lifecycle = self.lifecycle.lock().await;
            // A session deleted or archived while it was being restored must
            // not have its runtime installed after the fact.
            let live = self
                .storage
                .session_exists(&metadata.id)
                .and_then(|exists| {
                    if exists {
                        self.storage
                            .is_session_archived(&metadata.id)
                            .map(|archived| !archived)
                    } else {
                        Ok(false)
                    }
                });
            match live {
                Ok(true) => {
                    let mut sessions = self.sessions.lock().await;
                    if let Some(existing) = sessions.get(&metadata.id) {
                        (Ok(Some(existing.handle.clone())), false)
                    } else {
                        let handle = runtime.handle.clone();
                        sessions.insert(handle.id.clone(), runtime.clone());
                        on_installed(&handle);
                        (Ok(Some(handle)), true)
                    }
                }
                Ok(false) => (Ok(None), false),
                Err(error) => (Err(error.into()), false),
            }
        };
        if !keeps_runtime {
            let _ = runtime.handle.disconnect().await;
            if let Some(provider) = runtime.provider
                && runtime.isolated
            {
                let _ = provider.stop().await;
            }
        }
        installed
    }

    /// Discover tools for the session's model and derive capability status.
    ///
    /// Discovery failure is recorded rather than fatal: the session still
    /// runs, but capabilities report `Unknown` with the underlying error so
    /// the UI can explain why the loop may not work.
    async fn populate_capabilities(
        &self,
        provider: &Arc<dyn AgentProvider>,
        state: &mut SessionSnapshot,
    ) {
        let model = state
            .controls
            .model
            .clone()
            .or_else(|| state.metadata.model.clone());
        let catalog = match provider.discover_tools(model.as_deref()).await {
            Ok(catalog) => catalog,
            Err(error) => {
                self.diagnostics.record(DiagnosticEvent {
                    timestamp: timestamp(),
                    category: "session_manager".to_owned(),
                    operation: "discover_tools".to_owned(),
                    elapsed_ms: None,
                    session_id: Some(state.metadata.id.clone()),
                    success: false,
                    details: json!({"error": error.to_string()}),
                });
                ToolCatalog {
                    error: Some(error.to_string()),
                    ..ToolCatalog::default()
                }
            }
        };
        state.capabilities = CapabilityReport::from_catalog(&catalog);
        state.tool_catalog = catalog;
    }

    async fn restore_session(&self, metadata: SessionMetadata) -> Result<SessionRuntime> {
        self.restore_session_from_worktrees_root(metadata, self.roots.worktrees.as_deref())
            .await
    }

    async fn restore_session_from_worktrees_root(
        &self,
        metadata: SessionMetadata,
        worktrees_root: Option<&Path>,
    ) -> Result<SessionRuntime> {
        let started = Instant::now();
        let recovery_started = Instant::now();
        let recovered = self.storage.recover_session(&metadata.id)?;
        let recovery_ms = elapsed_ms(recovery_started);
        let replayed_events = recovered.replayed_events;
        let mut state = recovered.state;
        state.metadata = metadata.clone();
        state.status = SessionStatus::Recovering;
        let working_directory = PathBuf::from(&metadata.project_path);
        if !working_directory.is_dir()
            && !self.recreate_managed_worktree(
                &metadata,
                &state,
                &working_directory,
                worktrees_root,
            )
        {
            return self.restore_unavailable(
                &metadata,
                state,
                &working_directory,
                started,
                recovery_ms,
                replayed_events,
            );
        }
        let provider = self.provider_factory.create(&working_directory);
        let isolated = self.provider_factory.isolates_session_runtimes();
        let compatibility = match provider.start().await {
            Ok(compatibility) => compatibility,
            Err(error) => {
                if isolated {
                    let _ = provider.stop().await;
                }
                return Err(error.into());
            }
        };
        self.record_runtime_start(&metadata.id, &compatibility);
        let timing = RestoreTiming {
            started,
            recovery_ms,
            replayed_events,
        };
        let result = self
            .restore_with_provider(
                &metadata,
                working_directory,
                state,
                provider.clone(),
                isolated,
                timing,
            )
            .await;
        if result.is_err() {
            let _ = provider.disconnect(&metadata.sdk_session_id).await;
        }
        if result.is_err()
            && isolated
            && let Err(cleanup_error) = provider.stop().await
        {
            tracing::warn!(%cleanup_error, app_session_id = %metadata.id, "failed to stop provider after session restore failed");
        }
        result
    }

    async fn restore_with_provider(
        &self,
        metadata: &SessionMetadata,
        working_directory: PathBuf,
        mut state: SessionSnapshot,
        provider: Arc<dyn AgentProvider>,
        isolated: bool,
        timing: RestoreTiming,
    ) -> Result<SessionRuntime> {
        let resume_started = Instant::now();
        let provider_session = provider
            .resume_session(
                &metadata.sdk_session_id,
                SessionRequest {
                    auto_approve_tools: is_gcabb_worktree(
                        metadata.kind,
                        &working_directory,
                        metadata.repository_root.as_deref(),
                    ),
                    working_directory,
                    model: metadata.model.clone(),
                    mode: metadata.mode.clone(),
                    reasoning_effort: state.controls.reasoning_effort.clone(),
                    context_tier: state.controls.context_tier.clone(),
                },
            )
            .await?;
        let resume_ms = elapsed_ms(resume_started);
        let history_started = Instant::now();
        let history = match provider.history(&metadata.sdk_session_id).await {
            Ok(history) => history,
            Err(error) => {
                if let Err(cleanup_error) = provider.disconnect(&metadata.sdk_session_id).await {
                    self.record_restore_cleanup_failure(
                        metadata,
                        &error.to_string(),
                        &cleanup_error.to_string(),
                    );
                }
                return Err(error.into());
            }
        };
        let history_ms = elapsed_ms(history_started);
        let history_events = history.len();
        let reconcile_started = Instant::now();
        reconcile_history(&self.storage, &mut state, history)?;
        let reconcile_ms = elapsed_ms(reconcile_started);
        let controls_started = Instant::now();
        state.controls = match provider.controls(&metadata.sdk_session_id).await {
            Ok(controls) => controls,
            Err(error) => {
                let _ = provider.disconnect(&metadata.sdk_session_id).await;
                return Err(error.into());
            }
        };
        let controls_ms = elapsed_ms(controls_started);
        state.reconcile_after_restart(&timestamp());
        let capabilities_started = Instant::now();
        self.populate_capabilities(&provider, &mut state).await;
        let capabilities_ms = elapsed_ms(capabilities_started);
        let changes_started = Instant::now();
        refresh_changes_on_start(&mut state).await;
        let changes_ms = elapsed_ms(changes_started);
        let persistence_started = Instant::now();
        self.storage.write_snapshot(&state)?;
        let persistence_ms = elapsed_ms(persistence_started);
        let handle = self.spawn_actor(provider.clone(), isolated, state, provider_session);
        self.record_restore_success(
            metadata.id.clone(),
            timing.started,
            json!({
                "storageRecoveryMs": timing.recovery_ms,
                "replayedEvents": timing.replayed_events,
                "providerResumeMs": resume_ms,
                "historyMs": history_ms,
                "historyEvents": history_events,
                "reconcileMs": reconcile_ms,
                "controlsMs": controls_ms,
                "capabilitiesMs": capabilities_ms,
                "changesMs": changes_ms,
                "persistenceMs": persistence_ms
            }),
        );
        Ok(SessionRuntime {
            handle,
            provider: Some(provider),
            isolated,
        })
    }

    fn record_restore_success(&self, session_id: String, started: Instant, details: Value) {
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "restore_session".to_owned(),
            elapsed_ms: Some(elapsed_ms(started)),
            session_id: Some(session_id),
            success: true,
            details,
        });
    }

    fn restore_unavailable(
        &self,
        metadata: &SessionMetadata,
        mut state: SessionSnapshot,
        working_directory: &Path,
        started: Instant,
        recovery_ms: u64,
        replayed_events: usize,
    ) -> Result<SessionRuntime> {
        state.reconcile_after_restart(&timestamp());
        state.status = SessionStatus::Unavailable;
        state.capabilities.set(app_model::Capability {
            id: CapabilityId::Changes,
            status: CapabilityStatus::Unavailable,
            detail: "The session working directory is unavailable.".to_owned(),
            evidence: vec![working_directory.display().to_string()],
        });
        self.storage.write_snapshot(&state)?;
        if self.selected_session()?.as_deref() == Some(metadata.id.as_str()) {
            self.set_selected_session(None)?;
        }
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "restore_session".to_owned(),
            elapsed_ms: Some(elapsed_ms(started)),
            session_id: Some(metadata.id.clone()),
            success: true,
            details: json!({
                "storageRecoveryMs": recovery_ms,
                "replayedEvents": replayed_events,
                "readOnly": true,
                "reason": "working_directory_unavailable",
                "workingDirectory": working_directory
            }),
        });
        Ok(SessionRuntime {
            handle: SessionHandle::read_only(state),
            provider: None,
            isolated: false,
        })
    }

    /// Recreate only worktrees beneath GCABB's own managed root.
    fn recreate_managed_worktree(
        &self,
        metadata: &SessionMetadata,
        state: &SessionSnapshot,
        working_directory: &Path,
        worktrees_root: Option<&Path>,
    ) -> bool {
        let Some(worktrees_root) = worktrees_root else {
            return false;
        };
        // An archive removed this worktree on purpose. Recreating it here
        // would undo that and strand the patch the archive is holding.
        if self
            .storage
            .is_session_archived(&metadata.id)
            .unwrap_or(false)
        {
            return false;
        }
        if metadata.kind != SessionKind::Project
            || !working_directory.starts_with(worktrees_root)
            || working_directory == worktrees_root
            || working_directory
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            return false;
        }
        let Some(repository) = metadata.repository_root.as_deref().map(Path::new) else {
            return false;
        };
        let Some(branch) = state.changes.branch.as_deref() else {
            return false;
        };
        let git = GitService::new(repository);
        if !git.is_worktree() || !git.branch_exists(branch) {
            return false;
        }
        let result = git.recreate_worktree(working_directory, branch);
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "recreate_worktree".to_owned(),
            elapsed_ms: None,
            session_id: Some(metadata.id.clone()),
            success: result.is_ok(),
            details: json!({
                "workingDirectory": working_directory,
                "repository": repository,
                "branch": branch,
                "error": result.as_ref().err().map(ToString::to_string)
            }),
        });
        result.is_ok()
    }

    fn spawn_actor(
        &self,
        provider: Arc<dyn AgentProvider>,
        isolated: bool,
        state: SessionSnapshot,
        provider_session: ProviderSession,
    ) -> SessionHandle {
        let id = state.metadata.id.clone();
        let (command_tx, command_rx) = mpsc::channel(32);
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(state.clone()));
        let actor = SessionActor {
            provider,
            isolated,
            storage: self.storage.clone(),
            diagnostics: self.diagnostics.clone(),
            state,
            sdk_session_id: provider_session.sdk_session_id,
            provider_events: provider_session.events,
            provider_interactions: provider_session.interactions,
            pending_responses: HashMap::new(),
            last_base_refresh: Instant::now(),
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

    fn record_runtime_start(&self, app_session_id: &str, compatibility: &ProviderCompatibility) {
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "runtime_start".to_owned(),
            elapsed_ms: compatibility
                .startup
                .as_ref()
                .map(|startup| startup.total_ms),
            session_id: Some(app_session_id.to_owned()),
            success: true,
            details: json!({
                "processId": compatibility.process_id,
                "sdkProtocolVersion": compatibility.sdk_protocol_version,
                "negotiatedProtocolVersion": compatibility.negotiated_protocol_version
            }),
        });
    }

    fn record_restore_failure(
        &self,
        metadata: &SessionMetadata,
        error: &SessionManagerError,
        elapsed_ms: u64,
    ) {
        self.diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "session_manager".to_owned(),
            operation: "restore_session".to_owned(),
            elapsed_ms: Some(elapsed_ms),
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
        attachments: Vec<PromptAttachment>,
        response: oneshot::Sender<Result<String>>,
    },
    Lifecycle {
        kind: SessionCommandKind,
        response: oneshot::Sender<Result<()>>,
    },
    Respond {
        interaction_id: String,
        answer: InteractionResponse,
        response: oneshot::Sender<Result<()>>,
    },
    LoadOutput {
        kind: OutputStreamKind,
        identity: String,
        before_chunk: u64,
        max_chunks: u64,
        response: oneshot::Sender<Result<()>>,
    },
    RefreshChanges {
        force: bool,
        response: oneshot::Sender<Result<()>>,
    },
    Control {
        control: SessionControlCommand,
        response: oneshot::Sender<Result<()>>,
    },
    Queue {
        command: QueueCommand,
        response: oneshot::Sender<Result<Option<String>>>,
    },
}

/// Edits to the durable queue.
///
/// Every variant is answerable while the agent is mid-turn: the queue is
/// GCABB's own state, so none of these wait on the runtime being ready.
pub enum QueueCommand {
    Enqueue {
        prompt: String,
        display_prompt: Option<String>,
        delivery: QueueDelivery,
    },
    UpdateText {
        id: String,
        prompt: String,
        display_prompt: Option<String>,
    },
    Remove {
        id: String,
    },
    Reorder {
        ordered_ids: Vec<String>,
    },
    SetPaused {
        paused: bool,
    },
    Clear,
}

enum SessionCommandKind {
    Cancel,
    Disconnect,
}

enum SessionControlCommand {
    Rename {
        title: String,
        source: TitleSource,
    },
    Model {
        model: String,
        reasoning_effort: ReasoningEffortUpdate,
        context_tier: Option<String>,
    },
    Mode(String),
    ReasoningEffort(String),
    ContextTier(String),
    BaseRef(String),
}

enum ReasoningEffortUpdate {
    Preserve,
    Set(Option<String>),
}

struct SessionActor {
    provider: Arc<dyn AgentProvider>,
    isolated: bool,
    storage: Arc<Storage>,
    diagnostics: Arc<dyn DiagnosticsSink>,
    state: SessionSnapshot,
    sdk_session_id: String,
    provider_events: mpsc::Receiver<ProviderEvent>,
    provider_interactions: mpsc::Receiver<ProviderInteraction>,
    pending_responses: HashMap<String, oneshot::Sender<InteractionResponse>>,
    last_base_refresh: Instant,
    commands: mpsc::Receiver<SessionCommand>,
    snapshots: watch::Sender<Arc<SessionSnapshot>>,
}

impl SessionActor {
    async fn run(mut self) {
        self.load_queue();
        self.publish(false);
        loop {
            tokio::select! {
                event = self.provider_events.recv() => {
                    match event {
                        Some(ProviderEvent::Event(raw)) => self.apply_raw(&raw).await,
                        Some(ProviderEvent::Lagged(count)) => self.apply_raw(&json!({
                            "id": format!("lagged-{}-{count}", self.state.last_sequence + 1),
                            "type": "session.warning",
                            "data": {"message": format!("provider subscriber skipped {count} events")}
                        })).await,
                        Some(ProviderEvent::Closed) | None => {
                            self.handle_provider_closed().await;
                            break;
                        }
                    }
                }
                Some(interaction) = self.provider_interactions.recv() => {
                    self.receive_interaction(interaction);
                }
                command = self.commands.recv() => {
                    match command {
                        Some(SessionCommand::Send {
                            prompt,
                            attachments,
                            response,
                        }) => {
                            let result = self.provider
                                .send(&self.sdk_session_id, &prompt, &attachments)
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
                        Some(SessionCommand::Respond {
                            interaction_id,
                            answer,
                            response,
                        }) => {
                            let result = self.respond(&interaction_id, answer);
                            let _ = response.send(result);
                        }
                        Some(SessionCommand::LoadOutput {
                            kind,
                            identity,
                            before_chunk,
                            max_chunks,
                            response,
                        }) => {
                            let result = self.load_output(
                                kind,
                                &identity,
                                before_chunk,
                                max_chunks,
                            );
                            let _ = response.send(result);
                        }
                        Some(SessionCommand::RefreshChanges { force, response }) => {
                            let result = self.refresh_changes(force).await;
                            let _ = response.send(result);
                        }
                        Some(SessionCommand::Control { control, response }) => {
                            let result = self.apply_control(control).await;
                            let _ = response.send(result);
                        }
                        Some(SessionCommand::Queue { command, response }) => {
                            let result = self.apply_queue(command).await;
                            let _ = response.send(result);
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

    async fn apply_raw(&mut self, raw: &Value) {
        let sequence = self.state.last_sequence + 1;
        let event = DomainEvent::from_sdk_event_for(&self.state.metadata.id, sequence, raw);
        let output_updates = app_model::tools::output_updates(&self.state.tool_activity, &event);
        match self
            .storage
            .append_event_with_output(&event, &output_updates)
        {
            Ok(true) => {
                let refresh_after = {
                    let applied = self.state.apply(event);
                    if applied == ApplyOutcome::Applied {
                        let force_snapshot = self.state.status == SessionStatus::Idle
                            || self.state.status == SessionStatus::Failed
                            || self.state.last_sequence.is_multiple_of(SNAPSHOT_INTERVAL);
                        self.publish(force_snapshot);
                        true
                    } else {
                        false
                    }
                };
                if refresh_after {
                    self.refresh_changes_if_needed(raw).await;
                    self.sync_queue(raw).await;
                }
            }
            Ok(false) => {
                self.state.last_sequence = sequence;
                tracing::debug!(event_id = event.id, "duplicate event ignored");
            }
            Err(error) => self.record_actor_error("append_event", &error.to_string()),
        }
    }

    fn load_output(
        &mut self,
        kind: OutputStreamKind,
        identity: &str,
        before_chunk: u64,
        max_chunks: u64,
    ) -> Result<()> {
        let start_chunk = before_chunk.saturating_sub(max_chunks);
        let read = self.storage.read_output(
            &self.state.metadata.id,
            kind,
            identity,
            OutputRange {
                start_chunk,
                max_chunks: before_chunk - start_chunk,
            },
        )?;
        if read.next_chunk != before_chunk
            || !self.state.tool_activity.prepend_output(
                kind,
                identity,
                start_chunk,
                before_chunk,
                &read.content,
            )
        {
            return Err(SessionManagerError::Storage(
                StorageError::OutputIncomplete {
                    kind: kind.as_str(),
                    identity: identity.to_owned(),
                    expected: before_chunk - start_chunk,
                    actual: read.next_chunk - start_chunk,
                },
            ));
        }
        self.publish(false);
        Ok(())
    }

    /// Recompute changes when a worktree-mutating tool has just completed.
    ///
    /// Git runs on a blocking thread so a large diff cannot stall the actor's
    /// event loop or delay unrelated sessions.
    async fn refresh_changes_if_needed(&mut self, raw: &Value) {
        let event =
            DomainEvent::from_sdk_event_for(&self.state.metadata.id, self.state.last_sequence, raw);
        if !needs_changes_refresh(&self.state, &event) {
            return;
        }

        let worktree = PathBuf::from(&self.state.metadata.project_path);
        let base_ref = base_ref_for(&self.state);
        let computed =
            tokio::task::spawn_blocking(move || compute_changes(&worktree, &base_ref)).await;
        match computed {
            Ok(changes) => {
                apply_changes(&mut self.state, changes);
                self.publish(false);
            }
            Err(error) => self.record_actor_error("refresh_changes", &error.to_string()),
        }
    }

    async fn refresh_changes(&mut self, force: bool) -> Result<()> {
        let worktree = PathBuf::from(&self.state.metadata.project_path);
        let base_ref = base_ref_for(&self.state);
        let fetched_ref = base_ref.clone();
        let fetch = force || self.last_base_refresh.elapsed() >= BASE_REF_REFRESH_TTL;
        let (fetch_error, computed) = tokio::task::spawn_blocking(move || {
            let service = GitService::new(&worktree);
            let fetch_error = fetch
                .then(|| service.fetch_base_ref(&fetched_ref).err())
                .flatten();
            (fetch_error, compute_changes(&worktree, &fetched_ref))
        })
        .await
        .map_err(|error| SessionManagerError::BackgroundTask(error.to_string()))?;
        if fetch {
            self.last_base_refresh = Instant::now();
        }
        if let Some(error) = fetch_error {
            tracing::warn!(%error, %base_ref, "failed to refresh changes base; using cached ref");
        }
        apply_changes(&mut self.state, computed);
        self.publish(false);
        Ok(())
    }

    fn receive_interaction(&mut self, mut interaction: ProviderInteraction) {
        interaction
            .request
            .session_id
            .clone_from(&self.state.metadata.id);
        let interaction_id = interaction.request.id.clone();
        self.pending_responses
            .insert(interaction_id, interaction.response);
        self.state.add_interaction(interaction.request);
        self.publish(true);
    }

    fn respond(&mut self, interaction_id: &str, answer: InteractionResponse) -> Result<()> {
        let response = self
            .pending_responses
            .remove(interaction_id)
            .ok_or_else(|| SessionManagerError::SessionNotFound(interaction_id.to_owned()))?;
        response
            .send(answer.clone())
            .map_err(|_| SessionManagerError::ActorClosed)?;
        self.state
            .record_interaction_response(interaction_id, answer);
        self.state.remove_interaction(interaction_id);
        self.publish(true);
        Ok(())
    }

    async fn apply_control(&mut self, control: SessionControlCommand) -> Result<()> {
        match control {
            SessionControlCommand::Rename { title, source } => {
                if source == TitleSource::Manual
                    || self.state.metadata.title_source == TitleSource::Fallback
                {
                    self.state.metadata.title = title;
                    self.state.metadata.title_source = source;
                }
            }

            SessionControlCommand::Model {
                model,
                reasoning_effort,
                context_tier,
            } => {
                let sdk_reasoning_effort = match &reasoning_effort {
                    ReasoningEffortUpdate::Preserve => None,
                    ReasoningEffortUpdate::Set(effort) => effort.as_deref(),
                };
                self.provider
                    .set_model(
                        &self.sdk_session_id,
                        &model,
                        sdk_reasoning_effort,
                        context_tier.as_deref(),
                    )
                    .await?;
                if let ReasoningEffortUpdate::Set(reasoning_effort) = reasoning_effort {
                    self.state.controls.reasoning_effort = reasoning_effort;
                }
                if context_tier.is_some() {
                    self.state.controls.context_tier = context_tier;
                }
                self.state.controls.model = Some(model.clone());
                self.state.metadata.model = Some(model);
            }
            SessionControlCommand::Mode(mode) => {
                self.provider.set_mode(&self.sdk_session_id, &mode).await?;
                self.state.controls.mode = Some(mode.clone());
                self.state.metadata.mode = Some(mode);
            }
            SessionControlCommand::ReasoningEffort(effort) => {
                self.provider
                    .set_reasoning_effort(&self.sdk_session_id, &effort)
                    .await?;
                self.state.controls.reasoning_effort = Some(effort);
            }
            SessionControlCommand::ContextTier(tier) => {
                let model = self.state.controls.model.clone().ok_or_else(|| {
                    SessionManagerError::SessionNotFound(self.sdk_session_id.clone())
                })?;
                self.provider
                    .set_model(
                        &self.sdk_session_id,
                        &model,
                        self.state.controls.reasoning_effort.as_deref(),
                        Some(tier.as_str()),
                    )
                    .await?;
                self.state.controls.context_tier = Some(tier);
            }
            SessionControlCommand::BaseRef(base_ref) => {
                let worktree = PathBuf::from(&self.state.metadata.project_path);
                let selected = base_ref.clone();
                let (fetch_error, changes) = tokio::task::spawn_blocking(move || {
                    let service = GitService::new(&worktree);
                    let fetch_error = service.fetch_base_ref(&selected).err();
                    (fetch_error, compute_changes(&worktree, &selected))
                })
                .await
                .map_err(|error| SessionManagerError::BackgroundTask(error.to_string()))?;
                self.last_base_refresh = Instant::now();
                if let Some(error) = fetch_error {
                    tracing::warn!(%error, %base_ref, "failed to refresh selected base; using cached ref");
                }
                self.state.metadata.base_ref = Some(base_ref);
                apply_changes(&mut self.state, changes);
            }
        }
        self.state.metadata.updated_at = timestamp();
        self.storage.upsert_session(&self.state.metadata)?;
        self.publish(true);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.cancel_pending_interactions();
        let persistence_result = self
            .storage
            .write_snapshot(&self.state)
            .map_err(SessionManagerError::from);
        let disconnect_result = self
            .provider
            .disconnect(&self.sdk_session_id)
            .await
            .map_err(SessionManagerError::from);
        let stop_result = if self.isolated {
            self.provider
                .stop()
                .await
                .map_err(SessionManagerError::from)
        } else {
            Ok(())
        };
        self.state.status = SessionStatus::Disconnected;
        self.publish(false);
        persistence_result.and(disconnect_result).and(stop_result)
    }

    async fn handle_provider_closed(&mut self) {
        let message = "provider event stream closed unexpectedly";
        self.state.status = SessionStatus::Disconnected;
        self.state.last_error = Some(message.to_owned());
        self.cancel_pending_interactions();
        self.record_actor_error("provider_stream_closed", message);
        if let Err(error) = self.provider.disconnect(&self.sdk_session_id).await {
            self.record_actor_error("provider_stream_cleanup", &error.to_string());
        }
        if self.isolated
            && let Err(error) = self.provider.stop().await
        {
            self.record_actor_error("provider_runtime_cleanup", &error.to_string());
        }
        self.publish(true);
    }

    fn cancel_pending_interactions(&mut self) {
        for (_, response) in self.pending_responses.drain() {
            let _ = response.send(InteractionResponse::Cancel);
        }
        self.state.cancel_pending_interactions();
    }

    fn publish(&self, persist: bool) {
        if persist && let Err(error) = self.storage.write_snapshot(&self.state) {
            self.record_actor_error("write_snapshot", &error.to_string());
        }
        self.snapshots.send_replace(Arc::new(self.state.clone()));
    }

    /// Load the durable queue into the snapshot.
    fn load_queue(&mut self) {
        let session_id = self.state.metadata.id.clone();
        match self.storage.queue_view(&session_id) {
            Ok(queue) => self.state.queue = queue,
            Err(error) => self.record_actor_error("queue_view", &error.to_string()),
        }
    }

    /// Re-read the queue from storage so the snapshot matches what was written.
    fn reload_queue(&mut self) -> Result<()> {
        self.state.queue = self.storage.queue_view(&self.state.metadata.id)?;
        Ok(())
    }

    async fn apply_queue(&mut self, command: QueueCommand) -> Result<Option<String>> {
        let session_id = self.state.metadata.id.clone();
        let mut queued_id = None;
        match command {
            QueueCommand::Enqueue {
                prompt,
                display_prompt,
                delivery,
            } => {
                let now = timestamp();
                let item = QueueItem {
                    id: Uuid::new_v4().to_string(),
                    session_id: session_id.clone(),
                    position: self.storage.next_queue_position(&session_id)?,
                    prompt,
                    display_prompt,
                    state: QueueItemState::Pending,
                    delivery,
                    agent_mode: self.state.controls.mode.clone(),
                    created_at: now.clone(),
                    updated_at: now,
                    error: None,
                };
                queued_id = Some(item.id.clone());
                self.storage.upsert_queue_item(&item)?;
            }
            QueueCommand::UpdateText {
                id,
                prompt,
                display_prompt,
            } => {
                // Only an undelivered item can be edited: once it has been
                // handed over, the text the agent received is already fixed.
                let Some(mut item) = self
                    .storage
                    .queue_view(&session_id)?
                    .item(&id)
                    .filter(|item| item.state.is_pending())
                    .cloned()
                else {
                    return Ok(None);
                };
                item.prompt = prompt;
                item.display_prompt = display_prompt;
                item.updated_at = timestamp();
                self.storage.upsert_queue_item(&item)?;
            }
            QueueCommand::Remove { id } => {
                self.storage.delete_queue_item(&id)?;
            }
            QueueCommand::Reorder { ordered_ids } => {
                self.storage.reorder_queue(&session_id, &ordered_ids)?;
            }
            QueueCommand::SetPaused { paused } => {
                self.storage.set_queue_paused(&session_id, paused)?;
            }
            QueueCommand::Clear => {
                for item in self.storage.queue_view(&session_id)?.items {
                    if !item.state.is_pending() {
                        continue;
                    }
                    self.storage.delete_queue_item(&item.id)?;
                }
            }
        }
        self.reload_queue()?;
        self.publish(false);
        self.drain_queue().await;
        Ok(queued_id)
    }

    /// React to runtime signals that bear on the queue.
    async fn sync_queue(&mut self, raw: &Value) {
        if raw.get("type").and_then(Value::as_str) == Some("session.idle") {
            self.drain_queue().await;
        }
    }

    /// Retire items whose turn has finished.
    ///
    /// Called only when the session is idle, so anything still marked as
    /// dispatched has had its turn run to completion.
    /// Returns whether anything was retired, so the caller can publish. The
    /// last item in a queue retires with nothing left to dispatch behind it,
    /// and without a publish of its own it would sit in the UI as running
    /// forever.
    fn complete_dispatched(&mut self) -> bool {
        let dispatched: Vec<_> = self
            .state
            .queue
            .items
            .iter()
            .filter(|item| item.state == QueueItemState::Dispatched)
            .cloned()
            .collect();
        if dispatched.is_empty() {
            return false;
        }
        for mut item in dispatched {
            item.state = QueueItemState::Completed;
            item.updated_at = timestamp();
            if let Err(error) = self.storage.upsert_queue_item(&item) {
                self.record_actor_error("upsert_queue_item", &error.to_string());
            }
        }
        if let Err(error) = self.reload_queue() {
            self.record_actor_error("queue_view", &error.to_string());
        }
        true
    }

    /// Hand the next pending item to the agent when the session can take it.
    ///
    /// One item at a time: the next is delivered when the session next goes
    /// idle, so a follow-up stays editable until the moment it is dispatched.
    async fn drain_queue(&mut self) {
        if self.state.status != SessionStatus::Idle {
            return;
        }
        if self.complete_dispatched() {
            self.publish(false);
        }
        if self.state.queue.paused {
            return;
        }
        let Some(mut item) = self.state.queue.next_pending().cloned() else {
            return;
        };
        let request = QueueDeliveryRequest {
            prompt: item.prompt.clone(),
            display_prompt: item.display_prompt.clone(),
            delivery: item.delivery,
            agent_mode: item.agent_mode.clone(),
        };
        item.updated_at = timestamp();
        match self
            .provider
            .deliver_queued(&self.sdk_session_id, &request)
            .await
        {
            Ok(_) => {
                item.state = QueueItemState::Dispatched;
                item.error = None;
            }
            Err(error) => {
                // A failed item must not be retried on the next idle event, or
                // a permanently failing prompt would block the whole queue.
                item.state = QueueItemState::Failed;
                item.error = Some(error.to_string());
                self.record_actor_error("deliver_queued", &error.to_string());
            }
        }
        if let Err(error) = self.storage.upsert_queue_item(&item) {
            self.record_actor_error("upsert_queue_item", &error.to_string());
        }
        if let Err(error) = self.reload_queue() {
            self.record_actor_error("queue_view", &error.to_string());
        }
        self.publish(false);
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

fn normalize_generated_title(raw: &str) -> Option<String> {
    let line = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    let title = line
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '#'))
        .trim()
        .trim_end_matches(['.', '!', '?', ':'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let word_count = title.split_whitespace().count();
    ((2..=5).contains(&word_count) && title.chars().count() <= 56).then_some(title)
}

/// Attachment paths recorded across a session's transcript.
fn attachment_paths(state: &SessionSnapshot) -> Vec<PathBuf> {
    state
        .transcript
        .iter()
        .flat_map(|message| message.attachments.iter())
        .filter_map(|attachment| attachment.path.as_ref())
        .map(PathBuf::from)
        .collect()
}

/// Delete a session's pasted images.
///
/// Only files inside the managed attachments directory are removed. A user who
/// attached a picture from their own folder must still have it afterwards, so
/// anything outside that directory is left alone.
fn remove_attachments(paths: &[PathBuf], attachments_root: Option<&Path>) -> usize {
    let Some(root) = attachments_root else {
        return 0;
    };
    paths
        .iter()
        .filter(|path| path.starts_with(root))
        .filter(|path| std::fs::remove_file(path).is_ok())
        .count()
}

/// Delete the runtime's own state directory for a session.
///
/// Keyed by the id the runtime assigned, and confined to the directory the
/// caller named, so a malformed id cannot reach outside it.
fn remove_runtime_state(sdk_session_id: &str, runtime_state_root: Option<&Path>) -> bool {
    let Some(root) = runtime_state_root else {
        return false;
    };
    if sdk_session_id.is_empty() || sdk_session_id.contains(['/', '\\']) {
        return false;
    }
    let directory = root.join(sdk_session_id);
    if !directory.starts_with(root) || !directory.is_dir() {
        return false;
    }
    std::fs::remove_dir_all(&directory).is_ok()
}

fn reconcile_history(
    storage: &Storage,
    state: &mut SessionSnapshot,
    history: Vec<Value>,
) -> Result<()> {
    // The event log is the record of what was seen; the snapshot no longer
    // carries a copy of it.
    let mut seen = storage.event_ids(&state.metadata.id)?;
    for raw in history {
        let Some(event_id) = raw.get("id").and_then(Value::as_str) else {
            continue;
        };
        if seen.contains(event_id) {
            continue;
        }
        let event =
            DomainEvent::from_sdk_event_for(&state.metadata.id, state.last_sequence + 1, &raw);
        let output_updates = app_model::tools::output_updates(&state.tool_activity, &event);
        if storage.append_event_with_output(&event, &output_updates)? {
            seen.insert(event.id.clone());
            let _ = state.apply(event);
        } else {
            state.last_sequence += 1;
        }
    }
    Ok(())
}

/// Compute a changes view for a worktree. Runs git, so callers must keep it
/// off the actor's async loop.
fn compute_changes(worktree: &Path, base_ref: &str) -> app_model::ChangesView {
    let service = GitService::new(worktree);
    if !service.is_worktree() {
        return app_model::ChangesView {
            error: Some(format!(
                "{} is not a git worktree; changes are unavailable.",
                worktree.display()
            )),
            generated_at: Some(timestamp()),
            ..app_model::ChangesView::default()
        };
    }
    service.changes(base_ref, timestamp())
}

/// Apply a computed changes view and derive the `Changes` capability.
fn apply_changes(state: &mut SessionSnapshot, changes: app_model::ChangesView) {
    let base_ref = changes
        .base_label
        .clone()
        .unwrap_or_else(|| "HEAD".to_owned());
    let capability = if let Some(error) = &changes.error {
        let unavailable = error.contains("not a git worktree");
        app_model::Capability {
            id: CapabilityId::Changes,
            status: if unavailable {
                CapabilityStatus::Unavailable
            } else {
                CapabilityStatus::NeedsAttention
            },
            detail: error.clone(),
            evidence: Vec::new(),
        }
    } else {
        app_model::Capability {
            id: CapabilityId::Changes,
            status: CapabilityStatus::Available,
            detail: format!(
                "{} changed file(s) against {base_ref}.",
                changes.files.len()
            ),
            evidence: vec![base_ref],
        }
    };
    state.changes = changes;
    state.capabilities.set(capability);
}

/// The base ref a session compares against.
///
/// Falls back to `HEAD` when none was recorded, which keeps sessions created
/// before the changes view usable after the schema upgrade.
fn base_ref_for(state: &SessionSnapshot) -> String {
    state
        .metadata
        .base_ref
        .clone()
        .unwrap_or_else(|| "HEAD".to_owned())
}

/// Refresh the selected upstream and compute the first changes snapshot.
async fn refresh_changes_on_start(state: &mut SessionSnapshot) {
    // Chats are not attached to a checkout, so there is nothing to diff.
    if state.metadata.is_chat() {
        state.changes = app_model::ChangesView::default();
        state.capabilities.set(app_model::Capability {
            id: CapabilityId::Changes,
            status: CapabilityStatus::Unavailable,
            detail: "Chats are not attached to a repository.".to_owned(),
            evidence: Vec::new(),
        });
        return;
    }
    let worktree = PathBuf::from(&state.metadata.project_path);
    let base_ref = base_ref_for(state);
    let computed = tokio::task::spawn_blocking(move || {
        let service = GitService::new(&worktree);
        let fetch_error = service.fetch_base_ref(&base_ref).err();
        (fetch_error, compute_changes(&worktree, &base_ref))
    })
    .await;
    match computed {
        Ok((fetch_error, changes)) => {
            if let Some(error) = fetch_error {
                tracing::warn!(%error, "failed to refresh changes base; using cached ref");
            }
            apply_changes(state, changes);
        }
        Err(error) => {
            tracing::error!(%error, "changes refresh task failed");
            state.changes.error = Some(format!("changes refresh task failed: {error}"));
        }
    }
}

/// Whether an event indicates the worktree may have changed.
///
/// Only completions of worktree-mutating tool classes trigger a refresh, so
/// read-only activity does not cause repeated git invocations.
fn needs_changes_refresh(state: &SessionSnapshot, event: &DomainEvent) -> bool {
    if state.metadata.is_chat() {
        return false;
    }
    if event.source_type != "tool.execution_complete" {
        return false;
    }
    let Some(call_id) = event.details.get("toolCallId").and_then(Value::as_str) else {
        return false;
    };
    state
        .tool_activity
        .invocation(call_id)
        .is_some_and(|invocation| invocation.class.mutates_worktree())
}

fn timestamp() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| "0".to_owned(),
        |duration| duration.as_millis().to_string(),
    )
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn is_gcabb_worktree(
    kind: SessionKind,
    working_directory: &Path,
    repository_root: Option<&str>,
) -> bool {
    kind == SessionKind::Project
        && repository_root.is_some_and(|root| Path::new(root) != working_directory)
}

#[cfg(test)]
mod tests {
    use diagnostics::MemoryDiagnostics;
    use std::process::Command;
    use tempfile::tempdir;
    use test_harness::{FakeProvider, FakeProviderFactory, golden_events};

    use super::*;

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .output()
            .expect("git command runs");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Read a file, normalising line endings.
    ///
    /// Windows CI runs with `core.autocrlf=true`, so git checks files out with
    /// CRLF. That is git doing its job, not the archive losing content, and
    /// the assertions here are about content.
    fn read_text(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
    }

    fn initialise_repository(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "--initial-branch=main"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test"]);
        std::fs::write(path.join("base.txt"), "base\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-m", "base"]);
    }

    #[test]
    fn only_gcabb_worktrees_auto_approve_tools() {
        let repository = PathBuf::from("repository");
        let worktree = repository.join("gcabb-worktree");

        assert!(is_gcabb_worktree(
            SessionKind::Project,
            &worktree,
            repository.to_str(),
        ));
        assert!(!is_gcabb_worktree(
            SessionKind::Project,
            &repository,
            repository.to_str(),
        ));
        assert!(!is_gcabb_worktree(
            SessionKind::Chat,
            &worktree,
            repository.to_str(),
        ));
        assert!(!is_gcabb_worktree(SessionKind::Project, &worktree, None,));
    }

    fn request(path: PathBuf) -> CreateSessionRequest {
        CreateSessionRequest {
            project_path: path,
            repository_root: None,
            title: "Foundation test".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Project,
            model: None,
            mode: Some("interactive".to_owned()),
            reasoning_effort: Some("medium".to_owned()),
            context_tier: None,
            base_ref: None,
        }
    }

    #[tokio::test]
    async fn sessions_own_independent_provider_runtimes() {
        let factory = FakeProviderFactory::default();
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(factory.clone(), storage, diagnostics.clone());
        manager.start().await.unwrap();
        let directory = tempdir().unwrap();

        let first = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        let second = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();

        let providers = factory.providers();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].process_id(), Some(1));
        assert_eq!(providers[1].process_id(), Some(2));
        assert!(providers.iter().all(|provider| provider.is_started()));
        assert_eq!(providers[0].active_sessions().await, 1);
        assert_eq!(providers[1].active_sessions().await, 1);

        manager.close_session(first.id()).await.unwrap();

        assert!(!providers[0].is_started());
        assert!(providers[1].is_started());
        assert_eq!(
            second.send("still running").await.unwrap(),
            "message-fake-session-2001"
        );
        let runtime_processes = diagnostics
            .events()
            .into_iter()
            .filter(|event| event.operation == "runtime_start")
            .filter_map(|event| event.details["processId"].as_u64())
            .collect::<Vec<_>>();
        assert_eq!(runtime_processes, vec![1, 2]);

        manager.stop().await.unwrap();
        assert!(!providers[1].is_started());
    }

    #[tokio::test]
    async fn shared_provider_adapter_keeps_siblings_running_until_manager_stop() {
        let provider: Arc<dyn AgentProvider> = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics);
        manager.start().await.unwrap();
        let directory = tempdir().unwrap();
        let first = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        let second = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();

        manager.close_session(first.id()).await.unwrap();

        assert_eq!(
            second.send("still shared").await.unwrap(),
            "message-fake-session-2"
        );
        manager.stop().await.unwrap();
    }

    #[tokio::test]
    async fn deleted_session_cannot_be_reinserted_after_restore_finishes() {
        let hydrated = std::sync::atomic::AtomicBool::new(false);
        let factory = FakeProviderFactory::default();
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(factory.clone(), storage.clone(), diagnostics);
        manager.start().await.unwrap();
        let directory = tempdir().unwrap();
        let handle = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        let metadata = handle.snapshot().metadata.clone();
        let runtime = manager.sessions.lock().await.remove(handle.id()).unwrap();
        storage.delete_session(handle.id()).unwrap();

        let installed = manager
            .install_restored_runtime(&metadata, runtime, |_| {
                hydrated.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await
            .unwrap();

        assert!(installed.is_none());
        assert!(!hydrated.load(std::sync::atomic::Ordering::SeqCst));
        assert!(manager.sessions.lock().await.is_empty());
        assert!(!factory.providers()[0].is_started());
    }

    #[tokio::test]
    async fn failed_snapshot_cleanup_still_disconnects_shared_provider_session() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage.clone(), diagnostics);
        manager.start().await.unwrap();
        let directory = tempdir().unwrap();
        let handle = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        let metadata = handle.snapshot().metadata.clone();
        let runtime = manager.sessions.lock().await.remove(handle.id()).unwrap();
        storage.delete_session(handle.id()).unwrap();

        let installed = manager
            .install_restored_runtime(&metadata, runtime, |_| {})
            .await
            .unwrap();

        assert!(installed.is_none());
        assert_eq!(provider.active_sessions().await, 0);
        assert!(provider.is_started());
        manager.stop().await.unwrap();
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
        assert_eq!(snapshot.last_sequence, 4);
        assert_eq!(snapshot.last_sequence, 4);
        assert_eq!(snapshot.status, SessionStatus::Idle);
    }

    /// Attachments must reach the runtime, not stop at the actor boundary.
    #[tokio::test]
    async fn attachments_reach_the_provider() {
        let provider = Arc::new(FakeProvider::with_script(golden_events().unwrap()));
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics);
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(std::env::temp_dir()))
            .await
            .unwrap();

        handle
            .send_with_attachments(
                "look at this",
                vec![PromptAttachment::from_path(std::path::Path::new(
                    "/tmp/shot.png",
                ))],
            )
            .await
            .unwrap();

        let sent = provider.sent_attachments().await;
        assert_eq!(sent.len(), 1, "exactly one send happened");
        assert_eq!(sent[0].len(), 1, "the attachment never left the app");
        assert_eq!(sent[0][0].identity(), "/tmp/shot.png");
    }

    /// A pasted image belongs to the session, so it goes when the session
    /// goes. A picture the user attached from their own folder does not:
    /// deleting a session must never delete the user's files.
    #[test]
    fn only_managed_attachments_are_deleted() {
        let managed = tempdir().unwrap();
        let personal = tempdir().unwrap();
        let pasted = managed.path().join("abc-clipboard.png");
        let owned = personal.path().join("holiday.jpg");
        std::fs::write(&pasted, b"pasted").unwrap();
        std::fs::write(&owned, b"precious").unwrap();

        let removed = remove_attachments(&[pasted.clone(), owned.clone()], Some(managed.path()));

        assert_eq!(removed, 1);
        assert!(!pasted.exists(), "the pasted image was left behind");
        assert!(
            owned.exists(),
            "deleting a session deleted a file the user owns"
        );
    }

    /// The runtime's own state directory is removed with the session.
    #[test]
    fn runtime_state_is_removed_with_the_session() {
        let root = tempdir().unwrap();
        let directory = root.path().join("sdk-session-1");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("events.jsonl"), b"{}").unwrap();

        assert!(remove_runtime_state("sdk-session-1", Some(root.path())));
        assert!(!directory.exists());
    }

    /// A session id is used to build a path, so it must not be able to name
    /// somewhere else.
    #[test]
    fn a_traversing_session_id_cannot_escape_the_state_root() {
        let root = tempdir().unwrap();
        let outside = root.path().parent().unwrap().join("gcabb-escape-probe");
        std::fs::create_dir_all(&outside).unwrap();

        assert!(!remove_runtime_state(
            "../gcabb-escape-probe",
            Some(root.path())
        ));
        assert!(outside.exists(), "a session id escaped the state root");
        std::fs::remove_dir_all(&outside).ok();
    }

    /// Nothing is deleted from a location the caller did not name.
    #[test]
    fn no_roots_means_nothing_is_deleted() {
        let managed = tempdir().unwrap();
        let file = managed.path().join("abc-clipboard.png");
        std::fs::write(&file, b"pasted").unwrap();

        assert_eq!(remove_attachments(std::slice::from_ref(&file), None), 0);
        assert!(file.exists());
        assert!(!remove_runtime_state("sdk-session-1", None));
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
        let restored_manager = SessionManager::new(provider, reopened, diagnostics.clone());
        let (_, report) = restored_manager.start().await.unwrap();

        assert!(report.failed.is_empty());
        assert_eq!(report.restored.len(), 1);
        let restored = restored_manager.session(&app_session_id).await.unwrap();
        assert_eq!(restored.snapshot().last_sequence, 4);
        assert_eq!(restored.snapshot().status, SessionStatus::Idle);
        let events = diagnostics.events();
        let restore_timing = events
            .iter()
            .rev()
            .find(|event| {
                event.operation == "restore_session"
                    && event.session_id.as_deref() == Some(app_session_id.as_str())
                    && event.success
            })
            .expect("successful restore timing is recorded");
        assert!(restore_timing.elapsed_ms.is_some());
        assert_eq!(restore_timing.details["historyEvents"], 4);
        assert!(restore_timing.details["storageRecoveryMs"].is_number());
    }

    #[tokio::test]
    async fn selected_session_is_published_before_other_restores() {
        let directory = tempdir().unwrap();
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage.clone(), diagnostics.clone());
        manager.start().await.unwrap();
        let selected = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        let selected_id = selected.id().to_owned();
        manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        manager.stop().await.unwrap();

        let restored_manager = SessionManager::new(provider, storage, diagnostics);
        let mut published = Vec::new();
        let (_, report, remaining) = restored_manager
            .start_preferred_session(Some(&selected_id), |handle| {
                published.push(handle.id().to_owned());
            })
            .await
            .unwrap();

        assert_eq!(published.first(), Some(&selected_id));
        assert_eq!(published.len(), 1);
        assert_eq!(report.restored.len(), 1);
        assert_eq!(remaining.len(), 1);
        assert!(restored_manager.session(&selected_id).await.is_ok());

        let background = restored_manager
            .restore_remaining_sessions(remaining, |handle| {
                published.push(handle.id().to_owned());
            })
            .await;
        assert!(background.failed.is_empty());
        assert_eq!(published.len(), 2);
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
            repository_root: None,
            title: "Broken restore".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Project,
            model: None,
            mode: None,
            base_ref: None,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        storage.upsert_session(&metadata).unwrap();
        let manager = SessionManager::new(provider, storage, diagnostics.clone());

        let (_, report) = manager.start().await.unwrap();

        assert!(report.restored.is_empty());
        assert_eq!(report.failed.len(), 1);
        let events = diagnostics.events();
        let failure = events
            .iter()
            .find(|event| event.operation == "restore_session")
            .expect("restore failure timing is recorded");
        assert!(!failure.success);
        assert!(failure.elapsed_ms.is_some());
        let startup = events
            .iter()
            .find(|event| event.operation == "startup")
            .expect("startup summary is recorded");
        assert_eq!(startup.details["restoredSessions"], 0);
        assert_eq!(startup.details["failedSessions"], 1);
    }

    #[tokio::test]
    async fn missing_working_directory_restores_read_only_without_resuming() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("deleted-worktree");
        let provider = Arc::new(FakeProvider::default());
        provider.start().await.unwrap();
        let seeded = provider
            .create_session(SessionRequest {
                working_directory: directory.path().to_owned(),
                model: None,
                mode: None,
                reasoning_effort: None,
                context_tier: None,
                auto_approve_tools: false,
            })
            .await
            .unwrap();
        provider.disconnect(&seeded.sdk_session_id).await.unwrap();
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let metadata = SessionMetadata {
            id: "stale-session".to_owned(),
            sdk_session_id: seeded.sdk_session_id,
            project_path: missing.to_string_lossy().into_owned(),
            repository_root: None,
            title: "Deleted worktree".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Project,
            model: None,
            mode: None,
            base_ref: None,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        storage.upsert_session(&metadata).unwrap();
        let mut snapshot = SessionSnapshot::new(metadata.clone());
        snapshot.transcript.push(app_model::TranscriptMessage {
            id: "message-1".to_owned(),
            role: app_model::TranscriptRole::Assistant,
            content: "Persisted history".to_owned(),
            state: app_model::TranscriptState::Complete,
            timestamp: "1".to_owned(),
            sequence: 1,
            attachments: Vec::new(),
        });
        storage.write_snapshot(&snapshot).unwrap();
        storage.set_selected_session(Some(&metadata.id)).unwrap();
        let manager = SessionManager::new(provider.clone(), storage, diagnostics);

        let (_, report) = manager.start().await.unwrap();

        assert!(
            report.failed.is_empty(),
            "restore failed: {:?}",
            report
                .failed
                .iter()
                .map(|failure| failure.error.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(report.restored.len(), 1);
        let restored = report.restored[0].snapshot();
        assert_eq!(restored.status, SessionStatus::Unavailable);
        assert_eq!(restored.transcript[0].content, "Persisted history");
        assert_eq!(manager.selected_session().unwrap(), None);
        assert_eq!(provider.active_sessions().await, 0);

        let replacement = tempdir().unwrap();
        let relocated = manager
            .relocate_session("stale-session", replacement.path())
            .await
            .unwrap();
        assert_eq!(relocated.snapshot().status, SessionStatus::Idle);
        assert_eq!(
            relocated.snapshot().metadata.project_path,
            replacement.path().canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(provider.active_sessions().await, 1);
    }

    /// Archiving reduces a worktree to a branch plus a patch, and unarchiving
    /// puts both back. Uncommitted and untracked work must survive the trip.
    #[tokio::test]
    async fn archiving_discards_the_worktree_and_unarchiving_rebuilds_it() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let worktrees = directory.path().join("worktrees");
        let worktree = worktrees.join("repository").join("session");
        initialise_repository(&repository);
        let repository_git = GitService::new(&repository);
        repository_git
            .create_worktree(&worktree, "gcabb/archive-me", "main")
            .unwrap();
        // Committed, modified, and untracked work: all three must come back.
        std::fs::write(worktree.join("committed.txt"), "committed\n").unwrap();
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "-m", "session work"]);
        std::fs::write(worktree.join("base.txt"), "modified\n").unwrap();
        std::fs::write(worktree.join("scratch.txt"), "untracked\n").unwrap();

        let metadata = SessionMetadata {
            id: "archivable-session".to_owned(),
            sdk_session_id: "sdk-archivable".to_owned(),
            project_path: worktree.to_string_lossy().into_owned(),
            repository_root: Some(repository.to_string_lossy().into_owned()),
            title: "Archive me".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Project,
            model: None,
            mode: None,
            base_ref: Some("main".to_owned()),
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        storage.upsert_session(&metadata).unwrap();
        let manager = SessionManager::new(
            Arc::new(FakeProvider::default()),
            storage.clone(),
            Arc::new(MemoryDiagnostics::default()),
        )
        .with_session_roots(SessionRoots {
            worktrees: Some(worktrees),
            ..SessionRoots::default()
        });
        let roots = SessionRoots {
            worktrees: manager.roots.worktrees.clone(),
            ..SessionRoots::default()
        };

        let archival = manager
            .archive_session("archivable-session", &roots)
            .await
            .unwrap();

        assert!(
            matches!(
                archival.worktree,
                Some(ArchiveOutcome::Captured {
                    patch_saved: true,
                    ..
                })
            ),
            "expected the worktree to be captured: {:?}",
            archival.worktree
        );
        assert!(!worktree.exists(), "the worktree was not discarded");
        assert!(
            repository_git.branch_exists("gcabb/archive-me"),
            "archiving took the branch with it, leaving nothing to restore from"
        );
        assert!(
            storage.list_sessions().unwrap().is_empty(),
            "an archived session is still visible to the client"
        );
        assert_eq!(storage.list_archived_sessions().unwrap().len(), 1);

        let restoration = manager
            .unarchive_session("archivable-session")
            .await
            .unwrap();

        assert!(
            matches!(
                restoration.worktree,
                Some(RestoreOutcome::Recreated {
                    patch_applied: true,
                    ..
                })
            ),
            "expected the worktree to be rebuilt: {:?}",
            restoration.worktree
        );
        assert_eq!(storage.list_sessions().unwrap().len(), 1);
        assert!(storage.list_archived_sessions().unwrap().is_empty());
        assert_eq!(read_text(&worktree.join("committed.txt")), "committed\n");
        assert_eq!(
            read_text(&worktree.join("base.txt")),
            "modified\n",
            "uncommitted changes did not survive archiving"
        );
        assert_eq!(
            read_text(&worktree.join("scratch.txt")),
            "untracked\n",
            "untracked files did not survive archiving"
        );
    }

    /// A chat has no worktree, so archiving is purely a visibility change.
    /// A second archive must not overwrite the first one's record: that record
    /// holds the only copy of the session's uncommitted work.
    #[tokio::test]
    async fn archiving_twice_keeps_the_first_archive_intact() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let worktrees = directory.path().join("worktrees");
        let worktree = worktrees.join("repository").join("session");
        initialise_repository(&repository);
        GitService::new(&repository)
            .create_worktree(&worktree, "gcabb/archive-twice", "main")
            .unwrap();
        std::fs::write(worktree.join("base.txt"), "precious\n").unwrap();

        let metadata = SessionMetadata {
            id: "twice-session".to_owned(),
            sdk_session_id: "sdk-twice".to_owned(),
            project_path: worktree.to_string_lossy().into_owned(),
            repository_root: Some(repository.to_string_lossy().into_owned()),
            title: "Archive twice".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Project,
            model: None,
            mode: None,
            base_ref: Some("main".to_owned()),
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        storage.upsert_session(&metadata).unwrap();
        let manager = SessionManager::new(
            Arc::new(FakeProvider::default()),
            storage.clone(),
            Arc::new(MemoryDiagnostics::default()),
        );
        let roots = SessionRoots {
            worktrees: Some(worktrees),
            ..SessionRoots::default()
        };

        manager
            .archive_session("twice-session", &roots)
            .await
            .unwrap();
        manager
            .archive_session("twice-session", &roots)
            .await
            .unwrap();

        let archived = storage.session_archive("twice-session").unwrap().unwrap();
        assert_eq!(archived.branch.as_deref(), Some("gcabb/archive-twice"));
        assert!(
            archived
                .patch
                .as_deref()
                .is_some_and(|patch| patch.contains("precious")),
            "a second archive destroyed the first one's saved work"
        );
    }

    /// A rebuild that cannot even start must not consume the archive: the
    /// patch is the only copy of the user's uncommitted work.
    #[tokio::test]
    async fn a_failed_rebuild_leaves_the_session_archived_with_its_work_intact() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let worktrees = directory.path().join("worktrees");
        let worktree = worktrees.join("repository").join("session");
        initialise_repository(&repository);
        let repository_git = GitService::new(&repository);
        repository_git
            .create_worktree(&worktree, "gcabb/doomed-restore", "main")
            .unwrap();
        std::fs::write(worktree.join("base.txt"), "precious\n").unwrap();

        let metadata = SessionMetadata {
            id: "unrestorable-session".to_owned(),
            sdk_session_id: "sdk-unrestorable".to_owned(),
            project_path: worktree.to_string_lossy().into_owned(),
            repository_root: Some(repository.to_string_lossy().into_owned()),
            title: "Unrestorable".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Project,
            model: None,
            mode: None,
            base_ref: Some("main".to_owned()),
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        storage.upsert_session(&metadata).unwrap();
        let manager = SessionManager::new(
            Arc::new(FakeProvider::default()),
            storage.clone(),
            Arc::new(MemoryDiagnostics::default()),
        );
        let roots = SessionRoots {
            worktrees: Some(worktrees),
            ..SessionRoots::default()
        };
        manager
            .archive_session("unrestorable-session", &roots)
            .await
            .unwrap();
        // Delete the branch out from under the archive, so the worktree can no
        // longer be recreated.
        git(&repository, &["branch", "-D", "gcabb/doomed-restore"]);

        let error = manager
            .unarchive_session("unrestorable-session")
            .await
            .expect_err("rebuilding from a deleted branch must fail");

        assert!(matches!(
            error,
            SessionManagerError::ArchiveRestoreFailed { .. }
        ));
        let archived = storage.list_archived_sessions().unwrap();
        assert_eq!(archived.len(), 1, "a failed restore consumed the archive");
        assert!(
            archived[0]
                .archive
                .patch
                .as_deref()
                .is_some_and(|patch| patch.contains("precious")),
            "the only copy of the uncommitted work was discarded"
        );
    }

    #[tokio::test]
    async fn archiving_a_chat_only_hides_it() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let metadata = SessionMetadata {
            id: "chat-session".to_owned(),
            sdk_session_id: "sdk-chat".to_owned(),
            project_path: "/tmp".to_owned(),
            repository_root: None,
            title: "A chat".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Chat,
            model: None,
            mode: None,
            base_ref: None,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        storage.upsert_session(&metadata).unwrap();
        let manager = SessionManager::new(
            Arc::new(FakeProvider::default()),
            storage.clone(),
            Arc::new(MemoryDiagnostics::default()),
        );

        let archival = manager
            .archive_session("chat-session", &SessionRoots::default())
            .await
            .unwrap();

        assert!(archival.worktree.is_none());
        assert!(storage.list_sessions().unwrap().is_empty());

        manager.unarchive_session("chat-session").await.unwrap();

        assert_eq!(storage.list_sessions().unwrap().len(), 1);
    }

    /// Deleting an archived session must still work, and must take its archive
    /// record with it rather than orphaning a patch.
    #[tokio::test]
    async fn deleting_an_archived_session_clears_its_archive() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let metadata = SessionMetadata {
            id: "doomed-session".to_owned(),
            sdk_session_id: "sdk-doomed".to_owned(),
            project_path: "/tmp".to_owned(),
            repository_root: None,
            title: "Doomed".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Chat,
            model: None,
            mode: None,
            base_ref: None,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        storage.upsert_session(&metadata).unwrap();
        let manager = SessionManager::new(
            Arc::new(FakeProvider::default()),
            storage.clone(),
            Arc::new(MemoryDiagnostics::default()),
        );
        manager
            .archive_session("doomed-session", &SessionRoots::default())
            .await
            .unwrap();

        manager
            .delete_session("doomed-session", &SessionRoots::default())
            .await
            .unwrap();

        assert!(storage.list_archived_sessions().unwrap().is_empty());
        assert!(!storage.session_exists("doomed-session").unwrap());
    }

    #[tokio::test]
    async fn missing_managed_worktree_is_recreated_before_resume() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let worktrees = directory.path().join("worktrees");
        let worktree = worktrees.join("repository").join("session");
        initialise_repository(&repository);
        let git = GitService::new(&repository);
        git.create_worktree(&worktree, "gcabb/recover", "main")
            .unwrap();
        let provider = Arc::new(FakeProvider::default());
        provider.start().await.unwrap();
        let seeded = provider
            .create_session(SessionRequest {
                working_directory: worktree.clone(),
                model: None,
                mode: None,
                reasoning_effort: None,
                context_tier: None,
                auto_approve_tools: true,
            })
            .await
            .unwrap();
        provider.disconnect(&seeded.sdk_session_id).await.unwrap();

        let metadata = SessionMetadata {
            id: "recoverable-session".to_owned(),
            sdk_session_id: seeded.sdk_session_id,
            project_path: worktree.to_string_lossy().into_owned(),
            repository_root: Some(repository.to_string_lossy().into_owned()),
            title: "Recover worktree".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Project,
            model: None,
            mode: None,
            base_ref: Some("main".to_owned()),
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        storage.upsert_session(&metadata).unwrap();
        let mut snapshot = SessionSnapshot::new(metadata);
        snapshot.changes.branch = Some("gcabb/recover".to_owned());
        storage.write_snapshot(&snapshot).unwrap();
        git.remove_worktree(&worktree).unwrap();
        assert!(!worktree.exists());

        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics)
            .with_session_roots(SessionRoots {
                worktrees: Some(worktrees),
                ..SessionRoots::default()
            });

        let (_, report) = manager.start().await.unwrap();

        assert!(
            report.failed.is_empty(),
            "managed restore failed: {:?}",
            report
                .failed
                .iter()
                .map(|failure| failure.error.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(report.restored.len(), 1);
        assert_eq!(report.restored[0].snapshot().status, SessionStatus::Idle);
        assert!(worktree.join("base.txt").exists());
        assert_eq!(provider.active_sessions().await, 1);
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
        let factory = FakeProviderFactory::default();
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(factory.clone(), storage, diagnostics.clone());
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(std::env::temp_dir()))
            .await
            .unwrap();
        let provider = factory.providers()[0].clone();
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
        assert!(!provider.is_started());
        assert!(!diagnostics.events().is_empty());
    }

    #[tokio::test]
    async fn interaction_round_trip_waits_for_native_response() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics);
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(std::env::temp_dir()))
            .await
            .unwrap();
        let sdk_session_id = handle.snapshot().metadata.sdk_session_id.clone();
        let mut snapshots = handle.subscribe();
        let request = app_model::InteractionRequest {
            id: "permission-1".to_owned(),
            session_id: sdk_session_id.clone(),
            kind: app_model::InteractionKind::Permission,
            title: "Permission required".to_owned(),
            message: "Run cargo test?".to_owned(),
            choices: vec!["Allow once".to_owned(), "Deny".to_owned()],
            allow_freeform: true,
            details: Value::Null,
        };

        let response = provider
            .request_interaction(&sdk_session_id, request)
            .await
            .unwrap();
        snapshots
            .wait_for(|snapshot| !snapshot.pending_interactions.is_empty())
            .await
            .unwrap();
        provider
            .emit(
                &sdk_session_id,
                json!({
                    "id": "nested-event",
                    "agentId": "agent-1",
                    "type": "assistant.turn_start",
                    "data": {}
                }),
            )
            .await
            .unwrap();
        snapshots
            .wait_for(|snapshot| snapshot.last_sequence == 1)
            .await
            .unwrap();
        assert_eq!(snapshots.borrow().status, SessionStatus::Waiting);
        handle
            .respond("permission-1", InteractionResponse::Approve)
            .await
            .unwrap();

        assert_eq!(response.await.unwrap(), InteractionResponse::Approve);
        assert!(handle.snapshot().pending_interactions.is_empty());
    }

    #[tokio::test]
    async fn controls_update_snapshot_and_persist_metadata() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage.clone(), diagnostics);
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(std::env::temp_dir()))
            .await
            .unwrap();

        handle.set_model("model-1").await.unwrap();
        handle.set_mode("plan").await.unwrap();
        handle.set_reasoning_effort("high").await.unwrap();
        handle
            .set_model_with_reasoning_effort("model-2", Some("medium".to_owned()))
            .await
            .unwrap();

        let snapshot = handle.snapshot();
        assert_eq!(snapshot.controls.model.as_deref(), Some("model-2"));
        assert_eq!(snapshot.controls.mode.as_deref(), Some("plan"));
        assert_eq!(
            snapshot.controls.reasoning_effort.as_deref(),
            Some("medium")
        );
        let metadata = storage.list_sessions().unwrap();
        assert_eq!(metadata[0].model.as_deref(), Some("model-2"));
        assert_eq!(metadata[0].mode.as_deref(), Some("plan"));
    }

    #[tokio::test]
    async fn changing_base_ref_recomputes_changes_and_persists_selection() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage.clone(), diagnostics);
        manager.start().await.unwrap();
        let directory = tempdir().unwrap();
        initialise_repository(directory.path());
        git(directory.path(), &["branch", "feature"]);
        let missing_remote = directory.path().join("missing-remote");
        git(
            directory.path(),
            &["remote", "add", "origin", &missing_remote.to_string_lossy()],
        );
        git(
            directory.path(),
            &["update-ref", "refs/remotes/origin/feature", "feature"],
        );
        git(
            directory.path(),
            &["branch", "--set-upstream-to=origin/feature", "feature"],
        );
        let mut create = request(directory.path().to_owned());
        create.repository_root = Some(directory.path().to_string_lossy().into_owned());
        create.base_ref = Some("main".to_owned());
        let handle = manager.create_session(create).await.unwrap();

        handle.set_base_ref("feature").await.unwrap();

        let snapshot = handle.snapshot();
        assert_eq!(snapshot.metadata.base_ref.as_deref(), Some("feature"));
        assert_eq!(snapshot.changes.base_label.as_deref(), Some("feature"));
        assert_eq!(
            snapshot.changes.tracking_ref.as_deref(),
            Some("origin/feature")
        );
        let metadata = storage.list_sessions().unwrap();
        assert_eq!(metadata[0].base_ref.as_deref(), Some("feature"));
    }

    /// Regression: earlier builds registered one project per worktree, so a
    /// repository with several session worktrees appeared as several unrelated
    /// projects named after generated branch directories.
    /// Deleting a session must take its event log and snapshots with it, so a
    /// deleted session cannot be resurrected by recovery.
    #[tokio::test]
    async fn deleting_a_session_removes_its_events_and_snapshots() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage.clone(), diagnostics);
        manager.start().await.unwrap();
        let directory = tempdir().unwrap();
        let handle = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();
        let id = handle.id().to_owned();
        manager.set_selected_session(Some(&id)).unwrap();
        handle.send("hello").await.unwrap();

        manager
            .delete_session(&id, &SessionRoots::default())
            .await
            .unwrap();

        assert!(storage.list_sessions().unwrap().is_empty());
        assert!(manager.sessions().await.is_empty());
        // The selection must not dangle on a session that no longer exists.
        assert!(manager.selected_session().unwrap().is_none());
        assert!(storage.recover_session(&id).is_err());
    }

    #[tokio::test]
    async fn renaming_a_session_updates_the_snapshot_and_storage() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage.clone(), diagnostics);
        manager.start().await.unwrap();
        let directory = tempdir().unwrap();
        let handle = manager
            .create_session(request(directory.path().to_owned()))
            .await
            .unwrap();

        manager
            .rename_session(handle.id(), "Renamed session")
            .await
            .unwrap();

        assert_eq!(handle.snapshot().metadata.title, "Renamed session");
        assert_eq!(handle.snapshot().metadata.title_source, TitleSource::Manual);
        let stored = storage.list_sessions().unwrap();
        assert_eq!(stored[0].title, "Renamed session");
        assert_eq!(stored[0].title_source, TitleSource::Manual);
    }

    #[tokio::test]
    async fn generated_title_replaces_only_the_fallback_and_persists() {
        let provider = Arc::new(FakeProvider::default());
        provider
            .set_generated_title("  \"Improve session naming.\"  ")
            .await;
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage.clone(), diagnostics);
        manager.start().await.unwrap();
        let mut create = request(std::env::temp_dir());
        create.title = "Can you improve session naming?".to_owned();
        create.title_source = TitleSource::Fallback;
        let handle = manager.create_session(create).await.unwrap();

        manager
            .generate_session_title(handle.id(), "Can you improve session naming?")
            .await
            .unwrap();

        assert_eq!(handle.snapshot().metadata.title, "Improve session naming");
        assert_eq!(
            handle.snapshot().metadata.title_source,
            TitleSource::Generated
        );
        let stored = storage.list_sessions().unwrap();
        assert_eq!(stored[0].title, "Improve session naming");
        assert_eq!(stored[0].title_source, TitleSource::Generated);
    }

    #[tokio::test]
    async fn manual_title_is_never_replaced_by_generation() {
        let provider = Arc::new(FakeProvider::default());
        provider.set_generated_title("Generated replacement").await;
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage, diagnostics);
        manager.start().await.unwrap();
        let mut create = request(std::env::temp_dir());
        create.title = "Initial fallback title".to_owned();
        create.title_source = TitleSource::Fallback;
        let handle = manager.create_session(create).await.unwrap();
        handle.rename("User chosen title").await.unwrap();

        manager
            .generate_session_title(handle.id(), "Original prompt")
            .await
            .unwrap();

        assert_eq!(handle.snapshot().metadata.title, "User chosen title");
        assert_eq!(handle.snapshot().metadata.title_source, TitleSource::Manual);
    }

    #[tokio::test]
    async fn title_generation_failure_keeps_the_fallback() {
        let provider = Arc::new(FakeProvider::default());
        provider.fail_title_generation(true);
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage, diagnostics);
        manager.start().await.unwrap();
        let mut create = request(std::env::temp_dir());
        create.title = "Reliable fallback title".to_owned();
        create.title_source = TitleSource::Fallback;
        let handle = manager.create_session(create).await.unwrap();

        assert!(
            manager
                .generate_session_title(handle.id(), "Original prompt")
                .await
                .is_err()
        );
        assert_eq!(handle.snapshot().metadata.title, "Reliable fallback title");
        assert_eq!(
            handle.snapshot().metadata.title_source,
            TitleSource::Fallback
        );
    }

    #[test]
    fn generated_titles_must_be_concise_plain_text() {
        assert_eq!(
            normalize_generated_title("## Fix authentication flow!\nExtra explanation"),
            Some("Fix authentication flow".to_owned())
        );
        assert!(normalize_generated_title("One").is_none());
        assert!(normalize_generated_title("This title contains far too many words").is_none());
    }

    #[tokio::test]
    async fn adopting_repository_roots_folds_worktree_projects_into_one() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage.clone(), diagnostics);

        for (id, worktree) in [
            ("s1", "/worktrees/feature-a"),
            ("s2", "/worktrees/feature-b"),
        ] {
            storage
                .upsert_session(&SessionMetadata {
                    id: id.to_owned(),
                    sdk_session_id: format!("sdk-{id}"),
                    project_path: worktree.to_owned(),
                    repository_root: None,
                    title: "Legacy".to_owned(),
                    title_source: TitleSource::Manual,
                    kind: SessionKind::Project,
                    model: None,
                    mode: None,
                    base_ref: None,
                    created_at: "1".to_owned(),
                    updated_at: "1".to_owned(),
                })
                .unwrap();
            storage
                .upsert_project(&ProjectMetadata {
                    id: worktree.to_owned(),
                    path: worktree.to_owned(),
                    name: worktree.rsplit('/').next().unwrap().to_owned(),
                    default_branch: None,
                    last_opened_at: "1".to_owned(),
                })
                .unwrap();
        }
        // The repository itself is also registered, as a current build would.
        storage
            .upsert_project(&ProjectMetadata {
                id: "/src/gcabb".to_owned(),
                path: "/src/gcabb".to_owned(),
                name: "gcabb".to_owned(),
                default_branch: Some("main".to_owned()),
                last_opened_at: "2".to_owned(),
            })
            .unwrap();

        let updated = manager
            .adopt_repository_roots(|path| {
                path.starts_with("/worktrees/")
                    .then(|| "/src/gcabb".to_owned())
                    .or_else(|| Some(path.to_owned()))
            })
            .unwrap();

        assert_eq!(updated, 2);
        let sessions = storage.list_sessions().unwrap();
        for session in &sessions {
            assert_eq!(session.repository_root.as_deref(), Some("/src/gcabb"));
            assert_eq!(session.project_key(), "/src/gcabb");
        }
        // Both sessions keep their own worktree as the working directory.
        let mut worktrees: Vec<&str> = sessions.iter().map(|s| s.project_path.as_str()).collect();
        worktrees.sort_unstable();
        assert_eq!(worktrees, ["/worktrees/feature-a", "/worktrees/feature-b"]);

        // The worktree-shaped project rows are gone; the repository remains.
        let projects = manager.projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "gcabb");
    }

    /// Backfill must not run twice or clobber an already-recorded repository.
    #[tokio::test]
    async fn adopting_repository_roots_is_idempotent() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider, storage.clone(), diagnostics);
        storage
            .upsert_session(&SessionMetadata {
                id: "s1".to_owned(),
                sdk_session_id: "sdk-s1".to_owned(),
                project_path: "/worktrees/feature-a".to_owned(),
                repository_root: Some("/src/other".to_owned()),
                title: "Already adopted".to_owned(),
                title_source: TitleSource::Manual,
                kind: SessionKind::Project,
                model: None,
                mode: None,
                base_ref: None,
                created_at: "1".to_owned(),
                updated_at: "1".to_owned(),
            })
            .unwrap();

        let updated = manager
            .adopt_repository_roots(|_| Some("/src/gcabb".to_owned()))
            .unwrap();

        assert_eq!(updated, 0);
        assert_eq!(
            storage.list_sessions().unwrap()[0]
                .repository_root
                .as_deref(),
            Some("/src/other")
        );
    }

    #[tokio::test]
    async fn closed_session_can_resume_without_restarting_manager() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics);
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(std::env::temp_dir()))
            .await
            .unwrap();
        let id = handle.id().to_owned();

        manager.close_session(&id).await.unwrap();
        assert!(manager.session(&id).await.is_err());
        assert_eq!(provider.active_sessions().await, 0);

        let resumed = manager.resume_closed_session(&id).await.unwrap();
        assert_eq!(resumed.id(), id);
        assert_eq!(provider.active_sessions().await, 1);
    }

    #[tokio::test]
    async fn close_cancels_pending_interaction_callback() {
        let provider = Arc::new(FakeProvider::default());
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let diagnostics = Arc::new(MemoryDiagnostics::default());
        let manager = SessionManager::new(provider.clone(), storage, diagnostics);
        manager.start().await.unwrap();
        let handle = manager
            .create_session(request(std::env::temp_dir()))
            .await
            .unwrap();
        let mut snapshots = handle.subscribe();
        let sdk_session_id = handle.snapshot().metadata.sdk_session_id.clone();
        let response = provider
            .request_interaction(
                &sdk_session_id,
                app_model::InteractionRequest {
                    id: "input-1".to_owned(),
                    session_id: sdk_session_id.clone(),
                    kind: app_model::InteractionKind::UserInput,
                    title: "Input".to_owned(),
                    message: "Continue?".to_owned(),
                    choices: Vec::new(),
                    allow_freeform: true,
                    details: Value::Null,
                },
            )
            .await
            .unwrap();
        snapshots
            .wait_for(|snapshot| !snapshot.pending_interactions.is_empty())
            .await
            .unwrap();

        manager.close_session(handle.id()).await.unwrap();

        assert_eq!(response.await.unwrap(), InteractionResponse::Cancel);
    }
}
