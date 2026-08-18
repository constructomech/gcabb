use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod capability;
pub mod changes;
pub mod plan;
pub mod queue;
pub mod tools;

pub use capability::{Capability, CapabilityId, CapabilityReport, CapabilityStatus};
pub use changes::{ChangeStage, ChangeStatus, ChangedFile, ChangesView, DiffStats};
pub use plan::{AgentPlan, AgentTodo, AgentTodoStatus};
pub use queue::{QueueDelivery, QueueItem, QueueItemState, QueueView};
pub use tools::{
    InvocationState, OutputMetadata, OutputStreamKind, OutputStreamUpdate, TerminalSession,
    TerminalState, ToolActivity, ToolCatalog, ToolClass, ToolDescriptor, ToolInvocation,
    ToolSource,
};

pub const DOMAIN_EVENT_VERSION: u16 = 1;
/// Version 8 retains permission requests and their outcomes in the timeline.
pub const SNAPSHOT_VERSION: u16 = 8;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Observed,
    Inferred,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Model,
    Tool,
    Subagent,
    Terminal,
    Permission,
    File,
    Session,
    System,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Queued,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Recovering,
    Running,
    Waiting,
    #[default]
    Idle,
    Failed,
    Cancelled,
    Disconnected,
    /// Persisted history is available, but the session's working directory is not.
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    Fallback,
    Generated,
    #[default]
    Manual,
}

impl TitleSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Generated => "generated",
            Self::Manual => "manual",
        }
    }

    #[must_use]
    pub fn from_str_or_default(value: Option<&str>) -> Self {
        match value {
            Some("fallback") => Self::Fallback,
            Some("generated") => Self::Generated,
            _ => Self::Manual,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRole {
    User,
    Assistant,
    System,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptState {
    /// A steering message has been accepted but has not entered model context.
    Pending,
    Streaming,
    Complete,
    /// Streaming stopped before the runtime committed the message.
    ///
    /// The partial text stays on screen because the user saw it, but the
    /// runtime never wrote an `assistant.message` for it, so the model cannot
    /// see it on the next turn. Marking it keeps the transcript honest about
    /// what is actually in context.
    Interrupted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TranscriptMessage {
    pub id: String,
    pub role: TranscriptRole,
    pub content: String,
    pub state: TranscriptState,
    pub timestamp: String,
    /// Event sequence this message first appeared at.
    ///
    /// Interleaving messages with tool calls needs a total order, and the
    /// reducer's sequence is monotonic and app-owned, unlike event timestamps.
    #[serde(default)]
    pub sequence: u64,
    /// What was attached to this message, as the runtime echoed it back.
    ///
    /// Taken from the event rather than from composer state so the transcript
    /// shows what the model actually received.
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
}

/// An attachment recorded on a message in the transcript.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageAttachment {
    pub display_name: String,
    pub is_image: bool,
    /// Where the runtime stored the attachment, when it said.
    ///
    /// Needed to show the image itself rather than only its name. The runtime
    /// copies pasted images into its own workspace, so this path is the one
    /// that outlives the composer.
    #[serde(default)]
    pub path: Option<String>,
}

/// Read the attachments the runtime echoed back on a user message.
fn message_attachments(event: &DomainEvent) -> Vec<MessageAttachment> {
    let Some(attachments) = event.details.get("attachments").and_then(Value::as_array) else {
        return Vec::new();
    };
    attachments
        .iter()
        .map(|attachment| {
            let path = attachment.get("path").and_then(Value::as_str).unwrap_or("");
            let display_name = attachment
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map_or_else(
                    || {
                        std::path::Path::new(path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("attachment")
                            .to_owned()
                    },
                    str::to_owned,
                );
            // The runtime declares a MIME type only sometimes, so fall back to
            // the extension rather than showing a generic file for a picture.
            let is_image = attachment
                .get("mimeType")
                .and_then(Value::as_str)
                .is_some_and(|mime| mime.starts_with("image/"))
                || {
                    let lowered = path.to_lowercase();
                    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"]
                        .iter()
                        .any(|extension| lowered.ends_with(extension))
                };
            MessageAttachment {
                display_name,
                is_image,
                path: (!path.is_empty()).then(|| path.to_owned()),
            }
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Permission,
    Elicitation,
    UserInput,
    ExitPlanMode,
    AutoModeSwitch,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InteractionRequest {
    pub id: String,
    pub session_id: String,
    pub kind: InteractionKind,
    pub title: String,
    pub message: String,
    pub choices: Vec<String>,
    pub allow_freeform: bool,
    pub details: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InteractionRecord {
    pub request: InteractionRequest,
    /// Event sequence after which the interaction was requested.
    pub sequence: u64,
    pub response: Option<InteractionResponse>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionResponse {
    Approve,
    ApproveForSession,
    ApproveForLocation,
    ApprovePermanently,
    Reject { feedback: Option<String> },
    Submit { value: Value, freeform: bool },
    Cancel,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionControls {
    pub model: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub context_tier: Option<String>,
    pub available_models: Vec<ModelOption>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
    pub supported_reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub context_windows: Vec<ContextWindowOption>,
}

/// A selectable context-window tier for a model. Models that expose an
/// extended tier report more than one; the rest report at most one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextWindowOption {
    pub tier: String,
    pub max_tokens: Option<u64>,
}

/// Something attached to a prompt.
///
/// Screenshots are how interface defects get reported, so a session that
/// cannot receive one cannot be used to work on a user interface. A chosen or
/// dropped file is referenced by path, so the runtime opens it and the bytes
/// never cross the RPC boundary. A pasted image has no path to reference, so
/// it travels as bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptAttachment {
    /// A file on disk, referenced by path.
    File {
        /// Absolute path to the attached file.
        path: String,
        /// Label shown in the composer.
        display_name: String,
    },
    /// Raw image bytes, typically pasted from the clipboard.
    Image {
        /// Base64-encoded image data.
        data: String,
        /// MIME type of the data.
        mime_type: String,
        /// Label shown in the composer.
        display_name: String,
    },
}

impl PromptAttachment {
    /// Build an attachment from a chosen or dropped path.
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Self {
        let display_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment")
            .to_owned();
        Self::File {
            path: path.to_string_lossy().into_owned(),
            display_name,
        }
    }

    /// Build an attachment from image bytes with no backing file.
    #[must_use]
    pub fn from_image_bytes(bytes: &[u8], mime_type: impl Into<String>, index: usize) -> Self {
        use base64::Engine as _;
        Self::Image {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            mime_type: mime_type.into(),
            display_name: format!("Pasted image {index}"),
        }
    }

    /// Label shown in the composer.
    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            Self::File { display_name, .. } | Self::Image { display_name, .. } => display_name,
        }
    }

    /// A value that distinguishes this attachment from another.
    ///
    /// Two picks of the same file are the same attachment, but two pastes are
    /// not: a user who pastes twice meant to attach two images.
    #[must_use]
    pub fn identity(&self) -> String {
        match self {
            Self::File { path, .. } => path.clone(),
            Self::Image {
                data, display_name, ..
            } => format!("{display_name}:{}", data.len()),
        }
    }

    /// The decoded bytes of a pasted image, if this is one.
    ///
    /// Kept here so base64 stays an encoding detail of the model rather than
    /// something the UI has to know about.
    #[must_use]
    pub fn image_bytes(&self) -> Option<Vec<u8>> {
        use base64::Engine as _;
        match self {
            Self::Image { data, .. } => base64::engine::general_purpose::STANDARD.decode(data).ok(),
            Self::File { .. } => None,
        }
    }

    /// The declared MIME type, for attachments that have one.
    #[must_use]
    pub fn mime_type(&self) -> Option<&str> {
        match self {
            Self::Image { mime_type, .. } => Some(mime_type),
            Self::File { .. } => None,
        }
    }

    /// The path this attachment lives at, for attachments backed by a file.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::File { path, .. } => Some(path),
            Self::Image { .. } => None,
        }
    }

    /// Whether this attachment is an image.
    #[must_use]
    pub fn is_image(&self) -> bool {
        match self {
            Self::Image { .. } => true,
            Self::File { path, .. } => {
                let lowered = path.to_lowercase();
                [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"]
                    .iter()
                    .any(|extension| lowered.ends_with(extension))
            }
        }
    }
}

/// Where a new session runs.
///
/// A worktree gives the session its own checkout so parallel sessions in one
/// repository cannot fight over the working tree or the checked-out branch.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLocation {
    /// A new linked worktree, created for this session.
    #[default]
    NewWorktree,
    /// The repository already on disk, shared with everything else using it.
    LocalRepository,
}

impl SessionLocation {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NewWorktree => "New worktree",
            Self::LocalRepository => "Local repository",
        }
    }

    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::NewWorktree => "Creates a separate copy for this session",
            Self::LocalRepository => "Works in the repository already on your machine",
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewWorktree => "new-worktree",
            Self::LocalRepository => "local-repository",
        }
    }

    #[must_use]
    pub fn from_str_or_default(value: &str) -> Self {
        match value {
            "local-repository" => Self::LocalRepository,
            _ => Self::NewWorktree,
        }
    }
}

/// Whether a session is attached to a repository or is a standalone chat.
///
/// Chats have no project, no worktree, and therefore no changes view. They
/// exist so the app can be used for questions and planning that are not tied
/// to a checkout.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Project,
    Chat,
}

impl SessionKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Chat => "chat",
        }
    }

    #[must_use]
    pub fn from_str_or_default(value: Option<&str>) -> Self {
        match value {
            Some("chat") => Self::Chat,
            _ => Self::Project,
        }
    }

    #[must_use]
    pub const fn is_chat(self) -> bool {
        matches!(self, Self::Chat)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionMetadata {
    pub id: String,
    pub sdk_session_id: String,
    /// Working directory the CLI runs in — the session's worktree.
    pub project_path: String,
    /// Repository the session belongs to, used to group sessions by project.
    ///
    /// A repository has one main checkout and any number of worktrees. Sessions
    /// live in worktrees but belong to the repository, so grouping by the
    /// worktree path would show one "project" per worktree instead of one per
    /// repository. `None` for sessions recorded before this was tracked; those
    /// fall back to grouping by `project_path`.
    #[serde(default)]
    pub repository_root: Option<String>,
    pub title: String,
    /// Who last chose the title. Legacy sessions default to manual so an
    /// automatic rename can never overwrite an existing user-visible name.
    #[serde(default)]
    pub title_source: TitleSource,
    #[serde(default)]
    pub kind: SessionKind,
    pub model: Option<String>,
    pub mode: Option<String>,
    /// Git ref the changes view compares against.
    ///
    /// This is the logical branch selected by the user. Change refreshes resolve
    /// its current upstream and merge-base commit.
    #[serde(default)]
    pub base_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl SessionMetadata {
    /// Project this session groups under.
    #[must_use]
    pub fn project_key(&self) -> &str {
        self.repository_root
            .as_deref()
            .unwrap_or(&self.project_path)
    }

    /// Whether this session is a standalone chat with no repository.
    #[must_use]
    pub const fn is_chat(&self) -> bool {
        self.kind.is_chat()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMetadata {
    pub id: String,
    pub path: String,
    pub name: String,
    pub default_branch: Option<String>,
    pub last_opened_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DomainEvent {
    pub version: u16,
    pub sequence: u64,
    pub session_id: String,
    pub id: String,
    pub parent_id: Option<String>,
    pub agent_id: Option<String>,
    pub timestamp: String,
    pub source_type: String,
    pub kind: ActivityKind,
    pub state: ActivityState,
    pub visibility: Visibility,
    pub summary: String,
    pub details: Value,
    pub raw: Value,
}

const DIAGNOSTIC_EVENT_LIMIT: usize = 40;

/// Compact, user-facing telemetry projected from the SDK event stream.
///
/// The complete events remain in `domain_events`; this state keeps only the
/// information needed to explain an in-flight pause without embedding another
/// copy of the event log in every snapshot.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AgentDiagnostics {
    pub turn_started_at: Option<String>,
    pub last_event_at: Option<String>,
    pub last_event_type: Option<String>,
    pub latest_intent: Option<String>,
    pub activity: Option<String>,
    pub model: Option<String>,
    pub turn_id: Option<String>,
    pub response_bytes: Option<u64>,
    pub compaction: Option<CompactionDiagnostics>,
    pub last_usage: Option<UsageDiagnostics>,
    #[serde(default)]
    pub event_counts: BTreeMap<String, u64>,
    #[serde(default)]
    pub recent_events: Vec<DiagnosticSignal>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompactionDiagnostics {
    pub started_at: String,
    pub trigger: Option<String>,
    pub current_tokens: Option<u64>,
    pub token_limit: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UsageDiagnostics {
    pub model: Option<String>,
    pub duration_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DiagnosticSignal {
    pub sequence: u64,
    pub timestamp: String,
    pub event_type: String,
    pub summary: String,
}

impl DomainEvent {
    #[must_use]
    pub fn from_sdk_event(raw: &Value) -> Self {
        Self::from_sdk_event_for("", 0, raw)
    }

    #[must_use]
    pub fn from_sdk_event_for(session_id: &str, sequence: u64, raw: &Value) -> Self {
        let event_type = raw.get("type").and_then(Value::as_str).unwrap_or("unknown");
        let data = raw.get("data").cloned().unwrap_or(Value::Null);

        Self {
            version: DOMAIN_EVENT_VERSION,
            sequence,
            session_id: session_id.to_owned(),
            id: string_field(raw, "id")
                .unwrap_or_else(|| format!("{session_id}:{sequence}:{event_type}")),
            parent_id: string_field(raw, "parent_id").or_else(|| string_field(raw, "parentId")),
            agent_id: string_field(raw, "agent_id").or_else(|| string_field(raw, "agentId")),
            timestamp: string_field(raw, "timestamp").unwrap_or_default(),
            source_type: event_type.to_owned(),
            kind: classify_kind(event_type),
            state: classify_state(event_type, &data),
            visibility: Visibility::Observed,
            summary: summarize(event_type, &data),
            details: data,
            raw: raw.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub version: u16,
    pub metadata: SessionMetadata,
    pub status: SessionStatus,
    pub last_sequence: u64,
    #[serde(default)]
    pub transcript: Vec<TranscriptMessage>,
    #[serde(default)]
    pub pending_interactions: Vec<InteractionRequest>,
    #[serde(default)]
    pub interaction_history: Vec<InteractionRecord>,
    #[serde(default)]
    pub controls: SessionControls,
    /// Tools the runtime advertises for this session, proven via `tools.list`.
    #[serde(default)]
    pub tool_catalog: ToolCatalog,
    /// Projected tool invocations and shellId-keyed terminals.
    #[serde(default)]
    pub tool_activity: ToolActivity,
    /// Capability status for the inherited runtime features Phase 3 needs.
    #[serde(default)]
    pub capabilities: CapabilityReport,
    /// Worktree changes against the session's recorded base.
    #[serde(default)]
    pub changes: ChangesView,
    /// Prompts the developer has queued for this session.
    ///
    /// Skipped when serializing: the queue is durable state owned by the
    /// `queue_items` table, not a projection of the event log. Persisting a
    /// second copy inside snapshots would let the two disagree after a
    /// restart, so the session actor reloads it from storage instead.
    #[serde(skip)]
    pub queue: QueueView,
    /// The agent's own task list, as reported by the runtime.
    ///
    /// Skipped when serializing: the runtime owns these rows and is re-read
    /// whenever it signals a change, so a persisted copy would only ever be
    /// a stale duplicate.
    #[serde(skip)]
    pub agent_plan: AgentPlan,
    /// Latest SDK lifecycle and progress signals for user-facing diagnostics.
    #[serde(default)]
    pub diagnostics: AgentDiagnostics,
    pub last_error: Option<String>,
    #[serde(skip)]
    seen_event_ids: HashSet<String>,
}

impl SessionSnapshot {
    #[must_use]
    pub fn new(metadata: SessionMetadata) -> Self {
        let controls = SessionControls {
            model: metadata.model.clone(),
            mode: metadata.mode.clone(),
            ..SessionControls::default()
        };
        Self {
            version: SNAPSHOT_VERSION,
            metadata,
            status: SessionStatus::Starting,
            last_sequence: 0,
            transcript: Vec::new(),
            pending_interactions: Vec::new(),
            interaction_history: Vec::new(),
            controls,
            tool_catalog: ToolCatalog::default(),
            tool_activity: ToolActivity::default(),
            capabilities: CapabilityReport::default(),
            changes: ChangesView::default(),
            queue: QueueView::default(),
            agent_plan: AgentPlan::default(),
            diagnostics: AgentDiagnostics::default(),
            last_error: None,
            seen_event_ids: HashSet::new(),
        }
    }

    /// Rebuild the indexes that are derived rather than stored.
    ///
    /// Event ids are not among them: the event log is the record of which
    /// events were seen, so `seen_event_ids` covers only what this instance
    /// has applied since it was created.
    pub fn restore_indexes(&mut self) {
        self.tool_activity.restore_indexes();
    }

    /// Reconcile state that cannot still be active after reconnecting a runtime.
    pub fn reconcile_after_restart(&mut self, timestamp: &str) {
        self.cancel_pending_interactions();
        mark_streaming_interrupted(&mut self.transcript);
        tools::mark_running_invocations_cancelled(&mut self.tool_activity, timestamp);
        tools::mark_running_terminals_cancelled(&mut self.tool_activity, timestamp);
        self.diagnostics.activity = None;
        self.diagnostics.compaction = None;
        self.status = SessionStatus::Idle;
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "an applied event is consumed conceptually; taking it by value keeps callers from reusing it"
    )]
    pub fn apply(&mut self, event: DomainEvent) -> ApplyOutcome {
        if event.session_id != self.metadata.id {
            return ApplyOutcome::WrongSession;
        }
        if self.seen_event_ids.contains(&event.id) {
            return ApplyOutcome::Duplicate;
        }
        if event.sequence <= self.last_sequence {
            return ApplyOutcome::OutOfOrder;
        }

        self.last_sequence = event.sequence;
        if event.source_type == "user.message" {
            self.last_error = None;
        }
        if let Some(event_status) = status_for_event(&event) {
            self.status = if self.pending_interactions.is_empty()
                || matches!(
                    event_status,
                    SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Disconnected
                ) {
                event_status
            } else {
                SessionStatus::Waiting
            };
        }
        project_transcript(&mut self.transcript, &event);
        if is_abort(&event) {
            mark_streaming_interrupted(&mut self.transcript);
            tools::mark_running_invocations_cancelled(&mut self.tool_activity, &event.timestamp);
            tools::mark_running_terminals_cancelled(&mut self.tool_activity, &event.timestamp);
        }
        tools::project(&mut self.tool_activity, &event);
        Self::project_diagnostics(&mut self.diagnostics, &event);
        if event.source_type == "session.error" {
            self.last_error = Some(
                event
                    .details
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or(&event.summary)
                    .to_owned(),
            );
        }
        self.seen_event_ids.insert(event.id.clone());
        ApplyOutcome::Applied
    }

    /// Messages, root-agent tool calls, and permission requests in causal order.
    ///
    /// Subagent calls are excluded here and reached through
    /// [`ToolActivity::children_of`], so delegated work appears beneath the
    /// task that requested it rather than flattened into the main thread.
    #[must_use]
    pub fn timeline(&self) -> Vec<TimelineEntry<'_>> {
        let mut entries: Vec<TimelineEntry<'_>> = self
            .transcript
            .iter()
            .map(TimelineEntry::Message)
            .chain(
                self.tool_activity
                    .root_invocations()
                    .into_iter()
                    .map(TimelineEntry::Tool),
            )
            .chain(
                self.interaction_history
                    .iter()
                    .filter(|record| record.request.kind == InteractionKind::Permission)
                    .map(TimelineEntry::Interaction),
            )
            .collect();
        // Sequence is monotonic per session, so this is a stable total order.
        entries.sort_by_key(TimelineEntry::sequence);
        entries
    }

    #[must_use]
    pub fn immutable(self) -> Arc<Self> {
        Arc::new(self)
    }

    pub fn add_interaction(&mut self, request: InteractionRequest) {
        if !self
            .pending_interactions
            .iter()
            .any(|pending| pending.id == request.id)
        {
            if request.kind == InteractionKind::Permission {
                self.interaction_history.push(InteractionRecord {
                    request: request.clone(),
                    sequence: self.last_sequence,
                    response: None,
                });
            }
            self.pending_interactions.push(request);
            self.status = SessionStatus::Waiting;
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the event-to-diagnostic phase mapping is clearest as one exhaustive projection"
    )]
    fn project_diagnostics(diagnostics: &mut AgentDiagnostics, event: &DomainEvent) {
        diagnostics.last_event_at = Some(event.timestamp.clone());
        diagnostics.last_event_type = Some(event.source_type.clone());
        *diagnostics
            .event_counts
            .entry(event.source_type.clone())
            .or_default() += 1;
        if event.agent_id.is_some() {
            return;
        }

        let data = &event.details;
        match event.source_type.as_str() {
            "assistant.turn_start" => {
                diagnostics.turn_started_at = Some(event.timestamp.clone());
                diagnostics.latest_intent = None;
                diagnostics.activity = Some("Preparing model request".to_owned());
                diagnostics.model = string_field(data, "model");
                diagnostics.turn_id = string_field(data, "turnId");
                diagnostics.response_bytes = None;
                diagnostics.compaction = None;
            }
            "assistant.turn_retry" => {
                diagnostics.activity = Some(string_field(data, "reason").map_or_else(
                    || "Retrying model request".to_owned(),
                    |reason| format!("Retrying model request: {reason}"),
                ));
            }
            "model.call_start" => {
                diagnostics.activity = Some("Waiting for model response".to_owned());
                if let Some(model) = string_field(data, "model") {
                    diagnostics.model = Some(model);
                }
            }
            "assistant.intent" => {
                if let Some(intent) = string_field(data, "intent") {
                    diagnostics.latest_intent = Some(intent.clone());
                    diagnostics.activity = Some(intent);
                }
            }
            "assistant.reasoning" | "assistant.reasoning_delta" => {
                diagnostics.activity = Some("Model is reasoning".to_owned());
            }
            "assistant.server_tool_progress" => {
                let kind = string_field(data, "kind").unwrap_or_else(|| "server tool".to_owned());
                let status = string_field(data, "status").unwrap_or_else(|| "running".to_owned());
                diagnostics.activity = Some(format!("{kind}: {status}"));
            }
            "assistant.streaming_delta" => {
                diagnostics.activity = Some("Receiving model response".to_owned());
                diagnostics.response_bytes =
                    data.get("totalResponseSizeBytes").and_then(Value::as_u64);
            }
            "tool.execution_start" => {
                diagnostics.activity = Some(format!(
                    "Running {}",
                    string_field(data, "toolName").unwrap_or_else(|| "tool".to_owned())
                ));
            }
            "tool.execution_progress" => {
                diagnostics.activity = string_field(data, "progressMessage")
                    .or_else(|| Some("Tool is running".to_owned()));
            }
            "tool.execution_complete" => {
                diagnostics.activity = Some("Processing tool result".to_owned());
            }
            "hook.start" => {
                diagnostics.activity = Some(format!(
                    "Running {} hook",
                    string_field(data, "hookType").unwrap_or_else(|| "session".to_owned())
                ));
            }
            "hook.progress" => {
                diagnostics.activity =
                    string_field(data, "message").or_else(|| Some("Hook is running".to_owned()));
            }
            "subagent.started" => {
                diagnostics.activity = Some(format!(
                    "Waiting for {}",
                    string_field(data, "agentDisplayName").unwrap_or_else(|| "subagent".to_owned())
                ));
            }
            "subagent.completed" | "subagent.failed" => {
                diagnostics.activity = Some("Processing subagent result".to_owned());
            }
            "session.compaction_start" => {
                diagnostics.activity = Some("Compacting conversation context".to_owned());
                diagnostics.compaction = Some(CompactionDiagnostics {
                    started_at: event.timestamp.clone(),
                    trigger: string_field(data, "trigger"),
                    current_tokens: data.get("currentTokens").and_then(Value::as_u64),
                    token_limit: data.get("tokenLimit").and_then(Value::as_u64),
                });
            }
            "session.compaction_complete" => {
                diagnostics.activity = Some("Context compaction complete".to_owned());
                diagnostics.compaction = None;
            }
            "assistant.usage" => {
                diagnostics.last_usage = Some(UsageDiagnostics {
                    model: string_field(data, "model"),
                    duration_ms: data.get("duration").and_then(Value::as_u64),
                    input_tokens: data.get("inputTokens").and_then(Value::as_u64),
                    output_tokens: data.get("outputTokens").and_then(Value::as_u64),
                    cache_read_tokens: data.get("cacheReadTokens").and_then(Value::as_u64),
                });
            }
            "assistant.idle" => {
                diagnostics.activity = Some("Waiting for background work".to_owned());
            }
            "session.idle" => {
                diagnostics.activity = None;
                diagnostics.compaction = None;
            }
            "session.error" => diagnostics.activity = Some("Agent encountered an error".to_owned()),
            _ => {}
        }

        if let Some(summary) = Self::diagnostic_signal_summary(event) {
            let signal = DiagnosticSignal {
                sequence: event.sequence,
                timestamp: event.timestamp.clone(),
                event_type: event.source_type.clone(),
                summary,
            };
            if diagnostics
                .recent_events
                .last()
                .is_some_and(|previous| previous.event_type == signal.event_type)
            {
                diagnostics.recent_events.pop();
            }
            diagnostics.recent_events.push(signal);
            let overflow = diagnostics
                .recent_events
                .len()
                .saturating_sub(DIAGNOSTIC_EVENT_LIMIT);
            if overflow > 0 {
                diagnostics.recent_events.drain(..overflow);
            }
        }
    }

    fn diagnostic_signal_summary(event: &DomainEvent) -> Option<String> {
        let data = &event.details;
        let summary = match event.source_type.as_str() {
            "assistant.turn_start" => string_field(data, "model").map_or_else(
                || "Assistant turn started".to_owned(),
                |model| format!("Using {model}"),
            ),
            "assistant.turn_retry" => string_field(data, "reason").map_or_else(
                || "Model call retry".to_owned(),
                |reason| format!("Retry: {reason}"),
            ),
            "model.call_start" => "Model request dispatched".to_owned(),
            "assistant.intent" => string_field(data, "intent")?,
            "assistant.reasoning" | "assistant.reasoning_delta" => "Reasoning activity".to_owned(),
            "assistant.server_tool_progress" => format!(
                "{}: {}",
                string_field(data, "kind").unwrap_or_else(|| "Server tool".to_owned()),
                string_field(data, "status").unwrap_or_else(|| "running".to_owned())
            ),
            "assistant.streaming_delta" => data
                .get("totalResponseSizeBytes")
                .and_then(Value::as_u64)
                .map_or_else(
                    || "Response stream active".to_owned(),
                    |bytes| format!("{bytes} bytes received"),
                ),
            "tool.execution_start" => format!(
                "Started {}",
                string_field(data, "toolName").unwrap_or_else(|| "tool".to_owned())
            ),
            "tool.execution_progress" => string_field(data, "progressMessage")?,
            "tool.execution_complete" => "Tool completed".to_owned(),
            "hook.start" => format!(
                "Started {} hook",
                string_field(data, "hookType").unwrap_or_else(|| "session".to_owned())
            ),
            "hook.progress" => string_field(data, "message")?,
            "hook.end" => "Hook completed".to_owned(),
            "subagent.started" => format!(
                "Started {}",
                string_field(data, "agentDisplayName").unwrap_or_else(|| "subagent".to_owned())
            ),
            "subagent.completed" => "Subagent completed".to_owned(),
            "subagent.failed" => string_field(data, "error").map_or_else(
                || "Subagent failed".to_owned(),
                |error| format!("Failed: {error}"),
            ),
            "session.compaction_start" => "Context compaction started".to_owned(),
            "session.compaction_complete" => string_field(data, "error").map_or_else(
                || "Context compaction completed".to_owned(),
                |error| format!("Compaction failed: {error}"),
            ),
            "assistant.usage" => "Model usage reported".to_owned(),
            "assistant.idle" => "Assistant loop idle; background work may remain".to_owned(),
            "session.idle" => "Session idle".to_owned(),
            "session.error" => string_field(data, "message")
                .or_else(|| string_field(data, "error"))
                .unwrap_or_else(|| "Session error".to_owned()),
            _ => return None,
        };
        Some(truncate(&summary, 200))
    }

    pub fn remove_interaction(&mut self, interaction_id: &str) -> bool {
        let original_len = self.pending_interactions.len();
        self.pending_interactions
            .retain(|request| request.id != interaction_id);
        if self.pending_interactions.is_empty() && self.status == SessionStatus::Waiting {
            self.status = SessionStatus::Running;
        }
        self.pending_interactions.len() != original_len
    }

    pub fn record_interaction_response(
        &mut self,
        interaction_id: &str,
        response: InteractionResponse,
    ) {
        if let Some(record) = self
            .interaction_history
            .iter_mut()
            .find(|record| record.request.id == interaction_id && record.response.is_none())
        {
            record.response = Some(response);
        }
    }

    pub fn cancel_pending_interactions(&mut self) {
        for record in &mut self.interaction_history {
            if record.response.is_none()
                && self
                    .pending_interactions
                    .iter()
                    .any(|request| request.id == record.request.id)
            {
                record.response = Some(InteractionResponse::Cancel);
            }
        }
        self.pending_interactions.clear();
    }
}

/// One item in the session timeline.
///
/// The transcript alone shows only what the agent *said*; the timeline
/// interleaves what it *did* and asked permission to do.
#[derive(Clone, Copy, Debug)]
pub enum TimelineEntry<'a> {
    Message(&'a TranscriptMessage),
    Tool(&'a ToolInvocation),
    Interaction(&'a InteractionRecord),
}

impl TimelineEntry<'_> {
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        match self {
            Self::Message(message) => message.sequence,
            Self::Tool(invocation) => invocation.sequence,
            Self::Interaction(record) => record.sequence,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
    OutOfOrder,
    WrongSession,
}

#[must_use]
pub fn rebuild(
    mut snapshot: SessionSnapshot,
    events: impl IntoIterator<Item = DomainEvent>,
) -> SessionSnapshot {
    snapshot.restore_indexes();
    for event in events {
        let _ = snapshot.apply(event);
    }
    snapshot
}

fn status_for_event(event: &DomainEvent) -> Option<SessionStatus> {
    if event.source_type == "session.idle" {
        return Some(if event.state == ActivityState::Cancelled {
            SessionStatus::Cancelled
        } else {
            SessionStatus::Idle
        });
    }

    if event.source_type.ends_with(".requested") {
        return Some(SessionStatus::Waiting);
    }
    if matches!(
        event.source_type.as_str(),
        "user.message" | "assistant.turn_start"
    ) {
        return Some(SessionStatus::Running);
    }
    match event.state {
        ActivityState::Waiting => Some(SessionStatus::Waiting),
        ActivityState::Failed => Some(SessionStatus::Failed),
        ActivityState::Cancelled => Some(SessionStatus::Cancelled),
        ActivityState::Running | ActivityState::Queued | ActivityState::Completed => None,
    }
}

fn project_transcript(transcript: &mut Vec<TranscriptMessage>, event: &DomainEvent) {
    if event.agent_id.is_some()
        || event
            .details
            .get("parentToolCallId")
            .is_some_and(|value| !value.is_null())
    {
        return;
    }
    match event.source_type.as_str() {
        "assistant.turn_start" => {
            for message in transcript.iter_mut().filter(|message| {
                message.role == TranscriptRole::User && message.state == TranscriptState::Pending
            }) {
                message.state = TranscriptState::Complete;
            }
        }
        "user.message" => {
            let content = event
                .details
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let attachments = message_attachments(event);
            // A screenshot on its own is a complete message, so an empty body
            // is only uninteresting when nothing came with it.
            if !content.is_empty() || !attachments.is_empty() {
                transcript.push(TranscriptMessage {
                    id: event.id.clone(),
                    role: TranscriptRole::User,
                    content: content.to_owned(),
                    state: if event.details.get("delivery").and_then(Value::as_str)
                        == Some("steering")
                    {
                        TranscriptState::Pending
                    } else {
                        TranscriptState::Complete
                    },
                    timestamp: event.timestamp.clone(),
                    sequence: event.sequence,
                    attachments,
                });
            }
        }
        "assistant.message_start" => {
            let id = message_id(event);
            if !transcript.iter().any(|message| message.id == id) {
                transcript.push(TranscriptMessage {
                    id,
                    role: TranscriptRole::Assistant,
                    content: String::new(),
                    state: TranscriptState::Streaming,
                    timestamp: event.timestamp.clone(),
                    sequence: event.sequence,
                    attachments: Vec::new(),
                });
            }
        }
        "assistant.message_delta" => {
            let id = message_id(event);
            let delta = event
                .details
                .get("deltaContent")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(message) = transcript.iter_mut().find(|message| message.id == id) {
                message.content.push_str(delta);
            } else {
                transcript.push(TranscriptMessage {
                    id,
                    role: TranscriptRole::Assistant,
                    content: delta.to_owned(),
                    state: TranscriptState::Streaming,
                    timestamp: event.timestamp.clone(),
                    sequence: event.sequence,
                    attachments: Vec::new(),
                });
            }
        }
        "assistant.message" => {
            let id = message_id(event);
            let content = event
                .details
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(message) = transcript.iter_mut().find(|message| message.id == id) {
                content.clone_into(&mut message.content);
                message.state = TranscriptState::Complete;
            } else if !content.is_empty() {
                transcript.push(TranscriptMessage {
                    id,
                    role: TranscriptRole::Assistant,
                    content: content.to_owned(),
                    state: TranscriptState::Complete,
                    timestamp: event.timestamp.clone(),
                    sequence: event.sequence,
                    attachments: Vec::new(),
                });
            }
        }
        _ => {}
    }
}

/// Whether an event ends the current turn without completing it.
fn is_abort(event: &DomainEvent) -> bool {
    event.source_type == "abort"
        || (event.source_type == "session.idle" && event.state == ActivityState::Cancelled)
}

/// Mark any still-streaming assistant message as interrupted.
fn mark_streaming_interrupted(transcript: &mut [TranscriptMessage]) {
    for message in transcript
        .iter_mut()
        .filter(|message| message.state == TranscriptState::Streaming)
    {
        message.state = TranscriptState::Interrupted;
    }
}

fn message_id(event: &DomainEvent) -> String {
    event
        .details
        .get("messageId")
        .and_then(Value::as_str)
        .unwrap_or(&event.id)
        .to_owned()
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn classify_kind(event_type: &str) -> ActivityKind {
    if event_type.starts_with("model.") || event_type.starts_with("assistant.") {
        ActivityKind::Model
    } else if event_type.starts_with("tool.") || event_type.starts_with("external_tool.") {
        ActivityKind::Tool
    } else if event_type.starts_with("subagent.") {
        ActivityKind::Subagent
    } else if event_type.starts_with("permission.")
        || event_type.starts_with("user_input.")
        || event_type.starts_with("elicitation.")
    {
        ActivityKind::Permission
    } else if event_type.contains("workspace_file") {
        ActivityKind::File
    } else if event_type.starts_with("session.") {
        ActivityKind::Session
    } else {
        ActivityKind::System
    }
}

fn classify_state(event_type: &str, data: &Value) -> ActivityState {
    if event_type == "abort"
        || event_type.ends_with(".cancelled")
        || data.get("aborted").and_then(Value::as_bool) == Some(true)
    {
        ActivityState::Cancelled
    } else if data.get("success").and_then(Value::as_bool) == Some(false)
        || data.get("error").is_some_and(|error| !error.is_null())
        || event_type.ends_with(".failure")
        || event_type.ends_with(".failed")
        || event_type == "session.error"
    {
        ActivityState::Failed
    } else if event_type.ends_with(".requested") {
        ActivityState::Waiting
    } else if event_type.strip_suffix(".start").is_some()
        || event_type.strip_suffix(".started").is_some()
        || event_type.strip_suffix("_start").is_some()
    {
        ActivityState::Running
    } else if event_type.ends_with(".complete")
        || event_type.ends_with(".completed")
        || event_type.ends_with("_complete")
        || event_type.strip_suffix(".idle").is_some()
    {
        ActivityState::Completed
    } else {
        ActivityState::Queued
    }
}

fn summarize(event_type: &str, data: &Value) -> String {
    for field in ["message", "title", "toolName", "name", "summary", "error"] {
        if let Some(value) = data.get(field).and_then(Value::as_str) {
            return format!("{event_type}: {}", truncate(value, 160));
        }
    }
    event_type.to_owned()
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn metadata() -> SessionMetadata {
        SessionMetadata {
            id: "app-session".to_owned(),
            sdk_session_id: "sdk-session".to_owned(),
            project_path: "/tmp/project".to_owned(),
            repository_root: None,
            title: "Test".to_owned(),
            title_source: TitleSource::Manual,
            kind: SessionKind::Project,
            model: None,
            mode: None,
            base_ref: None,
            created_at: "2026-08-06T12:00:00Z".to_owned(),
            updated_at: "2026-08-06T12:00:00Z".to_owned(),
        }
    }

    fn event(sequence: u64, id: &str, event_type: &str) -> DomainEvent {
        DomainEvent::from_sdk_event_for(
            "app-session",
            sequence,
            &json!({
                "id": id,
                "timestamp": "2026-08-06T12:00:00Z",
                "type": event_type,
                "data": {}
            }),
        )
    }

    #[test]
    fn reducer_is_deterministic_and_idempotent() {
        let events = vec![
            event(1, "one", "assistant.turn_start"),
            event(2, "two", "session.idle"),
        ];
        let first = rebuild(SessionSnapshot::new(metadata()), events.clone());
        let second = rebuild(SessionSnapshot::new(metadata()), events);

        assert_eq!(first, second);
        assert_eq!(first.status, SessionStatus::Idle);

        let mut replayed = first.clone();
        assert_eq!(
            replayed.apply(event(2, "two", "session.idle")),
            ApplyOutcome::Duplicate
        );
        assert_eq!(replayed, first);
    }

    #[test]
    fn diagnostics_project_progress_without_retaining_reasoning_content() {
        let mut state = SessionSnapshot::new(metadata());
        let events = [
            json!({
                "id": "turn",
                "timestamp": "1",
                "type": "assistant.turn_start",
                "data": {"turnId": "turn-7", "model": "gpt-test"}
            }),
            json!({
                "id": "intent",
                "timestamp": "2",
                "type": "assistant.intent",
                "data": {"intent": "Inspecting the session state"}
            }),
            json!({
                "id": "child-turn",
                "agentId": "agent-1",
                "timestamp": "2",
                "type": "assistant.turn_start",
                "data": {"turnId": "child-turn", "model": "child-model"}
            }),
            json!({
                "id": "reasoning",
                "timestamp": "3",
                "type": "assistant.reasoning",
                "data": {"reasoningId": "r", "content": "private chain of thought"}
            }),
            json!({
                "id": "compact",
                "timestamp": "4",
                "type": "session.compaction_start",
                "data": {"trigger": "threshold", "currentTokens": 900, "tokenLimit": 1000}
            }),
        ];
        for (index, raw) in events.iter().enumerate() {
            state.apply(DomainEvent::from_sdk_event_for(
                "app-session",
                index as u64 + 1,
                raw,
            ));
        }

        assert_eq!(
            state.diagnostics.latest_intent.as_deref(),
            Some("Inspecting the session state")
        );
        assert_eq!(state.diagnostics.model.as_deref(), Some("gpt-test"));
        assert_eq!(state.diagnostics.turn_id.as_deref(), Some("turn-7"));
        assert_eq!(
            state.diagnostics.activity.as_deref(),
            Some("Compacting conversation context")
        );
        assert_eq!(
            state
                .diagnostics
                .compaction
                .as_ref()
                .and_then(|compaction| compaction.current_tokens),
            Some(900)
        );
        assert_eq!(
            state.diagnostics.event_counts.get("assistant.reasoning"),
            Some(&1)
        );
        assert!(
            state
                .diagnostics
                .recent_events
                .iter()
                .all(|event| !event.summary.contains("private chain of thought"))
        );
    }

    #[test]
    fn idle_survives_trailing_resume_metadata() {
        let mut state = SessionSnapshot::new(metadata());
        for event in [
            event(1, "turn", "assistant.turn_start"),
            event(2, "idle", "session.idle"),
            event(3, "resume", "session.resume"),
            event(4, "agents", "session.custom_agents_updated"),
        ] {
            assert_eq!(state.apply(event), ApplyOutcome::Applied);
        }

        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(
            state.apply(event(5, "user", "user.message")),
            ApplyOutcome::Applied
        );
        assert_eq!(state.status, SessionStatus::Running);
    }

    #[test]
    fn terminal_session_error_survives_idle_until_the_next_turn() {
        let mut state = SessionSnapshot::new(metadata());
        let error = DomainEvent::from_sdk_event_for(
            "app-session",
            1,
            &json!({
                "id": "error",
                "type": "session.error",
                "data": {"message": "The model could not process this image."}
            }),
        );

        assert_eq!(state.apply(error), ApplyOutcome::Applied);
        assert_eq!(
            state.last_error.as_deref(),
            Some("The model could not process this image.")
        );
        assert_eq!(
            state.apply(event(2, "assistant-idle", "assistant.idle")),
            ApplyOutcome::Applied
        );
        assert_eq!(
            state.apply(event(3, "idle", "session.idle")),
            ApplyOutcome::Applied
        );
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(
            state.last_error.as_deref(),
            Some("The model could not process this image.")
        );

        assert_eq!(
            state.apply(event(4, "next-turn", "user.message")),
            ApplyOutcome::Applied
        );
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn restart_cancels_activity_left_running_by_the_previous_runtime() {
        let mut state = SessionSnapshot::new(metadata());
        let events = [
            json!({"id":"user","type":"user.message","data":{"content":"run tests"}}),
            json!({"id":"message","type":"assistant.message_start","data":{"messageId":"m"}}),
            json!({
                "id": "tool",
                "type": "tool.execution_start",
                "timestamp": "1",
                "data": {
                    "toolCallId": "call",
                    "toolName": "bash",
                    "arguments": {"command": "cargo test", "shellId": "shell"},
                    "shellToolInfo": {"displayCommand": "cargo test"}
                }
            }),
        ];
        for (index, raw) in events.iter().enumerate() {
            let event = DomainEvent::from_sdk_event_for("app-session", index as u64 + 1, raw);
            assert_eq!(state.apply(event), ApplyOutcome::Applied);
        }

        state.reconcile_after_restart("2");

        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.transcript[1].state, TranscriptState::Interrupted);
        assert_eq!(
            state.tool_activity.invocations[0].state,
            InvocationState::Cancelled
        );
        assert_eq!(
            state.tool_activity.terminals[0].state,
            tools::TerminalState::Cancelled
        );
    }

    #[test]
    fn reducer_rejects_out_of_order_and_wrong_session_events() {
        let mut state = SessionSnapshot::new(metadata());
        assert_eq!(
            state.apply(event(2, "two", "assistant.turn_start")),
            ApplyOutcome::Applied
        );
        assert_eq!(
            state.apply(event(1, "one", "assistant.turn_start")),
            ApplyOutcome::OutOfOrder
        );

        let mut wrong = event(3, "three", "session.idle");
        wrong.session_id = "different".to_owned();
        assert_eq!(state.apply(wrong), ApplyOutcome::WrongSession);
    }

    #[test]
    fn raw_payload_and_subagent_correlation_are_preserved() {
        let raw = json!({
            "id": "event-1",
            "timestamp": "2026-08-06T12:00:00Z",
            "parentId": "turn-1",
            "agentId": "agent-2",
            "type": "tool.execution_start",
            "data": {"toolName": "shell", "arguments": {"command": "cargo test"}}
        });

        let event = DomainEvent::from_sdk_event_for("app-session", 1, &raw);

        assert_eq!(event.kind, ActivityKind::Tool);
        assert_eq!(event.state, ActivityState::Running);
        assert_eq!(event.parent_id.as_deref(), Some("turn-1"));
        assert_eq!(event.agent_id.as_deref(), Some("agent-2"));
        assert_eq!(event.raw, raw);
    }

    #[test]
    fn payload_outcome_overrides_completion_event_name() {
        let failed = DomainEvent::from_sdk_event_for(
            "app-session",
            1,
            &json!({
                "type": "tool.execution_complete",
                "data": {"success": false, "error": {"message": "boom"}}
            }),
        );
        let aborted = DomainEvent::from_sdk_event_for(
            "app-session",
            2,
            &json!({"type": "session.idle", "data": {"aborted": true}}),
        );

        assert_eq!(failed.state, ActivityState::Failed);
        assert_eq!(aborted.state, ActivityState::Cancelled);
    }

    #[test]
    fn transcript_coalesces_streaming_deltas_and_ignores_subagents() {
        let mut state = SessionSnapshot::new(metadata());
        let events = [
            json!({"id":"u","type":"user.message","data":{"content":"hello"}}),
            json!({"id":"s","type":"assistant.message_start","data":{"messageId":"m"}}),
            json!({"id":"d1","type":"assistant.message_delta","data":{"messageId":"m","deltaContent":"hi "}}),
            json!({"id":"d2","type":"assistant.message_delta","data":{"messageId":"m","deltaContent":"there"}}),
            json!({"id":"f","type":"assistant.message","data":{"messageId":"m","content":"hi there"}}),
            json!({"id":"nested","agentId":"agent-1","type":"assistant.message","data":{"messageId":"n","content":"hidden"}}),
        ];
        for (index, raw) in events.iter().enumerate() {
            let event = DomainEvent::from_sdk_event_for("app-session", index as u64 + 1, raw);
            assert_eq!(state.apply(event), ApplyOutcome::Applied);
        }

        assert_eq!(state.transcript.len(), 2);
        assert_eq!(state.transcript[0].role, TranscriptRole::User);
        assert_eq!(state.transcript[1].content, "hi there");
        assert_eq!(state.transcript[1].state, TranscriptState::Complete);
    }

    #[test]
    fn steering_message_stays_pending_until_the_next_root_turn_starts() {
        let mut state = SessionSnapshot::new(metadata());
        let events = [
            json!({"id":"u1","type":"user.message","data":{
                "content":"start the work",
                "delivery":"idle"
            }}),
            json!({"id":"turn-1","type":"assistant.turn_start","data":{"turnId":"1"}}),
            json!({"id":"u2","type":"user.message","data":{
                "content":"change direction",
                "delivery":"steering"
            }}),
            json!({"id":"nested","agentId":"agent-1","type":"assistant.turn_start",
                "data":{"turnId":"nested"}}),
            json!({"id":"tool","type":"tool.execution_complete","data":{
                "toolCallId":"call-1",
                "success":true
            }}),
        ];
        for (index, raw) in events.iter().enumerate() {
            let event = DomainEvent::from_sdk_event_for("app-session", index as u64 + 1, raw);
            assert_eq!(state.apply(event), ApplyOutcome::Applied);
        }

        assert_eq!(state.transcript[0].state, TranscriptState::Complete);
        assert_eq!(state.transcript[1].state, TranscriptState::Pending);

        let acknowledged = DomainEvent::from_sdk_event_for(
            "app-session",
            6,
            &json!({"id":"turn-2","type":"assistant.turn_start","data":{"turnId":"2"}}),
        );
        assert_eq!(state.apply(acknowledged), ApplyOutcome::Applied);
        assert_eq!(state.transcript[1].state, TranscriptState::Complete);
    }

    #[test]
    fn pending_interactions_are_idempotent_and_removable() {
        let mut state = SessionSnapshot::new(metadata());
        let request = InteractionRequest {
            id: "permission-1".to_owned(),
            session_id: "app-session".to_owned(),
            kind: InteractionKind::Permission,
            title: "Permission required".to_owned(),
            message: "Run command?".to_owned(),
            choices: Vec::new(),
            allow_freeform: false,
            details: Value::Null,
        };

        state.add_interaction(request.clone());
        state.add_interaction(request);
        assert_eq!(state.pending_interactions.len(), 1);
        assert_eq!(state.interaction_history.len(), 1);
        assert_eq!(state.status, SessionStatus::Waiting);
        state.record_interaction_response("permission-1", InteractionResponse::ApproveForSession);
        assert!(state.remove_interaction("permission-1"));
        assert!(state.pending_interactions.is_empty());
        assert_eq!(
            state.interaction_history[0].response,
            Some(InteractionResponse::ApproveForSession)
        );

        let repeated = state.interaction_history[0].request.clone();
        state.add_interaction(repeated);
        state.record_interaction_response(
            "permission-1",
            InteractionResponse::Reject { feedback: None },
        );
        assert_eq!(
            state.interaction_history[0].response,
            Some(InteractionResponse::ApproveForSession)
        );
        assert_eq!(
            state.interaction_history[1].response,
            Some(InteractionResponse::Reject { feedback: None })
        );
    }

    #[test]
    fn phase_one_snapshots_gain_phase_two_defaults() {
        let snapshot: SessionSnapshot = serde_json::from_value(json!({
            "version": 1,
            "metadata": metadata(),
            "status": "idle",
            "last_sequence": 0,
            "activities": [],
            "last_error": null
        }))
        .unwrap();

        assert!(snapshot.transcript.is_empty());
        assert!(snapshot.pending_interactions.is_empty());
        assert_eq!(snapshot.controls, SessionControls::default());
    }

    #[test]
    fn phase_two_snapshots_gain_phase_three_defaults() {
        let snapshot: SessionSnapshot = serde_json::from_value(json!({
            "version": 2,
            "metadata": metadata(),
            "status": "idle",
            "last_sequence": 0,
            "activities": [],
            "transcript": [],
            "pending_interactions": [],
            "controls": SessionControls::default(),
            "last_error": null
        }))
        .unwrap();

        assert_eq!(snapshot.tool_catalog, ToolCatalog::default());
        assert!(snapshot.tool_activity.invocations.is_empty());
        assert!(snapshot.capabilities.capabilities.is_empty());
        assert!(snapshot.changes.is_empty());
        assert!(snapshot.metadata.base_ref.is_none());
    }

    /// Build the event sequence a `bash` call produces.
    fn shell_events(call_id: &str, shell_id: &str, exit_code: i64) -> Vec<Value> {
        vec![
            json!({
                "id": format!("{call_id}-start"),
                "type": "tool.execution_start",
                "timestamp": "1",
                "data": {
                    "toolCallId": call_id,
                    "toolName": "bash",
                    "arguments": {"command": "cargo test", "shellId": shell_id},
                    "shellToolInfo": {
                        "displayCommand": "cargo test",
                        "hasWriteFileRedirection": false,
                        "possiblePaths": ["src/lib.rs"]
                    }
                }
            }),
            json!({
                "id": format!("{call_id}-partial"),
                "type": "tool.execution_partial_result",
                "timestamp": "2",
                "data": {"toolCallId": call_id, "partialOutput": "running tests\n"}
            }),
            json!({
                "id": format!("{call_id}-complete"),
                "type": "tool.execution_complete",
                "timestamp": "3",
                "data": {
                    "toolCallId": call_id,
                    "success": exit_code == 0,
                    "result": {
                        "content": "done",
                        "contents": [{
                            "type": "shell_exit",
                            "shellId": shell_id,
                            "exitCode": exit_code,
                            "cwd": "/repo",
                            "outputPreview": "running tests\n"
                        }]
                    }
                }
            }),
        ]
    }

    fn apply_all(state: &mut SessionSnapshot, events: Vec<Value>) {
        for (index, raw) in events.into_iter().enumerate() {
            let sequence = state.last_sequence + 1 + index as u64;
            let event = DomainEvent::from_sdk_event_for("app-session", sequence, &raw);
            assert_eq!(state.apply(event), ApplyOutcome::Applied);
        }
    }

    #[test]
    fn shell_tools_project_into_a_terminal_keyed_by_shell_id() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(&mut state, shell_events("call-1", "shell-a", 0));

        let terminal = state
            .tool_activity
            .terminal("shell-a")
            .expect("terminal exists");
        assert_eq!(terminal.command.as_deref(), Some("cargo test"));
        assert_eq!(terminal.exit_code, Some(0));
        assert_eq!(terminal.state, TerminalState::Exited);
        assert_eq!(terminal.output, "running tests\n");
        assert_eq!(terminal.tool_call_ids, vec!["call-1".to_owned()]);

        let invocation = state
            .tool_activity
            .invocation("call-1")
            .expect("invocation exists");
        assert_eq!(invocation.class, ToolClass::Shell);
        assert_eq!(invocation.state, InvocationState::Succeeded);
        assert_eq!(invocation.shell_id.as_deref(), Some("shell-a"));
    }

    /// The core Phase 3 shell contract: a terminal outlives the tool call that
    /// created it, so `read_bash` against the same shell appends to the
    /// terminal the UI is already showing rather than creating a new one.
    #[test]
    fn read_bash_appends_to_the_existing_terminal_rather_than_creating_one() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(&mut state, shell_events("call-1", "shell-a", 0));

        apply_all(
            &mut state,
            vec![
                json!({
                    "id": "call-2-start",
                    "type": "tool.execution_start",
                    "timestamp": "4",
                    "data": {
                        "toolCallId": "call-2",
                        "toolName": "read_bash",
                        "arguments": {"shellId": "shell-a"}
                    }
                }),
                json!({
                    "id": "call-2-partial",
                    "type": "tool.execution_partial_result",
                    "timestamp": "5",
                    "data": {
                        "toolCallId": "call-2",
                        "partialOutput":
                            "more output\n<output too long - dropped 1 line from the end>\n"
                    }
                }),
                json!({
                    "id": "call-2-complete",
                    "type": "tool.execution_complete",
                    "timestamp": "6",
                    "data": {
                        "toolCallId": "call-2",
                        "success": true,
                        "result": {
                            "content": "more output\nrecovered output\n",
                            "detailedContent": "more output\nrecovered output\n"
                        }
                    }
                }),
            ],
        );

        assert_eq!(
            state.tool_activity.terminals.len(),
            1,
            "read_bash must not create a second terminal for the same shell"
        );
        let terminal = state.tool_activity.terminal("shell-a").expect("terminal");
        assert_eq!(
            terminal.output,
            "running tests\nmore output\nrecovered output\n"
        );
        assert!(!terminal.output.contains("output too long"));
        assert_eq!(
            terminal.tool_call_ids,
            vec!["call-1".to_owned(), "call-2".to_owned()]
        );
        // The shell already reported exit; a later read must not resurrect it.
        assert_eq!(terminal.state, TerminalState::Exited);
    }

    /// The SDK has been observed to redeliver a `tool.execution_partial_result`
    /// event with a distinct event id but the exact same `partialOutput`
    /// chunk. Without guarding against this, the chunk would be appended
    /// twice and the same line of output would render twice in a row.
    #[test]
    fn a_redelivered_partial_result_is_not_applied_twice() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({
                    "id": "call-1-start",
                    "type": "tool.execution_start",
                    "timestamp": "1",
                    "data": {
                        "toolCallId": "call-1",
                        "toolName": "bash",
                        "arguments": {"command": "echo hi", "shellId": "shell-a"}
                    }
                }),
                json!({
                    "id": "call-1-partial-1",
                    "type": "tool.execution_partial_result",
                    "timestamp": "2",
                    "data": {"toolCallId": "call-1", "partialOutput": "hi\n"}
                }),
                // Same delivery timestamp and chunk, but a different event id:
                // a runtime redelivery rather than an event-log replay.
                json!({
                    "id": "call-1-partial-2",
                    "type": "tool.execution_partial_result",
                    "timestamp": "2",
                    "data": {"toolCallId": "call-1", "partialOutput": "hi\n"}
                }),
            ],
        );

        let invocation = state
            .tool_activity
            .invocation("call-1")
            .expect("invocation exists");
        assert_eq!(invocation.output, "hi\n");

        let terminal = state.tool_activity.terminal("shell-a").expect("terminal");
        assert_eq!(terminal.output, "hi\n");
    }

    #[test]
    fn completion_replaces_a_truncated_partial_with_the_full_result() {
        let mut state = SessionSnapshot::new(metadata());
        let events = [
            json!({
                "id": "start",
                "type": "tool.execution_start",
                "timestamp": "1",
                "data": {
                    "toolCallId": "call-1",
                    "toolName": "bash",
                    "arguments": {"command": "print-many-lines"}
                }
            }),
            json!({
                "id": "partial",
                "type": "tool.execution_partial_result",
                "timestamp": "2",
                "data": {
                    "toolCallId": "call-1",
                    "partialOutput": "line 1\n<output too long - dropped 207 lines from the end>\n"
                }
            }),
            json!({
                "id": "complete",
                "type": "tool.execution_complete",
                "timestamp": "3",
                "data": {
                    "toolCallId": "call-1",
                    "success": true,
                    "result": {
                        "content": "line 1\nline 2\nline 3\n",
                        "detailedContent": "line 1\nline 2\nline 3\n"
                    }
                }
            }),
        ];

        for raw in events {
            let event =
                DomainEvent::from_sdk_event_for("app-session", state.last_sequence + 1, &raw);
            let updates = tools::output_updates(&state.tool_activity, &event);
            if event.source_type == "tool.execution_complete" {
                assert_eq!(updates.len(), 1);
                assert!(updates[0].replace);
            }
            let event_type = event.source_type.clone();
            assert_eq!(state.apply(event), ApplyOutcome::Applied);
            if event_type == "tool.execution_partial_result" {
                let invocation = state.tool_activity.invocation("call-1").unwrap();
                assert_eq!(invocation.output, "line 1\n");
                assert!(!invocation.output.contains("output too long"));
                assert_eq!(updates[0].chunk.as_deref(), Some("line 1\n"));
            }
        }

        let invocation = state.tool_activity.invocation("call-1").unwrap();
        assert_eq!(invocation.output, "line 1\nline 2\nline 3\n");
        assert!(!invocation.output.contains("output too long"));
        assert_eq!(invocation.output_metadata.chunk_count, 1);
        assert_eq!(invocation.output_metadata.byte_count, 21);
        assert!(invocation.output_metadata.complete);
    }

    /// A later delivery with identical content is legitimate output. Content
    /// equality alone must not collapse append-only chunks.
    #[test]
    fn identical_chunks_from_distinct_deliveries_are_preserved() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({
                    "id": "call-1-start",
                    "type": "tool.execution_start",
                    "timestamp": "1",
                    "data": {
                        "toolCallId": "call-1",
                        "toolName": "bash",
                        "arguments": {"command": "echo hi", "shellId": "shell-a"}
                    }
                }),
                json!({
                    "id": "call-1-partial-1",
                    "type": "tool.execution_partial_result",
                    "timestamp": "2",
                    "data": {"toolCallId": "call-1", "partialOutput": "hi\n"}
                }),
                json!({
                    "id": "call-1-partial-2",
                    "type": "tool.execution_partial_result",
                    "timestamp": "3",
                    "data": {"toolCallId": "call-1", "partialOutput": "hi\n"}
                }),
                json!({
                    "id": "call-1-partial-3",
                    "type": "tool.execution_partial_result",
                    "timestamp": "4",
                    "data": {"toolCallId": "call-1", "partialOutput": "hi\n"}
                }),
            ],
        );

        let invocation = state
            .tool_activity
            .invocation("call-1")
            .expect("invocation exists");
        assert_eq!(invocation.output, "hi\nhi\nhi\n");
        assert_eq!(invocation.output_metadata.chunk_count, 3);
    }

    #[test]
    fn partial_delivery_fingerprint_survives_snapshot_restore() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({
                    "id": "call-1-start",
                    "type": "tool.execution_start",
                    "timestamp": "1",
                    "data": {
                        "toolCallId": "call-1",
                        "toolName": "bash",
                        "arguments": {"command": "echo hi", "shellId": "shell-a"}
                    }
                }),
                json!({
                    "id": "call-1-partial-1",
                    "type": "tool.execution_partial_result",
                    "timestamp": "2",
                    "data": {"toolCallId": "call-1", "partialOutput": "hi\n"}
                }),
            ],
        );
        let encoded = serde_json::to_value(&state).unwrap();
        let mut restored: SessionSnapshot = serde_json::from_value(encoded).unwrap();
        restored.restore_indexes();
        let redelivery = DomainEvent::from_sdk_event_for(
            "app-session",
            restored.last_sequence + 1,
            &json!({
                "id": "call-1-partial-2",
                "type": "tool.execution_partial_result",
                "timestamp": "2",
                "data": {"toolCallId": "call-1", "partialOutput": "hi\n"}
            }),
        );

        assert!(tools::output_updates(&restored.tool_activity, &redelivery).is_empty());
        assert_eq!(restored.apply(redelivery), ApplyOutcome::Applied);
        let invocation = restored
            .tool_activity
            .invocation("call-1")
            .expect("invocation exists");
        assert!(invocation.output.is_empty());
        assert_eq!(invocation.output_metadata.chunk_count, 1);
    }

    #[test]
    fn failed_tools_are_surfaced_with_structured_errors() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({
                    "id": "edit-start",
                    "type": "tool.execution_start",
                    "timestamp": "1",
                    "data": {
                        "toolCallId": "edit-1",
                        "toolName": "edit",
                        "arguments": {"path": "/repo/src/lib.rs"}
                    }
                }),
                json!({
                    "id": "edit-complete",
                    "type": "tool.execution_complete",
                    "timestamp": "2",
                    "data": {
                        "toolCallId": "edit-1",
                        "success": false,
                        "error": {"code": "ENOENT", "message": "file not found"}
                    }
                }),
            ],
        );

        let failures = state.tool_activity.failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].error_code.as_deref(), Some("ENOENT"));
        assert_eq!(failures[0].error_message.as_deref(), Some("file not found"));
        assert_eq!(failures[0].class, ToolClass::FileWrite);
    }

    #[test]
    fn edit_diffs_are_retained_for_the_changes_view() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({
                    "id": "edit-start",
                    "type": "tool.execution_start",
                    "timestamp": "1",
                    "data": {
                        "toolCallId": "edit-1",
                        "toolName": "edit",
                        "arguments": {"path": "src/lib.rs"}
                    }
                }),
                json!({
                    "id": "edit-complete",
                    "type": "tool.execution_complete",
                    "timestamp": "2",
                    "data": {
                        "toolCallId": "edit-1",
                        "success": true,
                        "result": {
                            "content": "edited",
                            "detailedContent": "@@ -1 +1 @@\n-old\n+new\n"
                        }
                    }
                }),
            ],
        );

        let invocation = state
            .tool_activity
            .invocation("edit-1")
            .expect("invocation");
        assert!(invocation.diff().is_some_and(|diff| diff.contains("+new")));
        assert_eq!(invocation.file_path(), Some("src/lib.rs"));
        assert_eq!(
            state.tool_activity.mutated_paths(),
            vec!["src/lib.rs".to_owned()]
        );
    }

    #[test]
    fn tool_activity_survives_snapshot_round_trip() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(&mut state, shell_events("call-1", "shell-a", 0));

        let encoded = serde_json::to_value(&state).unwrap();
        let mut restored: SessionSnapshot = serde_json::from_value(encoded).unwrap();
        restored.restore_indexes();

        // Indexes are rebuilt, so lookups work after recovery.
        assert!(restored.tool_activity.terminal("shell-a").is_some());
        assert!(restored.tool_activity.invocation("call-1").is_some());
    }

    #[test]
    fn subagent_tool_calls_are_tracked_and_attributed() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![json!({
                "id": "nested-start",
                "type": "tool.execution_start",
                "timestamp": "1",
                "agentId": "agent-7",
                "data": {
                    "toolCallId": "nested-1",
                    "toolName": "grep",
                    "arguments": {"pattern": "fn main"}
                }
            })],
        );

        let invocation = state
            .tool_activity
            .invocation("nested-1")
            .expect("nested invocation is tracked");
        // Nested tool calls stay out of the root transcript but must remain
        // visible as activity so the UI can nest them under their subagent.
        assert_eq!(invocation.agent_id.as_deref(), Some("agent-7"));
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn mcp_tools_are_classified_by_source() {
        let catalog = ToolCatalog {
            tools: vec![
                ToolDescriptor {
                    name: "view".to_owned(),
                    namespaced_name: None,
                    description: "read".to_owned(),
                    source: ToolSource::Builtin,
                    class: ToolClass::FileRead,
                },
                ToolDescriptor {
                    name: "search_code".to_owned(),
                    namespaced_name: Some("github/search_code".to_owned()),
                    description: "search".to_owned(),
                    source: ToolSource::Mcp {
                        server: "github".to_owned(),
                    },
                    class: ToolClass::Other,
                },
            ],
            discovered_at: Some("now".to_owned()),
            error: None,
        };

        let report = CapabilityReport::from_catalog(&catalog);
        assert_eq!(
            report.get(CapabilityId::FileRead).unwrap().status,
            CapabilityStatus::Available
        );
        assert_eq!(
            report.get(CapabilityId::GithubMcp).unwrap().status,
            CapabilityStatus::Available
        );
        // No shell tool was advertised, so the capability must report as
        // unavailable rather than silently assumed present.
        assert_eq!(
            report.get(CapabilityId::Shell).unwrap().status,
            CapabilityStatus::Unavailable
        );
        assert!(!report.is_self_hosting_ready());
    }

    /// The timeline is what makes a session observable: messages and the tool
    /// calls between them, in the order they happened.
    #[test]
    fn timeline_interleaves_messages_and_tool_calls() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({"id":"u","type":"user.message","data":{"content":"fix the bug"}}),
                json!({"id":"t1","type":"tool.execution_start",
                       "data":{"toolCallId":"c1","toolName":"grep",
                               "arguments":{"pattern":"fn main"}}}),
                json!({"id":"t1c","type":"tool.execution_complete",
                       "data":{"toolCallId":"c1","success":true}}),
                json!({"id":"t2","type":"tool.execution_start",
                       "data":{"toolCallId":"c2","toolName":"str_replace_editor",
                               "arguments":{"path":"src/lib.rs"}}}),
                json!({"id":"t2c","type":"tool.execution_complete",
                       "data":{"toolCallId":"c2","success":true,
                               "result":{"detailedContent":"@@ -1 +1 @@\n-old\n+new"}}}),
                json!({"id":"a","type":"assistant.message",
                       "data":{"messageId":"m","content":"fixed"}}),
            ],
        );

        let timeline = state.timeline();
        let shape: Vec<&str> = timeline
            .iter()
            .map(|entry| match entry {
                TimelineEntry::Message(_) => "message",
                TimelineEntry::Tool(_) => "tool",
                TimelineEntry::Interaction(_) => "interaction",
            })
            .collect();
        assert_eq!(shape, ["message", "tool", "tool", "message"]);

        // Ordering is by sequence, so the entries stay in causal order.
        let sequences: Vec<u64> = timeline.iter().map(TimelineEntry::sequence).collect();
        let mut sorted = sequences.clone();
        sorted.sort_unstable();
        assert_eq!(sequences, sorted);

        // The edit carries its diff, which is what makes it reviewable.
        let TimelineEntry::Tool(edit) = timeline[2] else {
            panic!("expected a tool entry");
        };
        assert_eq!(edit.verb(), "Edit");
        assert_eq!(edit.summary(), "src/lib.rs");
        assert!(edit.diff().is_some_and(|diff| diff.contains("+new")));
    }

    /// A multi-line command must not become a multi-line header; it filled the
    /// window with one tool call.
    #[test]
    fn multiline_commands_get_a_single_line_header() {
        let mut state = SessionSnapshot::new(metadata());
        let script = "python3 -c \"\na,b=0,1\nfor i in range(100):\n    print(a)\n\"";
        apply_all(
            &mut state,
            vec![json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command": script},
                                "shellToolInfo":{"displayCommand": script,
                                                 "hasWriteFileRedirection":false,
                                                 "possiblePaths":[]}}})],
        );

        let invocation = state.tool_activity.invocation("c1").expect("invocation");
        let header = invocation.summary_line();
        assert_eq!(
            header.lines().count(),
            1,
            "header must be one line: {header}"
        );
        assert!(header.starts_with("python3 -c"));
        // The rest stays available for the scrollable detail block.
        let detail = invocation.multiline_summary().expect("detail");
        assert!(detail.contains("range(100)"));
    }

    /// A single-line target needs no detail block.
    #[test]
    fn single_line_targets_have_no_detail_block() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"str_replace_editor",
                                "arguments":{"path":"src/lib.rs"}}})],
        );
        let invocation = state.tool_activity.invocation("c1").expect("invocation");
        assert_eq!(invocation.summary_line(), "src/lib.rs");
        assert!(invocation.multiline_summary().is_none());
    }

    #[test]
    fn shell_control_summaries_do_not_expose_internal_shell_ids() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"read_bash",
                                "arguments":{"shellId":"internal-shell-37"}}})],
        );

        let invocation = state.tool_activity.invocation("c1").expect("invocation");
        assert_eq!(invocation.summary(), "Check command output");
        assert!(!invocation.summary().contains("internal-shell-37"));
    }

    #[test]
    fn shell_control_summaries_reuse_the_commands_human_readable_description() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({"id":"start","type":"tool.execution_start",
                       "data":{"toolCallId":"c1","toolName":"bash",
                               "arguments":{"command":"./scripts/self-dev.sh run",
                                            "description":"Building and launching GCABB",
                                            "shellId":"internal-shell-37"}}}),
                json!({"id":"read","type":"tool.execution_start",
                       "data":{"toolCallId":"c2","toolName":"read_bash",
                               "arguments":{"shellId":"internal-shell-37"}}}),
            ],
        );

        let terminal = state
            .tool_activity
            .terminal("internal-shell-37")
            .expect("terminal");
        let read = state.tool_activity.invocation("c2").expect("invocation");
        assert_eq!(
            terminal.command.as_deref(),
            Some("Building and launching GCABB")
        );
        assert_eq!(read.summary(), "Building and launching GCABB");
    }

    /// Very long single-line targets are truncated rather than wrapped.
    #[test]
    fn long_targets_are_truncated_in_the_header() {
        let mut state = SessionSnapshot::new(metadata());
        let long = "x".repeat(400);
        apply_all(
            &mut state,
            vec![json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"grep",
                                "arguments":{"pattern": long}}})],
        );
        let header = state
            .tool_activity
            .invocation("c1")
            .expect("invocation")
            .summary_line();
        assert!(
            header.chars().count() <= 121,
            "got {} chars",
            header.chars().count()
        );
        assert!(header.ends_with('…'));
    }

    /// Delegated work belongs under the task that asked for it, not flattened
    /// into the main thread.
    #[test]
    fn subagent_calls_nest_under_their_task() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({"id":"t","type":"tool.execution_start",
                       "data":{"toolCallId":"task-1","toolName":"task",
                               "arguments":{"description":"survey harnesses"}}}),
                json!({"id":"sa","type":"subagent.started",
                       "data":{"agentId":"agent-7","parentToolCallId":"task-1"}}),
                json!({"id":"n","type":"tool.execution_start","agentId":"agent-7",
                       "data":{"toolCallId":"nested-1","toolName":"grep",
                               "arguments":{"pattern":"tools.list"}}}),
            ],
        );

        // The root timeline shows the task, not the subagent's own calls.
        let timeline = state.timeline();
        assert_eq!(timeline.len(), 1);
        let TimelineEntry::Tool(task) = timeline[0] else {
            panic!("expected the task entry");
        };
        assert_eq!(task.call_id, "task-1");

        let children = state.tool_activity.children_of("task-1");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].call_id, "nested-1");
        assert_eq!(children[0].summary(), "tools.list");
    }

    /// An unrelated task must not adopt another task's subagent work.
    #[test]
    fn subagent_calls_only_nest_under_their_own_task() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({"id":"t1","type":"tool.execution_start",
                       "data":{"toolCallId":"task-1","toolName":"task","arguments":{}}}),
                json!({"id":"t2","type":"tool.execution_start",
                       "data":{"toolCallId":"task-2","toolName":"task","arguments":{}}}),
                json!({"id":"sa","type":"subagent.started",
                       "data":{"agentId":"agent-7","parentToolCallId":"task-2"}}),
                json!({"id":"n","type":"tool.execution_start","agentId":"agent-7",
                       "data":{"toolCallId":"nested-1","toolName":"grep","arguments":{}}}),
            ],
        );

        assert!(state.tool_activity.children_of("task-1").is_empty());
        assert_eq!(state.tool_activity.children_of("task-2").len(), 1);
    }

    /// Cancelling mid-stream leaves partial text on screen that the runtime
    /// never committed, so the next turn cannot see it. The transcript must
    /// say so rather than showing it as ordinary conversation.
    #[test]
    fn aborting_marks_streaming_output_as_interrupted() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({"id":"u","type":"user.message","data":{"content":"write a story"}}),
                json!({"id":"s","type":"assistant.message_start","data":{"messageId":"m"}}),
                json!({"id":"d","type":"assistant.message_delta",
                       "data":{"messageId":"m","deltaContent":"# The Ascendance"}}),
                json!({"id":"a","type":"abort","data":{"reason":"user_initiated"}}),
            ],
        );

        let message = state
            .transcript
            .iter()
            .find(|message| message.role == TranscriptRole::Assistant)
            .expect("assistant message");
        assert_eq!(message.state, TranscriptState::Interrupted);
        // The text the user saw is kept; only its status changes.
        assert_eq!(message.content, "# The Ascendance");
    }

    /// Cancelling tears down the shells the turn started, but the runtime
    /// reports no completion for them. A terminal left showing "running"
    /// reads as work still in flight when nothing is running at all.
    #[test]
    fn aborting_marks_running_terminals_cancelled() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({"id":"t","type":"tool.execution_start",
                       "data":{"toolCallId":"c1","toolName":"bash",
                               "arguments":{"command":"sleep 900","shellId":"shell-1"}}}),
                json!({"id":"a","type":"abort","data":{"reason":"user_initiated"}}),
            ],
        );

        let terminal = state
            .tool_activity
            .terminal("shell-1")
            .expect("the shell was tracked");
        assert_eq!(terminal.state, crate::tools::TerminalState::Cancelled);
        assert!(
            state.tool_activity.active_terminals().is_empty(),
            "a cancelled shell still counted as active work"
        );
    }

    /// A shell that already exited keeps its exit, not a cancellation.
    #[test]
    fn aborting_does_not_relabel_finished_terminals() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({"id":"t","type":"tool.execution_start",
                       "data":{"toolCallId":"c1","toolName":"bash",
                               "arguments":{"command":"echo hi","shellId":"shell-1"}}}),
                json!({"id":"d","type":"tool.execution_complete",
                       "data":{"toolCallId":"c1","toolName":"bash","success":true,
                               "result":{"contents":[
                                   {"type":"shell_exit","shellId":"shell-1","exitCode":0}]}}}),
                json!({"id":"a","type":"abort","data":{"reason":"user_initiated"}}),
            ],
        );

        let terminal = state
            .tool_activity
            .terminal("shell-1")
            .expect("the shell was tracked");
        assert_eq!(terminal.state, crate::tools::TerminalState::Exited);
    }

    /// A completed message must not be relabelled by a later cancellation.
    #[test]
    fn aborting_does_not_touch_completed_messages() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![
                json!({"id":"u","type":"user.message","data":{"content":"hi"}}),
                json!({"id":"s","type":"assistant.message_start","data":{"messageId":"m"}}),
                json!({"id":"f","type":"assistant.message",
                       "data":{"messageId":"m","content":"hello"}}),
                json!({"id":"a","type":"abort","data":{"reason":"user_initiated"}}),
            ],
        );

        let message = state
            .transcript
            .iter()
            .find(|message| message.role == TranscriptRole::Assistant)
            .expect("assistant message");
        assert_eq!(message.state, TranscriptState::Complete);
    }

    #[test]
    fn combined_editor_tool_satisfies_both_read_and_write_capabilities() {
        // The runtime exposes file editing to the model as a single
        // `str_replace_editor` tool rather than the CLI's `view`/`create`/`edit`
        // aliases, so one tool must evidence both capabilities.
        let catalog = ToolCatalog {
            tools: vec![
                ToolDescriptor {
                    name: "str_replace_editor".to_owned(),
                    namespaced_name: None,
                    description: "edit files".to_owned(),
                    source: ToolSource::Builtin,
                    class: ToolClass::classify("str_replace_editor"),
                },
                ToolDescriptor {
                    name: "grep".to_owned(),
                    namespaced_name: None,
                    description: "search".to_owned(),
                    source: ToolSource::Builtin,
                    class: ToolClass::classify("grep"),
                },
                ToolDescriptor {
                    name: "bash".to_owned(),
                    namespaced_name: None,
                    description: "shell".to_owned(),
                    source: ToolSource::Builtin,
                    class: ToolClass::classify("bash"),
                },
            ],
            discovered_at: Some("now".to_owned()),
            error: None,
        };

        assert_eq!(
            ToolClass::classify("str_replace_editor"),
            ToolClass::FileEditor
        );
        assert!(ToolClass::FileEditor.reads_files());
        assert!(ToolClass::FileEditor.writes_files());
        assert!(ToolClass::FileEditor.mutates_worktree());

        let report = CapabilityReport::from_catalog(&catalog);
        for id in [
            CapabilityId::FileRead,
            CapabilityId::FileWrite,
            CapabilityId::Search,
            CapabilityId::Shell,
        ] {
            assert_eq!(
                report.get(id).unwrap().status,
                CapabilityStatus::Available,
                "{id:?} should be satisfied"
            );
        }
    }

    #[test]
    fn sessions_group_by_repository_not_by_worktree() {
        // Worktrees of one repository must collapse into a single project.
        // Grouping by worktree path produced one "project" per worktree, named
        // after the generated branch directory.
        let mut worktree_a = metadata();
        worktree_a.project_path = "/worktrees/feature-a".to_owned();
        worktree_a.repository_root = Some("/src/gcabb".to_owned());

        let mut worktree_b = metadata();
        worktree_b.project_path = "/worktrees/feature-b".to_owned();
        worktree_b.repository_root = Some("/src/gcabb".to_owned());

        assert_eq!(worktree_a.project_key(), worktree_b.project_key());
        assert_eq!(worktree_a.project_key(), "/src/gcabb");
    }

    #[test]
    fn sessions_without_a_repository_fall_back_to_their_worktree() {
        let mut legacy = metadata();
        legacy.repository_root = None;
        legacy.project_path = "/worktrees/legacy".to_owned();
        assert_eq!(legacy.project_key(), "/worktrees/legacy");
    }

    #[test]
    fn optional_capabilities_are_not_reported_as_blocking() {
        // An absent MCP server or skill tool is worth surfacing, but it does
        // not stop the edit-command-diff loop, so it must not be counted as
        // blocking in the headline badge.
        let mut report = CapabilityReport::default();
        for id in [
            CapabilityId::FileRead,
            CapabilityId::FileWrite,
            CapabilityId::Search,
            CapabilityId::Shell,
            CapabilityId::Changes,
        ] {
            report.set(Capability {
                id,
                status: CapabilityStatus::Available,
                detail: String::new(),
                evidence: Vec::new(),
            });
        }
        report.set(Capability {
            id: CapabilityId::GithubMcp,
            status: CapabilityStatus::Unavailable,
            detail: "no MCP configured".to_owned(),
            evidence: Vec::new(),
        });

        assert!(report.blocking().is_empty());
        assert!(report.is_self_hosting_ready());
        // It still shows up as degraded so the panel can explain it.
        assert_eq!(report.degraded().len(), 1);

        report.set(Capability {
            id: CapabilityId::Shell,
            status: CapabilityStatus::Unavailable,
            detail: "no shell tool".to_owned(),
            evidence: Vec::new(),
        });
        assert_eq!(report.blocking().len(), 1);
        assert!(!report.is_self_hosting_ready());
    }

    /// An attachment is part of what was asked. A transcript that drops it
    /// cannot be read back to understand the conversation, because the
    /// question referred to something no longer shown.
    #[test]
    fn a_user_message_keeps_the_attachments_it_was_sent_with() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![json!({"id":"u","type":"user.message","data":{
                "content":"what is wrong here",
                "attachments":[{
                    "type":"file",
                    "displayName":"Pasted Image",
                    "path":"/tmp/clipboard.png",
                    "mimeType":"image/png"
                }]
            }})],
        );

        let message = state
            .transcript
            .iter()
            .find(|message| message.role == TranscriptRole::User)
            .expect("user message");
        assert_eq!(message.attachments.len(), 1, "the attachment was dropped");
        assert_eq!(message.attachments[0].display_name, "Pasted Image");
        assert!(message.attachments[0].is_image);
    }

    /// An attachment with no text is still a message worth showing.
    #[test]
    fn an_attachment_only_message_is_kept() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![json!({"id":"u","type":"user.message","data":{
                "content":"",
                "attachments":[{
                    "type":"file",
                    "displayName":"Pasted Image",
                    "path":"/tmp/clipboard.png"
                }]
            }})],
        );

        assert_eq!(
            state.transcript.len(),
            1,
            "a message carrying only a screenshot vanished from the transcript"
        );
        assert_eq!(state.transcript[0].attachments.len(), 1);
    }

    /// Observed live: the runtime echoes an attachment back in the form it
    /// was sent. A blob comes back as a blob, with no path, so nothing can be
    /// loaded from it later. This is why pasted images must be sent as files.
    #[test]
    fn a_blob_attachment_comes_back_without_a_path() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![json!({"id":"u","type":"user.message","data":{
                "content":"look",
                "attachments":[{
                    "type":"blob",
                    "displayName":"Pasted image 1",
                    "mimeType":"image/png",
                    "data":"iVBORw=="
                }]
            }})],
        );

        let attachment = &state.transcript[0].attachments[0];
        assert!(attachment.is_image, "a PNG blob is still an image");
        assert!(
            attachment.path.is_none(),
            "a blob has no path, so it cannot be shown again"
        );
    }

    /// A file attachment echoes back with the path it was sent with, which is
    /// what makes it possible to show the picture again later.
    #[test]
    fn a_file_attachment_comes_back_with_its_path() {
        let mut state = SessionSnapshot::new(metadata());
        apply_all(
            &mut state,
            vec![json!({"id":"u","type":"user.message","data":{
                "content":"look",
                "attachments":[{
                    "type":"file",
                    "displayName":"Pasted image 1",
                    "path":"/tmp/gcabb/attachments/abc-clipboard.png"
                }]
            }})],
        );

        let attachment = &state.transcript[0].attachments[0];
        assert!(attachment.is_image);
        assert_eq!(
            attachment.path.as_deref(),
            Some("/tmp/gcabb/attachments/abc-clipboard.png")
        );
    }

    /// A pasted screenshot has no path, so it must carry its own bytes.
    #[test]
    fn a_pasted_image_carries_its_bytes_not_a_path() {
        let attachment =
            PromptAttachment::from_image_bytes(&[0x89, 0x50, 0x4E, 0x47], "image/png", 1);
        let PromptAttachment::Image {
            data,
            mime_type,
            display_name,
        } = &attachment
        else {
            panic!("a pasted image must not become a file reference");
        };
        assert_eq!(data, "iVBORw==");
        assert_eq!(mime_type, "image/png");
        assert_eq!(display_name, "Pasted image 1");
        assert!(attachment.is_image());
    }

    /// A file is judged an image by extension, since it has no declared type.
    #[test]
    fn a_chosen_file_is_recognized_as_an_image_by_extension() {
        assert!(PromptAttachment::from_path(std::path::Path::new("/tmp/Shot.PNG")).is_image());
        assert!(!PromptAttachment::from_path(std::path::Path::new("/tmp/notes.txt")).is_image());
    }

    /// A chat has no checkout, so an absent changes view is not a defect it
    /// suffered. Counting it as blocked told the user something was broken
    /// when the session was working exactly as designed.
    #[test]
    fn a_chat_is_not_blocked_by_having_no_changes_view() {
        let mut report = CapabilityReport::default();
        for id in [
            CapabilityId::FileRead,
            CapabilityId::FileWrite,
            CapabilityId::Search,
            CapabilityId::Shell,
        ] {
            report.set(Capability {
                id,
                status: CapabilityStatus::Available,
                detail: String::new(),
                evidence: Vec::new(),
            });
        }
        report.set(Capability {
            id: CapabilityId::Changes,
            status: CapabilityStatus::Unavailable,
            detail: "Chats are not attached to a repository.".to_owned(),
            evidence: Vec::new(),
        });

        assert!(
            report.blocking_for(SessionKind::Chat).is_empty(),
            "a chat was reported blocked for lacking a repository it never had"
        );
        // The same report on a project session is a genuine problem.
        assert_eq!(report.blocking_for(SessionKind::Project).len(), 1);
    }

    /// A missing shell blocks any session, chat or not.
    #[test]
    fn a_chat_is_still_blocked_by_a_missing_shell() {
        let mut report = CapabilityReport::default();
        report.set(Capability {
            id: CapabilityId::Shell,
            status: CapabilityStatus::Unavailable,
            detail: "no shell tool".to_owned(),
            evidence: Vec::new(),
        });
        assert_eq!(report.blocking_for(SessionKind::Chat).len(), 1);
    }

    #[test]
    fn failed_discovery_leaves_capabilities_unknown_with_the_reason() {
        let catalog = ToolCatalog {
            error: Some("transport closed".to_owned()),
            ..ToolCatalog::default()
        };
        let report = CapabilityReport::from_catalog(&catalog);
        let file_read = report.get(CapabilityId::FileRead).unwrap();
        assert_eq!(file_read.status, CapabilityStatus::Unknown);
        assert!(file_read.detail.contains("transport closed"));
    }
}
