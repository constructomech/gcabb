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
Rust Copilot SDK for session and runtime integration.

See [plan.md](plan.md) for the current architecture and implementation roadmap.

## License

GCABB is licensed under the [MIT License](LICENSE.txt). Required notices for
included third-party software are collected in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Current Session MVP

Phase 2 provides an end-to-end native session workflow:

- Persisted project and session navigation.
- Create, resume, close, cancel, and switch between background sessions.
- Native prompt composer and streaming transcript.
- Model, mode, reasoning-effort, and context-length controls.
- Permission, elicitation, user-input, plan-exit, and mode-switch dialogs.
- Worktree branch and Copilot process-health indicators.
- SQLite-backed selected-session restoration.

```sh
source "$HOME/.cargo/env"
cargo run -p gcabb-desktop
```

Set `GCABB_DATA_DIR` to isolate the application database during development.
Without it, GCABB uses the operating system's local application-data directory.

On Linux, install the desktop entry once so the taskbar and window titlebar can
resolve the application icon. Windows embeds its icon in the executable and
macOS uses the app bundle, but Wayland and X11 match the window's application ID
against an installed desktop entry instead.

```sh
./scripts/install-linux-desktop-entry.sh
```

See [`docs/phase-2/`](docs/phase-2/) for the implemented interaction model and
validation coverage.

## Phase 0 probe

The repository retains the executable SDK feasibility probe:

- `sdk-probe` starts the SDK-bundled CLI, records startup timing, exercises
  session lifecycle and typed RPCs, and writes raw plus normalized events as
  JSON Lines.

Rust 1.95 is pinned in `rust-toolchain.toml`.

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
