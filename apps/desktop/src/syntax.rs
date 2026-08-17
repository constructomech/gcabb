use std::path::Path;
use std::sync::LazyLock;

use gpui::{FontStyle, FontWeight, HighlightStyle, UnderlineStyle, px, rgb, rgba};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    FontStyle as SyntectFontStyle, Style as SyntectStyle, Theme, ThemeSet,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

const BLUE: u32 = 0x0058_a6ff;
const GREEN: u32 = 0x003f_b950;
const MUTED: u32 = 0x008b_949e;
const RED: u32 = 0x00f8_5161;
const ADDED_BACKGROUND: u32 = 0x2386_3626;
const DELETED_BACKGROUND: u32 = 0xf851_6126;

struct HighlightAssets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

static ASSETS: LazyLock<HighlightAssets> = LazyLock::new(|| {
    let syntaxes = SyntaxSet::load_defaults_newlines();
    let theme = ThemeSet::load_defaults()
        .themes
        .remove("base16-ocean.dark")
        .expect("Syntect's bundled Ocean theme must be available");
    HighlightAssets { syntaxes, theme }
});

fn syntax_for_path<'a>(syntaxes: &'a SyntaxSet, path: &Path) -> &'a SyntaxReference {
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| syntaxes.find_syntax_by_extension(extension))
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| syntaxes.find_syntax_by_extension(name))
        })
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
}

fn gpui_style(style: SyntectStyle) -> HighlightStyle {
    let foreground = style.foreground;
    let mut highlight = HighlightStyle {
        color: Some(
            rgb((u32::from(foreground.r) << 16)
                | (u32::from(foreground.g) << 8)
                | u32::from(foreground.b))
            .into(),
        ),
        ..HighlightStyle::default()
    };
    if style.font_style.contains(SyntectFontStyle::BOLD) {
        highlight.font_weight = Some(FontWeight::BOLD);
    }
    if style.font_style.contains(SyntectFontStyle::ITALIC) {
        highlight.font_style = Some(FontStyle::Italic);
    }
    if style.font_style.contains(SyntectFontStyle::UNDERLINE) {
        highlight.underline = Some(UnderlineStyle {
            thickness: px(1.),
            ..UnderlineStyle::default()
        });
    }
    highlight
}

fn push_syntax_highlights(
    highlighter: &mut HighlightLines<'_>,
    source: &str,
    source_offset: usize,
    syntaxes: &SyntaxSet,
    background: Option<u32>,
    highlights: &mut Vec<(std::ops::Range<usize>, HighlightStyle)>,
) -> Result<(), String> {
    let spans = highlighter
        .highlight_line(source, syntaxes)
        .map_err(|error| error.to_string())?;
    let mut offset = source_offset;
    for (style, text) in spans {
        let end = offset + text.len();
        if !text.is_empty() {
            let mut style = gpui_style(style);
            style.background_color = background.map(|color| rgba(color).into());
            highlights.push((offset..end, style));
        }
        offset = end;
    }
    Ok(())
}

fn advance_highlighter(
    highlighter: &mut HighlightLines<'_>,
    source: &str,
    syntaxes: &SyntaxSet,
) -> Result<(), String> {
    highlighter
        .highlight_line(source, syntaxes)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn line_highlight(color: u32, background: Option<u32>) -> HighlightStyle {
    HighlightStyle {
        color: Some(rgb(color).into()),
        background_color: background.map(|color| rgba(color).into()),
        ..HighlightStyle::default()
    }
}

/// Highlight a unified diff while maintaining independent parser state for the
/// old and new versions represented by each hunk.
pub(crate) fn diff_highlights(
    path: &Path,
    diff: &str,
) -> Result<Vec<(std::ops::Range<usize>, HighlightStyle)>, String> {
    let assets = &*ASSETS;
    let syntax = syntax_for_path(&assets.syntaxes, path);
    let mut old = HighlightLines::new(syntax, &assets.theme);
    let mut new = HighlightLines::new(syntax, &assets.theme);
    let mut highlights = Vec::new();
    let mut line_offset = 0;

    for line in diff.split_inclusive('\n') {
        let line_end = line_offset + line.len();
        let content = line.strip_suffix('\n').unwrap_or(line);

        if content.starts_with("@@") {
            old = HighlightLines::new(syntax, &assets.theme);
            new = HighlightLines::new(syntax, &assets.theme);
            highlights.push((line_offset..line_end, line_highlight(BLUE, None)));
        } else if content.starts_with("diff ")
            || content.starts_with("index ")
            || content.starts_with("--- ")
            || content.starts_with("+++ ")
            || content.starts_with("\\ No newline")
        {
            highlights.push((line_offset..line_end, line_highlight(MUTED, None)));
        } else if let Some(source) = line.strip_prefix('+') {
            highlights.push((
                line_offset..line_offset + 1,
                line_highlight(GREEN, Some(ADDED_BACKGROUND)),
            ));
            push_syntax_highlights(
                &mut new,
                source,
                line_offset + 1,
                &assets.syntaxes,
                Some(ADDED_BACKGROUND),
                &mut highlights,
            )?;
        } else if let Some(source) = line.strip_prefix('-') {
            highlights.push((
                line_offset..line_offset + 1,
                line_highlight(RED, Some(DELETED_BACKGROUND)),
            ));
            push_syntax_highlights(
                &mut old,
                source,
                line_offset + 1,
                &assets.syntaxes,
                Some(DELETED_BACKGROUND),
                &mut highlights,
            )?;
        } else {
            let (source, source_offset) = line
                .strip_prefix(' ')
                .map_or((line, line_offset), |source| (source, line_offset + 1));
            push_syntax_highlights(
                &mut old,
                source,
                source_offset,
                &assets.syntaxes,
                None,
                &mut highlights,
            )?;
            advance_highlighter(&mut new, source, &assets.syntaxes)?;
        }

        line_offset = line_end;
    }

    Ok(highlights)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_diff_structure_and_rust_syntax() {
        let diff = "@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n";
        let highlights = diff_highlights(Path::new("src/lib.rs"), diff).unwrap();

        let added_prefix = diff.find("+fn").unwrap();
        let deleted_prefix = diff.find("-fn").unwrap();
        assert!(highlights.iter().any(|(range, style)| {
            range == &(added_prefix..added_prefix + 1)
                && style.color.is_some()
                && style.background_color.is_some()
        }));
        assert!(highlights.iter().any(|(range, style)| {
            range == &(deleted_prefix..deleted_prefix + 1)
                && style.color.is_some()
                && style.background_color.is_some()
        }));
        assert!(
            highlights
                .iter()
                .any(|(range, _)| &diff[range.clone()] == "fn"),
            "Rust keywords should receive syntax highlighting"
        );
        assert!(
            highlights
                .windows(2)
                .all(|pair| pair[0].0.end <= pair[1].0.start),
            "GPUI highlight ranges must be ordered and non-overlapping"
        );
    }

    #[test]
    fn does_not_treat_file_headers_as_code_changes() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n";
        let highlights = diff_highlights(Path::new("src/lib.rs"), diff).unwrap();

        assert!(
            highlights
                .iter()
                .all(|(_, style)| style.background_color.is_none())
        );
    }
}
