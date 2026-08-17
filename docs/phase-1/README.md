# Phase 1 Application Foundation

## Architecture

Phase 1 replaces the feasibility-only projection with durable application
boundaries:

```text
GPUI FoundationView
    receives Arc<SessionSnapshot>
             |
SessionManager
    one serialized SessionActor per live session
       |
       +-- SessionRuntime per live session
       |      |
       |   CopilotProvider
       |      |
       |   github-copilot-sdk Client + CLI process
       |
       +-- Storage
              |
           SQLite WAL
```

The GPUI thread performs no SDK, database, Git, or filesystem work. A dedicated
service thread owns the Tokio runtime, provider factory, manager, and actors.
Every app session owns a separate provider, SDK client, and CLI process, so
closing or losing one runtime does not interrupt another. Compatibility and
title generation use short-lived clients rather than an ambient client shared
by active sessions. GPUI polls only service messages and Tokio watch snapshots
on a 33 ms frame budget.

## State and recovery

- Every normalized event has an app session ID, monotonic sequence, stable event
  ID, correlation IDs, normalized activity fields, and the complete raw SDK
  envelope.
- Reducers reject wrong-session and out-of-order events and ignore duplicate
  event IDs.
- SQLite runs in WAL mode with foreign keys, bounded busy timeout, and a
  forward-only schema version.
- Events are appended before reducer publication.
- Snapshots are written every 50 events and at idle, failure, and disconnect
  boundaries.
- Restart loads the latest snapshot, replays subsequent database events, resumes
  the SDK session, and reconciles SDK history by event ID.
- One failed resume is reported independently and does not hide other persisted
  session metadata.

## Provider compatibility

`CopilotProvider` checks that the negotiated protocol is at least version 3 and
exposes the Rust SDK version, SDK protocol, negotiated protocol, child PID, and
startup timing breakdown. SDK types stop at the provider boundary.

Permission handling currently denies by default. Phase 2 will connect provider
callbacks to explicit native permission and input UI.

## Diagnostics

Provider and actor failures are structured diagnostic events. Values under
token, authorization, password, and secret-like keys are recursively redacted
before entering tracing or in-memory diagnostics.

## Tests

The deterministic suite covers:

- Reducer ordering, idempotency, raw-payload retention, and outcome mapping.
- WAL migration and schema version.
- Snapshot-plus-event recovery.
- Actor event serialization and immutable snapshot publication.
- Database close/reopen, provider resume, history reconciliation, and duplicate
  suppression.
- Independent resume-failure reporting.
- Distinct provider runtimes and shutdown isolation between concurrent sessions.
- Recursive diagnostic redaction.
- Golden SDK event fixture validity.

Run:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
```

For an isolated desktop smoke run:

```sh
GCABB_DATA_DIR="$PWD/.phase1-smoke" cargo run -p gcabb-desktop
```
