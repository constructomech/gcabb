# GCABB

GCABB stands for **GitHub Copilot App But Better**.

GCABB is an experimental, Rust-native desktop client for GitHub Copilot CLI.
The project aims to preserve the session-oriented workflow of GitHub Copilot
App while improving performance, transparency, and developer control.

## Goals

- Build a responsive native interface in Rust without a browser-based UI.
- Use the official GitHub Copilot SDK and Copilot CLI runtime.
- Expose what the main agent, subagents, and tools are doing in real time.
- Show commands and output in attachable, interactive native terminals.
- Allow the changes view to compare against any branch or commit.
- Make startup, model, tool, terminal, Git, and UI latency observable.
- Retain isolated project sessions and worktree-based development.

## Approach

The initial architecture uses GPUI for the desktop interface and the official
Rust Copilot SDK for session and runtime integration. Zed is an architectural
reference for native rendering, background execution, terminal handling, and
session projection, but GCABB is a new application rather than a Zed fork.

See [plan.md](plan.md) for the current architecture and implementation roadmap.

## Current foundation

Phase 1 turns the feasibility spike into production-shaped application
boundaries:

- `app-model` owns versioned events, deterministic reducers, and immutable
  snapshots.
- `copilot-provider` isolates the official SDK and checks protocol
  compatibility.
- `session-manager` runs one serial actor per active session.
- `storage` persists metadata, events, and snapshots in SQLite WAL mode.
- `diagnostics` provides structured, redacted tracing.
- `test-harness` supplies a deterministic fake provider and golden fixtures.
- `gcabb-desktop` starts recovery off the UI thread and renders immutable session
  projections.

```sh
source "$HOME/.cargo/env"
cargo run -p gcabb-desktop
```

Set `GCABB_DATA_DIR` to isolate the application database during development.
Without it, GCABB uses the operating system's local application-data directory.

## Phase 0 probe

The repository retains the executable SDK feasibility probe:

- `sdk-probe` starts the SDK-bundled CLI, records startup timing, exercises
  session lifecycle and typed RPCs, and writes raw plus normalized events as
  JSON Lines.

Rust 1.94 is pinned in `rust-toolchain.toml`.

```sh
# Lifecycle and capability smoke test; no model prompt is sent.
cargo run -p sdk-probe -- --cwd .

# Isolate model-driven probes from the source tree.
mkdir -p .phase0/workspace
cargo run -p sdk-probe -- \
  --cwd .phase0/workspace \
  --approve-permissions \
  --prompt "Inspect the workspace and report what you find."
```

Probe output defaults to `.phase0/events.jsonl`, which is ignored by Git because
it can contain prompts, model output, file paths, and tool arguments. See
[`docs/phase-0/`](docs/phase-0/) for results, scenario commands, and known gaps.

## Disclaimer

This is an independent project and is not affiliated with or endorsed by
GitHub. GitHub Copilot is a trademark of GitHub, Inc.
