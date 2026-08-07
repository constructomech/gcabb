# Event Coverage Matrix

`observed` means a live SDK/CLI run emitted the data. `available` means the
typed API compiled and was installed but no live fixture triggered it.
`unavailable` identifies detail not present in the observed event stream.

| Area | Status | Correlation and payload |
| --- | --- | --- |
| Main-agent turns | observed | Turn start/end, intent, reasoning, message deltas, final messages, usage |
| Model calls | observed | Start and usage are visible; no successful model-call completion event was emitted |
| Tool lifecycle | observed | Name/arguments at start, partial output, structured result/error, success, duration through timestamps |
| Built-in shell | observed | Command, cwd-related metadata, shell ID, partial output, exit code, timeout and detached flags |
| File edits | observed | Tool arguments and detailed unified diff in tool completion |
| Permissions | observed | Requested/completed events and request IDs; host decision is handled separately |
| Hooks | observed | Start/end with hook name, tool or agent attribution, and timing |
| Custom terminal | observed | Invocation ID, incremental stdout/stderr, exit code, and bounded result tail |
| Subagent lifecycle | observed | Started/completed, type, model, duration, token count, tool count, `agentId`, parent tool call |
| Subagent internals | observed | Child model, message, tool, usage, and system events carry the same `agentId` |
| Fleet activation | observed | Typed `fleet.start` result and subsequent subagent event stream |
| Tasks list | observed | Typed read-only RPC completed |
| Agent list | observed | Typed read-only RPC completed |
| Plan/workspace/model/mode | observed | Typed read-only RPCs completed |
| Cancellation | observed | Abort reason and terminal idle event with `aborted: true` |
| Tool failure | observed | Structured code/message and `success: false` |
| Elicitation callback | available | Typed handler registered; no live event triggered |
| User-input callback | available | Typed handler registered; no live event triggered |
| Exit-plan callback | available | Typed handler registered; no live event triggered |
| Auto-mode-switch callback | available | Typed handler registered; no live event triggered |
| Hidden model reasoning | unavailable | Only reasoning content intentionally emitted by the runtime is visible |
| OS process tree for built-in shell | unavailable | Shell metadata is visible, but child-process topology is not an SDK event |
| Built-in shell interactive input/resize | unavailable | Requires host-owned PTY fallback |

## Visibility rules

- Direct SDK and app-service events are `observed`.
- Activity joined through stable `agentId`, `toolCallId`, `parentToolCallId`,
  request ID, or parent event ID may be marked `inferred` by future reducers.
- The UI must not infer hidden reasoning, unreported subprocesses, or completion
  after interrupted work.

Subagent assistant output belongs beneath its spawning task. It must not be
duplicated into the main transcript when `parentToolCallId` identifies it as
nested activity.

Raw SDK envelopes remain authoritative diagnostic evidence. The app projection
is versioned and preserves unrecognized event types as system activities rather
than dropping them.
