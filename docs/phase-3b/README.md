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

Large transcripts now use GPUI's variable-height `ListState` rather than an
overflow container holding every row. It lays out only the viewport plus 720 px
of overdraw, preserves the top row's pixel anchor when streaming content changes
height, follows the tail only while already following it, and stabilizes the
custom scrollbar against the geometry that was actually painted. Stable message
and tool ids back each row.

The mixed timeline is incrementally indexed as transcript and root-tool suffixes
arrive. Rendering no longer sorts the complete history or scans every invocation
for subagent children. Completed markdown documents are cached; streaming
messages remain uncached so partial syntax is always current. The deterministic
10,000-message UI fixture asserts that no more than 64 rows or markdown documents
are instantiated for one frame.

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

Paste initially appeared not to work at all. The cause was not the image
handling but the keybinding: paste was bound to `cmd-v`, so on Linux and
Windows the action never fired and neither text nor images were pasted. It is
now bound to `secondary-v`, which GPUI resolves to cmd on macOS and ctrl
elsewhere.

An attachment is part of what was asked, so the transcript records it. The
record comes from the `attachments` the runtime echoes back on `user.message`
rather than from composer state, so what is shown is what the model actually
received. A message carrying only a screenshot is kept rather than discarded as
empty, since the screenshot *is* the message.

Pasted images are written to disk under the app data directory and sent as
files, the same as a picked or dropped image. Sending the bytes inline seemed
reasonable -- there is no file, so send what there is -- but the runtime echoes
an attachment back *in the form it was sent*. A blob comes back a blob, with no
path, so nothing could load the picture afterwards. Every one of those base64
payloads was also persisted in the event log and copied into every subsequent
snapshot.

They are not written into the session worktree: files there would appear in the
changes view and could be committed by accident. The runtime references an
attached file in place rather than copying it, so the file has to outlive the
composer for the transcript to still show the picture.

Clicking an image chip opens it full size, in the composer and in the
transcript alike. A chip that shows only a filename is a poor record of what
was discussed, since the picture was the point.

The preview takes focus when it opens. That is not decoration: a click leaves
focus wherever it was, and the Escape binding is dispatched through the focus
path, so without it Escape was dead precisely when a user would reach for it.
The test that caught this clicks a chip and then presses Escape. An earlier
test that opened the preview programmatically and pressed Escape passed against
the broken build, because opening it by hand left focus somewhere that happened
to work.

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

## Storage

A local database reached 499 MB while holding 5.5 MB of events. Two faults
compounded.

`SessionSnapshot` carried the whole event log in an `activities` field, which
duplicated the `domain_events` table -- the actual record of what happened --
and made up about 99% of every snapshot written. Nothing read it: the interface
never touched it, and the one consumer, history reconciliation, is better served
by asking the event log directly. The field is gone.

Every snapshot was also kept as its own row, 176 of them for one session, while
only the newest is ever read. Writing a snapshot now replaces the previous one.
Together these made storage grow with the square of a session's length.

Opening an older database prunes the superseded rows and vacuums. On the
database above that returned 486 MB.

Tool and terminal output also no longer lives in serialized snapshots. It is
persisted once as ordered chunks with bounded stream metadata in the snapshot,
then restored as only the newest 64 chunks. Earlier chunks remain in SQLite and
the tool card prepends them in 64-chunk pages on request through the ordered range
API. Existing event logs are backfilled during schema migration, preserving
output that older snapshots had trimmed. Missing chunks fail the requested page
explicitly rather than presenting incomplete output as complete.

## Deleting a session

Deleting used to leave several things behind. Events and snapshots cascaded
correctly, and worktrees were already reclaimed with care, but diagnostics rows
had no foreign key to cascade from, the runtime's own state directory was never
touched -- 114 MB across 69 directories on one machine -- and freed database
pages were never returned to the filesystem.

All of these are now cleaned up, and the directories they live in are passed in
rather than discovered, so nothing is ever deleted from a location the caller
did not name. Two guards matter more than the cleanup itself: attachments are
removed only from inside the managed directory, so a picture attached from the
user's own folder is never deleted; and the runtime state directory is resolved
from a session id that cannot contain path separators, so it cannot name a
directory outside its root.

## Verifying the tests

Every fix here carries a regression test, and each test was run against the
broken code before being accepted. Two tests written during Phase 3a were inert
— they passed with the bug present — which is how that discipline started.

The scrollbar geometry bug is the standing counterexample: it was a consistency
invariant between two pieces of code, and no test of either piece alone would
have found it. Where a defect is structural, the fix should be structural too.

## Known gaps

- Detail blocks render in a proportional font, so commands, diffs, and columnar
  output do not align. Phase 5 introduces a monospace font and closes this.
- Assistant replies are markdown and are shown as their source, so emphasis,
  headings, lists, and code blocks read as literal punctuation. Phase 5 renders
  them. Zed's markdown crate is GPL-3.0-or-later and cannot be used in an MIT
  project, so the renderer will be GCABB's own over an MIT parser.
- Subagent nesting is exercised only with synthetic `subagent.started` events.
  The field shape came from Phase 0 notes and has not been observed live.
- Chats share one working directory, so concurrent chats can collide.
- Attachment files are removed only when they sit inside the managed
  attachments directory, so a picture attached from the user's own folder
  survives. Nothing prunes attachments of sessions deleted by older builds.
- A true per-shell stop, if the runtime ever exposes an RPC for it.
