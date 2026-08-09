use std::{
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, ClickEvent, ClipboardEntry, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, IntoElement, LayoutId, PaintQuad, Pixels, Render, Role, ShapedLine,
    SharedString, Style, Task, TextAlign, TextRun, UTF16Selection, Window, actions, div, fill,
    point, prelude::*, px, relative, rgb, size,
};

actions!(text_input, [Backspace, Submit, Paste, SelectAll]);

const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);
const CURSOR_BLINK_TICK: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub struct InputSubmitted {
    pub text: String,
}

/// Images pasted into the input.
///
/// The input owns no attachment state, so it reports the paste and lets the
/// composer decide what to do with it.
#[derive(Clone, Debug)]
pub struct ImagesPasted {
    pub images: Vec<PastedImage>,
}

/// One image lifted off the clipboard.
#[derive(Clone, Debug)]
pub struct PastedImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

pub struct TextInput {
    accessibility_id: SharedString,
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    cursor_visible: bool,
    cursor_blink_started_at: Instant,
    cursor_blink_enabled: bool,
    cursor_blink_task: Option<Task<()>>,
}

impl TextInput {
    #[must_use]
    pub fn new(
        cx: &mut Context<Self>,
        accessibility_id: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
    ) -> Self {
        Self {
            accessibility_id: accessibility_id.into(),
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            marked_range: None,
            last_layout: None,
            cursor_visible: true,
            cursor_blink_started_at: Instant::now(),
            cursor_blink_enabled: false,
            cursor_blink_task: None,
        }
    }

    /// Starts or stops the blink timer so it only runs while the caret is
    /// actually on screen. The caret is painted only when this input holds focus
    /// in an active window, so blinking outside that state repaints the whole
    /// window without changing a pixel.
    fn sync_cursor_blink(&mut self, window: &Window, cx: &mut Context<Self>) {
        let enabled = self.focus_handle.is_focused(window) && window.is_window_active();
        if enabled == self.cursor_blink_enabled {
            return;
        }
        self.cursor_blink_enabled = enabled;
        if enabled {
            self.reset_cursor_blink();
            self.cursor_blink_task = Some(Self::spawn_cursor_blink(cx));
        } else {
            // Dropping the task cancels it at its next await point.
            self.cursor_blink_task = None;
            self.cursor_visible = false;
        }
    }

    fn spawn_cursor_blink(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |input, cx| {
            loop {
                cx.background_executor().timer(CURSOR_BLINK_TICK).await;
                if input
                    .update(cx, |input, cx| {
                        if !input.cursor_blink_enabled {
                            return;
                        }
                        let elapsed = input.cursor_blink_started_at.elapsed();
                        let visible = (elapsed.as_millis() / CURSOR_BLINK_INTERVAL.as_millis())
                            .is_multiple_of(2);
                        if visible != input.cursor_visible {
                            input.cursor_visible = visible;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    #[must_use]
    pub fn value(&self) -> String {
        self.content.to_string()
    }

    pub fn set_value(&mut self, value: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = value.into();
        self.selected_range = self.content.len()..self.content.len();
        self.reset_cursor_blink();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_value("", cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() && self.selected_range.start > 0 {
            let previous = self.content[..self.selected_range.start]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index);
            self.selected_range = previous..self.selected_range.start;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.content.trim().to_owned();
        if !text.is_empty() {
            cx.emit(InputSubmitted { text });
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };

        // A screenshot is the usual reason to paste into a composer, so images
        // are reported rather than silently discarded. One clipboard item can
        // hold both text and an image, so both are handled.
        let images: Vec<PastedImage> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(PastedImage {
                    bytes: image.bytes.clone(),
                    mime_type: image.format.mime_type().to_owned(),
                }),
                _ => None,
            })
            .collect();
        if !images.is_empty() {
            cx.emit(ImagesPasted { images });
        }

        if let Some(text) = item.text()
            && !text.is_empty()
        {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.reset_cursor_blink();
        cx.notify();
    }

    fn on_click(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.activate(true);
        window.activate_window();
        window.focus(&self.focus_handle, cx);
        let focus_handle = self.focus_handle.clone();
        cx.on_next_frame(window, move |_, window, cx| {
            window.focus(&focus_handle, cx);
        });
        self.selected_range = self.content.len()..self.content.len();
        self.reset_cursor_blink();
        cx.notify();
    }

    fn reset_cursor_blink(&mut self) {
        self.cursor_visible = true;
        self.cursor_blink_started_at = Instant::now();
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for character in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += character.len_utf16();
            utf8_offset += character.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for character in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += character.len_utf8();
            utf16_offset += character.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}

impl EventEmitter<InputSubmitted> for TextInput {}
impl EventEmitter<ImagesPasted> for TextInput {}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.content = format!(
            "{}{}{}",
            &self.content[..range.start],
            new_text,
            &self.content[range.end..]
        )
        .into();
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.marked_range = None;
        self.reset_cursor_blink();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text_in_range(range_utf16, new_text, window, cx);
        if !new_text.is_empty() {
            let end = self.selected_range.end;
            self.marked_range = Some(end - new_text.len()..end);
        }
        if let Some(range) = new_selected_range_utf16 {
            self.selected_range = self.range_from_utf16(&range);
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(bounds.left() + line.x_for_index(range.start), bounds.top()),
            point(bounds.left() + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line = self.last_layout.as_ref()?;
        line.index_for_x(point.x)
            .map(|index| self.offset_to_utf16(index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let display_text = if content.is_empty() {
            input.placeholder.clone()
        } else {
            content
        };
        let mut text_color = window.text_style().color;
        if input.content.is_empty() {
            text_color.a *= 0.45;
        }
        let style = window.text_style();
        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &[run], None);
        let cursor_x = line.x_for_index(input.selected_range.end);
        let cursor = (input.focus_handle.is_focused(window) && input.cursor_visible).then(|| {
            fill(
                Bounds::new(
                    point(bounds.left() + cursor_x, bounds.top()),
                    size(px(1.5), bounds.size.height),
                ),
                rgb(0x6b_a6ff),
            )
        });
        PrepaintState {
            line: Some(line),
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let line = prepaint.line.take().expect("prepaint creates a line");
        line.paint(
            bounds.origin,
            window.line_height(),
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .expect("text paint should succeed");
        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_cursor_blink(window, cx);
        div()
            .id(self.accessibility_id.clone())
            .accessibility_id(self.accessibility_id.clone())
            .role(Role::TextInput)
            .aria_label(self.placeholder.clone())
            .aria_placeholder(self.placeholder.clone())
            .aria_value(self.content.clone())
            .focusable()
            .tab_stop(true)
            .focus_visible(|style| style.border_1().border_color(rgb(0x58_a6ff)))
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_click(cx.listener(Self::on_click))
            .w_full()
            .p_3()
            .line_height(px(22.))
            .text_size(px(15.))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub fn bind_text_input_keys(cx: &mut App) {
    use gpui::KeyBinding;

    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("enter", Submit, Some("TextInput")),
        // `secondary` is cmd on macOS and ctrl everywhere else. Binding cmd
        // directly meant paste was unreachable on Linux and Windows.
        KeyBinding::new("secondary-v", Paste, Some("TextInput")),
        KeyBinding::new("secondary-a", SelectAll, Some("TextInput")),
    ]);
}
