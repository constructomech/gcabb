//! The prompt queue: work the developer has lined up for a session.
//!
//! GCABB owns this queue rather than mirroring the runtime's. The Copilot CLI
//! keeps its pending prompts in memory only, so anything queued there is lost
//! when the session disconnects. Holding the queue here instead makes it
//! durable, editable while the agent is busy, and editable while no runtime
//! session exists at all.
//!
//! The runtime queue is therefore a projection of this one: items are handed
//! over for delivery and the runtime reports back what it has drained.

use serde::{Deserialize, Serialize};

/// Where a queued item is in its lifecycle.
///
/// Only `Pending` items are editable. The terminal states are retained so the
/// UI can show what a session has already worked through.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueItemState {
    /// Waiting to be delivered to the runtime.
    #[default]
    Pending,
    /// Handed to the runtime and awaiting or receiving a turn.
    Dispatched,
    /// The runtime finished the turn for this item.
    Completed,
    /// Delivery failed; `QueueItem::error` explains why.
    Failed,
    /// Removed by the developer before it was delivered.
    Cancelled,
}

impl QueueItemState {
    /// Whether the item still occupies a place in the pending queue.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Whether the item has reached a state it will not leave.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Dispatched => "Running",
            Self::Completed => "Done",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// How a queued item should reach the agent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueDelivery {
    /// Run when the session next becomes idle.
    #[default]
    WhenIdle,
    /// Interrupt an in-flight turn rather than waiting for it to finish.
    Steer,
}

/// A single prompt the developer has queued for a session.
///
/// The identifier is minted by GCABB and is stable for the life of the item,
/// including across restarts. Runtime-assigned identifiers are recorded
/// separately in `runtime_id` because the runtime reissues them per session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueueItem {
    pub id: String,
    pub session_id: String,
    /// Ordering key within the session. Contiguity is not guaranteed: removals
    /// leave gaps, which are closed the next time the queue is compacted.
    pub position: i64,
    /// Prompt text sent to the agent.
    pub prompt: String,
    /// Text shown in the UI when it should differ from the prompt.
    #[serde(default)]
    pub display_prompt: Option<String>,
    #[serde(default)]
    pub state: QueueItemState,
    #[serde(default)]
    pub delivery: QueueDelivery,
    /// Agent mode to request for this item, or the session's mode when unset.
    #[serde(default)]
    pub agent_mode: Option<String>,
    /// Identifier the runtime assigned when the item was handed over. Cleared
    /// whenever the runtime queue is rebuilt, since the runtime reissues ids.
    #[serde(default)]
    pub runtime_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Why delivery failed, when `state` is `Failed`.
    #[serde(default)]
    pub error: Option<String>,
}

impl QueueItem {
    /// Text to show for this item.
    #[must_use]
    pub fn label(&self) -> &str {
        self.display_prompt.as_deref().unwrap_or(&self.prompt)
    }

    /// A single-line, length-capped rendering of [`Self::label`] for list rows.
    #[must_use]
    pub fn summary(&self, max_chars: usize) -> String {
        let collapsed = self
            .label()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if collapsed.chars().count() <= max_chars {
            return collapsed;
        }
        let kept: String = collapsed
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        format!("{}…", kept.trim_end())
    }
}

/// The queue as the UI sees it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueueView {
    /// Every item GCABB is tracking, ordered by `position`.
    #[serde(default)]
    pub items: Vec<QueueItem>,
    /// Whether draining is suspended. While paused, items stay pending even
    /// when the session is idle.
    #[serde(default)]
    pub paused: bool,
    /// Steering messages the runtime is holding for the active turn. Reported
    /// by the runtime, so it can include input from other clients.
    #[serde(default)]
    pub runtime_steering: Vec<String>,
    /// Set when the queue could not be synchronised with the runtime.
    #[serde(default)]
    pub error: Option<String>,
}

impl QueueView {
    /// Items still waiting to run, in queue order.
    pub fn pending(&self) -> impl Iterator<Item = &QueueItem> {
        self.items.iter().filter(|item| item.state.is_pending())
    }

    /// How many items are waiting to run.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending().count()
    }

    /// Whether anything is worth showing for this session.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The item that should be delivered next, if any.
    #[must_use]
    pub fn next_pending(&self) -> Option<&QueueItem> {
        self.pending().min_by_key(|item| item.position)
    }

    /// Look up an item by its GCABB identifier.
    #[must_use]
    pub fn item(&self, id: &str) -> Option<&QueueItem> {
        self.items.iter().find(|item| item.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, position: i64, state: QueueItemState) -> QueueItem {
        QueueItem {
            id: id.to_owned(),
            session_id: "session".to_owned(),
            position,
            prompt: format!("prompt {id}"),
            display_prompt: None,
            state,
            delivery: QueueDelivery::WhenIdle,
            agent_mode: None,
            runtime_id: None,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            error: None,
        }
    }

    #[test]
    fn next_pending_follows_position_not_insertion_order() {
        let view = QueueView {
            items: vec![
                item("c", 30, QueueItemState::Pending),
                item("a", 10, QueueItemState::Pending),
                item("b", 20, QueueItemState::Pending),
            ],
            ..QueueView::default()
        };
        assert_eq!(view.next_pending().map(|item| item.id.as_str()), Some("a"));
        assert_eq!(view.pending_count(), 3);
    }

    #[test]
    fn dispatched_and_terminal_items_are_not_pending() {
        let view = QueueView {
            items: vec![
                item("done", 10, QueueItemState::Completed),
                item("running", 20, QueueItemState::Dispatched),
                item("cancelled", 30, QueueItemState::Cancelled),
                item("waiting", 40, QueueItemState::Pending),
            ],
            ..QueueView::default()
        };
        assert_eq!(view.pending_count(), 1);
        assert_eq!(
            view.next_pending().map(|item| item.id.as_str()),
            Some("waiting")
        );
        assert!(!view.is_empty());
    }

    #[test]
    fn summary_collapses_whitespace_and_truncates() {
        let mut queued = item("a", 10, QueueItemState::Pending);
        queued.prompt = "first line\n\nsecond    line".to_owned();
        assert_eq!(queued.summary(80), "first line second line");
        assert_eq!(queued.summary(8), "first l…");
    }

    #[test]
    fn display_prompt_overrides_prompt_for_labels() {
        let mut queued = item("a", 10, QueueItemState::Pending);
        queued.display_prompt = Some("shown".to_owned());
        assert_eq!(queued.label(), "shown");
    }

    #[test]
    fn queue_view_round_trips_through_json() {
        let view = QueueView {
            items: vec![item("a", 10, QueueItemState::Pending)],
            paused: true,
            runtime_steering: vec!["steer".to_owned()],
            error: None,
        };
        let encoded = serde_json::to_string(&view).expect("serialize queue view");
        let decoded: QueueView = serde_json::from_str(&encoded).expect("deserialize queue view");
        assert_eq!(view, decoded);
    }

    #[test]
    fn queue_view_defaults_fill_missing_fields() {
        let decoded: QueueView = serde_json::from_str("{}").expect("deserialize empty queue view");
        assert!(decoded.is_empty());
        assert!(!decoded.paused);
        assert!(decoded.next_pending().is_none());
    }
}
