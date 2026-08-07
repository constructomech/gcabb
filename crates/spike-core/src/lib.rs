use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DOMAIN_EVENT_VERSION: u16 = 1;

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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DomainEvent {
    pub version: u16,
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
}

impl DomainEvent {
    #[must_use]
    pub fn from_sdk_event(raw: &Value) -> Self {
        let event_type = raw.get("type").and_then(Value::as_str).unwrap_or("unknown");
        let data = raw.get("data").cloned().unwrap_or(Value::Null);

        Self {
            version: DOMAIN_EVENT_VERSION,
            id: string_field(raw, "id").unwrap_or_else(|| format!("unidentified:{event_type}")),
            parent_id: string_field(raw, "parent_id").or_else(|| string_field(raw, "parentId")),
            agent_id: string_field(raw, "agent_id").or_else(|| string_field(raw, "agentId")),
            timestamp: string_field(raw, "timestamp").unwrap_or_default(),
            source_type: event_type.to_owned(),
            kind: classify_kind(event_type),
            state: classify_state(event_type, &data),
            visibility: Visibility::Observed,
            summary: summarize(event_type, &data),
            details: data,
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

    #[test]
    fn normalizes_tool_event_without_discarding_raw_details() {
        let raw = json!({
            "id": "event-1",
            "timestamp": "2026-08-06T12:00:00Z",
            "parentId": "turn-1",
            "agentId": "agent-2",
            "type": "tool.execution_start",
            "data": {"toolName": "shell", "arguments": {"command": "cargo test"}}
        });

        let event = DomainEvent::from_sdk_event(&raw);

        assert_eq!(event.kind, ActivityKind::Tool);
        assert_eq!(event.state, ActivityState::Running);
        assert_eq!(event.visibility, Visibility::Observed);
        assert_eq!(event.parent_id.as_deref(), Some("turn-1"));
        assert_eq!(event.agent_id.as_deref(), Some("agent-2"));
        assert_eq!(event.details["arguments"]["command"], "cargo test");
    }

    #[test]
    fn unknown_events_remain_forward_compatible() {
        let event = DomainEvent::from_sdk_event(&json!({
            "type": "future.event",
            "data": {"newField": true}
        }));

        assert_eq!(event.kind, ActivityKind::System);
        assert_eq!(event.state, ActivityState::Queued);
        assert_eq!(event.details["newField"], true);
    }

    #[test]
    fn payload_outcome_overrides_completion_event_name() {
        let failed = DomainEvent::from_sdk_event(&json!({
            "type": "tool.execution_complete",
            "data": {"success": false, "error": {"message": "boom"}}
        }));
        let aborted = DomainEvent::from_sdk_event(&json!({
            "type": "session.idle",
            "data": {"aborted": true}
        }));

        assert_eq!(failed.state, ActivityState::Failed);
        assert_eq!(aborted.state, ActivityState::Cancelled);
    }

    #[test]
    fn summaries_are_bounded_by_character_count() {
        let long = "é".repeat(200);
        let event = DomainEvent::from_sdk_event(&json!({
            "type": "assistant.message",
            "data": {"message": long}
        }));

        assert_eq!(
            event.summary.chars().count(),
            "assistant.message: ".chars().count() + 163
        );
    }
}
