# GCABB

GCABB stands for **GitHub Copilot App But Better**.

GCABB is an experimental, Rust-native desktop client for GitHub Copilot CLI.
It keeps coding work organized into isolated sessions while making agent
activity, commands, and changes visible as they happen.

## What you can do

- Create, resume, cancel, close, and switch between coding sessions.
- Work in isolated project worktrees without blocking other sessions.
- Stream the conversation and inspect main-agent, subagent, and tool activity.
- Respond to permission, elicitation, user-input, plan, and mode requests.
- Choose the model, mode, reasoning effort, and context length.
- Inspect committed, staged, unstaged, and untracked changes.
- Follow commands and output in session terminals.
- Restore your selected session after restarting GCABB.

GCABB is still experimental. Keep important work committed or backed up.

## Install

The installers below select the newest published version, including release
candidates.

### macOS

```sh
curl -fsSL https://raw.githubusercontent.com/constructomech/gcabb/main/scripts/install-macos.sh | bash
open ~/Applications/GCABB/GCABB.app
```

GCABB supports Apple Silicon Macs and installs `GCABB.app` in
`~/Applications/GCABB`. Intel Macs are not supported. Run the executable
directly with `~/Applications/GCABB/GCABB.app/Contents/MacOS/gcabb-desktop`.

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/constructomech/gcabb/main/scripts/install-linux.sh | bash
~/.local/bin/gcabb-desktop
```

GCABB supports x86-64 Linux, installs in `~/.local/lib/gcabb`, and links the
command into `~/.local/bin`.

### Windows

Run these commands in PowerShell:

```powershell
irm https://raw.githubusercontent.com/constructomech/gcabb/main/scripts/install-windows.ps1 | iex
& "$env:LOCALAPPDATA\GCABB\gcabb-desktop.exe"
```

GCABB supports x86-64 Windows, including x86-64 emulation on Windows ARM64,
and installs in `%LOCALAPPDATA%\GCABB`.

To install a specific version or choose another location, download the
installer for your platform and use its tag argument or set
`GCABB_INSTALL_DIR`.

## Updates

Installed builds check the matching release channel for updates. Release
candidates receive newer release candidates, while stable versions receive
stable updates. When an update is available, GCABB offers **Update** and then
**Restart**.

GCABB checks when it starts and about every six hours while it remains open.
You can also choose **Settings** → **Check for updates** at any time.

Updates are verified with a signed release manifest and artifact checksum
before installation. GCABB keeps the previous installation until the new
version starts successfully.

The installed application also supports:

```sh
gcabb-desktop --version
gcabb-desktop --check-update
gcabb-desktop --apply-update
```

## License

GCABB is licensed under the [MIT License](LICENSE.txt). Required notices for
included third-party software are collected in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Disclaimer

This is an independent project and is not affiliated with or endorsed by
GitHub. GitHub Copilot is a trademark of GitHub, Inc.
