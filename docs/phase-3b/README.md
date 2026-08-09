# Phase 3b: Self-Hosting Parity

Phase 3a proved GCABB could *perform* the self-hosting loop. Using it to build
itself exposed a different problem: the work was not observable, and the
feedback channel a user-interface project depends on was missing.

Every item here came from running a real development session against GCABB's
own repository and asking which parts of that session GCABB could not have
supported.

## Showing the work

A session used to display prose and terminals while the actual work — reads,
searches, edits and the diffs they produced — happened invisibly. Tool activity
was already projected, correlated, and tested; nothing rendered it.

Messages and tool calls are now interleaved into one timeline ordered by event
sequence, so the transcript reads in the order things happened rather than
splitting narration from action. Each entry is a one-line header plus a bounded
detail block, because a session that runs hundreds of commands must stay
scannable. Subagent work nests under the tool call that spawned it, using the
`agentId` correlation the projection already carried; delegated work previously
appeared as an unexplained pause.

### Scrolling

Scrolling is listed here because it took four attempts and the failures are
instructive.

GPUI has no scrollbar widget, so the track and thumb are drawn by hand. The
persistent bug was that the thumb was *drawn* from fractions of the track with
one minimum size, and *hit-tested* from pixels against the viewport with a
different minimum. The two geometries diverged by more than the height of the
thumb, so pressing the visible thumb was classified as pressing bare track.

No behavioral test caught this, and in hindsight none of the ones written could
have: each checked drawing or hit-testing in isolation, and the defect was the
relationship between them. Both now read from a single `scrollbar_geometry`,
which makes the invariant structural rather than something tests must police.

A scroll gesture also used to affect both the pane under the pointer and the
transcript behind it. Nested scrollables now stop propagation, so a gesture
moves exactly the pane it is aimed at.

## Attachments

Screenshots are how interface defects actually get reported. Both composers had
a `+` control that promised attachments and delivered nothing, so the only way
to show this app a picture of itself was to describe the picture in words.

Chosen files become `Attachment::File` values carrying a path. The runtime
opens the file itself, so a large screenshot never crosses the RPC boundary as
base64.

Attachments belong to the one prompt they were staged on: submitting takes them
rather than copying them, so an unrelated follow-up question does not silently
resend a screenshot and confuse the model about what is being asked. An
attachment with no text is a complete message, since a screenshot frequently
says everything the user wants to say.

Three gestures stage an attachment, because a file picker is the least likely
of them to be reached for. Pasting was the notable omission: the input's paste
handler read only `item.text()`, so a pasted screenshot did nothing at all and
gave no sign it had been seen. Dropping a file did nothing either, since no
`on_drop` handler existed anywhere in the app.

A pasted image has no path to reference, so `PromptAttachment` is an enum: a
picked or dropped file travels as a path, and pasted bytes travel as an
`Attachment::Blob`. Making it an enum rather than an optional-bytes field keeps
the impossible states unrepresentable — an attachment is one or the other, never
both and never neither.

Identity distinguishes attachments for de-duplication, and the two cases differ
on purpose. Picking or dropping the same file twice is one attachment, because
it is one file. Pasting twice is two attachments, because someone who pastes
twice meant to attach two images — even when the bytes are identical.

## Per-shell stop: revised

Phase 3b asked for a control to stop one runaway command without cancelling the
session, on the stated premise that "`stop_bash` exists in the runtime."

That premise conflated two different surfaces. `stop_bash` is a tool the
*model* calls; it is not a request a *client* can make. The SDK's session API
exposes exactly one form of interruption, `abort`, and it applies to the whole
turn. Reading the client's public API and probing the CLI for an equivalent RPC
turned up nothing. The control cannot be built as specified.

The investigation did find a real defect. Aborting a turn tears down the shells
that turn started, but the runtime sends no completion event for them, so a
background shell kept its "running" badge for the rest of the session. That is
worse than having no stop control: it misrepresents what the machine is doing.
Abort now settles every still-running terminal as cancelled. Shells that already
reported an exit keep it, because a cancellation should not rewrite history for
work that finished on its own.

## Capability reporting

A live session's own capability report claimed "No file editing tool is
registered by the runtime" while that session was editing files.

The classifier knew `create`, `edit`, and `write`. This runtime edits with
`apply_patch` and searches with `rg`. Both fell through to `Other`, so the app
reported it could not do the two things it was visibly doing. This is the same
mistake as the earlier `str_replace_editor` surprise: the set of tool names is
the runtime's business and it changes. Classification stays name-based because
that is all `tools.list` provides, but the names now come from a live catalog
rather than from what seemed likely.

Separately, a chat displayed "2 blocked". One of those was the changes view,
which a chat cannot have because it has no checkout. That is not a failure the
session suffered; it is a thing the session never asked for. Requirements are
now judged against what a session is *for*, so a chat is not blocked by lacking
a repository — but a chat with no shell is still blocked, because that really
is broken.

## Verifying the tests

Every fix here carries a regression test, and each test was run against the
broken code before being accepted. Two tests written during Phase 3a were inert
— they passed with the bug present — which is how that discipline started.

The scrollbar geometry bug is the standing counterexample: it was a consistency
invariant between two pieces of code, and no test of either piece alone would
have found it. Where a defect is structural, the fix should be structural too.

## Known gaps

- Detail blocks render in a proportional font, so commands, diffs, and columnar
  output do not align.
- Subagent nesting is exercised only with synthetic `subagent.started` events.
  The field shape came from Phase 0 notes and has not been observed live.
- Chats share one working directory, so concurrent chats can collide.
- A true per-shell stop, if the runtime ever exposes an RPC for it.
