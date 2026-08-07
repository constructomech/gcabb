# Phase 2 Session MVP

## Outcome

GCABB can now run an ordinary Copilot coding session without opening the Copilot
CLI:

- Persist and navigate projects and sessions.
- Start a session by submitting the first prompt.
- Stream root-agent user and assistant messages into a native transcript.
- Switch sessions without pausing their actors or provider event streams.
- Cancel active work, close a session, and resume it without restarting GCABB.
- Change the model, interaction mode, and reasoning effort.
- Answer permission, elicitation, user-input, plan-exit, and automatic
  mode-switch callbacks in native dialogs.
- Restore the selected session and its transcript on restart.

## UX reference

The implementation used the supplied GitHub Copilot Application Atlas as an
interaction reference, particularly:

- `shell.initial` and `session.complete` for the persistent sidebar, title bar,
  transcript, and bottom composer.
- `composer.mode-menu`, `composer.model-menu`, and
  `composer.reasoning-menu` for grouped controls beneath the draft.
- `composer.project-menu` and `sidebar.grouping-menu` for project/session
  grouping.
- `health-check.dialog` and `session.info-menu` for process and worktree
  metadata.

The atlas remains a session artifact and is not committed to the repository.

## Interaction brokerage

SDK callbacks do not touch GPUI directly. Each SDK session receives an
`InteractionBroker` that:

1. Converts SDK callback inputs into an app-owned `InteractionRequest`.
2. Sends the request to the owning session actor.
3. Publishes the request in the immutable `SessionSnapshot`.
4. Waits on a one-shot response from native UI.
5. Maps the app-owned response back to the typed SDK result.

Pending requests keep the session in `Waiting` even when unrelated nested events
arrive. Close and shutdown cancel all pending callbacks before disconnecting so
the SDK event loop cannot deadlock.

## Transcript projection

The reducer:

- Adds `user.message` content as complete user messages.
- Creates an assistant message at `assistant.message_start`.
- Coalesces `assistant.message_delta` by `messageId`.
- Replaces streaming content with the authoritative `assistant.message`.
- Excludes events carrying `agentId` or `parentToolCallId` from the root
  transcript, preserving them for Phase 3 nested activity UI.

## Controls and lifecycle

Session controls use typed SDK RPCs:

- `session.model.list`, `session.model.getCurrent`, and model switching.
- `session.mode.get` and mode setting.
- `session.model.setReasoningEffort`.

Closing removes the live actor but preserves SQLite and SDK history. Resume
recreates the provider session, reconciles history by event ID, and replaces the
disconnected UI projection. Selecting another session only changes the visible
snapshot; every other actor remains active.

## Persistence

Schema version 2 adds:

- Project metadata.
- Selected-session app state.
- Backward-compatible defaults for transcript, controls, and pending
  interactions in version-1 snapshots.

Stale pending interactions are cleared during recovery because their original
callback channels cannot survive a process restart.

## Validation

Deterministic coverage includes:

- Streaming transcript coalescing and nested-output exclusion.
- Interaction request/response round trips.
- Waiting-state stability while nested events continue.
- Control updates and metadata persistence.
- Close/resume without manager restart.
- Pending callback cancellation on close.
- Project and selected-session round trips.
- Existing crash, reconciliation, redaction, reducer, and WAL tests.

An ignored real-provider integration test starts the bundled CLI, sends a prompt,
waits for a complete assistant transcript message, and disconnects cleanly:

```sh
cargo test -p session-manager --test live_provider -- --ignored --nocapture
```
