# Phase 3 Self-Hosting MVP

## Outcome

GCABB can complete the edit-command-result-diff loop on its own repository:

- Sessions run with the session worktree as the CLI working directory, so the
  inherited file, search, edit, and Git tools operate on the right checkout.
- Tools are discovered through the SDK at session start and projected into
  capability state instead of being assumed.
- Tool invocations, streaming output, failures, and shell lifecycles are
  app-owned projections the native UI renders.
- A changes view reports committed, staged, unstaged, and untracked changes
  against the session's recorded base, refreshed after worktree-mutating tools.
- Missing tools, failed discovery, tool failures, and a non-git session
  directory all surface as actionable session state.

See [tool-surface.md](tool-surface.md) for the verified inventory of what the
runtime provides and how it compares to other harnesses.

## Capability discovery

`AgentProvider::discover_tools` calls the SDK's `tools.list` RPC and maps the
result into `ToolCatalog`. `CapabilityReport::from_catalog` then derives status
for the capabilities the loop depends on:

| Capability | Proven by |
| --- | --- |
| File inspection | a tool whose class reads files (`str_replace_editor`, or `view`) |
| File editing | a tool whose class writes files (`str_replace_editor`, or `create`/`edit`) |
| Code search | `ToolClass::Search` (`glob`, `grep`) |
| Terminal commands | `ToolClass::Shell` (`bash`) |
| GitHub MCP | any tool with an MCP source |
| Skills | `ToolClass::Skill` (`skill`) |
| Changes view | a live git inspection of the session worktree |

Capabilities are deliberately derived from tool *class*, not from tool names.
The runtime's model-facing names differ from the CLI's user-facing aliases —
file editing arrives as a single `str_replace_editor` tool rather than
`view`/`create`/`edit` — so `ToolClass::FileEditor` evidences both reading and
writing, and both name surfaces map onto the same capability set.

Discovery failure is not fatal. The session still runs, the catalog records the
error, and every tool-backed capability reports `Unknown` with that error as its
detail, so the UI can explain why the loop may not work.

`ClientMode::CopilotCli` is pinned explicitly in `CopilotProvider::start`;
`ClientMode::Empty` would silently remove the file and shell tools.

## Terminal lifetime is keyed by shell, not by tool call

Copilot CLI exposes four shell tools that share a runtime-assigned `shellId`.
A terminal therefore outlives any single tool call, and the projection reflects
that:

- `bash` creates a `TerminalSession` and records its display command.
- `read_bash` against the same `shellId` appends to that terminal and adds its
  call id to `tool_call_ids` rather than creating a second terminal.
- `stop_bash` marks the terminal cancelled.
- A `shell_exit` content block sets the exit code and moves the terminal to
  `Exited`. A later read cannot resurrect an exited shell.

Output is mirrored into both the invocation and its terminal, bounded to
`MAX_RETAINED_OUTPUT` with front trimming on a character boundary, and flagged
via `output_truncated` when trimming occurs.

## Changes view

`git-service` runs the Git CLI behind typed argument vectors; no user text is
ever concatenated into a shell string.

- The comparison is against `merge-base(HEAD, base_ref)`, so commits landing on
  the base branch after the session started do not appear as session changes.
- A single `git diff --numstat -M -z <base>` covers committed, staged, and
  unstaged changes together, which is what accuracy requires. `-z` is used
  because the textual numstat format renders renames ambiguously as
  `old => new`.
- `git status --porcelain=v1` classifies each path's stage; paths absent from
  status are committed-only.
- Untracked files are added separately with a synthesized `--no-index` diff,
  since `git diff` cannot see them.
- Binary files and diffs above `MAX_DIFF_BYTES` record an omission reason
  instead of a diff.

The base ref is recorded once on `SessionMetadata` at creation, defaulting to
the project's default branch. Storage schema version 3 adds the column and
migrates existing databases in place.

Refresh is event-driven, not polled: the actor recomputes changes only after a
`tool.execution_complete` whose invocation belongs to a worktree-mutating class.
Git runs on a blocking thread so a large diff cannot stall the actor loop.

## UI

A session inspector panel beside the transcript exposes three tabs:

- **Changes** — changed-file list with per-file insertions and deletions, and
  the unified diff for the selected file.
- **Terminals** — one card per `shellId` with command, state, exit code,
  contributing call count, and a bounded output tail.
- **Capabilities** — per-capability status and detail, plus recent tool
  failures with their structured error codes.

The title-bar toggle summarizes changed files, running terminals, and blocked
capabilities.

## Windows self-development

Windows locks a running executable, so rebuilding GCABB into the default target
directory from inside a running GCABB fails at link time. `scripts/self-dev.ps1`
redirects `CARGO_TARGET_DIR` to a per-worktree directory under `LOCALAPPDATA`,
outside `target/` so `cargo clean` does not remove it, and warns when a
`gcabb-desktop` process is running from that directory. `scripts/self-dev.sh`
provides the same isolation on Linux and macOS.

```sh
./scripts/self-dev.sh test
```

```powershell
./scripts/self-dev.ps1 test
```

## Validation

Deterministic coverage in `crates/session-manager/tests/phase3_capabilities.rs`:

- Inherited tools are discovered and every capability reports available.
- Omitted tools report as blocking capabilities without failing the session.
- Discovery failure is visible and non-fatal.
- The changes view reports all four stages against the recorded base.
- Changes refresh after a mutating tool completes.
- Two tool calls sharing a `shellId` produce exactly one terminal.
- A non-git session directory degrades to an explained capability.

Reducer coverage in `crates/app-model` adds shell projection, `read_bash`
append semantics, structured tool failures, edit-diff retention, snapshot round
trips with index rebuilding, subagent attribution, and capability derivation.
`crates/git-service` covers stage classification, rename detection, totals, and
non-worktree handling.

The opt-in real-provider scenario drives the full loop:

```sh
cargo test -p session-manager --test phase3_capabilities -- --ignored --nocapture
```

It asserts `tools.list` returns a capability-complete set against the live
runtime, then has the agent create a file and run a command, and verifies the
resulting diff appears in the changes view. The test approves permission
requests as they arrive, since nothing else answers those callbacks.

Two behaviours were found by running it and are recorded here rather than
worked around silently:

- `tools.list` returns model-facing names that differ from the CLI's
  user-facing aliases, which is why the test asserts capability rather than
  tool names.
- Events that arrive after `session.idle` move the projected session status
  back to `Running`, so status is not a reliable completion signal. This is
  existing Phase 2 projection behaviour; the timeline work in Phase 5 is the
  right place to give turn completion an explicit lifecycle.
