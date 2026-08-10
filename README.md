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

Phase 3a adds the self-hosting loop:

- Runtime tool discovery through the SDK, projected into per-session
  capability state instead of assumed.
- A session inspector with changes, terminals, and capability tabs.
- A changes view covering committed, staged, unstaged, and untracked changes
  against the session's recorded base.
- Terminal state keyed by the runtime's shell id, so output from later
  `read_bash` calls appends to the terminal already on screen.

See [docs/phase-3](docs/phase-3/README.md) for details and
[docs/phase-3/tool-surface.md](docs/phase-3/tool-surface.md) for the verified
inventory of tools GCABB inherits from Copilot CLI.

```sh
source "$HOME/.cargo/env"
cargo run -p gcabb-desktop
```

On fish, source the shell-specific file instead, or add Cargo to `PATH` once
with `fish_add_path "$HOME/.cargo/bin"`:

```fish
source "$HOME/.cargo/env.fish"
cargo run -p gcabb-desktop
```

Set `GCABB_DATA_DIR` to isolate the application database during development.
Without it, GCABB uses the operating system's local application-data directory.

### Developing GCABB inside GCABB

Building GCABB from a GCABB session must not target the executable that session
is running from; on Windows the link step fails outright. The self-development
scripts redirect Cargo to a per-worktree target directory:

```sh
./scripts/self-dev.sh test
```

```powershell
./scripts/self-dev.ps1 test
```

Both accept `build`, `test`, `clippy`, `fmt`, and `run`.

On Linux, install the desktop entry once so the taskbar and window titlebar can
resolve the application icon. Windows embeds its icon in the executable, but
Wayland and X11 match the window's application ID against an installed desktop
entry instead.

```sh
./scripts/install-linux-desktop-entry.sh
```

See [`docs/phase-2/`](docs/phase-2/) for the implemented interaction model and
validation coverage.

## Releases and updates

Releases are tag-driven and cover Linux, macOS, and Windows. Pushing a `v*` tag
builds every target, validates the workspace, signs update metadata, and
publishes a single GitHub Release.

Install the newest published release, including prereleases, with the command
for your platform.

### macOS

```sh
curl -fsSL https://raw.githubusercontent.com/constructomech/gcabb/main/scripts/install-macos.sh | bash
~/Applications/GCABB/gcabb-desktop
```

The installer detects Apple Silicon or Intel and installs GCABB in
`~/Applications/GCABB`.

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/constructomech/gcabb/main/scripts/install-linux.sh | bash
~/.local/bin/gcabb-desktop
```

The installer supports x86-64 Linux, installs GCABB in `~/.local/lib/gcabb`, and
links the command into `~/.local/bin`.

### Windows

Run these commands in PowerShell:

```powershell
irm https://raw.githubusercontent.com/constructomech/gcabb/main/scripts/install-windows.ps1 | iex
& "$env:LOCALAPPDATA\GCABB\gcabb-desktop.exe"
```

The installer supports x86-64 Windows, including x86-64 emulation on Windows
ARM64, and installs GCABB in `%LOCALAPPDATA%\GCABB`.

The second command in each block launches the installed application. To install
a specific tag or choose another location, download the platform script and use
its tag argument or `GCABB_INSTALL_DIR` setting.

```sh
# The version is declared once, in [workspace.package] of the root Cargo.toml.
git tag v0.2.0 && git push origin v0.2.0     # stable channel
git tag v0.2.0-rc.1 && git push origin --tags # prerelease channel
```

Installed builds verify an ed25519 signature over the release manifest and the
SHA-256 of the downloaded artifact before replacing anything, and keep the
previous installation until the new one starts successfully. Builds from a
developer checkout report channel `dev` and never update themselves, so
`cargo run` cannot be overwritten by a release.

See [`docs/phase-4/`](docs/phase-4/) for key setup, channel rules, and the
rollback design.

An installed build can also be driven from the command line, which is how the
update loop is tested on each platform:

```sh
gcabb-desktop --version        # build identity
gcabb-desktop --check-update   # 0 available, 1 failed, 2 nothing to do
gcabb-desktop --apply-update   # download, verify, apply
scripts/update-rehearsal.sh    # build two versions and self-update between them
```

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
