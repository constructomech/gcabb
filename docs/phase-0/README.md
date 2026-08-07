# Phase 0 Feasibility Report

> Historical Phase 0 baseline. GPUI was subsequently upgraded to Zed commit
> `027cf0de` with `gpui_platform` and AccessKit support.

Date: 2026-08-06

## Outcome

The Rust-native architecture is feasible with the official SDK and GPUI. The
spike satisfies the Phase 0 exit criteria with one SDK compatibility issue to
track and several callback cases that still need purpose-built fixtures.

| Decision | Result |
| --- | --- |
| Embed or scrape the Copilot CLI TUI | Not required |
| Consume built-in shell output | Viable through `tool.execution_partial_result` |
| Keep a host-owned terminal fallback | Viable; `phase0_terminal` streams stdout/stderr independently |
| Attribute subagent activity | Viable through `agentId` and `toolCallId` |
| Use GPUI without Zed | Viable with pinned `gpui` 0.2.2 |
| Use `gpui_platform` now | No; it is not published with GPUI 0.2.2 |

## Live evidence

All live scenarios ran in ignored `.phase0/` fixtures. Their JSONL output is
intentionally not committed because it contains full prompts, paths, arguments,
and model output.

| Scenario | Evidence |
| --- | --- |
| Lifecycle smoke | Bundled CLI start, protocol handshake, model list, create, seven read-only typed RPCs, history read, disconnect, resume, stop |
| File and terminal | Inspected a fixture, created an exact 10-byte file, observed built-in shell partial output, and streamed three custom terminal lines |
| Failure | Nonexistent executable produced `tool.execution_complete` with `success: false` and a structured error |
| Cancellation | One `Session::abort` produced `abort` and `session.idle { aborted: true }` |
| Fleet | `fleet.start` returned `started: true`; one explore subagent emitted directly attributed model, tool, usage, and lifecycle events |

The successful scenario captured 249 SDK events spanning model streaming, tool
execution, permissions, hooks, file creation, built-in shell output, and the
custom tool. Across all scenarios, 37 distinct event types were observed.

## Timing

Measurements are from macOS 26.5.2 on arm64 and are indicative rather than a
benchmark suite.

| Measurement | Result |
| --- | ---: |
| Cold client start | 4,497 ms |
| Cold CLI resolution/extraction | 1,876 ms |
| Cold handshake | 2,614 ms |
| Warm client start | 335 ms |
| Warm handshake | 329 ms |
| Prompt to first reasoning event | 2,150 ms |
| Three-line custom terminal command | 608 ms plus agent/tool overhead |
| 50,000 timeline plus 50,000 terminal rows | 915 ms in 1,000-event deltas |

The warm result meets the 500 ms application-side startup budget for the SDK
client itself. First-token time remains model/network dependent and is reported
separately.

## Implementation

`sdk-probe`:

- Uses `Client::start`, `create_session`, `resume_session`, `send`, `abort`,
  `get_events`, `disconnect`, and `stop`.
- Records SDK startup phase timing without parsing logs.
- Exercises model, mode, workspace, plan, agent, tasks, and fleet typed RPCs.
- Installs permission, elicitation, user-input, plan-exit, auto-mode-switch, and
  hook handlers.
- Retains raw event envelopes and emits versioned `DomainEvent` projections.
- Registers an argv-based host terminal tool with incremental host output and a
  bounded 200-line result tail.

`gcabb-desktop-spike`:

- Runs as a native GPUI process without a WebView.
- Updates the activity and terminal projections asynchronously.
- Limits rendered rows to the latest 200 while retaining full spike state.
- Reads a selected Git base without checking it out or mutating repository state.
- Can inject coalesced high-volume updates with `GCABB_STRESS_EVENTS`.

## Exit criteria

| Criterion | Status |
| --- | --- |
| No terminal scraping or interactive TUI embedding | Met |
| Main-agent, tool, and subagent visibility limits known | Met; see event matrix |
| Host-owned terminal fallback proven | Met |
| GPUI responsive under coalesced event load | Met by native stress prototype; formal frame telemetry remains Phase 1 work |

## Remaining work

- A newly created session with no prompt was not persisted and could not be
  resumed. A prompted, disconnected session resumed successfully with its
  original ID; callers should not advertise empty sessions as recoverable.
- Elicitation, user-input, plan-exit, and auto-mode-switch handlers compile and
  are registered but were not naturally triggered by the live scenarios.
- The custom terminal fallback proves streaming and bounded return data, not PTY
  input, resize, detach, or process-tree cancellation. Those belong to Phase 4.
- GPUI 0.2.2 uses `Application::new()`. Zed main has moved platform startup into
  an unpublished `gpui_platform`; upgrading must be isolated behind the desktop
  entry point.

## Subagent UI model

Subagent visibility is based on the supported session-event stream:
`subagent.started`, `subagent.completed`, and `subagent.failed`. Child model,
message, tool, and usage events carry `agentId`; the spawning task is linked by
`toolCallId` or `parentToolCallId`.

This matches VS Code's Copilot CLI session renderer: it enriches the pending task
tool invocation from `subagent.started`, treats the task's
`tool.execution_complete` as completion, and keeps nested assistant output out
of the main transcript.

See [event coverage](event-coverage.md), [compatibility](compatibility.md), and
[reproduction scenarios](scenarios.md).
