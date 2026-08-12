//! Tool catalog, tool invocations, and shell lifecycle projections.
//!
//! GCABB does not implement agent tools. The Copilot CLI runtime owns them.
//! This module projects the runtime's `tool.*` event stream into app-owned
//! state the native UI can render.
//!
//! The shell projection is deliberately keyed by the runtime's `shellId`
//! rather than by `toolCallId`. Copilot CLI models background execution as a
//! family of four tools (`bash`, `read_bash`, `stop_bash`, `list_bash`) that
//! share a `shellId` handle, so a terminal outlives the individual tool call
//! that created it. A `read_bash` call against an existing shell must append
//! to the terminal the UI is already showing.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStreamKind {
    Invocation,
    Terminal,
}

impl OutputStreamKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invocation => "invocation",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputMetadata {
    #[serde(default)]
    pub chunk_count: u64,
    #[serde(default)]
    pub byte_count: u64,
    #[serde(default)]
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputStreamUpdate {
    pub kind: OutputStreamKind,
    pub identity: String,
    pub chunk: Option<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PartialOutputDelivery {
    call_id: String,
    timestamp: String,
    byte_count: u64,
    content_hash: u64,
}

/// Where a tool came from, so the UI can group and label it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSource {
    Builtin,
    Mcp { server: String },
    Custom,
}

impl ToolSource {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Builtin => "built-in".to_owned(),
            Self::Mcp { server } => format!("mcp:{server}"),
            Self::Custom => "custom".to_owned(),
        }
    }
}

/// Broad behavioural class of a tool, derived from its name.
///
/// The class drives UI affordances: file classes get a diff or file header,
/// shell classes get a terminal, search classes get a match list.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolClass {
    FileRead,
    FileWrite,
    /// Combined read-and-write file tool.
    ///
    /// The runtime exposes file editing to the model as a single
    /// `str_replace_editor` tool, even though the CLI presents the same
    /// capability to users as separate `view`, `create`, and `edit` tools.
    /// This class counts as evidence for both reading and writing.
    FileEditor,
    Search,
    Shell,
    ShellControl,
    Web,
    Delegation,
    Data,
    Skill,
    Interaction,
    Other,
}

impl ToolClass {
    /// Classify a tool by its runtime name.
    ///
    /// Both the model-facing names returned by `tools.list` and the
    /// user-facing CLI aliases are recognized, because the two surfaces do not
    /// use identical names. Unknown names fall back to [`ToolClass::Other`] so
    /// a runtime upgrade that adds tools degrades to a generic renderer
    /// instead of being dropped.
    #[must_use]
    pub fn classify(tool_name: &str) -> Self {
        match tool_name {
            "str_replace_editor" => Self::FileEditor,
            "view" => Self::FileRead,
            // `apply_patch` is how this runtime actually edits files; omitting
            // it made the app report that it could not write files at all.
            "create" | "edit" | "write" | "apply_patch" | "multi_edit" => Self::FileWrite,
            "grep" | "glob" | "rg" | "ripgrep" => Self::Search,
            "bash" | "powershell" | "local_shell" => Self::Shell,
            "read_bash" | "stop_bash" | "list_bash" => Self::ShellControl,
            "web_fetch" | "web_search" | "fetch_copilot_cli_documentation" => Self::Web,
            "task" | "read_agent" | "write_agent" | "list_agents" => Self::Delegation,
            "sql" | "session_store_sql" => Self::Data,
            "skill" => Self::Skill,
            "ask_user" | "task_complete" | "exit_plan_mode" => Self::Interaction,
            _ => Self::Other,
        }
    }

    /// Whether this class can mutate the session worktree.
    #[must_use]
    pub const fn mutates_worktree(self) -> bool {
        matches!(self, Self::FileWrite | Self::FileEditor | Self::Shell)
    }

    /// Whether this class evidences the ability to read files.
    #[must_use]
    pub const fn reads_files(self) -> bool {
        matches!(self, Self::FileRead | Self::FileEditor)
    }

    /// Whether this class evidences the ability to write files.
    #[must_use]
    pub const fn writes_files(self) -> bool {
        matches!(self, Self::FileWrite | Self::FileEditor)
    }
}

#[cfg(test)]
mod classification_tests {
    use super::ToolClass;

    /// Observed live: this runtime edits files with `apply_patch` and searches
    /// with `rg`. Classifying them as `Other` made the app report that it
    /// could not write files while it was writing files.
    #[test]
    fn the_tools_this_runtime_actually_ships_are_classified() {
        assert_eq!(ToolClass::classify("apply_patch"), ToolClass::FileWrite);
        assert!(ToolClass::classify("apply_patch").writes_files());
        assert_eq!(ToolClass::classify("rg"), ToolClass::Search);
        assert_eq!(ToolClass::classify("grep"), ToolClass::Search);
    }
}

/// One tool advertised by the runtime via `tools.list`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub namespaced_name: Option<String>,
    pub description: String,
    pub source: ToolSource,
    pub class: ToolClass,
}

/// Result of runtime tool discovery.
///
/// Discovery is a first-class projection rather than a hardcoded list: the
/// plan requires proving inherited capabilities through the SDK rather than
/// assuming parity with GitHub Copilot App.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCatalog {
    #[serde(default)]
    pub tools: Vec<ToolDescriptor>,
    #[serde(default)]
    pub discovered_at: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl ToolCatalog {
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.name == name)
    }

    #[must_use]
    pub fn by_class(&self, class: ToolClass) -> Vec<&ToolDescriptor> {
        self.tools
            .iter()
            .filter(|tool| tool.class == class)
            .collect()
    }

    #[must_use]
    pub fn is_discovered(&self) -> bool {
        self.discovered_at.is_some() && self.error.is_none()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// A single tool call, projected from `tool.execution_*` events.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolInvocation {
    pub call_id: String,
    /// Event sequence the call started at, used to order it against messages.
    #[serde(default)]
    pub sequence: u64,
    pub tool_name: String,
    pub class: ToolClass,
    pub source: ToolSource,
    pub state: InvocationState,
    /// Set when the tool ran inside a subagent, so the UI can nest it.
    pub agent_id: Option<String>,
    pub arguments: Value,
    /// Shell-aware display command, present only for shell tools.
    pub display_command: Option<String>,
    /// Paths the runtime believes the call may touch, used to refresh changes.
    #[serde(default)]
    pub possible_paths: Vec<String>,
    /// Output loaded from the append-only output store for display.
    #[serde(skip)]
    pub output: String,
    /// First persisted chunk represented by `output`.
    #[serde(skip)]
    pub output_start_chunk: u64,
    #[serde(default)]
    pub output_metadata: OutputMetadata,
    #[serde(default)]
    pub output_error: Option<String>,
    #[serde(skip)]
    pub output_load_error: Option<String>,
    /// Last partial-result delivery projected for this invocation.
    ///
    /// Persisting a compact fingerprint keeps deduplication effective across
    /// snapshot restore without copying an arbitrarily large chunk into the
    /// snapshot itself.
    #[serde(default)]
    last_partial_delivery: Option<PartialOutputDelivery>,
    /// Full detailed result retained for UI display, notably edit diffs.
    #[serde(skip)]
    pub detailed_output: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// Shell handle, when the call created or targeted a shell.
    pub shell_id: Option<String>,
    pub exit_code: Option<i64>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

impl ToolInvocation {
    /// The unified diff produced by a file-writing tool, when present.
    ///
    /// File-editing tools return a rendered diff in `detailedContent`; the
    /// changes view and transcript both render it.
    #[must_use]
    pub fn diff(&self) -> Option<&str> {
        if !self.class.writes_files() {
            return None;
        }
        self.detailed_output
            .as_deref()
            .filter(|content| content.contains("@@") || content.contains("+++"))
    }

    /// One-line description of what this call is doing.
    ///
    /// Falls back to the tool name so an unrecognized tool still reads
    /// sensibly rather than showing nothing.
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(command) = &self.display_command {
            return command.clone();
        }
        let argument = self
            .file_path()
            .map(str::to_owned)
            .or_else(|| self.string_argument("pattern"))
            .or_else(|| self.string_argument("query"))
            .or_else(|| self.string_argument("command"))
            .or_else(|| self.string_argument("url"))
            .or_else(|| self.string_argument("description"))
            .or_else(|| self.string_argument("shellId"));
        argument.unwrap_or_else(|| self.tool_name.clone())
    }

    /// First line of the summary, for a single-line header.
    ///
    /// Commands are frequently multi-line scripts; putting the whole thing in
    /// the header made one tool call fill the window.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let summary = self.summary();
        let first = summary.lines().next().unwrap_or_default().trim().to_owned();
        let truncated: String = first.chars().take(120).collect();
        if truncated.len() < first.len() {
            format!("{truncated}…")
        } else if summary.lines().count() > 1 {
            format!("{truncated} …")
        } else {
            truncated
        }
    }

    /// The full summary when it spans more than one line, so the detail can be
    /// shown in a scrollable block rather than the header.
    #[must_use]
    pub fn multiline_summary(&self) -> Option<String> {
        let summary = self.summary();
        (summary.lines().count() > 1).then_some(summary)
    }

    /// A string argument by name, when present and non-empty.
    #[must_use]
    pub fn string_argument(&self, name: &str) -> Option<String> {
        self.arguments
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    /// Short verb for the tool class, used as a label in the timeline.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        match self.class {
            ToolClass::FileRead => "Read",
            ToolClass::FileWrite | ToolClass::FileEditor => "Edit",
            ToolClass::Search => "Search",
            ToolClass::Shell => "Run",
            ToolClass::ShellControl => "Shell",
            ToolClass::Web => "Fetch",
            ToolClass::Delegation => "Task",
            ToolClass::Data => "Query",
            ToolClass::Skill => "Skill",
            ToolClass::Interaction => "Ask",
            ToolClass::Other => "Tool",
        }
    }

    /// Path argument for file-oriented tools, used for UI headers.
    #[must_use]
    pub fn file_path(&self) -> Option<&str> {
        self.arguments.get("path").and_then(Value::as_str)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Running,
    Exited,
    Cancelled,
}

/// A shell tracked by its runtime-assigned `shellId`.
///
/// Lifetime is independent of any single tool call: `bash` creates it,
/// `read_bash` appends to it, `stop_bash` cancels it, and `list_bash`
/// enumerates it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TerminalSession {
    pub shell_id: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub state: TerminalState,
    #[serde(skip)]
    pub output: String,
    /// First persisted chunk represented by `output`.
    #[serde(skip)]
    pub output_start_chunk: u64,
    #[serde(default)]
    pub output_metadata: OutputMetadata,
    #[serde(default)]
    pub output_error: Option<String>,
    #[serde(skip)]
    pub output_load_error: Option<String>,
    /// Last partial-result delivery mirrored into this terminal.
    #[serde(default)]
    last_partial_delivery: Option<PartialOutputDelivery>,
    pub exit_code: Option<i64>,
    /// Every tool call that has contributed to this shell, in order.
    #[serde(default)]
    pub tool_call_ids: Vec<String>,
    pub started_at: String,
    pub updated_at: String,
}

impl TerminalSession {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self.state, TerminalState::Running)
    }
}

/// App-owned projection of all tool and shell activity for one session.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ToolActivity {
    #[serde(default)]
    pub invocations: Vec<ToolInvocation>,
    #[serde(default)]
    pub terminals: Vec<TerminalSession>,
    /// Subagent id to the tool call that spawned it, so delegated work can be
    /// shown beneath the task that requested it.
    #[serde(default)]
    pub agent_parents: HashMap<String, String>,
    #[serde(skip)]
    invocation_index: HashMap<String, usize>,
    #[serde(skip)]
    terminal_index: HashMap<String, usize>,
}

impl ToolActivity {
    /// Rebuild lookup indexes after deserialization.
    pub fn restore_indexes(&mut self) {
        self.invocation_index = self
            .invocations
            .iter()
            .enumerate()
            .map(|(index, invocation)| (invocation.call_id.clone(), index))
            .collect();
        self.terminal_index = self
            .terminals
            .iter()
            .enumerate()
            .map(|(index, terminal)| (terminal.shell_id.clone(), index))
            .collect();
    }

    #[must_use]
    pub fn invocation(&self, call_id: &str) -> Option<&ToolInvocation> {
        self.invocation_index
            .get(call_id)
            .and_then(|index| self.invocations.get(*index))
    }

    #[must_use]
    pub fn terminal(&self, shell_id: &str) -> Option<&TerminalSession> {
        self.terminal_index
            .get(shell_id)
            .and_then(|index| self.terminals.get(*index))
    }

    /// Invocations that belong to the root agent.
    #[must_use]
    pub fn root_invocations(&self) -> Vec<&ToolInvocation> {
        self.invocations
            .iter()
            .filter(|invocation| invocation.agent_id.is_none())
            .collect()
    }

    /// Invocations a subagent made on behalf of `call_id`.
    #[must_use]
    pub fn children_of(&self, call_id: &str) -> Vec<&ToolInvocation> {
        self.invocations
            .iter()
            .filter(|invocation| {
                invocation
                    .agent_id
                    .as_deref()
                    .and_then(|agent| self.agent_parents.get(agent))
                    .is_some_and(|parent| parent == call_id)
            })
            .collect()
    }

    #[must_use]
    pub fn active_terminals(&self) -> Vec<&TerminalSession> {
        self.terminals
            .iter()
            .filter(|terminal| terminal.is_active())
            .collect()
    }

    #[must_use]
    pub fn running_invocations(&self) -> Vec<&ToolInvocation> {
        self.invocations
            .iter()
            .filter(|invocation| invocation.state == InvocationState::Running)
            .collect()
    }

    /// Failed invocations, newest last. Drives the actionable-failure surface.
    #[must_use]
    pub fn failures(&self) -> Vec<&ToolInvocation> {
        self.invocations
            .iter()
            .filter(|invocation| invocation.state == InvocationState::Failed)
            .collect()
    }

    /// Paths that mutating tools may have touched since `since`.
    ///
    /// Used to decide when the changes view needs refreshing without polling
    /// the filesystem.
    #[must_use]
    pub fn mutated_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        for invocation in &self.invocations {
            if !invocation.class.mutates_worktree() {
                continue;
            }
            if let Some(path) = invocation.file_path() {
                paths.push(path.to_owned());
            }
            paths.extend(invocation.possible_paths.iter().cloned());
        }
        paths.sort_unstable();
        paths.dedup();
        paths
    }

    fn upsert_invocation(&mut self, invocation: ToolInvocation) {
        if let Some(index) = self.invocation_index.get(&invocation.call_id) {
            self.invocations[*index] = invocation;
        } else {
            self.invocation_index
                .insert(invocation.call_id.clone(), self.invocations.len());
            self.invocations.push(invocation);
        }
    }

    fn invocation_mut(&mut self, call_id: &str) -> Option<&mut ToolInvocation> {
        let index = *self.invocation_index.get(call_id)?;
        self.invocations.get_mut(index)
    }

    fn terminal_mut(&mut self, shell_id: &str) -> Option<&mut TerminalSession> {
        let index = *self.terminal_index.get(shell_id)?;
        self.terminals.get_mut(index)
    }

    fn ensure_terminal(&mut self, shell_id: &str, timestamp: &str) -> &mut TerminalSession {
        if !self.terminal_index.contains_key(shell_id) {
            self.terminal_index
                .insert(shell_id.to_owned(), self.terminals.len());
            self.terminals.push(TerminalSession {
                shell_id: shell_id.to_owned(),
                command: None,
                cwd: None,
                state: TerminalState::Running,
                output: String::new(),
                output_start_chunk: 0,
                output_metadata: OutputMetadata::default(),
                output_error: None,
                output_load_error: None,
                last_partial_delivery: None,
                exit_code: None,
                tool_call_ids: Vec::new(),
                started_at: timestamp.to_owned(),
                updated_at: timestamp.to_owned(),
            });
        }
        let index = self.terminal_index[shell_id];
        &mut self.terminals[index]
    }
}

fn append_output(target: &mut String, metadata: &mut OutputMetadata, chunk: &str) {
    target.push_str(chunk);
    metadata.chunk_count += 1;
    metadata.byte_count += chunk.len() as u64;
}

fn partial_output_delivery(event: &crate::DomainEvent) -> Option<PartialOutputDelivery> {
    let call_id = event.details.get("toolCallId")?.as_str()?;
    let chunk = event.details.get("partialOutput")?.as_str()?;
    if event.timestamp.is_empty() || chunk.is_empty() {
        return None;
    }

    // FNV-1a is stable across processes and app versions, unlike
    // `DefaultHasher`. Timestamp and byte count further constrain matches.
    let mut content_hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in chunk.bytes() {
        content_hash ^= u64::from(byte);
        content_hash = content_hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(PartialOutputDelivery {
        call_id: call_id.to_owned(),
        timestamp: event.timestamp.clone(),
        byte_count: chunk.len() as u64,
        content_hash,
    })
}

fn is_redelivered_partial(activity: &ToolActivity, event: &crate::DomainEvent) -> bool {
    let Some(delivery) = partial_output_delivery(event) else {
        return false;
    };
    activity
        .invocation(&delivery.call_id)
        .is_some_and(|invocation| invocation.last_partial_delivery.as_ref() == Some(&delivery))
}

/// Project a single SDK event into tool and terminal state.
///
/// Unrecognized tool events are ignored rather than dropped from the activity
/// log; the raw event remains in `SessionSnapshot::activities`.
pub fn project(activity: &mut ToolActivity, event: &crate::DomainEvent) {
    match event.source_type.as_str() {
        "tool.execution_start" => project_start(activity, event),
        "tool.execution_partial_result" => project_partial(activity, event),
        "tool.execution_complete" => project_complete(activity, event),
        "subagent.started" => project_subagent(activity, event),
        _ => {}
    }
}

/// Record which tool call spawned a subagent.
///
/// Subagent tool calls carry only an `agentId`; this mapping is what lets the
/// UI show them beneath the task that requested them instead of as orphans.
fn project_subagent(activity: &mut ToolActivity, event: &crate::DomainEvent) {
    let data = &event.details;
    let agent_id = data
        .get("agentId")
        .and_then(Value::as_str)
        .or(event.agent_id.as_deref());
    let parent = data
        .get("parentToolCallId")
        .or_else(|| data.get("toolCallId"))
        .and_then(Value::as_str);
    if let (Some(agent_id), Some(parent)) = (agent_id, parent) {
        activity
            .agent_parents
            .insert(agent_id.to_owned(), parent.to_owned());
    }
}

fn project_start(activity: &mut ToolActivity, event: &crate::DomainEvent) {
    let data = &event.details;
    let Some(call_id) = data.get("toolCallId").and_then(Value::as_str) else {
        return;
    };
    let tool_name = data
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let source =
        data.get("mcpServerName")
            .and_then(Value::as_str)
            .map_or(ToolSource::Builtin, |server| ToolSource::Mcp {
                server: server.to_owned(),
            });
    let shell_info = data.get("shellToolInfo");
    let display_command = shell_info
        .and_then(|info| info.get("displayCommand"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let possible_paths = shell_info
        .and_then(|info| info.get("possiblePaths"))
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let arguments = data.get("arguments").cloned().unwrap_or(Value::Null);

    // `read_bash`/`stop_bash` name the shell they target in their arguments,
    // so the invocation is bound to an existing terminal immediately.
    let shell_id = arguments
        .get("shellId")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let class = ToolClass::classify(&tool_name);
    activity.upsert_invocation(ToolInvocation {
        call_id: call_id.to_owned(),
        sequence: event.sequence,
        tool_name,
        class,
        source,
        state: InvocationState::Running,
        agent_id: event.agent_id.clone(),
        arguments,
        display_command: display_command.clone(),
        possible_paths,
        output: String::new(),
        output_start_chunk: 0,
        output_metadata: OutputMetadata::default(),
        output_error: None,
        output_load_error: None,
        last_partial_delivery: None,
        detailed_output: None,
        error_code: None,
        error_message: None,
        shell_id: shell_id.clone(),
        exit_code: None,
        started_at: event.timestamp.clone(),
        completed_at: None,
    });

    if let Some(shell_id) = shell_id {
        let call_id = call_id.to_owned();
        let terminal = activity.ensure_terminal(&shell_id, &event.timestamp);
        terminal.updated_at.clone_from(&event.timestamp);
        if !terminal.tool_call_ids.contains(&call_id) {
            terminal.tool_call_ids.push(call_id);
        }
        if terminal.command.is_none() {
            terminal.command = display_command;
        }
    }
}

fn project_partial(activity: &mut ToolActivity, event: &crate::DomainEvent) {
    let data = &event.details;
    let Some(call_id) = data.get("toolCallId").and_then(Value::as_str) else {
        return;
    };
    let chunk = data
        .get("partialOutput")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if chunk.is_empty() {
        return;
    }
    let delivery = partial_output_delivery(event);

    let mut shell_id = None;
    if let Some(invocation) = activity.invocation_mut(call_id) {
        if invocation.last_partial_delivery.as_ref() != delivery.as_ref() || delivery.is_none() {
            append_output(
                &mut invocation.output,
                &mut invocation.output_metadata,
                chunk,
            );
            invocation.last_partial_delivery.clone_from(&delivery);
        }
        shell_id.clone_from(&invocation.shell_id);
    }

    // Mirror streaming output into the terminal so a shell that spans several
    // tool calls presents one continuous transcript.
    if let Some(shell_id) = shell_id
        && let Some(terminal) = activity.terminal_mut(&shell_id)
    {
        if terminal.last_partial_delivery.as_ref() != delivery.as_ref() || delivery.is_none() {
            append_output(&mut terminal.output, &mut terminal.output_metadata, chunk);
            terminal.last_partial_delivery = delivery;
        }
        terminal.updated_at.clone_from(&event.timestamp);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "completion projects one runtime result atomically across invocation and terminal state"
)]
fn project_complete(activity: &mut ToolActivity, event: &crate::DomainEvent) {
    let data = &event.details;
    let Some(call_id) = data.get("toolCallId").and_then(Value::as_str) else {
        return;
    };
    let success = data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let error = data.get("error");
    let result = data.get("result");
    let detailed = result
        .and_then(|result| result.get("detailedContent"))
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .and_then(|result| result.get("content"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);

    let shell_exit = result
        .and_then(|result| result.get("contents"))
        .and_then(Value::as_array)
        .and_then(|contents| {
            contents
                .iter()
                .find(|content| content.get("type").and_then(Value::as_str) == Some("shell_exit"))
        })
        .cloned();

    let mut terminal_update = None;
    if let Some(invocation) = activity.invocation_mut(call_id) {
        invocation.state = if success {
            InvocationState::Succeeded
        } else {
            InvocationState::Failed
        };
        invocation.completed_at = Some(event.timestamp.clone());
        if invocation.output.is_empty()
            && let Some(detailed) = detailed
        {
            append_output(
                &mut invocation.output,
                &mut invocation.output_metadata,
                &detailed,
            );
            invocation.detailed_output = Some(detailed);
        }
        invocation.output_metadata.complete = true;
        invocation.error_code = error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        invocation.error_message = error
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        if let Some(exit) = &shell_exit {
            let shell_id = exit
                .get("shellId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let exit_code = exit.get("exitCode").and_then(Value::as_i64);
            invocation.exit_code = exit_code;
            if invocation.shell_id.is_none() {
                invocation.shell_id.clone_from(&shell_id);
            }
            if let Some(shell_id) = shell_id {
                terminal_update = Some((
                    shell_id,
                    exit_code,
                    exit.get("cwd").and_then(Value::as_str).map(str::to_owned),
                    exit.get("outputPreview")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    exit.get("outputTruncated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    invocation.output.clone(),
                ));
            }
        }
    }

    if let Some((shell_id, exit_code, cwd, preview, preview_truncated, invocation_output)) =
        terminal_update
    {
        let call_id = call_id.to_owned();
        let timestamp = event.timestamp.clone();
        let terminal = activity.ensure_terminal(&shell_id, &timestamp);
        terminal.updated_at = timestamp;
        terminal.exit_code = exit_code;
        // A shell reports exit only once; later `read_bash` calls against the
        // same id must not resurrect it.
        terminal.state = if exit_code.is_some() {
            TerminalState::Exited
        } else {
            terminal.state
        };
        if terminal.cwd.is_none() {
            terminal.cwd = cwd;
        }
        if !terminal.tool_call_ids.contains(&call_id) {
            terminal.tool_call_ids.push(call_id);
        }
        // Only fall back to the preview when nothing streamed, otherwise the
        // preview would duplicate output already shown.
        if terminal.output.is_empty() {
            if !invocation_output.is_empty() {
                append_output(
                    &mut terminal.output,
                    &mut terminal.output_metadata,
                    &invocation_output,
                );
            } else if let Some(preview) = preview {
                append_output(
                    &mut terminal.output,
                    &mut terminal.output_metadata,
                    &preview,
                );
                if preview_truncated {
                    terminal.output_error =
                        Some("Runtime supplied only a truncated output preview.".to_owned());
                }
            }
        }
        terminal.output_metadata.complete = exit_code.is_some();
    }
}

/// Output mutations produced by an event before it is projected.
///
/// Storage calls this first so the event and its output chunks are committed in
/// one transaction. Projection then applies the same mutations to in-memory
/// display state.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "all output mutations for one event must be derived together before transactional persistence"
)]
pub fn output_updates(
    activity: &ToolActivity,
    event: &crate::DomainEvent,
) -> Vec<OutputStreamUpdate> {
    let data = &event.details;
    let Some(call_id) = data.get("toolCallId").and_then(Value::as_str) else {
        return Vec::new();
    };
    let invocation = activity.invocation(call_id);
    match event.source_type.as_str() {
        "tool.execution_start" => {
            let mut updates = vec![OutputStreamUpdate {
                kind: OutputStreamKind::Invocation,
                identity: call_id.to_owned(),
                chunk: None,
                complete: false,
            }];
            if let Some(shell_id) = data
                .get("arguments")
                .and_then(|arguments| arguments.get("shellId"))
                .and_then(Value::as_str)
            {
                updates.push(OutputStreamUpdate {
                    kind: OutputStreamKind::Terminal,
                    identity: shell_id.to_owned(),
                    chunk: None,
                    complete: false,
                });
            }
            updates
        }
        "tool.execution_partial_result" => {
            if is_redelivered_partial(activity, event) {
                return Vec::new();
            }
            let Some(chunk) = data
                .get("partialOutput")
                .and_then(Value::as_str)
                .filter(|chunk| !chunk.is_empty())
            else {
                return Vec::new();
            };
            let mut updates = vec![OutputStreamUpdate {
                kind: OutputStreamKind::Invocation,
                identity: call_id.to_owned(),
                chunk: Some(chunk.to_owned()),
                complete: false,
            }];
            if let Some(shell_id) = invocation.and_then(|invocation| invocation.shell_id.as_deref())
            {
                updates.push(OutputStreamUpdate {
                    kind: OutputStreamKind::Terminal,
                    identity: shell_id.to_owned(),
                    chunk: Some(chunk.to_owned()),
                    complete: false,
                });
            }
            updates
        }
        "tool.execution_complete" => {
            let result = data.get("result");
            let detailed = result
                .and_then(|result| result.get("detailedContent"))
                .and_then(Value::as_str)
                .or_else(|| {
                    result
                        .and_then(|result| result.get("content"))
                        .and_then(Value::as_str)
                });
            let invocation_chunk = invocation
                .filter(|invocation| invocation.output_metadata.byte_count == 0)
                .and(detailed)
                .filter(|chunk| !chunk.is_empty())
                .map(str::to_owned);
            let terminal_fallback = invocation_chunk.clone().or_else(|| {
                invocation
                    .map(|invocation| invocation.output.clone())
                    .filter(|output| !output.is_empty())
            });
            let mut updates = vec![OutputStreamUpdate {
                kind: OutputStreamKind::Invocation,
                identity: call_id.to_owned(),
                chunk: invocation_chunk,
                complete: true,
            }];
            let shell_exit = result
                .and_then(|result| result.get("contents"))
                .and_then(Value::as_array)
                .and_then(|contents| {
                    contents.iter().find(|content| {
                        content.get("type").and_then(Value::as_str) == Some("shell_exit")
                    })
                });
            let shell_id = shell_exit
                .and_then(|exit| exit.get("shellId"))
                .and_then(Value::as_str)
                .or_else(|| invocation.and_then(|invocation| invocation.shell_id.as_deref()));
            if let Some(shell_id) = shell_id {
                let terminal = activity.terminal(shell_id);
                let preview = shell_exit
                    .and_then(|exit| exit.get("outputPreview"))
                    .and_then(Value::as_str);
                let terminal_chunk = terminal
                    .is_none_or(|terminal| terminal.output_metadata.byte_count == 0)
                    .then(|| terminal_fallback.or_else(|| preview.map(str::to_owned)))
                    .flatten()
                    .filter(|chunk| !chunk.is_empty());
                updates.push(OutputStreamUpdate {
                    kind: OutputStreamKind::Terminal,
                    identity: shell_id.to_owned(),
                    chunk: terminal_chunk,
                    complete: shell_exit.and_then(|exit| exit.get("exitCode")).is_some(),
                });
            }
            updates
        }
        _ => Vec::new(),
    }
}

impl ToolActivity {
    pub fn set_output(
        &mut self,
        kind: OutputStreamKind,
        identity: &str,
        output: std::result::Result<(String, OutputMetadata, u64), String>,
    ) {
        match kind {
            OutputStreamKind::Invocation => {
                if let Some(invocation) = self.invocation_mut(identity) {
                    match output {
                        Ok((output, metadata, start_chunk)) => {
                            invocation.output = output;
                            invocation.output_start_chunk = start_chunk;
                            invocation.output_metadata = metadata;
                            invocation.output_load_error = None;
                            if !matches!(
                                invocation.class,
                                ToolClass::Shell | ToolClass::ShellControl
                            ) {
                                invocation.detailed_output = Some(invocation.output.clone());
                            }
                        }
                        Err(error) => {
                            invocation.output.clear();
                            invocation.detailed_output = None;
                            invocation.output_load_error = Some(error);
                        }
                    }
                }
            }
            OutputStreamKind::Terminal => {
                if let Some(terminal) = self.terminal_mut(identity) {
                    match output {
                        Ok((output, metadata, start_chunk)) => {
                            terminal.output = output;
                            terminal.output_start_chunk = start_chunk;
                            terminal.output_metadata = metadata;
                            terminal.output_load_error = None;
                        }
                        Err(error) => {
                            terminal.output.clear();
                            terminal.output_load_error = Some(error);
                        }
                    }
                }
            }
        }
    }

    pub fn prepend_output(
        &mut self,
        kind: OutputStreamKind,
        identity: &str,
        start_chunk: u64,
        before_chunk: u64,
        content: &str,
    ) -> bool {
        let (output, current_start) = match kind {
            OutputStreamKind::Invocation => {
                let Some(invocation) = self.invocation_mut(identity) else {
                    return false;
                };
                (&mut invocation.output, &mut invocation.output_start_chunk)
            }
            OutputStreamKind::Terminal => {
                let Some(terminal) = self.terminal_mut(identity) else {
                    return false;
                };
                (&mut terminal.output, &mut terminal.output_start_chunk)
            }
        };
        if *current_start != before_chunk || start_chunk >= before_chunk {
            return false;
        }
        output.insert_str(0, content);
        *current_start = start_chunk;
        true
    }
}

/// Mark a terminal cancelled in response to a user-driven stop.
pub fn mark_terminal_cancelled(activity: &mut ToolActivity, shell_id: &str, timestamp: &str) {
    if let Some(terminal) = activity.terminal_mut(shell_id) {
        terminal.state = TerminalState::Cancelled;
        timestamp.clone_into(&mut terminal.updated_at);
    }
}

/// Mark every still-running terminal cancelled.
///
/// Cancelling a turn tears down the shells that turn started, but the runtime
/// sends no completion event for them. Without this a background shell shows
/// as running for the rest of the session, which reads as work still in
/// flight when nothing is running at all.
pub fn mark_running_terminals_cancelled(activity: &mut ToolActivity, timestamp: &str) {
    let running: Vec<String> = activity
        .terminals
        .iter()
        .filter(|terminal| terminal.is_active())
        .map(|terminal| terminal.shell_id.clone())
        .collect();
    for shell_id in running {
        mark_terminal_cancelled(activity, &shell_id, timestamp);
    }
}

/// Mark every tool call left running by a stopped runtime as cancelled.
pub fn mark_running_invocations_cancelled(activity: &mut ToolActivity, timestamp: &str) {
    for invocation in &mut activity.invocations {
        if invocation.state == InvocationState::Running {
            invocation.state = InvocationState::Cancelled;
            invocation.completed_at = Some(timestamp.to_owned());
        }
    }
}
