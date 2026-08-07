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

## Status

GCABB is in the design and feasibility stage. The first implementation milestone
will validate Copilot SDK event coverage, live terminal output, session recovery,
and native GPUI rendering.

## Disclaimer

This is an independent project and is not affiliated with or endorsed by
GitHub. GitHub Copilot is a trademark of GitHub, Inc.
