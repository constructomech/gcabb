# Runtime contract

GCABB depends on two runtime surfaces the SDK marks **experimental**:
`session.queue.*`, and the per-session SQL surface reached through a hosted
session filesystem. Experimental means their behaviour can change under a
version bump with no corresponding change here.

The risk is not that they disappear loudly. It is that they change quietly:
queued items stop keeping their order, an identifier stops addressing the item
it named, or a batch that used to apply silently stops applying. Each of those
surfaces in the app as odd behaviour rather than as a failure.

`crates/session-manager/tests/runtime_contract.rs` pins those properties
against a live runtime:

```sh
cargo test -p session-manager --test runtime_contract -- --ignored
```

Run it whenever either pinned version changes. Both are pinned deliberately:

- `github-copilot-sdk = "=1.0.9"` in the workspace manifest.
- The CLI binary the SDK resolves, which updates independently of the crate.

## What is pinned, and why

| Property | Why it matters |
| --- | --- |
| The runtime offers a queue | Its absence is survivable but silently changes how prompts are delivered |
| Queued items keep insertion order | GCABB's ordering is the queue's whole purpose |
| A reported id still addresses its item | Removal and reordering address items by id |
| Immediate delivery interrupts a running turn | Steering is the one capability GCABB cannot give up |
| A hosted database receives the agent's SQL | Without it the agent's task list is not shared |

## Degradation, not breakage

Nothing here blocks a session. Both capabilities are reported through
`CapabilityReport` so a developer can see which one they have:

- Losing the queue surface drops GCABB to `SendOnIdle`, delivering queued
  prompts through `session.send` as the session becomes idle. GCABB's own
  queue is unaffected, because it never lived in the runtime.
- Losing the hosted database makes the agent's task list read-only rather
  than unavailable.

That is why the contract tests assert the capability is *present* rather than
that the app still works. The fallbacks have their own deterministic coverage
in `queue_transport.rs` and `queue_session.rs`.

## Behaviour found by running these

Recorded rather than worked around silently:

- The runtime's `exec` queries carry **multi-statement** batches, including
  the one that creates `todos` and `todo_deps`. A prepared-statement path
  accepts only the first statement, so the tables are never created and the
  agent's `sql` tool fails while looking like the agent declined to use it.
  `SqliteStore::exec` handles batches; `write` keeps the prepared path for its
  bindings.
- Runtime queue identifiers are insertion ordinals that restart per session,
  so they are recorded separately from GCABB's own ids and cleared whenever a
  session ends.
- The runtime's prompt queue is memory-only. It is absent from `events.jsonl`
  and `session.db`, and a disconnect loses it, which is why GCABB owns the
  durable queue and treats the runtime's as a projection.
