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

    MarkdownDocument {
        children: stack.pop().expect("markdown root exists").1,
    }
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
}
