# Native Rust Copilot Sessions App

## Decision

Build a new Rust-native desktop application around the official
`github-copilot-sdk` crate.

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
| Native UI | GPUI and `gpui_platform` pinned to revision `027cf0de` |
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

Use GPUI as a standalone Apache-2.0 dependency and keep the application surface
small. Avoid unrelated editor, workspace, project, LSP, extension, remote,
cloud, multi-buffer, and settings systems.

Implementation principles:

- Foreground GPUI entity mutations plus background executors.
- An explicit provider boundary.
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

Status: completed.

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

Status: implemented; hardening continues with later phases.

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

Status: implemented; UX hardening continues with later phases.

- Project and session list.
- Create, resume, close, and cancel.
- Prompt composer, streaming transcript, model, mode, and effort controls.
- Permission, elicitation, and user-input UX.
- Worktree metadata and process-health indicators.

Exit criteria:

- Ordinary coding sessions can be completed without opening Copilot CLI.
- Session switching does not pause unrelated background work.

### Phase 3a: Self-Hosting Foundations (2-3 weeks)

Status: implemented. See [docs/phase-3](docs/phase-3/README.md) and the
verified runtime inventory in
[docs/phase-3/tool-surface.md](docs/phase-3/tool-surface.md).

Phase 3a closes the mechanical loop: the agent can inspect, edit, run commands,
and produce a reviewable diff inside GCABB. It deliberately stops short of
making that work *observable*, which Phase 3b addresses.

Make GCABB capable of developing GCABB. Prefer capabilities inherited from the
official Copilot CLI runtime over app-specific reimplementations, and prove each
inherited capability through the SDK instead of assuming parity with GitHub
Copilot App.

- Preserve the session worktree as the CLI working directory so built-in file
  inspection, search, editing, and Git tools operate on the correct checkout.
- Discover the runtime tool set through the SDK's `tools.list` RPC at session
  start rather than hardcoding tool names, and pin `ClientMode::CopilotCli`
  explicitly. `ClientMode::Empty` strips the built-in file, search, and shell
  tools, and that regression would otherwise appear as an unexplained model
  failure rather than a configuration error.
- Enable terminal calls with incremental command, output, exit-status, and
  cancellation UI. Key terminal state by the runtime's `shellId` rather than by
  tool call: Copilot CLI models background execution as four tools
  (`bash`, `read_bash`, `stop_bash`, `list_bash`) that share a shell handle, so
  a terminal outlives the call that created it and a later read must append to
  the terminal already on screen. Use built-in shell events when sufficient;
  otherwise use the host-owned terminal tool proven in Phase 0.
- Preserve the CLI's GitHub MCP integration and authentication. Verify tool
  discovery and an authenticated read operation from a GCABB-created session;
  do not build a second GitHub client into the agent loop.
- Preserve the CLI's existing skill discovery semantics, including user-level
  skills under `~/.copilot` and repository-level skills from the session
  worktree. Verify that both scopes load and that repository skills cannot leak
  across projects.
- Add a GCA-like changes view with a changed-file list and readable unified
  diffs for the session worktree, refreshed after file and Git changes.
- Surface tool failures, permission requests, missing authentication, and
  unavailable capabilities as actionable session state.
- Support a Windows self-development workflow that builds and tests GCABB into
  an output location that does not contend with the running GCABB executable.
- Add a deterministic capability test plus one opt-in real-provider
  self-hosting scenario: inspect the GCABB repository, make a source change,
  run formatting and targeted tests, inspect a GitHub resource through MCP, and
  review the resulting diff without leaving GCABB.

Exit criteria:

- A developer can complete the edit-command-result-diff loop on GCABB's own
  repository without opening Copilot CLI or another Git client.
- Global and repository skills, GitHub MCP, file tools, and terminal tools are
  proven available in a GCABB-created session.
- Commands use the session worktree, stream useful progress, and can be
  cancelled without terminating unrelated sessions.
- The changes view accurately shows committed, staged, and unstaged changes
  against the session's recorded base.

### Phase 3b: Self-Hosting Parity (1-2 weeks)

Status: implemented, with one item revised against the runtime.

Phase 3a proved GCABB can *perform* the self-hosting loop. Dogfooding GCABB to
build GCABB then exposed a different problem: the work is not observable, and
the feedback channel a UI project depends on is missing. These gaps were found
by running a full development session against GCABB's own repository and asking
which parts of that session GCABB could not have supported.

The ordering below is by whether the gap blocks self-hosting outright.

- Render tool activity in the transcript. `ToolActivity::invocations` is
  already projected, correlated, and tested, but nothing displays it, so a
  session shows prose and terminals while the actual work — reads, searches,
  edits and their diffs — is invisible. This is the single largest gap between
  GCABB's stated goal of showing what the agent is doing and what it shows.
- Accept image attachments on the composer and pass them to the runtime. The
  `+` control is currently an inert placeholder. Screenshots are the primary
  way UI defects are reported, so without this GCABB cannot be used to develop
  its own interface.
- Give the user per-shell control. **Revised: not buildable as written.**
  `stop_bash` is a tool the *model* calls, not a request a client can make; the
  only client-side interruption the SDK exposes is a turn-wide abort. The
  premise that "`stop_bash` exists in the runtime" conflated the model's tool
  surface with the client's RPC surface. What the investigation did find was a
  real defect: aborting left background shells displaying "running" forever,
  because the runtime sends no completion event for shells it tears down. Abort
  now settles them as cancelled. A true per-shell stop needs an RPC the runtime
  does not currently offer.
- Nest subagent activity under the task that spawned it. Events already carry
  `agentId` and `parentToolCallId`; delegated work currently appears as an
  unexplained pause.
- Reconcile the discovered tool surface with what sessions actually offer. A
  live `tools.list` returned neither `sql` nor `session_store_sql` nor
  `web_search`, so capabilities that development workflows use may be absent
  without the UI saying so. Determine whether this is model-scoped and report
  it in the capabilities panel either way.
- Keep composer controls consistent between the home and session composers,
  including thinking level and context window, so selecting a session never
  silently drops a control.

Exit criteria:

- A developer can watch a session edit files and run commands without leaving
  GCABB, including the diff each edit produced.
- A screenshot can be attached to a prompt and reaches the model.
- A single running command can be stopped from the UI without cancelling the
  session or disturbing other sessions.
- Subagent work is attributable to the task that requested it.
- Any capability the runtime does not provide is visible in the capabilities
  panel rather than surfacing as an unexplained failure.

What shipped:

- Tool activity is interleaved with messages in one timeline ordered by event
  sequence, with per-entry scrollable detail blocks and subagent work nested
  under the task that spawned it.
- The transcript and every detail block have draggable scrollbars, and a scroll
  gesture affects only the pane under the pointer.
- Prompts carry file attachments, sent as paths so the runtime opens the file
  itself. Attachments belong to the one prompt they were staged on.
- Capability reporting was corrected in two ways found by reading a live
  session's own report: `apply_patch` and `rg` were classified as unknown
  tools, so the app claimed it could not edit or search while doing both; and a
  chat was reported "blocked" for lacking a changes view it can never have.

Still open, and deferred rather than done:

- Detail blocks render in a proportional font, so commands, diffs, and columnar
  output do not align. Addressed in Phase 5, which introduces a monospace font.
- Subagent nesting is exercised only with synthetic `subagent.started` events;
  the field shape came from Phase 0 notes and has not been observed live.
- Chats share one working directory, so concurrent chats can collide.
- A true per-shell stop, if the runtime ever exposes one.

### Phase 4: Tagged Releases and Auto-Update (1-2 weeks)

Turn the Phase 3a self-hosting build into a repeatable dogfooding loop without
making release engineering a prerequisite for the Self-Hosting MVP itself.

- Define the application version in one authoritative location and expose it in
  the UI and diagnostics.
- Add a tag-driven GitHub Actions release workflow that builds the pinned
  Windows target, runs release validation, packages an installer, generates
  checksums and update metadata, and publishes a GitHub Release.
- Produce release notes from the tag and repository history, with an explicit
  prerelease channel for self-hosting builds and a stable channel for promoted
  releases.
- Cryptographically sign update metadata and artifacts independently of
  platform code signing; keep signing keys out of the repository and fail
  closed when verification fails.
- Add a client updater that checks the selected channel, compares semantic
  versions, shows release notes and download progress, stages the update, and
  applies it on restart.
- Make Windows replacement and rollback safe when GCABB is updating the
  executable currently in use. Preserve user data, session state, and the
  bundled CLI compatibility contract across updates.
- Allow automatic checks to be disabled and updates to be deferred; never
  interrupt an active coding session to install an update.

Exit criteria:

- Pushing a version tag produces a versioned Windows GitHub Release and valid
  update metadata without manual packaging.
- A Phase 3a installation can discover, verify, download, and apply the next
  tagged prerelease, then resume its existing projects and sessions.
- Invalid signatures, interrupted downloads, incompatible updates, and failed
  replacement leave the installed client runnable and provide a recovery path.

### Phase 5: Rich Text Rendering (1-2 weeks)

Status: planned.

Assistant replies are markdown, and GCABB shows them as their source: a reply
reads `**Hardware issue detected:**` rather than emphasising the phrase, and
lists, headings, and code blocks arrive as literal punctuation. Everything the
model writes to be read is currently harder to read than it would be in a
terminal.

Zed's `markdown` crate is **GPL-3.0-or-later** and cannot be used or copied
here: GCABB is MIT, and depending on it would force the whole application to
become GPL. This is not merely a licence header to review; it rules out reading
that implementation for guidance as well. The other Zed crates GCABB depends on
(`gpui`, `gpui_platform`, `gpui_linux`) are Apache-2.0 and unaffected.

Parsing therefore uses `pulldown-cmark`, which is MIT and depends only on
`bitflags` and `memchr`, with default features off so the HTML renderer is not
pulled in. Rendering is GCABB's own, built on primitives GPUI already provides:
a `TextRun` carries font weight, style, family, background, underline, and
strikethrough, which is the whole of inline markdown within one wrapped
`StyledText`, and block elements are ordinary divs of the kind the transcript
already builds.

- Render headings, paragraphs, bullet and numbered lists, fenced and inline
  code, block quotes, horizontal rules, links, and inline emphasis.
- Render partial markdown sanely while a reply streams. The parser sees
  unclosed fences and half-written emphasis on nearly every frame, so the
  rendering must not flicker between interpretations as text arrives.
- Adopt a monospace font for code, which the application currently sets nowhere.
  This also closes the Phase 3b gap where tool detail blocks render commands,
  diffs, and columnar output in a proportional font that does not align.
- Keep the source text recoverable, so a reply can still be copied as the
  markdown that was written rather than as flattened prose.
- Defer tables and inline images; neither is needed to read a reply, and both
  add layout work disproportionate to that benefit.

Exit criteria:

- A reply containing headings, lists, emphasis, and code reads as formatted text
  rather than as markdown source.
- Markdown renders correctly while streaming and does not change interpretation
  once the reply completes.
- Code and command output render in a monospace font, in both assistant replies
  and tool detail blocks.
- No GPL-licensed code or derivation enters the project; the dependency added
  for parsing is MIT.

### Phase 6: Operability and Visibility (2 weeks)

- Add the activity timeline, agent tree, filters, inspector, and
  duration/status display.
- Correlate SDK, hook, task, fleet, terminal, Git, file, MCP, and skill events.
- Add latency breakdowns, capability diagnostics, and redacted diagnostic
  export.
- Link command and file activity to the Phase 3a terminal output and changes
  view.

Exit criteria:

- Every observed activity has explicit lifecycle and timing.
- A stalled self-hosting session can be localized to model, tool, input,
  terminal, Git, MCP, skill loading, SDK, or UI propagation.
- Diagnostic exports explain capability discovery without exposing credentials,
  prompts, repository content, or sensitive tool arguments.

### Phase 7: Terminal and Changes Hardening (3-4 weeks)

- Upgrade Phase 3a command output into the native virtualized GPUI terminal with
  PTYs, ANSI/VT parsing, bounded scrollback, attach/detach, interactive input,
  resize, process-tree control, and persistence.
- Add user-created terminals and reusable terminal sessions.
- Expand the Phase 3a changes view with repository/ref discovery, selectable
  bases, merge-base calculation, persisted base selection, and activity links.
- Add virtualized split/unified diffs, syntax highlighting, context expansion,
  and defined handling for renames, binaries, submodules, deleted files, and
  large files.

Exit criteria:

- Long-running and interactive commands remain responsive under high-volume,
  memory-bounded output.
- Cancellation targets the correct process tree, and view changes do not stop
  active commands.
- Changing the diff base updates the view without changing branches or agent
  state.
- Results match Git across committed and uncommitted changes, including the
  documented edge cases.

### Phase 8: Product Hardening and Cross-Platform Distribution (2-3 weeks)

- Production OS code signing and installer polish.
- Extend the Phase 4 release and update pipeline to macOS and Linux.
- Windows-first release testing, then macOS and Linux.
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
| Release/update | Tag workflow, manifest signatures, channel selection, staged replacement, rollback |
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
| Update supply-chain or replacement failure | Signed metadata, checksums, staged install, rollback, fail closed |
| Diagnostic data leaks | Structured redaction and explicit export preview |

## Deferred Extensions

- ACP provider for Claude, Gemini, Codex, or other registry agents.
- AHP server for remote, mobile, or simultaneous multi-client access.
- Cloud-hosted sessions.
- Embedded editor or full IDE features.

These remain outside the MVP and must not distort the initial domain model.

## Estimate

The Windows-first implementation is approximately **15-20 engineer-weeks** for
one experienced Rust desktop engineer. Two engineers could target roughly
**10-13 calendar weeks** after the feasibility spike. A polished cross-platform
release may require another 4-6 engineer-weeks.

The largest near-term uncertainty is whether CLI-owned shell, GitHub MCP, and
skill capabilities retain full behavior through SDK-created sessions. The
largest later uncertainty is the amount of native terminal and diff hardening
needed beyond the Phase 3a and 3b self-hosting surfaces.

Rendering work carries a licensing constraint rather than a technical one:
Zed's markdown and terminal crates are GPL-3.0-or-later, so the Apache-2.0
`gpui` foundation is reusable but those higher-level crates are not. Phases 5
and 7 must build on permissively licensed parsers rather than adapting Zed's,
and estimates assume that.

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
- GPUI: https://docs.rs/gpui
