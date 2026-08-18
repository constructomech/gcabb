use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app_model::{
    ChangeStatus, ChangedFile, ContextWindowOption, InteractionKind, InteractionResponse,
    OutputStreamKind, ProjectMetadata, PromptAttachment, SessionKind, SessionLocation,
    SessionMetadata, SessionSnapshot, SessionStatus, TranscriptRole, TranscriptState,
};
use chrono::DateTime;
use copilot_provider::{CopilotProviderFactory, ProviderCompatibility};
use diagnostics::{DiagnosticEvent, DiagnosticsSink, TracingDiagnostics, init_tracing};
use git_service::GitService;
use gpui::prelude::FluentBuilder;
use gpui::{
    Animation, AnimationExt, App, AppContext, Bounds, Context, CursorStyle, Element, ElementId,
    Entity, ExternalPaths, FocusHandle, Focusable, FollowMode, FontStyle, GlobalElementId,
    HighlightStyle, InteractiveElement, IntoElement, KeyBinding, LayoutId, ListAlignment,
    ListState, MouseButton, ParentElement, PathPromptOptions, Render, Role, SharedString,
    StatefulInteractiveElement, StrikethroughStyle, Styled, StyledText, TextLayout,
    TitlebarOptions, UnderlineStyle, Window, WindowBounds, WindowOptions, actions, deferred, div,
    list, px, relative, rgb, size,
};
use session_manager::{
    RestoreFailure, SessionHandle, SessionManager, SessionRoots, WorktreeOutcome,
};
use session_orchestrator::{
    LaunchOrigin, LaunchProgress, LaunchRequest, LaunchTitle, SessionOrchestrator,
};
use storage::Storage;
use tokio::sync::watch;
use ui_components::{ImagesPasted, InputSubmitted, PastedImage, TextInput, bind_text_input_keys};
use updater::install::InstallLayout;
use updater::version::BuildStamp;

mod markdown;
mod settings;
mod syntax;
mod updates;

use markdown::{MarkdownDocument, MarkdownNode, MarkdownTag};
use settings::AppSettings;
use updates::{UpdateRequest, UpdateService, UpdateUi};

const BACKGROUND: u32 = 0x000d_1117;
const SIDEBAR: u32 = 0x0016_1b22;
const PANEL: u32 = 0x000d_1117;
const ELEVATED: u32 = 0x0021_262d;
const SUBTLE: u32 = 0x001b_222c;
const BORDER: u32 = 0x0030_363d;
const PRIMARY: u32 = 0x00f0_f3f6;
const MUTED: u32 = 0x008b_949e;
const GREEN: u32 = 0x003f_b950;
const DATA_DIRECTORY_NAME: &str = "GCABB-data";
const PERSISTENT_DATA_ENTRIES: &[&str] = &[
    "gcabb.db",
    "gcabb.db-shm",
    "gcabb.db-wal",
    settings::SETTINGS_FILE,
    "update-settings.json",
    "attachments",
    "chats",
    "worktrees",
];
const BLUE: u32 = 0x0058_a6ff;
const AMBER: u32 = 0x00d2_9900;
const RED: u32 = 0x00f8_5161;
const COMPACT_WIDTH: f32 = 920.0;
const CONVERSATION_COLUMN_WIDTH: f32 = 820.0;
const UPDATE_POLL_INTERVAL: Duration = Duration::from_hours(6);
const UPDATE_POLL_JITTER: Duration = Duration::from_mins(30);
/// Vertical budget for the detail blocks inside one tool entry.
const ENTRY_DETAIL_BUDGET: f32 = 480.0;
/// Measured thumb geometry for a scrollable region.
#[derive(Clone, Copy)]
struct ScrollbarGeometry {
    track_top: gpui::Pixels,
    track: f32,
    thumb_top: f32,
    thumb: f32,
    usable: f32,
    scrollable: f32,
}

/// A scrollbar drag in progress.
#[derive(Clone, Debug)]
struct ScrollbarDrag {
    /// Which scrollable region is being dragged.
    id: String,
    /// Distance from the top of the thumb to the grab point, so the thumb
    /// keeps its position under the pointer instead of recentring on it.
    grab_offset: f32,
}

/// A glide back to the conversation tail in progress.
#[derive(Clone, Copy, Debug)]
struct ScrollToBottom {
    /// When the glide started. Progress is derived from wall time rather than
    /// step count so the duration holds regardless of frame rate.
    started: Instant,
    /// Scroll offset the glide started from, in pixels below the top.
    from: f32,
}

/// Smallest usable scrollbar thumb.
const MIN_THUMB_HEIGHT: f32 = 24.0;
/// Scrollbar track width; wide enough to aim at without crowding content.
const SCROLLBAR_WIDTH: f32 = 14.0;
/// Thumb width, leaving a small margin inside the track.
const THUMB_WIDTH: f32 = 10.0;
/// Scrollbar id for the conversation itself.
const TRANSCRIPT_SCROLL_ID: &str = "transcript";
/// A wheel listener that claims the gesture only when its region moved.
type ScrollWheelGuard = Box<dyn Fn(&gpui::ScrollWheelEvent, &mut Window, &mut App)>;

/// Scroll region behind the Changes panel's single scrollbar.
const CHANGES_SCROLL_ID: &str = "changes-scroll";
/// Scroll region of the composer's mode, model, and effort menus.
const CONTROL_MENU_SCROLL_ID: &str = "control-menu-scroll";
/// Extra content laid out above and below the viewport to avoid blank flashes
/// during fast trackpad and scrollbar movement.
const TRANSCRIPT_OVERDRAW: f32 = 720.0;
/// Initial height estimate used to size the scrollbar without laying out every
/// row. Measured dynamic heights replace it as rows enter the window.
const TRANSCRIPT_ROW_HEIGHT_HINT: f32 = 96.0;
/// How long the transcript takes to glide back to the tail.
///
/// Long enough that the intervening content is visibly flying past — which is
/// what tells the reader how far the view moved and in which direction — and
/// short enough that it reads as a snap that happens to be traceable rather
/// than a trip to sit through.
const SCROLL_TO_BOTTOM_DURATION: Duration = Duration::from_millis(250);
/// Step interval for the glide, around 120Hz so the motion stays smooth on
/// high refresh rate displays.
const SCROLL_TO_BOTTOM_STEP: Duration = Duration::from_millis(8);
/// Height of the fade that dims the conversation tail while the transcript is
/// parked above it, marking the content the reader has scrolled past.
const TRANSCRIPT_TAIL_FADE: f32 = 112.0;
const MARKDOWN_CACHE_CAPACITY: usize = 128;
const DIFF_CACHE_CAPACITY: usize = 64;
const DIFF_ADDED_BACKGROUND: u32 = 0x2386_3626;
const DIFF_DELETED_BACKGROUND: u32 = 0xf851_6126;
/// Maximum live shell output shaped on the UI thread for one tool row.
const LIVE_OUTPUT_PREVIEW_BYTES: usize = 16 * 1_024;
const LIVE_OUTPUT_PREVIEW_LINES: usize = 64;
/// The command never takes more than a third of that budget, so output — the
/// part worth reading — always gets the majority.
const COMMAND_BLOCK_HEIGHT: f32 = ENTRY_DETAIL_BUDGET / 3.0;

/// Desktop-environment application identifier. On Wayland this becomes the
/// `xdg_toplevel` app ID and on X11 the `WM_CLASS`; both are used to match the
/// installed `com.constructomech.gcabb.desktop` entry that supplies the icon.
const APP_ID: &str = "com.constructomech.gcabb";

actions!(
    gcabb,
    [CopyTranscript, DismissPopup, FocusNext, FocusPrevious]
);

const MARKDOWN_STRONG: u8 = 1;
const MARKDOWN_EMPHASIS: u8 = 1 << 1;
const MARKDOWN_STRIKETHROUGH: u8 = 1 << 2;

#[derive(Clone, Default)]
struct MarkdownInlineStyle {
    marks: u8,
    link: Option<String>,
    code: bool,
    monospace: bool,
}

impl MarkdownInlineStyle {
    fn has(&self, mark: u8) -> bool {
        self.marks & mark != 0
    }
}

#[derive(Default)]
struct MarkdownInlineContent {
    text: String,
    highlights: Vec<(std::ops::Range<usize>, HighlightStyle)>,
    font_family_overrides: Vec<(std::ops::Range<usize>, SharedString)>,
    links: Vec<(std::ops::Range<usize>, String)>,
}

struct DiffDocument {
    source: SharedString,
    lines: Vec<DiffLine>,
    muted: bool,
}

struct DiffLine {
    source: SharedString,
    highlights: Vec<(std::ops::Range<usize>, HighlightStyle)>,
    background: Option<u32>,
}

#[derive(Default)]
struct DiffCache {
    documents: HashMap<String, Arc<DiffDocument>>,
    order: VecDeque<String>,
}

fn diff_lines(
    source: &str,
    highlights: &[(std::ops::Range<usize>, HighlightStyle)],
) -> Vec<DiffLine> {
    let mut line_start = 0;
    let mut highlight_index = 0;

    source
        .split_inclusive('\n')
        .map(|line| {
            let text = line.strip_suffix('\n').unwrap_or(line);
            let line_end = line_start + text.len();
            while highlight_index < highlights.len()
                && highlights[highlight_index].0.end <= line_start
            {
                highlight_index += 1;
            }

            let mut line_highlights = Vec::new();
            let mut index = highlight_index;
            while index < highlights.len() && highlights[index].0.start < line_end {
                let (range, style) = &highlights[index];
                let start = range.start.max(line_start) - line_start;
                let end = range.end.min(line_end) - line_start;
                if start < end {
                    line_highlights.push((start..end, *style));
                }
                index += 1;
            }

            let background = if text.starts_with('+') && !text.starts_with("+++ ") {
                Some(DIFF_ADDED_BACKGROUND)
            } else if text.starts_with('-') && !text.starts_with("--- ") {
                Some(DIFF_DELETED_BACKGROUND)
            } else {
                None
            };
            line_start += line.len();
            DiffLine {
                source: text.to_owned().into(),
                highlights: line_highlights,
                background,
            }
        })
        .collect()
}

#[cfg(test)]
mod diff_line_tests {
    use super::*;

    #[test]
    fn change_backgrounds_belong_to_rows_not_text_runs() {
        let lines = diff_lines("--- a/file\n+++ b/file\n-old\n+new\n context\n", &[]);

        assert_eq!(lines[0].background, None);
        assert_eq!(lines[1].background, None);
        assert_eq!(lines[2].background, Some(DIFF_DELETED_BACKGROUND));
        assert_eq!(lines[3].background, Some(DIFF_ADDED_BACKGROUND));
        assert_eq!(lines[4].background, None);
        assert!(lines.iter().all(|line| line.highlights.is_empty()));
    }
}

impl MarkdownInlineContent {
    fn push(&mut self, text: &str, style: &MarkdownInlineStyle) {
        if text.is_empty() {
            return;
        }

        let range = self.text.len()..self.text.len() + text.len();
        self.text.push_str(text);

        let mut highlight = HighlightStyle::default();
        if style.has(MARKDOWN_STRONG) {
            highlight.font_weight = Some(gpui::FontWeight::BOLD);
        }
        if style.has(MARKDOWN_EMPHASIS) {
            highlight.font_style = Some(FontStyle::Italic);
        }
        if style.has(MARKDOWN_STRIKETHROUGH) {
            highlight.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.),
                ..Default::default()
            });
        }
        if style.monospace {
            self.font_family_overrides
                .push((range.clone(), ".ZedMono".into()));
        }
        if style.code {
            highlight.background_color = Some(rgb(SUBTLE).into());
        }
        if let Some(target) = &style.link {
            highlight.color = Some(rgb(BLUE).into());
            highlight.underline = Some(UnderlineStyle {
                thickness: px(1.),
                ..Default::default()
            });
            self.links.push((range.clone(), target.clone()));
        }
        self.highlights.push((range, highlight));
    }
}

#[derive(Clone)]
struct TranscriptTextBlock {
    order: (u64, usize),
    content: SharedString,
    bounds: Option<Bounds<gpui::Pixels>>,
    layout: Option<TextLayout>,
}

#[derive(Clone)]
struct TranscriptTextEndpoint {
    block_id: String,
    order: (u64, usize),
    index: usize,
}

#[derive(Default)]
struct TranscriptTextSelection {
    message_orders: HashMap<String, u64>,
    blocks: HashMap<String, TranscriptTextBlock>,
    anchor: Option<TranscriptTextEndpoint>,
    focus: Option<TranscriptTextEndpoint>,
    dragging: bool,
}

impl TranscriptTextSelection {
    fn clamp_index(content: &str, index: usize) -> usize {
        let mut index = index.min(content.len());
        while !content.is_char_boundary(index) {
            index -= 1;
        }
        index
    }

    fn register_block(&mut self, block_id: &str, order: (u64, usize), content: SharedString) {
        self.blocks.insert(
            block_id.to_owned(),
            TranscriptTextBlock {
                order,
                content,
                bounds: None,
                layout: None,
            },
        );
        for endpoint in [&mut self.anchor, &mut self.focus] {
            if let Some(endpoint) = endpoint.as_mut()
                && endpoint.block_id == block_id
                && let Some(block) = self.blocks.get(block_id)
            {
                endpoint.order = order;
                endpoint.index = Self::clamp_index(&block.content, endpoint.index);
            }
        }
    }

    fn update_geometry(
        &mut self,
        block_id: &str,
        bounds: Bounds<gpui::Pixels>,
        layout: TextLayout,
    ) {
        if let Some(block) = self.blocks.get_mut(block_id) {
            block.bounds = Some(bounds);
            block.layout = Some(layout);
        }
    }

    fn begin(
        &mut self,
        block_id: String,
        order: (u64, usize),
        content: &SharedString,
        index: usize,
    ) {
        self.register_block(&block_id, order, content.clone());
        let endpoint = TranscriptTextEndpoint {
            block_id,
            order,
            index: Self::clamp_index(content, index),
        };
        self.anchor = Some(endpoint.clone());
        self.focus = Some(endpoint);
        self.dragging = true;
    }

    fn extend(
        &mut self,
        block_id: String,
        order: (u64, usize),
        content: &SharedString,
        index: usize,
    ) {
        if !self.dragging {
            return;
        }
        self.register_block(&block_id, order, content.clone());
        self.focus = Some(TranscriptTextEndpoint {
            block_id,
            order,
            index: Self::clamp_index(content, index),
        });
    }

    fn extend_to_position(&mut self, position: gpui::Point<gpui::Pixels>) {
        if !self.dragging {
            return;
        }
        let destination = self.blocks.iter().find_map(|(block_id, block)| {
            let bounds = block.bounds?;
            if !bounds.contains(&position) {
                return None;
            }
            let layout = block.layout.as_ref()?;
            let index = layout
                .index_for_position(position)
                .unwrap_or_else(|index| index)
                .min(block.content.len());
            Some((block_id.clone(), block.order, block.content.clone(), index))
        });
        if let Some((block_id, order, content, index)) = destination {
            self.extend(block_id, order, &content, index);
        }
    }

    fn ordered_endpoints(&self) -> Option<(&TranscriptTextEndpoint, &TranscriptTextEndpoint)> {
        let anchor = self.anchor.as_ref()?;
        let focus = self.focus.as_ref()?;
        if (anchor.order, anchor.index) <= (focus.order, focus.index) {
            Some((anchor, focus))
        } else {
            Some((focus, anchor))
        }
    }

    fn range_for(&self, block_id: &str, content: &str) -> Option<std::ops::Range<usize>> {
        let block = self.blocks.get(block_id)?;
        let (start, end) = self.ordered_endpoints()?;
        if block.order < start.order || block.order > end.order {
            return None;
        }
        let range = if start.block_id == end.block_id {
            Self::clamp_index(content, start.index)..Self::clamp_index(content, end.index)
        } else if block_id == start.block_id {
            Self::clamp_index(content, start.index)..content.len()
        } else if block_id == end.block_id {
            0..Self::clamp_index(content, end.index)
        } else {
            0..content.len()
        };
        (!range.is_empty()).then_some(range)
    }

    fn is_empty(&self) -> bool {
        self.ordered_endpoints()
            .is_none_or(|(start, end)| start.block_id == end.block_id && start.index == end.index)
    }

    fn selected_text(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut blocks = self
            .blocks
            .iter()
            .filter_map(|(block_id, block)| {
                self.range_for(block_id, &block.content)
                    .map(|range| (block.order, block.content[range].to_owned()))
            })
            .collect::<Vec<_>>();
        blocks.sort_by_key(|(order, _)| *order);
        Some(
            blocks
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

struct SelectableTranscriptText {
    block_id: String,
    order: (u64, usize),
    content: SharedString,
    highlights: Vec<(std::ops::Range<usize>, HighlightStyle)>,
    font_family_overrides: Vec<(std::ops::Range<usize>, SharedString)>,
    links: Vec<(std::ops::Range<usize>, String)>,
    selection: Rc<RefCell<TranscriptTextSelection>>,
    focus: FocusHandle,
}

struct SelectableTextPrepaint {
    text: StyledText,
    layout: TextLayout,
    hitbox: gpui::Hitbox,
}

impl SelectableTranscriptText {
    fn new(
        block_id: String,
        order: (u64, usize),
        content: MarkdownInlineContent,
        selection: Rc<RefCell<TranscriptTextSelection>>,
        focus: FocusHandle,
    ) -> Self {
        let MarkdownInlineContent {
            text,
            highlights,
            font_family_overrides,
            links,
        } = content;
        let content: SharedString = text.into();
        selection
            .borrow_mut()
            .register_block(&block_id, order, content.clone());
        Self {
            block_id,
            order,
            content,
            highlights,
            font_family_overrides,
            links,
            selection,
            focus,
        }
    }

    fn merged_highlights(&self) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
        let selected = self.selection.borrow();
        let selected = selected.range_for(&self.block_id, &self.content);
        let mut boundaries = vec![0, self.content.len()];
        for (range, _) in &self.highlights {
            boundaries.extend([range.start, range.end]);
        }
        if let Some(range) = &selected {
            boundaries.extend([range.start, range.end]);
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        boundaries
            .windows(2)
            .filter_map(|pair| {
                let range = pair[0]..pair[1];
                if range.is_empty() {
                    return None;
                }
                let mut style = self
                    .highlights
                    .iter()
                    .find_map(|(highlight_range, style)| {
                        (range.start >= highlight_range.start && range.end <= highlight_range.end)
                            .then_some(*style)
                    })
                    .unwrap_or_default();
                if selected.as_ref().is_some_and(|selected| {
                    range.start >= selected.start && range.end <= selected.end
                }) {
                    style.background_color = Some(gpui::rgba(0x2f81_f766).into());
                }
                Some((range, style))
            })
            .collect()
    }
}

impl Element for SelectableTranscriptText {
    type RequestLayoutState = StyledText;
    type PrepaintState = SelectableTextPrepaint;

    fn id(&self) -> Option<ElementId> {
        Some(self.block_id.clone().into())
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
        let mut text = StyledText::new(self.content.clone())
            .with_highlights(self.merged_highlights())
            .with_font_family_overrides(self.font_family_overrides.clone());
        let (layout_id, ()) = text.request_layout(id, inspector_id, window, cx);
        (layout_id, text)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<gpui::Pixels>,
        text: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        text.prepaint(id, inspector_id, bounds, &mut (), window, cx);
        let layout = text.layout().clone();
        self.selection
            .borrow_mut()
            .update_geometry(&self.block_id, bounds, layout.clone());
        let hitbox = window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal);
        SelectableTextPrepaint {
            text: std::mem::replace(text, StyledText::new("")),
            layout,
            hitbox,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "painting selectable text registers its drag, release, link, and copy interactions"
    )]
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<gpui::Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let current_view = window.current_view();
        let mouse_index = prepaint
            .layout
            .index_for_position(window.mouse_position())
            .unwrap_or_else(|index| index)
            .min(self.content.len());
        let over_link = self
            .links
            .iter()
            .any(|(range, _)| range.contains(&mouse_index));
        window.set_cursor_style(
            if over_link {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            },
            &prepaint.hitbox,
        );

        let block_id = self.block_id.clone();
        let order = self.order;
        let content = self.content.clone();
        let selection = self.selection.clone();
        let focus = self.focus.clone();
        let layout = prepaint.layout.clone();
        let hitbox = prepaint.hitbox.clone();
        window.on_mouse_event(
            move |event: &gpui::MouseDownEvent, phase, window: &mut Window, cx| {
                if phase != gpui::DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let index = layout
                    .index_for_position(event.position)
                    .unwrap_or_else(|index| index)
                    .min(content.len());
                let mut selected = selection.borrow_mut();
                selected.begin(block_id.clone(), order, &content, index);
                drop(selected);
                window.focus(&focus, cx);
                cx.notify(current_view);
            },
        );

        let selection = self.selection.clone();
        window.on_mouse_event(
            move |event: &gpui::MouseMoveEvent, phase, _window: &mut Window, cx| {
                if phase != gpui::DispatchPhase::Bubble {
                    return;
                }
                let mut selected = selection.borrow_mut();
                if !selected.dragging {
                    return;
                }
                selected.extend_to_position(event.position);
                drop(selected);
                cx.notify(current_view);
            },
        );

        let block_id = self.block_id.clone();
        let selection = self.selection.clone();
        let links = self.links.clone();
        let layout = prepaint.layout.clone();
        window.on_mouse_event(
            move |event: &gpui::MouseUpEvent, phase, _window: &mut Window, cx| {
                if phase != gpui::DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                let mut selected = selection.borrow_mut();
                if !selected.dragging {
                    return;
                }
                selected.dragging = false;
                let clicked_this_block = selected
                    .anchor
                    .as_ref()
                    .is_some_and(|anchor| anchor.block_id == block_id)
                    && selected
                        .focus
                        .as_ref()
                        .is_some_and(|focus| focus.block_id == block_id);
                let open_target = (selected.is_empty() && clicked_this_block).then(|| {
                    let index = layout
                        .index_for_position(event.position)
                        .unwrap_or_else(|index| index);
                    links
                        .iter()
                        .find_map(|(range, target)| range.contains(&index).then(|| target.clone()))
                });
                drop(selected);
                if let Some(Some(target)) = open_target {
                    cx.open_url(&target);
                }
                cx.notify(current_view);
            },
        );

        prepaint
            .text
            .paint(id, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}

impl IntoElement for SelectableTranscriptText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// One rendering unit inside a sequence of Markdown sibling nodes: either a
/// stretch of adjacent inline nodes that must share a single text layout, or a
/// single node that renders as its own block.
#[derive(Clone, Debug, PartialEq)]
enum MarkdownRun {
    Inline(std::ops::Range<usize>),
    Block(usize),
}

fn safe_markdown_url(target: &str) -> Option<String> {
    let target = target.trim();
    let lowercase = target.to_ascii_lowercase();
    (lowercase.starts_with("https://")
        || lowercase.starts_with("http://")
        || lowercase.starts_with("mailto:"))
    .then(|| target.to_owned())
}

/// An image shown full size over the session.
#[derive(Clone)]
struct ImagePreview {
    title: String,
    source: PreviewSource,
}

/// Where the pixels for a preview come from.
///
/// A file on disk is loaded by path so the bytes are not held twice. A pasted
/// image has no file yet, so its decoded bytes are kept until the runtime
/// echoes back a path for it.
#[derive(Clone)]
enum PreviewSource {
    Path(PathBuf),
    Bytes(std::sync::Arc<gpui::Image>),
}

/// Build a preview for an attachment staged in the composer.
fn draft_preview(attachment: &PromptAttachment) -> Option<ImagePreview> {
    if !attachment.is_image() {
        return None;
    }
    let title = attachment.display_name().to_owned();
    if let Some(path) = attachment.path() {
        return Some(ImagePreview {
            title,
            source: PreviewSource::Path(PathBuf::from(path)),
        });
    }
    Some(ImagePreview {
        title,
        source: PreviewSource::Bytes(std::sync::Arc::new(gpui::Image {
            format: image_format_for(attachment.mime_type()?)?,
            bytes: attachment.image_bytes()?,
            id: 0,
        })),
    })
}

/// Map a MIME type onto the format gpui needs to decode it.
fn image_format_for(mime_type: &str) -> Option<gpui::ImageFormat> {
    match mime_type {
        "image/png" => Some(gpui::ImageFormat::Png),
        "image/jpeg" => Some(gpui::ImageFormat::Jpeg),
        "image/webp" => Some(gpui::ImageFormat::Webp),
        "image/gif" => Some(gpui::ImageFormat::Gif),
        "image/bmp" => Some(gpui::ImageFormat::Bmp),
        _ => None,
    }
}
enum ServiceUpdate {
    Ready {
        compatibility: ProviderCompatibility,
        projects: Vec<ProjectMetadata>,
        failures: Vec<RestoreFailure>,
    },
    SessionHydrated(SessionHandle),
    RestorationFinished(Vec<RestoreFailure>),
    SessionAdded(SessionHandle),
    SessionsDiscovered(Vec<SessionHandle>),
    /// A session was deleted and must be dropped from the UI.
    SessionDeleted(String),
    /// A session deletion failed; the spinner shown while it was in flight
    /// must be cleared and the error surfaced.
    SessionDeleteFailed {
        app_session_id: String,
        error: String,
    },
    /// The configured project list changed, with the project to select next.
    ProjectsChanged {
        projects: Vec<ProjectMetadata>,
        selected: Option<String>,
    },
    SessionLaunchProgress(SessionLaunchProgress),
    PromptAccepted(Option<String>),
    ActionFailed(String),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SessionLaunchProgress {
    CreatingWorktree,
    WorktreeReady(PathBuf),
}

enum ServiceCommand {
    Submit {
        app_session_id: Option<String>,
        prompt: String,
        attachments: Vec<PromptAttachment>,
        project_path: PathBuf,
        model: Option<String>,
        mode: String,
        reasoning_effort: Option<String>,
        context_tier: Option<String>,
        /// Git ref new sessions compare their changes against.
        base_ref: Option<String>,
        /// Repository new sessions group under.
        repository_root: Option<String>,
        /// Whether to create a project session or a standalone chat.
        kind: SessionKind,
        /// Where a new project session should run.
        location: SessionLocation,
        /// Root under which a new worktree should be created.
        worktrees_root: PathBuf,
    },
    Cancel {
        app_session_id: String,
    },
    Resume {
        app_session_id: String,
        /// Managed root used to recreate a missing worktree, when known.
        worktrees_root: Option<PathBuf>,
    },
    RelocateSession {
        app_session_id: String,
        working_directory: PathBuf,
    },
    Respond {
        app_session_id: String,
        interaction_id: String,
        response: InteractionResponse,
    },
    LoadEarlierOutput {
        app_session_id: String,
        identity: String,
        before_chunk: u64,
    },
    SetModel {
        app_session_id: String,
        model: String,
        reasoning_effort: Option<String>,
        context_tier: Option<String>,
    },
    SetMode {
        app_session_id: String,
        mode: String,
    },
    SetReasoningEffort {
        app_session_id: String,
        effort: String,
    },
    SetContextTier {
        app_session_id: String,
        tier: String,
    },
    SetBaseRef {
        app_session_id: String,
        base_ref: String,
    },
    RefreshChanges {
        app_session_id: String,
        force: bool,
    },
    Select {
        app_session_id: Option<String>,
    },
    RenameSession {
        app_session_id: String,
        title: String,
    },
    DeleteSession {
        app_session_id: String,
        /// Root that owned this particular worktree, including a previous root.
        worktrees_root: Option<PathBuf>,
    },
    /// Register a directory chosen by the user as a project.
    AddProject {
        path: PathBuf,
    },
    RemoveProject {
        project_id: String,
    },
    Stop,
}

struct AppService {
    updates: Receiver<ServiceUpdate>,
    commands: Sender<ServiceCommand>,
    stopped: Receiver<()>,
    bootstrap: Option<BootstrapState>,
}

struct BootstrapState {
    projects: Vec<ProjectMetadata>,
    sessions: Vec<SessionMetadata>,
    selected_session: Option<String>,
}

impl AppService {
    #[allow(clippy::too_many_lines)]
    fn start(project_root: PathBuf, database_path: &Path) -> Self {
        let startup_started = Instant::now();
        let diagnostics = Arc::new(TracingDiagnostics);
        let storage_started = Instant::now();
        let storage = match Storage::open(database_path) {
            Ok(storage) => Arc::new(storage),
            Err(error) => {
                return Self::failed(format!(
                    "failed to open {}: {error}",
                    database_path.display()
                ));
            }
        };
        let storage_ms = elapsed_millis(storage_started);
        let bootstrap = BootstrapState {
            projects: storage.list_projects().unwrap_or_else(|error| {
                tracing::error!(%error, "failed to list bootstrap projects");
                Vec::new()
            }),
            sessions: storage.list_sessions().unwrap_or_else(|error| {
                tracing::error!(%error, "failed to list bootstrap sessions");
                Vec::new()
            }),
            selected_session: storage.selected_session().unwrap_or(None),
        };
        diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "desktop_startup".to_owned(),
            operation: "bootstrap".to_owned(),
            elapsed_ms: Some(elapsed_millis(startup_started)),
            session_id: bootstrap.selected_session.clone(),
            success: true,
            details: serde_json::json!({
                "storageMs": storage_ms,
                "projectCount": bootstrap.projects.len(),
                "sessionCount": bootstrap.sessions.len()
            }),
        });
        let preferred_session = bootstrap
            .selected_session
            .as_ref()
            .filter(|id| bootstrap.sessions.iter().any(|session| &session.id == *id))
            .cloned()
            .or_else(|| bootstrap.sessions.first().map(|session| session.id.clone()));
        let (update_tx, updates) = channel();
        let (commands, command_rx) = channel();
        let (stopped_tx, stopped) = channel();
        thread::Builder::new()
            .name("gcabb-services".to_owned())
            .spawn(move || {
                let runtime_started = Instant::now();
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("gcabb-worker")
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = update_tx.send(ServiceUpdate::Failed(format!(
                            "failed to create async runtime: {error}"
                        )));
                        let _ = stopped_tx.send(());
                        return;
                    }
                };
                let runtime_ms = elapsed_millis(runtime_started);
                let provider_factory =
                    CopilotProviderFactory::new(project_root.clone(), diagnostics.clone());
                let session_roots = SessionRoots {
                    worktrees: None,
                    attachments: attachments_directory(),
                    runtime_state: runtime_state_root(),
                };
                let manager = Arc::new(
                    SessionManager::new(provider_factory, storage, diagnostics.clone())
                        .with_session_roots(session_roots.clone()),
                );
                let orchestrator = SessionOrchestrator::new(manager.clone(), session_roots.clone());
                // Projects are configured by the user, not inferred from the
                // launch directory. Auto-registering the launch repository
                // would silently re-add a project the user had removed.

                // Fold projects and sessions recorded by earlier builds, which
                // registered one project per worktree, into their repository.
                let adoption_started = Instant::now();
                match manager.adopt_repository_roots(|path| {
                    let path = Path::new(path);
                    path.is_dir()
                        .then(|| repository_root(path).to_string_lossy().into_owned())
                }) {
                    Ok(0) => {}
                    Ok(count) => {
                        tracing::info!(count, "associated sessions with their repository");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to adopt repository roots");
                    }
                }
                let adoption_ms = elapsed_millis(adoption_started);

                let manager_started = Instant::now();
                let mut restoration_task = None;
                match runtime.block_on(manager.start_preferred_session(
                    preferred_session.as_deref(),
                    |handle| {
                        let _ = update_tx.send(ServiceUpdate::SessionHydrated(handle));
                    },
                )) {
                    Ok((compatibility, report, remaining)) => {
                        let manager_ms = elapsed_millis(manager_started);
                        let metadata_started = Instant::now();
                        let projects = manager.projects().unwrap_or_else(|error| {
                            tracing::error!(%error, "failed to list projects");
                            Vec::new()
                        });
                        let metadata_ms = elapsed_millis(metadata_started);
                        diagnostics.record(DiagnosticEvent {
                            timestamp: timestamp(),
                            category: "desktop_startup".to_owned(),
                            operation: "ready".to_owned(),
                            elapsed_ms: Some(elapsed_millis(startup_started)),
                            session_id: preferred_session.clone(),
                            success: true,
                            details: serde_json::json!({
                                "runtimeMs": runtime_ms,
                                "storageMs": storage_ms,
                                "adoptionMs": adoption_ms,
                                "managerMs": manager_ms,
                                "metadataMs": metadata_ms,
                                "projectCount": projects.len(),
                                "restoredSessions": report.restored.len(),
                                "failedSessions": report.failed.len(),
                                "remainingSessions": remaining.len()
                            }),
                        });
                        let _ = update_tx.send(ServiceUpdate::Ready {
                            compatibility,
                            projects,
                            failures: report.failed,
                        });
                        let background_manager = manager.clone();
                        let background_updates = update_tx.clone();
                        restoration_task = Some(runtime.spawn(async move {
                            let report = background_manager
                                .restore_remaining_sessions(remaining, |handle| {
                                    let _ = background_updates
                                        .send(ServiceUpdate::SessionHydrated(handle));
                                })
                                .await;
                            let _ = background_updates
                                .send(ServiceUpdate::RestorationFinished(report.failed));
                        }));
                    }
                    Err(error) => {
                        let _ = update_tx.send(ServiceUpdate::Failed(format!(
                            "Copilot provider startup failed: {error}"
                        )));
                    }
                }

                while let Ok(command) = command_rx.recv() {
                    if matches!(command, ServiceCommand::Stop) {
                        if let Some(task) = restoration_task.take() {
                            let _ = runtime.block_on(task);
                        }
                        let _ = runtime.block_on(manager.stop());
                        break;
                    }
                    // Project changes publish a project list rather than a
                    // session, so they are handled before the session commands.
                    match command {
                        ServiceCommand::DeleteSession {
                            app_session_id,
                            worktrees_root,
                        } => {
                            let mut deletion_roots = session_roots.clone();
                            deletion_roots.worktrees = worktrees_root;
                            match runtime
                                .block_on(manager.delete_session(&app_session_id, &deletion_roots))
                            {
                                Ok(deletion) => {
                                    let _ =
                                        update_tx.send(ServiceUpdate::SessionDeleted(deletion.id));
                                    // A preserved or unremovable worktree is
                                    // worth saying out loud so it cannot be
                                    // orphaned silently.
                                    if let Some(notice) =
                                        deletion.worktree.as_ref().and_then(WorktreeOutcome::notice)
                                    {
                                        let _ = update_tx.send(ServiceUpdate::ActionFailed(notice));
                                    }
                                }
                                Err(error) => {
                                    let _ = update_tx.send(ServiceUpdate::SessionDeleteFailed {
                                        app_session_id,
                                        error: error.to_string(),
                                    });
                                }
                            }
                        }
                        ServiceCommand::AddProject { path } => {
                            match register_directory_as_project(&manager, &path) {
                                Ok(selected) => {
                                    let projects = manager.projects().unwrap_or_default();
                                    let _ = update_tx.send(ServiceUpdate::ProjectsChanged {
                                        projects,
                                        selected: Some(selected),
                                    });
                                }
                                Err(error) => {
                                    let _ = update_tx.send(ServiceUpdate::ActionFailed(error));
                                }
                            }
                        }
                        ServiceCommand::RemoveProject { project_id } => {
                            if let Err(error) = manager.remove_project(&project_id) {
                                let _ =
                                    update_tx.send(ServiceUpdate::ActionFailed(error.to_string()));
                            } else {
                                let projects = manager.projects().unwrap_or_default();
                                let selected = projects.first().map(|project| project.path.clone());
                                let _ = update_tx
                                    .send(ServiceUpdate::ProjectsChanged { projects, selected });
                            }
                        }
                        command => {
                            let submit_origin = match &command {
                                ServiceCommand::Submit { app_session_id, .. } => {
                                    Some(app_session_id.clone())
                                }
                                _ => None,
                            };
                            match runtime.block_on(handle_service_command(
                                &manager,
                                &orchestrator,
                                command,
                                &update_tx,
                            )) {
                                Ok(Some(handle)) => {
                                    let _ = update_tx.send(ServiceUpdate::SessionAdded(handle));
                                    if let Some(origin) = submit_origin {
                                        let _ =
                                            update_tx.send(ServiceUpdate::PromptAccepted(origin));
                                    }
                                }
                                Ok(None) => {
                                    if let Some(origin) = submit_origin {
                                        let _ =
                                            update_tx.send(ServiceUpdate::PromptAccepted(origin));
                                    }
                                }
                                Err(error) => {
                                    let _ = update_tx.send(ServiceUpdate::ActionFailed(error));
                                    let sessions = runtime.block_on(manager.sessions());
                                    let _ =
                                        update_tx.send(ServiceUpdate::SessionsDiscovered(sessions));
                                }
                            }
                        }
                    }
                }
                let _ = stopped_tx.send(());
            })
            .expect("failed to start GCABB service thread");
        Self {
            updates,
            commands,
            stopped,
            bootstrap: Some(bootstrap),
        }
    }

    fn failed(error: String) -> Self {
        let (update_tx, updates) = channel();
        let (commands, _command_rx) = channel();
        let (stopped_tx, stopped) = channel();
        let _ = update_tx.send(ServiceUpdate::Failed(error));
        let _ = stopped_tx.send(());
        Self {
            updates,
            commands,
            stopped,
            bootstrap: None,
        }
    }

    /// A service with no backing thread, plus the command receiver.
    ///
    /// View tests drive real UI code but must not start a Copilot provider, so
    /// commands are captured and asserted on instead of executed.
    #[cfg(test)]
    fn for_test() -> (Self, Receiver<ServiceCommand>) {
        let (service, commands, _updates) = Self::for_test_with_updates();
        (service, commands)
    }

    #[cfg(test)]
    fn for_test_with_updates() -> (Self, Receiver<ServiceCommand>, Sender<ServiceUpdate>) {
        let (update_tx, updates) = channel();
        let (commands, command_rx) = channel();
        let (_stopped_tx, stopped) = channel();
        (
            Self {
                updates,
                commands,
                stopped,
                bootstrap: None,
            },
            command_rx,
            update_tx,
        )
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_service_command(
    manager: &SessionManager,
    orchestrator: &SessionOrchestrator,
    command: ServiceCommand,
    updates: &Sender<ServiceUpdate>,
) -> Result<Option<SessionHandle>, String> {
    let mut created = None;
    match command {
        ServiceCommand::Submit {
            app_session_id,
            prompt,
            attachments,
            project_path,
            model,
            mode,
            reasoning_effort,
            context_tier,
            base_ref,
            repository_root,
            kind,
            location,
            worktrees_root,
        } => {
            if let Some(id) = app_session_id {
                let handle = manager
                    .session(&id)
                    .await
                    .map_err(|error| error.to_string())?;
                manager
                    .set_selected_session(Some(handle.id()))
                    .map_err(|error| error.to_string())?;
                handle
                    .send_with_attachments(prompt, attachments)
                    .await
                    .map_err(|error| error.to_string())?;
            } else {
                let result = orchestrator
                    .launch(
                        LaunchRequest {
                            project_path,
                            repository_root: repository_root.map(PathBuf::from),
                            worktrees_root,
                            kind,
                            location,
                            prompt,
                            attachments,
                            model,
                            mode,
                            reasoning_effort,
                            context_tier,
                            base_ref,
                            title: LaunchTitle::Automatic,
                            origin: LaunchOrigin::UserActivation,
                        },
                        |progress| {
                            let progress = match progress {
                                LaunchProgress::CreatingWorktree => {
                                    SessionLaunchProgress::CreatingWorktree
                                }
                                LaunchProgress::WorktreeReady(path) => {
                                    SessionLaunchProgress::WorktreeReady(path)
                                }
                            };
                            let _ = updates.send(ServiceUpdate::SessionLaunchProgress(progress));
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                created = Some(result.handle);
            }
        }
        ServiceCommand::Cancel { app_session_id } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .cancel()
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::Resume {
            app_session_id,
            worktrees_root,
        } => {
            created = Some(
                manager
                    .resume_closed_session_from_worktrees_root(
                        &app_session_id,
                        worktrees_root.as_deref(),
                    )
                    .await
                    .map_err(|error| error.to_string())?,
            );
        }
        ServiceCommand::RelocateSession {
            app_session_id,
            working_directory,
        } => {
            created = Some(
                manager
                    .relocate_session(&app_session_id, &working_directory)
                    .await
                    .map_err(|error| error.to_string())?,
            );
        }
        ServiceCommand::Respond {
            app_session_id,
            interaction_id,
            response,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .respond(interaction_id, response)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::LoadEarlierOutput {
            app_session_id,
            identity,
            before_chunk,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .load_output_before(
                OutputStreamKind::Invocation,
                identity,
                before_chunk,
                storage::RESTORED_OUTPUT_CHUNKS,
            )
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetModel {
            app_session_id,
            model,
            reasoning_effort,
            context_tier,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_model_with_options(model, reasoning_effort, context_tier)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetMode {
            app_session_id,
            mode,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_mode(mode)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetReasoningEffort {
            app_session_id,
            effort,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_reasoning_effort(effort)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetContextTier {
            app_session_id,
            tier,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_context_tier(tier)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetBaseRef {
            app_session_id,
            base_ref,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_base_ref(base_ref)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::RefreshChanges {
            app_session_id,
            force,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .refresh_changes(force)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::RenameSession {
            app_session_id,
            title,
        } => manager
            .rename_session(&app_session_id, &title)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::Select { app_session_id } => manager
            .set_selected_session(app_session_id.as_deref())
            .map_err(|error| error.to_string())?,
        // Project commands publish a project list instead of a session and
        // are handled before this dispatch.
        ServiceCommand::AddProject { .. }
        | ServiceCommand::RemoveProject { .. }
        | ServiceCommand::DeleteSession { .. }
        | ServiceCommand::Stop => {}
    }
    Ok(created)
}

/// Register a user-chosen directory as a project.
///
/// The directory may be any folder on disk. When it is inside a git worktree
/// the repository root is registered instead, so adding a worktree and adding
/// its main checkout produce the same project rather than duplicates.
///
/// Returns the path that should become the selected project.
fn register_directory_as_project(manager: &SessionManager, path: &Path) -> Result<String, String> {
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let root = repository_root(&canonical);
    let path_string = root.to_string_lossy().into_owned();
    let project = ProjectMetadata {
        id: path_string.clone(),
        path: path_string.clone(),
        name: root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Project")
            .to_owned(),
        default_branch: default_branch(&root),
        last_opened_at: timestamp(),
    };
    manager
        .register_project(&project)
        .map_err(|error| error.to_string())?;
    Ok(path_string)
}

/// Root directory session worktrees are created under.
///
/// Kept beside the application database so it follows `GCABB_DATA_DIR` during
/// development and never lands inside a repository.
/// Where the runtime keeps per-session state, keyed by its own session id.
///
/// Deleting a session leaves this behind otherwise; one machine had 114 MB
/// across 69 directories for sessions that no longer existed.
fn runtime_state_root() -> Option<PathBuf> {
    let path = dirs::home_dir()?.join(".copilot").join("session-state");
    path.is_dir().then_some(path)
}

fn default_worktrees_root() -> PathBuf {
    data_directory().map_or_else(
        |_| PathBuf::from(".gcabb").join("worktrees"),
        |base| base.join("worktrees"),
    )
}

fn summary_line(summary: &str) -> String {
    let first = summary.lines().next().unwrap_or_default().trim();
    let truncated: String = first.chars().take(120).collect();
    if truncated.len() < first.len() {
        format!("{truncated}…")
    } else if summary.lines().count() > 1 {
        format!("{truncated} …")
    } else {
        truncated
    }
}
fn session_uses_worktree(metadata: &SessionMetadata) -> bool {
    !metadata.is_chat()
        && metadata
            .repository_root
            .as_deref()
            .is_some_and(|root| Path::new(root) != Path::new(&metadata.project_path))
}

struct SessionProjection {
    _handle: Option<SessionHandle>,
    receiver: Option<watch::Receiver<Arc<SessionSnapshot>>>,
    snapshot: Arc<SessionSnapshot>,
    /// When the current active turn began, retained across Starting -> Running.
    running_since: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimelineItemKind {
    SessionStart(SessionStartItem),
    Message(usize),
    Tool(usize),
    Interaction(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionStartItem {
    CreatingWorktree,
    WorktreeReady,
    CopilotSessionStarted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TimelineItem {
    id: String,
    sequence: u64,
    kind: TimelineItemKind,
}

#[derive(Default)]
struct TimelineIndex {
    session_id: String,
    items: Vec<TimelineItem>,
    scanned_messages: usize,
    scanned_invocations: usize,
    scanned_interactions: usize,
    children: HashMap<String, Vec<usize>>,
}

impl TimelineIndex {
    fn reset(&mut self, snapshot: &SessionSnapshot) {
        self.session_id.clone_from(&snapshot.metadata.id);
        self.items.clear();
        self.scanned_messages = 0;
        self.scanned_invocations = 0;
        self.scanned_interactions = 0;
        self.children.clear();
        if session_uses_worktree(&snapshot.metadata) {
            self.items.extend(
                [
                    SessionStartItem::CreatingWorktree,
                    SessionStartItem::WorktreeReady,
                    SessionStartItem::CopilotSessionStarted,
                ]
                .into_iter()
                .map(|item| TimelineItem {
                    id: format!("session-start-{item:?}"),
                    sequence: 0,
                    kind: TimelineItemKind::SessionStart(item),
                }),
            );
        }
        self.append(snapshot);
    }

    fn append(&mut self, snapshot: &SessionSnapshot) {
        let mut additions = Vec::new();
        additions.extend(
            snapshot.transcript[self.scanned_messages..]
                .iter()
                .enumerate()
                .map(|(offset, message)| TimelineItem {
                    id: message.id.clone(),
                    sequence: message.sequence,
                    kind: TimelineItemKind::Message(self.scanned_messages + offset),
                }),
        );
        for (offset, invocation) in snapshot.tool_activity.invocations[self.scanned_invocations..]
            .iter()
            .enumerate()
        {
            let index = self.scanned_invocations + offset;
            if let Some(agent) = invocation.agent_id.as_deref() {
                if let Some(parent) = snapshot.tool_activity.agent_parents.get(agent) {
                    self.children.entry(parent.clone()).or_default().push(index);
                }
            } else {
                additions.push(TimelineItem {
                    id: invocation.call_id.clone(),
                    sequence: invocation.sequence,
                    kind: TimelineItemKind::Tool(index),
                });
            }
        }
        additions.extend(
            snapshot.interaction_history[self.scanned_interactions..]
                .iter()
                .enumerate()
                .filter(|(_, record)| record.request.kind == InteractionKind::Permission)
                .map(|(offset, record)| TimelineItem {
                    id: format!(
                        "permission-{}-{}",
                        self.scanned_interactions + offset,
                        record.request.id
                    ),
                    sequence: record.sequence,
                    kind: TimelineItemKind::Interaction(self.scanned_interactions + offset),
                }),
        );
        additions.sort_by_key(|item| item.sequence);
        self.items.extend(additions);
        self.scanned_messages = snapshot.transcript.len();
        self.scanned_invocations = snapshot.tool_activity.invocations.len();
        self.scanned_interactions = snapshot.interaction_history.len();
    }

    fn sync(&mut self, snapshot: &SessionSnapshot) -> bool {
        let reset = self.session_id != snapshot.metadata.id
            || self.scanned_messages > snapshot.transcript.len()
            || self.scanned_invocations > snapshot.tool_activity.invocations.len()
            || self.scanned_interactions > snapshot.interaction_history.len();
        let previous_len = self.items.len();
        if reset {
            self.reset(snapshot);
        } else {
            self.append(snapshot);
        }
        reset || self.items.len() != previous_len
    }
}

struct CachedMarkdown {
    source: String,
    document: Arc<markdown::MarkdownDocument>,
}

impl SessionProjection {
    fn new(handle: SessionHandle) -> Self {
        let receiver = handle.subscribe();
        let snapshot = receiver.borrow().clone();
        let running_since = session_is_running(snapshot.status).then(Instant::now);
        Self {
            _handle: Some(handle),
            receiver: Some(receiver),
            snapshot,
            running_since,
        }
    }

    fn bootstrap(metadata: SessionMetadata) -> Self {
        let mut snapshot = SessionSnapshot::new(metadata);
        snapshot.status = SessionStatus::Recovering;
        Self {
            _handle: None,
            receiver: None,
            snapshot: Arc::new(snapshot),
            running_since: None,
        }
    }

    fn set_snapshot(&mut self, snapshot: Arc<SessionSnapshot>) {
        let was_running = session_is_running(self.snapshot.status);
        let is_running = session_is_running(snapshot.status);
        match (was_running, is_running) {
            (false, true) => self.running_since = Some(Instant::now()),
            (true, false) => self.running_since = None,
            _ => {}
        }
        self.snapshot = snapshot;
    }

    fn id(&self) -> &str {
        &self.snapshot.metadata.id
    }

    #[cfg(test)]
    fn for_test(handle: SessionHandle) -> Self {
        Self::new(handle)
    }
}

enum StartupState {
    Starting,
    Ready(ProviderCompatibility),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupNavigation {
    Untouched,
    Changed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlMenu {
    Project,
    Location,
    Mode,
    Model,
    Effort,
    Context,
}

/// Sentinel option value that opens the folder picker from the project menu.
const ADD_PROJECT_OPTION: &str = "\u{0}add-project";
/// Sentinel option value that switches the composer to a standalone chat.
const CHAT_OPTION: &str = "\u{0}chat";

/// An open session context menu, anchored at the click position.
struct SessionMenu {
    id: String,
    title: String,
    position: gpui::Point<gpui::Pixels>,
}

/// Context menu for a project, anchored where the user right-clicked.
struct ProjectMenu {
    id: String,
    name: String,
    position: gpui::Point<gpui::Pixels>,
}

/// Phase 3 inspector tabs for the session side panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionPanel {
    Changes,
    Terminals,
    Capabilities,
}

impl SessionPanel {
    const ALL: [Self; 3] = [Self::Changes, Self::Terminals, Self::Capabilities];

    const fn label(self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Terminals => "Terminals",
            Self::Capabilities => "Capabilities",
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Changes => "panel-changes",
            Self::Terminals => "panel-terminals",
            Self::Capabilities => "panel-capabilities",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsVisibility {
    Closed,
    Open,
}

#[derive(Clone)]
struct WorktreeConfiguration {
    data_dir: Option<PathBuf>,
    settings: AppSettings,
    default_root: PathBuf,
}

impl WorktreeConfiguration {
    fn load(data_dir: &Result<PathBuf, String>) -> Self {
        let default_root = data_dir
            .as_ref()
            .map_or_else(|_| default_worktrees_root(), |path| path.join("worktrees"));
        let settings = data_dir
            .as_ref()
            .map_or_else(|_| AppSettings::default(), |path| AppSettings::load(path));
        Self {
            data_dir: data_dir.as_ref().ok().cloned(),
            settings,
            default_root,
        }
    }
}

struct SessionMvpView {
    startup: StartupState,
    projects: Vec<ProjectMetadata>,
    sessions: Vec<SessionProjection>,
    selected_session: Option<String>,
    /// User navigation during startup wins over delayed bootstrap/restoration.
    startup_navigation: StartupNavigation,
    /// Repository grouping key for the sidebar.
    selected_project: PathBuf,
    /// Directory new sessions run in.
    workspace_root: PathBuf,
    /// Directory GCABB was launched from, used when no project is selected.
    launch_workspace: PathBuf,
    /// Working directory chats run in, since chats have no repository.
    chats_workspace: PathBuf,
    /// Where pasted images are written so they can be referenced by path.
    attachments_root: Option<PathBuf>,
    worktree_configuration: WorktreeConfiguration,
    /// Whether the composer will start a chat rather than a project session.
    composing_chat: bool,
    /// Where the next project session will run.
    draft_location: SessionLocation,
    /// Files staged to travel with the next prompt.
    draft_attachments: Vec<PromptAttachment>,
    /// The image being shown full size, if any.
    image_preview: Option<ImagePreview>,
    /// Focus for the preview, so Escape reaches it however it was opened.
    image_preview_focus: FocusHandle,
    /// Branch currently checked out in the selected project, refreshed when
    /// the selection changes so the composer never runs git per frame.
    project_branch: Option<String>,
    /// Variable-height virtual list state for the transcript.
    transcript_list: ListState,
    /// Shared selection for the currently dragged transcript text block.
    transcript_selection: Rc<RefCell<TranscriptTextSelection>>,
    /// Receives copy shortcuts while transcript text is selected.
    transcript_selection_focus: FocusHandle,
    /// Geometry used to paint the transcript thumb, retained so hit testing
    /// cannot race later dynamic-height measurements.
    drawn_transcript_scrollbar: Option<ScrollbarGeometry>,
    /// Glide back to the conversation tail in progress, if any.
    scroll_to_bottom: Option<ScrollToBottom>,
    /// Task stepping the glide. Dropping it stops the motion, so replacing it
    /// is how a second press or a manual scroll takes over.
    scroll_to_bottom_task: Option<gpui::Task<()>>,
    /// Stable, incrementally maintained order and child lookup for transcript rows.
    timeline: TimelineIndex,
    /// Parsed documents for immutable completed messages.
    markdown_cache: HashMap<String, CachedMarkdown>,
    markdown_cache_order: VecDeque<String>,
    /// Syntax-highlighted changed files, bounded because one diff may be large.
    diff_cache: RefCell<DiffCache>,
    /// Number of transcript rows instantiated during the latest render pass.
    transcript_rows_rendered: usize,
    /// Last snapshot revision whose mutable rows were invalidated.
    transcript_snapshot_sequence: u64,
    transcript_snapshot_ptr: usize,
    /// Scroll positions of the detail blocks inside tool entries, keyed by
    /// block id so each keeps its position across renders.
    detail_scrolls: RefCell<HashMap<String, gpui::ScrollHandle>>,
    /// Last rendered content length for each detail block, used to follow
    /// streaming shell output without resetting blocks the user scrolled up.
    detail_extents: RefCell<HashMap<String, usize>>,
    /// Where each scroll region sat before the wheel event being handled, so a
    /// region can tell whether it actually moved.
    scroll_positions: RefCell<HashMap<String, gpui::Point<gpui::Pixels>>>,
    /// Large completed outputs the user explicitly chose to lay out in full.
    expanded_tool_outputs: HashSet<String>,
    /// Tool rows whose detailed card is open, keyed by session.
    expanded_tools: HashMap<String, HashSet<String>>,
    /// Scrollbar currently being dragged, if any.
    ///
    /// Tracked on the view rather than the thumb so a drag keeps working once
    /// the pointer leaves the narrow track, which is most of the time.
    dragging_scrollbar: Option<ScrollbarDrag>,
    /// Transcript shape retained for diagnostics and regression assertions.
    transcript_extent: (String, usize, usize, usize, usize),
    restore_failures: Vec<RestoreFailure>,
    updates: Receiver<ServiceUpdate>,
    commands: Sender<ServiceCommand>,
    branch: String,
    composer: Entity<TextInput>,
    /// Incomplete prompt entered before a session is selected.
    home_draft: String,
    /// Incomplete prompts keyed by the session they belong to.
    session_drafts: HashMap<String, String>,
    interaction_input: Entity<TextInput>,
    draft_mode: String,
    draft_model: Option<String>,
    draft_effort: String,
    draft_context_tier: Option<String>,
    sidebar_open: bool,
    panel_open: bool,
    active_panel: SessionPanel,
    /// Changed files whose diff is expanded in the Changes panel, keyed by
    /// session so switching sessions or panel tabs keeps each one's state.
    expanded_changes: HashMap<String, HashSet<String>>,
    /// Focus handles for changed-file rows, keyed by session and path so a row
    /// keeps its focus identity while change data refreshes.
    change_focus: RefCell<HashMap<String, FocusHandle>>,
    /// Whether the Changes base selector is open.
    base_menu_visibility: SettingsVisibility,
    /// Branches discovered when the base selector was opened.
    base_ref_options: Vec<String>,
    /// Cached project default shown by the open Base menu.
    base_default_ref: Option<String>,
    open_control_menu: Option<ControlMenu>,
    /// Session whose context menu is open, and where to draw it.
    session_menu: Option<SessionMenu>,
    /// Project whose context menu is open, and where to draw it.
    project_menu: Option<ProjectMenu>,
    /// Session being renamed, if the rename dialog is open.
    renaming_session: Option<String>,
    rename_input: Entity<TextInput>,
    /// Sessions with a delete in flight, shown with a spinner in place of
    /// the status dot until the backend confirms removal.
    deleting_sessions: HashSet<String>,
    /// Startup progress shown before the new session has an id or transcript.
    session_launch: Option<SessionLaunchProgress>,
    action_error: Option<String>,
    settings_error: Option<String>,
    /// What the update banner is showing.
    update_ui: UpdateUi,
    /// Background update worker, absent for developer builds that never update.
    update_service: Option<UpdateService>,
    settings_visibility: SettingsVisibility,
    diagnostics_visibility: SettingsVisibility,
    running_since: HashMap<String, Instant>,
    last_event_seen: HashMap<String, (u64, Instant)>,
    last_activity_repaint: Instant,
    _poll_task: gpui::Task<()>,
    _running_tick_task: gpui::Task<()>,
    _update_poll_task: gpui::Task<()>,
}

impl SessionMvpView {
    #[allow(clippy::too_many_lines)]
    fn new(
        service: AppService,
        project_root: PathBuf,
        branch: String,
        chats_workspace: PathBuf,
        attachments_root: Option<PathBuf>,
        worktree_configuration: WorktreeConfiguration,
        cx: &mut Context<Self>,
    ) -> Self {
        let AppService {
            updates,
            commands,
            stopped,
            bootstrap,
        } = service;
        let quit_commands = commands.clone();
        let stopped = Arc::new(Mutex::new(stopped));
        let background_executor = cx.background_executor().clone();
        cx.on_app_quit(move |_, _| {
            let quit_commands = quit_commands.clone();
            let stopped = stopped.clone();
            let background_executor = background_executor.clone();
            async move {
                let _ = quit_commands.send(ServiceCommand::Stop);
                for _ in 0..10 {
                    let is_stopped = stopped
                        .lock()
                        .map_or(true, |receiver| receiver.try_recv().is_ok());
                    if is_stopped {
                        break;
                    }
                    background_executor.timer(Duration::from_millis(10)).await;
                }
            }
        })
        .detach();

        let composer = cx.new(|cx| {
            TextInput::new(
                cx,
                "composer-input",
                "Ask anything, paste a URL, type / for commands, # for issues or & for sessions...",
            )
        });
        cx.subscribe(&composer, |view, _, event: &InputSubmitted, cx| {
            view.submit_prompt(event.text.clone());
            cx.notify();
        })
        .detach();
        cx.subscribe(&composer, |view, _, event: &ImagesPasted, cx| {
            view.attach_pasted_images(&event.images, cx);
        })
        .detach();
        cx.observe(&composer, |_, _, cx| cx.notify()).detach();
        let interaction_input =
            cx.new(|cx| TextInput::new(cx, "interaction-input", "Type your response..."));
        cx.subscribe(&interaction_input, |view, _, event: &InputSubmitted, cx| {
            view.submit_interaction(event.text.clone());
            view.interaction_input.update(cx, TextInput::clear);
            cx.notify();
        })
        .detach();
        let rename_input = cx.new(|cx| TextInput::new(cx, "rename-input", "Session name"));
        cx.subscribe(&rename_input, |view, _, event: &InputSubmitted, cx| {
            view.commit_rename(&event.text, cx);
        })
        .detach();

        let poll_task = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                if view
                    .update(cx, |view, cx| {
                        // Both sides must run, so avoid short-circuiting here.
                        let updated = view.apply_service_updates(cx);
                        let refreshed = view.refresh_snapshots();
                        let banner_changed = view.apply_update_events();
                        let timers_changed = view.sync_activity_timers();
                        let repaint_timer = view.activity_timer_due();
                        if updated || refreshed || banner_changed || timers_changed || repaint_timer
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let running_tick_task = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if view
                    .update(cx, |view, cx| {
                        if view
                            .selected()
                            .is_some_and(|session| session.running_since.is_some())
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        // A developer build never updates itself, so it gets no worker and no
        // banner rather than a disabled one.
        let build = BuildStamp::current();
        let update_service = match (build.is_release(), data_directory()) {
            (true, Ok(data_dir)) => {
                let service = UpdateService::start(build, data_dir);
                service.request(UpdateRequest::Check { automatic: true });
                Some(service)
            }
            _ => None,
        };
        let periodic_update_delay = update_poll_delay();
        let update_poll_task = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(periodic_update_delay).await;
                if view
                    .update(cx, |view, _| {
                        view.request_update(UpdateRequest::Check { automatic: true });
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let transcript_list = ListState::new(0, ListAlignment::Top, px(TRANSCRIPT_OVERDRAW))
            .with_uniform_item_height(px(TRANSCRIPT_ROW_HEIGHT_HINT));
        transcript_list.set_follow_mode(FollowMode::Tail);
        let mut view = Self {
            startup: StartupState::Starting,
            projects: Vec::new(),
            sessions: Vec::new(),
            selected_session: None,
            startup_navigation: StartupNavigation::Untouched,
            selected_project: repository_root(&project_root),
            workspace_root: project_root.clone(),
            launch_workspace: project_root,
            chats_workspace,
            attachments_root,
            worktree_configuration,
            composing_chat: false,
            draft_location: SessionLocation::default(),
            draft_attachments: Vec::new(),
            image_preview: None,
            image_preview_focus: cx.focus_handle(),
            project_branch: None,
            transcript_list,
            transcript_selection: Rc::new(RefCell::new(TranscriptTextSelection::default())),
            transcript_selection_focus: cx.focus_handle(),
            drawn_transcript_scrollbar: None,
            scroll_to_bottom: None,
            scroll_to_bottom_task: None,
            timeline: TimelineIndex::default(),
            markdown_cache: HashMap::new(),
            markdown_cache_order: VecDeque::new(),
            diff_cache: RefCell::new(DiffCache::default()),
            transcript_rows_rendered: 0,
            transcript_snapshot_sequence: 0,
            transcript_snapshot_ptr: 0,
            detail_scrolls: RefCell::new(HashMap::new()),
            detail_extents: RefCell::new(HashMap::new()),
            scroll_positions: RefCell::new(HashMap::new()),
            expanded_tool_outputs: HashSet::new(),
            expanded_tools: HashMap::new(),
            dragging_scrollbar: None,
            transcript_extent: (String::new(), 0, 0, 0, 0),
            restore_failures: Vec::new(),
            updates,
            commands,
            branch,
            composer,
            home_draft: String::new(),
            session_drafts: HashMap::new(),
            interaction_input,
            draft_mode: "interactive".to_owned(),
            draft_model: None,
            draft_effort: "medium".to_owned(),
            draft_context_tier: None,
            sidebar_open: true,
            panel_open: false,
            active_panel: SessionPanel::Changes,
            expanded_changes: HashMap::new(),
            change_focus: RefCell::new(HashMap::new()),
            base_menu_visibility: SettingsVisibility::Closed,
            base_ref_options: Vec::new(),
            base_default_ref: None,
            open_control_menu: None,
            session_menu: None,
            project_menu: None,
            renaming_session: None,
            rename_input,
            deleting_sessions: HashSet::new(),
            session_launch: None,
            action_error: None,
            settings_error: None,
            update_ui: UpdateUi::default(),
            update_service,
            settings_visibility: SettingsVisibility::Closed,
            diagnostics_visibility: SettingsVisibility::Closed,
            running_since: HashMap::new(),
            last_event_seen: HashMap::new(),
            last_activity_repaint: Instant::now(),
            _poll_task: poll_task,
            _running_tick_task: running_tick_task,
            _update_poll_task: update_poll_task,
        };
        if let Some(bootstrap) = bootstrap {
            view.apply_bootstrap(bootstrap);
        }
        view
    }

    /// Message, accent colour, and optional detail line for the update banner.
    fn update_banner_text(&self) -> Option<(String, u32, Option<String>)> {
        let (message, accent) = match &self.update_ui {
            UpdateUi::Hidden => return None,
            UpdateUi::Checking => ("Checking for updates…".to_owned(), MUTED),
            UpdateUi::Available { version, .. } => (format!("GCABB {version} is available"), BLUE),
            UpdateUi::Downloading { .. } => (
                self.update_ui.percent().map_or_else(
                    || "Downloading update…".to_owned(),
                    |percent| format!("Downloading update… {percent}%"),
                ),
                BLUE,
            ),
            UpdateUi::ReadyToRestart { version } => (
                format!("GCABB {version} is installed and starts on restart"),
                GREEN,
            ),
            UpdateUi::Failed(error) => (format!("Update failed: {error}"), RED),
        };

        // Release notes are shown as a short summary; the full text lives in
        // the GitHub Release, and a banner is the wrong place for a changelog.
        let summary = match &self.update_ui {
            UpdateUi::Available { notes, .. } => notes
                .lines()
                .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
                .map(|line| line.trim().to_owned()),
            _ => None,
        };

        Some((message, accent, summary))
    }

    /// The update banner, when there is something to say about an update.
    ///
    /// Returns `None` in the common case so an install with nothing to report
    /// spends no space on it.
    fn update_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (message, accent, summary) = self.update_banner_text()?;

        let banner = div()
            .id("update-banner")
            .debug_selector(|| "update-banner".to_owned())
            .accessibility_id("update-banner")
            .role(Role::Group)
            .aria_label("Update")
            .flex()
            .items_center()
            .gap_3()
            .w_full()
            .px_4()
            .py_2()
            .bg(rgb(ELEVATED))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded_full()
                    .bg(rgb(accent))
                    .flex_shrink_0(),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().text_color(rgb(PRIMARY)).child(message))
                    .when_some(summary, |column, summary| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .truncate()
                                .child(summary),
                        )
                    }),
            );

        let banner = match &self.update_ui {
            UpdateUi::Available { .. } => banner
                .child(action_button("Update", BLUE, cx, |view, _| {
                    view.request_update(UpdateRequest::Install);
                }))
                .child(action_button("Later", ELEVATED, cx, |view, _| {
                    view.request_update(UpdateRequest::Defer);
                })),
            UpdateUi::ReadyToRestart { version } => {
                banner.child(Self::restart_button(version.clone(), cx))
            }
            UpdateUi::Failed(_) => {
                banner.child(action_button("Dismiss", ELEVATED, cx, |view, _| {
                    view.update_ui = UpdateUi::Hidden;
                }))
            }
            _ => banner,
        };

        Some(banner.into_any_element())
    }

    /// Button that starts the replacement build and closes this one.
    fn restart_button(version: String, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("update-restart")
            .debug_selector(|| "update-restart".to_owned())
            .accessibility_id("update-restart")
            .role(Role::Button)
            .aria_label("Restart")
            .focusable()
            .tab_stop(true)
            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
            .px_4()
            .py_2()
            .rounded_md()
            .bg(rgb(GREEN))
            .text_color(rgb(BACKGROUND))
            .child("Restart")
            .hover(|style| style.opacity(0.85).cursor_pointer())
            .on_click(cx.listener(
                move |view, _, _, cx| match updates::restart_into_updated_build(&version) {
                    // The replacement is running, so this process can go.
                    Ok(()) => cx.quit(),
                    Err(error) => {
                        view.update_ui = UpdateUi::Failed(error);
                        cx.notify();
                    }
                },
            ))
    }

    /// Forwards a request to the update worker.
    fn request_update(&mut self, request: UpdateRequest) {
        if let Some(service) = self.update_service.as_ref() {
            service.request(request);
        }
    }

    fn current_worktrees_root(&self) -> PathBuf {
        self.worktree_configuration
            .settings
            .worktrees_root(&self.worktree_configuration.default_root)
    }

    fn display_worktree_path(&self, path: &Path) -> String {
        self.worktree_configuration
            .settings
            .display_worktree_path(path, &self.worktree_configuration.default_root)
    }

    fn persist_worktrees_root(&mut self, root: Option<PathBuf>) -> Result<(), String> {
        let mut settings = self.worktree_configuration.settings.clone();
        match root {
            Some(root) => {
                settings.set_worktrees_root(root, &self.worktree_configuration.default_root);
            }
            None => settings.use_default_worktrees_root(&self.worktree_configuration.default_root),
        }
        if let Some(data_dir) = self.worktree_configuration.data_dir.as_deref() {
            settings
                .save(data_dir)
                .map_err(|error| format!("could not save worktree location: {error}"))?;
        }
        self.worktree_configuration.settings = settings;
        Ok(())
    }

    fn choose_worktrees_root(&mut self, cx: &mut Context<Self>) {
        self.settings_error = None;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose worktree location".into()),
        });
        cx.spawn(async move |view, cx| {
            let selection = match paths.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                Ok(Err(error)) => {
                    let _ = view.update(cx, |view, cx| {
                        view.settings_error =
                            Some(format!("could not open the folder picker: {error}"));
                        cx.notify();
                    });
                    return;
                }
                Err(_) => None,
            };
            let Some(path) = selection else {
                return;
            };
            let path = path.canonicalize().unwrap_or(path);
            let _ = view.update(cx, |view, cx| {
                if let Err(error) = view.persist_worktrees_root(Some(path)) {
                    view.settings_error = Some(error);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn settings_worktrees_button(cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("settings-change-worktrees")
            .accessibility_id("settings-change-worktrees")
            .role(Role::Button)
            .aria_label("Change worktree location")
            .focusable()
            .tab_stop(true)
            .px_4()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .text_sm()
            .child("Change…")
            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
            .on_click(cx.listener(|view, _, _, cx| {
                view.choose_worktrees_root(cx);
                cx.notify();
            }))
            .into_any_element()
    }

    fn settings_check_button(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let updates_available = self.update_service.is_some();
        let checking = self.update_ui == UpdateUi::Checking;
        let check_label = if checking {
            "Checking…"
        } else if updates_available {
            "Check for updates"
        } else {
            "Unavailable in development builds"
        };

        div()
            .id("settings-check-updates")
            .accessibility_id("settings-check-updates")
            .role(Role::Button)
            .aria_label(check_label)
            .focusable()
            .tab_stop(updates_available && !checking)
            .px_4()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .text_sm()
            .text_color(if updates_available && !checking {
                rgb(PRIMARY)
            } else {
                rgb(MUTED)
            })
            .child(check_label)
            .when(updates_available && !checking, |button| {
                button
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.request_update(UpdateRequest::Check { automatic: false });
                        cx.notify();
                    }))
            })
            .into_any_element()
    }

    fn settings_close_button(cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("settings-close")
            .accessibility_id("settings-close")
            .role(Role::Button)
            .aria_label("Close settings")
            .focusable()
            .tab_stop(true)
            .px_4()
            .py_2()
            .rounded_md()
            .bg(rgb(ELEVATED))
            .child("Close")
            .hover(|style| style.opacity(0.85).cursor_pointer())
            .on_click(cx.listener(|view, _, _, cx| {
                view.settings_visibility = SettingsVisibility::Closed;
                cx.notify();
            }))
            .into_any_element()
    }

    fn settings_worktrees_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let worktrees_root = self.current_worktrees_root();
        let uses_default = self
            .worktree_configuration
            .settings
            .uses_default_worktrees_root();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().child("Worktree location"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(worktrees_root.to_string_lossy().into_owned()),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(
                        "Changes apply to new worktrees only. Existing sessions keep their \
                         current locations.",
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(!uses_default, |buttons| {
                        buttons.child(
                            div()
                                .id("settings-default-worktrees")
                                .accessibility_id("settings-default-worktrees")
                                .role(Role::Button)
                                .aria_label("Use default worktree location")
                                .focusable()
                                .tab_stop(true)
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child("Use default")
                                .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                .on_click(cx.listener(|view, _, _, cx| {
                                    if let Err(error) = view.persist_worktrees_root(None) {
                                        view.settings_error = Some(error);
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .child(Self::settings_worktrees_button(cx)),
            )
    }

    fn settings_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.settings_visibility != SettingsVisibility::Open {
            return None;
        }
        let version = BuildStamp::current().version.to_string();

        Some(
            div()
                .id("settings-dialog")
                .accessibility_id("settings-dialog")
                .role(Role::Dialog)
                .aria_label("Settings")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x0000_00a8))
                .child(
                    div()
                        .id("settings-panel")
                        .w(px(620.0))
                        .flex()
                        .flex_col()
                        .gap_4()
                        .p_5()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        .child(
                            div()
                                .id("settings-heading")
                                .role(Role::Heading)
                                .aria_level(2)
                                .aria_label("Settings")
                                .text_xl()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("Settings"),
                        )
                        .child(self.settings_worktrees_row(cx))
                        .when_some(self.settings_error.clone(), |panel, error| {
                            panel.child(
                                div()
                                    .id("settings-error")
                                    .role(Role::Alert)
                                    .aria_label(error.clone())
                                    .text_xs()
                                    .text_color(rgb(RED))
                                    .child(error),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_4()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(div().child("Updates"))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .child(format!("Current version: {version}")),
                                        ),
                                )
                                .child(self.settings_check_button(cx)),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .child(Self::settings_close_button(cx)),
                        ),
                ),
        )
    }

    /// Drains pending update-worker events into the banner.
    fn apply_update_events(&mut self) -> bool {
        // Destructured so the worker and the banner are borrowed as separate
        // fields rather than through one borrow of `self`.
        let Self {
            update_service,
            update_ui,
            ..
        } = self;
        update_service
            .as_ref()
            .is_some_and(|service| service.drain(update_ui))
    }

    /// Drains pending service updates, returning whether any were applied so the
    /// caller can skip repainting when the poll tick found nothing to do.
    fn apply_service_updates(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        loop {
            let update = match self.updates.try_recv() {
                Ok(update) => update,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            changed = true;
            match update {
                ServiceUpdate::Ready {
                    compatibility,
                    projects,
                    failures,
                } => {
                    self.startup = StartupState::Ready(compatibility);
                    self.projects = projects;
                    self.apply_restore_failures(failures);
                }
                ServiceUpdate::SessionHydrated(handle) => {
                    self.upsert_hydrated_session(handle, cx);
                }
                ServiceUpdate::RestorationFinished(failures) => {
                    self.apply_restore_failures(failures);
                }
                ServiceUpdate::SessionAdded(handle) => {
                    let id = handle.id().to_owned();
                    self.session_launch = None;
                    self.upsert_hydrated_session(handle, cx);
                    self.switch_composer_draft(Some(id), cx);
                }
                ServiceUpdate::SessionsDiscovered(handles) => {
                    for handle in handles {
                        self.upsert_hydrated_session(handle, cx);
                    }
                }
                ServiceUpdate::ProjectsChanged { projects, selected } => {
                    self.apply_projects_changed(projects, selected, cx);
                }
                ServiceUpdate::SessionLaunchProgress(progress) => {
                    self.session_launch = Some(progress);
                }
                ServiceUpdate::SessionDeleted(id) => {
                    self.sessions.retain(|session| session.id() != id);
                    self.restore_failures
                        .retain(|failure| failure.app_session_id != id);
                    self.deleting_sessions.remove(&id);
                    if self.selected_session.as_deref() == Some(id.as_str()) {
                        self.switch_composer_draft(None, cx);
                    }
                    self.session_drafts.remove(&id);
                    self.expanded_changes.remove(&id);
                    self.expanded_tools.remove(&id);
                    if self.session_menu.as_ref().is_some_and(|menu| menu.id == id) {
                        self.session_menu = None;
                    }
                    if self.renaming_session.as_deref() == Some(id.as_str()) {
                        self.renaming_session = None;
                    }
                }
                ServiceUpdate::SessionDeleteFailed {
                    app_session_id,
                    error,
                } => {
                    self.deleting_sessions.remove(&app_session_id);
                    self.action_error = Some(error);
                }
                ServiceUpdate::PromptAccepted(origin) => {
                    if let Some(id) = origin.as_deref() {
                        self.session_drafts.remove(id);
                    } else {
                        self.home_draft.clear();
                    }
                    if self.selected_session == origin {
                        self.composer.update(cx, TextInput::clear);
                    }
                }
                ServiceUpdate::ActionFailed(error) => {
                    self.session_launch = None;
                    self.action_error = Some(error);
                }
                ServiceUpdate::Failed(error) => self.startup = StartupState::Failed(error),
            }
        }
        changed
    }

    fn apply_bootstrap(&mut self, bootstrap: BootstrapState) {
        self.projects = bootstrap.projects;
        self.sessions = bootstrap
            .sessions
            .into_iter()
            .map(SessionProjection::bootstrap)
            .collect();
        if self.startup_navigation == StartupNavigation::Untouched {
            self.selected_session = bootstrap
                .selected_session
                .filter(|id| self.sessions.iter().any(|session| session.id() == id))
                .or_else(|| self.sessions.first().map(|session| session.id().to_owned()));
            self.adopt_selected_session_location();
        }
        if self.projects.is_empty() && self.selected_session.is_none() {
            self.composing_chat = true;
        }
    }

    fn apply_restore_failures(&mut self, failures: Vec<RestoreFailure>) {
        for failure in &failures {
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.id() == failure.app_session_id)
            {
                let mut snapshot = (*session.snapshot).clone();
                snapshot.status = SessionStatus::Failed;
                snapshot.last_error = Some(failure.error.clone());
                session.set_snapshot(Arc::new(snapshot));
            }
        }
        self.restore_failures.extend(failures);
    }

    fn upsert_hydrated_session(&mut self, handle: SessionHandle, cx: &mut Context<Self>) {
        let id = handle.id().to_owned();
        let unavailable = handle.snapshot().status == SessionStatus::Unavailable;
        if let Some(index) = self.sessions.iter().position(|session| session.id() == id) {
            self.sessions[index] = SessionProjection::new(handle);
        } else {
            self.sessions.insert(0, SessionProjection::new(handle));
        }
        if unavailable && self.selected_session.as_deref() == Some(id.as_str()) {
            self.switch_composer_draft(None, cx);
            return;
        }
        if self.startup_navigation == StartupNavigation::Untouched {
            if self.selected_session.is_none() && !unavailable {
                self.switch_composer_draft(Some(id), cx);
            }
            self.adopt_selected_session_location();
        }
    }

    /// Save the current composer text and restore the draft for `target`.
    fn switch_composer_draft(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        let value = self.composer.read(cx).value().clone();
        if let Some(id) = self.selected_session.as_ref() {
            if value.is_empty() {
                self.session_drafts.remove(id);
            } else {
                self.session_drafts.insert(id.clone(), value);
            }
        } else {
            self.home_draft = value;
        }

        self.selected_session = target;
        let draft = self.selected_session.as_ref().map_or_else(
            || self.home_draft.clone(),
            |id| self.session_drafts.get(id).cloned().unwrap_or_default(),
        );
        self.composer
            .update(cx, |input, cx| input.set_value(draft, cx));
    }

    fn adopt_selected_session_location(&mut self) {
        if let Some((project, workspace)) = self
            .selected()
            .filter(|session| !session.snapshot.metadata.is_chat())
            .map(|session| {
                (
                    PathBuf::from(session.snapshot.metadata.project_key()),
                    PathBuf::from(&session.snapshot.metadata.project_path),
                )
            })
        {
            self.selected_project = project;
            self.workspace_root = workspace;
        }
    }

    /// Adopt a new project list, selecting `selected` when one was given.
    fn apply_projects_changed(
        &mut self,
        projects: Vec<ProjectMetadata>,
        selected: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.projects = projects;
        if let Some(selected) = selected {
            self.select_project(&selected, cx);
            return;
        }
        // No projects are configured, so there is nothing to select. Falling
        // back to the launch directory made the pill advertise a project that
        // was not in the list; chat is the only target that needs no
        // configuration.
        self.composing_chat = true;
        self.selected_project.clone_from(&self.launch_workspace);
        self.workspace_root.clone_from(&self.launch_workspace);
        self.switch_composer_draft(None, cx);
        self.startup_navigation = StartupNavigation::Changed;
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: None,
        });
    }

    /// Pulls any changed session snapshots, returning whether one actually moved.
    fn refresh_snapshots(&mut self) -> bool {
        let mut changed = false;
        for projection in &mut self.sessions {
            if projection
                .receiver
                .as_ref()
                .is_some_and(|receiver| receiver.has_changed().unwrap_or(false))
            {
                let snapshot = projection
                    .receiver
                    .as_mut()
                    .expect("changed receiver is present")
                    .borrow_and_update()
                    .clone();
                projection.set_snapshot(snapshot);
                changed = true;
            }
        }
        changed
    }

    fn sync_activity_timers(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;
        let live: HashMap<_, _> = self
            .sessions
            .iter()
            .map(|session| {
                (
                    session.id().to_owned(),
                    (
                        matches!(
                            session.snapshot.status,
                            SessionStatus::Running | SessionStatus::Starting
                        ),
                        session.snapshot.last_sequence,
                    ),
                )
            })
            .collect();

        self.running_since.retain(|id, _| {
            let keep = live.get(id).is_some_and(|(running, _)| *running);
            changed |= !keep;
            keep
        });
        self.last_event_seen.retain(|id, _| live.contains_key(id));

        for (id, (running, sequence)) in live {
            if running && !self.running_since.contains_key(&id) {
                self.running_since.insert(id.clone(), now);
                changed = true;
            }
            match self.last_event_seen.get_mut(&id) {
                Some((seen_sequence, seen_at)) if *seen_sequence != sequence => {
                    *seen_sequence = sequence;
                    *seen_at = now;
                    changed = true;
                }
                None => {
                    self.last_event_seen.insert(id, (sequence, now));
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    fn activity_timer_due(&mut self) -> bool {
        if self.running_since.is_empty()
            || self.last_activity_repaint.elapsed() < Duration::from_secs(1)
        {
            return false;
        }
        self.last_activity_repaint = Instant::now();
        true
    }

    fn selected(&self) -> Option<&SessionProjection> {
        let id = self.selected_session.as_deref()?;
        self.sessions.iter().find(|session| session.id() == id)
    }

    fn submit_prompt(&mut self, prompt: String) {
        if self.session_launch.is_some() {
            return;
        }
        let attachments = std::mem::take(&mut self.draft_attachments);
        self.action_error = None;
        let supported_efforts = self
            .draft_model
            .as_deref()
            .map_or_else(Vec::new, |model| self.supported_reasoning_efforts(model));
        // A chat has no repository, so it gets a neutral working directory and
        // no changes base.
        let (project_path, repository_root, base_ref, kind) = if self.targets_chat() {
            (self.chats_workspace.clone(), None, None, SessionKind::Chat)
        } else {
            (
                self.workspace_root.clone(),
                Some(self.selected_project.to_string_lossy().into_owned()),
                self.selected_project_base_ref(),
                SessionKind::Project,
            )
        };
        if self.selected_session.is_none()
            && kind == SessionKind::Project
            && self.draft_location == SessionLocation::NewWorktree
            && repository_root
                .as_deref()
                .is_some_and(|root| GitService::new(root).is_worktree())
        {
            self.session_launch = Some(SessionLaunchProgress::CreatingWorktree);
        }
        let _ = self.commands.send(ServiceCommand::Submit {
            app_session_id: self.selected_session.clone(),
            prompt,
            attachments,
            project_path,
            model: self.draft_model.clone(),
            mode: self.draft_mode.clone(),
            reasoning_effort: reasoning_effort_for_model(&supported_efforts, &self.draft_effort),
            context_tier: self.selectable_context_tier(),
            base_ref,
            repository_root,
            kind,
            location: self.draft_location,
            worktrees_root: self.current_worktrees_root(),
        });
    }

    /// Whether the composer will act on a chat rather than a project.
    ///
    /// A selected session decides for itself; otherwise the draft state does.
    fn targets_chat(&self) -> bool {
        self.selected().map_or(self.composing_chat, |session| {
            session.snapshot.metadata.is_chat()
        })
    }

    /// Label for the composer's project pill.
    ///
    /// Chat mode must be visible here, otherwise choosing Chat changes state
    /// with no on-screen effect.
    fn composer_project_label(&self) -> String {
        if self.targets_chat() {
            return "Chat".to_owned();
        }
        self.projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .map_or_else(
                // The launch directory is not a project unless the user added
                // it, so naming it here would advertise a project that is not
                // in the picker.
                || "No project".to_owned(),
                |project| project.name.clone(),
            )
    }

    /// Synchronize the virtual list with append-only snapshot state.
    ///
    /// New entries extend the index without sorting or scanning old rows. A
    /// snapshot revision only invalidates the visible neighborhood and tail, so
    /// streaming output cannot force whole-history layout.
    fn sync_transcript(&mut self) {
        let Some(snapshot) = self.selected().map(|session| session.snapshot.clone()) else {
            if self.timeline.items.is_empty() {
                return;
            }
            self.timeline = TimelineIndex::default();
            self.transcript_list.reset(0);
            *self.transcript_selection.borrow_mut() = TranscriptTextSelection::default();
            return;
        };
        let old_len = self.timeline.items.len();
        let old_session = self.timeline.session_id.clone();
        let changed_shape = self.timeline.sync(&snapshot);
        if old_session != snapshot.metadata.id {
            self.cancel_scroll_to_bottom();
            self.transcript_list.reset_with_uniform_height(
                self.timeline.items.len(),
                px(TRANSCRIPT_ROW_HEIGHT_HINT),
            );
            self.transcript_list.set_follow_mode(FollowMode::Tail);
            self.transcript_list.scroll_to_end();
            self.markdown_cache.clear();
            self.markdown_cache_order.clear();
            *self.transcript_selection.borrow_mut() = TranscriptTextSelection::default();
        } else if changed_shape {
            self.transcript_list
                .splice(old_len..old_len, self.timeline.items.len() - old_len);
        }
        let snapshot_ptr = Arc::as_ptr(&snapshot) as usize;
        if self.transcript_snapshot_sequence != snapshot.last_sequence
            || self.transcript_snapshot_ptr != snapshot_ptr
        {
            let top = self.transcript_list.logical_scroll_top().item_ix;
            let visible_end = (top + 32).min(self.timeline.items.len());
            if top < visible_end {
                self.transcript_list.remeasure_items(top..visible_end);
            }
            let tail = self.timeline.items.len().saturating_sub(2);
            if tail < self.timeline.items.len() && tail >= visible_end {
                self.transcript_list
                    .remeasure_items(tail..self.timeline.items.len());
            }
            self.transcript_snapshot_sequence = snapshot.last_sequence;
            self.transcript_snapshot_ptr = snapshot_ptr;
        }
        let extent = (
            snapshot.metadata.id.clone(),
            snapshot.transcript.len(),
            snapshot
                .transcript
                .last()
                .map_or(0, |message| message.content.len()),
            snapshot.tool_activity.invocations.len(),
            snapshot
                .tool_activity
                .invocations
                .last()
                .map_or(0, |invocation| invocation.output.len()),
        );
        self.transcript_extent = extent;
    }

    /// Branch shown beside the location pill.
    ///
    /// A new worktree does not exist yet, so it names the base branch it will
    /// be created from. Running in the local repository names the branch that
    /// repository currently has checked out. Neither is the branch of the
    /// directory GCABB happened to be launched from.
    fn composer_branch_label(&self) -> String {
        let default_branch = self
            .projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .and_then(|project| project.default_branch.clone());
        match self.draft_location {
            SessionLocation::NewWorktree => default_branch
                .or_else(|| self.project_branch.clone())
                .unwrap_or_else(|| "HEAD".to_owned()),
            SessionLocation::LocalRepository => self
                .project_branch
                .clone()
                .or(default_branch)
                .unwrap_or_else(|| "HEAD".to_owned()),
        }
    }

    /// Start composing a standalone chat.
    fn new_chat(&mut self, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.composing_chat = true;
        self.switch_composer_draft(None, cx);
        self.startup_navigation = StartupNavigation::Changed;
        self.action_error = None;
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: None,
        });
        cx.notify();
    }

    /// Base ref new sessions in the selected project compare against.
    ///
    /// The repository's default branch is the natural base for a session
    /// worktree; sessions record it once so later movement on that branch does
    /// not silently change what the changes view reports. Falls back to
    /// resolving it directly when the project has none recorded.
    fn selected_project_base_ref(&self) -> Option<String> {
        self.projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .and_then(|project| project.default_branch.clone())
            .or_else(|| default_branch(&self.selected_project))
    }

    fn submit_interaction(&mut self, value: String) {
        let Some(session) = self.selected() else {
            return;
        };
        let Some(interaction) = session
            .snapshot
            .pending_interactions
            .iter()
            .find(|interaction| interaction.kind != InteractionKind::Permission)
        else {
            return;
        };
        let _ = self.commands.send(ServiceCommand::Respond {
            app_session_id: session.id().to_owned(),
            interaction_id: interaction.id.clone(),
            response: InteractionResponse::Submit {
                value: value.into(),
                freeform: true,
            },
        });
    }

    fn select_session(&mut self, id: String, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.base_menu_visibility = SettingsVisibility::Closed;
        self.base_default_ref = None;
        self.switch_composer_draft(Some(id), cx);
        self.startup_navigation = StartupNavigation::Changed;
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: self.selected_session.clone(),
        });
        if let Some(controls) = self
            .selected()
            .map(|session| session.snapshot.controls.clone())
        {
            // Selecting a chat must leave the project selection alone: the
            // sidebar filters project sessions by it, so repointing it at the
            // chats directory hid every project session.
            if let Some((project, workspace)) = self
                .selected()
                .filter(|session| !session.snapshot.metadata.is_chat())
                .map(|session| {
                    (
                        PathBuf::from(session.snapshot.metadata.project_key()),
                        PathBuf::from(&session.snapshot.metadata.project_path),
                    )
                })
            {
                self.selected_project = project;
                self.workspace_root = workspace;
            }
            self.draft_mode = controls.mode.unwrap_or_else(|| "interactive".to_owned());
            self.draft_model = controls.model;
            self.draft_effort = controls
                .reasoning_effort
                .unwrap_or_else(|| "medium".to_owned());
            self.draft_context_tier = controls.context_tier;
        }
        if self.active_panel == SessionPanel::Changes
            && self
                .selected()
                .is_some_and(|session| !session.snapshot.metadata.is_chat())
        {
            self.refresh_selected_changes(false, cx);
        }
        cx.notify();
    }

    fn select_project(&mut self, path: &str, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.startup_navigation = StartupNavigation::Changed;
        // Choosing a project leaves chat mode. This is the single place that
        // means "the user picked a project", so adding a project, picking one
        // from the menu, and restoring a session all clear the flag here.
        self.composing_chat = false;
        self.selected_project = PathBuf::from(path);
        self.project_branch = git_output(Path::new(path), &["branch", "--show-current"]);
        // New sessions run in the project directory the user chose.
        self.workspace_root = PathBuf::from(path);
        let selected_session = self
            .sessions
            .iter()
            .find(|session| session.snapshot.metadata.project_key() == path)
            .map(|session| session.id().to_owned());
        self.switch_composer_draft(selected_session, cx);
        if let Some(workspace) = self
            .selected()
            .map(|session| PathBuf::from(&session.snapshot.metadata.project_path))
        {
            self.workspace_root = workspace;
        }
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: self.selected_session.clone(),
        });
        cx.notify();
    }

    /// Open the platform folder picker and register the chosen directory.
    fn add_project(&mut self, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.action_error = None;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn(async move |view, cx| {
            let selection = match paths.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                // The Linux picker goes through a desktop portal, which can
                // fail outright; surface that rather than silently doing
                // nothing.
                Ok(Err(error)) => {
                    let message = format!("could not open the folder picker: {error}");
                    let _ = view.update(cx, |view, cx| {
                        view.action_error = Some(message);
                        cx.notify();
                    });
                    return;
                }
                // The channel closes when the dialog is dismissed.
                Err(_) => None,
            };
            let Some(path) = selection else {
                return;
            };
            let _ = view.update(cx, |view, cx| {
                let _ = view.commands.send(ServiceCommand::AddProject { path });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Choose a replacement directory for a session whose original path vanished.
    fn locate_session(&mut self, app_session_id: String, cx: &mut Context<Self>) {
        self.action_error = None;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Locate session working directory".into()),
        });
        cx.spawn(async move |view, cx| {
            let selection = match paths.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                Ok(Err(error)) => {
                    let message = format!("could not open the folder picker: {error}");
                    let _ = view.update(cx, |view, cx| {
                        view.action_error = Some(message);
                        cx.notify();
                    });
                    return;
                }
                Err(_) => None,
            };
            let Some(working_directory) = selection else {
                return;
            };
            let _ = view.update(cx, |view, cx| {
                let _ = view.commands.send(ServiceCommand::RelocateSession {
                    app_session_id: app_session_id.clone(),
                    working_directory,
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn remove_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.project_menu = None;
        self.action_error = None;
        let _ = self
            .commands
            .send(ServiceCommand::RemoveProject { project_id });
        cx.notify();
    }

    /// Open the context menu for a project at the pointer position.
    fn open_project_menu(
        &mut self,
        id: String,
        name: String,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_control_menu = None;
        self.session_menu = None;
        self.project_menu = Some(ProjectMenu { id, name, position });
        cx.notify();
    }

    fn dismiss_context_menu(&mut self, cx: &mut Context<Self>) {
        let dismissed_session = self.session_menu.take().is_some();
        let dismissed_project = self.project_menu.take().is_some();
        if dismissed_session || dismissed_project {
            cx.notify();
        }
    }

    /// Open the context menu for a session at the pointer position.
    fn open_session_menu(
        &mut self,
        id: String,
        title: String,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_control_menu = None;
        self.project_menu = None;
        self.session_menu = Some(SessionMenu {
            id,
            title,
            position,
        });
        cx.notify();
    }

    fn dismiss_session_menu(&mut self, cx: &mut Context<Self>) {
        self.dismiss_context_menu(cx);
    }

    /// Open the rename dialog, seeded with the session's current title.
    fn begin_rename(
        &mut self,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_menu = None;
        self.renaming_session = Some(id);
        self.rename_input
            .update(cx, |input, cx| input.set_value(title, cx));
        // Open ready to type rather than requiring a click into the field.
        let focus_handle = self.rename_input.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, title: &str, cx: &mut Context<Self>) {
        let Some(app_session_id) = self.renaming_session.take() else {
            return;
        };
        let title = title.trim().to_owned();
        // An empty name would leave the session unidentifiable in the sidebar.
        if !title.is_empty() {
            let _ = self.commands.send(ServiceCommand::RenameSession {
                app_session_id: app_session_id.clone(),
                title: title.clone(),
            });
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.id() == app_session_id)
            {
                // Reflect the new name immediately; the actor's snapshot
                // follows once the command is applied.
                let mut snapshot = (*session.snapshot).clone();
                snapshot.metadata.title = title;
                session.set_snapshot(Arc::new(snapshot));
            }
        }
        self.rename_input.update(cx, TextInput::clear);
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming_session = None;
        self.rename_input.update(cx, TextInput::clear);
        cx.notify();
    }

    fn delete_session(&mut self, app_session_id: String, cx: &mut Context<Self>) {
        self.session_menu = None;
        self.action_error = None;
        let worktrees_root = self
            .sessions
            .iter()
            .find(|session| session.id() == app_session_id)
            .and_then(|session| {
                self.worktree_configuration
                    .settings
                    .owning_root_for_worktree(
                        Path::new(&session.snapshot.metadata.project_path),
                        &self.worktree_configuration.default_root,
                    )
            });
        self.deleting_sessions.insert(app_session_id.clone());
        let _ = self.commands.send(ServiceCommand::DeleteSession {
            app_session_id,
            worktrees_root,
        });
        cx.notify();
    }

    /// Context menu for a session, anchored where the user right-clicked.
    fn session_context_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let menu = self.session_menu.as_ref()?;
        let rename_id = menu.id.clone();
        let rename_title = menu.title.clone();
        let delete_id = menu.id.clone();
        let label = menu.title.clone();
        Some(
            div()
                .id("session-menu")
                .accessibility_id("session-menu")
                .role(Role::Menu)
                .aria_label(format!("Actions for {label}"))
                .absolute()
                .left(menu.position.x)
                .top(menu.position.y)
                .w(px(200.0))
                .flex()
                .flex_col()
                .p_1()
                .rounded_lg()
                .bg(rgb(ELEVATED))
                .border_1()
                .border_color(rgb(BORDER))
                .shadow_lg()
                .child(
                    div()
                        .id("session-menu-rename")
                        .debug_selector(|| "session-menu-rename".to_owned())
                        .accessibility_id("session-menu-rename")
                        .role(Role::MenuItem)
                        .aria_label("Rename session")
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .text_sm()
                        .text_color(rgb(PRIMARY))
                        .child("Rename")
                        .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.begin_rename(rename_id.clone(), rename_title.clone(), window, cx);
                        })),
                )
                .child(
                    div()
                        .id("session-menu-delete")
                        .debug_selector(|| "session-menu-delete".to_owned())
                        .accessibility_id("session-menu-delete")
                        .role(Role::MenuItem)
                        .aria_label("Delete session")
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .text_sm()
                        .text_color(rgb(RED))
                        .child("Delete session")
                        .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.delete_session(delete_id.clone(), cx);
                        })),
                ),
        )
    }

    /// Context menu for a project, anchored where the user right-clicked.
    fn project_context_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let menu = self.project_menu.as_ref()?;
        let project_id = menu.id.clone();
        let label = menu.name.clone();
        Some(
            div()
                .id("project-menu")
                .accessibility_id("project-menu")
                .role(Role::Menu)
                .aria_label(format!("Actions for {label}"))
                .absolute()
                .left(menu.position.x)
                .top(menu.position.y)
                .w(px(200.0))
                .flex()
                .flex_col()
                .p_1()
                .rounded_lg()
                .bg(rgb(ELEVATED))
                .border_1()
                .border_color(rgb(BORDER))
                .shadow_lg()
                .child(
                    div()
                        .id("project-menu-remove")
                        .debug_selector(|| "project-menu-remove".to_owned())
                        .accessibility_id("project-menu-remove")
                        .role(Role::MenuItem)
                        .aria_label(format!("Remove {label}"))
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .text_sm()
                        .text_color(rgb(RED))
                        .child("Remove project")
                        .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.remove_project(project_id.clone(), cx);
                        })),
                ),
        )
    }

    /// Rename dialog for the session chosen from the context menu.
    fn rename_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.renaming_session.as_ref()?;
        Some(
            div()
                .id("rename-dialog")
                .accessibility_id("rename-dialog")
                .role(Role::Dialog)
                .aria_label("Rename session")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x0000_00a8))
                .child(
                    div()
                        .id("rename-panel")
                        .w(px(460.0))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_5()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        .child(
                            div()
                                .id("rename-heading")
                                .role(Role::Heading)
                                .aria_level(2)
                                .aria_label("Rename session")
                                .text_xl()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("Rename session"),
                        )
                        .child(
                            div()
                                .border_1()
                                .border_color(rgb(BORDER))
                                .rounded_md()
                                .child(self.rename_input.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .id("rename-cancel")
                                        .accessibility_id("rename-cancel")
                                        .role(Role::Button)
                                        .aria_label("Cancel rename")
                                        .focusable()
                                        .tab_stop(true)
                                        .focus_visible(|style| {
                                            style.border_1().border_color(rgb(BLUE))
                                        })
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .text_color(rgb(MUTED))
                                        .child("Cancel")
                                        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.cancel_rename(cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .id("rename-confirm")
                                        .accessibility_id("rename-confirm")
                                        .role(Role::Button)
                                        .aria_label("Confirm rename")
                                        .focusable()
                                        .tab_stop(true)
                                        .focus_visible(|style| {
                                            style.border_1().border_color(rgb(BLUE))
                                        })
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .bg(rgb(GREEN))
                                        .text_color(rgb(BACKGROUND))
                                        .child("Rename")
                                        .hover(|style| style.opacity(0.85).cursor_pointer())
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            let title = view.rename_input.read(cx).value();
                                            view.commit_rename(&title, cx);
                                        })),
                                ),
                        ),
                ),
        )
    }

    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.switch_composer_draft(None, cx);
        self.startup_navigation = StartupNavigation::Changed;
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: None,
        });
        self.action_error = None;
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    fn new_session_for_project(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.composing_chat = false;
        self.selected_project = PathBuf::from(path);
        self.workspace_root = PathBuf::from(path);
        self.project_branch = git_output(Path::new(path), &["branch", "--show-current"]);
        self.new_session(window, cx);
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    /// Chips for the files staged on the next prompt, each removable.
    ///
    /// Returns nothing when there is nothing attached so the composer keeps
    /// its usual shape.
    fn attachment_strip(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.draft_attachments.is_empty() {
            return None;
        }
        let chips: Vec<_> = self
            .draft_attachments
            .iter()
            .map(|attachment| {
                let identity = attachment.identity();
                let label = attachment.display_name().to_owned();
                let preview_identity = identity.clone();
                let preview_label = label.clone();
                let remove_identity = identity.clone();
                let remove_label = label.clone();
                let icon = if attachment.is_image() { "IMG" } else { "FILE" };
                let preview = draft_preview(attachment);
                let content = div()
                    .id(SharedString::from(format!(
                        "preview-attachment-{preview_identity}"
                    )))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().text_color(rgb(MUTED)).child(icon))
                    .child(div().text_xs().text_color(rgb(PRIMARY)).child(label))
                    .when_some(preview, |content, preview| {
                        content
                            .accessibility_id(format!("preview-attachment-{preview_identity}"))
                            .role(Role::Button)
                            .aria_label(format!("Preview {preview_label}"))
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .hover(gpui::Styled::cursor_pointer)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open_image_preview(preview.clone(), window, cx);
                            }))
                    });
                div()
                    .id(SharedString::from(format!("attachment-{identity}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(SUBTLE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(content)
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "remove-attachment-{remove_identity}"
                            )))
                            .accessibility_id(format!("remove-attachment-{remove_identity}"))
                            .role(Role::Button)
                            .aria_label(format!("Remove {remove_label}"))
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("x")
                            .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.remove_attachment(&remove_identity, cx);
                            })),
                    )
            })
            .collect();
        Some(
            div()
                .id("attachment-strip")
                .accessibility_id("attachment-strip")
                .debug_selector(|| "attachment-strip".to_owned())
                .flex()
                .flex_wrap()
                .gap_2()
                .px_3()
                .pb_2()
                .children(chips),
        )
    }

    /// Show an attachment full size.
    ///
    /// Takes focus so Escape closes it. A click on a chip leaves focus
    /// wherever it was, which left Escape dead exactly when a user would
    /// reach for it.
    fn open_image_preview(
        &mut self,
        preview: ImagePreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.image_preview = Some(preview);
        window.focus(&self.image_preview_focus, cx);
        cx.notify();
    }

    /// Close the preview, if one is open.
    fn dismiss_image_preview(&mut self, cx: &mut Context<Self>) {
        if self.image_preview.take().is_some() {
            cx.notify();
        }
    }

    /// The full-size image overlay.
    fn image_preview_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let preview = self.image_preview.as_ref()?;
        let title = preview.title.clone();
        let image: gpui::Img = match &preview.source {
            PreviewSource::Path(path) => gpui::img(path.clone()),
            PreviewSource::Bytes(image) => gpui::img(image.clone()),
        };
        Some(
            div()
                .id("image-preview")
                .accessibility_id("image-preview")
                .track_focus(&self.image_preview_focus)
                .debug_selector(|| "image-preview".to_owned())
                .role(Role::Dialog)
                .aria_label(format!("Preview of {title}"))
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(gpui::rgba(0x0000_00d8))
                // Anywhere outside the picture closes it, which is what a
                // lightbox trains people to expect.
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|view, _, _, cx| view.dismiss_image_preview(cx)),
                )
                .child(
                    div()
                        .id("image-preview-close")
                        .accessibility_id("image-preview-close")
                        .role(Role::Button)
                        .aria_label("Close image preview")
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .text_sm()
                        .text_color(rgb(PRIMARY))
                        .child("Close")
                        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.dismiss_image_preview(cx);
                        })),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(PRIMARY))
                        .child(title.clone()),
                )
                .child(
                    div()
                        .id("image-preview-frame")
                        .max_w(px(1100.0))
                        .max_h(px(760.0))
                        .p_2()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        // Clicking the picture itself must not dismiss it.
                        .occlude()
                        .child(image.max_w(px(1080.0)).max_h(px(720.0))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child("Click anywhere or press Escape to close"),
                ),
        )
    }

    /// Stage images pasted into the composer.
    ///
    /// A pasted screenshot has no path, so it is carried as bytes. Each paste
    /// is a distinct attachment: someone who pastes twice meant two images.
    fn attach_pasted_images(&mut self, images: &[PastedImage], cx: &mut Context<Self>) {
        let directory = self.attachments_root.clone();
        for image in images {
            let index = self.draft_attachments.len() + 1;
            let (bytes, mime_type) = match normalize_pasted_image(&image.bytes, &image.mime_type) {
                Ok(normalized) => normalized,
                Err(error) => {
                    self.action_error = Some(error);
                    continue;
                }
            };
            // Written to disk and sent as a file, matching what a picked or
            // dropped image does. Sending bytes inline instead meant the
            // runtime echoed back a blob with no path, so the transcript could
            // never show the picture again -- and a copy of those bytes was
            // persisted in the event log and in every later snapshot.
            let attachment = directory
                .as_deref()
                .and_then(|directory| write_pasted_image(directory, &bytes, &mime_type, index))
                .unwrap_or_else(|| PromptAttachment::from_image_bytes(&bytes, mime_type, index));
            self.draft_attachments.push(attachment);
        }
        cx.notify();
    }

    /// Stage files dropped onto the composer.
    fn attach_dropped_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for path in paths {
            let attachment = PromptAttachment::from_path(path);
            if !self
                .draft_attachments
                .iter()
                .any(|existing| existing.identity() == attachment.identity())
            {
                self.draft_attachments.push(attachment);
            }
        }
        cx.notify();
    }

    /// Open a file chooser and attach what the user picks.
    ///
    /// Screenshots are the primary way interface defects get reported, so this
    /// is the difference between a session that can work on the UI and one
    /// that cannot.
    fn pick_attachments(cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |view, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let _ = view.update(cx, |view, cx| {
                for path in paths {
                    let attachment = PromptAttachment::from_path(&path);
                    if !view
                        .draft_attachments
                        .iter()
                        .any(|existing| existing.identity() == attachment.identity())
                    {
                        view.draft_attachments.push(attachment);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Drop an attachment the user changed their mind about.
    fn remove_attachment(&mut self, identity: &str, cx: &mut Context<Self>) {
        self.draft_attachments
            .retain(|attachment| attachment.identity() != identity);
        cx.notify();
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let prompt = self.composer.read(cx).value();
        let prompt = prompt.trim();
        // An attachment alone is a complete message; a screenshot often says
        // everything the user wants to say.
        if !prompt.is_empty() || !self.draft_attachments.is_empty() {
            self.submit_prompt(prompt.to_owned());
            cx.notify();
        }
    }

    fn toggle_control_menu(&mut self, menu: ControlMenu) {
        self.base_menu_visibility = SettingsVisibility::Closed;
        self.open_control_menu = toggled_menu(self.open_control_menu, menu);
    }

    fn toggle_base_menu(&mut self, cx: &mut Context<Self>) {
        if self.base_menu_visibility == SettingsVisibility::Open {
            self.base_menu_visibility = SettingsVisibility::Closed;
            cx.notify();
            return;
        }
        let Some(snapshot) = self.selected().map(|session| session.snapshot.clone()) else {
            return;
        };
        match GitService::new(&snapshot.metadata.project_path).base_refs() {
            Ok(mut options) => {
                if let Some(selected) = snapshot.metadata.base_ref.clone()
                    && !options.contains(&selected)
                {
                    options.push(selected);
                }
                options.sort_by_key(|reference| (reference.contains('/'), reference.clone()));
                self.base_ref_options = options;
                self.base_default_ref = self.default_base_ref(&snapshot);
                self.base_menu_visibility = SettingsVisibility::Open;
                self.open_control_menu = None;
                cx.notify();
            }
            Err(error) => {
                self.action_error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn default_base_ref(&self, snapshot: &SessionSnapshot) -> Option<String> {
        self.projects
            .iter()
            .find(|project| project.path == snapshot.metadata.project_key())
            .and_then(|project| project.default_branch.clone())
            .or_else(|| {
                snapshot
                    .metadata
                    .repository_root
                    .as_deref()
                    .and_then(|root| default_branch(Path::new(root)))
            })
    }

    fn set_changes_base(&mut self, base_ref: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        self.base_menu_visibility = SettingsVisibility::Closed;
        self.action_error = None;
        let _ = self.commands.send(ServiceCommand::SetBaseRef {
            app_session_id: session_id,
            base_ref,
        });
        cx.notify();
    }

    fn refresh_selected_changes(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        self.action_error = None;
        let _ = self.commands.send(ServiceCommand::RefreshChanges {
            app_session_id: session_id,
            force,
        });
        cx.notify();
    }

    fn dismiss_control_menu(&mut self, cx: &mut Context<Self>) {
        if self.open_control_menu.take().is_some() {
            cx.notify();
        }
    }

    fn choose_control(&mut self, menu: ControlMenu, value: String, cx: &mut Context<Self>) {
        match menu {
            ControlMenu::Project => {
                self.open_control_menu = None;
                if value == ADD_PROJECT_OPTION {
                    self.add_project(cx);
                } else if value == CHAT_OPTION {
                    self.new_chat(cx);
                } else {
                    self.select_project(&value, cx);
                }
                return;
            }
            ControlMenu::Location => {
                self.draft_location = SessionLocation::from_str_or_default(&value);
            }
            ControlMenu::Mode => {
                value.clone_into(&mut self.draft_mode);
                if let Some(id) = self.selected_session.clone() {
                    let _ = self.commands.send(ServiceCommand::SetMode {
                        app_session_id: id,
                        mode: value,
                    });
                }
            }
            ControlMenu::Model => {
                let supported_efforts = self.supported_reasoning_efforts(&value);
                self.draft_model = Some(value.clone());
                let reasoning_effort = if supported_efforts.is_empty() {
                    None
                } else {
                    if !supported_efforts.contains(&self.draft_effort) {
                        self.draft_effort.clone_from(
                            supported_efforts
                                .iter()
                                .find(|effort| effort.as_str() == "medium")
                                .unwrap_or(&supported_efforts[0]),
                        );
                    }
                    Some(self.draft_effort.clone())
                };
                self.draft_context_tier = default_context_tier(&self.context_windows(&value));
                let context_tier = self.selectable_context_tier();
                if let Some(id) = self.selected_session.clone() {
                    let _ = self.commands.send(ServiceCommand::SetModel {
                        app_session_id: id,
                        model: value,
                        reasoning_effort,
                        context_tier,
                    });
                }
            }
            ControlMenu::Effort => {
                value.clone_into(&mut self.draft_effort);
                if let Some(id) = self.selected_session.clone() {
                    let _ = self.commands.send(ServiceCommand::SetReasoningEffort {
                        app_session_id: id,
                        effort: value,
                    });
                }
            }
            ControlMenu::Context => {
                self.draft_context_tier = Some(value.clone());
                if let Some(id) = self.selected_session.clone() {
                    let _ = self.commands.send(ServiceCommand::SetContextTier {
                        app_session_id: id,
                        tier: value,
                    });
                }
            }
        }
        self.open_control_menu = None;
    }

    fn provider_status(&self) -> (String, u32) {
        match &self.startup {
            StartupState::Starting => ("Starting Copilot...".to_owned(), AMBER),
            StartupState::Ready(compatibility) => (
                format!(
                    "Connected · protocol {} · pid {}",
                    compatibility.negotiated_protocol_version,
                    compatibility
                        .process_id
                        .map_or_else(|| "external".to_owned(), |pid| pid.to_string())
                ),
                GREEN,
            ),
            StartupState::Failed(error) => (error.clone(), RED),
        }
    }

    fn model_options(&self) -> Vec<(String, String, String)> {
        self.selected()
            .map(|session| &session.snapshot.controls.available_models)
            .filter(|models| !models.is_empty())
            .or(match &self.startup {
                StartupState::Ready(compatibility) => Some(&compatibility.available_models),
                StartupState::Starting | StartupState::Failed(_) => None,
            })
            .into_iter()
            .flatten()
            .map(|model| (model.id.clone(), model.name.clone(), String::new()))
            .collect()
    }

    /// Chat, the configured projects, and an entry that opens the folder
    /// picker. Chat leads because it needs no configuration.
    fn project_options(&self) -> Vec<(String, String, String)> {
        let mut options = vec![(
            CHAT_OPTION.to_owned(),
            "Chat".to_owned(),
            "A session with no repository".to_owned(),
        )];
        options.extend(self.projects.iter().map(|project| {
            let missing = !Path::new(&project.path).is_dir();
            let description = if missing {
                format!("{} (folder is missing)", project.path)
            } else {
                project.path.clone()
            };
            (project.path.clone(), project.name.clone(), description)
        }));
        options.push((
            ADD_PROJECT_OPTION.to_owned(),
            "Add project…".to_owned(),
            "Choose a folder on disk".to_owned(),
        ));
        options
    }

    fn mode_options(&self) -> Vec<(String, String, String)> {
        let modes = match &self.startup {
            StartupState::Ready(compatibility) => &compatibility.available_modes,
            StartupState::Starting | StartupState::Failed(_) => return Vec::new(),
        };
        modes
            .iter()
            .map(|mode| {
                let description = match mode.as_str() {
                    "interactive" => "Step-by-step collaboration",
                    "plan" => "Plan first, execute when ready",
                    "autopilot" => "End-to-end execution",
                    _ => "Copilot agent mode",
                };
                (mode.clone(), title_case(mode), description.to_owned())
            })
            .collect()
    }

    fn supported_reasoning_efforts(&self, model_id: &str) -> Vec<String> {
        // The per-session catalog can list a model without its reasoning
        // efforts. Treat that as missing information and fall back to the
        // app-level catalog, otherwise the thinking-level pill disappears once
        // a session is selected even though the model supports it.
        self.model_entry(model_id)
            .map(|model| model.supported_reasoning_efforts.clone())
            .filter(|efforts| !efforts.is_empty())
            .unwrap_or_default()
    }

    /// Catalog entry describing what a model *can* do.
    ///
    /// The application catalog is authoritative for capabilities. The
    /// per-session catalog is a collapsed view of the session's current state:
    /// it reports no reasoning efforts at all and folds the context tiers into
    /// a single `default` entry holding the active window. Preferring it made
    /// the thinking-level control vanish and the context-length control
    /// degrade to static text as soon as a session was selected. The session
    /// catalog is still used when the app catalog does not know the model.
    fn model_entry(&self, model_id: &str) -> Option<&app_model::ModelOption> {
        let app = match &self.startup {
            StartupState::Ready(compatibility) => compatibility
                .available_models
                .iter()
                .find(|model| model.id == model_id),
            StartupState::Starting | StartupState::Failed(_) => None,
        };
        app.or_else(|| {
            self.selected().and_then(|session| {
                session
                    .snapshot
                    .controls
                    .available_models
                    .iter()
                    .find(|model| model.id == model_id)
            })
        })
    }

    fn effort_options(&self) -> Vec<(String, String, String)> {
        let model_id = self.draft_model.as_deref().unwrap_or("gpt-5.6-sol");
        self.supported_reasoning_efforts(model_id)
            .into_iter()
            .map(|effort| {
                let description = match effort.as_str() {
                    "low" => "Faster responses",
                    "medium" => "Balanced reasoning",
                    "high" => "Deeper reasoning",
                    "xhigh" => "Most thorough reasoning",
                    _ => "Provider-supported reasoning level",
                };
                (
                    effort.clone(),
                    effort_label(&effort),
                    description.to_owned(),
                )
            })
            .collect()
    }

    fn context_windows(&self, model_id: &str) -> Vec<ContextWindowOption> {
        // Resolved through `model_entry` so a per-session catalog entry that
        // carries no capability detail falls back to the app catalog. Without
        // that, selecting a session silently drops the context-length control
        // the same way it dropped the thinking-level control.
        self.model_entry(model_id)
            .map_or_else(Vec::new, |model| model.context_windows.clone())
    }

    /// The model the composer will actually submit with, which falls back to
    /// the catalog's auto entry while no model has been picked explicitly.
    fn effective_model(&self) -> Option<String> {
        self.draft_model.clone().or_else(|| {
            self.model_options()
                .into_iter()
                .find_map(|(id, label, _)| label.eq_ignore_ascii_case("auto").then_some(id))
        })
    }

    fn draft_context_windows(&self) -> Vec<ContextWindowOption> {
        self.effective_model()
            .map_or_else(Vec::new, |model| self.context_windows(&model))
    }

    /// The tier to submit with a request, which is only meaningful when the
    /// model actually offers a choice between context windows.
    fn selectable_context_tier(&self) -> Option<String> {
        let windows = self.draft_context_windows();
        if windows.len() < 2 {
            return None;
        }
        self.draft_context_tier
            .clone()
            .or_else(|| default_context_tier(&windows))
    }

    fn context_options(&self) -> Vec<(String, String, String)> {
        self.draft_context_windows()
            .into_iter()
            .map(|window| {
                let description = match window.tier.as_str() {
                    "default" => "Standard context window",
                    "long_context" => "Extended context window",
                    _ => "Provider-supported context window",
                };
                (
                    window.tier.clone(),
                    context_window_label(&window),
                    description.to_owned(),
                )
            })
            .collect()
    }

    fn draft_context_label(&self) -> Option<String> {
        let windows = self.draft_context_windows();
        if windows.len() < 2 {
            return windows.first().map(context_window_label);
        }
        let selected = self
            .draft_context_tier
            .clone()
            .or_else(|| default_context_tier(&windows))?;
        windows
            .iter()
            .find(|window| window.tier == selected)
            .map(context_window_label)
    }

    /// Renders a context-length selector when the model offers more than one
    /// context window, and a plain readout when it offers exactly one.
    fn context_control(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let label = self.draft_context_label()?;
        if self.draft_context_windows().len() > 1 {
            Some(
                control_pill(
                    "context",
                    label,
                    ControlMenu::Context,
                    self.open_control_menu == Some(ControlMenu::Context),
                    cx,
                )
                .into_any_element(),
            )
        } else {
            Some(context_readout(label).into_any_element())
        }
    }

    fn draft_model_label(&self) -> String {
        let Some(selected) = self.draft_model.as_deref() else {
            return "Auto".to_owned();
        };
        self.model_options()
            .into_iter()
            .find_map(|(id, label, _)| (id == selected).then_some(label))
            .unwrap_or_else(|| selected.to_owned())
    }

    #[allow(clippy::too_many_lines)]
    fn control_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let menu = self.open_control_menu?;
        let (title, selected, options) = match menu {
            ControlMenu::Project => (
                "Project",
                if self.targets_chat() {
                    CHAT_OPTION.to_owned()
                } else {
                    self.selected_project.to_string_lossy().into_owned()
                },
                self.project_options(),
            ),
            ControlMenu::Location => (
                "Where to run this session",
                self.draft_location.as_str().to_owned(),
                [
                    SessionLocation::NewWorktree,
                    SessionLocation::LocalRepository,
                ]
                .into_iter()
                .map(|location| {
                    (
                        location.as_str().to_owned(),
                        location.label().to_owned(),
                        location.description().to_owned(),
                    )
                })
                .collect(),
            ),
            ControlMenu::Mode => ("Mode", self.draft_mode.clone(), self.mode_options()),
            ControlMenu::Model => {
                let options = self.model_options();
                let selected = self
                    .draft_model
                    .clone()
                    .or_else(|| {
                        options.iter().find_map(|(id, label, _)| {
                            (label.eq_ignore_ascii_case("auto")).then(|| id.clone())
                        })
                    })
                    .unwrap_or_default();
                ("Model", selected, options)
            }
            ControlMenu::Effort => (
                "Reasoning effort",
                self.draft_effort.clone(),
                self.effort_options(),
            ),
            ControlMenu::Context => {
                let options = self.context_options();
                let selected = self
                    .draft_context_tier
                    .clone()
                    .or_else(|| options.first().map(|(tier, _, _)| tier.clone()))
                    .unwrap_or_default();
                ("Context length", selected, options)
            }
        };
        let width = if menu == ControlMenu::Model {
            px(340.0)
        } else {
            px(260.0)
        };
        let handle = self
            .detail_scrolls
            .borrow_mut()
            .entry(CONTROL_MENU_SCROLL_ID.to_owned())
            .or_default()
            .clone();
        Some(
            div()
                .id("composer-control-menu")
                .accessibility_id("composer-control-menu")
                .role(Role::ListBox)
                .aria_label(title)
                .w(width)
                .max_h(px(460.0))
                .track_scroll(&handle)
                .overflow_y_scroll()
                .on_scroll_wheel(self.claim_scroll_when_moved(CONTROL_MENU_SCROLL_ID, &handle, cx))
                .p_2()
                .rounded_lg()
                .border_1()
                .border_color(rgb(BORDER))
                .bg(rgb(PANEL))
                .shadow_lg()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(MUTED))
                        .child(title),
                )
                .children(options.into_iter().enumerate().map(
                    |(index, (value, label, description))| {
                        let is_selected = value == selected;
                        let option_value = value.clone();
                        let has_description = !description.is_empty();
                        let accessible_label = label.clone();
                        let accessible_description = description.clone();
                        div()
                            .id(("control-option", index))
                            .accessibility_id(format!("{}-option-{value}", control_menu_id(menu)))
                            .role(Role::ListBoxOption)
                            .aria_label(accessible_label)
                            .aria_selected(is_selected)
                            .when(has_description, |option| {
                                option.aria_description(accessible_description)
                            })
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_2()
                            .rounded_md()
                            .bg(if is_selected {
                                rgb(ELEVATED)
                            } else {
                                rgb(PANEL)
                            })
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.choose_control(menu, option_value.clone(), cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(px(16.0))
                                    .text_color(rgb(MUTED))
                                    .child(if is_selected { "✓" } else { "" }),
                            )
                            .child(div().flex().flex_col().min_w_0().child(label).when(
                                has_description,
                                |content| {
                                    content.child(
                                        div().text_xs().text_color(rgb(MUTED)).child(description),
                                    )
                                },
                            ))
                    },
                )),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn sidebar(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_path = self.selected_project.to_string_lossy();
        let sessions = self
            .sessions
            .iter()
            .filter(|session| !session.snapshot.metadata.is_chat())
            .filter(|session| session.snapshot.metadata.project_key() == selected_path)
            .map(|session| {
                let id = session.id().to_owned();
                let accessible_id = id.clone();
                let label = session.snapshot.metadata.title.clone();
                let menu_id = id.clone();
                let menu_label = label.clone();
                let selected = self.selected_session.as_deref() == Some(id.as_str());
                let is_deleting = self.deleting_sessions.contains(&id);
                let spinner_id = SharedString::from(format!("session-spinner-{id}"));
                div()
                    .id(SharedString::from(format!("session-{id}")))
                    .debug_selector(|| "session-row".to_owned())
                    .accessibility_id(accessible_id)
                    .role(Role::ListItem)
                    .aria_label(label)
                    .aria_selected(selected)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .ml_5()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(if selected {
                        rgb(ELEVATED)
                    } else {
                        rgb(SIDEBAR)
                    })
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.select_session(id.clone(), cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                            view.open_session_menu(
                                menu_id.clone(),
                                menu_label.clone(),
                                event.position,
                                cx,
                            );
                        }),
                    )
                    .child(if is_deleting {
                        progress_spinner(spinner_id).into_any_element()
                    } else {
                        div()
                            .w(px(7.0))
                            .h(px(7.0))
                            .rounded_full()
                            .bg(status_color(session.snapshot.status))
                            .into_any_element()
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(PRIMARY))
                            .overflow_hidden()
                            .child(session.snapshot.metadata.title.clone()),
                    )
            });
        let chats = self
            .sessions
            .iter()
            .filter(|session| session.snapshot.metadata.is_chat())
            .map(|session| {
                let id = session.id().to_owned();
                let label = session.snapshot.metadata.title.clone();
                let menu_id = id.clone();
                let menu_label = label.clone();
                let selected = self.selected_session.as_deref() == Some(id.as_str());
                let is_deleting = self.deleting_sessions.contains(&id);
                let spinner_id = SharedString::from(format!("chat-spinner-{id}"));
                div()
                    .id(SharedString::from(format!("chat-{id}")))
                    .debug_selector(|| "chat-row".to_owned())
                    .accessibility_id(id.clone())
                    .role(Role::ListItem)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .ml_5()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(if selected {
                        rgb(ELEVATED)
                    } else {
                        rgb(SIDEBAR)
                    })
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.select_session(id.clone(), cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                            view.open_session_menu(
                                menu_id.clone(),
                                menu_label.clone(),
                                event.position,
                                cx,
                            );
                        }),
                    )
                    .child(if is_deleting {
                        progress_spinner(spinner_id).into_any_element()
                    } else {
                        div()
                            .w(px(7.0))
                            .h(px(7.0))
                            .rounded_full()
                            .bg(status_color(session.snapshot.status))
                            .into_any_element()
                    })
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(PRIMARY))
                            .overflow_hidden()
                            .child(label),
                    )
            });
        let projects = self.projects.iter().map(|project| {
            let path = project.path.clone();
            let new_session_path = path.clone();
            let project_id = project.id.clone();
            let menu_project_id = project_id.clone();
            let selected = project.path == selected_path;
            let label = project.name.clone();
            let menu_label = label.clone();
            let missing = !Path::new(&project.path).is_dir();
            div()
                .id(SharedString::from(format!("project-{path}")))
                .debug_selector(|| "project-row".to_owned())
                .accessibility_id(path.clone())
                .role(Role::ListItem)
                .aria_label(if missing {
                    format!("{label} (folder is missing)")
                } else {
                    label.clone()
                })
                .aria_selected(selected)
                .focusable()
                .tab_stop(true)
                .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .text_sm()
                .text_color(if selected { rgb(PRIMARY) } else { rgb(MUTED) })
                .bg(rgb(SIDEBAR))
                .child(div().text_color(rgb(MUTED)).child("▱"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(if missing { rgb(AMBER) } else { rgb(PRIMARY) })
                        .child(project.name.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("new-session-{project_id}")))
                        .debug_selector(|| "project-new-session".to_owned())
                        .accessibility_id(format!("new-session-{project_id}"))
                        .role(Role::Button)
                        .aria_label(format!("New session for {label}"))
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .w(px(20.0))
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .text_color(rgb(MUTED))
                        .child("+")
                        .hover(|style| style.bg(rgb(SUBTLE)).text_color(rgb(PRIMARY)))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            view.new_session_for_project(&new_session_path, window, cx);
                        })),
                )
                .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.select_project(&path, cx);
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                        view.open_project_menu(
                            menu_project_id.clone(),
                            menu_label.clone(),
                            event.position,
                            cx,
                        );
                    }),
                )
        });
        div()
            .id("sidebar")
            .accessibility_id("sidebar")
            .role(Role::Navigation)
            .aria_label("Projects and sessions")
            .flex()
            .flex_col()
            .w(if compact { px(300.0) } else { px(280.0) })
            .h_full()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .id("sidebar-titlebar")
                    .h(px(56.0))
                    .flex()
                    .items_center()
                    .pl_3()
                    .pr_3()
                    .gap_3()
                    .child(
                        div()
                            .id("sidebar-toggle")
                            .accessibility_id("sidebar-toggle")
                            .role(Role::Button)
                            .aria_label("Collapse sidebar")
                            .aria_expanded(true)
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .text_color(rgb(MUTED))
                            .child("▯")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.toggle_sidebar(cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(div().text_color(rgb(MUTED)).child("<"))
                    .child(div().text_color(rgb(MUTED)).child(">")),
            )
            .child(
                div()
                    .id("primary-destinations")
                    .role(Role::Navigation)
                    .aria_label("Primary")
                    .flex()
                    .flex_col()
                    .px_2()
                    .gap_1()
                    .child(
                        div()
                            .id("destination-home")
                            .accessibility_id("destination-home")
                            .role(Role::Button)
                            .aria_label("Home")
                            .aria_selected(self.selected_session.is_none())
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(if self.selected_session.is_none() {
                                rgb(ELEVATED)
                            } else {
                                rgb(SIDEBAR)
                            })
                            .child(div().text_color(rgb(MUTED)).child("⌂"))
                            .child("Home")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.new_session(window, cx);
                            })),
                    )
                    .child(disabled_destination("destination-my-work", "☷", "My work"))
                    .child(disabled_destination(
                        "destination-automations",
                        "□",
                        "Automations",
                    ))
                    .child(disabled_destination("destination-search", "⌕", "Search")),
            )
            .child(
                div()
                    .mt_5()
                    .flex()
                    .items_center()
                    .px_4()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("Sessions")
                    .child(div().flex_1())
                    .child(div().id("session-grouping").text_xs().child("By project"))
                    .child(
                        div()
                            .id("new-session")
                            .debug_selector(|| "new-session".to_owned())
                            .accessibility_id("new-session")
                            .role(Role::Button)
                            .aria_label("New session")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .ml_3()
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .text_lg()
                            .child("+")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.new_session(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("session-list")
                    .role(Role::List)
                    .aria_label("Sessions")
                    .flex()
                    .flex_col()
                    .px_2()
                    .mt_2()
                    .gap_1()
                    .child(
                        div()
                            .id("chats-home")
                            .accessibility_id("chats-home")
                            .role(Role::Button)
                            .aria_label("Chats")
                            .aria_selected(self.selected_session.is_none())
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .group("chats-row")
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .text_color(rgb(MUTED))
                            .child("◯")
                            .child(div().flex_1().child("Chats"))
                            .child(
                                div()
                                    .id("new-chat")
                                    .debug_selector(|| "new-chat".to_owned())
                                    .accessibility_id("new-chat")
                                    .role(Role::Button)
                                    .aria_label("New chat")
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .w(px(20.0))
                                    .h(px(20.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_color(rgb(MUTED))
                                    // Revealed on hover of the row, matching
                                    // how the app surfaces this affordance.
                                    .opacity(0.0)
                                    .group_hover("chats-row", |style| style.opacity(1.0))
                                    .child("+")
                                    .hover(|style| style.bg(rgb(ELEVATED)).text_color(rgb(PRIMARY)))
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.new_chat(cx);
                                    })),
                            )
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.new_chat(cx);
                            })),
                    )
                    .children(chats)
                    .children(projects)
                    .children(sessions)
                    .when(self.projects.is_empty(), |list| {
                        list.child(
                            div()
                                .id("no-projects")
                                .role(Role::Status)
                                .aria_label("No projects configured")
                                .px_3()
                                .py_2()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child("No projects yet. Use Add project below the composer."),
                        )
                    }),
            )
            .children(
                self.restore_failures
                    .iter()
                    .enumerate()
                    .map(|(index, failure)| {
                        div()
                            .id(("restore-failure", index))
                            .role(Role::Alert)
                            .aria_label(format!("Restore failed: {}", failure.error))
                            .text_xs()
                            .text_color(rgb(RED))
                            .child(format!("Restore failed: {}", failure.error))
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("sidebar-footer")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .pb_4()
                    .text_sm()
                    .child(
                        div()
                            .w(px(24.0))
                            .h(px(24.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(ELEVATED))
                            .text_xs()
                            .child("GC"),
                    )
                    .child(div().flex_1().child("Local workspace"))
                    .child(
                        div()
                            .id("settings-button")
                            .accessibility_id("settings-button")
                            .role(Role::Button)
                            .aria_label("Settings")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_color(rgb(MUTED))
                            .child("Settings")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.settings_error = None;
                                view.settings_visibility = SettingsVisibility::Open;
                                cx.notify();
                            })),
                    ),
            )
    }

    /// The scroll handle behind a scrollbar id.
    fn scroll_handle(&self, id: &str) -> Option<gpui::ScrollHandle> {
        self.detail_scrolls.borrow().get(id).cloned()
    }

    /// Move a scrollable region so the pointer position maps to a position in
    /// its content.
    ///
    /// The handle reports its own viewport bounds in window coordinates, which
    /// is what lets a thumb anywhere on screen be dragged without the element
    /// having to measure itself.
    fn drag_scrollbar_to(&self, id: &str, pointer_y: gpui::Pixels, grab_offset: f32) {
        if id == TRANSCRIPT_SCROLL_ID {
            let Some(geometry) = self
                .drawn_transcript_scrollbar
                .or_else(|| self.transcript_scrollbar_geometry())
            else {
                return;
            };
            let local = f32::from(pointer_y - geometry.track_top) - grab_offset;
            let fraction = (local / geometry.usable).clamp(0.0, 1.0);
            self.transcript_list.set_offset_from_scrollbar(gpui::point(
                px(0.0),
                px(-(fraction * geometry.scrollable)),
            ));
            return;
        }
        let Some(handle) = self.scroll_handle(id) else {
            return;
        };
        let Some(geometry) = Self::scrollbar_geometry(&handle) else {
            return;
        };
        let local = f32::from(pointer_y - geometry.track_top) - grab_offset;
        let fraction = (local / geometry.usable).clamp(0.0, 1.0);
        handle.set_offset(gpui::point(
            handle.offset().x,
            px(-(fraction * geometry.scrollable)),
        ));
    }

    /// Where a scrollable region's thumb currently sits.
    fn scrollbar_geometry(handle: &gpui::ScrollHandle) -> Option<ScrollbarGeometry> {
        let bounds = handle.bounds();
        let track = f32::from(bounds.size.height);
        let scrollable = f32::from(handle.max_offset().y);
        if track <= 0.0 || scrollable <= 0.0 {
            return None;
        }

        let thumb = (track * (track / (track + scrollable))).max(MIN_THUMB_HEIGHT);
        let usable = (track - thumb).max(1.0);
        let scrolled = (-f32::from(handle.offset().y) / scrollable).clamp(0.0, 1.0);
        Some(ScrollbarGeometry {
            track_top: bounds.origin.y,
            track,
            thumb_top: scrolled * usable,
            thumb,
            usable,
            scrollable,
        })
    }

    fn transcript_scrollbar_geometry(&self) -> Option<ScrollbarGeometry> {
        let bounds = self.transcript_list.viewport_bounds();
        let track = f32::from(bounds.size.height);
        let scrollable = f32::from(self.transcript_list.max_offset_for_scrollbar().y);
        if track <= 0.0 || scrollable <= 0.0 {
            return None;
        }
        let thumb = (track * (track / (track + scrollable))).max(MIN_THUMB_HEIGHT);
        let usable = (track - thumb).max(1.0);
        let offset = self.transcript_list.scroll_px_offset_for_scrollbar().y;
        let scrolled = (-f32::from(offset) / scrollable).clamp(0.0, 1.0);
        Some(ScrollbarGeometry {
            track_top: bounds.origin.y,
            track,
            thumb_top: scrolled * usable,
            thumb,
            usable,
            scrollable,
        })
    }

    /// Whether the transcript is parked away from its tail, so newer output
    /// sits below the viewport.
    ///
    /// Follow-tail is the source of truth rather than a fresh offset
    /// comparison: the list re-engages it during layout once the true bottom
    /// is reached, which accounts for rows whose height is still an estimate.
    fn transcript_is_away_from_tail(&self) -> bool {
        !self.transcript_list.is_following_tail()
            && self.transcript_list.max_offset_for_scrollbar().y > px(0.0)
    }

    /// Glide the transcript back to the conversation tail.
    ///
    /// The view travels rather than snapping so the content flying past shows
    /// how far it moved, which a jump cut cannot convey.
    fn scroll_transcript_to_bottom(&mut self, cx: &mut Context<Self>) {
        let from = -f32::from(self.transcript_list.scroll_px_offset_for_scrollbar().y);
        self.scroll_to_bottom = Some(ScrollToBottom {
            started: Instant::now(),
            from,
        });
        self.scroll_to_bottom_task = Some(cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(SCROLL_TO_BOTTOM_STEP).await;
                let still_gliding = view.update(cx, |view, cx| {
                    let still_gliding = view.step_scroll_to_bottom(Instant::now());
                    cx.notify();
                    still_gliding
                });
                if !matches!(still_gliding, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Advance the glide, reporting whether it is still running.
    ///
    /// The destination is re-read every step, so output arriving mid-flight
    /// extends the glide instead of leaving it stranded above the new tail.
    fn step_scroll_to_bottom(&mut self, now: Instant) -> bool {
        let Some(glide) = self.scroll_to_bottom else {
            return false;
        };
        let elapsed = now.saturating_duration_since(glide.started).as_secs_f32();
        let progress = (elapsed / SCROLL_TO_BOTTOM_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        if progress >= 1.0 {
            self.scroll_to_bottom = None;
            // The measured maximum trails the true bottom while rows below the
            // viewport are still estimated, so the landing re-engages
            // follow-tail instead of settling on the estimate.
            self.transcript_list.set_follow_mode(FollowMode::Tail);
            return false;
        }
        // Ease out: quick departure, gentle arrival, so the tail is readable
        // the instant it comes to rest.
        let eased = 1.0 - (1.0 - progress).powi(5);
        let target = f32::from(self.transcript_list.max_offset_for_scrollbar().y);
        let offset = glide.from + (target - glide.from) * eased;
        self.transcript_list
            .set_offset_from_scrollbar(gpui::point(px(0.0), px(-offset)));
        true
    }

    /// Abandon a glide in progress, leaving the transcript where it is.
    ///
    /// Scrolling by hand during the glide means the reader wants a different
    /// destination, and fighting them for the scroll position would be worse
    /// than arriving nowhere.
    fn cancel_scroll_to_bottom(&mut self) {
        self.scroll_to_bottom = None;
        self.scroll_to_bottom_task = None;
    }

    /// Begin a scrollbar drag, remembering where the thumb was grabbed.
    ///
    /// Pressing the track jumps the thumb under the pointer; pressing the
    /// thumb keeps it where it is so the content does not lurch on grab.
    fn begin_scrollbar_drag(&mut self, id: &str, pointer_y: gpui::Pixels) {
        if id == TRANSCRIPT_SCROLL_ID {
            // Grabbing the thumb is another way of choosing a destination, so
            // it takes over from a glide rather than competing with it.
            self.cancel_scroll_to_bottom();
            let Some(geometry) = self
                .drawn_transcript_scrollbar
                .or_else(|| self.transcript_scrollbar_geometry())
            else {
                return;
            };
            let local = f32::from(pointer_y - geometry.track_top);
            let within_thumb =
                local >= geometry.thumb_top && local <= geometry.thumb_top + geometry.thumb;
            let grab_offset = if within_thumb {
                local - geometry.thumb_top
            } else {
                geometry.thumb / 2.0
            };
            let current_offset = self.transcript_list.scroll_px_offset_for_scrollbar();
            self.transcript_list.scrollbar_drag_started();
            self.dragging_scrollbar = Some(ScrollbarDrag {
                id: id.to_owned(),
                grab_offset,
            });
            if within_thumb {
                self.transcript_list
                    .set_offset_from_scrollbar(current_offset);
            } else {
                self.drag_scrollbar_to(id, pointer_y, grab_offset);
            }
            return;
        }
        let Some(handle) = self.scroll_handle(id) else {
            return;
        };
        let grab_offset = Self::scrollbar_geometry(&handle).map_or(0.0, |geometry| {
            let local = f32::from(pointer_y - geometry.track_top);
            let within_thumb =
                local >= geometry.thumb_top && local <= geometry.thumb_top + geometry.thumb;
            if within_thumb {
                local - geometry.thumb_top
            } else {
                geometry.thumb / 2.0
            }
        });
        self.dragging_scrollbar = Some(ScrollbarDrag {
            id: id.to_owned(),
            grab_offset,
        });
        self.drag_scrollbar_to(id, pointer_y, grab_offset);
    }

    fn end_scrollbar_drag(&mut self) {
        if self
            .dragging_scrollbar
            .as_ref()
            .is_some_and(|drag| drag.id == TRANSCRIPT_SCROLL_ID)
        {
            self.transcript_list.scrollbar_drag_ended();
        }
        self.dragging_scrollbar = None;
    }

    /// A scrollbar for a scrollable region, shown while the pointer is over it.
    ///
    /// GPUI has no scrollbar element, so this draws the track and thumb and
    /// wires the drag itself.
    fn scrollbar(
        id: &str,
        handle: &gpui::ScrollHandle,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        // Drawn from the same geometry the drag hit-tests against. Computing
        // the two separately let them disagree — different clamps, and one
        // measured against the track while the other measured against the
        // viewport — so a press on the visible thumb was classified as a press
        // on bare track and jumped the content instead of grabbing.
        let geometry = Self::scrollbar_geometry(handle)?;
        Some(Self::scrollbar_element(id, geometry, group, cx))
    }

    fn transcript_scrollbar(
        &mut self,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        self.drawn_transcript_scrollbar = None;
        let geometry = self.transcript_scrollbar_geometry()?;
        self.drawn_transcript_scrollbar = Some(geometry);
        Some(Self::scrollbar_element(
            TRANSCRIPT_SCROLL_ID,
            geometry,
            group,
            cx,
        ))
    }

    fn scrollbar_element(
        id: &str,
        geometry: ScrollbarGeometry,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let track_id = id.to_owned();
        let thumb_id = id.to_owned();

        div()
            .id(SharedString::from(format!("{id}-scrollbar")))
            .debug_selector(|| "scrollbar".to_owned())
            .occlude()
            .absolute()
            .top_0()
            .right_0()
            .w(px(SCROLLBAR_WIDTH))
            .h(px(geometry.track))
            .opacity(0.0)
            .group_hover(group, |style| style.opacity(1.0))
            // Pressing bare track jumps the thumb there and starts a drag.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                    view.begin_scrollbar_drag(&track_id, event.position.y);
                    cx.notify();
                }),
            )
            // The track occludes what is behind it, so a release over the
            // track never reaches the window handler.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    view.end_scrollbar_drag();
                    cx.notify();
                }),
            )
            .child(
                div()
                    // The thumb sits above the track and would otherwise
                    // swallow presses meant for it, so it carries the same
                    // handlers rather than relying on the track's.
                    .id(SharedString::from(format!("{id}-thumb")))
                    .debug_selector(|| "scrollbar-thumb".to_owned())
                    .absolute()
                    .top(px(geometry.thumb_top))
                    .right(px(2.0))
                    .w(px(THUMB_WIDTH))
                    .h(px(geometry.thumb))
                    .rounded_full()
                    .bg(rgb(BORDER))
                    .hover(|style| style.bg(rgb(MUTED)).cursor_pointer())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                            view.begin_scrollbar_drag(&thumb_id, event.position.y);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|view, _, _, cx| {
                            view.end_scrollbar_drag();
                            cx.notify();
                        }),
                    ),
            )
    }

    /// Hand the wheel to a scroll region only while the region can use it.
    ///
    /// GPUI applies a wheel event to every scrollable container under the
    /// pointer and leaves it propagating, so a nested region and the surface
    /// behind it both move. Claiming the event unconditionally is just as
    /// wrong, and is what this replaces: a region with nothing to scroll, or
    /// one already at its extent, swallowed the wheel and the surface behind
    /// it never moved.
    ///
    /// GPUI has already applied this event by the time the returned listener
    /// runs, so comparing where the region sat before with where it sits now
    /// says whether it consumed the gesture.
    fn claim_scroll_when_moved(
        &self,
        id: &str,
        handle: &gpui::ScrollHandle,
        cx: &Context<Self>,
    ) -> ScrollWheelGuard {
        self.scroll_positions
            .borrow_mut()
            .insert(id.to_owned(), Self::clamped_offset(handle));
        let id = id.to_owned();
        let handle = handle.clone();
        Box::new(cx.listener(move |view, _: &gpui::ScrollWheelEvent, _, cx| {
            let after = Self::clamped_offset(&handle);
            let before = view
                .scroll_positions
                .borrow()
                .get(&id)
                .copied()
                .unwrap_or(after);
            view.scroll_positions.borrow_mut().insert(id.clone(), after);
            // GPUI lets the offset run past the extent and only clamps it at
            // the next paint, which would read as movement next time.
            if handle.offset() != after {
                handle.set_offset(after);
            }
            if before != after {
                cx.stop_propagation();
            }
        }))
    }

    /// Where a scroll region sits, with the overscroll GPUI allows removed.
    fn clamped_offset(handle: &gpui::ScrollHandle) -> gpui::Point<gpui::Pixels> {
        let max = handle.max_offset();
        let offset = handle.offset();
        gpui::point(
            offset.x.clamp(-max.x, px(0.0)),
            offset.y.clamp(-max.y, px(0.0)),
        )
    }

    /// Claim the wheel for a horizontally scrolling region, but only for a
    /// horizontal gesture.
    ///
    /// Code blocks, tables, and diffs scroll sideways inside surfaces that
    /// scroll down. Paired with `restrict_scroll_to_axis`, this keeps a
    /// vertical wheel over one of them scrolling the surface behind it rather
    /// than being swallowed or, worse, turned into sideways movement.
    fn claim_horizontal_scroll(event: &gpui::ScrollWheelEvent, window: &mut Window, cx: &mut App) {
        let delta = event.delta.pixel_delta(window.line_height());
        if delta.x != px(0.0) && delta.x.abs() > delta.y.abs() {
            cx.stop_propagation();
        }
    }

    /// A bounded, scrollable block of detail inside a tool entry.
    ///
    /// Commands, diffs, and output are frequently taller than any sensible
    /// entry. Clipping them hid the interesting part; scrolling keeps the
    /// entry compact while leaving the whole thing reachable.
    ///
    /// The block consumes its own wheel events so scrolling inside it does not
    /// also scroll the transcript behind it, and draws a thumb on hover
    /// because there is no platform scrollbar behind an overflow container.
    fn detail_block(
        &self,
        id: &str,
        content: String,
        max_height: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let handle = self
            .detail_scrolls
            .borrow_mut()
            .entry(id.to_owned())
            .or_default()
            .clone();
        let previous_extent = self
            .detail_extents
            .borrow_mut()
            .insert(id.to_owned(), content.len());
        let at_tail = f32::from(handle.max_offset().y) + f32::from(handle.offset().y) <= 1.0;
        if previous_extent.is_none_or(|previous| content.len() > previous) && at_tail {
            handle.scroll_to_bottom();
        }

        let group = SharedString::from(format!("scroll-{id}"));
        let claim_scroll = self.claim_scroll_when_moved(id, &handle, cx);
        let scrollbar = Self::scrollbar(id, &handle, group.clone(), cx);

        div()
            .id(SharedString::from(format!("{id}-frame")))
            .group(group)
            .relative()
            .mt_1()
            .w_full()
            .child(
                div()
                    .id(SharedString::from(id.to_owned()))
                    .debug_selector(|| "tool-detail".to_owned())
                    .track_scroll(&handle)
                    .max_h(px(max_height))
                    .w_full()
                    .min_w_0()
                    .overflow_x_scroll()
                    .overflow_y_scroll()
                    // Without this the transcript scrolls too, so reading a
                    // command's output dragged the whole conversation along.
                    // Claiming it outright was its own bug: a block short
                    // enough to need no scrolling, or already at its end, left
                    // the transcript stuck under the pointer.
                    .on_scroll_wheel(claim_scroll)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(content),
            )
            .children(scrollbar)
    }

    fn tool_expanded(&self, call_id: &str) -> bool {
        self.selected_session.as_ref().is_some_and(|session_id| {
            self.expanded_tools
                .get(session_id)
                .is_some_and(|expanded| expanded.contains(call_id))
        })
    }

    fn toggle_tool(&mut self, call_id: &str) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let expanded = self.expanded_tools.entry(session_id).or_default();
        if !expanded.remove(call_id) {
            expanded.insert(call_id.to_owned());
        }
    }

    fn terminal_is_running(&self, invocation: &app_model::ToolInvocation) -> bool {
        if invocation.class != app_model::ToolClass::Shell {
            return false;
        }
        invocation
            .shell_id
            .as_deref()
            .and_then(|shell_id| self.selected()?.snapshot.tool_activity.terminal(shell_id))
            .map_or(
                invocation.state == app_model::InvocationState::Running,
                app_model::TerminalSession::is_active,
            )
    }

    fn tool_icon(class: app_model::ToolClass) -> &'static str {
        match class {
            app_model::ToolClass::FileRead => "◇",
            app_model::ToolClass::FileWrite | app_model::ToolClass::FileEditor => "±",
            app_model::ToolClass::Search => "⌕",
            app_model::ToolClass::Shell | app_model::ToolClass::ShellControl => ">_",
            app_model::ToolClass::Web => "↗",
            app_model::ToolClass::Delegation => "◎",
            app_model::ToolClass::Data => "▤",
            app_model::ToolClass::Skill => "◆",
            app_model::ToolClass::Interaction => "?",
            app_model::ToolClass::Other => "•",
        }
    }

    fn tool_argument_detail(
        invocation: &app_model::ToolInvocation,
    ) -> Option<(&'static str, String)> {
        let named = match invocation.class {
            app_model::ToolClass::FileRead
            | app_model::ToolClass::FileWrite
            | app_model::ToolClass::FileEditor => invocation
                .string_argument("path")
                .map(|value| ("Path", value)),
            app_model::ToolClass::Search => invocation
                .string_argument("pattern")
                .or_else(|| invocation.string_argument("query"))
                .map(|value| ("Pattern", value)),
            app_model::ToolClass::Shell | app_model::ToolClass::ShellControl => invocation
                .string_argument("command")
                .or_else(|| invocation.display_command.clone())
                .map(|value| ("Command", value)),
            app_model::ToolClass::Web => invocation
                .string_argument("url")
                .or_else(|| invocation.string_argument("query"))
                .map(|value| ("Request", value)),
            app_model::ToolClass::Delegation
            | app_model::ToolClass::Skill
            | app_model::ToolClass::Interaction => invocation
                .string_argument("description")
                .or_else(|| invocation.string_argument("prompt"))
                .map(|value| ("Prompt", value)),
            app_model::ToolClass::Data | app_model::ToolClass::Other => None,
        };
        named.or_else(|| {
            (!invocation.arguments.is_null()
                && invocation
                    .arguments
                    .as_object()
                    .is_none_or(|arguments| !arguments.is_empty()))
            .then(|| {
                (
                    "Arguments",
                    serde_json::to_string_pretty(&invocation.arguments)
                        .unwrap_or_else(|_| invocation.arguments.to_string()),
                )
            })
        })
    }

    fn tool_diff_counts(diff: &str) -> (usize, usize) {
        diff.lines().fold((0, 0), |(insertions, deletions), line| {
            if line.starts_with('+') && !line.starts_with("+++") {
                (insertions + 1, deletions)
            } else if line.starts_with('-') && !line.starts_with("---") {
                (insertions, deletions + 1)
            } else {
                (insertions, deletions)
            }
        })
    }

    fn reported_match_count(output: &str) -> Option<usize> {
        let lowercase = output.to_ascii_lowercase();
        if ["no matches", "no results", "no files matched"]
            .iter()
            .any(|marker| lowercase.contains(marker))
        {
            return Some(0);
        }
        let prefix = output.split_once(" match")?.0;
        prefix
            .split(|character: char| !character.is_ascii_digit())
            .rfind(|part| !part.is_empty())?
            .parse()
            .ok()
    }

    fn tool_brief(invocation: &app_model::ToolInvocation, has_diff: bool) -> Option<String> {
        match invocation.state {
            app_model::InvocationState::Running => return Some("Working".to_owned()),
            app_model::InvocationState::Failed => return Some("Failed".to_owned()),
            app_model::InvocationState::Cancelled => return Some("Cancelled".to_owned()),
            app_model::InvocationState::Succeeded => {}
        }
        if has_diff {
            return None;
        }
        if !invocation.output.is_empty() {
            let count = if invocation.class == app_model::ToolClass::Search {
                Self::reported_match_count(&invocation.output)
                    .unwrap_or_else(|| invocation.output.lines().count())
            } else {
                invocation.output.lines().count()
            };
            let noun = if invocation.class == app_model::ToolClass::Search {
                if count == 1 { "result" } else { "results" }
            } else if count == 1 {
                "line"
            } else {
                "lines"
            };
            return Some(format!("{count} {noun}"));
        }
        tool_duration(invocation)
            .map(|duration| format!("Worked for {}", format_activity_duration(duration)))
    }

    /// A tool call in the timeline, followed by independently expandable
    /// activity rows for work delegated to a subagent.
    fn tool_entry(
        &self,
        invocation: &app_model::ToolInvocation,
        children: &[&app_model::ToolInvocation],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let nested = children
            .iter()
            .map(|child| self.tool_activity_entry(child, true, cx))
            .collect::<Vec<_>>();
        div()
            .id(SharedString::from(format!("tool-{}", invocation.call_id)))
            .debug_selector(|| "tool-entry".to_owned())
            .accessibility_id(invocation.call_id.clone())
            .role(Role::ListItem)
            .aria_label(format!(
                "{} {}",
                invocation.verb(),
                invocation.summary_line()
            ))
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .child(self.tool_activity_entry(invocation, false, cx))
            .when(!nested.is_empty(), |entry| {
                entry.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .border_l_1()
                        .border_color(rgb(BORDER))
                        .children(nested),
                )
            })
            .into_any_element()
    }

    /// One compact activity row and its optional GCABB detail card.
    #[allow(clippy::too_many_lines)]
    fn tool_activity_entry(
        &self,
        invocation: &app_model::ToolInvocation,
        nested: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (status, status_color) = match invocation.state {
            app_model::InvocationState::Running => ("running", GREEN),
            app_model::InvocationState::Succeeded => ("done", MUTED),
            app_model::InvocationState::Failed => ("failed", RED),
            app_model::InvocationState::Cancelled => ("cancelled", MUTED),
        };
        let full_summary = invocation.file_path().map_or_else(
            || invocation.summary(),
            |path| self.display_worktree_path(Path::new(path)),
        );
        let summary = summary_line(&full_summary);
        let verb = invocation.verb();
        let label = format!("{verb} {summary}");
        let diff = invocation.diff().map(str::to_owned);
        let diff_counts = diff.as_deref().map(Self::tool_diff_counts);
        let brief = Self::tool_brief(invocation, diff.is_some());
        let accessible_brief = diff_counts.map_or_else(
            || brief.clone().unwrap_or_default(),
            |(insertions, deletions)| format!("{insertions} additions, {deletions} deletions"),
        );
        let error = invocation.error_message.clone();
        let output_error = invocation
            .output_load_error
            .clone()
            .or_else(|| invocation.output_error.clone());
        // Restored command output is already a bounded chunk window. When the
        // user explicitly prepends older windows, keep them reachable here.
        let has_output = !invocation.output.is_empty() && diff.is_none();
        let output_is_large = has_output && output_needs_preview(&invocation.output);
        let output_is_expanded = invocation.state != app_model::InvocationState::Running
            && self.expanded_tool_outputs.contains(&invocation.call_id);
        let output = has_output.then(|| {
            if invocation.state == app_model::InvocationState::Running
                || (output_is_large && !output_is_expanded)
            {
                live_output_preview(&invocation.output)
            } else {
                invocation.output.clone()
            }
        });
        let output_toggle = (output_is_large
            && invocation.state != app_model::InvocationState::Running)
            .then(|| (invocation.call_id.clone(), output_is_expanded));
        let earlier_output = (invocation.output_start_chunk > 0).then(|| {
            (
                self.selected_session.clone().unwrap_or_default(),
                invocation.call_id.clone(),
                invocation.output_start_chunk,
            )
        });
        let exit = invocation
            .exit_code
            .filter(|code| *code != 0)
            .map(|code| format!("exit {code}"));
        let argument_detail = Self::tool_argument_detail(invocation);
        let terminal_running = self.terminal_is_running(invocation);
        let expanded = terminal_running || self.tool_expanded(&invocation.call_id);
        let call_id = invocation.call_id.clone();
        let selector_call_id = invocation.call_id.clone();
        let disclosure_label = if terminal_running {
            format!("Details for {label}, expanded while terminal is running")
        } else if expanded {
            format!("Collapse details for {label}")
        } else {
            format!("Expand details for {label}")
        };
        let started = format_activity_timestamp(&invocation.started_at);
        let duration = tool_duration(invocation).map(format_activity_duration);
        let output_metadata = has_output.then(|| {
            let lines = invocation.output.lines().count();
            format!(
                "{} {} · {}",
                lines,
                if lines == 1 { "line" } else { "lines" },
                format_byte_count(invocation.output_metadata.byte_count)
            )
        });
        let detail_card = div()
            .debug_selector(|| "tool-expanded-card".to_owned())
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .mt_1()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(SUBTLE))
            .overflow_hidden()
            .border_1()
            .border_color(rgb(
                if invocation.state == app_model::InvocationState::Failed {
                    RED
                } else {
                    BORDER
                },
            ))
            .when_some(exit, |entry, exit| {
                entry.child(div().text_xs().text_color(rgb(RED)).child(exit))
            })
            .when_some(error, |entry, error| {
                entry.child(div().text_xs().text_color(rgb(RED)).child(error))
            })
            .when_some(output_error, |entry, error| {
                entry.child(
                    div()
                        .id(SharedString::from(format!(
                            "tool-output-error-{}",
                            invocation.call_id
                        )))
                        .role(Role::Alert)
                        .text_xs()
                        .text_color(rgb(RED))
                        .child(format!("Output unavailable: {error}")),
                )
            })
            .when_some(argument_detail, |entry, (argument_label, detail)| {
                entry
                    .child(div().text_xs().text_color(rgb(MUTED)).child(argument_label))
                    .child(self.detail_block(
                        &format!("tool-argument-{}", invocation.call_id),
                        detail,
                        COMMAND_BLOCK_HEIGHT,
                        cx,
                    ))
            })
            .when_some(diff, |entry, diff| {
                entry.child(self.detail_block(
                    &format!("tool-diff-{}", invocation.call_id),
                    diff,
                    ENTRY_DETAIL_BUDGET - COMMAND_BLOCK_HEIGHT,
                    cx,
                ))
            })
            .when_some(
                earlier_output,
                |entry, (session_id, identity, before_chunk)| {
                    entry.child(
                        div()
                            .id(SharedString::from(format!(
                                "load-output-{}",
                                invocation.call_id
                            )))
                            .role(Role::Button)
                            .aria_label("Load earlier retained output")
                            .focusable()
                            .tab_stop(true)
                            .mt_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_xs()
                            .text_color(rgb(BLUE))
                            .child(format!(
                                "Load earlier output ({before_chunk} chunks retained)"
                            ))
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                let _ = view.commands.send(ServiceCommand::LoadEarlierOutput {
                                    app_session_id: session_id.clone(),
                                    identity: identity.clone(),
                                    before_chunk,
                                });
                                cx.notify();
                            })),
                    )
                },
            )
            .when_some(output, |entry, output| {
                entry.child(self.detail_block(
                    &format!("tool-output-{}", invocation.call_id),
                    output,
                    ENTRY_DETAIL_BUDGET - COMMAND_BLOCK_HEIGHT,
                    cx,
                ))
            })
            .when_some(output_toggle, |entry, (call_id, expanded)| {
                entry.child(
                    div()
                        .id(SharedString::from(format!("toggle-output-{call_id}")))
                        .debug_selector(|| "toggle-tool-output".to_owned())
                        .role(Role::Button)
                        .aria_label(if expanded {
                            "Show latest output"
                        } else {
                            "Show complete output"
                        })
                        .focusable()
                        .tab_stop(true)
                        .mt_1()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .text_xs()
                        .text_color(rgb(BLUE))
                        .child(if expanded {
                            "Show latest output".to_owned()
                        } else {
                            format!(
                                "Show complete output ({} bytes)",
                                invocation.output_metadata.byte_count
                            )
                        })
                        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if expanded {
                                view.expanded_tool_outputs.remove(&call_id);
                            } else {
                                view.expanded_tool_outputs.insert(call_id.clone());
                            }
                            cx.notify();
                        })),
                )
            })
            .child(
                div()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!("Started {started}"))
                    .when_some(duration, |metadata, duration| {
                        metadata.child(format!("Duration {duration}"))
                    })
                    .when_some(output_metadata, |metadata, output| {
                        metadata.child(format!("Output {output}"))
                    }),
            );

        div()
            .id(SharedString::from(format!(
                "tool-activity-{}",
                invocation.call_id
            )))
            .debug_selector(move || format!("tool-toggle-{selector_call_id}"))
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .child(
                div()
                    .id(SharedString::from(format!(
                        "tool-toggle-{}",
                        invocation.call_id
                    )))
                    .debug_selector(|| "tool-card".to_owned())
                    .role(Role::Button)
                    .aria_label(if accessible_brief.is_empty() {
                        format!("{disclosure_label} ({status})")
                    } else {
                        format!("{disclosure_label} ({status}), {accessible_brief}")
                    })
                    .aria_expanded(expanded)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.rounded_sm().border_1().border_color(rgb(BLUE)))
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl(px(if nested { 24.0 } else { 0.0 }))
                    .py_1()
                    .rounded_sm()
                    .child(
                        div()
                            .w(px(20.0))
                            .flex_none()
                            .when(invocation.class == app_model::ToolClass::Search, |icon| {
                                icon.text_lg()
                            })
                            .when(invocation.class != app_model::ToolClass::Search, |icon| {
                                icon.text_xs()
                            })
                            .text_color(rgb(status_color))
                            .child(Self::tool_icon(invocation.class)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .text_color(rgb(BLUE))
                            .child(verb.to_owned()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(summary),
                    )
                    .when_some(diff_counts, |row, (insertions, deletions)| {
                        row.child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(GREEN))
                                .child(format!("+{insertions}")),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(RED))
                                .child(format!("-{deletions}")),
                        )
                    })
                    .when_some(brief, |row, brief| {
                        row.child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child(brief),
                        )
                    })
                    .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if !terminal_running {
                            view.toggle_tool(&call_id);
                        }
                        cx.notify();
                    })),
            )
            .when(expanded, |entry| {
                entry.child(
                    div()
                        .w_full()
                        .pl(px(if nested { 52.0 } else { 28.0 }))
                        .child(detail_card),
                )
            })
            .into_any_element()
    }

    /// One conversation message.
    /// Chips for what a message was sent with, clickable when previewable.
    fn message_attachment_chips(
        message: &app_model::TranscriptMessage,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        message
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                let accessible_id = format!("message-attachment-{}-{index}", message.id);
                let accessible_label = attachment.display_name.clone();
                // Only an image backed by a file the runtime kept can be
                // shown; a name alone is not enough to load pixels.
                let preview = attachment
                    .is_image
                    .then(|| attachment.path.clone())
                    .flatten()
                    .map(|path| ImagePreview {
                        title: attachment.display_name.clone(),
                        source: PreviewSource::Path(PathBuf::from(path)),
                    });
                div()
                    .id(SharedString::from(accessible_id.clone()))
                    .debug_selector(|| "message-attachment".to_owned())
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(SUBTLE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .when_some(preview, |chip, preview| {
                        chip.accessibility_id(accessible_id)
                            .role(Role::Button)
                            .aria_label(format!("Preview {accessible_label}"))
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .hover(|style| style.border_color(rgb(BLUE)).cursor_pointer())
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open_image_preview(preview.clone(), window, cx);
                            }))
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(if attachment.is_image { "IMG" } else { "FILE" }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(PRIMARY))
                            .child(attachment.display_name.clone()),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn collect_markdown_inline(
        nodes: &[MarkdownNode],
        style: &MarkdownInlineStyle,
        content: &mut MarkdownInlineContent,
    ) {
        for node in nodes {
            match node {
                MarkdownNode::Container(tag, children) => {
                    let mut child_style = style.clone();
                    match tag {
                        MarkdownTag::Strong => child_style.marks |= MARKDOWN_STRONG,
                        MarkdownTag::Emphasis => child_style.marks |= MARKDOWN_EMPHASIS,
                        MarkdownTag::Strikethrough => {
                            child_style.marks |= MARKDOWN_STRIKETHROUGH;
                        }
                        MarkdownTag::Link(target) | MarkdownTag::Image(target) => {
                            child_style.link = safe_markdown_url(target);
                        }
                        _ => {}
                    }
                    Self::collect_markdown_inline(children, &child_style, content);
                }
                MarkdownNode::Text(text) | MarkdownNode::Html(text) => {
                    content.push(text, style);
                }
                MarkdownNode::Code(text) => {
                    let mut code_style = style.clone();
                    code_style.code = true;
                    code_style.monospace = true;
                    content.push(text, &code_style);
                }
                MarkdownNode::SoftBreak => {
                    content.push(" ", style);
                }
                MarkdownNode::HardBreak => {
                    content.push("\n", style);
                }
                MarkdownNode::TaskMarker(checked) => {
                    let mut marker_style = style.clone();
                    marker_style.monospace = true;
                    content.push(if *checked { "[x] " } else { "[ ] " }, &marker_style);
                }
                MarkdownNode::Rule => {}
            }
        }
    }

    fn markdown_inline_content(nodes: &[MarkdownNode]) -> MarkdownInlineContent {
        let mut content = MarkdownInlineContent::default();
        Self::collect_markdown_inline(nodes, &MarkdownInlineStyle::default(), &mut content);
        content
    }

    fn markdown_inline_block(
        nodes: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
    ) -> gpui::AnyElement {
        let content = Self::markdown_inline_content(nodes);
        let inline_index = *element_index;
        *element_index += 1;
        let message_order = selection
            .borrow()
            .message_orders
            .get(message_id)
            .copied()
            .unwrap_or_default();
        let text = SelectableTranscriptText::new(
            format!("markdown-{message_id}-{inline_index}"),
            (message_order, inline_index),
            content,
            selection.clone(),
            selection_focus.clone(),
        );
        let selector = format!("markdown-inline-{message_id}-{inline_index}");
        div()
            .debug_selector(move || selector.clone())
            .cursor(CursorStyle::IBeam)
            .min_w_0()
            .child(
                div()
                    .debug_selector(|| "markdown-inline".to_owned())
                    .min_w_0()
                    .child(text),
            )
            .into_any_element()
    }

    fn markdown_table_section(
        nodes: &[MarkdownNode],
        header: bool,
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
    ) -> Vec<gpui::AnyElement> {
        if header
            && nodes
                .iter()
                .all(|node| matches!(node, MarkdownNode::Container(MarkdownTag::TableCell, _)))
        {
            return vec![Self::markdown_table_row(
                nodes,
                true,
                message_id,
                element_index,
                selection,
                selection_focus,
            )];
        }

        nodes
            .iter()
            .filter_map(|node| {
                let MarkdownNode::Container(MarkdownTag::TableRow, cells) = node else {
                    return None;
                };
                Some(Self::markdown_table_row(
                    cells,
                    header,
                    message_id,
                    element_index,
                    selection,
                    selection_focus,
                ))
            })
            .collect()
    }

    fn markdown_table_row(
        cells: &[MarkdownNode],
        header: bool,
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .min_w_full()
            .children(cells.iter().filter_map(|cell| {
                let MarkdownNode::Container(MarkdownTag::TableCell, content) = cell else {
                    return None;
                };
                Some(
                    div()
                        .min_w(px(120.))
                        .flex_1()
                        .p_2()
                        .border_b_1()
                        .border_r_1()
                        .border_color(rgb(BORDER))
                        .when(header, |cell| {
                            cell.bg(rgb(SUBTLE)).font_weight(gpui::FontWeight::SEMIBOLD)
                        })
                        .child(Self::markdown_inline_block(
                            content,
                            message_id,
                            element_index,
                            selection,
                            selection_focus,
                        )),
                )
            }))
            .into_any_element()
    }

    fn markdown_heading(
        level: u8,
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
    ) -> gpui::AnyElement {
        div()
            .mt_1()
            .when(level <= 2, |heading| {
                heading.text_xl().font_weight(gpui::FontWeight::BOLD)
            })
            .when(level == 3, |heading| {
                heading.text_lg().font_weight(gpui::FontWeight::BOLD)
            })
            .when(level >= 4, |heading| {
                heading.font_weight(gpui::FontWeight::SEMIBOLD)
            })
            .child(Self::markdown_inline_block(
                children,
                message_id,
                element_index,
                selection,
                selection_focus,
            ))
            .into_any_element()
    }

    fn markdown_quote(
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .pl_3()
            .border_l_2()
            .border_color(rgb(MUTED))
            .text_color(rgb(MUTED))
            .flex()
            .flex_col()
            .gap_2()
            .children(Self::markdown_blocks(
                children,
                message_id,
                element_index,
                selection,
                selection_focus,
                cx,
            ))
            .into_any_element()
    }

    fn markdown_code_block(
        language: Option<&String>,
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let code = markdown::plain_text(children);
        let copy = code.clone();
        let block_index = *element_index;
        *element_index += 1;
        let message_order = selection
            .borrow()
            .message_orders
            .get(message_id)
            .copied()
            .unwrap_or_default();
        let code_len = code.len();
        let selectable_code = SelectableTranscriptText::new(
            format!("markdown-code-{message_id}-{block_index}"),
            (message_order, block_index),
            MarkdownInlineContent {
                text: code,
                highlights: Vec::new(),
                font_family_overrides: vec![(0..code_len, ".ZedMono".into())],
                links: Vec::new(),
            },
            selection.clone(),
            selection_focus.clone(),
        );
        div()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(SUBTLE))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(language.cloned().unwrap_or_else(|| "code".to_owned()))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "copy-code-{message_id}-{block_index}"
                            )))
                            .role(Role::Button)
                            .aria_label("Copy code")
                            .focusable()
                            .tab_stop(true)
                            .px_2()
                            .rounded_sm()
                            .child("Copy")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    copy.clone(),
                                ));
                            })),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!(
                        "code-content-{message_id}-{block_index}"
                    )))
                    .debug_selector(|| "markdown-code".to_owned())
                    .p_3()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .on_scroll_wheel(Self::claim_horizontal_scroll)
                    .whitespace_nowrap()
                    .font_family(".ZedMono")
                    .text_sm()
                    .child(selectable_code),
            )
            .into_any_element()
    }

    fn markdown_list(
        start: Option<u64>,
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let first = start.unwrap_or(1);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(children.iter().enumerate().map(|(index, child)| {
                let marker = if start.is_some() {
                    format!("{}.", first + u64::try_from(index).unwrap_or(0))
                } else {
                    "•".to_owned()
                };
                let content = match child {
                    MarkdownNode::Container(MarkdownTag::Item, content) => content,
                    _ => std::slice::from_ref(child),
                };
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .w(px(24.))
                            .flex_shrink_0()
                            .text_color(rgb(MUTED))
                            .child(marker),
                    )
                    .child(div().min_w_0().flex_1().flex().flex_col().gap_1().children(
                        Self::markdown_blocks(
                            content,
                            message_id,
                            element_index,
                            selection,
                            selection_focus,
                            cx,
                        ),
                    ))
            }))
            .into_any_element()
    }

    fn markdown_table(
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
    ) -> gpui::AnyElement {
        let table_index = *element_index;
        *element_index += 1;
        let mut rows = Vec::new();
        for child in children {
            match child {
                MarkdownNode::Container(MarkdownTag::TableHead, head) => {
                    rows.extend(Self::markdown_table_section(
                        head,
                        true,
                        message_id,
                        element_index,
                        selection,
                        selection_focus,
                    ));
                }
                MarkdownNode::Container(MarkdownTag::TableRow, _) => {
                    rows.extend(Self::markdown_table_section(
                        std::slice::from_ref(child),
                        false,
                        message_id,
                        element_index,
                        selection,
                        selection_focus,
                    ));
                }
                _ => {}
            }
        }
        div()
            .id(SharedString::from(format!(
                "markdown-table-{message_id}-{table_index}"
            )))
            .debug_selector(|| "markdown-table".to_owned())
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .on_scroll_wheel(Self::claim_horizontal_scroll)
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .children(rows)
            .into_any_element()
    }

    fn markdown_block(
        node: &MarkdownNode,
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match node {
            MarkdownNode::Container(MarkdownTag::Paragraph, children) => {
                Self::markdown_inline_block(
                    children,
                    message_id,
                    element_index,
                    selection,
                    selection_focus,
                )
            }
            MarkdownNode::Container(MarkdownTag::Heading(level), children) => {
                Self::markdown_heading(
                    *level,
                    children,
                    message_id,
                    element_index,
                    selection,
                    selection_focus,
                )
            }
            MarkdownNode::Container(MarkdownTag::BlockQuote, children) => Self::markdown_quote(
                children,
                message_id,
                element_index,
                selection,
                selection_focus,
                cx,
            ),
            MarkdownNode::Container(MarkdownTag::CodeBlock(language), children) => {
                Self::markdown_code_block(
                    language.as_ref(),
                    children,
                    message_id,
                    element_index,
                    selection,
                    selection_focus,
                    cx,
                )
            }
            MarkdownNode::Container(MarkdownTag::List(start), children) => Self::markdown_list(
                *start,
                children,
                message_id,
                element_index,
                selection,
                selection_focus,
                cx,
            ),
            MarkdownNode::Container(MarkdownTag::Table, children) => Self::markdown_table(
                children,
                message_id,
                element_index,
                selection,
                selection_focus,
            ),
            MarkdownNode::Rule => div()
                .w_full()
                .h(px(1.))
                .my_2()
                .bg(rgb(BORDER))
                .into_any_element(),
            MarkdownNode::Container(_, children) => div()
                .flex()
                .flex_col()
                .gap_1()
                .children(Self::markdown_blocks(
                    children,
                    message_id,
                    element_index,
                    selection,
                    selection_focus,
                    cx,
                ))
                .into_any_element(),
            _ => Self::markdown_inline_block(
                std::slice::from_ref(node),
                message_id,
                element_index,
                selection,
                selection_focus,
            ),
        }
    }

    /// Whether a node belongs to the inline text flow rather than being a
    /// block of its own. Markdown lists and other containers frequently hold
    /// bare inline nodes (a "tight" list item is not wrapped in a paragraph),
    /// so these have to be regrouped into one text run before rendering or
    /// every emphasis, link, and inline code span would land on its own line.
    fn is_markdown_inline(node: &MarkdownNode) -> bool {
        match node {
            MarkdownNode::Container(tag, _) => matches!(
                tag,
                MarkdownTag::Emphasis
                    | MarkdownTag::Strong
                    | MarkdownTag::Strikethrough
                    | MarkdownTag::Link(_)
                    | MarkdownTag::Image(_)
            ),
            MarkdownNode::Text(_)
            | MarkdownNode::Code(_)
            | MarkdownNode::Html(_)
            | MarkdownNode::SoftBreak
            | MarkdownNode::HardBreak
            | MarkdownNode::TaskMarker(_) => true,
            MarkdownNode::Rule => false,
        }
    }

    /// Splits sibling nodes into stretches of adjacent inline nodes and
    /// standalone block nodes, dropping inline stretches that carry no visible
    /// text (whitespace separators between blocks).
    fn markdown_runs(nodes: &[MarkdownNode]) -> Vec<MarkdownRun> {
        let mut runs = Vec::new();
        let mut index = 0;
        while index < nodes.len() {
            if Self::is_markdown_inline(&nodes[index]) {
                let start = index;
                while index < nodes.len() && Self::is_markdown_inline(&nodes[index]) {
                    index += 1;
                }
                if !markdown::plain_text(&nodes[start..index]).trim().is_empty() {
                    runs.push(MarkdownRun::Inline(start..index));
                }
            } else {
                runs.push(MarkdownRun::Block(index));
                index += 1;
            }
        }
        runs
    }

    /// Renders a sequence of sibling nodes, merging each stretch of adjacent
    /// inline nodes into a single wrapped text block.
    fn markdown_blocks(
        nodes: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        Self::markdown_runs(nodes)
            .into_iter()
            .map(|run| match run {
                MarkdownRun::Inline(range) => Self::markdown_inline_block(
                    &nodes[range],
                    message_id,
                    element_index,
                    selection,
                    selection_focus,
                ),
                MarkdownRun::Block(index) => Self::markdown_block(
                    &nodes[index],
                    message_id,
                    element_index,
                    selection,
                    selection_focus,
                    cx,
                ),
            })
            .collect()
    }

    fn markdown_content(
        message_id: &str,
        document: &MarkdownDocument,
        selection: &Rc<RefCell<TranscriptTextSelection>>,
        selection_focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut element_index = 0;
        div()
            .id(SharedString::from(format!("markdown-content-{message_id}")))
            .debug_selector(|| "markdown-content".to_owned())
            .flex()
            .flex_col()
            .gap_2()
            .children(Self::markdown_blocks(
                &document.children,
                message_id,
                &mut element_index,
                selection,
                selection_focus,
                cx,
            ))
            .into_any_element()
    }

    fn message_markdown(
        &mut self,
        message: &app_model::TranscriptMessage,
    ) -> Arc<MarkdownDocument> {
        if message.state != TranscriptState::Complete {
            return Arc::new(markdown::parse(&message.content));
        }
        if !self.markdown_cache.contains_key(&message.id) {
            if self.markdown_cache.len() == MARKDOWN_CACHE_CAPACITY
                && let Some(evicted) = self.markdown_cache_order.pop_front()
            {
                self.markdown_cache.remove(&evicted);
            }
            self.markdown_cache_order.push_back(message.id.clone());
        }
        let cached = self
            .markdown_cache
            .entry(message.id.clone())
            .or_insert_with(|| CachedMarkdown {
                source: message.content.clone(),
                document: Arc::new(markdown::parse(&message.content)),
            });
        if cached.source != message.content {
            cached.source.clone_from(&message.content);
            cached.document = Arc::new(markdown::parse(&message.content));
        }
        cached.document.clone()
    }

    fn message_aria_label(message: &app_model::TranscriptMessage) -> String {
        let speaker = if message.role == TranscriptRole::User {
            "You"
        } else {
            "Copilot"
        };
        let pending = if message.state == TranscriptState::Pending {
            " (pending acknowledgement)"
        } else {
            ""
        };
        format!("{speaker}{pending}: {}", message.content)
    }

    fn copy_icon(surface: u32) -> impl IntoElement {
        div()
            .relative()
            .w(px(16.0))
            .h(px(16.0))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(10.0))
                    .h(px(10.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(MUTED)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(4.0))
                    .left(px(4.0))
                    .w(px(10.0))
                    .h(px(10.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(MUTED))
                    .bg(rgb(surface)),
            )
    }

    fn transcript_copy_button(
        message: &app_model::TranscriptMessage,
        is_user: bool,
        markdown_source: String,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let surface = if is_user { ELEVATED } else { BACKGROUND };
        div()
            .id(SharedString::from(format!("copy-markdown-{}", message.id)))
            .debug_selector(|| "copy-markdown".to_owned())
            .role(Role::Button)
            .aria_label(if is_user {
                "Copy your message"
            } else {
                "Copy Copilot message"
            })
            .focusable()
            .tab_stop(true)
            .focus_visible(|style| style.opacity(1.0).border_color(rgb(BLUE)))
            .absolute()
            .top(px(if is_user { 8.0 } else { 0.0 }))
            .right(px(if is_user { 8.0 } else { 0.0 }))
            .w(px(28.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(rgb(surface))
            .bg(rgb(surface))
            .opacity(0.0)
            .group_hover(group, |style| style.opacity(1.0))
            .child(Self::copy_icon(surface))
            .hover(|style| style.border_color(rgb(BORDER)).cursor_pointer())
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(markdown_source.clone()));
            }))
            .into_any_element()
    }

    fn transcript_message(
        &mut self,
        message: &app_model::TranscriptMessage,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_user = message.role == TranscriptRole::User;
        let attachments = Self::message_attachment_chips(message, cx);
        let markdown_source = message.content.clone();
        let document = self.message_markdown(message);
        self.transcript_selection
            .borrow_mut()
            .message_orders
            .insert(message.id.clone(), message.sequence);
        let markdown = Self::markdown_content(
            &message.id,
            &document,
            &self.transcript_selection,
            &self.transcript_selection_focus,
            cx,
        );
        let group = SharedString::from(format!("message-hover-{}", message.id));
        let copy =
            Self::transcript_copy_button(message, is_user, markdown_source, group.clone(), cx);
        div()
            .id(SharedString::from(format!("message-{}", message.id)))
            .accessibility_id(message.id.clone())
            .role(Role::ListItem)
            .aria_label(Self::message_aria_label(message))
            .flex()
            .w_full()
            .justify_end()
            .when(!is_user, gpui::Styled::justify_start)
            .child(
                div()
                    .debug_selector(|| "transcript-message".to_owned())
                    .group(group)
                    .relative()
                    .when(message.state == TranscriptState::Pending, |bubble| {
                        bubble
                            .debug_selector(|| "pending-steering-message".to_owned())
                            .opacity(0.55)
                    })
                    .when(is_user, |bubble| {
                        // User messages are capped narrower than the agent's
                        // and pushed right by the parent's `justify_end`, so
                        // they read as indented from the left edge and are
                        // easy to spot while scrolling back through the
                        // transcript, while staying right-aligned with the
                        // agent's output below.
                        bubble
                            .max_w(relative(0.85))
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(ELEVATED))
                            .border_1()
                            .border_color(rgb(BORDER))
                    })
                    .when(!is_user, |message| message.w_full().py_2())
                    .min_w_0()
                    .when(is_user, |bubble| {
                        bubble.child(div().text_xs().text_color(rgb(BLUE)).child("You"))
                    })
                    .when(!message.content.is_empty(), |bubble| bubble.child(copy))
                    .when(!message.content.is_empty(), |bubble| {
                        bubble.child(
                            div()
                                .when(is_user, gpui::Styled::mt_2)
                                .pr_8()
                                .text_color(rgb(PRIMARY))
                                .child(markdown),
                        )
                    })
                    .when(!attachments.is_empty(), |bubble| {
                        bubble.child(
                            div()
                                .id(SharedString::from(format!(
                                    "message-attachments-{}",
                                    message.id
                                )))
                                .debug_selector(|| "message-attachments".to_owned())
                                .mt_2()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .children(attachments),
                        )
                    })
                    .when(message.state == TranscriptState::Interrupted, |bubble| {
                        bubble.child(div().mt_1().text_xs().text_color(rgb(AMBER)).child(
                            "Interrupted — the model does not have this \
                                             in its context",
                        ))
                    })
                    .when(message.state == TranscriptState::Streaming, |bubble| {
                        bubble.child(
                            div()
                                .mt_1()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child("Streaming..."),
                        )
                    }),
            )
    }

    #[allow(clippy::too_many_lines)]
    fn permission_entry(
        interaction_index: usize,
        record: &app_model::InteractionRecord,
        snapshot: &SessionSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let request = &record.request;
        let details = (!request.details.is_null()).then(|| {
            serde_json::to_string_pretty(&request.details)
                .unwrap_or_else(|_| request.details.to_string())
        });
        let status = record.response.as_ref().map(|response| match response {
            InteractionResponse::Approve
            | InteractionResponse::ApproveForSession
            | InteractionResponse::ApproveForLocation
            | InteractionResponse::ApprovePermanently => "Allowed",
            InteractionResponse::Reject { .. } => "Denied",
            InteractionResponse::Cancel => "Cancelled",
            InteractionResponse::Submit { .. } => "Answered",
        });
        let turn = snapshot
            .transcript
            .iter()
            .filter(|message| {
                message.role == TranscriptRole::User && message.sequence <= record.sequence
            })
            .count();
        let context = if turn == 0 {
            format!("Session: {}", snapshot.metadata.title)
        } else {
            format!("Session: {} · Turn {turn}", snapshot.metadata.title)
        };
        let choices = request.choices.iter().enumerate().map(|(index, choice)| {
            let choice = choice.clone();
            let response_choice = choice.clone();
            let session_id = snapshot.metadata.id.clone();
            let interaction_id = request.id.clone();
            let description = permission_scope_description(&choice);
            let selector = format!("permission-scope-{index}");
            div()
                .id(("permission-scope", index))
                .debug_selector({
                    let selector = selector.clone();
                    move || selector.clone()
                })
                .accessibility_id(selector)
                .role(Role::Button)
                .aria_label(format!("{choice}. {description}"))
                .focusable()
                .tab_stop(true)
                .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(BORDER))
                .child(div().flex_1().child(choice))
                .child(div().text_xs().text_color(rgb(MUTED)).child(description))
                .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                .on_click(cx.listener(move |view, _, _, _| {
                    let _ = view.commands.send(ServiceCommand::Respond {
                        app_session_id: session_id.clone(),
                        interaction_id: interaction_id.clone(),
                        response: choice_response(InteractionKind::Permission, &response_choice),
                    });
                }))
        });

        div()
            .id(SharedString::from(format!(
                "permission-{interaction_index}-{}",
                request.id
            )))
            .debug_selector(|| "permission-entry".to_owned())
            .accessibility_id(format!("permission-{interaction_index}-{}", request.id))
            .role(Role::ListItem)
            .aria_label(format!("Permission required: {}", request.message))
            .w_full()
            .p_4()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(AMBER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(AMBER))
                            .child(request.title.clone()),
                    )
                    .when_some(status, |heading, status| {
                        heading.child(div().text_xs().text_color(rgb(MUTED)).child(status))
                    }),
            )
            .child(div().mt_1().text_xs().text_color(rgb(MUTED)).child(context))
            .child(
                div()
                    .mt_3()
                    .text_color(rgb(PRIMARY))
                    .child(request.message.clone()),
            )
            .when_some(details, |card, details| {
                card.child(
                    div()
                        .mt_3()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(SUBTLE))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .text_sm()
                        .child(details),
                )
            })
            .when(record.response.is_none(), |card| {
                card.child(div().mt_3().flex().flex_col().gap_2().children(choices))
            })
    }

    fn render_timeline_row(&mut self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(item) = self.timeline.items.get(index).cloned() else {
            return div().into_any_element();
        };
        let Some(snapshot) = self.selected().map(|session| session.snapshot.clone()) else {
            return div().into_any_element();
        };
        self.transcript_rows_rendered += 1;
        let bottom_padding = match item.kind {
            TimelineItemKind::SessionStart(_) => 0.0,
            TimelineItemKind::Tool(_) => 4.0,
            TimelineItemKind::Message(_) | TimelineItemKind::Interaction(_) => 12.0,
        };
        let content = match item.kind {
            TimelineItemKind::SessionStart(item) => Some(
                self.session_start_row(item, &snapshot.metadata)
                    .into_any_element(),
            ),
            TimelineItemKind::Message(message_index) => snapshot
                .transcript
                .get(message_index)
                .map(|message| self.transcript_message(message, cx).into_any_element()),
            TimelineItemKind::Tool(invocation_index) => snapshot
                .tool_activity
                .invocations
                .get(invocation_index)
                .map(|invocation| {
                    let children = self
                        .timeline
                        .children
                        .get(&invocation.call_id)
                        .into_iter()
                        .flatten()
                        .filter_map(|index| snapshot.tool_activity.invocations.get(*index))
                        .collect::<Vec<_>>();
                    self.tool_entry(invocation, &children, cx)
                        .into_any_element()
                }),
            TimelineItemKind::Interaction(interaction_index) => snapshot
                .interaction_history
                .get(interaction_index)
                .map(|record| {
                    Self::permission_entry(interaction_index, record, &snapshot, cx)
                        .into_any_element()
                }),
        };
        div()
            .id(SharedString::from(format!("timeline-{}", item.id)))
            .w_full()
            .min_w_0()
            .px_5()
            .pb(px(bottom_padding))
            .child(
                div()
                    .debug_selector(|| "transcript-content".to_owned())
                    .mx_auto()
                    .w_full()
                    .max_w(px(CONVERSATION_COLUMN_WIDTH))
                    .min_w_0()
                    .children(content),
            )
            .into_any_element()
    }

    fn session_start_row(
        &self,
        item: SessionStartItem,
        metadata: &SessionMetadata,
    ) -> impl IntoElement {
        let (id, label, detail) = match item {
            SessionStartItem::CreatingWorktree => ("creating-worktree", "Creating worktree", None),
            SessionStartItem::WorktreeReady => (
                "worktree-ready",
                "Worktree ready",
                Some(self.display_worktree_path(Path::new(&metadata.project_path))),
            ),
            SessionStartItem::CopilotSessionStarted => {
                ("copilot-session-started", "Copilot session started", None)
            }
        };
        let aria_label = detail
            .as_ref()
            .map_or_else(|| label.to_owned(), |detail| format!("{label}: {detail}"));
        // Recorded as history: these are milestones from the session's actual
        // creation, not a decoration re-rendered ahead of the transcript, so
        // they carry the same timestamp as everything else that happened then.
        let timestamp = format_session_created_at(&metadata.created_at);
        div()
            .id(SharedString::from(format!("session-start-{id}")))
            .debug_selector(move || format!("session-start-{id}"))
            .role(Role::Status)
            .aria_label(format!("{aria_label} — {timestamp}"))
            .flex()
            .items_center()
            .gap_3()
            .py_1()
            .text_sm()
            .text_color(rgb(MUTED))
            .child(div().w(px(18.0)).text_color(rgb(GREEN)).child("✓"))
            .child(label)
            .when_some(detail, |row, detail| {
                row.child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(rgb(MUTED))
                        .child(detail),
                )
            })
            .child(div().flex_1())
            .child(div().text_xs().text_color(rgb(MUTED)).child(timestamp))
    }

    fn copy_transcript_selection(
        &mut self,
        _: &CopyTranscript,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selection = self.transcript_selection.borrow();
        let Some(text) = selection.selected_text() else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    fn resuming_session_placeholder() -> gpui::Stateful<gpui::Div> {
        div()
            .id("resuming-session")
            .debug_selector(|| "resuming-session".to_owned())
            .role(Role::Status)
            .aria_label("Resuming session")
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_color(rgb(MUTED))
                    .child(progress_spinner("resuming-session-spinner".into()))
                    .child("Resuming session…"),
            )
    }

    fn transcript(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = self.selected() else {
            return div()
                .id("empty-session")
                .role(Role::Group)
                .aria_label("New session")
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(640.0))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .items_center()
                        .child(
                            div()
                                .id("empty-session-heading")
                                .role(Role::Heading)
                                .aria_level(2)
                                .aria_label("What should Copilot work on?")
                                .text_2xl()
                                .child("What should Copilot work on?"),
                        )
                        .child(
                            div()
                                .text_color(rgb(MUTED))
                                .child("Start a coding session in the current checkout."),
                        ),
                );
        };
        if session.snapshot.status == SessionStatus::Recovering {
            return Self::resuming_session_placeholder();
        }
        self.transcript_rows_rendered = 0;
        let group = SharedString::from("scroll-transcript");
        let view = cx.entity();
        let list_state = self.transcript_list.clone();
        let running_indicator = self.running_indicator();
        let scrollbar = self.transcript_scrollbar(group.clone(), cx);
        let away_from_tail = self.transcript_is_away_from_tail();
        let transcript = list(list_state, move |index, _, cx| {
            view.update(cx, |view, cx| view.render_timeline_row(index, cx))
        })
        .flex_1()
        .min_h_0()
        .py_5();

        // The fade and the button ride with the rows rather than the whole
        // frame, so the running indicator below stays legible.
        let rows = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(transcript)
            .when(away_from_tail, |rows| {
                rows.child(Self::transcript_tail_fade())
                    .child(Self::scroll_to_bottom_button(cx))
            });

        div()
            .id("transcript-frame")
            .key_context("TranscriptSelection")
            .track_focus(&self.transcript_selection_focus)
            .on_action(cx.listener(Self::copy_transcript_selection))
            .group(group)
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            // A glide is a request for one destination; steering by hand
            // replaces it, so the two never fight over the scroll position.
            .on_scroll_wheel(cx.listener(|view, _: &gpui::ScrollWheelEvent, _, _| {
                view.cancel_scroll_to_bottom();
            }))
            .child(
                div()
                    .id("transcript")
                    .debug_selector(|| "transcript".to_owned())
                    .role(Role::List)
                    .aria_label("Conversation")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .child(rows)
                            .children(running_indicator),
                    ),
            )
            .children(scrollbar)
    }

    /// Dims the conversation tail while the transcript is parked above it.
    ///
    /// The unread bottom edge fading into the background is what makes "there
    /// is more below" legible at a glance, before the button is even noticed.
    fn transcript_tail_fade() -> impl IntoElement {
        div()
            .id("transcript-tail-fade")
            .debug_selector(|| "transcript-tail-fade".to_owned())
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(TRANSCRIPT_TAIL_FADE))
            // `BACKGROUND` carries no alpha channel, so shift it into place and
            // fade from fully transparent to the surface behind the transcript.
            .bg(gpui::linear_gradient(
                180.0,
                gpui::linear_color_stop(gpui::rgba(BACKGROUND << 8), 0.0),
                gpui::linear_color_stop(gpui::rgba((BACKGROUND << 8) | 0xff), 0.85),
            ))
    }

    /// The affordance that returns the transcript to the newest output.
    fn scroll_to_bottom_button(cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .absolute()
            .bottom(px(12.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("scroll-to-bottom")
                    .debug_selector(|| "scroll-to-bottom".to_owned())
                    .accessibility_id("scroll-to-bottom")
                    .role(Role::Button)
                    .aria_label("Scroll to newest output")
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_color(rgb(BLUE)))
                    .w(px(32.0))
                    .h(px(32.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(ELEVATED))
                    .text_color(rgb(MUTED))
                    .child("↓")
                    .hover(|style| {
                        style
                            .bg(rgb(BORDER))
                            .text_color(rgb(PRIMARY))
                            .cursor_pointer()
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.scroll_transcript_to_bottom(cx);
                    })),
            )
    }

    /// Running state at the conversation tail, where the user is already
    /// watching for the next activity.
    fn running_indicator(&self) -> Option<impl IntoElement + use<>> {
        let session = self.selected()?;
        let running_since = session.running_since?;
        if !session_is_running(session.snapshot.status) {
            return None;
        }
        let elapsed = running_since.elapsed().as_secs();
        Some(
            div()
                .id("running-indicator")
                .debug_selector(|| "running-indicator".to_owned())
                .accessibility_id("running-indicator")
                .role(Role::Status)
                .aria_label(format!("Agent running, {elapsed} seconds"))
                .mx_auto()
                .w_full()
                .max_w(px(CONVERSATION_COLUMN_WIDTH))
                .flex()
                .items_center()
                .gap_2()
                .px_6()
                .pb_3()
                .text_xs()
                .text_color(rgb(MUTED))
                .child(progress_spinner("running-indicator-spinner".into()))
                .child(format!("{elapsed}s")),
        )
    }

    /// Phase 3 inspector: changes, terminals, and capability state.
    ///
    /// Rendered beside the transcript so the edit-command-result-diff loop can
    /// be completed without leaving GCABB.
    fn side_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.selected()?;
        let snapshot = session.snapshot.clone();
        let active = self.active_panel;
        let tabs = SessionPanel::ALL.map(|panel| {
            let selected = panel == active;
            div()
                .id(panel.id())
                .accessibility_id(panel.id())
                .role(Role::Tab)
                .aria_label(panel.label())
                .aria_selected(selected)
                .focusable()
                .tab_stop(true)
                .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                .px_3()
                .py_1()
                .text_xs()
                .rounded_md()
                .text_color(if selected { rgb(PRIMARY) } else { rgb(MUTED) })
                .when(selected, |tab| tab.bg(rgb(ELEVATED)))
                .child(panel.label())
                .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.active_panel = panel;
                    if panel == SessionPanel::Changes {
                        view.refresh_selected_changes(false, cx);
                    } else {
                        view.base_menu_visibility = SettingsVisibility::Closed;
                    }
                    cx.notify();
                }))
        });

        let body = match active {
            SessionPanel::Changes => self.changes_panel(&snapshot, cx).into_any_element(),
            SessionPanel::Terminals => Self::terminals_panel(&snapshot).into_any_element(),
            SessionPanel::Capabilities => Self::capabilities_panel(&snapshot).into_any_element(),
        };

        Some(
            div()
                .id("session-panel")
                .accessibility_id("session-panel")
                .role(Role::Group)
                .aria_label("Session inspector")
                .flex()
                .flex_col()
                .w(px(420.0))
                .min_h_0()
                .border_l_1()
                .border_color(rgb(BORDER))
                .bg(rgb(SIDEBAR))
                .child(
                    div()
                        .id("session-panel-tabs")
                        .role(Role::TabList)
                        .aria_label("Inspector sections")
                        .flex()
                        .gap_1()
                        .p_2()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .children(tabs),
                )
                .child(
                    div()
                        .id("session-panel-body")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .p_3()
                        .gap_2()
                        .child(body),
                ),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn changes_base_controls(
        &self,
        snapshot: &SessionSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = snapshot.metadata.base_ref.as_deref().unwrap_or("HEAD");
        let resolved = snapshot.changes.tracking_ref.as_deref().unwrap_or(selected);
        let commit = snapshot
            .changes
            .base
            .as_deref()
            .map_or("unresolved", |commit| &commit[..commit.len().min(8)]);
        let default = self.base_default_ref.clone();
        let options = self
            .base_ref_options
            .iter()
            .enumerate()
            .map(|(index, reference)| {
                let value = reference.clone();
                let is_selected = reference == selected;
                div()
                    .id(("changes-base-option", index))
                    .debug_selector({
                        let selector = format!("changes-base-option-{index}");
                        move || selector.clone()
                    })
                    .accessibility_id(format!("changes-base-option-{index}"))
                    .role(Role::Button)
                    .aria_label(reference.clone())
                    .aria_selected(is_selected)
                    .focusable()
                    .tab_stop(true)
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .when(is_selected, |row| row.bg(rgb(ELEVATED)))
                    .child(reference.clone())
                    .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.set_changes_base(value.clone(), cx);
                    }))
            });

        div()
            .id("changes-base-controls")
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .id("changes-base")
                    .debug_selector(|| "changes-base".to_owned())
                    .accessibility_id("changes-base")
                    .role(Role::ComboBox)
                    .aria_label("Change comparison base")
                    .aria_value(selected)
                    .aria_expanded(self.base_menu_visibility == SettingsVisibility::Open)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child("Base")
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(|view, _, _, cx| view.toggle_base_menu(cx))),
            )
            .child(
                div()
                    .id("changes-refresh")
                    .debug_selector(|| "changes-refresh".to_owned())
                    .accessibility_id("changes-refresh")
                    .role(Role::Button)
                    .aria_label("Refresh changes and base branch")
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .child("\u{21bb}")
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.refresh_selected_changes(true, cx);
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!("{selected} \u{2192} {resolved} @ {commit}")),
            )
            .when(
                self.base_menu_visibility == SettingsVisibility::Open,
                |controls| {
                    controls.child(
                        deferred(
                            div()
                                .id("changes-base-menu")
                                .accessibility_id("changes-base-menu")
                                .role(Role::ListBox)
                                .aria_label("Comparison base branches")
                                .occlude()
                                .absolute()
                                .top(px(34.0))
                                .left_0()
                                .w(px(320.0))
                                .max_h(px(360.0))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .p_2()
                                .rounded_lg()
                                .border_1()
                                .border_color(rgb(BORDER))
                                .bg(rgb(PANEL))
                                .shadow_lg()
                                .when_some(default, |menu, default| {
                                    let value = default.clone();
                                    menu.child(
                                        div()
                                            .id("changes-base-reset")
                                            .debug_selector(|| "changes-base-reset".to_owned())
                                            .accessibility_id("changes-base-reset")
                                            .role(Role::Button)
                                            .aria_label(format!("Reset to default base {default}"))
                                            .focusable()
                                            .tab_stop(true)
                                            .w_full()
                                            .min_w_0()
                                            .px_3()
                                            .py_2()
                                            .rounded_md()
                                            .truncate()
                                            .text_color(rgb(BLUE))
                                            .child(format!("Reset to default ({default})"))
                                            .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                                            .on_click(cx.listener(move |view, _, _, cx| {
                                                view.set_changes_base(value.clone(), cx);
                                            })),
                                    )
                                })
                                .children(options),
                        )
                        .with_priority(1),
                    )
                },
            )
    }

    #[allow(clippy::too_many_lines)]
    fn changes_panel(
        &self,
        snapshot: &SessionSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let changes = &snapshot.changes;
        let controls = self.changes_base_controls(snapshot, cx).into_any_element();
        if let Some(error) = &changes.error {
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .child(controls)
                .child(
                    div()
                        .id("changes-error")
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .text_sm()
                        .text_color(rgb(RED))
                        .child(error.clone()),
                )
                .into_any_element();
        }
        if changes.is_empty() {
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .child(controls)
                .child(
                    div()
                        .id("changes-empty")
                        .role(Role::Status)
                        .aria_label("No changes")
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child(format!(
                            "No changes against {}.",
                            changes.base_label.as_deref().unwrap_or("base")
                        )),
                )
                .into_any_element();
        }

        let totals = changes.totals();
        let session_id = snapshot.metadata.id.clone();
        let handle = self
            .detail_scrolls
            .borrow_mut()
            .entry(CHANGES_SCROLL_ID.to_owned())
            .or_default()
            .clone();
        let group = SharedString::from("scroll-changes");
        let claim_scroll = self.claim_scroll_when_moved(CHANGES_SCROLL_ID, &handle, cx);
        let mut entries = Vec::with_capacity(changes.files.len());
        for file in &changes.files {
            entries.push(self.change_entry(&session_id, file, cx).into_any_element());
        }
        let scrollbar = Self::scrollbar(CHANGES_SCROLL_ID, &handle, group.clone(), cx);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(controls)
            .child(
                div()
                    .id("changes-summary")
                    .role(Role::Status)
                    .aria_label(format!(
                        "{} files changed, {} insertions, {} deletions against {}",
                        changes.files.len(),
                        totals.insertions,
                        totals.deletions,
                        changes.base_label.as_deref().unwrap_or("base")
                    ))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!(
                        "{} file(s) \u{b7} +{} \u{2212}{} \u{b7} vs {}",
                        changes.files.len(),
                        totals.insertions,
                        totals.deletions,
                        changes.base_label.as_deref().unwrap_or("base")
                    )),
            )
            .child(
                // One scrolling surface for rows and their expanded diffs, so
                // the panel never stacks a diff viewport inside a file list.
                div()
                    .id("changes-scroll-frame")
                    .group(group)
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("changes-list")
                            .debug_selector(|| "changes-list".to_owned())
                            .role(Role::List)
                            .aria_label("Changed files")
                            .track_scroll(&handle)
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .gap_1()
                            .pr_2()
                            .overflow_y_scroll()
                            .on_scroll_wheel(claim_scroll)
                            .children(entries),
                    )
                    .children(scrollbar),
            )
            .into_any_element()
    }

    /// Whether a file's diff is currently expanded in the Changes panel.
    fn change_expanded(&self, session_id: &str, path: &str) -> bool {
        self.expanded_changes
            .get(session_id)
            .is_some_and(|paths| paths.contains(path))
    }

    /// Expand a collapsed file's diff, or collapse an expanded one.
    fn toggle_change(&mut self, session_id: &str, path: &str) {
        let expanded = self
            .expanded_changes
            .entry(session_id.to_owned())
            .or_default();
        if !expanded.remove(path) {
            expanded.insert(path.to_owned());
        }
    }

    /// How a changed file is named in the panel, with both names for renames.
    fn change_display_path(file: &ChangedFile) -> String {
        match (file.status, file.original_path.as_deref()) {
            (ChangeStatus::Renamed, Some(original)) => {
                format!("{original} \u{2192} {}", file.path)
            }
            _ => file.path.clone(),
        }
    }

    /// The accessible name of a changed-file row.
    fn change_row_label(file: &ChangedFile) -> String {
        format!(
            "{} {} +{} -{}",
            file.status.label(),
            Self::change_display_path(file),
            file.stats.insertions,
            file.stats.deletions
        )
    }

    /// What an expanded file shows, and whether it is a placeholder rather
    /// than a diff. Binary, omitted, and failed diffs report beneath their own
    /// row instead of replacing the panel.
    fn change_diff_text(file: &ChangedFile) -> (String, bool) {
        if file.binary {
            return ("Binary file. No diff to show.".to_owned(), true);
        }
        if let Some(diff) = file.diff.clone().filter(|diff| !diff.trim().is_empty()) {
            return (diff, false);
        }
        if let Some(reason) = file.diff_omitted_reason.clone() {
            return (reason, true);
        }
        ("Diff unavailable.".to_owned(), true)
    }

    /// The focus handle for a changed-file row, created on first render.
    fn change_row_focus(
        &self,
        session_id: &str,
        path: &str,
        cx: &mut Context<Self>,
    ) -> FocusHandle {
        self.change_focus
            .borrow_mut()
            .entry(format!("{session_id}\u{1f}{path}"))
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    const fn change_status_color(status: ChangeStatus) -> u32 {
        match status {
            ChangeStatus::Added | ChangeStatus::Untracked => GREEN,
            ChangeStatus::Deleted => RED,
            ChangeStatus::Renamed => BLUE,
            ChangeStatus::Modified => MUTED,
        }
    }

    /// A changed file row plus, when expanded, its complete diff in flow.
    fn change_entry(
        &self,
        session_id: &str,
        file: &ChangedFile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = file.path.clone();
        let expanded = self.change_expanded(session_id, &path);
        let focus = self.change_row_focus(session_id, &path, cx);
        let display_path = Self::change_display_path(file);
        let label = Self::change_row_label(file);
        let row_session = session_id.to_owned();
        let row_path = path.clone();
        let toggle_session = session_id.to_owned();
        let toggle_path = path.clone();
        let disclosure_label = if expanded {
            format!("Collapse diff for {path}")
        } else {
            format!("Expand diff for {path}")
        };
        let selector_path = path.clone();
        let status_color = rgb(Self::change_status_color(file.status));

        div()
            .id(SharedString::from(format!("change-entry-{path}")))
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .child(
                div()
                    .id(SharedString::from(format!("change-{path}")))
                    .debug_selector(move || format!("change-row-{selector_path}"))
                    .role(Role::ListItem)
                    .aria_label(label)
                    .aria_expanded(expanded)
                    .track_focus(&focus)
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(expanded, |row| row.bg(rgb(ELEVATED)))
                    .child(
                        div()
                            .id(SharedString::from(format!("change-toggle-{path}")))
                            .debug_selector({
                                let path = path.clone();
                                move || format!("change-toggle-{path}")
                            })
                            .role(Role::Button)
                            .aria_label(disclosure_label)
                            .aria_expanded(expanded)
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(if expanded { "\u{25be}" } else { "\u{25b8}" })
                            .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.toggle_change(&toggle_session, &toggle_path);
                                // Without this the row behind the control
                                // toggles a second time and nothing moves.
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(status_color)
                            .child(file.status.label()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(rgb(PRIMARY))
                            .child(display_path),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(GREEN))
                            .child(format!("+{}", file.stats.insertions)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(RED))
                            .child(format!("-{}", file.stats.deletions)),
                    )
                    .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.toggle_change(&row_session, &row_path);
                        cx.notify();
                    })),
            )
            .when(expanded, |entry| {
                entry.child(self.change_diff(session_id, file))
            })
    }

    fn change_diff_document(&self, session_id: &str, file: &ChangedFile) -> Arc<DiffDocument> {
        let key = format!("{session_id}\u{1f}{}", file.path);
        let (body, muted) = Self::change_diff_text(file);
        let mut cache = self.diff_cache.borrow_mut();

        if let Some(document) = cache.documents.get(&key)
            && document.source.as_ref() == body
            && document.muted == muted
        {
            return Arc::clone(document);
        }

        let highlights = if muted {
            Vec::new()
        } else {
            syntax::diff_highlights(Path::new(&file.path), &body).unwrap_or_else(|error| {
                tracing::warn!(
                    path = %file.path,
                    %error,
                    "failed to syntax-highlight diff"
                );
                Vec::new()
            })
        };
        let lines = diff_lines(&body, &highlights);
        let document = Arc::new(DiffDocument {
            source: body.into(),
            lines,
            muted,
        });

        if !cache.documents.contains_key(&key) {
            if cache.documents.len() == DIFF_CACHE_CAPACITY
                && let Some(evicted) = cache.order.pop_front()
            {
                cache.documents.remove(&evicted);
            }
            cache.order.push_back(key.clone());
        }
        cache.documents.insert(key, Arc::clone(&document));
        document
    }

    /// A file's complete diff, laid out in the panel's own scroll flow.
    ///
    /// Only the horizontal axis scrolls here: a vertical scroller would trap
    /// the wheel and give the panel a second competing scrollbar.
    fn change_diff(&self, session_id: &str, file: &ChangedFile) -> impl IntoElement {
        let path = file.path.clone();
        let document = self.change_diff_document(session_id, file);
        let body = div()
            .flex()
            .flex_col()
            .min_w_full()
            .children(document.lines.iter().map(|line| {
                let text =
                    StyledText::new(line.source.clone()).with_highlights(line.highlights.clone());
                div()
                    .min_w_full()
                    .when_some(line.background, |row, color| row.bg(gpui::rgba(color)))
                    .child(text)
            }));

        div()
            .id(SharedString::from(format!("change-diff-{path}")))
            .debug_selector({
                let path = path.clone();
                move || format!("change-diff-{path}")
            })
            .role(Role::Group)
            .aria_label(format!("Diff for {path}"))
            .mt_1()
            .mb_1()
            .ml_4()
            .min_w_0()
            .p_2()
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_xs()
            .font_family(".ZedMono")
            .whitespace_nowrap()
            .overflow_x_scroll()
            // Without this a vertical wheel over a diff is remapped onto the
            // horizontal axis instead of scrolling the panel.
            .restrict_scroll_to_axis()
            .on_scroll_wheel(Self::claim_horizontal_scroll)
            .text_color(if document.muted {
                rgb(MUTED)
            } else {
                rgb(PRIMARY)
            })
            .child(body)
    }

    fn terminals_panel(snapshot: &SessionSnapshot) -> impl IntoElement {
        let terminals = &snapshot.tool_activity.terminals;
        if terminals.is_empty() {
            return div()
                .id("terminals-empty")
                .role(Role::Status)
                .aria_label("No terminals")
                .text_sm()
                .text_color(rgb(MUTED))
                .child("No shell commands have run in this session.")
                .into_any_element();
        }
        let cards = terminals.iter().rev().take(12).map(|terminal| {
            let (state_label, state_color) = terminal_state_display(terminal.state);
            let command = terminal_title(terminal);
            let aria_label = format!("{command}, {state_label}");
            let exit = terminal
                .exit_code
                .map_or_else(String::new, |code| format!(" · exit {code}"));
            div()
                .id(SharedString::from(format!(
                    "terminal-{}",
                    terminal.shell_id
                )))
                .role(Role::Group)
                .aria_label(aria_label)
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(rgb(PRIMARY))
                                .child(command),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(state_color))
                                .child(format!("{state_label}{exit}")),
                        ),
                )
                .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                    "{} call(s) · {} bytes in {} chunk(s)",
                    terminal.tool_call_ids.len(),
                    terminal.output_metadata.byte_count,
                    terminal.output_metadata.chunk_count
                )))
                .child(
                    div()
                        .max_h(px(160.0))
                        .overflow_hidden()
                        .text_xs()
                        .text_color(rgb(PRIMARY))
                        .child(terminal_tail(&terminal.output)),
                )
                .when_some(terminal_output_error(terminal), |card, error| {
                    card.child(
                        div()
                            .id(SharedString::from(format!(
                                "terminal-output-error-{}",
                                terminal.shell_id
                            )))
                            .role(Role::Alert)
                            .text_xs()
                            .text_color(rgb(RED))
                            .child(format!("Output unavailable: {error}")),
                    )
                })
        });
        div()
            .id("terminals-list")
            .role(Role::List)
            .aria_label("Terminals")
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .children(cards)
            .into_any_element()
    }

    #[allow(clippy::too_many_lines)]
    fn capabilities_panel(snapshot: &SessionSnapshot) -> impl IntoElement {
        let report = &snapshot.capabilities;
        let failures = snapshot.tool_activity.failures();
        let rows = report.capabilities.iter().map(|capability| {
            let (label, color) = match capability.status {
                app_model::CapabilityStatus::Available => ("available", GREEN),
                app_model::CapabilityStatus::Unavailable => ("unavailable", RED),
                app_model::CapabilityStatus::NeedsAttention => ("needs attention", AMBER),
                app_model::CapabilityStatus::Unknown => ("unknown", MUTED),
            };
            div()
                .id(SharedString::from(format!(
                    "capability-{}",
                    capability.id.label().to_lowercase().replace(' ', "-")
                )))
                .role(Role::ListItem)
                .aria_label(format!(
                    "{}: {label}. {}",
                    capability.id.label(),
                    capability.detail
                ))
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(PRIMARY))
                                .child(capability.id.label()),
                        )
                        .child(div().text_xs().text_color(rgb(color)).child(label)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(capability.detail.clone()),
                )
        });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .overflow_hidden()
            .child(
                div()
                    .id("capabilities-list")
                    .role(Role::List)
                    .aria_label("Capabilities")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(rows),
            )
            .when(!failures.is_empty(), |panel| {
                let items = failures.into_iter().rev().take(6).map(|invocation| {
                    let message = invocation
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Tool failed without a message.".to_owned());
                    div()
                        .id(SharedString::from(format!(
                            "tool-failure-{}",
                            invocation.call_id
                        )))
                        .role(Role::ListItem)
                        .aria_label(format!("{} failed: {message}", invocation.tool_name))
                        .flex()
                        .flex_col()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(RED))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(RED))
                                .child(invocation.tool_name.clone()),
                        )
                        .child(div().text_xs().text_color(rgb(MUTED)).child(message))
                });
                panel.child(
                    div()
                        .id("tool-failures")
                        .role(Role::List)
                        .aria_label("Recent tool failures")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(items),
                )
            })
            .into_any_element()
    }

    fn running_activity(&self) -> Option<impl IntoElement> {
        let session = self.selected()?;
        if !matches!(
            session.snapshot.status,
            SessionStatus::Running | SessionStatus::Starting
        ) {
            return None;
        }
        let elapsed = self
            .selected()
            .and_then(|session| {
                session
                    .snapshot
                    .diagnostics
                    .turn_started_at
                    .as_deref()
                    .and_then(elapsed_since_timestamp)
            })
            .or_else(|| self.running_since.get(session.id()).map(Instant::elapsed))
            .unwrap_or_default();
        let diagnostics = &session.snapshot.diagnostics;
        let label = diagnostics
            .latest_intent
            .as_ref()
            .or(diagnostics.activity.as_ref())
            .map_or("Agent is working", String::as_str);

        Some(
            div()
                .id("running-activity")
                .debug_selector(|| "running-activity".to_owned())
                .role(Role::Status)
                .aria_label(label)
                .mx_auto()
                .w_full()
                .max_w(px(CONVERSATION_COLUMN_WIDTH))
                .px_3()
                .pb_2()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(rgb(MUTED))
                .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(GREEN)))
                .child(
                    div()
                        .id("running-elapsed")
                        .debug_selector(|| "running-elapsed".to_owned())
                        .text_color(rgb(GREEN))
                        .child(format_elapsed(elapsed)),
                )
                .child(
                    div()
                        .id("running-intent")
                        .debug_selector(|| "running-intent".to_owned())
                        .child(label.to_owned()),
                ),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn diagnostics_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.diagnostics_visibility != SettingsVisibility::Open {
            return None;
        }
        let session = self.selected()?;
        let snapshot = &session.snapshot;
        let diagnostics = &snapshot.diagnostics;
        let elapsed = self
            .selected()
            .and_then(|session| {
                session
                    .snapshot
                    .diagnostics
                    .turn_started_at
                    .as_deref()
                    .and_then(elapsed_since_timestamp)
            })
            .or_else(|| self.running_since.get(session.id()).map(Instant::elapsed))
            .unwrap_or_default();
        let silence = diagnostics
            .last_event_at
            .as_deref()
            .and_then(elapsed_since_timestamp)
            .or_else(|| {
                self.last_event_seen
                    .get(session.id())
                    .map(|(_, seen_at)| seen_at.elapsed())
            })
            .unwrap_or_default();
        let active_tools: Vec<_> = snapshot
            .tool_activity
            .invocations
            .iter()
            .filter(|invocation| invocation.state == app_model::InvocationState::Running)
            .map(|invocation| {
                diagnostic_field(
                    invocation.tool_name.clone(),
                    format!("{} · running", invocation.summary_line()),
                )
            })
            .collect();
        let event_counts: Vec<_> = diagnostics
            .event_counts
            .iter()
            .map(|(event_type, count)| diagnostic_field(event_type.clone(), count.to_string()))
            .collect();
        let recent_events: Vec<_> = diagnostics
            .recent_events
            .iter()
            .rev()
            .map(|event| {
                diagnostic_field(
                    format!("#{} {}", event.sequence, event.event_type),
                    event.summary.clone(),
                )
            })
            .collect();
        let usage = diagnostics.last_usage.as_ref().map(|usage| {
            format!(
                "{} · {} ms · {} input / {} output tokens · {} cached",
                usage.model.as_deref().unwrap_or("unknown model"),
                usage.duration_ms.unwrap_or_default(),
                usage.input_tokens.unwrap_or_default(),
                usage.output_tokens.unwrap_or_default(),
                usage.cache_read_tokens.unwrap_or_default(),
            )
        });
        let compaction = diagnostics.compaction.as_ref().map(|compaction| {
            format!(
                "{} / {} tokens · trigger {}",
                compaction.current_tokens.unwrap_or_default(),
                compaction.token_limit.unwrap_or_default(),
                compaction.trigger.as_deref().unwrap_or("unknown"),
            )
        });

        let (provider_status, provider_color) = self.provider_status();

        Some(
            div()
                .id("diagnostics-dialog")
                .debug_selector(|| "diagnostics-dialog".to_owned())
                .accessibility_id("diagnostics-dialog")
                .role(Role::Dialog)
                .aria_label("Agent diagnostics")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x0000_00a8))
                .child(
                    div()
                        .id("diagnostics-panel")
                        .w(px(720.0))
                        .max_h(px(720.0))
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .p_5()
                                .border_b_1()
                                .border_color(rgb(BORDER))
                                .child(
                                    div()
                                        .id("diagnostics-heading")
                                        .role(Role::Heading)
                                        .aria_level(2)
                                        .text_xl()
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .child("Agent diagnostics"),
                                )
                                .child(
                                    div()
                                        .id("diagnostics-close")
                                        .debug_selector(|| "diagnostics-close".to_owned())
                                        .accessibility_id("diagnostics-close")
                                        .role(Role::Button)
                                        .aria_label("Close agent diagnostics")
                                        .focusable()
                                        .tab_stop(true)
                                        .px_3()
                                        .py_1()
                                        .rounded_md()
                                        .bg(rgb(ELEVATED))
                                        .child("Close")
                                        .hover(|style| style.opacity(0.85).cursor_pointer())
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.diagnostics_visibility =
                                                SettingsVisibility::Closed;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("diagnostics-content")
                                .p_5()
                                .flex()
                                .flex_1()
                                .flex_col()
                                .min_h_0()
                                .gap_4()
                                .overflow_y_scroll()
                                .child(diagnostic_section(
                                    "Connection",
                                    vec![
                                        div()
                                            .id("provider-status")
                                            .role(Role::Status)
                                            .aria_label(provider_status.clone())
                                            .text_color(rgb(provider_color))
                                            .child(provider_status)
                                            .into_any_element(),
                                    ],
                                ))
                                .child(diagnostic_section(
                                    "Current activity",
                                    vec![
                                        diagnostic_field(
                                            "Status",
                                            format!("{:?}", snapshot.status),
                                        ),
                                        diagnostic_field("Elapsed", format_elapsed(elapsed)),
                                        diagnostic_field(
                                            "Current activity",
                                            diagnostics.activity.clone().unwrap_or_else(|| {
                                                "No active SDK phase".to_owned()
                                            }),
                                        ),
                                        diagnostic_field(
                                            "Assistant intent",
                                            diagnostics
                                                .latest_intent
                                                .clone()
                                                .unwrap_or_else(|| "Not reported".to_owned()),
                                        ),
                                        diagnostic_field(
                                            "Model / turn",
                                            format!(
                                                "{} / {}",
                                                diagnostics.model.as_deref().unwrap_or("unknown"),
                                                diagnostics.turn_id.as_deref().unwrap_or("unknown")
                                            ),
                                        ),
                                        diagnostic_field(
                                            "Last SDK event",
                                            format!(
                                                "{} · {} ago",
                                                diagnostics
                                                    .last_event_type
                                                    .as_deref()
                                                    .unwrap_or("none"),
                                                format_elapsed(silence)
                                            ),
                                        ),
                                        diagnostic_field(
                                            "Response stream",
                                            diagnostics.response_bytes.map_or_else(
                                                || "No byte count reported".to_owned(),
                                                |bytes| format!("{bytes} bytes received"),
                                            ),
                                        ),
                                    ],
                                ))
                                .when_some(compaction, |content, value| {
                                    content.child(diagnostic_section(
                                        "Context compaction",
                                        vec![diagnostic_field("In progress", value)],
                                    ))
                                })
                                .when_some(usage, |content, value| {
                                    content.child(diagnostic_section(
                                        "Latest model call",
                                        vec![diagnostic_field("Usage", value)],
                                    ))
                                })
                                .when(!active_tools.is_empty(), |content| {
                                    content.child(diagnostic_section("Active tools", active_tools))
                                })
                                .child(diagnostic_section("SDK event counts", event_counts))
                                .child(diagnostic_section(
                                    "Recent progress signals",
                                    recent_events,
                                )),
                        ),
                ),
        )
    }

    fn unavailable_session_notice(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let session = self.selected().expect("selected session");
        let app_session_id = session.id().to_owned();
        let locate_id = app_session_id.clone();
        let delete_id = app_session_id.clone();
        let retry_id = app_session_id.clone();
        let working_directory = PathBuf::from(&session.snapshot.metadata.project_path);
        let worktrees_root = self
            .worktree_configuration
            .settings
            .owning_root_for_worktree(
                &working_directory,
                &self.worktree_configuration.default_root,
            );
        let can_recreate = worktrees_root.is_some() && session.snapshot.changes.branch.is_some();

        div()
            .id("session-unavailable")
            .debug_selector(|| "session-unavailable".to_owned())
            .accessibility_id("session-unavailable")
            .role(Role::Status)
            .aria_label("Session working directory unavailable")
            .mx_auto()
            .mb_4()
            .w_full()
            .max_w(px(CONVERSATION_COLUMN_WIDTH))
            .flex()
            .items_center()
            .gap_3()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child("Worktree unavailable")
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Stored history is available read-only."),
                    ),
            )
            .when(can_recreate, |notice| {
                notice.child(action_button("Recreate", GREEN, cx, move |view, _| {
                    view.action_error = None;
                    let _ = view.commands.send(ServiceCommand::Resume {
                        app_session_id: retry_id.clone(),
                        worktrees_root: worktrees_root.clone(),
                    });
                }))
            })
            .child(action_button("Locate folder", BLUE, cx, move |view, cx| {
                view.locate_session(locate_id.clone(), cx);
            }))
            .child(action_button("Delete session", RED, cx, move |view, cx| {
                view.delete_session(delete_id.clone(), cx);
            }))
            .into_any_element()
    }

    #[allow(clippy::too_many_lines)]
    fn session_composer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self
            .selected()
            .is_some_and(|session| session.snapshot.status == SessionStatus::Unavailable)
        {
            return self.unavailable_session_notice(cx);
        }
        let mode = title_case(&self.draft_mode);
        let effort = effort_label(&self.draft_effort);
        let model = self.draft_model_label();
        let supports_reasoning = !self.effort_options().is_empty();
        let context_control = self.context_control(cx);
        let selected = self.selected();
        let running = selected.is_some_and(|session| {
            matches!(
                session.snapshot.status,
                SessionStatus::Running | SessionStatus::Starting
            )
        });
        let has_draft =
            !self.composer.read(cx).value().trim().is_empty() || !self.draft_attachments.is_empty();
        let stops_running_session = running && !has_draft;
        let action_id = if stops_running_session {
            "stop-session"
        } else {
            "submit-prompt"
        };
        let action_label = if stops_running_session {
            "Stop agent"
        } else if running {
            "Send steering message"
        } else {
            "Send message"
        };
        let disconnected =
            selected.is_some_and(|session| session.snapshot.status == SessionStatus::Disconnected);
        let resume = disconnected
            .then(|| self.selected_session.clone())
            .flatten();
        div()
            .id("composer")
            .debug_selector(|| "composer".to_owned())
            .accessibility_id("composer")
            .relative()
            .role(Role::Group)
            .aria_label("Message composer")
            .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                view.attach_dropped_paths(paths.paths(), cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(rgb(BLUE)))
            .mx_auto()
            .mb_4()
            .w_full()
            .max_w(px(CONVERSATION_COLUMN_WIDTH))
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_lg()
            .shadow_lg()
            .child(self.composer.clone())
            .children(self.attachment_strip(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .pb_3()
                    .child(
                        div()
                            .id("attachments-placeholder")
                            .accessibility_id("attachments-placeholder")
                            .role(Role::Button)
                            .aria_label("Attach files")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .text_lg()
                            .text_color(rgb(MUTED))
                            .child("+")
                            .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
                            .on_click(cx.listener(|_, _, _, cx| Self::pick_attachments(cx))),
                    )
                    .child(control_pill(
                        "mode",
                        mode,
                        ControlMenu::Mode,
                        self.open_control_menu == Some(ControlMenu::Mode),
                        cx,
                    ))
                    .child(control_pill(
                        "model",
                        model,
                        ControlMenu::Model,
                        self.open_control_menu == Some(ControlMenu::Model),
                        cx,
                    ))
                    .when(supports_reasoning, |row| {
                        row.child(control_pill(
                            "effort",
                            effort,
                            ControlMenu::Effort,
                            self.open_control_menu == Some(ControlMenu::Effort),
                            cx,
                        ))
                    })
                    .children(context_control)
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("open-diagnostics")
                            .debug_selector(|| "open-diagnostics".to_owned())
                            .accessibility_id("open-diagnostics")
                            .role(Role::Button)
                            .aria_label("Open agent diagnostics")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .w(px(32.0))
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(ELEVATED))
                            .text_color(rgb(MUTED))
                            .child("?")
                            .hover(|style| {
                                style
                                    .bg(rgb(BORDER))
                                    .text_color(rgb(PRIMARY))
                                    .cursor_pointer()
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.diagnostics_visibility = SettingsVisibility::Open;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id(action_id)
                            .debug_selector(move || action_id.to_owned())
                            .accessibility_id(action_id)
                            .role(Role::Button)
                            .aria_label(action_label)
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .w(px(32.0))
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(ELEVATED))
                            .text_color(if stops_running_session {
                                rgb(RED)
                            } else {
                                rgb(MUTED)
                            })
                            .child(if stops_running_session { "■" } else { "↑" })
                            .hover(|style| {
                                style
                                    .bg(rgb(BORDER))
                                    .text_color(rgb(PRIMARY))
                                    .cursor_pointer()
                            })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if stops_running_session {
                                    if let Some(app_session_id) = view.selected_session.clone() {
                                        let _ = view
                                            .commands
                                            .send(ServiceCommand::Cancel { app_session_id });
                                    }
                                } else {
                                    view.submit_composer(cx);
                                }
                            })),
                    )
                    .when(disconnected, |row| {
                        row.when_some(resume, |row, id| {
                            row.child(
                                div()
                                    .id("resume-session")
                                    .accessibility_id("resume-session")
                                    .role(Role::Button)
                                    .aria_label("Resume session")
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(GREEN))
                                    .text_color(rgb(BACKGROUND))
                                    .child("Resume")
                                    .hover(|style| style.opacity(0.85).cursor_pointer())
                                    .on_click(cx.listener(move |view, _, _, _| {
                                        let _ = view.commands.send(ServiceCommand::Resume {
                                            app_session_id: id.clone(),
                                            worktrees_root: None,
                                        });
                                    })),
                            )
                        })
                    }),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_lines)]
    fn home_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project_name = self.composer_project_label();
        let chat = self.targets_chat();
        let location_label = self.draft_location.label().to_owned();
        let branch = self.composer_branch_label();
        let mode = title_case(&self.draft_mode);
        let model = self.draft_model_label();
        let effort = effort_label(&self.draft_effort);
        let supports_reasoning = !self.effort_options().is_empty();
        let context_control = self.context_control(cx);

        div()
            .id("home-composer")
            .accessibility_id("home-composer")
            .role(Role::Group)
            .aria_label("Message composer")
            .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                view.attach_dropped_paths(paths.paths(), cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(rgb(BLUE)))
            .relative()
            .w_full()
            .max_w(px(CONVERSATION_COLUMN_WIDTH))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(rgb(SUBTLE))
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_h(px(108.0))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_lg()
                    .child(self.composer.clone())
                    .children(self.attachment_strip(cx))
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .pb_3()
                            .child(
                                div()
                                    .id("home-attachments-placeholder")
                                    .accessibility_id("home-attachments-placeholder")
                                    .role(Role::Button)
                                    .aria_label("Attach files")
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .w(px(28.0))
                                    .h(px(28.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_lg()
                                    .text_color(rgb(MUTED))
                                    .child("+")
                                    .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
                                    .on_click(
                                        cx.listener(|_, _, _, cx| Self::pick_attachments(cx)),
                                    ),
                            )
                            .child(control_pill(
                                "mode",
                                mode,
                                ControlMenu::Mode,
                                self.open_control_menu == Some(ControlMenu::Mode),
                                cx,
                            ))
                            .child(div().h(px(20.0)).border_l_1().border_color(rgb(BORDER)))
                            .child(control_pill(
                                "model",
                                model,
                                ControlMenu::Model,
                                self.open_control_menu == Some(ControlMenu::Model),
                                cx,
                            ))
                            .when(supports_reasoning, |row| {
                                row.child(control_pill(
                                    "effort",
                                    effort,
                                    ControlMenu::Effort,
                                    self.open_control_menu == Some(ControlMenu::Effort),
                                    cx,
                                ))
                            })
                            .children(context_control)
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("home-submit-prompt")
                                    .accessibility_id("home-submit-prompt")
                                    .role(Role::Button)
                                    .aria_label("Send message")
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .w(px(32.0))
                                    .h(px(32.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(rgb(ELEVATED))
                                    .text_color(rgb(MUTED))
                                    .child("↑")
                                    .hover(|style| {
                                        style
                                            .bg(rgb(BORDER))
                                            .text_color(rgb(PRIMARY))
                                            .cursor_pointer()
                                    })
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.submit_composer(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("checkout-context")
                    .flex()
                    .items_center()
                    .gap_4()
                    .h(px(48.0))
                    .px_4()
                    .min_w_0()
                    .overflow_hidden()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(
                        div()
                            .id("project-pill")
                            .accessibility_id("project-pill")
                            .role(Role::ComboBox)
                            .aria_label("Project")
                            .aria_value(project_name.clone())
                            .aria_expanded(self.open_control_menu == Some(ControlMenu::Project))
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .child(format!("▱ {project_name}"))
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.toggle_control_menu(ControlMenu::Project);
                                cx.notify();
                            })),
                    )
                    // A chat has no checkout, so the checkout details are
                    // replaced rather than shown as if they applied.
                    .when(chat, |strip| strip.child("↗ No repository"))
                    .when(!chat, |strip| {
                        strip
                            .child(
                                div()
                                    .id("location-pill")
                                    .debug_selector(|| "location-pill".to_owned())
                                    .accessibility_id("location-pill")
                                    .role(Role::ComboBox)
                                    .aria_label("Where to run this session")
                                    .aria_value(location_label.clone())
                                    .aria_expanded(
                                        self.open_control_menu == Some(ControlMenu::Location),
                                    )
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .child(format!("↗ {location_label}"))
                                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.toggle_control_menu(ControlMenu::Location);
                                        cx.notify();
                                    })),
                            )
                            .child(format!("⌁ {branch}"))
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("add-project")
                            .accessibility_id("add-project")
                            .role(Role::Button)
                            .aria_label("Add project folder")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .child("+ Add project")
                            .hover(|style| {
                                style
                                    .bg(rgb(ELEVATED))
                                    .text_color(rgb(PRIMARY))
                                    .cursor_pointer()
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.add_project(cx);
                            })),
                    ),
            )
    }

    fn session_launch_progress(&self, progress: SessionLaunchProgress) -> impl IntoElement {
        let ready_path = match progress {
            SessionLaunchProgress::CreatingWorktree => None,
            SessionLaunchProgress::WorktreeReady(path) => Some(self.display_worktree_path(&path)),
        };
        div()
            .id("session-launch-progress")
            .role(Role::Status)
            .aria_label(if ready_path.is_some() {
                "Worktree created; starting Copilot session"
            } else {
                "Creating worktree"
            })
            .w_full()
            .max_w(px(CONVERSATION_COLUMN_WIDTH))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .id("launch-creating-worktree")
                    .debug_selector(|| "launch-creating-worktree".to_owned())
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_color(rgb(MUTED))
                    .child(if ready_path.is_some() {
                        div()
                            .w(px(18.0))
                            .text_color(rgb(GREEN))
                            .child("✓")
                            .into_any_element()
                    } else {
                        progress_spinner("launch-worktree-spinner".into()).into_any_element()
                    })
                    .child("Creating worktree..."),
            )
            .when_some(ready_path, |column, path| {
                column
                    .child(
                        div()
                            .id("launch-worktree-ready")
                            .debug_selector(|| "launch-worktree-ready".to_owned())
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_color(rgb(MUTED))
                            .child(div().w(px(18.0)).text_color(rgb(GREEN)).child("✓"))
                            .child("Worktree ready")
                            .child(div().min_w_0().overflow_hidden().child(path)),
                    )
                    .child(
                        div()
                            .id("launch-copilot-session")
                            .debug_selector(|| "launch-copilot-session".to_owned())
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_color(rgb(MUTED))
                            .child(progress_spinner("launch-session-spinner".into()))
                            .child("Starting Copilot session..."),
                    )
            })
    }

    #[allow(clippy::too_many_lines)]
    fn home(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let launch = self.session_launch.clone();
        let launching = launch.is_some();

        div()
            .id("home")
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .when(compact, gpui::StatefulInteractiveElement::overflow_y_scroll)
            .when(!compact, gpui::Styled::overflow_hidden)
            .px(if compact { px(24.0) } else { px(40.0) })
            .pb_6()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .w_full()
                    .pt(if compact { px(92.0) } else { px(118.0) })
                    .when(!launching, |column| {
                        column.child(
                            div()
                                .id("gcabb-mark")
                                .w(px(72.0))
                                .h(px(72.0))
                                .mb_10()
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(rgb(MUTED))
                                .text_color(rgb(BACKGROUND))
                                .text_xl()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("GC"),
                        )
                    })
                    .when_some(launch, |column, launch| {
                        column.child(self.session_launch_progress(launch))
                    })
                    .when(!launching, |column| column.child(self.home_composer(cx))),
            )
    }

    #[allow(clippy::too_many_lines)]
    fn interaction_prompt(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.selected()?;
        let interaction = session
            .snapshot
            .pending_interactions
            .iter()
            .find(|interaction| interaction.kind != InteractionKind::Permission)?
            .clone();
        let app_session_id = session.id().to_owned();
        let interaction_id = interaction.id.clone();
        let reject = interaction_id.clone();
        let cancel_session = app_session_id.clone();
        let choices = interaction
            .choices
            .iter()
            .enumerate()
            .filter(|_| interaction.kind != InteractionKind::Permission)
            .map(|(index, choice)| {
                let choice = choice.clone();
                let kind = interaction.kind;
                let id = interaction_id.clone();
                let session_id = app_session_id.clone();
                let selector = format!("interaction-choice-{index}");
                div()
                    .id(("interaction-choice", index))
                    .debug_selector({
                        let selector = selector.clone();
                        move || selector.clone()
                    })
                    .accessibility_id(selector)
                    .role(Role::Button)
                    .aria_label(choice.clone())
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .when(index == 0, |row| row.bg(rgb(SUBTLE)))
                    .child(
                        div()
                            .w(px(28.0))
                            .text_color(rgb(MUTED))
                            .child(format!("{}.", index + 1)),
                    )
                    .child(div().flex_1().child(choice.clone()))
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, _| {
                        let _ = view.commands.send(ServiceCommand::Respond {
                            app_session_id: session_id.clone(),
                            interaction_id: id.clone(),
                            response: choice_response(kind, &choice),
                        });
                    }))
            });
        let permission_choices = interaction
            .choices
            .iter()
            .filter(|choice| choice.as_str() != "Deny")
            .cloned()
            .collect::<Vec<_>>();
        Some(
            div()
                .id("interaction-prompt")
                .debug_selector(|| "interaction-prompt".to_owned())
                .accessibility_id("interaction-prompt")
                .role(Role::Group)
                .aria_label(interaction.title.clone())
                .w_full()
                .px_5()
                .pb_3()
                .child(
                    div()
                        .id("interaction-panel")
                        .mx_auto()
                        .w_full()
                        .max_w(px(CONVERSATION_COLUMN_WIDTH))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .id("interaction-heading")
                                .role(Role::Heading)
                                .aria_level(2)
                                .aria_label(interaction.title.clone())
                                .text_lg()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(interaction.title),
                        )
                        .when(interaction.kind == InteractionKind::Permission, |dialog| {
                            dialog.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .child("REQUESTED ACTION"),
                                    )
                                    .child(
                                        div()
                                            .p_3()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(BORDER))
                                            .bg(rgb(SUBTLE))
                                            .child(interaction.message.clone()),
                                    ),
                            )
                        })
                        .when(interaction.kind != InteractionKind::Permission, |dialog| {
                            dialog.child(div().text_color(rgb(MUTED)).child(interaction.message))
                        })
                        .when(
                            interaction.kind != InteractionKind::Permission
                                && !interaction.choices.is_empty(),
                            |dialog| dialog.child(div().h(px(1.0)).bg(rgb(BORDER)).mx(px(-16.0))),
                        )
                        .children(choices)
                        .when(interaction.allow_freeform, |dialog| {
                            dialog.child(
                                div()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .rounded_md()
                                    .child(self.interaction_input.clone()),
                            )
                        })
                        .when(interaction.kind == InteractionKind::Permission, |dialog| {
                            let session_id = app_session_id.clone();
                            let scope_choices =
                                permission_choices
                                    .iter()
                                    .enumerate()
                                    .map(|(index, choice)| {
                                        let choice = choice.clone();
                                        let description = permission_scope_description(&choice);
                                        let response_choice = choice.clone();
                                        let response_session = app_session_id.clone();
                                        let response_id = interaction_id.clone();
                                        let selector = format!("permission-scope-{index}");
                                        div()
                                            .id(("permission-scope", index))
                                            .debug_selector({
                                                let selector = selector.clone();
                                                move || selector.clone()
                                            })
                                            .accessibility_id(selector)
                                            .role(Role::Button)
                                            .aria_label(format!("{choice}. {description}"))
                                            .focusable()
                                            .tab_stop(true)
                                            .focus_visible(|style| {
                                                style.border_1().border_color(rgb(BLUE))
                                            })
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .px_3()
                                            .py_3()
                                            .rounded_md()
                                            .when(index == 0, |row| row.bg(rgb(SUBTLE)))
                                            .child(
                                                div()
                                                    .w(px(28.0))
                                                    .text_color(rgb(MUTED))
                                                    .child(format!("{}.", index + 1)),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .flex_1()
                                                    .gap_1()
                                                    .child(
                                                        div()
                                                            .font_weight(gpui::FontWeight::MEDIUM)
                                                            .child(choice),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(rgb(MUTED))
                                                            .child(description),
                                                    ),
                                            )
                                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                            .on_click(cx.listener(move |view, _, _, _| {
                                                let _ =
                                                    view.commands.send(ServiceCommand::Respond {
                                                        app_session_id: response_session.clone(),
                                                        interaction_id: response_id.clone(),
                                                        response: choice_response(
                                                            InteractionKind::Permission,
                                                            &response_choice,
                                                        ),
                                                    });
                                            }))
                                    });
                            dialog
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .mt_1()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .child("ALLOW THIS ACTION"),
                                        )
                                        .children(scope_choices),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .items_center()
                                        .mt_1()
                                        .child(div().text_xs().text_color(rgb(MUTED)).child(
                                            "Project rules can be changed in Copilot settings.",
                                        ))
                                        .child(action_button("Deny", RED, cx, move |view, _| {
                                            let _ = view.commands.send(ServiceCommand::Respond {
                                                app_session_id: session_id.clone(),
                                                interaction_id: reject.clone(),
                                                response: InteractionResponse::Reject {
                                                    feedback: None,
                                                },
                                            });
                                        })),
                                )
                        })
                        .when(interaction.kind != InteractionKind::Permission, |dialog| {
                            dialog.child(div().flex().justify_end().child(action_button(
                                "Cancel",
                                RED,
                                cx,
                                move |view, _| {
                                    let _ = view.commands.send(ServiceCommand::Respond {
                                        app_session_id: cancel_session.clone(),
                                        interaction_id: interaction_id.clone(),
                                        response: InteractionResponse::Cancel,
                                    });
                                },
                            )))
                        }),
                ),
        )
    }
}

impl Render for SessionMvpView {
    #[allow(clippy::too_many_lines)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_transcript();
        let compact = compact_layout(f32::from(window.viewport_size().width));
        let show_sidebar = self.sidebar_open;
        let content_left = if show_sidebar {
            if compact { 300.0 } else { 280.0 }
        } else {
            0.0
        };
        let control_menu_left = self.open_control_menu.map_or(0, control_menu_offset);
        let session_selected = self.selected_session.is_some();
        let session_unavailable = self
            .selected()
            .is_some_and(|session| session.snapshot.status == SessionStatus::Unavailable);
        let title = self.selected().map_or_else(
            || "New session".to_owned(),
            |session| session.snapshot.metadata.title.clone(),
        );
        // The session's own worktree branch, not the repository default. The
        // changes view already resolved it, so no extra git call is needed.
        // A chat has no checkout, so it reports no repository instead of
        // inheriting an unrelated branch name.
        let chat = self
            .selected()
            .is_some_and(|session| session.snapshot.metadata.is_chat());
        let branch = if chat {
            "no repository".to_owned()
        } else {
            self.selected()
                .and_then(|session| session.snapshot.changes.branch.clone())
                .filter(|branch| !branch.is_empty())
                .unwrap_or_else(|| self.branch.clone())
        };
        let workspace = self.selected().map_or_else(
            || self.workspace_root.clone(),
            |session| PathBuf::from(&session.snapshot.metadata.project_path),
        );
        let session_error = self
            .selected()
            .and_then(|session| session.snapshot.last_error.clone());
        div()
            .id("gcabb")
            .accessibility_id("gcabb")
            .role(Role::Application)
            .aria_label("GCABB")
            .on_action(cx.listener(|_, _: &FocusNext, window, cx| {
                window.focus_next(cx);
            }))
            .on_action(cx.listener(|_, _: &FocusPrevious, window, cx| {
                window.focus_prev(cx);
            }))
            .on_action(cx.listener(|view, _: &DismissPopup, _, cx| {
                view.dismiss_control_menu(cx);
                view.dismiss_session_menu(cx);
                view.dismiss_image_preview(cx);
                view.diagnostics_visibility = SettingsVisibility::Closed;
                view.settings_visibility = SettingsVisibility::Closed;
                if view.renaming_session.is_some() {
                    view.cancel_rename(cx);
                }
            }))
            .relative()
            .flex()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_sm()
            .text_color(rgb(PRIMARY))
            // Scrollbar drags are tracked at the window so the thumb keeps
            // following the pointer once it leaves the narrow track.
            .on_mouse_move(cx.listener(|view, event: &gpui::MouseMoveEvent, _, cx| {
                if let Some(drag) = view.dragging_scrollbar.clone() {
                    if event.pressed_button == Some(MouseButton::Left) {
                        view.drag_scrollbar_to(&drag.id, event.position.y, drag.grab_offset);
                        cx.notify();
                    } else {
                        view.end_scrollbar_drag();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, _| {
                    view.end_scrollbar_drag();
                }),
            )
            .when(show_sidebar, |root| root.child(self.sidebar(compact, cx)))
            .child(
                div()
                    .id("main-content")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .when_some(self.update_banner(cx), gpui::ParentElement::child)
                    .when(!show_sidebar, |main| {
                        main.child(
                            div()
                                .id("collapsed-titlebar")
                                .absolute()
                                .top_0()
                                .left_0()
                                .h(px(56.0))
                                .flex()
                                .items_center()
                                .pl_3()
                                .child(
                                    div()
                                        .id("sidebar-toggle")
                                        .accessibility_id("sidebar-toggle")
                                        .role(Role::Button)
                                        .aria_label("Expand sidebar")
                                        .aria_expanded(false)
                                        .focusable()
                                        .tab_stop(true)
                                        .focus_visible(|style| {
                                            style.border_1().border_color(rgb(BLUE))
                                        })
                                        .w(px(24.0))
                                        .h(px(24.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .text_color(rgb(MUTED))
                                        .child("▯")
                                        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.toggle_sidebar(cx);
                                        })),
                                ),
                        )
                    })
                    .when(self.selected_session.is_none(), |main| {
                        main.child(self.home(compact, cx))
                    })
                    .when(self.selected_session.is_some(), |main| {
                        main.child(
                            div()
                                .h(px(56.0))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px_5()
                                .border_b_1()
                                .border_color(rgb(BORDER))
                                .child(div().flex().flex_col().child(div().child(title)).child(
                                    div().text_xs().text_color(rgb(MUTED)).child(format!(
                                        "{} · {}",
                                        workspace.display(),
                                        branch
                                    )),
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .when(session_unavailable, |status| {
                                            status.child(
                                                div()
                                                    .id("worktree-unavailable-badge")
                                                    .debug_selector(|| {
                                                        "worktree-unavailable-badge".to_owned()
                                                    })
                                                    .role(Role::Status)
                                                    .aria_label("Worktree unavailable")
                                                    .px_2()
                                                    .py_1()
                                                    .rounded_md()
                                                    .text_xs()
                                                    .text_color(rgb(AMBER))
                                                    .bg(rgb(SUBTLE))
                                                    .child("Worktree unavailable"),
                                            )
                                        })
                                        .child(
                                            div()
                                                .id("panel-toggle")
                                                .accessibility_id("panel-toggle")
                                                .role(Role::Button)
                                                .aria_label("Toggle session inspector")
                                                .aria_expanded(self.panel_open)
                                                .focusable()
                                                .tab_stop(true)
                                                .focus_visible(|style| {
                                                    style.border_1().border_color(rgb(BLUE))
                                                })
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .child(changes_badge(self.selected()))
                                                .hover(|style| {
                                                    style.bg(rgb(ELEVATED)).cursor_pointer()
                                                })
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    view.panel_open = !view.panel_open;
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        )
                    })
                    .when(self.selected_session.is_some(), |main| {
                        main.child(
                            div()
                                .flex()
                                .flex_1()
                                .min_h_0()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .min_h_0()
                                        .child(self.transcript(cx))
                                        .when_some(
                                            self.running_activity(),
                                            gpui::ParentElement::child,
                                        )
                                        .when_some(session_error, |column, error| {
                                            column.child(
                                                div()
                                                    .id("session-error")
                                                    .debug_selector(|| "session-error".to_owned())
                                                    .role(Role::Alert)
                                                    .aria_label(error.clone())
                                                    .mx_auto()
                                                    .mb_2()
                                                    .text_sm()
                                                    .text_color(rgb(RED))
                                                    .child(error),
                                            )
                                        })
                                        .when_some(self.action_error.clone(), |column, error| {
                                            column.child(
                                                div()
                                                    .id("action-error")
                                                    .role(Role::Alert)
                                                    .aria_label(error.clone())
                                                    .mx_auto()
                                                    .mb_2()
                                                    .text_sm()
                                                    .text_color(rgb(RED))
                                                    .child(error),
                                            )
                                        })
                                        .when_some(self.interaction_prompt(cx), |column, prompt| {
                                            column.child(prompt)
                                        })
                                        .when(
                                            self.selected().is_some_and(|session| {
                                                session.snapshot.pending_interactions.is_empty()
                                            }),
                                            |column| {
                                                column.child(
                                                    div()
                                                        .w_full()
                                                        .px_5()
                                                        .child(self.session_composer(cx)),
                                                )
                                            },
                                        ),
                                )
                                .when_some(
                                    if self.panel_open {
                                        self.side_panel(cx)
                                    } else {
                                        None
                                    },
                                    gpui::ParentElement::child,
                                ),
                        )
                    }),
            )
            .when(self.open_control_menu.is_some(), |root| {
                root.child(
                    div()
                        .id("dismiss-control-menu")
                        .absolute()
                        .inset_0()
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|view, _, _, cx| view.dismiss_control_menu(cx)),
                        ),
                )
            })
            .when_some(self.control_menu(cx), |root, menu| {
                root.child(
                    div()
                        .absolute()
                        .left(px(content_left))
                        .right_0()
                        .when(session_selected, |popup| popup.bottom(px(104.0)))
                        .when(!session_selected, |popup| {
                            popup.top(if compact { px(310.0) } else { px(332.0) })
                        })
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(CONVERSATION_COLUMN_WIDTH))
                                .pl(px(f32::from(control_menu_left)))
                                .child(menu),
                        ),
                )
            })
            .when(
                self.session_menu.is_some() || self.project_menu.is_some(),
                |root| {
                    root.child(
                        div()
                            .id("dismiss-context-menu")
                            .absolute()
                            .inset_0()
                            // Dismiss on mouse up, not mouse down: tearing the menu
                            // down on press removes the item before its click can
                            // complete on release.
                            //
                            // Only the left button dismisses. The right-click that
                            // opens the menu releases *after* this overlay exists,
                            // so a right-button handler here would immediately
                            // close the menu the same click just opened.
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|view, _, _, cx| view.dismiss_context_menu(cx)),
                            ),
                    )
                },
            )
            .when_some(self.session_context_menu(cx), gpui::ParentElement::child)
            .when_some(self.project_context_menu(cx), gpui::ParentElement::child)
            .when_some(self.rename_dialog(cx), gpui::ParentElement::child)
            .when_some(self.settings_dialog(cx), gpui::ParentElement::child)
            .when_some(self.diagnostics_dialog(cx), gpui::ParentElement::child)
            .when_some(self.image_preview_overlay(cx), gpui::ParentElement::child)
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let hours = seconds / 3600;
    let minutes = seconds / 60;
    let minutes = minutes % 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else if minutes > 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{seconds}s")
    }
}

fn timestamp_millis(timestamp: &str) -> Option<u128> {
    timestamp.parse::<u128>().ok().or_else(|| {
        DateTime::parse_from_rfc3339(timestamp)
            .ok()
            .and_then(|timestamp| u128::try_from(timestamp.timestamp_millis()).ok())
    })
}

fn format_activity_timestamp(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp).map_or_else(
        |_| timestamp.to_owned(),
        |timestamp| {
            timestamp
                .with_timezone(&chrono::Local)
                .format("%b %-d, %Y · %-I:%M %p")
                .to_string()
        },
    )
}

/// `metadata.created_at`/`updated_at` are always epoch-millis strings (see
/// `session-manager`'s `timestamp()`), unlike SDK-sourced event timestamps
/// which are RFC3339. Format directly rather than routing through the
/// RFC3339-only `format_activity_timestamp`.
fn format_session_created_at(created_at: &str) -> String {
    created_at
        .parse::<i64>()
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(
            || created_at.to_owned(),
            |timestamp| {
                timestamp
                    .with_timezone(&chrono::Local)
                    .format("%b %-d, %Y · %-I:%M %p")
                    .to_string()
            },
        )
}

fn tool_duration(invocation: &app_model::ToolInvocation) -> Option<Duration> {
    let started = timestamp_millis(&invocation.started_at)?;
    let elapsed = invocation.completed_at.as_deref().map_or_else(
        || elapsed_since_timestamp(&invocation.started_at),
        |completed| {
            let completed = timestamp_millis(completed)?;
            Some(Duration::from_millis(
                u64::try_from(completed.saturating_sub(started)).unwrap_or(u64::MAX),
            ))
        },
    )?;
    Some(elapsed)
}

fn format_activity_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{}ms", duration.as_millis())
    } else {
        format_elapsed(duration)
    }
}

fn format_byte_count(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    let (unit, suffix) = if bytes >= MIB {
        (MIB, "MB")
    } else if bytes >= KIB {
        (KIB, "KB")
    } else {
        return format!("{bytes} B");
    };
    let whole = bytes / unit;
    let tenths = ((bytes % unit) * 10 + unit / 2) / unit;
    if tenths == 10 {
        format!("{}.0 {suffix}", whole + 1)
    } else {
        format!("{whole}.{tenths} {suffix}")
    }
}

fn elapsed_since_timestamp(timestamp: &str) -> Option<Duration> {
    let event_millis = timestamp_millis(timestamp)?;
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(Duration::from_millis(
        u64::try_from(now_millis.saturating_sub(event_millis)).unwrap_or(u64::MAX),
    ))
}

fn diagnostic_field(label: impl Into<String>, value: impl Into<String>) -> gpui::AnyElement {
    div()
        .flex()
        .items_start()
        .gap_3()
        .child(
            div()
                .w(px(170.0))
                .flex_none()
                .text_xs()
                .text_color(rgb(MUTED))
                .child(label.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(rgb(PRIMARY))
                .child(value.into()),
        )
        .into_any_element()
}

fn diagnostic_section(title: &str, rows: Vec<gpui::AnyElement>) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .child(title.to_owned()),
        )
        .when(rows.is_empty(), |section| {
            section.child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("No data reported"),
            )
        })
        .children(rows)
        .into_any_element()
}

/// Trailing slice of terminal output displayed until transcript virtualization.
fn terminal_tail(output: &str) -> String {
    tail_lines(output, 40)
}

fn output_needs_preview(output: &str) -> bool {
    output.len() > LIVE_OUTPUT_PREVIEW_BYTES
        || output
            .lines()
            .rev()
            .take(LIVE_OUTPUT_PREVIEW_LINES + 1)
            .count()
            > LIVE_OUTPUT_PREVIEW_LINES
}

fn live_output_preview(output: &str) -> String {
    let mut byte_start = output.len().saturating_sub(LIVE_OUTPUT_PREVIEW_BYTES);
    while !output.is_char_boundary(byte_start) {
        byte_start += 1;
    }
    let window = &output[byte_start..];
    let newline_index = if window.ends_with('\n') {
        LIVE_OUTPUT_PREVIEW_LINES
    } else {
        LIVE_OUTPUT_PREVIEW_LINES - 1
    };
    let line_start = window
        .match_indices('\n')
        .rev()
        .nth(newline_index)
        .map_or(0, |(index, _)| index + 1);
    let start = byte_start + line_start;
    if start == 0 {
        return output.to_owned();
    }
    format!(
        "[showing latest output; earlier output is retained]\n{}",
        &output[start..]
    )
}

fn terminal_state_display(state: app_model::TerminalState) -> (&'static str, u32) {
    match state {
        app_model::TerminalState::Running => ("Still running", GREEN),
        app_model::TerminalState::Exited => ("Completed", MUTED),
        app_model::TerminalState::Cancelled => ("Interrupted", RED),
    }
}

fn terminal_title(terminal: &app_model::TerminalSession) -> String {
    terminal
        .command
        .clone()
        .unwrap_or_else(|| "Shell command".to_owned())
}

fn terminal_output_error(terminal: &app_model::TerminalSession) -> Option<String> {
    terminal
        .output_load_error
        .clone()
        .or_else(|| terminal.output_error.clone())
}

/// The last `max_lines` lines of `output`.
fn tail_lines(output: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return String::new();
    }
    let mut lines = output.lines().rev().take(max_lines + 1).collect::<Vec<_>>();
    if lines.len() <= max_lines {
        return output.to_owned();
    }
    lines.truncate(max_lines);
    lines.reverse();
    lines.join("\n")
}

/// Label for the inspector toggle, summarizing changed files at a glance.
fn changes_badge(session: Option<&SessionProjection>) -> String {
    session.map_or_else(
        || "Inspector".to_owned(),
        |session| {
            let changed = session.snapshot.changes.files.len();
            let terminals = session.snapshot.tool_activity.active_terminals().len();
            let blocking = session
                .snapshot
                .capabilities
                .blocking_for(session.snapshot.metadata.kind)
                .len();
            let mut parts = vec![format!("{changed} changed")];
            if terminals > 0 {
                parts.push(format!("{terminals} running"));
            }
            if blocking > 0 {
                parts.push(format!("{blocking} blocked"));
            }
            parts.join(" · ")
        },
    )
}

fn control_pill(
    id: &'static str,
    value: String,
    menu: ControlMenu,
    expanded: bool,
    cx: &mut Context<SessionMvpView>,
) -> impl IntoElement {
    let label = match menu {
        ControlMenu::Project => "Project",
        ControlMenu::Location => "Where to run this session",
        ControlMenu::Mode => "Mode",
        ControlMenu::Model => "Model",
        ControlMenu::Effort => "Reasoning effort",
        ControlMenu::Context => "Context length",
    };
    div()
        .id(id)
        .accessibility_id(id)
        .role(Role::ComboBox)
        .aria_label(label)
        .aria_value(value.clone())
        .aria_expanded(expanded)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(ELEVATED))
        .text_xs()
        .text_color(rgb(MUTED))
        .child(value)
        .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
        .on_click(cx.listener(move |view, _, _, cx| {
            view.toggle_control_menu(menu);
            cx.notify();
        }))
}

fn context_readout(value: String) -> impl IntoElement {
    div()
        .id("context")
        .accessibility_id("context")
        .role(Role::Definition)
        .aria_label("Context length")
        .px_3()
        .py_1()
        .text_xs()
        .text_color(rgb(MUTED))
        .child(value)
}

fn control_menu_id(menu: ControlMenu) -> &'static str {
    match menu {
        ControlMenu::Project => "project",
        ControlMenu::Location => "location",
        ControlMenu::Mode => "mode",
        ControlMenu::Model => "model",
        ControlMenu::Effort => "effort",
        ControlMenu::Context => "context",
    }
}

fn disabled_destination(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .text_color(rgb(MUTED))
        .child(icon)
        .child(label)
        .child(div().flex_1())
        .child(div().text_xs().child("Unavailable"))
}

fn compact_layout(width: f32) -> bool {
    width < COMPACT_WIDTH
}

fn control_menu_offset(menu: ControlMenu) -> u16 {
    match menu {
        // The project and location pills sit in the checkout strip below the
        // composer, left to right.
        ControlMenu::Project => 0,
        ControlMenu::Location => 96,
        ControlMenu::Mode => 40,
        ControlMenu::Model => 128,
        ControlMenu::Effort => 216,
        ControlMenu::Context => 304,
    }
}

fn toggled_menu(current: Option<ControlMenu>, requested: ControlMenu) -> Option<ControlMenu> {
    (current != Some(requested)).then_some(requested)
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + characters.as_str()
    })
}

fn effort_label(value: &str) -> String {
    match value {
        "xhigh" => "Extra high".to_owned(),
        other => title_case(other),
    }
}

fn reasoning_effort_for_model(supported_efforts: &[String], selected: &str) -> Option<String> {
    (!supported_efforts.is_empty()).then(|| selected.to_owned())
}

fn default_context_tier(windows: &[ContextWindowOption]) -> Option<String> {
    windows
        .iter()
        .find(|window| window.tier == "default")
        .or_else(|| windows.first())
        .map(|window| window.tier.clone())
}

fn context_window_label(window: &ContextWindowOption) -> String {
    window.max_tokens.map_or_else(
        || match window.tier.as_str() {
            "long_context" => "Long context".to_owned(),
            other => title_case(other),
        },
        |tokens| format!("{} context", token_label(tokens)),
    )
}

fn token_label(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let tenths = tokens / 100_000;
        if tenths.is_multiple_of(10) {
            format!("{}M", tenths / 10)
        } else {
            format!("{}.{}M", tenths / 10, tenths % 10)
        }
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn action_button(
    label: &'static str,
    color: u32,
    cx: &mut Context<SessionMvpView>,
    action: impl Fn(&mut SessionMvpView, &mut Context<SessionMvpView>) + 'static,
) -> impl IntoElement {
    div()
        .id(label)
        .debug_selector(move || label.to_owned())
        .accessibility_id(label)
        .role(Role::Button)
        .aria_label(label)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(color))
        .text_color(rgb(BACKGROUND))
        .child(label)
        .hover(|style| style.opacity(0.85).cursor_pointer())
        .on_click(cx.listener(move |view, _, _, cx| {
            action(view, cx);
            cx.notify();
        }))
}

fn status_color(status: SessionStatus) -> gpui::Rgba {
    match status {
        SessionStatus::Running | SessionStatus::Starting => rgb(GREEN),
        SessionStatus::Waiting => rgb(AMBER),
        SessionStatus::Failed | SessionStatus::Cancelled => rgb(RED),
        SessionStatus::Idle
        | SessionStatus::Recovering
        | SessionStatus::Disconnected
        | SessionStatus::Unavailable => rgb(MUTED),
    }
}

fn session_is_running(status: SessionStatus) -> bool {
    matches!(status, SessionStatus::Running | SessionStatus::Starting)
}

/// Frames of the shared progress spinner.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Animated progress glyph used for both deletion and active agent work.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn progress_spinner(id: SharedString) -> impl IntoElement {
    div()
        .w(px(14.0))
        .h(px(14.0))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(14.0))
        .line_height(px(14.0))
        .text_color(rgb(MUTED))
        .with_animation(
            id,
            Animation::new(Duration::from_millis(800)).repeat(),
            |this, delta| {
                let frame_ix =
                    ((delta * SPINNER_FRAMES.len() as f32) as usize).min(SPINNER_FRAMES.len() - 1);
                this.child(SPINNER_FRAMES[frame_ix])
            },
        )
}

fn permission_scope_description(choice: &str) -> &'static str {
    match choice {
        "Allow once" => "Only this request will be approved.",
        "Allow for this session" => "Remember this approval until the session ends.",
        "Always allow for this project" => "Remember this approval for this project.",
        "Always allow this domain" => "Remember this website approval across sessions.",
        _ => "Approve this request.",
    }
}

fn choice_response(kind: InteractionKind, choice: &str) -> InteractionResponse {
    match (kind, choice) {
        (InteractionKind::Permission, "Allow for this session") => {
            InteractionResponse::ApproveForSession
        }
        (InteractionKind::Permission, "Always allow for this project") => {
            InteractionResponse::ApproveForLocation
        }
        (InteractionKind::Permission, "Always allow this domain") => {
            InteractionResponse::ApprovePermanently
        }
        (InteractionKind::Permission, "Allow once")
        | (InteractionKind::AutoModeSwitch, "Switch once") => InteractionResponse::Approve,
        (InteractionKind::AutoModeSwitch, "Always switch") => InteractionResponse::Submit {
            value: "always".into(),
            freeform: false,
        },
        (InteractionKind::Permission | InteractionKind::AutoModeSwitch, _) => {
            InteractionResponse::Reject { feedback: None }
        }
        _ => InteractionResponse::Submit {
            value: choice.to_owned().into(),
            freeform: false,
        },
    }
}

fn data_directory() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("GCABB_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    dirs::data_local_dir()
        .map(|base| base.join(DATA_DIRECTORY_NAME))
        .ok_or_else(|| "operating system did not provide a local data directory".to_owned())
}

/// Moves data written by older builds out of the replaceable install directory.
///
/// During an update, the old updater moves the complete installation to its
/// backup before launching the new build. Migrating before that backup is
/// cleaned preserves the database and user-created files across the transition
/// to the dedicated data directory.
fn prepare_data_directory_for_build(build: &BuildStamp) -> Result<PathBuf, String> {
    let data_dir = data_directory()?;
    if build.is_release() {
        let layout = InstallLayout::for_running_executable().map_err(|error| error.to_string())?;
        if std::env::var_os("GCABB_DATA_DIR").is_none() {
            let legacy = dirs::data_local_dir().map(|base| base.join("gcabb"));
            let mut sources = vec![layout.backup_root.clone()];
            if let Some(legacy) = legacy {
                sources.push(legacy);
            }
            migrate_persistent_data(&data_dir, &sources)?;
        }
        // Data is now independent of the installation, so the rollback copy can
        // be removed without deleting session state.
        layout.clean_completed_updates();
    }
    Ok(data_dir)
}

fn database_path(data_dir: &Path) -> Result<PathBuf, String> {
    prepare_data_directory(data_dir)
}

fn migrate_persistent_data(target: &Path, sources: &[PathBuf]) -> Result<(), String> {
    if target.exists() {
        return Ok(());
    }
    let source_with_entries = || {
        sources.iter().find(|source| {
            PERSISTENT_DATA_ENTRIES
                .iter()
                .any(|entry| source.join(entry).exists())
        })
    };
    let Some(source) = sources
        .iter()
        .find(|source| source.join("gcabb.db").exists())
        .or_else(source_with_entries)
    else {
        return Ok(());
    };

    let staging = target.with_extension("migrating");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|error| format!("failed to clear {}: {error}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)
        .map_err(|error| format!("failed to create {}: {error}", staging.display()))?;
    for entry in PERSISTENT_DATA_ENTRIES {
        let from = source.join(entry);
        if from.exists() {
            copy_persistent_path(&from, &staging.join(entry))?;
        }
    }
    std::fs::rename(&staging, target).map_err(|error| {
        format!(
            "failed to finish data migration from {} to {}: {error}",
            source.display(),
            target.display()
        )
    })
}

fn copy_persistent_path(from: &Path, to: &Path) -> Result<(), String> {
    if from.is_dir() {
        std::fs::create_dir_all(to)
            .map_err(|error| format!("failed to create {}: {error}", to.display()))?;
        let entries = std::fs::read_dir(from)
            .map_err(|error| format!("failed to read {}: {error}", from.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("failed to read {}: {error}", from.display()))?;
            copy_persistent_path(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to).map_err(|error| {
            format!(
                "failed to copy {} to {}: {error}",
                from.display(),
                to.display()
            )
        })?;
    }
    Ok(())
}

/// Working directory for chats.
///
/// Chats have no repository, but the CLI still needs a valid working
/// directory. A dedicated folder under the app data directory keeps chat tool
/// activity away from any checkout; if it cannot be created, fall back to the
/// launch directory so chats still work.
fn chats_directory(fallback: &Path) -> PathBuf {
    let Ok(base) = data_directory() else {
        return fallback.to_owned();
    };
    let path = base.join("chats");
    if std::fs::create_dir_all(&path).is_err() {
        return fallback.to_owned();
    }
    path
}

/// Where pasted images are kept.
///
/// Deliberately not the session worktree: files written there would appear in
/// the changes view and could be committed by accident. The runtime references
/// an attached file in place rather than copying it, so this has to outlive the
/// composer for the transcript to still show the picture later.
fn attachments_directory() -> Option<PathBuf> {
    let base = data_directory().ok()?;
    let path = base.join("attachments");
    std::fs::create_dir_all(&path).ok()?;
    Some(path)
}

/// Normalize clipboard images to the format accepted across vision providers.
fn normalize_pasted_image(bytes: &[u8], mime_type: &str) -> Result<(Vec<u8>, String), String> {
    let format = image::guess_format(bytes)
        .ok()
        .or_else(|| image::ImageFormat::from_mime_type(mime_type))
        .ok_or_else(|| format!("Unsupported pasted image format: {mime_type}"))?;
    if format == image::ImageFormat::Png {
        return Ok((bytes.to_vec(), "image/png".to_owned()));
    }
    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|error| format!("Could not decode pasted {mime_type} image: {error}"))?;
    let mut png = std::io::Cursor::new(Vec::new());
    decoded
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|error| format!("Could not convert pasted {mime_type} image to PNG: {error}"))?;
    Ok((png.into_inner(), "image/png".to_owned()))
}

/// Write a pasted image to disk so it can be referenced by path.
fn write_pasted_image(
    directory: &Path,
    bytes: &[u8],
    mime_type: &str,
    index: usize,
) -> Option<PromptAttachment> {
    if mime_type != "image/png" {
        return None;
    }
    let path = directory.join(format!("{}-clipboard.png", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).ok()?;
    Some(PromptAttachment::File {
        path: path.to_string_lossy().into_owned(),
        display_name: format!("Pasted image {index}"),
    })
}

fn prepare_data_directory(path: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    Ok(path.join("gcabb.db"))
}

/// The branch currently checked out in `root`.
fn git_branch(root: &Path) -> String {
    git_output(root, &["branch", "--show-current"]).unwrap_or_else(|| "detached".to_owned())
}

/// The repository a worktree belongs to.
///
/// A repository has one main checkout plus any number of linked worktrees, and
/// `git worktree list` reports the main checkout first. Sessions run inside
/// worktrees but belong to the repository; grouping by worktree path would
/// otherwise show one project per worktree instead of one project per
/// repository. Falls back to `root` when it is not a git worktree.
fn repository_root(root: &Path) -> PathBuf {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    git_output(&root, &["worktree", "list", "--porcelain"])
        .and_then(|output| {
            output
                .lines()
                .find_map(|line| line.strip_prefix("worktree ").map(str::to_owned))
        })
        .map_or(root, |path| {
            let path = PathBuf::from(path);
            path.canonicalize().unwrap_or(path)
        })
}

/// The repository's default branch, used as the changes-view base.
///
/// Resolution order is the remote's published HEAD, then conventional local
/// names. This is deliberately not the checked-out branch: comparing a session
/// worktree against its own branch would report no changes at all.
fn default_branch(root: &Path) -> Option<String> {
    if let Some(head) = git_output(
        root,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some(branch) = head.split_once('/').map(|(_, branch)| branch.to_owned()) {
            return Some(branch);
        }
        return Some(head);
    }
    for candidate in ["main", "master"] {
        if git_output(
            root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ],
        )
        .is_some()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn update_poll_delay() -> Duration {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
        ^ u64::from(std::process::id());
    update_poll_delay_for(seed)
}

fn update_poll_delay_for(seed: u64) -> Duration {
    let jitter_seconds = UPDATE_POLL_JITTER.as_secs();
    let offset = seed % (jitter_seconds * 2 + 1);
    UPDATE_POLL_INTERVAL.saturating_sub(UPDATE_POLL_JITTER) + Duration::from_secs(offset)
}

fn timestamp() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| "0".to_owned(),
        |duration| duration.as_millis().to_string(),
    )
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Install every key binding the app responds to.
///
/// Shared with the interaction tests so they exercise the bindings the app
/// actually ships. Tests that installed their own bindings once let a
/// macOS-only paste shortcut reach Linux users unnoticed.
fn bind_app_keys(cx: &mut App) {
    bind_text_input_keys(cx);
    cx.bind_keys([
        KeyBinding::new("secondary-c", CopyTranscript, Some("TranscriptSelection")),
        KeyBinding::new("escape", DismissPopup, None),
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrevious, None),
    ]);
}

/// Records the running build's identity.
fn resolve_build_identity() -> BuildStamp {
    let build = BuildStamp::current();
    tracing::info!(
        version = %build.version,
        channel = %build.channel,
        commit = build.commit.as_deref().unwrap_or("unknown"),
        target = build.target,
        release = build.is_release(),
        "gcabb build identity"
    );
    build
}

/// How the binary was asked to run.
enum Invocation {
    /// Open the application window.
    Desktop,
    /// Print the build identity and exit.
    Version,
    /// Report whether an update is available and exit.
    CheckUpdate,
    /// Apply an available update and exit.
    ApplyUpdate,
    Help,
    Unknown(String),
}

fn invocation() -> Invocation {
    match std::env::args().nth(1).as_deref() {
        None => Invocation::Desktop,
        Some("--version" | "-V") => Invocation::Version,
        Some("--check-update") => Invocation::CheckUpdate,
        Some("--apply-update") => Invocation::ApplyUpdate,
        Some("--help" | "-h") => Invocation::Help,
        Some(other) => Invocation::Unknown(other.to_owned()),
    }
}

const USAGE: &str = "\
GCABB

Usage:
  gcabb-desktop                 Open the application
  gcabb-desktop --version       Print the build identity
  gcabb-desktop --check-update  Report whether an update is available
  gcabb-desktop --apply-update  Download, verify, and apply an available update
  gcabb-desktop --help          Show this message

Exit codes for the update commands:
  0  an update is available, or was applied
  1  the check or the update failed
  2  nothing to do
";

fn main() {
    if let Err(error) = init_tracing("gcabb=info") {
        eprintln!("failed to initialize structured tracing: {error}");
    }
    if let Some(code) = updates::run_update_helper_if_requested() {
        std::process::exit(code);
    }
    let build = resolve_build_identity();
    let data_dir = prepare_data_directory_for_build(&build);

    // The update commands run the same code the window drives, so CI can
    // exercise the loop on each platform without driving a GUI.
    match invocation() {
        Invocation::Desktop => {}
        Invocation::Version => {
            println!("{}", build.display());
            return;
        }
        Invocation::Help => {
            print!("{USAGE}");
            return;
        }
        Invocation::Unknown(argument) => {
            eprintln!("unrecognised argument {argument}\n");
            print!("{USAGE}");
            std::process::exit(1);
        }
        command @ (Invocation::CheckUpdate | Invocation::ApplyUpdate) => {
            let apply = matches!(command, Invocation::ApplyUpdate);
            let code = match &data_dir {
                Ok(data_dir) => updates::run_headless(&build, data_dir, apply),
                Err(error) => {
                    eprintln!("{error}");
                    1
                }
            };
            std::process::exit(code);
        }
    }

    let window_title = format!("GCABB {}", build.display());
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let branch = git_branch(&project_root);
    let worktree_configuration = WorktreeConfiguration::load(&data_dir);
    let service = match data_dir
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|path| database_path(path))
    {
        Ok(path) => AppService::start(project_root.clone(), &path),
        Err(error) => AppService::failed(error),
    };
    let chats_workspace = chats_directory(&project_root);

    gpui_platform::application().run(move |cx: &mut App| {
        bind_app_keys(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1280.0), px(860.0)), cx);
        let service = service;
        let project_root = project_root.clone();
        let branch = branch.clone();
        let chats_workspace = chats_workspace.clone();
        let worktree_configuration = worktree_configuration.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(window_title.clone().into()),
                        ..Default::default()
                    }),
                    app_id: Some(APP_ID.to_owned()),
                    window_min_size: Some(size(px(640.0), px(520.0))),
                    ..Default::default()
                },
                move |_, cx| {
                    cx.new(|cx| {
                        SessionMvpView::new(
                            service,
                            project_root,
                            branch,
                            chats_workspace,
                            attachments_directory(),
                            worktree_configuration,
                            cx,
                        )
                    })
                },
            )
            .expect("failed to open GCABB window");
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                window.focus(&view.composer.focus_handle(cx), cx);
            })
            .expect("failed to focus composer");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use app_model::ContextWindowOption;

    use super::{
        COMPACT_WIDTH, ControlMenu, UPDATE_POLL_INTERVAL, UPDATE_POLL_JITTER, compact_layout,
        context_window_label, control_menu_id, control_menu_offset, default_branch,
        default_context_tier, effort_label, migrate_persistent_data, reasoning_effort_for_model,
        repository_root, toggled_menu, token_label, update_poll_delay_for,
    };
    use app_model::SessionLocation;
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?} failed");
    }

    /// A repository with one linked worktree.
    fn repo_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).expect("create main");
        git(&main, &["init", "--initial-branch=main"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "T"]);
        std::fs::write(main.join("a.txt"), "a\n").expect("write");
        git(&main, &["add", "."]);
        git(&main, &["commit", "-m", "base"]);
        let worktree = dir.path().join("wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                worktree.to_str().unwrap(),
                "-b",
                "feature",
            ],
        );
        (dir, main, worktree)
    }

    #[test]
    fn update_poll_jitter_stays_within_the_six_hour_window() {
        let minimum = UPDATE_POLL_INTERVAL.saturating_sub(UPDATE_POLL_JITTER);
        let maximum = UPDATE_POLL_INTERVAL + UPDATE_POLL_JITTER;

        assert_eq!(update_poll_delay_for(0), minimum);
        assert!(update_poll_delay_for(u64::MAX) <= maximum);
    }

    #[test]
    fn markdown_inline_styles_share_one_text_layout() {
        let document = crate::markdown::parse(
            "[#55](https://github.com/constructomech/gcabb/issues/55) Show **steering** [comments](https://example.com/comments)",
        );
        let content = super::SessionMvpView::markdown_inline_content(&document.children);

        assert_eq!(content.text, "#55 Show steering comments");
        assert_eq!(content.links.len(), 2);
        assert_eq!(&content.text[content.links[0].0.clone()], "#55");
        assert_eq!(
            content.links[0].1,
            "https://github.com/constructomech/gcabb/issues/55"
        );
        assert_eq!(&content.text[content.links[1].0.clone()], "comments");
        assert_eq!(content.links[1].1, "https://example.com/comments");
    }

    #[test]
    fn markdown_task_markers_are_monospace_without_code_background() {
        let document = crate::markdown::parse("- [x] done");
        let content = super::SessionMvpView::markdown_inline_content(&document.children);

        assert_eq!(content.text, "[x] done");
        assert_eq!(content.font_family_overrides[0].0, 0..4);
        assert_eq!(content.highlights[0].1.background_color, None);
    }

    #[test]
    fn tight_list_item_inline_nodes_share_one_text_layout() {
        let document = crate::markdown::parse("- Source: `Minecraft/`, `src/`, `handheld/`");
        let crate::markdown::MarkdownNode::Container(_, items) = &document.children[0] else {
            panic!("expected a list container");
        };
        let crate::markdown::MarkdownNode::Container(_, content) = &items[0] else {
            panic!("expected a list item container");
        };

        // Without grouping, every `Text` and `Code` node here would become its
        // own stacked block and the item would render one fragment per line.
        assert!(content.len() > 1);
        assert_eq!(
            super::SessionMvpView::markdown_runs(content),
            vec![super::MarkdownRun::Inline(0..content.len())]
        );

        let inline = super::SessionMvpView::markdown_inline_content(content);
        assert_eq!(inline.text, "Source: Minecraft/, src/, handheld/");
    }

    #[test]
    fn loose_list_item_keeps_nested_blocks_separate() {
        let document = crate::markdown::parse("- intro `code`\n\n  ```\nfenced\n```\n");
        let crate::markdown::MarkdownNode::Container(_, items) = &document.children[0] else {
            panic!("expected a list container");
        };
        let crate::markdown::MarkdownNode::Container(_, content) = &items[0] else {
            panic!("expected a list item container");
        };

        assert_eq!(
            super::SessionMvpView::markdown_runs(content),
            vec![super::MarkdownRun::Block(0), super::MarkdownRun::Block(1)]
        );
    }

    #[test]
    fn live_output_preview_bounds_ui_text_work() {
        let output = (0..2_000).fold(String::new(), |mut output, line| {
            writeln!(output, "compiler output line {line:04}").expect("write fixture");
            output
        });
        let preview = super::live_output_preview(&output);

        assert!(preview.starts_with("[showing latest output; earlier output is retained]\n"));
        assert!(preview.ends_with("compiler output line 1999\n"));
        assert!(
            preview.len()
                <= super::LIVE_OUTPUT_PREVIEW_BYTES
                    + "[showing latest output; earlier output is retained]\n".len()
        );
        assert!(preview.lines().count() <= super::LIVE_OUTPUT_PREVIEW_LINES + 1);
    }

    #[test]
    fn live_output_preview_keeps_a_utf8_safe_long_line_suffix() {
        let output = "é".repeat(super::LIVE_OUTPUT_PREVIEW_BYTES);
        let preview = super::live_output_preview(&output);

        assert!(preview.starts_with("[showing latest output; earlier output is retained]\n"));
        assert!(preview.ends_with('é'));
        assert!(
            preview.len()
                <= super::LIVE_OUTPUT_PREVIEW_BYTES
                    + "[showing latest output; earlier output is retained]\n".len()
        );
    }

    #[test]
    fn live_output_preview_counts_an_unterminated_line_toward_the_limit() {
        let output = (0..100)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = super::live_output_preview(&output);

        assert_eq!(
            preview
                .lines()
                .skip_while(|line| line.starts_with("[showing latest output;"))
                .count(),
            super::LIVE_OUTPUT_PREVIEW_LINES
        );
        assert!(preview.ends_with("line 99"));
    }

    #[test]
    fn search_briefs_report_zero_and_parsed_match_counts() {
        assert_eq!(
            super::SessionMvpView::reported_match_count("No matches found."),
            Some(0)
        );
        assert_eq!(
            super::SessionMvpView::reported_match_count(
                "[grep content: 76 matches across 1 file(s)]"
            ),
            Some(76)
        );
    }

    #[test]
    fn transcript_selection_clamps_when_streaming_rewrites_a_block() {
        let mut selection = super::TranscriptTextSelection::default();
        let original = gpui::SharedString::from("é");
        selection.begin("block".to_owned(), (1, 0), &original, 0);
        selection.extend("block".to_owned(), (1, 0), &original, "é".len());
        selection.register_block("block", (1, 0), "x".into());

        assert_eq!(selection.selected_text().as_deref(), Some("x"));
    }

    /// Adding a worktree folder must resolve to its repository, so adding a
    /// worktree and its main checkout cannot create two projects.
    #[test]
    fn adding_a_worktree_folder_resolves_to_the_repository() {
        let (_guard, main, worktree) = repo_with_worktree();
        let canonical_main = main.canonicalize().expect("canonical main worktree");
        assert_eq!(repository_root(&worktree), canonical_main);
        assert_eq!(repository_root(&main), canonical_main);
    }

    /// A plain directory that is not a repository is still usable as a project.
    #[test]
    fn adding_a_non_repository_folder_keeps_the_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonical tempdir");
        assert_eq!(repository_root(dir.path()), canonical);
        assert!(default_branch(dir.path()).is_none());
    }

    #[test]
    fn update_backup_data_is_migrated_without_installation_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backup = dir.path().join(".GCABB-update-backup");
        let target = dir.path().join("GCABB-data");
        std::fs::create_dir_all(backup.join("attachments")).expect("attachments");
        std::fs::write(backup.join("gcabb.db"), b"database").expect("database");
        std::fs::write(backup.join("gcabb.db-wal"), b"wal").expect("wal");
        std::fs::write(backup.join("attachments").join("image.png"), b"image").expect("attachment");
        std::fs::write(backup.join("gcabb-desktop.exe"), b"binary").expect("binary");

        migrate_persistent_data(&target, &[backup]).expect("migration");

        assert_eq!(
            std::fs::read(target.join("gcabb.db")).expect("migrated database"),
            b"database"
        );
        assert_eq!(
            std::fs::read(target.join("gcabb.db-wal")).expect("migrated wal"),
            b"wal"
        );
        assert_eq!(
            std::fs::read(target.join("attachments").join("image.png"))
                .expect("migrated attachment"),
            b"image"
        );
        assert!(!target.join("gcabb-desktop.exe").exists());
    }

    #[test]
    fn existing_data_directory_is_never_overwritten_by_a_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backup = dir.path().join(".GCABB-update-backup");
        let target = dir.path().join("GCABB-data");
        std::fs::create_dir_all(&backup).expect("backup");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(backup.join("gcabb.db"), b"old").expect("old database");
        std::fs::write(target.join("gcabb.db"), b"current").expect("current database");

        migrate_persistent_data(&target, &[backup]).expect("migration");

        assert_eq!(
            std::fs::read(target.join("gcabb.db")).expect("current database"),
            b"current"
        );
    }

    #[test]
    fn migration_prefers_the_source_containing_the_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let incomplete_backup = dir.path().join(".GCABB-update-backup");
        let legacy = dir.path().join("GCABB");
        let target = dir.path().join("GCABB-data");
        std::fs::create_dir_all(&incomplete_backup).expect("backup");
        std::fs::create_dir_all(&legacy).expect("legacy");
        std::fs::write(
            incomplete_backup.join("update-settings.json"),
            b"incomplete",
        )
        .expect("settings");
        std::fs::write(legacy.join("gcabb.db"), b"database").expect("database");

        migrate_persistent_data(&target, &[incomplete_backup, legacy]).expect("migration");

        assert_eq!(
            std::fs::read(target.join("gcabb.db")).expect("migrated database"),
            b"database"
        );
    }

    /// The changes base must be the repository default, never the branch a
    /// worktree happens to have checked out.
    #[test]
    fn default_branch_is_the_repository_default_not_the_checked_out_branch() {
        let (_guard, main, worktree) = repo_with_worktree();
        assert_eq!(default_branch(&main).as_deref(), Some("main"));
        assert_eq!(default_branch(&worktree).as_deref(), Some("main"));
    }

    /// New worktree is the default so sessions do not disturb the checkout
    /// the developer is using.
    #[test]
    fn new_worktree_is_the_default_location() {
        assert_eq!(SessionLocation::default(), SessionLocation::NewWorktree);
        assert_eq!(SessionLocation::NewWorktree.label(), "New worktree");
        assert_eq!(SessionLocation::LocalRepository.label(), "Local repository");
        assert_eq!(
            SessionLocation::from_str_or_default("local-repository"),
            SessionLocation::LocalRepository
        );
        // Unknown values fall back to the safe option rather than the shared
        // checkout.
        assert_eq!(
            SessionLocation::from_str_or_default("nonsense"),
            SessionLocation::NewWorktree
        );
    }

    fn window(tier: &str, max_tokens: Option<u64>) -> ContextWindowOption {
        ContextWindowOption {
            tier: tier.to_owned(),
            max_tokens,
        }
    }

    #[test]
    fn compact_layout_uses_stable_breakpoint() {
        assert!(compact_layout(COMPACT_WIDTH - 1.0));
        assert!(!compact_layout(COMPACT_WIDTH));
    }

    #[test]
    fn selector_menu_opens_switches_and_closes() {
        assert_eq!(
            toggled_menu(None, ControlMenu::Model),
            Some(ControlMenu::Model)
        );
        assert_eq!(
            toggled_menu(Some(ControlMenu::Model), ControlMenu::Effort),
            Some(ControlMenu::Effort)
        );
        assert_eq!(
            toggled_menu(Some(ControlMenu::Model), ControlMenu::Model),
            None
        );
    }

    #[test]
    fn selector_menus_align_with_their_composer_pills() {
        assert_eq!(control_menu_offset(ControlMenu::Mode), 40);
        assert_eq!(control_menu_offset(ControlMenu::Model), 128);
        assert_eq!(control_menu_offset(ControlMenu::Effort), 216);
    }

    #[test]
    fn selector_accessibility_ids_match_their_triggers() {
        assert_eq!(control_menu_id(ControlMenu::Mode), "mode");
        assert_eq!(control_menu_id(ControlMenu::Model), "model");
        assert_eq!(control_menu_id(ControlMenu::Effort), "effort");
    }

    #[test]
    fn effort_labels_match_menu_copy() {
        assert_eq!(effort_label("medium"), "Medium");
        assert_eq!(effort_label("xhigh"), "Extra high");
    }

    #[test]
    fn context_length_labels_are_human_readable() {
        assert_eq!(token_label(200_000), "200K");
        assert_eq!(token_label(1_000_000), "1M");
        assert_eq!(token_label(1_500_000), "1.5M");
        assert_eq!(
            context_window_label(&window("long_context", Some(1_000_000))),
            "1M context"
        );
        assert_eq!(
            context_window_label(&window("long_context", None)),
            "Long context"
        );
    }

    #[test]
    fn context_tier_defaults_to_the_standard_window() {
        assert_eq!(default_context_tier(&[]), None);
        assert_eq!(
            default_context_tier(&[
                window("long_context", Some(1_000_000)),
                window("default", Some(200_000)),
            ]),
            Some("default".to_owned())
        );
    }

    #[test]
    fn context_selector_only_appears_for_multiple_windows() {
        assert_eq!(control_menu_id(ControlMenu::Context), "context");
        assert_eq!(control_menu_offset(ControlMenu::Context), 304);
    }

    #[test]
    fn reasoning_effort_is_only_submitted_for_supported_models() {
        assert_eq!(reasoning_effort_for_model(&[], "medium"), None);
        assert_eq!(
            reasoning_effort_for_model(&["low".to_owned(), "medium".to_owned()], "medium"),
            Some("medium".to_owned())
        );
    }

    /// View-level interaction tests.
    ///
    /// These drive the real GPUI element tree with simulated mouse input,
    /// which is the only way to catch event-wiring mistakes such as a dismiss
    /// overlay consuming the click meant for a menu item.
    fn change_file(path: &str, status: app_model::ChangeStatus) -> app_model::ChangedFile {
        app_model::ChangedFile {
            path: path.to_owned(),
            original_path: None,
            status,
            stage: app_model::ChangeStage::Unstaged,
            stats: app_model::DiffStats {
                insertions: 3,
                deletions: 4,
            },
            diff: None,
            binary: false,
            diff_omitted_reason: None,
        }
    }

    /// A rename that only showed the new path hid what was actually renamed.
    #[test]
    fn a_renamed_file_is_labelled_with_both_paths() {
        let mut file = change_file("src/new.rs", app_model::ChangeStatus::Renamed);
        file.original_path = Some("src/old.rs".to_owned());

        assert_eq!(
            crate::SessionMvpView::change_display_path(&file),
            "src/old.rs \u{2192} src/new.rs"
        );
        assert_eq!(
            crate::SessionMvpView::change_row_label(&file),
            "renamed src/old.rs \u{2192} src/new.rs +3 -4"
        );
    }

    #[test]
    fn a_deleted_file_keeps_its_status_in_the_row_label() {
        let file = change_file("src/gone.rs", app_model::ChangeStatus::Deleted);

        assert_eq!(
            crate::SessionMvpView::change_row_label(&file),
            "deleted src/gone.rs +3 -4"
        );
    }

    /// Files with no diff must still say why under their own row rather than
    /// expanding to nothing.
    #[test]
    fn files_without_a_diff_report_why() {
        let mut binary = change_file("assets/logo.png", app_model::ChangeStatus::Added);
        binary.binary = true;
        let mut omitted = change_file("src/huge.rs", app_model::ChangeStatus::Modified);
        omitted.diff_omitted_reason = Some("File is too large to diff.".to_owned());
        let mut blank = change_file("src/empty.rs", app_model::ChangeStatus::Modified);
        blank.diff = Some("   \n".to_owned());
        let mut diffed = change_file("src/lib.rs", app_model::ChangeStatus::Modified);
        diffed.diff = Some("@@ -1 +1 @@\n-old\n+new\n".to_owned());

        assert_eq!(
            crate::SessionMvpView::change_diff_text(&binary),
            ("Binary file. No diff to show.".to_owned(), true)
        );
        assert_eq!(
            crate::SessionMvpView::change_diff_text(&omitted),
            ("File is too large to diff.".to_owned(), true)
        );
        assert_eq!(
            crate::SessionMvpView::change_diff_text(&blank),
            ("Diff unavailable.".to_owned(), true)
        );
        assert_eq!(
            crate::SessionMvpView::change_diff_text(&diffed),
            ("@@ -1 +1 @@\n-old\n+new\n".to_owned(), false)
        );
    }

    mod interaction {
        use app_model::{
            InteractionKind, InteractionRequest, InteractionResponse, SessionKind, SessionMetadata,
            SessionSnapshot, SessionStatus, TitleSource,
        };
        use gpui::{FollowMode, Modifiers, MouseButton, TestAppContext, VisualTestContext};
        use session_manager::SessionHandle;
        use std::sync::Arc;

        use crate::{
            AppService, SCROLL_TO_BOTTOM_DURATION, ServiceCommand, ServiceUpdate,
            SessionLaunchProgress, SessionMvpView, SessionProjection, UpdateUi,
        };

        fn snapshot(id: &str, title: &str) -> SessionSnapshot {
            let mut state = SessionSnapshot::new(SessionMetadata {
                id: id.to_owned(),
                sdk_session_id: format!("sdk-{id}"),
                project_path: "/tmp/project".to_owned(),
                repository_root: Some("/tmp/project".to_owned()),
                title: title.to_owned(),
                title_source: TitleSource::Manual,
                kind: SessionKind::Project,
                model: None,
                mode: None,
                base_ref: None,
                created_at: "1".to_owned(),
                updated_at: "1".to_owned(),
            });
            state.status = app_model::SessionStatus::Idle;
            state
        }

        fn interaction(
            kind: InteractionKind,
            title: &str,
            message: &str,
            choices: &[&str],
            allow_freeform: bool,
        ) -> InteractionRequest {
            InteractionRequest {
                id: "interaction-1".to_owned(),
                session_id: "sdk-session-1".to_owned(),
                kind,
                title: title.to_owned(),
                message: message.to_owned(),
                choices: choices.iter().map(|choice| (*choice).to_owned()).collect(),
                allow_freeform,
                details: serde_json::Value::Null,
            }
        }

        /// Build the real view with one session row rendered.
        fn setup(
            cx: &mut TestAppContext,
        ) -> (
            gpui::Entity<SessionMvpView>,
            &mut VisualTestContext,
            std::sync::mpsc::Receiver<ServiceCommand>,
        ) {
            let (view, cx, commands, _) = setup_with_attachments(cx);
            (view, cx, commands)
        }

        fn expand_first_tool(cx: &mut VisualTestContext) {
            let row = cx.debug_bounds("tool-card").expect("tool row rendered");
            cx.simulate_click(row.center(), Modifiers::none());
            cx.run_until_parked();
        }

        fn setup_for_bootstrap(
            cx: &mut TestAppContext,
        ) -> (
            gpui::Entity<SessionMvpView>,
            &mut VisualTestContext,
            std::sync::mpsc::Receiver<ServiceCommand>,
            std::sync::mpsc::Sender<ServiceUpdate>,
        ) {
            let (service, commands, updates) = AppService::for_test_with_updates();
            cx.update(super::super::bind_app_keys);
            let (view, cx) = cx.add_window_view(|_, cx| {
                SessionMvpView::new(
                    service,
                    std::path::PathBuf::from("/tmp/project"),
                    "main".to_owned(),
                    std::path::PathBuf::from("/tmp/chats"),
                    None,
                    crate::WorktreeConfiguration {
                        data_dir: None,
                        settings: crate::AppSettings::default(),
                        default_root: std::path::PathBuf::from("/tmp/worktrees"),
                    },
                    cx,
                )
            });
            cx.run_until_parked();
            (view, cx, commands, updates)
        }

        /// Same view, plus a temporary directory for pasted images.
        fn setup_with_attachments(
            cx: &mut TestAppContext,
        ) -> (
            gpui::Entity<SessionMvpView>,
            &mut VisualTestContext,
            std::sync::mpsc::Receiver<ServiceCommand>,
            tempfile::TempDir,
        ) {
            let attachments = tempfile::tempdir().expect("temp dir");
            let attachments_root = attachments.path().to_owned();
            let (service, commands) = AppService::for_test();
            cx.update(super::super::bind_app_keys);
            let (view, cx) = cx.add_window_view(|_, cx| {
                let mut view = SessionMvpView::new(
                    service,
                    std::path::PathBuf::from("/tmp/project"),
                    "main".to_owned(),
                    std::path::PathBuf::from("/tmp/chats"),
                    Some(attachments_root),
                    crate::WorktreeConfiguration {
                        data_dir: None,
                        settings: crate::AppSettings::default(),
                        default_root: std::path::PathBuf::from("/tmp/worktrees"),
                    },
                    cx,
                );
                view.selected_project = std::path::PathBuf::from("/tmp/project");
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(
                    snapshot("session-1", "First session"),
                ))];
                view
            });
            cx.run_until_parked();
            (view, cx, commands, attachments)
        }

        fn assert_horizontally_aligned(
            label: &str,
            actual: gpui::Bounds<gpui::Pixels>,
            expected: gpui::Bounds<gpui::Pixels>,
        ) {
            let left_delta = f32::from(actual.origin.x - expected.origin.x).abs();
            let width_delta = f32::from(actual.size.width - expected.size.width).abs();
            assert!(
                left_delta < 0.5 && width_delta < 0.5,
                "{label} is not aligned: {actual:?} vs {expected:?}"
            );
        }

        #[gpui::test]
        fn configured_worktree_location_is_sent_with_new_sessions(cx: &mut TestAppContext) {
            let repository = tempfile::tempdir().expect("repository");
            super::git(repository.path(), &["init", "-q"]);
            let custom = repository.path().join("custom-worktrees");
            let (view, cx, commands, _) = setup_for_bootstrap(cx);
            view.update(cx, |view, _| {
                let configuration = &mut view.worktree_configuration;
                configuration
                    .settings
                    .set_worktrees_root(custom.clone(), &configuration.default_root);
                view.composing_chat = false;
                view.selected_project = repository.path().to_owned();
                view.workspace_root = repository.path().to_owned();
                view.submit_prompt("Start a session".to_owned());
            });

            let root = commands
                .try_iter()
                .find_map(|command| match command {
                    ServiceCommand::Submit { worktrees_root, .. } => Some(worktrees_root),
                    _ => None,
                })
                .expect("a submit command was sent");
            assert_eq!(root, custom);
        }

        #[gpui::test]
        fn worktree_launch_shows_creation_before_ready(cx: &mut TestAppContext) {
            let repository = tempfile::tempdir().expect("repository");
            super::git(repository.path(), &["init", "-q"]);
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            view.update(cx, |view, cx| {
                view.composing_chat = false;
                view.selected_project = repository.path().to_owned();
                view.workspace_root = repository.path().to_owned();
                view.submit_prompt("Start a session".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("launch-creating-worktree").is_some());
            assert!(cx.debug_bounds("launch-worktree-ready").is_none());

            updates
                .send(ServiceUpdate::SessionLaunchProgress(
                    SessionLaunchProgress::WorktreeReady(std::path::PathBuf::from(
                        "/tmp/worktrees/session",
                    )),
                ))
                .unwrap();
            view.update(cx, |view, cx| {
                view.apply_service_updates(cx);
                cx.notify();
            });
            cx.run_until_parked();

            let creating = cx
                .debug_bounds("launch-creating-worktree")
                .expect("creation status rendered");
            let ready = cx
                .debug_bounds("launch-worktree-ready")
                .expect("ready status rendered");
            assert!(
                creating.origin.y < ready.origin.y,
                "creation status must precede worktree ready"
            );
            assert!(cx.debug_bounds("launch-copilot-session").is_some());
        }

        #[gpui::test]
        fn completed_worktree_start_steps_precede_the_prompt(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.metadata.project_path = "/tmp/worktrees/session".to_owned();
                state.transcript.push(app_model::TranscriptMessage {
                    id: "prompt".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "Start a session".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let creating = cx
                .debug_bounds("session-start-creating-worktree")
                .expect("creation status rendered");
            let ready = cx
                .debug_bounds("session-start-worktree-ready")
                .expect("ready status rendered");
            let started = cx
                .debug_bounds("session-start-copilot-session-started")
                .expect("session status rendered");
            let prompt = cx
                .debug_bounds("transcript-message")
                .expect("submitted prompt rendered");
            assert!(creating.origin.y < ready.origin.y);
            assert!(ready.origin.y < started.origin.y);
            assert!(started.origin.y < prompt.origin.y);
            assert!(
                ready.origin.y - creating.bottom() <= gpui::px(1.0),
                "startup rows should not have extra space between them"
            );
            assert!(
                started.origin.y - ready.bottom() <= gpui::px(1.0),
                "startup rows should form one compact activity group"
            );
        }

        #[gpui::test]
        fn bootstrap_selects_the_stored_session_before_hydration(cx: &mut TestAppContext) {
            let (mut service, _commands) = AppService::for_test();
            let first = snapshot("session-1", "First session").metadata;
            let second = snapshot("session-2", "Second session").metadata;
            service.bootstrap = Some(super::super::BootstrapState {
                projects: Vec::new(),
                sessions: vec![first, second],
                selected_session: Some("session-2".to_owned()),
            });
            cx.update(super::super::bind_app_keys);
            let (view, cx) = cx.add_window_view(|_, cx| {
                SessionMvpView::new(
                    service,
                    std::path::PathBuf::from("/tmp/project"),
                    "main".to_owned(),
                    std::path::PathBuf::from("/tmp/chats"),
                    None,
                    crate::WorktreeConfiguration {
                        data_dir: None,
                        settings: crate::AppSettings::default(),
                        default_root: std::path::PathBuf::from("/tmp/worktrees"),
                    },
                    cx,
                )
            });

            view.read_with(cx, |view, _| {
                assert_eq!(view.selected_session.as_deref(), Some("session-2"));
                assert_eq!(view.sessions.len(), 2);
                assert_eq!(
                    view.selected().unwrap().snapshot.status,
                    SessionStatus::Recovering
                );
            });
        }

        #[gpui::test]
        fn recovering_session_shows_resuming_spinner_until_hydrated(cx: &mut TestAppContext) {
            let (mut service, _commands, updates) = AppService::for_test_with_updates();
            let metadata = snapshot("session-1", "First session").metadata;
            service.bootstrap = Some(super::super::BootstrapState {
                projects: Vec::new(),
                sessions: vec![metadata],
                selected_session: Some("session-1".to_owned()),
            });
            cx.update(super::super::bind_app_keys);
            let (view, cx) = cx.add_window_view(|_, cx| {
                SessionMvpView::new(
                    service,
                    std::path::PathBuf::from("/tmp/project"),
                    "main".to_owned(),
                    std::path::PathBuf::from("/tmp/chats"),
                    None,
                    crate::WorktreeConfiguration {
                        data_dir: None,
                        settings: crate::AppSettings::default(),
                        default_root: std::path::PathBuf::from("/tmp/worktrees"),
                    },
                    cx,
                )
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("resuming-session").is_some(),
                "resuming spinner should render while session is still hydrating"
            );

            updates
                .send(ServiceUpdate::SessionAdded(SessionHandle::for_test(
                    snapshot("session-1", "First session"),
                )))
                .unwrap();
            view.update(cx, |view, cx| {
                view.apply_service_updates(cx);
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("resuming-session").is_none(),
                "resuming spinner should disappear once the session is hydrated"
            );
        }

        #[gpui::test]
        fn navigation_before_bootstrap_is_never_overwritten(cx: &mut TestAppContext) {
            let (view, cx, _commands, _updates) = setup_for_bootstrap(cx);
            view.update_in(cx, SessionMvpView::new_session);
            view.update(cx, |view, _| {
                view.apply_bootstrap(super::super::BootstrapState {
                    projects: Vec::new(),
                    sessions: vec![snapshot("session-1", "First session").metadata],
                    selected_session: Some("session-1".to_owned()),
                });
            });

            view.read_with(cx, |view, _| {
                assert!(view.selected_session.is_none());
                assert_eq!(view.sessions.len(), 1);
            });
        }

        #[gpui::test]
        fn hydration_replaces_the_shell_without_changing_navigation(cx: &mut TestAppContext) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            view.update(cx, |view, _| {
                view.apply_bootstrap(super::super::BootstrapState {
                    projects: Vec::new(),
                    sessions: vec![snapshot("session-1", "First session").metadata],
                    selected_session: Some("session-1".to_owned()),
                });
            });
            view.update_in(cx, SessionMvpView::new_session);
            updates
                .send(ServiceUpdate::SessionHydrated(SessionHandle::for_test(
                    snapshot("session-1", "Hydrated session"),
                )))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                assert!(view.selected_session.is_none());
                assert_eq!(view.sessions[0].snapshot.status, SessionStatus::Idle);
                assert_eq!(view.sessions[0].snapshot.metadata.title, "Hydrated session");
            });
        }

        #[gpui::test]
        fn first_hydration_is_selected_when_bootstrap_metadata_was_empty(cx: &mut TestAppContext) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            updates
                .send(ServiceUpdate::SessionHydrated(SessionHandle::for_test(
                    snapshot("session-1", "Recovered session"),
                )))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                assert_eq!(view.selected_session.as_deref(), Some("session-1"));
            });
        }

        #[gpui::test]
        fn unavailable_hydration_is_not_selected_automatically(cx: &mut TestAppContext) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            let mut unavailable = snapshot("session-1", "Archived session");
            unavailable.status = SessionStatus::Unavailable;
            updates
                .send(ServiceUpdate::SessionHydrated(SessionHandle::for_test(
                    unavailable,
                )))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                assert!(view.selected_session.is_none());
                assert_eq!(view.sessions[0].snapshot.status, SessionStatus::Unavailable);
            });
        }

        #[gpui::test]
        fn unavailable_session_is_read_only_and_offers_recovery(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut unavailable = snapshot("session-1", "Archived session");
                unavailable.status = SessionStatus::Unavailable;
                unavailable.metadata.project_path = view
                    .worktree_configuration
                    .default_root
                    .join("project")
                    .join("gcabb-archived")
                    .to_string_lossy()
                    .into_owned();
                unavailable.changes.branch = Some("gcabb/archived".to_owned());
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(
                    unavailable,
                ))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("worktree-unavailable-badge").is_some());
            assert!(cx.debug_bounds("session-unavailable").is_some());
            assert!(cx.debug_bounds("composer-input").is_none());
            assert!(cx.debug_bounds("Recreate").is_some());
            assert!(cx.debug_bounds("Locate folder").is_some());
            assert!(cx.debug_bounds("Delete session").is_some());

            let recreate = cx.debug_bounds("Recreate").expect("recreate button");
            cx.simulate_click(recreate.center(), Modifiers::none());
            assert!(matches!(
                commands.recv().expect("resume command"),
                ServiceCommand::Resume {
                    app_session_id,
                    worktrees_root: Some(worktrees_root),
                } if app_session_id == "session-1"
                    && worktrees_root == std::path::Path::new("/tmp/worktrees")
            ));

            let delete = cx
                .debug_bounds("Delete session")
                .expect("delete session button");
            cx.simulate_click(delete.center(), Modifiers::none());
            assert!(matches!(
                commands.recv().expect("delete command"),
                ServiceCommand::DeleteSession { app_session_id, .. }
                    if app_session_id == "session-1"
            ));
        }

        #[gpui::test]
        fn hydration_refreshes_repository_grouping_after_metadata_adoption(
            cx: &mut TestAppContext,
        ) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            let mut legacy = snapshot("session-1", "Legacy session").metadata;
            legacy.repository_root = None;
            legacy.project_path = "/tmp/repository/worktree".to_owned();
            view.update(cx, |view, _| {
                view.apply_bootstrap(super::super::BootstrapState {
                    projects: Vec::new(),
                    sessions: vec![legacy],
                    selected_session: Some("session-1".to_owned()),
                });
            });
            let mut adopted = snapshot("session-1", "Legacy session");
            adopted.metadata.project_path = "/tmp/repository/worktree".to_owned();
            adopted.metadata.repository_root = Some("/tmp/repository".to_owned());
            updates
                .send(ServiceUpdate::SessionHydrated(SessionHandle::for_test(
                    adopted,
                )))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.selected_project,
                    std::path::PathBuf::from("/tmp/repository")
                );
                assert_eq!(
                    view.workspace_root,
                    std::path::PathBuf::from("/tmp/repository/worktree")
                );
            });
        }

        #[gpui::test]
        fn restoration_failure_keeps_the_selected_shell_and_surfaces_error(
            cx: &mut TestAppContext,
        ) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            view.update(cx, |view, _| {
                view.apply_bootstrap(super::super::BootstrapState {
                    projects: Vec::new(),
                    sessions: vec![snapshot("session-1", "First session").metadata],
                    selected_session: Some("session-1".to_owned()),
                });
            });
            updates
                .send(ServiceUpdate::Ready {
                    compatibility: copilot_provider::ProviderCompatibility {
                        sdk_crate_version: "test".to_owned(),
                        sdk_protocol_version: 3,
                        negotiated_protocol_version: 3,
                        process_id: None,
                        startup: None,
                        available_modes: Vec::new(),
                        available_models: Vec::new(),
                    },
                    projects: Vec::new(),
                    failures: vec![session_manager::RestoreFailure {
                        app_session_id: "session-1".to_owned(),
                        sdk_session_id: "sdk-session-1".to_owned(),
                        error: "saved worktree is missing".to_owned(),
                    }],
                })
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                let session = view.selected().expect("failed session remains selected");
                assert_eq!(session.snapshot.status, SessionStatus::Failed);
                assert_eq!(
                    session.snapshot.last_error.as_deref(),
                    Some("saved worktree is missing")
                );
            });
        }

        #[gpui::test]
        fn composer_drafts_are_restored_per_session_and_home(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update_in(cx, |view, window, cx| {
                view.sessions
                    .push(SessionProjection::for_test(SessionHandle::for_test(
                        snapshot("session-2", "Second session"),
                    )));
                view.composer
                    .update(cx, |input, cx| input.set_value("home draft", cx));

                view.select_session("session-1".to_owned(), cx);
                assert!(view.composer.read(cx).value().is_empty());
                view.composer
                    .update(cx, |input, cx| input.set_value("first draft", cx));

                view.select_session("session-2".to_owned(), cx);
                assert!(view.composer.read(cx).value().is_empty());
                view.composer
                    .update(cx, |input, cx| input.set_value("second draft", cx));

                view.select_session("session-1".to_owned(), cx);
                assert_eq!(view.composer.read(cx).value(), "first draft");

                view.new_session(window, cx);
                assert_eq!(view.composer.read(cx).value(), "home draft");

                view.select_session("session-2".to_owned(), cx);
                assert_eq!(view.composer.read(cx).value(), "second draft");
            });
        }

        #[gpui::test]
        fn accepted_prompt_only_clears_its_originating_session(cx: &mut TestAppContext) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            view.update(cx, |view, cx| {
                view.sessions = vec![
                    SessionProjection::for_test(SessionHandle::for_test(snapshot(
                        "session-1",
                        "First session",
                    ))),
                    SessionProjection::for_test(SessionHandle::for_test(snapshot(
                        "session-2",
                        "Second session",
                    ))),
                ];
                view.select_session("session-1".to_owned(), cx);
                view.composer
                    .update(cx, |input, cx| input.set_value("submitted draft", cx));
                view.select_session("session-2".to_owned(), cx);
                view.composer
                    .update(cx, |input, cx| input.set_value("untouched draft", cx));
            });
            updates
                .send(ServiceUpdate::PromptAccepted(Some("session-1".to_owned())))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.update(cx, |view, cx| {
                assert_eq!(view.composer.read(cx).value(), "untouched draft");
                view.select_session("session-1".to_owned(), cx);
                assert!(view.composer.read(cx).value().is_empty());
            });
        }

        #[gpui::test]
        fn active_empty_composer_uses_the_trailing_action_to_cancel(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                let mut snapshot = (*view.sessions[0].snapshot).clone();
                snapshot.status = SessionStatus::Running;
                view.sessions[0].set_snapshot(Arc::new(snapshot));
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("running-indicator").is_some());
            assert!(cx.debug_bounds("stop-session").is_some());
            assert!(cx.debug_bounds("submit-prompt").is_none());
            assert!(cx.debug_bounds("close-session").is_none());

            let stop = cx
                .debug_bounds("stop-session")
                .expect("stop action rendered");
            cx.simulate_click(stop.center(), Modifiers::none());

            match commands.try_recv().expect("a command was sent") {
                ServiceCommand::Cancel { app_session_id } => {
                    assert_eq!(app_session_id, "session-1");
                }
                _ => panic!("expected a Cancel command"),
            }
        }

        #[gpui::test]
        fn running_session_shows_elapsed_activity_and_intent(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                let mut snapshot = (*view.sessions[0].snapshot).clone();
                snapshot.status = SessionStatus::Running;
                snapshot.diagnostics.latest_intent = Some("Reviewing repository state".to_owned());
                snapshot.diagnostics.activity = Some("Waiting for model response".to_owned());
                view.sessions[0].snapshot = Arc::new(snapshot);
                view.sync_activity_timers();
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("running-activity").is_some());
            assert!(cx.debug_bounds("running-elapsed").is_some());
            assert!(cx.debug_bounds("running-intent").is_some());
        }

        #[gpui::test]
        fn diagnostics_button_opens_and_closes_dialog(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            let button = cx
                .debug_bounds("open-diagnostics")
                .expect("diagnostics button rendered");
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();

            assert!(cx.debug_bounds("diagnostics-dialog").is_some());
            let close = cx
                .debug_bounds("diagnostics-close")
                .expect("diagnostics close rendered");
            cx.simulate_click(close.center(), Modifiers::none());
            cx.run_until_parked();
            assert!(cx.debug_bounds("diagnostics-dialog").is_none());
        }

        #[gpui::test]
        fn pending_question_is_inline_and_replaces_the_composer(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                Arc::make_mut(&mut view.sessions[0].snapshot)
                    .pending_interactions
                    .push(interaction(
                        InteractionKind::UserInput,
                        "Choose a direction",
                        "Which approach should I take?",
                        &["Keep it simple", "Add configuration"],
                        true,
                    ));
                cx.notify();
            });
            cx.run_until_parked();

            let transcript = cx.debug_bounds("transcript").expect("transcript rendered");
            let prompt = cx
                .debug_bounds("interaction-prompt")
                .expect("inline interaction rendered");
            assert!(
                prompt.origin.y >= transcript.origin.y + transcript.size.height,
                "interaction should follow the transcript instead of covering it"
            );
            assert!(
                cx.debug_bounds("composer").is_none(),
                "the regular composer should not compete with a pending question"
            );

            let choice = cx
                .debug_bounds("interaction-choice-0")
                .expect("question choice rendered");
            cx.simulate_click(choice.center(), Modifiers::none());
            match commands.try_recv().expect("a response was sent") {
                ServiceCommand::Respond {
                    app_session_id,
                    interaction_id,
                    response,
                } => {
                    assert_eq!(app_session_id, "session-1");
                    assert_eq!(interaction_id, "interaction-1");
                    assert_eq!(
                        response,
                        InteractionResponse::Submit {
                            value: "Keep it simple".into(),
                            freeform: false,
                        }
                    );
                }
                _ => panic!("expected an interaction response"),
            }
        }

        #[gpui::test]
        fn pending_permission_is_inline_and_keeps_scope_actions(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                let mut request = interaction(
                    InteractionKind::Permission,
                    "Permission required",
                    "Run cargo test",
                    &["Allow once", "Allow for this session", "Deny"],
                    false,
                );
                request.details = serde_json::json!({
                    "command": "cargo test",
                    "arguments": ["--workspace"],
                    "path": "/tmp/project"
                });
                Arc::make_mut(&mut view.sessions[0].snapshot).add_interaction(request);
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("permission-entry").is_some());
            assert!(cx.debug_bounds("interaction-prompt").is_none());
            assert!(cx.debug_bounds("composer").is_none());
            let scope = cx
                .debug_bounds("permission-scope-1")
                .expect("session permission rendered");
            cx.simulate_click(scope.center(), Modifiers::none());
            match commands.try_recv().expect("a response was sent") {
                ServiceCommand::Respond {
                    app_session_id,
                    interaction_id,
                    response,
                } => {
                    assert_eq!(app_session_id, "session-1");
                    assert_eq!(interaction_id, "interaction-1");
                    assert_eq!(response, InteractionResponse::ApproveForSession);
                }
                _ => panic!("expected an interaction response"),
            }

            view.update(cx, |view, cx| {
                let snapshot = Arc::make_mut(&mut view.sessions[0].snapshot);
                snapshot.record_interaction_response(
                    "interaction-1",
                    InteractionResponse::ApproveForSession,
                );
                snapshot.remove_interaction("interaction-1");
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("permission-entry").is_some());
            assert!(cx.debug_bounds("permission-scope-1").is_none());
        }

        #[gpui::test]
        fn typing_during_active_work_turns_stop_into_steering_send(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                let mut snapshot = (*view.sessions[0].snapshot).clone();
                snapshot.status = SessionStatus::Running;
                view.sessions[0].set_snapshot(Arc::new(snapshot));
                cx.notify();
            });
            cx.run_until_parked();

            let composer = view.read_with(cx, |view, _| view.composer.clone());
            composer.update(cx, |input, cx| input.set_value("change direction", cx));
            cx.run_until_parked();

            assert!(cx.debug_bounds("stop-session").is_none());
            let send = cx
                .debug_bounds("submit-prompt")
                .expect("steering send action rendered");
            cx.simulate_click(send.center(), Modifiers::none());

            match commands.try_recv().expect("a command was sent") {
                ServiceCommand::Submit {
                    app_session_id,
                    prompt,
                    ..
                } => {
                    assert_eq!(app_session_id.as_deref(), Some("session-1"));
                    assert_eq!(prompt, "change direction");
                }
                _ => panic!("expected a Submit command"),
            }
        }

        #[gpui::test]
        fn running_indicator_clears_when_the_turn_stops(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                let mut snapshot = (*view.sessions[0].snapshot).clone();
                snapshot.status = SessionStatus::Running;
                view.sessions[0].set_snapshot(Arc::new(snapshot));
                cx.notify();
            });
            cx.run_until_parked();
            assert!(cx.debug_bounds("running-indicator").is_some());

            view.update(cx, |view, cx| {
                let mut snapshot = (*view.sessions[0].snapshot).clone();
                snapshot.status = SessionStatus::Cancelled;
                view.sessions[0].set_snapshot(Arc::new(snapshot));
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("running-indicator").is_none());
            view.read_with(cx, |view, _| {
                assert!(view.sessions[0].running_since.is_none());
            });
        }

        /// Regression: the right-click that opens the menu releases after the
        /// dismiss overlay exists. A right-button handler on that overlay made
        /// the menu flash open and vanish on the same click.
        #[gpui::test]
        fn right_click_menu_survives_the_release_of_the_opening_click(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let row = cx
                .debug_bounds("session-row")
                .expect("session row rendered");

            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert!(view.session_menu.is_some(), "menu should open on press");
            });

            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert!(
                    view.session_menu.is_some(),
                    "menu must survive the release of the click that opened it"
                );
            });
        }

        /// Regression: dismissing on mouse *down* removed the menu item before
        /// its click could complete on release, so Rename never ran.
        #[gpui::test]
        fn clicking_rename_opens_the_rename_dialog(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let row = cx
                .debug_bounds("session-row")
                .expect("session row rendered");
            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();

            let item = cx
                .debug_bounds("session-menu-rename")
                .expect("rename item rendered");
            cx.simulate_click(item.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert_eq!(view.renaming_session.as_deref(), Some("session-1"));
                assert!(view.session_menu.is_none(), "menu closes after choosing");
            });
        }

        #[gpui::test]
        fn clicking_delete_sends_a_delete_command(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            let row = cx
                .debug_bounds("session-row")
                .expect("session row rendered");
            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();

            let item = cx
                .debug_bounds("session-menu-delete")
                .expect("delete item rendered");
            cx.simulate_click(item.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(view.session_menu.is_none());
                assert!(
                    view.deleting_sessions.contains("session-1"),
                    "row shows a spinner while the delete is in flight"
                );
            });
            let command = commands.try_recv().expect("a command was sent");
            match command {
                ServiceCommand::DeleteSession { app_session_id, .. } => {
                    assert_eq!(app_session_id, "session-1");
                }
                _ => panic!("expected a DeleteSession command"),
            }
        }

        #[gpui::test]
        fn project_plus_starts_a_new_session_for_that_project(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "project-id".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "Project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let button = cx
                .debug_bounds("project-new-session")
                .expect("project new-session button rendered");
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert_eq!(view.selected_project, std::path::Path::new("/tmp/project"));
                assert_eq!(view.workspace_root, std::path::Path::new("/tmp/project"));
                assert!(view.selected_session.is_none());
            });
            match commands.try_recv().expect("a command was sent") {
                ServiceCommand::Select { app_session_id } => assert!(app_session_id.is_none()),
                _ => panic!("expected a Select command"),
            }
        }

        #[gpui::test]
        fn project_removal_is_available_from_the_right_click_menu(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "project-id".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "Project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                cx.notify();
            });
            cx.run_until_parked();

            let row = cx
                .debug_bounds("project-row")
                .expect("project row rendered");
            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();

            let item = cx
                .debug_bounds("project-menu-remove")
                .expect("remove-project item rendered");
            cx.simulate_click(item.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| assert!(view.project_menu.is_none()));
            match commands.try_recv().expect("a command was sent") {
                ServiceCommand::RemoveProject { project_id } => {
                    assert_eq!(project_id, "project-id");
                }
                _ => panic!("expected a RemoveProject command"),
            }
        }

        /// Left-clicking away from an open menu still dismisses it.
        #[gpui::test]
        fn clicking_away_dismisses_the_menu(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let row = cx
                .debug_bounds("session-row")
                .expect("session row rendered");
            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| assert!(view.session_menu.is_some()));

            let away = gpui::Point::new(gpui::px(900.0), gpui::px(600.0));
            cx.simulate_click(away, Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| assert!(view.session_menu.is_none()));
        }

        /// Renaming updates the sidebar immediately and asks the service to
        /// persist the new title.
        #[gpui::test]
        fn committing_a_rename_updates_the_row_and_sends_the_command(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.renaming_session = Some("session-1".to_owned());
                view.commit_rename("Renamed", cx);
            });

            view.read_with(cx, |view, _| {
                assert_eq!(view.sessions[0].snapshot.metadata.title, "Renamed");
                assert!(view.renaming_session.is_none());
            });
            match commands.try_recv().expect("a command was sent") {
                ServiceCommand::RenameSession { title, .. } => assert_eq!(title, "Renamed"),
                _ => panic!("expected a RenameSession command"),
            }
        }

        /// The project picker offers Chat first, then projects, then the
        /// folder picker. Chat needs no configuration so it leads.
        #[gpui::test]
        fn project_menu_offers_chat_projects_and_add_project(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                cx.notify();
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/project".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
            });

            let options = view.read_with(cx, |view, _| view.project_options());
            let values: Vec<&str> = options.iter().map(|(value, _, _)| value.as_str()).collect();
            assert_eq!(values.first().copied(), Some(super::super::CHAT_OPTION));
            assert!(values.contains(&"/tmp/project"));
            assert_eq!(
                values.last().copied(),
                Some(super::super::ADD_PROJECT_OPTION)
            );
        }

        /// Choosing Chat switches the composer to a repository-less session.
        #[gpui::test]
        fn choosing_chat_starts_a_chat_session(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| {
                assert!(view.composing_chat);
                assert!(view.selected_session.is_none());
            });

            view.update(cx, |view, _| view.submit_prompt("hello".to_owned()));
            // The Select command from new_chat comes first.
            let mut submit = None;
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit {
                    kind,
                    project_path,
                    repository_root,
                    base_ref,
                    ..
                } = command
                {
                    submit = Some((kind, project_path, repository_root, base_ref));
                }
            }
            let (kind, project_path, repository_root, base_ref) =
                submit.expect("a submit command was sent");
            assert_eq!(kind, SessionKind::Chat);
            assert_eq!(project_path, std::path::PathBuf::from("/tmp/chats"));
            assert!(repository_root.is_none(), "a chat has no repository");
            assert!(base_ref.is_none(), "a chat has no changes base");
        }

        /// A staged attachment travels with the prompt it was staged on.
        #[gpui::test]
        fn submitting_carries_staged_attachments(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, _| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
            });

            view.update(cx, |view, _| view.submit_prompt("look".to_owned()));

            let mut attachments = None;
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit {
                    attachments: sent, ..
                } = command
                {
                    attachments = Some(sent);
                }
            }
            let attachments = attachments.expect("a submit command was sent");
            assert_eq!(attachments.len(), 1, "the staged screenshot was dropped");
            assert_eq!(attachments[0].identity(), "/tmp/shot.png");
            assert_eq!(attachments[0].display_name(), "shot.png");
        }

        /// Attachments belong to one prompt, not to every later prompt.
        #[gpui::test]
        fn attachments_do_not_repeat_on_the_next_prompt(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, _| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
                view.submit_prompt("look".to_owned());
                view.submit_prompt("and now".to_owned());
            });

            let mut sends = Vec::new();
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit { attachments, .. } = command {
                    sends.push(attachments);
                }
            }
            assert_eq!(sends.len(), 2, "both prompts were sent");
            assert_eq!(sends[0].len(), 1);
            assert!(
                sends[1].is_empty(),
                "the screenshot was resent with an unrelated follow-up"
            );
        }

        /// An attachment on its own is a complete message.
        #[gpui::test]
        fn an_attachment_alone_can_be_submitted(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
                view.submit_composer(cx);
            });

            let sent = std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, ServiceCommand::Submit { .. }));
            assert!(sent, "an empty prompt with a screenshot sent nothing");
        }

        /// Removing an attachment takes it off the next prompt.
        #[gpui::test]
        fn removing_an_attachment_unstages_it(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
                view.remove_attachment("/tmp/shot.png", cx);
            });
            view.update(cx, |view, _| {
                assert!(view.draft_attachments.is_empty());
            });
        }

        /// The chip strip only exists when something is attached.
        #[gpui::test]
        fn the_attachment_strip_appears_with_an_attachment(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("attachment-strip").is_none(),
                "the strip took up space with nothing attached"
            );

            view.update(cx, |view, cx| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
                cx.notify();
            });
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("attachment-strip").is_some(),
                "the attached screenshot was never shown"
            );
        }

        /// A pasted screenshot has no path, so it must travel as bytes.
        #[gpui::test]
        fn pasting_an_image_stages_it_as_an_attachment(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.attach_pasted_images(
                    &[super::super::PastedImage {
                        bytes: vec![0x89, 0x50, 0x4E, 0x47],
                        mime_type: "image/png".to_owned(),
                    }],
                    cx,
                );
                view.submit_prompt("what is wrong here".to_owned());
            });

            let mut attachments = None;
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit {
                    attachments: sent, ..
                } = command
                {
                    attachments = Some(sent);
                }
            }
            let attachments = attachments.expect("a submit command was sent");
            assert_eq!(attachments.len(), 1, "the pasted screenshot was dropped");
            let app_model::PromptAttachment::Image {
                mime_type, data, ..
            } = &attachments[0]
            else {
                panic!("a pasted image must travel as bytes, not as a path");
            };
            assert_eq!(mime_type, "image/png");
            // base64 of the PNG magic bytes, so the payload survived intact.
            assert_eq!(data, "iVBORw==");
        }

        /// Two pastes mean two images, even when the bytes are identical.
        #[gpui::test]
        fn pasting_twice_stages_two_images(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let image = super::super::PastedImage {
                bytes: vec![1, 2, 3],
                mime_type: "image/png".to_owned(),
            };
            view.update(cx, |view, cx| {
                view.attach_pasted_images(std::slice::from_ref(&image), cx);
                view.attach_pasted_images(std::slice::from_ref(&image), cx);
            });
            view.update(cx, |view, _| {
                assert_eq!(
                    view.draft_attachments.len(),
                    2,
                    "the second paste was mistaken for a duplicate of the first"
                );
            });
        }

        /// Dropping files onto the composer stages them.
        #[gpui::test]
        fn dropping_files_stages_them(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.attach_dropped_paths(
                    &[
                        std::path::PathBuf::from("/tmp/one.png"),
                        std::path::PathBuf::from("/tmp/two.png"),
                    ],
                    cx,
                );
            });
            view.update(cx, |view, _| {
                assert_eq!(view.draft_attachments.len(), 2);
                assert_eq!(view.draft_attachments[0].display_name(), "one.png");
            });
        }

        /// Dropping the same file twice attaches it once.
        #[gpui::test]
        fn dropping_the_same_file_twice_stages_it_once(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let paths = [std::path::PathBuf::from("/tmp/one.png")];
            view.update(cx, |view, cx| {
                view.attach_dropped_paths(&paths, cx);
                view.attach_dropped_paths(&paths, cx);
            });
            view.update(cx, |view, _| {
                assert_eq!(view.draft_attachments.len(), 1);
            });
        }

        /// Removing one pasted image must not remove its identical twin.
        #[gpui::test]
        fn removing_one_pasted_image_keeps_the_other(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let image = super::super::PastedImage {
                bytes: vec![1, 2, 3],
                mime_type: "image/png".to_owned(),
            };
            view.update(cx, |view, cx| {
                view.attach_pasted_images(std::slice::from_ref(&image), cx);
                view.attach_pasted_images(std::slice::from_ref(&image), cx);
                let first = view.draft_attachments[0].identity();
                view.remove_attachment(&first, cx);
            });
            view.update(cx, |view, _| {
                assert_eq!(
                    view.draft_attachments.len(),
                    1,
                    "removing one image took its twin with it"
                );
                assert_eq!(view.draft_attachments[0].display_name(), "Pasted image 2");
            });
        }

        /// Paste was bound to cmd only, so on Linux and Windows the action
        /// never fired and a pasted screenshot vanished without a trace.
        #[gpui::test]
        fn pasting_an_image_with_the_platform_shortcut_attaches_it(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            cx.update(|_, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image {
                    format: gpui::ImageFormat::Png,
                    bytes: vec![0x89, 0x50, 0x4E, 0x47],
                    id: 1,
                }));
            });
            view.update_in(cx, |view, window, cx| {
                let handle = gpui::Focusable::focus_handle(view.composer.read(cx), cx);
                window.focus(&handle, cx);
            });
            cx.run_until_parked();

            cx.simulate_keystrokes("secondary-v");
            cx.run_until_parked();

            view.update(cx, |view, _| {
                assert_eq!(
                    view.draft_attachments.len(),
                    1,
                    "the platform paste shortcut did not reach the composer"
                );
                assert!(view.draft_attachments[0].is_image());
            });
        }

        #[gpui::test]
        fn composer_wraps_text_to_multiple_lines(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let single_line_height = cx
                .debug_bounds("composer-input")
                .expect("composer rendered")
                .size
                .height;

            view.update(cx, |view, cx| {
                view.composer.update(cx, |input, cx| {
                    input.set_value("word ".repeat(300), cx);
                });
            });
            cx.run_until_parked();

            let wrapped_height = cx
                .debug_bounds("composer-input")
                .expect("composer rendered")
                .size
                .height;
            assert!(
                wrapped_height > single_line_height * 2.,
                "long composer text remained on one line: {wrapped_height:?}"
            );
        }

        #[gpui::test]
        fn shift_enter_inserts_a_newline_without_submitting(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update_in(cx, |view, window, cx| {
                view.composer
                    .update(cx, |input, cx| input.set_value("first line", cx));
                let handle = gpui::Focusable::focus_handle(view.composer.read(cx), cx);
                window.focus(&handle, cx);
            });
            cx.run_until_parked();

            cx.simulate_keystrokes("shift-enter");
            cx.run_until_parked();

            view.read_with(cx, |view, cx| {
                assert_eq!(view.composer.read(cx).value(), "first line\n");
            });
            assert!(
                commands.try_recv().is_err(),
                "shift-enter submitted the composer"
            );
        }

        #[gpui::test]
        fn transcript_renders_markdown_and_copies_its_source(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let source = "# Result\n\n| Name | State |\n|---|---|\n| Build | **Passing** |\n\n```rust\nfn main() {}\n```";
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "markdown-message".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: source.to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("markdown-content").is_some());
            assert!(cx.debug_bounds("markdown-table").is_some());
            assert!(cx.debug_bounds("markdown-code").is_some());

            let copy = cx
                .debug_bounds("copy-markdown")
                .expect("copy markdown button rendered");
            cx.simulate_click(copy.center(), Modifiers::none());
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(source.to_owned())
            );
        }

        #[gpui::test]
        fn transcript_text_can_be_selected_and_copied(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "selectable-message".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: "Selectable first paragraph.\n\nAnd a second paragraph.".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let first = cx
                .debug_bounds("markdown-inline-selectable-message-0")
                .expect("first selectable paragraph rendered");
            let second = cx
                .debug_bounds("markdown-inline-selectable-message-1")
                .expect("second selectable paragraph rendered");
            let start = gpui::point(first.origin.x + gpui::px(2.0), first.center().y);
            let end = gpui::point(second.right() - gpui::px(2.0), second.center().y);
            cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
            cx.simulate_event(gpui::MouseMoveEvent {
                position: end,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::none(),
            });
            cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
            cx.run_until_parked();

            let selected = view.read_with(cx, |view, _| {
                let selection = view.transcript_selection.borrow();
                selection.selected_text().expect("selected transcript text")
            });
            assert!(selected.contains("first paragraph"));
            assert!(selected.contains("second paragraph"));
            cx.simulate_keystrokes("secondary-c");
            cx.run_until_parked();
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(selected)
            );
        }

        #[gpui::test]
        fn markdown_link_and_following_text_stay_on_one_line(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "inline-markdown-message".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: "- [#55](https://github.com/constructomech/gcabb/issues/55) Show steering comments greyed out until the model acknowledges them".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let inline = cx
                .debug_bounds("markdown-inline")
                .expect("inline markdown rendered");
            assert!(
                f32::from(inline.size.height) < 32.,
                "link boundary introduced a line break: {inline:?}"
            );
        }

        /// Inline code inside a tight list item must flow with the surrounding
        /// text instead of being stacked one fragment per line.
        #[gpui::test]
        fn list_item_inline_code_stays_on_one_line(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "list-markdown-message".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: "- Source: `Minecraft/`, `src/`, `handheld/`".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let inline = cx
                .debug_bounds("markdown-inline")
                .expect("inline markdown rendered");
            assert!(
                f32::from(inline.size.height) < 32.,
                "inline code split the list item across lines: {inline:?}"
            );
        }

        /// Clicking an image chip in the transcript shows the picture.
        #[gpui::test]
        fn clicking_a_transcript_image_opens_a_preview(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m1".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "look".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: vec![app_model::MessageAttachment {
                        display_name: "Pasted Image".to_owned(),
                        is_image: true,
                        path: Some("/tmp/clipboard.png".to_owned()),
                    }],
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let chip = cx
                .debug_bounds("message-attachment")
                .expect("the attachment chip rendered");
            cx.simulate_click(chip.center(), Modifiers::none());
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("image-preview").is_some(),
                "clicking the image chip did not open a preview"
            );
        }

        /// The real sequence: click a chip, then press Escape. If opening the
        /// preview leaves focus outside the action's dispatch path, Escape is
        /// dead exactly when the user is most likely to reach for it.
        #[gpui::test]
        fn escape_closes_a_preview_opened_by_clicking(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m1".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "look".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: vec![app_model::MessageAttachment {
                        display_name: "Pasted Image".to_owned(),
                        is_image: true,
                        path: Some("/tmp/clipboard.png".to_owned()),
                    }],
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let chip = cx
                .debug_bounds("message-attachment")
                .expect("the attachment chip rendered");
            cx.simulate_click(chip.center(), Modifiers::none());
            cx.run_until_parked();
            assert!(cx.debug_bounds("image-preview").is_some());

            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("image-preview").is_none(),
                "escape did nothing after opening the preview by click"
            );
        }

        /// The preview closes without needing a specific target to hit.
        #[gpui::test]
        fn the_image_preview_closes_on_escape(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update_in(cx, |view, window, cx| {
                view.open_image_preview(
                    super::super::ImagePreview {
                        title: "Pasted Image".to_owned(),
                        source: super::super::PreviewSource::Path(std::path::PathBuf::from(
                            "/tmp/clipboard.png",
                        )),
                    },
                    window,
                    cx,
                );
            });
            cx.run_until_parked();
            assert!(cx.debug_bounds("image-preview").is_some());

            // Focus the composer first, mirroring a user who was typing.
            view.update_in(cx, |view, window, cx| {
                let handle = gpui::Focusable::focus_handle(view.composer.read(cx), cx);
                window.focus(&handle, cx);
            });
            cx.run_until_parked();
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("image-preview").is_none(),
                "escape left the preview open"
            );
        }

        /// A non-image attachment has nothing to preview.
        #[gpui::test]
        fn a_non_image_attachment_does_not_open_a_preview(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m1".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "look".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: vec![app_model::MessageAttachment {
                        display_name: "notes.txt".to_owned(),
                        is_image: false,
                        path: Some("/tmp/notes.txt".to_owned()),
                    }],
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let chip = cx
                .debug_bounds("message-attachments")
                .expect("the attachment chip rendered");
            cx.simulate_click(chip.center(), Modifiers::none());
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("image-preview").is_none(),
                "a text file was opened as a picture"
            );
        }

        /// The bug this replaces: pasted images were sent as inline blobs, and
        /// the runtime echoes an attachment back in the form it was sent. A
        /// blob has no path, so the transcript could never show the picture
        /// again. The earlier test fabricated a path that pasted images never
        /// actually receive, so it passed against a broken build.
        #[gpui::test]
        fn a_pasted_image_is_written_to_disk_and_sent_as_a_file(cx: &mut TestAppContext) {
            let (view, cx, commands, _attachments) = setup_with_attachments(cx);
            let png: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
            view.update(cx, |view, cx| {
                view.attach_pasted_images(
                    &[super::super::PastedImage {
                        bytes: png.clone(),
                        mime_type: "image/png".to_owned(),
                    }],
                    cx,
                );
                view.submit_prompt("look".to_owned());
            });

            let mut sent = None;
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit { attachments, .. } = command {
                    sent = Some(attachments);
                }
            }
            let sent = sent.expect("a submit command was sent");
            assert_eq!(sent.len(), 1);
            let path = sent[0]
                .path()
                .expect("a pasted image must be sent as a file, not an inline blob");
            assert_eq!(
                std::fs::read(path).expect("the image was written to disk"),
                png,
                "the file does not hold the pasted bytes"
            );
            assert!(
                std::path::Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "png"),
                "the extension names the format"
            );
        }

        /// macOS commonly places screenshots on the clipboard as TIFF even
        /// when the source image was a PNG. Models accept PNG but reject TIFF.
        #[gpui::test]
        fn a_pasted_tiff_is_converted_to_png(cx: &mut TestAppContext) {
            let (view, cx, commands, _attachments) = setup_with_attachments(cx);
            let mut tiff = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgba8(1, 1)
                .write_to(&mut tiff, image::ImageFormat::Tiff)
                .expect("encode test TIFF");
            view.update(cx, |view, cx| {
                view.attach_pasted_images(
                    &[super::super::PastedImage {
                        bytes: tiff.into_inner(),
                        mime_type: "image/tiff".to_owned(),
                    }],
                    cx,
                );
                view.submit_prompt("look".to_owned());
            });

            let sent = commands
                .try_iter()
                .find_map(|command| match command {
                    ServiceCommand::Submit { attachments, .. } => Some(attachments),
                    _ => None,
                })
                .expect("a submit command was sent");
            let path = sent[0]
                .path()
                .expect("a converted image must be sent as a file");
            assert_eq!(
                std::path::Path::new(path)
                    .extension()
                    .and_then(|value| value.to_str()),
                Some("png")
            );
            let png = std::fs::read(path).expect("read converted PNG");
            assert!(png.starts_with(&[0x89, 0x50, 0x4E, 0x47]));
            assert_eq!(
                image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                    .expect("decode converted PNG")
                    .width(),
                1
            );
        }

        #[test]
        fn a_pasted_jpeg_is_also_normalized_to_png() {
            let mut jpeg = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(1, 1)
                .write_to(&mut jpeg, image::ImageFormat::Jpeg)
                .expect("encode test JPEG");

            let (png, mime_type) =
                super::super::normalize_pasted_image(&jpeg.into_inner(), "image/jpeg")
                    .expect("normalize JPEG");
            assert_eq!(mime_type, "image/png");
            assert!(png.starts_with(&[0x89, 0x50, 0x4E, 0x47]));
        }

        #[gpui::test]
        fn a_session_error_is_visible_after_the_runtime_returns_to_idle(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "Failed image");
                state.status = SessionStatus::Idle;
                state.last_error = Some("The model could not process this image.".to_owned());
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("session-error").is_some(),
                "the terminal session error was hidden by the idle status"
            );
        }

        /// A pasted image is previewable from the composer before it is sent.
        #[gpui::test]
        fn a_pasted_image_can_be_previewed_before_sending(cx: &mut TestAppContext) {
            let (view, cx, _commands, _attachments) = setup_with_attachments(cx);
            view.update(cx, |view, cx| {
                view.attach_pasted_images(
                    &[super::super::PastedImage {
                        bytes: vec![0x89, 0x50, 0x4E, 0x47],
                        mime_type: "image/png".to_owned(),
                    }],
                    cx,
                );
            });
            view.update(cx, |view, _| {
                let preview = super::super::draft_preview(&view.draft_attachments[0])
                    .expect("a pasted image can be previewed");
                assert!(matches!(
                    preview.source,
                    super::super::PreviewSource::Path(_)
                ));
            });
        }

        /// A sent attachment is part of what was asked, so the transcript must
        /// still show it after the composer is cleared.
        #[gpui::test]
        fn a_sent_attachment_is_shown_in_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m1".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "what is wrong here".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: vec![app_model::MessageAttachment {
                        display_name: "Pasted Image".to_owned(),
                        is_image: true,
                        path: None,
                    }],
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("message-attachments").is_some(),
                "the attachment vanished once the message was sent"
            );
        }

        /// Choosing a project returns the composer to project mode.
        #[gpui::test]
        fn choosing_a_project_leaves_chat_mode(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
                view.choose_control(
                    super::super::ControlMenu::Project,
                    "/tmp/project".to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| assert!(!view.composing_chat));
        }

        /// The hover-revealed plus on the Chats row starts a chat.
        #[gpui::test]
        fn clicking_new_chat_starts_a_chat(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let plus = cx
                .debug_bounds("new-chat")
                .expect("new chat button rendered");
            cx.simulate_click(plus.center(), Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| assert!(view.composing_chat));
        }

        #[gpui::test]
        fn clicking_new_session_focuses_the_composer(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let plus = cx
                .debug_bounds("new-session")
                .expect("new session button rendered");

            cx.simulate_click(plus.center(), Modifiers::none());
            cx.run_until_parked();

            view.update_in(cx, |view, window, cx| {
                let handle = gpui::Focusable::focus_handle(view.composer.read(cx), cx);
                assert!(handle.is_focused(window));
            });
        }

        /// Regression: choosing Chat updated internal state but the composer
        /// still showed the project, so selecting Chat looked like a no-op.
        #[gpui::test]
        fn choosing_chat_updates_the_composer_pill_label(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/project".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "project");
            });

            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.composer_project_label(),
                    "Chat",
                    "the pill must show Chat once chat mode is chosen"
                );
                assert!(view.targets_chat());
            });
        }

        /// Regression: the hover plus set state but nothing on screen changed.
        #[gpui::test]
        fn clicking_new_chat_updates_the_composer_pill_label(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let plus = cx
                .debug_bounds("new-chat")
                .expect("new chat button rendered");
            cx.simulate_click(plus.center(), Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "Chat");
            });
        }

        /// Regression: adding a project while in chat mode selected the new
        /// project in the menu but left the pill showing Chat, because only
        /// the menu's project branch cleared the flag.
        #[gpui::test]
        fn adding_a_project_while_in_chat_mode_leaves_chat_mode(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "Chat");
            });

            // The service reports the newly added project and asks for it to
            // be selected, which is the path add-project takes.
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/added".to_owned(),
                    path: "/tmp/added".to_owned(),
                    name: "added".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                view.select_project("/tmp/added", cx);
            });

            view.read_with(cx, |view, _| {
                assert!(
                    !view.targets_chat(),
                    "adding a project must leave chat mode"
                );
                assert_eq!(
                    view.composer_project_label(),
                    "added",
                    "the pill must follow the newly selected project"
                );
            });
        }

        /// The menu's checkmark must agree with the pill.
        #[gpui::test]
        fn project_menu_marks_chat_as_selected_in_chat_mode(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
                view.toggle_control_menu(super::super::ControlMenu::Project);
            });
            view.read_with(cx, |view, _| {
                assert!(view.targets_chat());
                assert_eq!(view.composer_project_label(), "Chat");
            });
        }

        /// Selecting a chat session shows chat context, not a stale branch.
        #[gpui::test]
        fn selecting_a_chat_session_reports_no_repository(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut chat = snapshot("chat-1", "A chat");
                chat.metadata.kind = SessionKind::Chat;
                chat.metadata.repository_root = None;
                view.sessions
                    .push(SessionProjection::for_test(SessionHandle::for_test(chat)));
                view.selected_session = Some("chat-1".to_owned());
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert!(view.targets_chat());
                assert_eq!(view.composer_project_label(), "Chat");
            });
        }

        /// Phase 3b: tool calls must be visible in the transcript, not just
        /// the prose around them.
        #[gpui::test]
        fn tool_calls_render_in_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for (index, raw) in [
                    serde_json::json!({"id":"u","type":"user.message",
                        "data":{"content":"fix it"}}),
                    serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"str_replace_editor",
                                "arguments":{"path":"src/lib.rs"}}}),
                    serde_json::json!({"id":"tc","type":"tool.execution_complete",
                        "data":{"toolCallId":"c1","success":true,
                                "result":{"detailedContent":"@@ -1 +1 @@\n-old\n+new"}}}),
                ]
                .into_iter()
                .enumerate()
                {
                    let event = app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        u64::try_from(index).unwrap_or(0) + 1,
                        &raw,
                    );
                    state.apply(event);
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("tool-entry").is_some(),
                "the edit should be visible in the transcript"
            );
            assert!(
                cx.debug_bounds("tool-expanded-card").is_none(),
                "tool details should start collapsed"
            );
            expand_first_tool(cx);
            assert!(
                cx.debug_bounds("tool-expanded-card").is_some(),
                "clicking the row should reveal the detail card"
            );
            view.read_with(cx, |view, _| {
                let snapshot = &view.selected().unwrap().snapshot;
                let timeline = snapshot.timeline();
                assert_eq!(timeline.len(), 2, "one message and one tool call");
                assert!(view.tool_expanded("c1"));
            });
        }

        #[gpui::test]
        fn nested_tool_rows_expand_independently(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for (sequence, raw) in [
                    serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"task-1","toolName":"task",
                                "arguments":{"description":"Survey the UI"}}}),
                    serde_json::json!({"id":"sa","type":"subagent.started",
                        "data":{"agentId":"agent-7","parentToolCallId":"task-1"}}),
                    serde_json::json!({"id":"n","type":"tool.execution_start","agentId":"agent-7",
                        "data":{"toolCallId":"nested-1","toolName":"grep",
                                "arguments":{"pattern":"tool_entry"}}}),
                ]
                .into_iter()
                .enumerate()
                {
                    state.apply(app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        u64::try_from(sequence).unwrap_or(0) + 1,
                        &raw,
                    ));
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("tool-toggle-task-1").is_some());
            let nested = cx
                .debug_bounds("tool-toggle-nested-1")
                .expect("nested activity row rendered");
            cx.simulate_click(nested.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(!view.tool_expanded("task-1"));
                assert!(view.tool_expanded("nested-1"));
            });
        }

        #[gpui::test]
        fn expanded_shell_details_use_the_actual_command(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.apply(app_model::DomainEvent::from_sdk_event_for(
                    "session-1",
                    1,
                    &serde_json::json!({"id":"t","type":"tool.execution_start",
                    "data":{"toolCallId":"c1","toolName":"bash",
                            "arguments":{
                                "description":"Validate first 20 Fibonacci numbers",
                                "command":"python3 - <<'PY'\nprint('validation')\nPY"
                            },
                            "shellToolInfo":{
                                "displayCommand":"python3 - <<'PY'\nprint('validation')\nPY",
                                "hasWriteFileRedirection":false,
                                "possiblePaths":[]
                            }}}),
                ));
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                let invocation = &view.selected().unwrap().snapshot.tool_activity.invocations[0];
                assert_eq!(
                    SessionMvpView::tool_argument_detail(invocation),
                    Some((
                        "Command",
                        "python3 - <<'PY'\nprint('validation')\nPY".to_owned()
                    ))
                );
            });
        }

        #[gpui::test]
        fn running_terminals_expand_until_they_exit(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let running = |completed: bool| {
                let mut state = snapshot("session-1", "First session");
                state.apply(app_model::DomainEvent::from_sdk_event_for(
                    "session-1",
                    1,
                    &serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command":"sleep 1","shellId":"shell-1"}}}),
                ));
                if completed {
                    state.apply(app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        2,
                        &serde_json::json!({"id":"d","type":"tool.execution_complete",
                        "data":{"toolCallId":"c1","toolName":"bash","success":true,
                                "result":{"contents":[
                                    {"type":"shell_exit","shellId":"shell-1","exitCode":0}
                                ]}}}),
                    ));
                }
                state
            };
            view.update(cx, |view, cx| {
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(
                    running(false),
                ))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("tool-expanded-card").is_some(),
                "a running terminal should reveal its details automatically"
            );
            view.read_with(cx, |view, _| {
                assert!(
                    !view.tool_expanded("c1"),
                    "automatic expansion must not become a manual preference"
                );
            });

            view.update(cx, |view, cx| {
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(
                    running(true),
                ))];
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("tool-expanded-card").is_none(),
                "the terminal should collapse after exit"
            );
            expand_first_tool(cx);
            assert!(
                cx.debug_bounds("tool-expanded-card").is_some(),
                "an exited terminal can still be opened manually"
            );
        }

        #[gpui::test]
        fn short_tool_entries_fill_the_composer_column(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.apply(app_model::DomainEvent::from_sdk_event_for(
                    "session-1",
                    1,
                    &serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"grep",
                                "arguments":{"query":"x"}}}),
                ));
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let composer = cx.debug_bounds("composer").expect("composer rendered");
            let column = cx
                .debug_bounds("transcript-content")
                .expect("transcript column rendered");
            let tool = cx.debug_bounds("tool-card").expect("tool card rendered");
            assert_horizontally_aligned("transcript column", column, composer);
            assert_horizontally_aligned("short tool card", tool, composer);
        }

        #[gpui::test]
        fn wide_terminal_output_stays_inside_the_conversation_column(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for (sequence, raw) in [
                    serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command":"printf wide"},
                                "shellToolInfo":{"displayCommand":"printf wide",
                                                 "hasWriteFileRedirection":false,
                                                 "possiblePaths":[]}}}),
                    serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput":"x".repeat(4_000)}}),
                ]
                .into_iter()
                .enumerate()
                {
                    state.apply(app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        u64::try_from(sequence).unwrap_or(0) + 1,
                        &raw,
                    ));
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            expand_first_tool(cx);

            let column = cx
                .debug_bounds("transcript-content")
                .expect("transcript column rendered");
            let tool = cx.debug_bounds("tool-card").expect("tool card rendered");
            let output = cx
                .debug_bounds("tool-detail")
                .expect("terminal output rendered");
            assert_horizontally_aligned("terminal tool card", tool, column);
            assert!(
                output.origin.x >= tool.origin.x && output.right() <= tool.right(),
                "terminal output escaped its card: {output:?} vs {tool:?}"
            );
        }

        #[gpui::test]
        fn large_completed_output_is_collapsed_until_requested(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for (sequence, raw) in [
                    serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command":"cargo build"}}}),
                    serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput":"compiler output\n".repeat(2_000)}}),
                    serde_json::json!({"id":"c","type":"tool.execution_complete",
                        "data":{"toolCallId":"c1","success":true,"result":{"content":""}}}),
                ]
                .into_iter()
                .enumerate()
                {
                    state.apply(app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        u64::try_from(sequence).unwrap_or(0) + 1,
                        &raw,
                    ));
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            expand_first_tool(cx);

            let toggle = cx
                .debug_bounds("toggle-tool-output")
                .expect("large completed output is collapsed");
            cx.simulate_click(toggle.center(), Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert!(view.expanded_tool_outputs.contains("c1"));
            });
        }

        #[gpui::test]
        fn conversation_column_tracks_resizing_and_the_inspector(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "assistant".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: "Done".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.simulate_resize(gpui::size(gpui::px(1_400.0), gpui::px(800.0)));
            cx.run_until_parked();

            let wide_composer = cx.debug_bounds("composer").expect("composer rendered");
            let wide_message = cx
                .debug_bounds("transcript-message")
                .expect("message rendered");
            assert_horizontally_aligned("wide message", wide_message, wide_composer);
            assert_eq!(
                wide_composer.size.width,
                gpui::px(super::super::CONVERSATION_COLUMN_WIDTH)
            );

            view.update(cx, |view, cx| {
                view.panel_open = true;
                cx.notify();
            });
            cx.run_until_parked();
            let inspected_composer = cx.debug_bounds("composer").expect("composer rendered");
            let inspected_message = cx
                .debug_bounds("transcript-message")
                .expect("message rendered");
            assert_horizontally_aligned(
                "message with inspector open",
                inspected_message,
                inspected_composer,
            );

            view.update(cx, |view, cx| {
                view.panel_open = false;
                cx.notify();
            });
            cx.simulate_resize(gpui::size(gpui::px(800.0), gpui::px(800.0)));
            cx.run_until_parked();
            let compact_composer = cx.debug_bounds("composer").expect("composer rendered");
            let compact_message = cx
                .debug_bounds("transcript-message")
                .expect("message rendered");
            assert_horizontally_aligned("compact message", compact_message, compact_composer);
            assert!(
                compact_composer.size.width < wide_composer.size.width,
                "the column did not respond to the narrower window"
            );
        }

        #[gpui::test]
        fn user_messages_are_narrower_and_right_aligned(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "user".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "A user message with enough text to size the bubble.".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.simulate_resize(gpui::size(gpui::px(1_400.0), gpui::px(800.0)));
            cx.run_until_parked();

            let composer = cx.debug_bounds("composer").expect("composer rendered");
            let message = cx
                .debug_bounds("transcript-message")
                .expect("user message rendered");
            assert_eq!(message.right(), composer.right());
            assert!(
                message.size.width <= composer.size.width * 0.85,
                "user message was not capped at 85%: {message:?} vs {composer:?}"
            );
            assert!(message.origin.x > composer.origin.x);
        }

        #[gpui::test]
        fn pending_steering_messages_use_pending_styling(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "steering".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "Change direction.".to_owned(),
                    state: app_model::TranscriptState::Pending,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("pending-steering-message").is_some(),
                "pending steering message did not use de-emphasized styling"
            );
        }

        /// The command block is capped at a third of the entry budget, so a
        /// long script cannot crowd out the output worth reading.
        #[test]
        fn command_block_is_capped_at_a_third_of_the_entry() {
            assert!(
                (super::super::COMMAND_BLOCK_HEIGHT - super::super::ENTRY_DETAIL_BUDGET / 3.0)
                    .abs()
                    < f32::EPSILON
            );
            let output_height =
                super::super::ENTRY_DETAIL_BUDGET - super::super::COMMAND_BLOCK_HEIGHT;
            assert!(
                output_height > super::super::COMMAND_BLOCK_HEIGHT * 1.9,
                "output should get the majority of the budget"
            );
        }

        /// Regression: scrolling a tool's output also scrolled the transcript
        /// behind it, dragging the whole conversation along.
        #[gpui::test]
        fn scrolling_output_does_not_scroll_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                let mut sequence = 0;
                let mut apply =
                    |raw: &serde_json::Value, state: &mut app_model::SessionSnapshot| {
                        sequence += 1;
                        state.apply(app_model::DomainEvent::from_sdk_event_for(
                            "session-1",
                            sequence,
                            raw,
                        ));
                    };
                // Enough messages that the transcript itself can scroll.
                for index in 0..60 {
                    apply(
                        &serde_json::json!({"id": format!("u{index}"), "type":"user.message",
                            "data":{"content": format!("message {index}")}}),
                        &mut state,
                    );
                }
                apply(
                    &serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command":"seq 1 500"},
                                "shellToolInfo":{"displayCommand":"seq 1 500",
                                                 "hasWriteFileRedirection":false,
                                                 "possiblePaths":[]}}}),
                    &mut state,
                );
                let output = (1..=500)
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                apply(
                    &serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput": output}}),
                    &mut state,
                );
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            expand_first_tool(cx);

            let wheel = |position, cx: &mut VisualTestContext| {
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(120.0))),
                    modifiers: Modifiers::none(),
                    touch_phase: gpui::TouchPhase::Moved,
                });
                cx.run_until_parked();
            };

            // Control: over the transcript itself, the wheel must scroll it.
            // Without this the assertion below could pass on a transcript that
            // never scrolls at all.
            let transcript = cx.debug_bounds("transcript").expect("transcript rendered");
            let before = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            wheel(
                gpui::point(transcript.center().x, transcript.origin.y + gpui::px(8.0)),
                cx,
            );
            let after_transcript = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert_ne!(
                before, after_transcript,
                "the control case must scroll the transcript"
            );

            // Over a tool entry's output, only the block moves.
            let block = cx
                .debug_bounds("tool-detail")
                .expect("output block rendered");
            wheel(block.center(), cx);
            let after_block = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert_eq!(
                after_transcript, after_block,
                "the transcript must not move when scrolling inside a tool entry"
            );
        }

        /// Regression: the thumb was drawn but inert, so the wheel was the only
        /// way to move a scrollable region.
        #[gpui::test]
        fn dragging_the_transcript_scrollbar_scrolls_it(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..120 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            // Scrollbar geometry needs a measured layout, so give it the
            // follow-up frame the extent change requests.
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            view.update(cx, |view, cx| {
                let max = view.transcript_list.max_offset_for_scrollbar().y;
                view.transcript_list.set_follow_mode(FollowMode::Normal);
                view.transcript_list
                    .set_offset_from_scrollbar(gpui::point(gpui::px(0.0), -max));
                cx.notify();
            });
            cx.run_until_parked();

            // Start at the bottom without auto-follow changing the offset while
            // the press is being measured.
            let bottom = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert!(bottom < gpui::px(0.0));

            // Press near the top of the track: the content should move and the
            // drag should become active.
            let track = cx.debug_bounds("scrollbar").expect("scrollbar rendered");
            let geometry = view.read_with(cx, |view, _| {
                view.drawn_transcript_scrollbar.expect("scrollbar geometry")
            });
            let track_x = track.center().x;
            let near_top = geometry.track_top + gpui::px((geometry.thumb_top / 2.0).max(1.0));
            cx.simulate_mouse_down(
                gpui::point(track_x, near_top),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();

            let after = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert!(
                after != bottom,
                "dragging the scrollbar must scroll: {bottom:?} -> {after:?}"
            );
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.dragging_scrollbar
                        .as_ref()
                        .map(|drag| drag.id.as_str()),
                    Some(super::super::TRANSCRIPT_SCROLL_ID),
                    "the press should begin a drag"
                );
            });

            // Releasing ends the drag so later moves do not keep scrolling.
            cx.simulate_mouse_up(
                gpui::point(track_x, near_top),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert!(view.dragging_scrollbar.is_none());
            });
        }

        /// Regression: the thumb sits above the track and swallowed presses, so
        /// it could only be grabbed by clicking the sliver of track beside it.
        #[gpui::test]
        fn the_scrollbar_thumb_itself_can_be_grabbed(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..120 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();

            // Auto-follow leaves the thumb at the bottom of its track.
            let geometry = view.read_with(cx, |view, _| {
                view.drawn_transcript_scrollbar
                    .expect("the transcript should be scrollable")
            });
            let before = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            let track = cx.debug_bounds("scrollbar").expect("scrollbar rendered");

            // Press the middle of the thumb itself, not the track beside it.
            let thumb_middle =
                geometry.track_top + gpui::px(geometry.thumb_top + geometry.thumb / 2.0);
            cx.simulate_mouse_down(
                gpui::point(track.center().x, thumb_middle),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(
                    view.dragging_scrollbar.is_some(),
                    "pressing the thumb must start a drag"
                );
            });

            // Grabbing the middle of the thumb must not move the content; the
            // grab point is preserved rather than recentred on the pointer.
            let after = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert!(
                (f32::from(after) - f32::from(before)).abs() < 2.0,
                "grabbing the thumb should not lurch: {before:?} -> {after:?}"
            );
        }

        /// Regression: the thumb was drawn from one calculation and hit-tested
        /// against another, so pressing the visible thumb was often treated as
        /// pressing bare track. Where it is drawn must be where it is grabbed.
        #[gpui::test]
        fn the_drawn_thumb_matches_the_grabbable_thumb(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                // Enough content that the thumb is small, which is where the
                // two calculations diverged.
                for index in 0..400 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();

            let geometry = view.read_with(cx, |view, _| {
                view.transcript_scrollbar_geometry()
                    .expect("the transcript should be scrollable")
            });
            let drawn = cx.debug_bounds("scrollbar-thumb").expect("thumb rendered");

            let drawn_top = f32::from(drawn.origin.y - geometry.track_top);
            assert!(
                (drawn_top - geometry.thumb_top).abs() < 1.0,
                "thumb drawn at {drawn_top} but grabbable at {}",
                geometry.thumb_top
            );
            assert!(
                (f32::from(drawn.size.height) - geometry.thumb).abs() < 1.0,
                "thumb drawn {:?} tall but grabbable {} tall",
                drawn.size.height,
                geometry.thumb
            );

            // Pressing the drawn thumb's middle must therefore grab it, not
            // jump the content.
            let before = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            cx.simulate_mouse_down(drawn.center(), MouseButton::Left, Modifiers::none());
            cx.run_until_parked();
            let after = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert!(
                (f32::from(after) - f32::from(before)).abs() < 4.0,
                "pressing the drawn thumb must grab it: {before:?} -> {after:?}"
            );
        }

        /// Regression: the drag recentred the thumb on the pointer, so grabbing
        /// it anywhere but the exact middle made the content jump before the
        /// drag had moved at all.
        #[gpui::test]
        fn grabbing_the_thumb_off_centre_does_not_jump(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..120 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();

            // Move off the bottom so the thumb has room either side of it.
            view.update(cx, |view, cx| {
                view.drag_scrollbar_to(
                    super::super::TRANSCRIPT_SCROLL_ID,
                    view.transcript_list.viewport_bounds().origin.y + gpui::px(200.0),
                    0.0,
                );
                cx.notify();
            });
            cx.run_until_parked();

            let before = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            let geometry = view.read_with(cx, |view, _| {
                view.drawn_transcript_scrollbar.expect("scrollable")
            });
            let track = cx.debug_bounds("scrollbar").expect("scrollbar rendered");

            // Press near the top edge of the thumb rather than its centre.
            let near_thumb_top = geometry.track_top + gpui::px(geometry.thumb_top + 2.0);
            cx.simulate_mouse_down(
                gpui::point(track.center().x, near_thumb_top),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();

            let after = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert!(
                (f32::from(after) - f32::from(before)).abs() < 4.0,
                "pressing the thumb must not move the content: {before:?} -> {after:?}"
            );
        }

        /// Regression: command output was clipped inside a tool entry, so the
        /// end of a long run was unreachable.
        #[gpui::test]
        fn tool_output_scrolls_inside_the_entry(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                let mut sequence = 0;
                let mut apply =
                    |raw: &serde_json::Value, state: &mut app_model::SessionSnapshot| {
                        sequence += 1;
                        state.apply(app_model::DomainEvent::from_sdk_event_for(
                            "session-1",
                            sequence,
                            raw,
                        ));
                    };
                apply(
                    &serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command":"seq 1 500"},
                                "shellToolInfo":{"displayCommand":"seq 1 500",
                                                 "hasWriteFileRedirection":false,
                                                 "possiblePaths":[]}}}),
                    &mut state,
                );
                let output = (1..=500)
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                apply(
                    &serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput": output}}),
                    &mut state,
                );
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            expand_first_tool(cx);

            let bounds = cx
                .debug_bounds("tool-detail")
                .expect("output block rendered");
            // The block is bounded, so 500 lines of output cannot stretch the
            // entry to the height of the conversation.
            let budget = super::super::ENTRY_DETAIL_BUDGET - super::super::COMMAND_BLOCK_HEIGHT;
            assert!(
                bounds.size.height <= gpui::px(budget),
                "the entry stays compact, got {:?}",
                bounds.size.height
            );
        }

        /// `read_bash` carries output from a long-running shell. Treating it as
        /// control-only hid every compile chunk until the agent finished.
        #[gpui::test]
        fn running_read_bash_output_streams_in_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for (sequence, raw) in [
                    serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"read_bash",
                                "arguments":{"shellId":"36"}}}),
                    serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput":"Compiling gcabb v0.1.0\n"}}),
                ]
                .into_iter()
                .enumerate()
                {
                    state.apply(app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        u64::try_from(sequence).unwrap_or(0) + 1,
                        &raw,
                    ));
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            expand_first_tool(cx);

            assert!(
                cx.debug_bounds("tool-detail").is_some(),
                "partial read_bash output should render before the shell completes"
            );
            view.read_with(cx, |view, _| {
                assert_eq!(view.transcript_extent.3, 1);
                assert_eq!(view.transcript_extent.4, "Compiling gcabb v0.1.0\n".len());
            });
        }

        /// Regression: the transcript clipped its overflow, so a long
        /// conversation ran off the bottom of the window with no way to reach
        /// it. It must scroll, and follow new output as it arrives.
        #[gpui::test]
        fn transcript_scrolls_and_follows_new_output(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..80 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| {
                view.transcript_list.set_follow_mode(FollowMode::Normal);
                cx.notify();
            });
            cx.run_until_parked();

            // Auto-follow leaves the view at the tail, so scroll back up.
            let before = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert!(
                before < gpui::px(0.0),
                "the transcript should be scrolled to the newest output, got {before:?}"
            );
            // A wheel event over the transcript must move it. A clipped
            // container swallows the event and the offset stays put.
            let bounds = cx.debug_bounds("transcript").expect("transcript rendered");
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: bounds.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(400.0))),
                modifiers: Modifiers::none(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
            let after = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert!(
                after != before,
                "scrolling up must move the transcript: {before:?} -> {after:?}"
            );

            // The complete model remains available even though only a window is
            // instantiated.
            view.read_with(cx, |view, _| {
                assert_eq!(view.selected().unwrap().snapshot.transcript.len(), 80);
            });
        }

        /// Fills the selected session with enough output to scroll, then parks
        /// the transcript above the tail.
        fn scroll_transcript_away_from_tail(
            view: &gpui::Entity<SessionMvpView>,
            cx: &mut gpui::VisualTestContext,
        ) {
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..80 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let bounds = cx.debug_bounds("transcript").expect("transcript rendered");
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: bounds.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(400.0))),
                modifiers: Modifiers::none(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
        }

        /// Reaching the newest output should never require a hunt for the
        /// scrollbar, and the dimmed tail is what makes the gap obvious before
        /// the button is even noticed. Neither belongs on screen while the
        /// transcript already sits at the bottom.
        #[gpui::test]
        fn jumping_to_the_tail_is_offered_only_while_scrolled_away(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            scroll_transcript_away_from_tail(&view, cx);

            assert!(
                cx.debug_bounds("scroll-to-bottom").is_some(),
                "scrolling up should offer a way back to the newest output"
            );
            assert!(
                cx.debug_bounds("transcript-tail-fade").is_some(),
                "the conversation tail should dim while it is scrolled past"
            );

            view.update(cx, |view, cx| {
                view.transcript_list.set_follow_mode(FollowMode::Tail);
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("scroll-to-bottom").is_none(),
                "the affordance should retire once the transcript is at the tail"
            );
            assert!(
                cx.debug_bounds("transcript-tail-fade").is_none(),
                "nothing is being scrolled past, so nothing should be dimmed"
            );
        }

        /// The jump glides rather than cutting: the intervening content flying
        /// past is what tells the reader how far the view moved. A snap would
        /// leave them re-orienting at the tail.
        #[gpui::test]
        fn jumping_to_the_tail_glides_instead_of_snapping(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            scroll_transcript_away_from_tail(&view, cx);

            let parked = view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            let button = cx
                .debug_bounds("scroll-to-bottom")
                .expect("the jump affordance should be rendered");
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();

            let started = view
                .read_with(cx, |view, _| view.scroll_to_bottom)
                .expect("pressing the affordance should start a glide")
                .started;

            // Halfway through, the transcript has left where it was parked but
            // has not yet reached the tail.
            let midpoint = view.update(cx, |view, _| {
                let still_gliding =
                    view.step_scroll_to_bottom(started + SCROLL_TO_BOTTOM_DURATION / 2);
                assert!(still_gliding, "the glide should still be running midway");
                assert!(
                    !view.transcript_list.is_following_tail(),
                    "the glide should not arrive early"
                );
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            });
            assert!(
                midpoint < parked,
                "the glide should be travelling toward the tail: {parked:?} -> {midpoint:?}"
            );

            view.update(cx, |view, _| {
                let still_gliding = view.step_scroll_to_bottom(started + SCROLL_TO_BOTTOM_DURATION);
                assert!(!still_gliding, "the glide should end at its full duration");
                assert!(
                    view.transcript_list.is_following_tail(),
                    "landing at the tail should resume following new output"
                );
                assert!(
                    view.scroll_to_bottom.is_none(),
                    "a landed glide should leave no state behind"
                );
            });
        }

        /// Steering by hand mid-glide means the reader wants somewhere else.
        /// Continuing to drag them to the tail would be a fight they did not
        /// ask for.
        #[gpui::test]
        fn scrolling_by_hand_abandons_the_glide(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            scroll_transcript_away_from_tail(&view, cx);

            let button = cx
                .debug_bounds("scroll-to-bottom")
                .expect("the jump affordance should be rendered");
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();
            assert!(
                view.read_with(cx, |view, _| view.scroll_to_bottom.is_some()),
                "the glide should be running before the manual scroll"
            );

            let bounds = cx.debug_bounds("transcript").expect("transcript rendered");
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: bounds.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(200.0))),
                modifiers: Modifiers::none(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(
                    view.scroll_to_bottom.is_none(),
                    "scrolling by hand should abandon the glide"
                );
            });
        }

        /// The deterministic large-session shape must not make render work grow
        /// with retained history. This counter covers element creation and,
        /// through the markdown cache, parsing of completed off-screen rows.
        #[gpui::test]
        fn large_transcript_render_work_is_bounded_by_the_window(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "Large session");
                state.transcript = (0..10_000)
                    .map(|index| app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("## Result {index}\n\nDeterministic transcript row."),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    })
                    .collect();
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert_eq!(view.timeline.items.len(), 10_000);
                assert!(
                    view.transcript_rows_rendered <= 64,
                    "rendered {} rows for a 10,000-row transcript",
                    view.transcript_rows_rendered
                );
                assert!(
                    view.markdown_cache.len() <= 64,
                    "parsed {} off-screen markdown documents",
                    view.markdown_cache.len()
                );
            });
        }

        /// Switching sessions and receiving new output both scroll to the tail,
        /// but an unchanged transcript does not, so reading is not interrupted.
        #[gpui::test]
        fn transcript_only_follows_when_it_grows(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m0".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: "first".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            // The first render adopts the session and records its extent.
            let extent = view.read_with(cx, |view, _| view.transcript_extent.clone());
            assert_eq!(extent.0, "session-1");
            assert_eq!(extent.1, 1);

            // Re-rendering without new output leaves the recorded extent alone.
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            let unchanged = view.read_with(cx, |view, _| view.transcript_extent.clone());
            assert_eq!(unchanged, extent);
        }

        /// Regression: selecting a chat repointed the project selection at the
        /// chats directory, which hid every project session in the sidebar.
        #[gpui::test]
        fn selecting_a_chat_keeps_project_sessions_visible(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut chat = snapshot("chat-1", "A chat");
                chat.metadata.kind = SessionKind::Chat;
                chat.metadata.repository_root = None;
                chat.metadata.project_path = "/tmp/chats".to_owned();
                view.sessions
                    .push(SessionProjection::for_test(SessionHandle::for_test(chat)));
                cx.notify();
            });
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("session-row").is_some(),
                "the project session should be listed before selecting a chat"
            );

            view.update(cx, |view, cx| {
                view.select_session("chat-1".to_owned(), cx);
            });
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.selected_project,
                    std::path::PathBuf::from("/tmp/project"),
                    "a chat must not repoint the project selection"
                );
            });
            assert!(
                cx.debug_bounds("session-row").is_some(),
                "project sessions must stay visible while a chat is selected"
            );
        }

        /// Regression: removing the last project left the pill naming the
        /// launch directory, which was not a configured project.
        #[gpui::test]
        fn removing_the_last_project_falls_back_to_chat(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/project".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "project");
            });

            // The service reports an empty project list after a removal.
            view.update(cx, |view, cx| {
                view.projects = Vec::new();
                view.composing_chat = true;
                view.selected_session = None;
                cx.notify();
            });

            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.composer_project_label(),
                    "Chat",
                    "with no projects configured the composer targets chat"
                );
            });
        }

        /// An unconfigured project selection must not be named as if it were
        /// a project.
        #[gpui::test]
        fn unknown_project_selection_reads_as_no_project(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = Vec::new();
                view.composing_chat = false;
                view.selected_project = std::path::PathBuf::from("/tmp/not-a-project");
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "No project");
            });
        }

        fn model(id: &str, efforts: &[&str], context: Option<u64>) -> app_model::ModelOption {
            app_model::ModelOption {
                id: id.to_owned(),
                name: "GPT-5.6 Sol".to_owned(),
                supported_reasoning_efforts: efforts
                    .iter()
                    .map(|effort| (*effort).to_owned())
                    .collect(),
                context_windows: context
                    .map(|max_tokens| app_model::ContextWindowOption {
                        tier: "default".to_owned(),
                        max_tokens: Some(max_tokens),
                    })
                    .into_iter()
                    .collect(),
            }
        }

        /// A model that exposes an extended context tier.
        fn two_tier_model(id: &str, efforts: &[&str]) -> app_model::ModelOption {
            app_model::ModelOption {
                id: id.to_owned(),
                name: "GPT-5.6 Sol".to_owned(),
                supported_reasoning_efforts: efforts
                    .iter()
                    .map(|effort| (*effort).to_owned())
                    .collect(),
                context_windows: vec![
                    app_model::ContextWindowOption {
                        tier: "default".to_owned(),
                        max_tokens: Some(400_000),
                    },
                    app_model::ContextWindowOption {
                        tier: "long_context".to_owned(),
                        max_tokens: Some(1_050_000),
                    },
                ],
            }
        }

        /// Regression: the per-session model catalog can list a model without
        /// its reasoning efforts, which made the thinking-level pill vanish as
        /// soon as a session was selected.
        #[gpui::test]
        fn session_keeps_the_thinking_level_pill_from_the_app_catalog(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                // The app catalog knows the model's capabilities.
                view.startup =
                    super::super::StartupState::Ready(copilot_provider::ProviderCompatibility {
                        sdk_crate_version: "test".to_owned(),
                        sdk_protocol_version: 3,
                        negotiated_protocol_version: 3,
                        process_id: None,
                        startup: None,
                        available_modes: vec!["interactive".to_owned()],
                        available_models: vec![model(
                            "gpt-5.6-sol",
                            &["low", "medium", "high"],
                            Some(1_000_000),
                        )],
                    });
                view.draft_model = Some("gpt-5.6-sol".to_owned());
                cx.notify();
            });
            // Home has the pill.
            view.read_with(cx, |view, _| {
                assert!(!view.effort_options().is_empty());
                assert_eq!(view.draft_model_label(), "GPT-5.6 Sol");
            });

            // Selecting a session whose catalog omits the capability detail
            // must not lose either the thinking level or the context length.
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.controls.available_models = vec![model("gpt-5.6-sol", &[], None)];
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });

            view.read_with(cx, |view, _| {
                assert!(
                    !view.effort_options().is_empty(),
                    "the session composer must offer the same thinking levels"
                );
                assert_eq!(
                    view.draft_context_label().as_deref(),
                    Some("1M context"),
                    "the session composer must show the same context length"
                );
            });
        }

        /// The app catalog is authoritative for capabilities, because the
        /// per-session catalog collapses context tiers into the active window
        /// and reports no reasoning efforts at all.
        #[gpui::test]
        fn app_catalog_is_authoritative_for_capabilities(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.startup =
                    super::super::StartupState::Ready(copilot_provider::ProviderCompatibility {
                        sdk_crate_version: "test".to_owned(),
                        sdk_protocol_version: 3,
                        negotiated_protocol_version: 3,
                        process_id: None,
                        startup: None,
                        available_modes: vec!["interactive".to_owned()],
                        available_models: vec![two_tier_model("gpt-5.6-sol", &["low", "medium"])],
                    });
                let mut state = snapshot("session-1", "First session");
                // Shaped like the live session catalog: no efforts, and the
                // tiers collapsed into a single active window.
                state.controls.available_models = vec![model("gpt-5.6-sol", &[], Some(1_050_000))];
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                view.draft_model = Some("gpt-5.6-sol".to_owned());
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.effort_options().len(), 2);
                // Both tiers stay selectable, so the control stays a picker
                // instead of degrading to static text.
                assert_eq!(view.draft_context_windows().len(), 2);
            });
        }

        /// Regression: the branch beside the location pill showed the branch of
        /// the directory GCABB was launched from, not the session's base.
        #[gpui::test]
        fn branch_label_follows_the_project_and_location(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/project".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                view.selected_project = std::path::PathBuf::from("/tmp/project");
                // The launch directory is on an unrelated branch.
                view.branch = "launch-worktree-branch".to_owned();
                view.project_branch = Some("feature".to_owned());
                cx.notify();
            });

            // A new worktree branches from the project default.
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_branch_label(), "main");
            });

            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Location,
                    app_model::SessionLocation::LocalRepository
                        .as_str()
                        .to_owned(),
                    cx,
                );
            });
            // Running in place uses the branch that checkout has now.
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_branch_label(), "feature");
                assert_ne!(view.composer_branch_label(), view.branch);
            });
        }

        /// The location pill is offered for projects and switches the target.
        #[gpui::test]
        fn location_pill_switches_where_the_session_runs(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.read_with(cx, |view, _| {
                assert_eq!(view.draft_location, app_model::SessionLocation::NewWorktree);
            });
            assert!(
                cx.debug_bounds("location-pill").is_some(),
                "a project session offers a location"
            );

            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Location,
                    app_model::SessionLocation::LocalRepository
                        .as_str()
                        .to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.draft_location,
                    app_model::SessionLocation::LocalRepository
                );
            });
        }

        /// A chat has no checkout, so it must not offer a location.
        #[gpui::test]
        fn chats_do_not_offer_a_location(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
            });
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("location-pill").is_none(),
                "a chat has no checkout to choose"
            );
        }

        /// Chats are listed under Chats, not under a project.
        #[gpui::test]
        fn chats_are_listed_separately_from_project_sessions(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut chat = snapshot("chat-1", "A chat");
                chat.metadata.kind = SessionKind::Chat;
                chat.metadata.repository_root = None;
                view.sessions
                    .push(SessionProjection::for_test(SessionHandle::for_test(chat)));
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("chat-row").is_some(),
                "the chat should render under Chats"
            );
            view.read_with(cx, |view, _| {
                let chats: Vec<_> = view
                    .sessions
                    .iter()
                    .filter(|session| session.snapshot.metadata.is_chat())
                    .collect();
                assert_eq!(chats.len(), 1);
            });
        }

        /// An empty name would leave the row unidentifiable, so it is ignored.
        #[gpui::test]
        fn blank_renames_are_ignored(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.renaming_session = Some("session-1".to_owned());
                view.commit_rename("   ", cx);
            });

            view.read_with(cx, |view, _| {
                assert_eq!(view.sessions[0].snapshot.metadata.title, "First session");
            });
            assert!(commands.try_recv().is_err(), "no command should be sent");
        }

        /// An install with nothing to report must not lose space to a banner.
        #[gpui::test]
        fn no_banner_is_shown_when_there_is_no_update(cx: &mut TestAppContext) {
            let (_view, cx, _commands) = setup(cx);
            assert!(
                cx.debug_bounds("update-banner").is_none(),
                "the banner must stay hidden when there is no update"
            );
        }

        #[gpui::test]
        fn an_offered_update_shows_the_banner_with_its_version(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::Available {
                    version: "0.2.0".to_owned(),
                    notes: "## GCABB v0.2.0\n\nFaster startup.".to_owned(),
                };
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("update-banner").is_some(),
                "an offered update must be visible"
            );
            view.read_with(cx, |view, _| {
                let (message, _, summary) =
                    view.update_banner_text().expect("banner text rendered");
                assert!(message.contains("0.2.0"), "got {message}");
                // The heading is skipped so the summary is the first real line.
                assert_eq!(summary.as_deref(), Some("Faster startup."));
            });
        }

        /// Regression guard: without a worker the buttons must still be inert
        /// rather than panicking, since a developer build has no worker at all.
        #[gpui::test]
        fn pressing_update_without_a_worker_is_harmless(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::Available {
                    version: "0.2.0".to_owned(),
                    notes: String::new(),
                };
                cx.notify();
            });
            cx.run_until_parked();

            let button = cx.debug_bounds("Update").expect("update button rendered");
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(view.update_service.is_none(), "test builds have no worker");
                assert!(matches!(view.update_ui, UpdateUi::Available { .. }));
            });
        }

        /// A failed update must be dismissible, or the banner would be stuck.
        #[gpui::test]
        fn a_failed_update_can_be_dismissed(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::Failed("signature does not match".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let button = cx.debug_bounds("Dismiss").expect("dismiss button rendered");
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| assert_eq!(view.update_ui, UpdateUi::Hidden));
            assert!(
                cx.debug_bounds("update-banner").is_none(),
                "the banner goes away once dismissed"
            );
        }

        /// An applied update takes effect on restart, so it must say so and
        /// offer the restart rather than silently doing nothing.
        #[gpui::test]
        fn an_applied_update_offers_a_restart(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::ReadyToRestart {
                    version: "0.2.0".to_owned(),
                };
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("update-restart").is_some(),
                "a staged update must offer a restart"
            );
            view.read_with(cx, |view, _| {
                let (message, _, _) = view.update_banner_text().expect("banner text rendered");
                assert!(message.contains("restart"), "got {message}");
            });
        }

        // --- Changes panel -------------------------------------------------

        fn changed_file(
            path: &str,
            status: app_model::ChangeStatus,
            diff: Option<&str>,
        ) -> app_model::ChangedFile {
            app_model::ChangedFile {
                path: path.to_owned(),
                original_path: None,
                status,
                stage: app_model::ChangeStage::Unstaged,
                stats: app_model::DiffStats {
                    insertions: 2,
                    deletions: 1,
                },
                diff: diff.map(str::to_owned),
                binary: false,
                diff_omitted_reason: None,
            }
        }

        fn snapshot_with_changes(files: Vec<app_model::ChangedFile>) -> SessionSnapshot {
            let mut state = snapshot("session-1", "First session");
            state.changes = app_model::ChangesView {
                base: Some("abc1234".to_owned()),
                base_label: Some("main".to_owned()),
                tracking_ref: Some("origin/main".to_owned()),
                head: Some("def5678".to_owned()),
                branch: Some("feature".to_owned()),
                files,
                generated_at: Some("1".to_owned()),
                error: None,
            };
            state
        }

        /// Render the Changes panel for one session with the given files.
        fn open_changes(
            view: &gpui::Entity<SessionMvpView>,
            cx: &mut VisualTestContext,
            files: Vec<app_model::ChangedFile>,
        ) {
            view.update(cx, |view, cx| {
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(
                    snapshot_with_changes(files),
                ))];
                view.selected_session = Some("session-1".to_owned());
                view.panel_open = true;
                view.active_panel = crate::SessionPanel::Changes;
                cx.notify();
            });
            cx.simulate_resize(gpui::size(gpui::px(1_400.0), gpui::px(800.0)));
            cx.run_until_parked();
        }

        #[gpui::test]
        fn changes_refresh_button_requests_a_forced_base_refresh(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            open_changes(&view, cx, Vec::new());

            let refresh = cx
                .debug_bounds("changes-refresh")
                .expect("changes refresh button");
            cx.simulate_click(refresh.center(), Modifiers::none());

            assert!(matches!(
                commands.try_recv(),
                Ok(ServiceCommand::RefreshChanges {
                    app_session_id,
                    force: true,
                }) if app_session_id == "session-1"
            ));
        }

        #[gpui::test]
        fn changes_base_menu_selects_a_persisted_comparison_branch(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            open_changes(&view, cx, Vec::new());
            view.update(cx, |view, cx| {
                view.base_ref_options = vec!["release/next".to_owned()];
                view.base_menu_visibility = crate::SettingsVisibility::Open;
                cx.notify();
            });
            cx.run_until_parked();

            let option = cx
                .debug_bounds("changes-base-option-0")
                .expect("base branch option");
            cx.simulate_click(option.center(), Modifiers::none());

            assert!(matches!(
                commands.try_recv(),
                Ok(ServiceCommand::SetBaseRef {
                    app_session_id,
                    base_ref,
                }) if app_session_id == "session-1" && base_ref == "release/next"
            ));
        }

        #[gpui::test]
        fn changes_base_menu_can_reset_to_the_project_default(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            open_changes(&view, cx, Vec::new());
            view.update(cx, |view, cx| {
                view.base_default_ref = Some("main".to_owned());
                view.base_menu_visibility = crate::SettingsVisibility::Open;
                cx.notify();
            });
            cx.run_until_parked();

            let reset = cx
                .debug_bounds("changes-base-reset")
                .expect("reset base option");
            cx.simulate_click(reset.center(), Modifiers::none());

            assert!(matches!(
                commands.try_recv(),
                Ok(ServiceCommand::SetBaseRef {
                    app_session_id,
                    base_ref,
                }) if app_session_id == "session-1" && base_ref == "main"
            ));
        }

        fn changes_scroll(
            view: &gpui::Entity<SessionMvpView>,
            cx: &mut VisualTestContext,
        ) -> gpui::ScrollHandle {
            view.read_with(cx, |view, _| {
                view.scroll_handle(crate::CHANGES_SCROLL_ID)
                    .expect("the changes panel tracks a scroll handle")
            })
        }

        fn long_diff(lines: usize) -> String {
            use std::fmt::Write as _;

            let mut diff = String::from("@@ -1,1 +1,1 @@\n");
            for line in 0..lines {
                let _ = writeln!(diff, "+line {line}");
            }
            diff
        }

        #[gpui::test]
        fn a_file_row_expands_and_collapses_its_diff(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            open_changes(
                &view,
                cx,
                vec![changed_file(
                    "src/lib.rs",
                    app_model::ChangeStatus::Modified,
                    Some("@@ -1 +1 @@\n-old\n+new\n"),
                )],
            );

            assert!(
                cx.debug_bounds("change-diff-src/lib.rs").is_none(),
                "files start collapsed"
            );
            let row = cx
                .debug_bounds("change-row-src/lib.rs")
                .expect("the file row is rendered");
            cx.simulate_click(row.center(), Modifiers::none());
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("change-diff-src/lib.rs").is_some(),
                "clicking a row expands its diff"
            );
            view.read_with(cx, |view, _| {
                assert!(view.change_expanded("session-1", "src/lib.rs"));
            });

            let row = cx
                .debug_bounds("change-row-src/lib.rs")
                .expect("the file row is still rendered");
            cx.simulate_click(row.center(), Modifiers::none());
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("change-diff-src/lib.rs").is_none(),
                "clicking an expanded row collapses it"
            );
        }

        #[gpui::test]
        fn the_disclosure_control_toggles_the_diff(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            open_changes(
                &view,
                cx,
                vec![changed_file(
                    "src/lib.rs",
                    app_model::ChangeStatus::Modified,
                    Some("@@ -1 +1 @@\n-old\n+new\n"),
                )],
            );

            let toggle = cx
                .debug_bounds("change-toggle-src/lib.rs")
                .expect("the disclosure control is rendered");
            cx.simulate_click(toggle.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(view.change_expanded("session-1", "src/lib.rs"));
            });
            assert!(cx.debug_bounds("change-diff-src/lib.rs").is_some());
        }

        /// Rows must be reachable without a pointer, so focus plus Enter has to
        /// expand exactly the focused file.
        #[gpui::test]
        fn a_focused_row_expands_with_the_keyboard(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            open_changes(
                &view,
                cx,
                vec![changed_file(
                    "src/lib.rs",
                    app_model::ChangeStatus::Modified,
                    Some("@@ -1 +1 @@\n-old\n+new\n"),
                )],
            );

            let focus = view.read_with(cx, |view, _| {
                view.change_focus
                    .borrow()
                    .get("session-1\u{1f}src/lib.rs")
                    .cloned()
                    .expect("the row owns a focus handle")
            });
            cx.update(|window, cx| window.focus(&focus, cx));
            cx.run_until_parked();
            cx.simulate_event(gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("enter").expect("keystroke"),
                is_held: false,
                prefer_character_input: false,
            });
            cx.simulate_event(gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse("enter").expect("keystroke"),
            });
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(
                    view.change_expanded("session-1", "src/lib.rs"),
                    "Enter on a focused row must expand it"
                );
            });
        }

        /// The point of the redesign: diffs live in the panel's own scroll
        /// flow, so expanding files extends one document instead of creating
        /// per-file viewports.
        #[gpui::test]
        fn expanded_diffs_extend_the_panels_own_scroll_flow(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            open_changes(
                &view,
                cx,
                vec![
                    changed_file(
                        "src/lib.rs",
                        app_model::ChangeStatus::Modified,
                        Some(&long_diff(60)),
                    ),
                    changed_file(
                        "src/main.rs",
                        app_model::ChangeStatus::Added,
                        Some(&long_diff(60)),
                    ),
                ],
            );

            let collapsed = f32::from(changes_scroll(&view, cx).max_offset().y);
            assert!(
                collapsed.abs() < f32::EPSILON,
                "two collapsed rows must fit without scrolling, got {collapsed}"
            );

            for path in ["src/lib.rs", "src/main.rs"] {
                view.update(cx, |view, cx| {
                    view.toggle_change("session-1", path);
                    cx.notify();
                });
            }
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("change-diff-src/lib.rs").is_some()
                    && cx.debug_bounds("change-diff-src/main.rs").is_some(),
                "multiple files stay expanded at once"
            );
            let expanded = changes_scroll(&view, cx).max_offset().y;
            assert!(
                f32::from(expanded) > 0.0,
                "expanded diffs must extend the outer scroll flow"
            );
        }

        #[gpui::test]
        fn many_changed_files_scroll_as_one_list(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let files = (0..120)
                .map(|index| {
                    changed_file(
                        Box::leak(format!("src/file_{index}.rs").into_boxed_str()),
                        app_model::ChangeStatus::Modified,
                        Some("@@ -1 +1 @@\n-old\n+new\n"),
                    )
                })
                .collect();
            open_changes(&view, cx, files);

            let handle = changes_scroll(&view, cx);
            assert!(
                f32::from(handle.max_offset().y) > 0.0,
                "a long file list must be scrollable"
            );
            assert!(
                cx.debug_bounds("change-row-src/file_0.rs").is_some(),
                "the first row is rendered"
            );
        }

        /// A very large diff must lay out in full instead of being trapped in
        /// its own viewport.
        #[gpui::test]
        fn a_very_large_diff_is_not_given_its_own_viewport(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            open_changes(
                &view,
                cx,
                vec![changed_file(
                    "src/huge.rs",
                    app_model::ChangeStatus::Modified,
                    Some(&format!("+{}\n{}", "x".repeat(4_000), long_diff(2_000))),
                )],
            );
            view.update(cx, |view, cx| {
                view.toggle_change("session-1", "src/huge.rs");
                cx.notify();
            });
            cx.run_until_parked();

            let list = cx
                .debug_bounds("changes-list")
                .expect("the changes list is rendered");
            let diff = cx
                .debug_bounds("change-diff-src/huge.rs")
                .expect("the diff is rendered");
            assert!(
                diff.size.height > list.size.height,
                "a large diff must lay out beyond the viewport instead of scrolling inside it"
            );
            assert!(
                diff.size.width <= list.size.width,
                "a long diff line must not widen the panel"
            );
            assert!(f32::from(changes_scroll(&view, cx).max_offset().y) > 0.0);
        }

        /// Refreshed change data must not throw away what the user opened, or
        /// where they had scrolled to.
        #[gpui::test]
        fn expansion_and_scroll_survive_refreshes_and_tab_switches(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let files: Vec<_> = (0..40)
                .map(|index| {
                    changed_file(
                        Box::leak(format!("src/file_{index}.rs").into_boxed_str()),
                        app_model::ChangeStatus::Modified,
                        Some("@@ -1 +1 @@\n-old\n+new\n"),
                    )
                })
                .collect();
            open_changes(&view, cx, files.clone());
            view.update(cx, |view, cx| {
                view.toggle_change("session-1", "src/file_1.rs");
                cx.notify();
            });
            cx.run_until_parked();
            let handle = changes_scroll(&view, cx);
            handle.set_offset(gpui::point(gpui::px(0.0), gpui::px(-60.0)));
            cx.run_until_parked();

            let mut refreshed = files;
            refreshed[1].stats.insertions = 9;
            refreshed.push(changed_file(
                "src/new.rs",
                app_model::ChangeStatus::Added,
                Some("@@ -0,0 +1 @@\n+fresh\n"),
            ));
            view.update(cx, |view, cx| {
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(
                    snapshot_with_changes(refreshed),
                ))];
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("change-diff-src/file_1.rs").is_some(),
                "a refresh must not collapse a file that still exists"
            );
            let offset = f32::from(changes_scroll(&view, cx).offset().y);
            assert!(
                (offset + 60.0).abs() < f32::EPSILON,
                "a refresh must not move the panel's scroll position, got {offset}"
            );

            view.update(cx, |view, cx| {
                view.active_panel = crate::SessionPanel::Terminals;
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| {
                view.active_panel = crate::SessionPanel::Changes;
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("change-diff-src/file_1.rs").is_some(),
                "switching panel tabs must not collapse expanded diffs"
            );
        }

        /// Renames, deletions, and binary files still need a row, and the
        /// states that have no diff report under their own row.
        #[gpui::test]
        fn renamed_deleted_and_binary_files_report_under_their_own_row(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let mut renamed = changed_file("src/new.rs", app_model::ChangeStatus::Renamed, None);
            renamed.original_path = Some("src/old.rs".to_owned());
            let deleted = changed_file(
                "src/gone.rs",
                app_model::ChangeStatus::Deleted,
                Some("@@ -1 +0,0 @@\n-old\n"),
            );
            let mut binary = changed_file("assets/logo.png", app_model::ChangeStatus::Added, None);
            binary.binary = true;
            open_changes(&view, cx, vec![renamed, deleted, binary]);

            for path in ["src/new.rs", "src/gone.rs", "assets/logo.png"] {
                view.update(cx, |view, cx| {
                    view.toggle_change("session-1", path);
                    cx.notify();
                });
            }
            cx.run_until_parked();

            assert!(cx.debug_bounds("change-row-src/new.rs").is_some());
            assert!(cx.debug_bounds("change-row-src/gone.rs").is_some());
            assert!(cx.debug_bounds("change-row-assets/logo.png").is_some());
            assert!(
                cx.debug_bounds("change-diff-assets/logo.png").is_some(),
                "a binary file reports beneath its own row"
            );
            assert!(cx.debug_bounds("change-diff-src/gone.rs").is_some());
        }

        /// Deleting a session must not leave its expansion state behind for a
        /// later session to inherit.
        #[gpui::test]
        fn deleting_a_session_drops_its_expansion_state(cx: &mut TestAppContext) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            open_changes(
                &view,
                cx,
                vec![changed_file(
                    "src/lib.rs",
                    app_model::ChangeStatus::Modified,
                    Some("@@ -1 +1 @@\n-old\n+new\n"),
                )],
            );
            view.update(cx, |view, cx| {
                view.toggle_change("session-1", "src/lib.rs");
                view.toggle_tool("c1");
                cx.notify();
            });
            updates
                .send(ServiceUpdate::SessionDeleted("session-1".to_owned()))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                assert!(!view.change_expanded("session-1", "src/lib.rs"));
                assert!(!view.expanded_tools.contains_key("session-1"));
            });
        }

        // --- Wheel handoff between nested scroll regions ---------------------

        /// A session whose transcript is long enough to scroll, with one tool
        /// entry whose output is `lines` long.
        fn transcript_with_tool_output(lines: usize) -> SessionSnapshot {
            let mut state = snapshot("session-1", "First session");
            for index in 0..80 {
                state.transcript.push(app_model::TranscriptMessage {
                    id: format!("m{index}"),
                    role: app_model::TranscriptRole::Assistant,
                    content: format!("message {index} with enough text to take a line"),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: u64::try_from(index).unwrap_or(0) + 1,
                    attachments: Vec::new(),
                });
            }
            let mut sequence = 1_000;
            let mut apply = |raw: &serde_json::Value, state: &mut app_model::SessionSnapshot| {
                sequence += 1;
                state.apply(app_model::DomainEvent::from_sdk_event_for(
                    "session-1",
                    sequence,
                    raw,
                ));
            };
            apply(
                &serde_json::json!({"id":"t","type":"tool.execution_start",
                    "data":{"toolCallId":"c1","toolName":"bash",
                            "arguments":{"command":"seq"},
                            "shellToolInfo":{"displayCommand":"seq",
                                             "hasWriteFileRedirection":false,
                                             "possiblePaths":[]}}}),
                &mut state,
            );
            let output = (1..=lines)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            apply(
                &serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                    "data":{"toolCallId":"c1","partialOutput": output}}),
                &mut state,
            );
            state
        }

        fn show(
            view: &gpui::Entity<SessionMvpView>,
            cx: &mut VisualTestContext,
            state: SessionSnapshot,
        ) {
            view.update(cx, |view, cx| {
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                view.expanded_tools
                    .entry("session-1".to_owned())
                    .or_default()
                    .insert("c1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
        }

        fn wheel(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>, x: f32, y: f32) {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: at,
                delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(x), gpui::px(y))),
                modifiers: Modifiers::none(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
        }

        fn transcript_offset(
            view: &gpui::Entity<SessionMvpView>,
            cx: &mut VisualTestContext,
        ) -> gpui::Pixels {
            view.read_with(cx, |view, _| {
                view.transcript_list.scroll_px_offset_for_scrollbar().y
            })
        }

        fn detail_offset(
            view: &gpui::Entity<SessionMvpView>,
            cx: &mut VisualTestContext,
        ) -> gpui::Pixels {
            view.read_with(cx, |view, _| {
                view.scroll_handle("tool-output-c1")
                    .expect("the output block tracks a scroll handle")
                    .offset()
                    .y
            })
        }

        /// A block short enough to need no scrolling used to swallow the wheel,
        /// which left the transcript stuck wherever the pointer happened to be.
        #[gpui::test]
        fn a_detail_block_with_nothing_to_scroll_leaves_the_wheel_alone(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            show(&view, cx, transcript_with_tool_output(1));

            let block = cx
                .debug_bounds("tool-detail")
                .expect("the output block is rendered");
            let before = transcript_offset(&view, cx);
            wheel(cx, block.center(), 0.0, 400.0);
            let after = transcript_offset(&view, cx);

            assert!(
                after != before,
                "a block with nothing to scroll must let the transcript move: {before:?} -> {after:?}"
            );
        }

        #[gpui::test]
        fn a_scrollable_detail_block_takes_the_wheel_from_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            show(&view, cx, transcript_with_tool_output(500));
            let block = cx
                .debug_bounds("tool-detail")
                .expect("the output block is rendered");
            // Streaming output follows its tail, so start from the top.
            view.update(cx, |view, cx| {
                view.scroll_handle("tool-output-c1")
                    .expect("scroll handle")
                    .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
                cx.notify();
            });
            cx.run_until_parked();

            let transcript_before = transcript_offset(&view, cx);
            wheel(cx, block.center(), 0.0, -200.0);

            assert!(
                detail_offset(&view, cx) < gpui::px(0.0),
                "the block under the pointer must scroll"
            );
            assert_eq!(
                transcript_offset(&view, cx),
                transcript_before,
                "the transcript must not scroll along with the block"
            );
        }

        /// The handoff: once the inner block has nothing left to give, the
        /// surface behind it takes over instead of the gesture dying.
        #[gpui::test]
        fn a_detail_block_at_its_end_hands_the_wheel_back(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            show(&view, cx, transcript_with_tool_output(500));
            let block = cx
                .debug_bounds("tool-detail")
                .expect("the output block is rendered");
            // Park the block against its top edge, with the transcript sitting
            // at its tail so there is room above it to take over.
            view.update(cx, |view, cx| {
                view.scroll_handle("tool-output-c1")
                    .expect("scroll handle")
                    .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
                cx.notify();
            });
            cx.run_until_parked();

            let transcript_before = transcript_offset(&view, cx);
            wheel(cx, block.center(), 0.0, 200.0);

            assert_eq!(
                detail_offset(&view, cx),
                gpui::px(0.0),
                "the block is already at its end"
            );
            assert_ne!(
                transcript_offset(&view, cx),
                transcript_before,
                "a block at its end must hand the wheel to the transcript"
            );
        }

        /// Diffs scroll sideways inside a panel that scrolls down, so a
        /// vertical wheel over one belongs to the panel.
        #[gpui::test]
        fn a_vertical_wheel_over_a_diff_scrolls_the_changes_panel(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let files: Vec<_> = (0..40)
                .map(|index| {
                    changed_file(
                        Box::leak(format!("src/file_{index}.rs").into_boxed_str()),
                        app_model::ChangeStatus::Modified,
                        Some(&long_diff(40)),
                    )
                })
                .collect();
            open_changes(&view, cx, files);
            view.update(cx, |view, cx| {
                view.toggle_change("session-1", "src/file_0.rs");
                cx.notify();
            });
            cx.run_until_parked();

            let diff = cx
                .debug_bounds("change-diff-src/file_0.rs")
                .expect("the diff is rendered");
            let before = changes_scroll(&view, cx).offset().y;
            wheel(cx, diff.center(), 0.0, -120.0);

            assert!(
                changes_scroll(&view, cx).offset().y < before,
                "a vertical wheel over a diff must scroll the panel"
            );
        }

        /// The diff still owns sideways gestures, and must not pass them on for
        /// the panel to turn into vertical movement.
        #[gpui::test]
        fn a_horizontal_wheel_over_a_diff_stays_in_the_diff(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let files: Vec<_> = (0..40)
                .map(|index| {
                    changed_file(
                        Box::leak(format!("src/file_{index}.rs").into_boxed_str()),
                        app_model::ChangeStatus::Modified,
                        Some(&format!("+{}\n", "x".repeat(400))),
                    )
                })
                .collect();
            open_changes(&view, cx, files);
            view.update(cx, |view, cx| {
                view.toggle_change("session-1", "src/file_0.rs");
                cx.notify();
            });
            cx.run_until_parked();

            let diff = cx
                .debug_bounds("change-diff-src/file_0.rs")
                .expect("the diff is rendered");
            let before = changes_scroll(&view, cx).offset().y;
            wheel(cx, diff.center(), -80.0, 0.0);

            let after = changes_scroll(&view, cx).offset().y;
            assert!(
                (f32::from(after - before)).abs() < f32::EPSILON,
                "a sideways gesture must not scroll the panel: {before:?} -> {after:?}"
            );
        }

        #[gpui::test]
        fn download_progress_is_shown_in_the_banner(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::Downloading {
                    received: 512,
                    total: Some(1024),
                };
                cx.notify();
            });
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                let (message, _, _) = view.update_banner_text().expect("banner text rendered");
                assert!(message.contains("50%"), "got {message}");
            });
        }
    }
}
