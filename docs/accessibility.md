# Accessibility

GCABB pins GPUI and `gpui_platform` to upstream revision
`027cf0def75e5c027504f402a6a6c0dcac11f178`. This revision uses AccessKit to
project GPUI elements into each operating system's native accessibility API.
Rust 1.95 is required by that upstream revision.

The upstream dependency graph uses the GPL-licensed `ztracing` package only for
the `instrument` attribute in the path GCABB compiles. GCABB patches that package
with the local MIT-compatible `crates/ztracing-compat` shim, which re-exports
`tracing::instrument`. This keeps GPL implementation code out of the binary
while retaining the pinned AccessKit APIs.

The initial session shell exposes:

- An application root and projects/sessions navigation region.
- Project and session lists with selected state.
- Headings, status messages, and alerts.
- A conversation list with attributed messages.
- A labelled text input with current value and placeholder.
- Mode, model, and effort comboboxes with values, expanded state, selectable
  options, and press actions.
- Sidebar, navigation, session, submit, lifecycle, and interaction controls with
  names, selection state where applicable, keyboard focus, and press actions.
- Attachment pickers, removable attachment chips, image-preview buttons, and a
  labelled close action for the image-preview dialog.
- Permission and input dialogs.
- Tab and Shift-Tab focus traversal.

Every semantic node has a stable GPUI element ID and role. Interactive nodes also
have an external accessibility identifier, label, focusability, and an accessible
action. Custom-painted controls must provide these explicitly; drawing text or
registering a mouse handler does not create an accessibility node.

## macOS smoke check

The upgraded application was inspected through `AXUIElement` on 2026-08-07.
macOS reported named navigation, list, heading, status, text-field, and button
nodes. `AXPress` changed mode from interactive to plan and effort from medium to
high. Pressing the text field through AX focused it, and typed text was reflected
in `AXValue` without submitting a prompt.

The smoke validator checks the required roles and identifiers, presses the
composer from AX, verifies that it receives focus even when GCABB starts in the
background, and exercises the mode control:

```sh
cargo build -p gcabb-desktop
GCABB_DATA_DIR="$(mktemp -d)" target/debug/gcabb-desktop &
app_pid=$!
swift scripts/validate_macos_accessibility.swift "$app_pid"
kill "$app_pid"
```

The terminal or host application running the validator must have permission in
**System Settings > Privacy & Security > Accessibility**.

New UI components should receive a lightweight AX/AccessKit interaction contract
covering role, label, value or state, stable identity, keyboard focus, and
supported actions.

Unavailable placeholders (My work, Automations, Search, Settings, and
back/forward glyphs) are intentionally omitted from the accessibility tree
because they do not perform an action. The pointer-only popup backdrop is also
presentational; selector triggers and Escape provide accessible ways to close
the popup.
