# Native Rust Copilot Sessions App

## Decision

Build a new Rust-native desktop application around the official
`github-copilot-sdk` crate. Use Zed as an architectural reference, not as a
codebase to fork.

Do not put AHP or ACP on the critical path:

- The Copilot SDK is the only initial agent backend.
- ACP may become a second provider later.
- AHP is deferred unless detached, remote, or simultaneous multi-client
  sessions become a concrete requirement.

## Product Goal

Create a faster, more transparent alternative to GitHub Copilot App while
preserving its session-oriented workflow:

1. Create and resume isolated coding sessions.
2. Show exactly what the main agent and subagents are doing.
3. Display running commands in live native terminals.
4. Compare session changes against any selected base branch or commit.
5. Keep latency, process activity, tools, and context usage observable.

The application is a session manager, not a general-purpose editor.

## Current GitHub Copilot App Baseline

Local inspection of GitHub Copilot App 1.1.0 on Windows establishes:

```text
Tauri 2.11.5 Rust host (`github.exe`)
  ├─ Edge WebView2 frontend
  ├─ bundled `copilot-sdk` JavaScript distribution
  └─ one or more child processes:
       copilot.exe --server --stdio --no-auto-update
            └─ SDK protocol v3, Content-Length-framed JSON-RPC
```

The child executables are resolved from the SDK-managed cache, for example:
`%LOCALAPPDATA%\github-copilot-sdk\cli\1.0.73\copilot.exe`.

This is the Copilot SDK server protocol, not the CLI's separate public ACP mode
(`copilot --acp`). The exact App-side implementation is closed source: the
package proves that the App ships the JavaScript SDK for extensions and uses the
same server protocol, but does not prove whether its primary Rust control plane
calls that JavaScript SDK or implements the generated protocol directly.

Responsibility boundary:

| App owns | Copilot CLI runtime owns |
| --- | --- |
| Projects, session navigation, worktree placement | Agent loop and model requests |
| WebView UI and Tauri IPC | Tools, MCP, skills, modes, planning |
| Process supervision and SDK callbacks | Subagent and fleet orchestration |
| Permission, elicitation, and input dialogs | Persistent conversation events |
| Changes, PR, issue, and workflow UX | CLI session filesystem and chronicle |
| Deep links and app-local metadata | Usage/model/tool lifecycle events |

CLI session data is shared with normal Copilot CLI at
`~/.copilot/session-state/` and `~/.copilot/session-store.db`. The App subscribes
to SDK events, turns them into UI state, and sends user actions back through
typed SDK RPCs and callback responses.

Our design intentionally retains the successful boundary—native host around the
official SDK and CLI runtime—while replacing Tauri/WebView2 with GPUI, exposing
more event detail, adding live native terminals, and making the changes base
selectable.

## Architecture

```text
┌──────────────────────── Native GPUI Application ────────────────────────┐
│                                                                         │
│  GPUI views                                                             │
│    ├─ session list                                                      │
│    ├─ conversation and activity timeline                                │
│    ├─ agent/subagent tree                                               │
│    ├─ native terminal                                                   │
│    └─ selectable-base changes view                                      │
│             │ immutable snapshots and compact deltas                    │
│             ▼                                                           │
│  SessionManager                                                         │
│    ├─ SessionActor per active session                                   │
│    ├─ normalized DomainEvent log                                        │
│    └─ app-owned SessionState projection                                 │
│             │                                                           │
│             ▼                                                           │
│  CopilotProvider                                                        │
│    └─ github-copilot-sdk                                                │
│         └─ JSON-RPC over stdio/TCP                                      │
│              └─ copilot --server --stdio                                │
│                                                                         │
│  Supporting services                                                    │
│    ├─ TerminalService: PTYs, output, input, process trees               │
│    ├─ GitService: refs, merge bases, status, diffs                      │
│    ├─ Storage: SQLite metadata, events, snapshots                       │
│    ├─ FileMonitor: worktree/index changes                               │
│    └─ Diagnostics: tracing, metrics, exportable bundles                 │
└─────────────────────────────────────────────────────────────────────────┘
```

## Technology

| Area | Choice |
| --- | --- |
| Native UI | GPUI with `gpui_platform` |
| Async runtime | Tokio |
| Copilot runtime | `github-copilot-sdk` |
| Persistence | SQLite in WAL mode |
| Terminal process | `portable-pty`, ConPTY on Windows |
| Terminal parser/model | `alacritty_terminal` or compatible VT parser |
| Terminal renderer | Custom virtualized GPUI element |
| Diff renderer | Custom virtualized GPUI split/unified view |
| Text storage | Rope-based storage |
| Highlighting | Tree-sitter |
| Git | Git CLI behind typed Rust argument APIs |
| File monitoring | `notify` with debounce/coalescing |
| Diagnostics | `tracing`, structured logs, optional OpenTelemetry |

GPUI is Apache-2.0, GPU accelerated, and built for editor-scale text surfaces.
Pin a known revision because it remains pre-1.0.

Slint is the fallback if GPUI proves too unstable during the feasibility spike.

## Source Strategy

Do not fork Zed:

- `gpui` is separately reusable under Apache-2.0.
- Zed's `agent`, `acp_thread`, and `agent_ui` crates are GPL-3.0-or-later.
- Those crates depend heavily on Zed's editor, workspace, project, LSP,
  extension, remote, cloud, multi-buffer, and settings systems.
- Removing features would create a large downstream editor fork rather than a
  small sessions application.

Learn from these Zed patterns:

- Foreground GPUI entity mutations plus background executors.
- A provider boundary similar to `AgentConnection`.
- A normalized thread projection between provider events and UI.
- Full terminal output for the user with bounded output for the model.
- Virtualized terminal and diff rendering.
- SQLite thread metadata and snapshot persistence.
- Undo-aware edit tracking and explicit subagent correlation through `agentId`,
  `toolCallId`, and `parentToolCallId`.

Do not copy GPL implementation code unless the project deliberately adopts GPL
after legal review.

## Threading and Responsiveness

GPUI state lives on the platform UI thread. All expensive work stays elsewhere:

- Copilot SDK I/O, Git, SQLite, file watching, and PTY I/O run on Tokio workers.
- Terminal parsing, syntax highlighting, and diff calculation use bounded
  background workers.
- Each active session has one actor that serializes provider events into
  deterministic domain events.
- Workers publish immutable reference-counted snapshots to GPUI.
- Token streams and terminal writes are coalesced to a frame budget.
- Terminal scrollback, timelines, file lists, and diffs render visible rows
  only.
- No filesystem, network, process, database, or parsing operation blocks the UI
  thread.

Performance budgets:

- Warm application interactive in under 500 ms.
- Session open or resume UI in under 1 second, excluding CLI startup.
- Provider event to visible UI p95 under 50 ms.
- Terminal output to visible UI p95 under 100 ms.
- Changes refresh under 500 ms for ordinary repositories.
- Stable frame pacing during token streaming and high-volume terminal output.

## Internal Model

The UI consumes app-owned state rather than raw SDK objects.

```text
AppState
  projects
  sessions

SessionState
  id
  sdk_session_id
  project
  worktree
  title
  mode
  model
  reasoning_effort
  status
  turns
  activity
  agent_tree
  terminals
  changes
  selected_base
  diagnostics

ActivityNode
  id
  parent_id
  kind: model | tool | subagent | terminal | permission | file | system
  state: queued | running | waiting | completed | failed | cancelled
  started_at
  completed_at
  summary
  structured_details
  related_terminal
  related_files
  correlation_ids
  visibility: observed | inferred | unavailable
```

Normalize every SDK event into a versioned `DomainEvent`. Reducers derive the
current state. Retain raw SDK events separately for diagnostics and future
remapping.

The SDK remains authoritative for Copilot conversation/session history. The app
database owns UI metadata, selected base refs, normalized activity, terminal
metadata, and diagnostics.

## Copilot SDK Integration

Use the SDK rather than custom JSON-RPC:

- `Client::start` for CLI resolution, process startup, handshake, and health.
- `create_session` and `resume_session`.
- Streaming session subscriptions.
- Typed permission, elicitation, user-input, plan-exit, and mode-switch handlers.
- Typed RPC namespace for models, modes, workspaces, plans, tasks, agents,
  sessions, and fleet.
- SDK hooks for prompt, tool, session, and error lifecycle events.
- Startup timing APIs for launch and handshake diagnostics.
- Explicit CLI version compatibility checks.

Keep integration behind a narrow `AgentProvider` trait, but implement only the
methods required by Copilot. Avoid designing a universal protocol prematurely.

```text
AgentProvider
  start
  stop
  create_session
  resume_session
  send
  cancel
  events
  models
  modes
  permissions
  subagents
  diagnostics
```

An ACP adapter can implement this trait later if supporting other agents becomes
important.

## Feature 1: Agent and Subagent Visibility

Build a unified activity timeline from:

- Assistant message and reasoning events exposed by the SDK.
- Tool start, progress, result, and failure events.
- `subagent.started`, `subagent.completed`, and `subagent.failed` events, with
  child activity correlated through agent and tool-call identifiers.
- Permission and user-input callbacks.
- Session hooks before and after tool use.
- Fleet, task, and agent RPCs.
- Terminal lifecycle and output.
- File monitor and Git changes.
- App-generated timing spans around every SDK call.

Views:

- **Timeline:** chronological activity with duration and status.
- **Agent tree:** main agent, subagents, parent tool call, elapsed time, and last
  observed activity.
- **Nested activity:** subagent messages and tools appear beneath the task that
  spawned them rather than being interleaved into the main transcript.
- **Inspector:** normalized details plus optional redacted raw SDK payload.
- **Diagnostics:** startup, model, tool, terminal, Git, idle, and UI propagation
  timing.

Never imply visibility that the SDK does not provide. Label information:

- `observed`: directly emitted by the SDK or app-owned service.
- `inferred`: correlated from tool/task/session identifiers.
- `unavailable`: known activity for which details are not exposed.

The feasibility spike must produce an event-coverage matrix for main-agent,
fleet, task-agent, and tool-spawned work.

## Feature 2: Live Native Terminals

The feasibility spike must determine whether the SDK's built-in shell tools
provide sufficient incremental output and process control.

Preferred order:

1. Map SDK shell/tool events to terminals when they expose command, cwd,
   incremental output, status, and cancellation.
2. If insufficient, register a host-owned terminal tool through the SDK and
   exclude the conflicting built-in shell tool for sessions using enhanced
   terminals.

The host-owned terminal service:

- Creates PTYs with command, cwd, environment, session, turn, and tool IDs.
- Streams full output into a bounded scrollback model.
- Returns configurable bounded head/tail output to the agent.
- Supports attach, detach, input, resize, interrupt, terminate, and release.
- Keeps commands alive across view changes.
- Terminates the correct process tree on cancellation.
- Applies backpressure and disk-spill/retention policies for noisy commands.
- Parses ANSI/VT output off-thread and renders visible cells through GPUI.

Phase the UX:

1. Read-only live output and exit status.
2. Interactive input, selection, copy, resize, interrupt, and kill.
3. User-created terminals and reusable terminal sessions.

## Feature 3: Selectable Changes Base

Do not depend on Copilot's `/diff` behavior. Compute changes independently:

- Enumerate local branches, remote-tracking branches, tags, and commits.
- Store the selected base per app session.
- Resolve the base to an immutable object ID.
- Calculate and display the merge base.
- Combine committed branch changes with index and worktree changes.
- Recompute after HEAD, index, worktree, or base changes.
- Preserve the selection without checking out or mutating branches.
- Detect deleted or rewritten refs and require explicit replacement.

Default base:

1. Base recorded when the session/worktree was created.
2. Current branch upstream.
3. Repository default branch.
4. `main`, then `master`.

Display the exact comparison OIDs. Use argument arrays for Git commands and
load large files or hunks on demand.

Native changes UI:

- File tree with committed, staged, and unstaged grouping.
- Split and unified modes.
- Virtualized rows and context expansion.
- Syntax highlighting.
- Rename, binary, submodule, deleted-file, and large-file handling.
- Links from activity nodes to affected files and hunks.

## Persistence and Recovery

SQLite stores:

- Projects and app sessions.
- Copilot SDK session IDs.
- Worktree and selected-base metadata.
- Versioned normalized domain events.
- Periodic state snapshots.
- Terminal metadata and configurable output tails.
- Diagnostics and compatibility information.

Recovery:

- Resume the SDK session when available.
- Rebuild app state from the latest snapshot plus subsequent domain events.
- Reconcile with `session.get_events()` after unclean shutdown.
- Mark interrupted commands and turns honestly; never synthesize success.
- Keep migrations forward-only with tested backups and rollback guidance.

## Workspace Layout

```text
apps/
  desktop/               GPUI application entry point
crates/
  app-model/             domain events, reducers, immutable snapshots
  copilot-provider/      github-copilot-sdk integration
  session-manager/       session actors, lifecycle, recovery
  terminal-service/      PTYs, parser, buffering, process trees
  git-service/           refs, merge bases, status, diffs
  storage/               SQLite, migrations, snapshots
  diagnostics/           tracing, metrics, redaction, exports
  ui-components/         timeline, agent tree, terminal, diff, inspector
  test-harness/          fake provider and deterministic event fixtures
```

## Delivery Plan

### Phase 0: Feasibility and Event-Coverage Spike (1-2 weeks)

- Start the latest compatible CLI through the Rust SDK.
- Measure CLI startup and first-token timing.
- Create, prompt, cancel, disconnect, and resume a session.
- Capture every SDK event and callback for:
  - file inspection and editing
  - a long-running shell command
  - permission and elicitation
  - a fleet/subagent task
  - failure and cancellation
- Exercise typed tasks, fleet, agent, plan, workspace, model, and mode RPCs.
- Determine whether built-in shell events support the required live terminal.
- Prototype GPUI with a streaming timeline, terminal, and diff.
- Produce the event-coverage and SDK/CLI compatibility matrices.

Exit criteria:

- No terminal scraping or interactive-TUI embedding is required.
- Main-agent, tool, and subagent visibility limits are known.
- A host-owned terminal-tool fallback is proven if built-in events are
  insufficient.
- GPUI remains responsive under token and terminal-output load.

### Phase 1: Application Foundation (2 weeks)

- Create the workspace and GPUI shell.
- Implement app state, domain events, reducers, and session actors.
- Integrate Copilot client lifecycle and version checks.
- Add SQLite migrations, snapshots, crash recovery, and structured tracing.
- Build a deterministic fake provider and golden SDK-event fixtures.

Exit criteria:

- State rebuild is deterministic.
- App restart restores session metadata and resumes an SDK session.
- Provider or CLI crashes surface actionable recovery options.

### Phase 2: Session MVP (2 weeks)

- Project and session list.
- Create, resume, close, and cancel.
- Prompt composer, streaming transcript, model, mode, and effort controls.
- Permission, elicitation, and user-input UX.
- Worktree metadata and process-health indicators.

Exit criteria:

- Ordinary coding sessions can be completed without opening Copilot CLI.
- Session switching does not pause unrelated background work.

### Phase 3: Visibility MVP (2 weeks)

- Activity timeline, agent tree, filters, inspector, and duration/status display.
- Correlate SDK, hook, task, fleet, terminal, Git, and file events.
- Add latency breakdowns and redacted diagnostic export.

Exit criteria:

- Every observed activity has explicit lifecycle and timing.
- A stalled session can be localized to model, tool, input, terminal, Git, SDK,
  or UI propagation.

### Phase 4: Terminal MVP (2-3 weeks)

- Integrate built-in output or the host-owned terminal tool.
- Implement PTYs, parsing, bounded scrollback, process control, and persistence.
- Build the native virtualized GPUI terminal.

Exit criteria:

- Long-running commands stream while active.
- High-volume output remains memory-bounded and responsive.
- Cancellation targets the correct process tree.

### Phase 5: Changes View MVP (2-3 weeks)

- Implement repository/ref discovery, base selection, merge-base calculation,
  status monitoring, and diff generation.
- Build native file tree and virtualized split/unified diff surfaces.
- Persist the selected base and link changes to activity.

Exit criteria:

- Changing the base updates the view without changing branches or agent state.
- Results match Git across committed and uncommitted changes.
- Edge cases have defined behavior.

### Phase 6: Hardening and Distribution (2-3 weeks)

- Signed installers and update strategy.
- Windows-first testing, then macOS and Linux.
- Accessibility, keyboard navigation, high-DPI, and theme support.
- Resource limits, redaction, retention, dependency review, and opt-in crash
  reporting.
- Compatibility tests against pinned and supported Copilot CLI versions.
- Startup, latency, terminal, diff, memory, and recovery benchmarks.

## Testing

| Layer | Tests |
| --- | --- |
| Domain model | Reducer determinism, idempotency, ordering, migrations |
| Copilot provider | Golden fixtures for SDK events, callbacks, and failures |
| Sessions | Create, resume, cancel, crash, reconnect, reconciliation |
| Terminal | ANSI fragmentation, UTF-8, resize, input, flood, process trees |
| Git | Fixture repositories for refs, merge bases, renames, binaries, submodules |
| GPUI | Test contexts for interaction, focus, virtualization, accessibility |
| End-to-end | Small opt-in suite against real Copilot SDK and CLI |

Most tests use the fake provider. Real Copilot tests remain small to avoid cost,
nondeterminism, and account requirements.

## Principal Risks

| Risk | Mitigation |
| --- | --- |
| SDK lacks live built-in shell output | Prove host-owned SDK tool in Phase 0 |
| Incomplete subagent telemetry | Coverage matrix and honest visibility labels |
| GPUI pre-1.0 churn | Pin revision and isolate framework-specific widgets |
| Native terminal complexity | Reuse PTY/parser crates; phase interactivity |
| Native diff complexity | Virtualize from the start; load hunks on demand |
| SDK/CLI version skew | Compatibility matrix, startup check, pinned support range |
| Git comparison surprises | Display resolved base and merge-base OIDs |
| Session event duplication | Versioned IDs, deterministic reducers, reconciliation |
| Diagnostic data leaks | Structured redaction and explicit export preview |

## Deferred Extensions

- ACP provider for Claude, Gemini, Codex, or other registry agents.
- AHP server for remote, mobile, or simultaneous multi-client access.
- Cloud-hosted sessions.
- Embedded editor or full IDE features.

These remain outside the MVP and must not distort the initial domain model.

## Estimate

The Windows-first implementation is approximately **11-15 engineer-weeks** for
one experienced Rust desktop engineer. Two engineers could target roughly
**7-10 calendar weeks** after the feasibility spike. A polished cross-platform
release may require another 4-6 engineer-weeks.

The largest uncertainty is whether SDK shell events are sufficient for a live
terminal. The second is the amount of native terminal and diff widget work.

## UX Inputs Needed

Screenshots or short recordings of these Copilot App surfaces will support the
interaction specification:

- Project/session list and session creation.
- Active conversation and tool cards.
- Agent/subagent status.
- Changes view and file diff navigation.
- Permission and user-input dialogs.
- Session switching while work continues.
- Error, cancellation, and recovery states.

## References

- Copilot SDK: https://github.com/github/copilot-sdk
- Rust Copilot SDK:
  https://github.com/github/copilot-sdk/tree/main/rust
- GPUI:
  https://github.com/zed-industries/zed/tree/main/crates/gpui
- Zed ACP integration:
  https://github.com/zed-industries/zed/blob/main/crates/agent_servers/src/acp.rs
- Zed provider abstraction:
  https://github.com/zed-industries/zed/blob/main/crates/acp_thread/src/connection.rs
- Zed terminal integration:
  https://github.com/zed-industries/zed/blob/main/crates/acp_thread/src/terminal.rs
- Zed diff model:
  https://github.com/zed-industries/zed/blob/main/crates/acp_thread/src/diff.rs
