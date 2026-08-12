use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MarkdownDocument {
    pub(crate) children: Vec<MarkdownNode>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MarkdownNode {
    Container(MarkdownTag, Vec<Self>),
    Text(String),
    Code(String),
    Html(String),
    SoftBreak,
    HardBreak,
    Rule,
    TaskMarker(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MarkdownTag {
    Paragraph,
    Heading(u8),
    BlockQuote,
    CodeBlock(Option<String>),
    List(Option<u64>),
    Item,
    Table,
    TableHead,
    TableRow,
    TableCell,
    Emphasis,
    Strong,
    Strikethrough,
    Link(String),
    Image(String),
    Other,
}

pub(crate) fn parse(source: &str) -> MarkdownDocument {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    let mut stack = vec![(MarkdownTag::Other, Vec::new())];

    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => stack.push((markdown_tag(tag), Vec::new())),
            Event::End(_) => {
                if stack.len() > 1 {
                    let (tag, children) = stack.pop().expect("markdown stack is non-empty");
                    stack
                        .last_mut()
                        .expect("markdown root remains")
                        .1
                        .push(MarkdownNode::Container(tag, children));
                }
            }
            Event::Text(text) => push(&mut stack, MarkdownNode::Text(text.into_string())),
            Event::Code(text) => push(&mut stack, MarkdownNode::Code(text.into_string())),
            Event::Html(html) | Event::InlineHtml(html) => {
                // Raw HTML is deliberately retained as inert text. GPUI never
                // interprets it, so scripts and event attributes cannot run.
                push(&mut stack, MarkdownNode::Html(html.into_string()));
            }
            Event::SoftBreak => push(&mut stack, MarkdownNode::SoftBreak),
            Event::HardBreak => push(&mut stack, MarkdownNode::HardBreak),
            Event::Rule => push(&mut stack, MarkdownNode::Rule),
            Event::TaskListMarker(checked) => {
                push(&mut stack, MarkdownNode::TaskMarker(checked));
            }
            Event::FootnoteReference(label) => {
                push(&mut stack, MarkdownNode::Text(format!("[^{label}]")));
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                push(&mut stack, MarkdownNode::Code(text.into_string()));
            }
        }
    }

    // Pulldown-cmark normally balances partial input, including an unfinished
    // code fence. Keep this fallback so future parser behavior cannot discard
    // streaming content.
    while stack.len() > 1 {
        let (tag, children) = stack.pop().expect("markdown stack is non-empty");
        stack
            .last_mut()
            .expect("markdown root remains")
            .1
            .push(MarkdownNode::Container(tag, children));
    }

    let mut children = stack.pop().expect("markdown root exists").1;
    autolink_nodes(&mut children, false);

    MarkdownDocument { children }
}

/// Detects bare `https://`/`http://` URLs inside plain text runs and rewrites
/// them as `Link` containers, matching the Markdown-link autolinking behavior
/// GCABB's renderer already applies to explicit `[label](url)` syntax.
///
/// Text already nested inside a `Link` tag is left untouched so link labels
/// are never re-linked or duplicated.
fn autolink_nodes(nodes: &mut Vec<MarkdownNode>, inside_link: bool) {
    let mut index = 0;
    while index < nodes.len() {
        match &mut nodes[index] {
            MarkdownNode::Container(tag, children) => {
                let now_inside_link = inside_link || matches!(tag, MarkdownTag::Link(_));
                autolink_nodes(children, now_inside_link);
                index += 1;
            }
            MarkdownNode::Text(text) if !inside_link => {
                let split = split_autolinks(text);
                let inserted = split.len();
                nodes.splice(index..=index, split);
                // None of the freshly inserted nodes need further
                // autolinking: plain `Text` runs already went through
                // `split_autolinks`, and `Link` containers wrap literal URL
                // text that must not be re-linked.
                index += inserted;
            }
            _ => index += 1,
        }
    }
}

/// Splits `text` into a run of `Text` and autolinked `Link` container nodes.
///
/// Trailing punctuation adjacent to a URL (closing brackets, sentence-ending
/// punctuation) is treated as part of the surrounding sentence rather than
/// the link target, matching common Markdown autolink conventions.
fn split_autolinks(text: &str) -> Vec<MarkdownNode> {
    let mut nodes = Vec::new();
    let mut rest = text;
    let mut plain = String::new();

    while let Some(rel_start) = find_url_start(rest) {
        let candidate = &rest[rel_start..];
        let url_len = url_extent(candidate);
        if url_len == 0 {
            // Not actually a valid autolink boundary; keep scanning past it
            // as plain text so we don't loop forever on the same position.
            plain.push_str(&rest[..=rel_start]);
            rest = &rest[rel_start + 1..];
            continue;
        }

        plain.push_str(&rest[..rel_start]);
        if !plain.is_empty() {
            nodes.push(MarkdownNode::Text(std::mem::take(&mut plain)));
        }

        let url = &candidate[..url_len];
        nodes.push(MarkdownNode::Container(
            MarkdownTag::Link(url.to_owned()),
            vec![MarkdownNode::Text(url.to_owned())],
        ));
        rest = &candidate[url_len..];
    }

    plain.push_str(rest);
    if !plain.is_empty() || nodes.is_empty() {
        nodes.push(MarkdownNode::Text(plain));
    }
    nodes
}

/// Finds the byte offset of the next `http://`/`https://` scheme in `text`
/// that begins at the start of a word (start of string or after whitespace
/// or an opening bracket), so URLs embedded in things like `foo:https://` are
/// not mistakenly treated as autolinks.
fn find_url_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut search_from = 0;
    while let Some(offset) = text[search_from..]
        .find("http://")
        .into_iter()
        .chain(text[search_from..].find("https://"))
        .min()
    {
        let start = search_from + offset;
        let boundary_ok = start == 0
            || matches!(
                bytes[start - 1],
                b' ' | b'\t' | b'\n' | b'(' | b'[' | b'{' | b'*' | b'_' | b'\'' | b'"'
            );
        if boundary_ok {
            return Some(start);
        }
        search_from = start + 1;
    }
    None
}

/// Returns the byte length of the URL starting at the beginning of
/// `candidate`, stopping at whitespace and stripping trailing punctuation
/// that is more likely to belong to the surrounding sentence than the link.
fn url_extent(candidate: &str) -> usize {
    let end = candidate
        .find(|c: char| c.is_whitespace())
        .unwrap_or(candidate.len());
    let mut url = &candidate[..end];

    // A URL must have something after the scheme to be worth linking.
    let scheme_len = if url.starts_with("https://") { 8 } else { 7 };
    if url.len() <= scheme_len {
        return 0;
    }

    // Trim trailing punctuation unless it balances an opening bracket that
    // is part of the URL itself (e.g. Wikipedia-style `(disambiguation)`
    // URLs).
    while let Some(last) = url.chars().last() {
        let should_trim = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => url.matches('(').count() < url.matches(')').count(),
            ']' => url.matches('[').count() < url.matches(']').count(),
            _ => false,
        };
        if !should_trim {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }

    if url.len() > scheme_len { url.len() } else { 0 }
}

fn push(stack: &mut [(MarkdownTag, Vec<MarkdownNode>)], node: MarkdownNode) {
    stack.last_mut().expect("markdown root exists").1.push(node);
}

fn markdown_tag(tag: Tag<'_>) -> MarkdownTag {
    match tag {
        Tag::Paragraph => MarkdownTag::Paragraph,
        Tag::Heading { level, .. } => MarkdownTag::Heading(heading_level(level)),
        Tag::BlockQuote(_) => MarkdownTag::BlockQuote,
        Tag::CodeBlock(kind) => MarkdownTag::CodeBlock(match kind {
            CodeBlockKind::Indented => None,
            CodeBlockKind::Fenced(language) => {
                let language = language.trim();
                (!language.is_empty()).then(|| language.to_owned())
            }
        }),
        Tag::List(start) => MarkdownTag::List(start),
        Tag::Item => MarkdownTag::Item,
        Tag::Table(_) => MarkdownTag::Table,
        Tag::TableHead => MarkdownTag::TableHead,
        Tag::TableRow => MarkdownTag::TableRow,
        Tag::TableCell => MarkdownTag::TableCell,
        Tag::Emphasis => MarkdownTag::Emphasis,
        Tag::Strong => MarkdownTag::Strong,
        Tag::Strikethrough => MarkdownTag::Strikethrough,
        Tag::Link { dest_url, .. } => MarkdownTag::Link(dest_url.into_string()),
        Tag::Image { dest_url, .. } => MarkdownTag::Image(dest_url.into_string()),
        _ => MarkdownTag::Other,
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

pub(crate) fn plain_text(nodes: &[MarkdownNode]) -> String {
    let mut text = String::new();
    for node in nodes {
        match node {
            MarkdownNode::Container(_, children) => text.push_str(&plain_text(children)),
            MarkdownNode::Text(value) | MarkdownNode::Code(value) | MarkdownNode::Html(value) => {
                text.push_str(value);
            }
            MarkdownNode::SoftBreak | MarkdownNode::HardBreak => text.push('\n'),
            MarkdownNode::Rule => text.push_str("---"),
            MarkdownNode::TaskMarker(checked) => {
                text.push_str(if *checked { "[x] " } else { "[ ] " });
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{MarkdownNode, MarkdownTag, parse, plain_text};

    #[test]
    fn parses_gfm_table_and_inline_formatting() {
        let document = parse("| A | B |\n|---|---|\n| **one** | ~~two~~ |");
        assert!(matches!(
            document.children.as_slice(),
            [MarkdownNode::Container(MarkdownTag::Table, _)]
        ));
        assert_eq!(plain_text(&document.children), "ABonetwo");
    }

    #[test]
    fn keeps_unfinished_streaming_code_fence() {
        let document = parse("before\n\n```rust\nfn main() {");
        assert!(plain_text(&document.children).contains("fn main() {"));
    }

    #[test]
    fn retains_raw_html_as_inert_text() {
        let document = parse("<script>alert('no')</script>");
        assert_eq!(
            plain_text(&document.children),
            "<script>alert('no')</script>"
        );
    }

    fn links(nodes: &[MarkdownNode]) -> Vec<(String, String)> {
        let mut found = Vec::new();
        for node in nodes {
            if let MarkdownNode::Container(tag, children) = node {
                if let MarkdownTag::Link(target) = tag {
                    found.push((target.clone(), plain_text(children)));
                }
                found.extend(links(children));
            }
        }
        found
    }

    #[test]
    fn autolinks_explicit_markdown_link() {
        let document = parse("see [docs](https://example.com/docs) now");
        assert_eq!(
            links(&document.children),
            vec![("https://example.com/docs".to_owned(), "docs".to_owned())]
        );
    }

    #[test]
    fn autolinks_bare_url_in_plain_text() {
        let document = parse("visit https://example.com for more");
        assert_eq!(
            links(&document.children),
            vec![(
                "https://example.com".to_owned(),
                "https://example.com".to_owned()
            )]
        );
        assert_eq!(
            plain_text(&document.children),
            "visit https://example.com for more"
        );
    }

    #[test]
    fn autolinks_multiple_bare_urls() {
        let document = parse("http://a.example and https://b.example are both fine");
        assert_eq!(
            links(&document.children),
            vec![
                ("http://a.example".to_owned(), "http://a.example".to_owned()),
                (
                    "https://b.example".to_owned(),
                    "https://b.example".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn does_not_autolink_bare_url_inside_markdown_link_label() {
        let document = parse("[see https://a.example here](https://b.example)");
        assert_eq!(
            links(&document.children),
            vec![(
                "https://b.example".to_owned(),
                "see https://a.example here".to_owned()
            )]
        );
    }

    #[test]
    fn trims_trailing_punctuation_from_bare_url() {
        let document = parse("check this out: https://example.com/page.");
        assert_eq!(
            links(&document.children),
            vec![(
                "https://example.com/page".to_owned(),
                "https://example.com/page".to_owned()
            )]
        );
        assert!(plain_text(&document.children).ends_with('.'));
    }

    #[test]
    fn keeps_balanced_trailing_parenthesis_in_bare_url() {
        let document = parse("see (https://example.com/page) here");
        assert_eq!(
            links(&document.children),
            vec![(
                "https://example.com/page".to_owned(),
                "https://example.com/page".to_owned()
            )]
        );
    }

    #[test]
    fn unsafe_scheme_link_target_is_rejected_by_render_time_filter() {
        // `parse` preserves every explicit Markdown link target as-is; it is
        // the renderer's `safe_markdown_url` allowlist (exercised in
        // `main.rs`) that keeps unsafe schemes inert at click time. This
        // confirms the parser still recognizes the link node so the
        // renderer has something to filter.
        let document = parse("[danger](javascript:alert(1))");
        assert_eq!(
            links(&document.children),
            vec![("javascript:alert(1)".to_owned(), "danger".to_owned())]
        );
    }

    #[test]
    fn does_not_autolink_url_without_boundary() {
        let document = parse("foohttps://example.com bar");
        assert_eq!(links(&document.children), Vec::new());
        assert_eq!(plain_text(&document.children), "foohttps://example.com bar");
    }

    #[test]
    fn does_not_autolink_scheme_with_only_trailing_punctuation() {
        let document = parse("not a URL: https://...");
        assert_eq!(links(&document.children), Vec::new());
        assert_eq!(plain_text(&document.children), "not a URL: https://...");
    }

    #[test]
    fn autolinks_bare_url_inside_quotes() {
        let document = parse(r#"visit "https://example.com" now"#);
        assert_eq!(
            links(&document.children),
            vec![(
                "https://example.com".to_owned(),
                "https://example.com".to_owned()
            )]
        );
    }
}
