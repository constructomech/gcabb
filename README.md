# GCABB

GCABB stands for **GitHub Copilot App But Better**.

GCABB is an experimental, Rust-native desktop client for GitHub Copilot CLI.
It keeps coding work organized into isolated sessions while making agent
activity, commands, and changes visible as they happen.

## What you can do

- Create, resume, cancel, close, and switch between coding sessions.
- Archive a session to reclaim its worktree while keeping its full history,
  and unarchive it from Settings to rebuild the worktree along with any
  tracked or untracked work that was never committed.
- Optionally archive or delete an entire descendant session tree. Recursive
  lifecycle actions are always opt-in; the confirmation checkbox starts
  unchecked so the default continues to affect only the selected session.
- Fork a project session with `/fork`, preserving its durable Copilot history
  and safe repository state in a new isolated worktree.
- Work in isolated project worktrees without blocking other sessions.
- Stream the conversation and inspect main-agent, subagent, and tool activity.
- Respond to permission, elicitation, user-input, plan, and mode requests.
- Type `/` in the composer to autocomplete app commands, including `/next` to
  queue what the agent should do after the current turn.
- Choose a discovered custom agent, model, mode, reasoning effort, and context length.
- Discover repository and user agents, skills, and instructions for each workspace.
- Save app-open automations with natural-language schedules, optional conditions, and run history.
- Use complete agent rosters for delegated subagent work.
- Inspect committed, staged, unstaged, and untracked changes.
- Follow commands and output in session terminals.
- Restore your selected session after restarting GCABB.

GCABB is still experimental. Keep important work committed or backed up.

## Session lifecycle safety

Archiving or deleting a parent does not affect its child sessions by default.
Those children remain visible as root-level sessions when their parent is no
longer available. To affect the full tree, explicitly select **Also archive
descendant sessions** or **Also delete descendant sessions** in the
confirmation dialog.

Recursive operations take one persisted snapshot of the tree, then stop and
clean up descendants before their parents. Archiving retains every session
record and captures uncommitted tracked and untracked work before removing a
managed worktree. Deleting removes session records, runtime state, and
attachments, but never discards a dirty worktree: any checkout with
uncommitted work is left on disk and reported. Cleanup failures and preserved
paths are shown in the app instead of being silently ignored.

Unarchive affects only the selected archived session. It never silently
unarchives descendants.

## Forks, child sessions, and CLI tasks

A **fork** is a new app session whose Copilot history comes from another
session. It has its own SDK session, runtime process, branch, and worktree, and
it is shown beside the source rather than nested below it. A **child session**
is a separately prompted app session created for coordination and is nested
under its parent. A CLI `task` is lighter-weight subagent activity inside one
runtime; it does not create an app session or worktree.

`/fork` copies the complete durable SDK history. Backend callers may instead
provide an exact SDK event id; that event and everything after it are excluded.
The boundary must belong to the source session.

The fork starts at the source worktree's exact `HEAD`. GCABB reproduces staged
and unstaged tracked changes, renames, deletions, and non-ignored untracked
files without changing the source index or files. Ignored files, runtime state,
credential-like untracked files, and paths outside the repository are never
copied. A conflicted index, submodule, special file, escaping symlink, changing
source snapshot, or other state GCABB cannot reproduce exactly causes the fork
to fail before it is exposed. Untracked executable bits are preserved on Unix;
untracked symlinks are deliberately unsupported on platforms where they cannot
be recreated safely.

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
