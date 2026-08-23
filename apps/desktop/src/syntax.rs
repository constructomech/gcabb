use std::path::Path;
use std::sync::LazyLock;

use gpui::{FontStyle, FontWeight, HighlightStyle, UnderlineStyle, px, rgb};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    FontStyle as SyntectFontStyle, Style as SyntectStyle, Theme, ThemeSet,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

const BLUE: u32 = 0x0058_a6ff;
const GREEN: u32 = 0x003f_b950;
const MUTED: u32 = 0x008b_949e;
const RED: u32 = 0x00f8_5161;
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

#[derive(Clone, Copy)]
enum DiffSide {
    Old,
    New,
    Both,
}

fn diff_header_path(line: &str, in_hunk: bool) -> Option<(DiffSide, &Path)> {
    let mut headers = [
        ("*** Update File: ", DiffSide::Both),
        ("*** Add File: ", DiffSide::Both),
        ("*** Delete File: ", DiffSide::Both),
        ("*** Move to: ", DiffSide::New),
        ("--- ", DiffSide::Old),
        ("+++ ", DiffSide::New),
    ]
    .into_iter();
    let (side, path) = headers.find_map(|(prefix, side)| {
        (!in_hunk || !matches!(side, DiffSide::Old | DiffSide::New))
            .then(|| line.strip_prefix(prefix).map(|path| (side, path)))
            .flatten()
    })?;
    let path = path.split_once('\t').map_or(path, |(path, _)| path);
    if path == "/dev/null" {
        return None;
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .trim_matches('"');
    Some((side, Path::new(path)))
}

fn is_diff_metadata(line: &str, in_hunk: bool) -> bool {
    line.starts_with("diff ")
        || line.starts_with("index ")
        || (!in_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")))
        || line.starts_with("*** Begin Patch")
        || line.starts_with("*** End Patch")
        || line.starts_with("*** Update File: ")
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Move to: ")
        || line.starts_with("\\ No newline")
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
    highlights: &mut Vec<(std::ops::Range<usize>, HighlightStyle)>,
) -> Result<(), String> {
    let spans = highlighter
        .highlight_line(source, syntaxes)
        .map_err(|error| error.to_string())?;
    let mut offset = source_offset;
    for (style, text) in spans {
        let end = offset + text.len();
        if !text.is_empty() {
            highlights.push((offset..end, gpui_style(style)));
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

fn line_highlight(color: u32) -> HighlightStyle {
    HighlightStyle {
        color: Some(rgb(color).into()),
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
    let fallback_syntax = syntax_for_path(&assets.syntaxes, path);
    let mut old_syntax = fallback_syntax;
    let mut new_syntax = fallback_syntax;
    let mut old = HighlightLines::new(old_syntax, &assets.theme);
    let mut new = HighlightLines::new(new_syntax, &assets.theme);
    let mut highlights = Vec::new();
    let mut line_offset = 0;
    let mut in_hunk = false;

    for line in diff.split_inclusive('\n') {
        let line_end = line_offset + line.len();
        let content = line.strip_suffix('\n').unwrap_or(line);

        if content.starts_with("diff ")
            || content.starts_with("*** Update File: ")
            || content.starts_with("*** Add File: ")
            || content.starts_with("*** Delete File: ")
        {
            in_hunk = false;
        }
        if let Some((side, header_path)) = diff_header_path(content, in_hunk) {
            let header_syntax = syntax_for_path(&assets.syntaxes, header_path);
            match side {
                DiffSide::Old => {
                    old_syntax = header_syntax;
                    old = HighlightLines::new(old_syntax, &assets.theme);
                }
                DiffSide::New => {
                    new_syntax = header_syntax;
                    new = HighlightLines::new(new_syntax, &assets.theme);
                }
                DiffSide::Both => {
                    old_syntax = header_syntax;
                    new_syntax = header_syntax;
                    old = HighlightLines::new(old_syntax, &assets.theme);
                    new = HighlightLines::new(new_syntax, &assets.theme);
                }
            }
        }

        if content.starts_with("@@") {
            in_hunk = true;
            old = HighlightLines::new(old_syntax, &assets.theme);
            new = HighlightLines::new(new_syntax, &assets.theme);
            highlights.push((line_offset..line_end, line_highlight(BLUE)));
        } else if is_diff_metadata(content, in_hunk) {
            highlights.push((line_offset..line_end, line_highlight(MUTED)));
        } else if let Some(source) = line.strip_prefix('+') {
            in_hunk = true;
            highlights.push((line_offset..line_offset + 1, line_highlight(GREEN)));
            push_syntax_highlights(
                &mut new,
                source,
                line_offset + 1,
                &assets.syntaxes,
                &mut highlights,
            )?;
        } else if let Some(source) = line.strip_prefix('-') {
            in_hunk = true;
            highlights.push((line_offset..line_offset + 1, line_highlight(RED)));
            push_syntax_highlights(
                &mut old,
                source,
                line_offset + 1,
                &assets.syntaxes,
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
            range == &(added_prefix..added_prefix + 1) && style.color.is_some()
        }));
        assert!(highlights.iter().any(|(range, style)| {
            range == &(deleted_prefix..deleted_prefix + 1) && style.color.is_some()
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

    #[test]
    fn apply_patch_headers_select_the_affected_file_language() {
        let diff = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn old() {}\n+pub fn new() {}\n*** End Patch\n";
        let highlights = diff_highlights(Path::new(""), diff).unwrap();

        assert!(
            highlights
                .iter()
                .any(|(range, _)| &diff[range.clone()] == "fn"),
            "the path in an apply_patch header should select Rust syntax"
        );
        let header_start = diff.find("*** Update File").unwrap();
        assert!(highlights.iter().any(|(range, _)| {
            range.start == header_start && &diff[range.clone()] == "*** Update File: src/lib.rs\n"
        }));
    }

    #[test]
    fn switches_languages_between_files_in_one_patch() {
        let diff = "*** Update File: src/lib.rs\n@@\n+pub fn run() {}\n*** Update File: web/app.js\n@@\n+const ready = true;\n";
        let highlights = diff_highlights(Path::new(""), diff).unwrap();

        for token in ["fn", "const"] {
            assert!(
                highlights
                    .iter()
                    .any(|(range, _)| &diff[range.clone()] == token),
                "{token} should be highlighted using its file's syntax"
            );
        }
    }

    #[test]
    fn header_like_source_lines_remain_code_inside_hunks() {
        let diff =
            "--- a/query.sql\n+++ b/query.sql\n@@ -1 +1 @@\n--- old comment\n+++ new comment\n";
        let highlights = diff_highlights(Path::new(""), diff).unwrap();

        for source_line in ["--- old comment", "+++ new comment"] {
            let start = diff.find(source_line).unwrap();
            assert!(
                highlights
                    .iter()
                    .any(|(range, _)| { range.start == start && range.end == start + 1 })
            );
        }
    }
}
