use std::{
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Render, Role, SharedString, StyledText, Task, TextLayout,
    TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px,
    rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Submit,
        InsertNewline,
        Paste,
        Copy,
        Cut,
        SelectAll
    ]
);

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
    selection_anchor: usize,
    marked_range: Option<Range<usize>>,
    last_layout: Option<TextLayout>,
    is_selecting: bool,
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
            selection_anchor: 0,
            marked_range: None,
            last_layout: None,
            is_selecting: false,
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
        self.selection_anchor = self.content.len();
        self.marked_range = None;
        self.reset_cursor_blink();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_value("", cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let previous = self.previous_boundary(self.cursor_offset());
            if previous == self.cursor_offset() {
                window.play_system_bell();
                return;
            }
            self.select_to(previous, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if next == self.cursor_offset() {
                window.play_system_bell();
                return;
            }
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor_offset())
        } else {
            self.selected_range.start
        };
        self.move_to(offset, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.selected_range.is_empty() {
            self.next_boundary(self.cursor_offset())
        } else {
            self.selected_range.end
        };
        self.move_to(offset, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.vertical_offset(-1.), cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.vertical_offset(1.), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.vertical_offset(-1.), cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.vertical_offset(1.), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_range(self.cursor_offset()).start, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_range(self.cursor_offset()).end, cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_range(self.cursor_offset()).start, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_range(self.cursor_offset()).end, cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.content.trim().to_owned();
        if !text.is_empty() {
            cx.emit(InputSubmitted { text });
        }
    }

    fn insert_newline(&mut self, _: &InsertNewline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
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

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_owned(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_anchor = 0;
        self.reset_cursor_blink();
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.activate(true);
        window.activate_window();
        window.focus(&self.focus_handle, cx);
        self.is_selecting = true;
        let offset = self.index_for_position(event.position);
        match event.click_count {
            2 => {
                self.selected_range = self.word_range(offset);
                self.selection_anchor = self.selected_range.start;
                self.reset_cursor_blink();
                cx.notify();
            }
            3.. => {
                self.selected_range = self.line_range(offset);
                self.selection_anchor = self.selected_range.start;
                self.reset_cursor_blink();
                cx.notify();
            }
            _ if event.modifiers.shift => self.select_to(offset, cx),
            _ => self.move_to(offset, cx),
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_position(event.position), cx);
        }
    }

    fn reset_cursor_blink(&mut self) {
        self.cursor_visible = true;
        self.cursor_blink_started_at = Instant::now();
    }

    fn cursor_offset(&self) -> usize {
        if !self.selected_range.is_empty() && self.selection_anchor == self.selected_range.end {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_anchor = offset;
        self.reset_cursor_blink();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = self.selection_anchor.min(offset)..self.selection_anchor.max(offset);
        self.reset_cursor_blink();
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn index_for_position(&self, position: gpui::Point<Pixels>) -> usize {
        self.last_layout
            .as_ref()
            .map_or(self.content.len(), |layout| {
                layout
                    .index_for_position(position)
                    .unwrap_or_else(|index| index)
                    .min(self.content.len())
            })
    }

    fn vertical_offset(&self, direction: f32) -> usize {
        let Some(layout) = &self.last_layout else {
            return if direction.is_sign_negative() {
                0
            } else {
                self.content.len()
            };
        };
        let Some(position) = layout.position_for_index(self.cursor_offset()) else {
            return self.cursor_offset();
        };
        layout
            .index_for_position(point(
                position.x,
                position.y + layout.line_height() * direction,
            ))
            .unwrap_or_else(|index| index)
            .min(self.content.len())
    }

    fn word_range(&self, offset: usize) -> Range<usize> {
        word_range(&self.content, offset)
    }

    fn line_range(&self, offset: usize) -> Range<usize> {
        line_range(&self.content, offset)
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        offset_from_utf16(&self.content, offset)
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

fn word_range(content: &str, offset: usize) -> Range<usize> {
    if content.is_empty() {
        return 0..0;
    }
    let offset = offset.min(content.len());
    let probe = if offset == content.len() {
        content
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index)
    } else {
        offset
    };
    content
        .split_word_bound_indices()
        .find_map(|(start, word)| {
            let end = start + word.len();
            (start <= probe && probe < end).then_some(start..end)
        })
        .unwrap_or(offset..offset)
}

fn offset_from_utf16(content: &str, offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for character in content.chars() {
        if utf16_count >= offset {
            break;
        }
        utf16_count += character.len_utf16();
        utf8_offset += character.len_utf8();
    }
    utf8_offset
}

fn line_range(content: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(content.len());
    let start = content[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = content[offset..]
        .find('\n')
        .map_or(content.len(), |index| offset + index);
    start..end
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
            reversed: !self.selected_range.is_empty()
                && self.selection_anchor == self.selected_range.end,
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

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_range.take().is_some() {
            cx.notify();
        }
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
        self.selection_anchor = cursor;
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
        let replacement_start = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range).start)
            .or_else(|| self.marked_range.as_ref().map(|range| range.start))
            .unwrap_or(self.selected_range.start);
        self.replace_text_in_range(range_utf16, new_text, window, cx);
        if !new_text.is_empty() {
            self.marked_range = Some(replacement_start..replacement_start + new_text.len());
        }
        if let Some(range) = new_selected_range_utf16 {
            self.selected_range = replacement_start + offset_from_utf16(new_text, range.start)
                ..replacement_start + offset_from_utf16(new_text, range.end);
            self.selection_anchor = self.selected_range.start;
        }
        self.reset_cursor_blink();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let position = layout.position_for_index(range.end)?;
        Some(Bounds::new(position, size(px(1.), layout.line_height())))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        let index = layout
            .index_for_position(point)
            .unwrap_or_else(|index| index);
        Some(self.offset_to_utf16(index.min(self.content.len())))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_range = self.range_from_utf16(&range_utf16);
        self.selection_anchor = self.selected_range.start;
        self.reset_cursor_blink();
        cx.notify();
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.content.len()))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    text: StyledText,
    cursor: Option<PaintQuad>,
    layout: TextLayout,
}

impl Element for TextElement {
    type RequestLayoutState = StyledText;
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.read(cx);
        let display_text = if input.content.is_empty() {
            input.placeholder.clone()
        } else {
            input.content.clone()
        };
        let style = window.text_style();
        let mut boundaries = vec![0, display_text.len()];
        if !input.content.is_empty() {
            boundaries.extend([input.selected_range.start, input.selected_range.end]);
            if let Some(marked) = &input.marked_range {
                boundaries.extend([marked.start, marked.end]);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();

        let mut runs = Vec::new();
        for pair in boundaries.windows(2) {
            let range = pair[0]..pair[1];
            if range.is_empty() {
                continue;
            }
            let selected = !input.selected_range.is_empty()
                && range.start >= input.selected_range.start
                && range.end <= input.selected_range.end;
            let marked = input
                .marked_range
                .as_ref()
                .is_some_and(|marked| range.start >= marked.start && range.end <= marked.end);
            let mut color = style.color;
            if input.content.is_empty() {
                color.a *= 0.45;
            }
            runs.push(TextRun {
                len: range.len(),
                font: style.font(),
                color,
                background_color: selected.then_some(rgba(0x2f81_f733).into()),
                underline: marked.then_some(UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.),
                    wavy: false,
                }),
                strikethrough: None,
            });
        }

        let mut text = StyledText::new(display_text).with_runs(runs);
        let (layout_id, ()) = text.request_layout(id, inspector_id, window, cx);
        (layout_id, text)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        text: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let mut state = ();
        text.prepaint(None, None, bounds, &mut state, window, cx);
        let layout = text.layout().clone();
        let input = self.input.read(cx);
        let cursor_position = if input.content.is_empty() {
            Some(bounds.origin)
        } else {
            layout.position_for_index(input.cursor_offset())
        };
        let cursor = (input.focus_handle.is_focused(window)
            && input.cursor_visible
            && input.selected_range.is_empty())
        .then(|| {
            fill(
                Bounds::new(
                    cursor_position.unwrap_or(bounds.origin),
                    size(px(1.5), layout.line_height()),
                ),
                rgb(0x6b_a6ff),
            )
        });
        PrepaintState {
            text: std::mem::replace(text, StyledText::new("")),
            cursor,
            layout,
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
        let mut state = ();
        prepaint
            .text
            .paint(None, None, bounds, &mut state, &mut (), window, cx);
        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(prepaint.layout.clone());
        });
    }
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_cursor_blink(window, cx);
        let debug_selector = self.accessibility_id.to_string();
        div()
            .id(self.accessibility_id.clone())
            .debug_selector(move || debug_selector.clone())
            .accessibility_id(self.accessibility_id.clone())
            .role(Role::TextInput)
            .aria_label(self.placeholder.clone())
            .aria_placeholder(self.placeholder.clone())
            .aria_value(self.content.clone())
            .focusable()
            .tab_stop(true)
            .focus_visible(|style| style.border_1().border_color(rgb(0x58_a6ff)))
            .flex()
            .flex_col()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::insert_newline))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::select_all))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
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
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("up", Up, Some("TextInput")),
        KeyBinding::new("down", Down, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("shift-up", SelectUp, Some("TextInput")),
        KeyBinding::new("shift-down", SelectDown, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("shift-home", SelectHome, Some("TextInput")),
        KeyBinding::new("shift-end", SelectEnd, Some("TextInput")),
        KeyBinding::new("enter", Submit, Some("TextInput")),
        KeyBinding::new("shift-enter", InsertNewline, Some("TextInput")),
        // `secondary` is cmd on macOS and ctrl everywhere else. Binding cmd
        // directly meant paste was unreachable on Linux and Windows.
        KeyBinding::new("secondary-v", Paste, Some("TextInput")),
        KeyBinding::new("secondary-c", Copy, Some("TextInput")),
        KeyBinding::new("secondary-x", Cut, Some("TextInput")),
        KeyBinding::new("secondary-a", SelectAll, Some("TextInput")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::{line_range, offset_from_utf16, word_range};

    #[test]
    fn word_selection_uses_unicode_boundaries() {
        let content = "hello 世界";
        assert_eq!(word_range(content, 1), 0..5);
        assert_eq!(word_range(content, 7), 6..9);
    }

    #[test]
    fn line_selection_stays_within_hard_line_breaks() {
        let content = "first\nsecond\nthird";
        assert_eq!(line_range(content, 8), 6..12);
        assert_eq!(line_range(content, content.len()), 13..18);
    }

    #[test]
    fn composition_offsets_convert_from_utf16() {
        assert_eq!(offset_from_utf16("a😀b", 3), 5);
    }
}
