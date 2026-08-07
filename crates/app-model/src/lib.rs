use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DOMAIN_EVENT_VERSION: u16 = 1;
pub const SNAPSHOT_VERSION: u16 = 1;

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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionMetadata {
    pub id: String,
    pub sdk_session_id: String,
    pub project_path: String,
    pub title: String,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub created_at: String,
    pub updated_at: String,
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
    pub activities: Vec<DomainEvent>,
    pub last_error: Option<String>,
    #[serde(skip)]
    seen_event_ids: HashSet<String>,
}

impl SessionSnapshot {
    #[must_use]
    pub fn new(metadata: SessionMetadata) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            metadata,
            status: SessionStatus::Starting,
            last_sequence: 0,
            activities: Vec::new(),
            last_error: None,
            seen_event_ids: HashSet::new(),
        }
    }

    pub fn restore_indexes(&mut self) {
        self.seen_event_ids = self
            .activities
            .iter()
            .map(|event| event.id.clone())
            .collect();
    }

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
        self.status = status_for_event(&event);
        if self.status == SessionStatus::Failed {
            self.last_error = Some(event.summary.clone());
        }
        self.seen_event_ids.insert(event.id.clone());
        self.activities.push(event);
        ApplyOutcome::Applied
    }

    #[must_use]
    pub fn immutable(self) -> Arc<Self> {
        Arc::new(self)
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

fn status_for_event(event: &DomainEvent) -> SessionStatus {
    if event.source_type == "session.idle" {
        return if event.state == ActivityState::Cancelled {
            SessionStatus::Cancelled
        } else {
            SessionStatus::Idle
        };
    }
    if event.source_type.ends_with(".requested") {
        return SessionStatus::Waiting;
    }
    match event.state {
        ActivityState::Waiting => SessionStatus::Waiting,
        ActivityState::Failed => SessionStatus::Failed,
        ActivityState::Cancelled => SessionStatus::Cancelled,
        ActivityState::Running | ActivityState::Queued | ActivityState::Completed => {
            SessionStatus::Running
        }
    }
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
            title: "Test".to_owned(),
            model: None,
            mode: None,
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
}
