# Copilot CLI Tool Surface

Evidence for what GCABB inherits from the Copilot CLI runtime, and how that
compares to other coding-agent harnesses. GCABB implements none of these tools;
it hosts the runtime that owns them. This document exists so the host renders
their activity correctly and so a runtime upgrade that changes the tool surface
is detected rather than assumed.

## Verification method

Two independent probes were used, because the CLI's user-facing tool names and
the model-facing names returned by `tools.list` are not the same.

The bundled CLI prints its full registered tool set when given a tool name it
does not recognize, because the "disabled tools" line then lists every tool
except the unknown one:

```sh
"$HOME/.cache/github-copilot-sdk/cli/1.0.79-5/copilot" \
  --available-tools=bogus -p "hi" --allow-all-tools
```

The model-facing list comes from calling `tools.list` through the SDK against
the same CLI build; the ignored test
`real_provider_completes_the_self_hosting_loop` performs exactly this call and
asserts the resulting capabilities. Neither list is inferred from
documentation.

## Built-in tools (Copilot CLI 1.0.79-5)

Two different name surfaces exist, and they do not match. This matters more
than any single tool name, so both are recorded.

### User-facing CLI tool names

| Category | Tools |
| --- | --- |
| File read and list | `view` |
| File write | `create`, `edit` |
| Search | `glob`, `grep` |
| Shell | `bash`, `read_bash`, `stop_bash`, `list_bash` |
| Web | `web_fetch`, `web_search` |
| Delegation | `task`, `read_agent`, `write_agent`, `list_agents` |
| Data | `sql`, `session_store_sql` |
| Skills and docs | `skill`, `fetch_copilot_cli_documentation` |
| GitHub MCP (default subset) | `get_file_contents`, `search_code`, `search_users`, `get_copilot_space`, `list_copilot_spaces` |

### Model-facing names returned by `tools.list`

A live `tools.list` call against the same CLI build returns:

```text
bash, read_bash, stop_bash, list_bash, str_replace_editor, web_fetch,
fetch_copilot_cli_documentation, skill, ask_user, read_agent, list_agents,
write_agent, grep, glob, task
```

The differences are load-bearing:

- File editing is a **single `str_replace_editor` tool**, not the separate
  `view`, `create`, and `edit` tools the CLI presents to users. One tool
  provides both reading and writing.
- `ask_user` is registered here but absent from the CLI's user-facing list.
- `web_search`, `sql`, and `session_store_sql` did not appear, and no MCP tools
  were present in an unconfigured environment. The list also varies by model,
  which is why `ToolsListRequest` takes an optional model id.

GCABB therefore classifies `str_replace_editor` as `ToolClass::FileEditor`,
which evidences both the file-read and file-write capabilities, and recognizes
both name surfaces so either shape maps onto the same capability set. Asserting
on capability rather than on tool name is the reason the live test survives this
difference.

The SDK additionally defines `BUILTIN_TOOLS_ISOLATED`, a curated set safe for
multi-tenant hosts: `ask_user`, `task_complete`, `exit_plan_mode`, `task`,
`read_agent`, `write_agent`, `list_agents`, `send_inbox`, `context_board`,
`skill`.

Notably absent compared with other harnesses: no whole-file `write`, no
`multi_edit`, no LSP or semantic-search tool, and no notebook or browser tools.

## Comparison with other harnesses

The consensus core across Claude Code, Codex CLI, Gemini CLI, Cursor, Cline,
OpenHands, Aider, and the MCP filesystem server is: read, whole-file write,
targeted edit, glob, grep, list directory, shell, and a user-clarification
tool. Copilot CLI covers all of these except whole-file write.

Differences that affect GCABB's UI:

| Concern | Copilot CLI | Elsewhere | Consequence for GCABB |
| --- | --- | --- | --- |
| Targeted edit | `edit`, exact string replace | String replace (Claude Code, Gemini, Cline, MCP fs) or unified-diff patch (Codex, Aider) | One diff renderer suffices; edits return a rendered diff in `detailedContent` |
| Whole-file write | none | `Write`, `write_file`, `write_to_file` | The changes view never needs a bulk-replace case |
| Directory listing | folded into `view` | Separate `list_dir` / `list_directory` | One renderer must handle both file and directory results |
| Background execution | four tools sharing a `shellId` | An `is_background` parameter on one shell tool | Terminals must be keyed by `shellId`, not by tool call |
| Code intelligence | none | Claude Code `LSP`, Cursor `codebase_search`, Cline `list_code_definition_names` | Search UI is limited to glob and grep results |

The `shellId` difference is the most consequential. Every other surveyed
harness models a background process as a parameter on a single shell tool, so a
terminal and a tool call are one-to-one. Copilot CLI models it as a family of
four tools that share a runtime-assigned handle, so a terminal outlives the
tool call that created it: `bash` creates it, `read_bash` appends to it,
`stop_bash` cancels it, `list_bash` enumerates it. GCABB therefore keys
terminal state by `shellId` and appends later tool-call output to the existing
terminal instead of creating a second one.

## Runtime discovery

Tool presence is discovered rather than hardcoded, via the SDK's `tools.list`
RPC:

```text
tools.list  ->  ToolList { tools: [ Tool { name, description, instructions,
                                           namespacedName, parameters } ] }
```

`ToolsListRequest` takes an optional model, because the returned list reflects
model-specific overrides. The RPC is marked experimental in the SDK, so the SDK
and CLI versions are pinned together.

MCP tools are distinguished by `namespacedName` (`server/tool`); built-ins have
no namespace. `CapabilityReport::from_catalog` maps the discovered catalog onto
the capabilities the self-hosting loop needs, so a runtime that stops
registering `bash` surfaces as an unavailable capability rather than as an
unexplained model failure.

## Client mode

`ClientOptions::mode` is pinned to `ClientMode::CopilotCli`. `ClientMode::Empty`
disables ambient CLI behavior and strips the built-in file, search, and shell
tools that the self-hosting loop depends on. Because that failure would appear
at model time rather than at configuration time, the mode is set explicitly in
`CopilotProvider::start` rather than left to the default.

## Events GCABB projects

| Event | Fields consumed |
| --- | --- |
| `tool.execution_start` | `toolCallId`, `toolName`, `arguments`, `mcpServerName`, `shellToolInfo.displayCommand`, `shellToolInfo.possiblePaths` |
| `tool.execution_partial_result` | `toolCallId`, `partialOutput` |
| `tool.execution_complete` | `toolCallId`, `success`, `error.code`, `error.message`, `result.detailedContent`, `result.contents[type=shell_exit]` |

`shell_exit` content carries `shellId`, `exitCode`, `cwd`, `outputPreview`, and
`outputTruncated`. The preview is only used when nothing streamed, so it never
duplicates output the UI already displayed.

## Session filesystem interception

The SDK exposes `SessionFsProvider`, which routes all per-session filesystem
operations (`readFile`, `writeFile`, `appendFile`, `exists`, `stat`, `mkdir`,
`readdir`, `readdirWithTypes`, `rm`, `rename`, plus SQLite) through host code.
Phase 3 does not use it: setting the session working directory is sufficient to
satisfy the worktree requirement, and intercepting file I/O would make GCABB
responsible for semantics the runtime already implements correctly. It remains
the natural mechanism for a future sandboxing or remote-worktree feature.
