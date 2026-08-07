# Accessibility

GCABB pins GPUI and `gpui_platform` to Zed commit
`027cf0def75e5c027504f402a6a6c0dcac11f178`. This revision uses AccessKit to
project GPUI elements into each operating system's native accessibility API.
Rust 1.95 is required by that upstream revision.

Current GPUI uses Zed's GPL-licensed `ztracing` package only for the
`instrument` attribute in the dependency path GCABB compiles. GCABB patches that
package with the local MIT-compatible `crates/ztracing-compat` shim, which
re-exports `tracing::instrument`. This keeps GPL implementation code out of the
binary while retaining the pinned AccessKit APIs.

The application shell and session surface expose:

- An application root and labelled primary/projects/sessions navigation region.
- Project and session lists with selected state.
- Headings, status messages, and alerts.
- A conversation list with attributed messages.
- A labelled text input with current value and placeholder.
- A sidebar disclosure control with expanded state.
- SDK-backed mode and model selectors with expanded state and labelled option
  lists; reasoning effort appears only when the selected model reports supported
  levels.
- Submit, session, and interaction buttons with press actions.
- Permission and input dialogs.
- Tab and Shift-Tab focus traversal.

Every semantic node has a stable GPUI element ID and role. Interactive nodes also
have an external accessibility identifier, label, focusability, and an accessible
action. Custom-painted controls must provide these explicitly; drawing text or
registering a mouse handler does not create an accessibility node.

## macOS smoke check

The atlas-aligned application was inspected through `AXUIElement` on 2026-08-07.
macOS reported named application, navigation, list, image, status, text-field,
and button nodes. `AXPress` opened the mode, model, and effort option lists,
selected Plan, GPT-5.6 Sol, and High, collapsed and reopened the sidebar, and
verified the composer without submitting a prompt.

New UI components should receive a lightweight AX/AccessKit interaction contract
covering role, label, value or state, stable identity, keyboard focus, and
supported actions.
