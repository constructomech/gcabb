# SDK and CLI Compatibility

## Verified combination

| Component | Version |
| --- | --- |
| Rust | 1.94.0 |
| `github-copilot-sdk` | 1.0.9 |
| Embedded Copilot CLI | 1.0.78 |
| SDK protocol | 3 |
| GPUI | 0.2.2 |
| Host | macOS 26.5.2 arm64 |

The exact dependency versions are locked in `Cargo.lock`. The SDK default
`bundled-cli` feature resolved and launched its matching cached executable from
`~/Library/Caches/github-copilot-sdk/cli/1.0.78/copilot`.

## Results

| Check | Result |
| --- | --- |
| CLI resolution and extraction | pass |
| Stdio process start | pass |
| Protocol handshake | pass |
| Session create/disconnect | pass |
| Prompted session resume after disconnect | pass |
| Models RPC | pass |
| Session model/mode RPCs | pass |
| Workspace and plan RPCs | pass |
| Agent, tasks, and fleet RPCs | pass |
| Event subscription and history reconciliation | pass |
| Raw and typed event serialization | pass |
| Callback registration | pass |
| GPUI native compile | pass |

An empty session that had never received a prompt returned `Session not found`
when resumed after disconnect. A prompted session resumed successfully with the
same ID. App recovery should therefore persist and expose resumability only
after the runtime has durable session history.

Subagent lifecycle is consumed through `subagent.started`,
`subagent.completed`, and `subagent.failed` session events. This is the public
SDK surface documented for fleet mode and the same event path used by VS Code's
Copilot CLI session renderer.

GPUI 0.2.2 is the latest published standalone crate and owns
`Application::new()`. Zed main has since introduced `gpui_platform`, but that
crate is not published alongside GPUI 0.2.2. The spike therefore pins the
published API rather than a moving Zed Git revision.

On macOS, GPUI shader compilation requires Xcode's Metal toolchain:

```sh
xcodebuild -downloadComponent MetalToolchain
```

## Upgrade policy

Upgrade the SDK and bundled CLI together through a released SDK crate. For every
candidate pair, rerun the lifecycle, success, failure, cancellation, and fleet
scenarios and compare distinct event types and payload fixtures before changing
the supported range.
