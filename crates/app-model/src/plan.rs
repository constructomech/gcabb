//! The agent's own task list.
//!
//! Distinct from the prompt queue in [`crate::queue`]: that queue is work the
//! developer lined up for the agent, while this is the breakdown the agent
//! made for itself while working. It is read-only — the runtime exposes no way
//! to write these rows — so it is shown as reporting, not as something to edit.

use serde::{Deserialize, Serialize};

/// Progress of a single agent todo.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTodoStatus {
    #[default]
    Pending,
    InProgress,
    Done,
    Blocked,
}

impl AgentTodoStatus {
    /// Decode the runtime's status string.
    ///
    /// Unrecognised values become `Pending` rather than being dropped: the
    /// column is free-form, and a todo with an odd status is still outstanding
    /// work worth showing.
    #[must_use]
    pub fn from_runtime(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "in_progress" | "in-progress" | "active" => Self::InProgress,
            "done" | "completed" | "complete" => Self::Done,
            "blocked" => Self::Blocked,
            _ => Self::Pending,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::InProgress => "In progress",
            Self::Done => "Done",
            Self::Blocked => "Blocked",
        }
    }

    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Done)
    }

    /// The status a click advances to.
    ///
    /// Blocked is not in the cycle: the agent sets it to record that
    /// something is in its way, and a developer clicking through statuses is
    /// not saying that.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Pending => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done | Self::Blocked => Self::Pending,
        }
    }

    /// The value the runtime's schema stores.
    #[must_use]
    pub const fn as_runtime(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

/// One row of the agent's todo list.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentTodo {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: AgentTodoStatus,
    /// Identifiers of todos that must finish before this one.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl AgentTodo {
    /// Whether this todo is waiting on work that has not finished.
    #[must_use]
    pub fn is_blocked_by(&self, unfinished: &[String]) -> bool {
        self.depends_on.iter().any(|id| unfinished.contains(id))
    }
}

/// The agent's task list as reported by the runtime.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentPlan {
    #[serde(default)]
    pub todos: Vec<AgentTodo>,
    /// Whether the app can change these rows.
    ///
    /// True only when GCABB hosts the session filesystem and therefore owns
    /// the database the agent writes through. The panel uses this to decide
    /// whether to offer editing at all, rather than offering it and failing.
    #[serde(default)]
    pub writable: bool,
}

impl AgentPlan {
    /// Whether there is anything worth showing.
    ///
    /// A writable plan is worth showing even when empty, since that is where
    /// the developer adds the first entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.todos.is_empty()
    }

    #[must_use]
    pub fn total(&self) -> usize {
        self.todos.len()
    }

    #[must_use]
    pub fn completed(&self) -> usize {
        self.todos
            .iter()
            .filter(|todo| todo.status.is_finished())
            .count()
    }

    /// The todo the agent is working on, if it has said.
    #[must_use]
    pub fn current(&self) -> Option<&AgentTodo> {
        self.todos
            .iter()
            .find(|todo| todo.status == AgentTodoStatus::InProgress)
    }

    /// Identifiers of todos that have not finished.
    #[must_use]
    pub fn unfinished_ids(&self) -> Vec<String> {
        self.todos
            .iter()
            .filter(|todo| !todo.status.is_finished())
            .map(|todo| todo.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(id: &str, status: AgentTodoStatus) -> AgentTodo {
        AgentTodo {
            id: id.to_owned(),
            title: format!("Doing {id}"),
            description: None,
            status,
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn runtime_statuses_decode_including_unknown_values() {
        assert_eq!(
            AgentTodoStatus::from_runtime("in_progress"),
            AgentTodoStatus::InProgress
        );
        assert_eq!(AgentTodoStatus::from_runtime("DONE"), AgentTodoStatus::Done);
        assert_eq!(
            AgentTodoStatus::from_runtime("blocked"),
            AgentTodoStatus::Blocked
        );
        // An unrecognised status still represents outstanding work.
        assert_eq!(
            AgentTodoStatus::from_runtime("wat"),
            AgentTodoStatus::Pending
        );
    }

    #[test]
    fn progress_counts_only_finished_todos() {
        let plan = AgentPlan {
            todos: vec![
                todo("a", AgentTodoStatus::Done),
                todo("b", AgentTodoStatus::InProgress),
                todo("c", AgentTodoStatus::Blocked),
            ],
            writable: false,
        };
        assert_eq!(plan.total(), 3);
        assert_eq!(plan.completed(), 1);
        assert_eq!(plan.current().map(|todo| todo.id.as_str()), Some("b"));
        assert_eq!(plan.unfinished_ids(), vec!["b".to_owned(), "c".to_owned()]);
    }

    #[test]
    fn dependencies_only_block_while_the_dependency_is_unfinished() {
        let mut blocked = todo("b", AgentTodoStatus::Pending);
        blocked.depends_on = vec!["a".to_owned()];
        assert!(blocked.is_blocked_by(&["a".to_owned()]));
        assert!(!blocked.is_blocked_by(&[]));
    }

    #[test]
    fn an_empty_plan_is_reported_as_empty() {
        let plan = AgentPlan::default();
        assert!(plan.is_empty());
        assert_eq!(plan.completed(), 0);
        assert!(plan.current().is_none());
    }

    #[test]
    fn clicking_through_statuses_never_lands_on_blocked() {
        let mut status = AgentTodoStatus::Pending;
        let mut seen = Vec::new();
        for _ in 0..6 {
            status = status.next();
            seen.push(status);
        }
        assert!(!seen.contains(&AgentTodoStatus::Blocked));
        assert_eq!(
            &seen[..3],
            &[
                AgentTodoStatus::InProgress,
                AgentTodoStatus::Done,
                AgentTodoStatus::Pending
            ]
        );
    }

    #[test]
    fn a_blocked_todo_can_be_moved_back_into_the_cycle() {
        assert_eq!(AgentTodoStatus::Blocked.next(), AgentTodoStatus::Pending);
    }

    #[test]
    fn runtime_status_strings_round_trip() {
        for status in [
            AgentTodoStatus::Pending,
            AgentTodoStatus::InProgress,
            AgentTodoStatus::Done,
            AgentTodoStatus::Blocked,
        ] {
            assert_eq!(AgentTodoStatus::from_runtime(status.as_runtime()), status);
        }
    }
}
