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

## Phase 0 spike

The repository contains an executable feasibility spike:

- `sdk-probe` starts the SDK-bundled CLI, records startup timing, exercises
  session lifecycle and typed RPCs, and writes raw plus normalized events as
  JSON Lines.
- `gcabb-desktop-spike` is a native GPUI surface for a streaming activity
  timeline, terminal output, and a Git comparison against a selected base.
- `spike-core` owns the versioned event normalization boundary.

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

# Open the native prototype. The optional argument is a Git base.
cargo run -p gcabb-desktop-spike -- main

# Coalesce 50,000 simulated token and terminal updates on frame boundaries.
GCABB_STRESS_EVENTS=50000 cargo run -p gcabb-desktop-spike -- main
```

Probe output defaults to `.phase0/events.jsonl`, which is ignored by Git because
it can contain prompts, model output, file paths, and tool arguments. See
[`docs/phase-0/`](docs/phase-0/) for results, scenario commands, and known gaps.

## Disclaimer

This is an independent project and is not affiliated with or endorsed by
GitHub. GitHub Copilot is a trademark of GitHub, Inc.
