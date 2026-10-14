//! Matrix HTML (`org.matrix.custom.html`) -> styled terminal text.
//!
//! The HTML is flattened into spans: inline styles become marks, block
//! structure (paragraphs, quotes, lists, code blocks) becomes newlines and
//! line prefixes. Only text and styles ever reach the terminal, so unknown
//! tags simply contribute their text; reply fallbacks and non-content tags
//! (scripts, styles) are dropped.

use ruma_html::{
    Html, NodeData, NodeRef,
    matrix::{AnchorUri, MatrixElement},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub link: bool,
    pub mention: bool,
    pub spoiler: bool,
    /// Structure the renderer adds (quote bars, bullets, link targets).
    pub dim: bool,
    pub color: Option<(u8, u8, u8)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub marks: Marks,
}

/// Styled spans for a formatted message, or `None` when the formatting adds
/// nothing over the plain body.
pub fn render(html: &str) -> Option<Vec<Span>> {
    let doc = Html::parse(html);
    let mut writer = Writer::default();
    for node in doc.children() {
        writer.node(&node, Marks::default());
    }
    let spans = writer.finish();
    spans.iter().any(|s| s.marks != Marks::default() || s.text.contains('\n')).then_some(spans)
}

/// The plain text of rendered spans (for snippets and search).
pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

struct List {
    ordered: bool,
    next: i64,
}

#[derive(Default)]
struct Writer {
    spans: Vec<Span>,
    /// Nothing written on the current line yet.
    line_start: bool,
    started: bool,
    /// A block ended; start a new line before the next text.
    pending_break: bool,
    quote: usize,
    pre: usize,
    lists: Vec<List>,
    /// The bullet for the first line of the current list item.
    bullet: Option<String>,
    /// Columns that later lines of a list item are indented by.
    item_indent: usize,
}

impl Writer {
    fn push(&mut self, text: &str, marks: Marks) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some(last) if last.marks == marks => last.text.push_str(text),
            _ => self.spans.push(Span { text: text.to_owned(), marks }),
        }
    }

    fn block_break(&mut self) {
        if self.started {
            self.pending_break = true;
        }
    }

    fn newline(&mut self) {
        self.push("\n", Marks::default());
        self.line_start = true;
        self.pending_break = false;
    }

    fn prefix(&mut self) {
        let dim = Marks { dim: true, ..Marks::default() };
        for _ in 0..self.quote {
            self.push("▎ ", dim);
        }
        let depth = self.lists.len().saturating_sub(1);
        match self.bullet.take() {
            Some(bullet) => {
                self.push(&"  ".repeat(depth), Marks::default());
                self.item_indent = depth * 2 + bullet.chars().count();
                self.push(&bullet, dim);
            }
            None if !self.lists.is_empty() => self.push(&" ".repeat(self.item_indent), Marks::default()),
            None => {}
        }
        if self.pre > 0 {
            self.push("  ", Marks::default());
        }
    }

    fn text(&mut self, raw: &str, marks: Marks) {
        let text = if self.pre > 0 { raw.to_owned() } else { collapse_whitespace(raw) };
        let text = if self.line_start || !self.started || self.pending_break {
            text.trim_start_matches(' ').to_owned()
        } else {
            text
        };
        if text.is_empty() {
            return;
        }
        if self.pending_break {
            self.newline();
        }
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            if piece.is_empty() {
                continue;
            }
            if self.line_start || !self.started {
                self.prefix();
            }
            self.push(piece, marks);
            self.line_start = false;
            self.started = true;
        }
    }

    fn children(&mut self, node: &NodeRef, marks: Marks) {
        for child in node.children() {
            self.node(&child, marks);
        }
    }

    fn node(&mut self, node: &NodeRef, marks: Marks) {
        let element = match node.data() {
            NodeData::Text(text) => return self.text(&text.borrow(), marks),
            NodeData::Element(element) => element.to_matrix().element,
            _ => return self.children(node, marks),
        };
        let mut m = marks;
        match element {
            MatrixElement::B | MatrixElement::Strong => m.bold = true,
            MatrixElement::I | MatrixElement::Em => m.italic = true,
            MatrixElement::S | MatrixElement::Del => m.strike = true,
            MatrixElement::U => m.underline = true,
            MatrixElement::Code(_) => m.code = true,
            MatrixElement::Span(span) => {
                m.spoiler |= span.spoiler.is_some();
                if let Some(color) = span.color.as_deref().and_then(parse_color) {
                    m.color = Some(color);
                }
            }
            MatrixElement::A(anchor) => {
                match anchor.href {
                    Some(AnchorUri::MatrixTo(_) | AnchorUri::Matrix(_)) => m.mention = true,
                    Some(AnchorUri::Other(url)) => {
                        m.link = true;
                        let label = node_text(node);
                        self.children(node, m);
                        let shown = label.trim().trim_end_matches('/');
                        let bare = url.split_once("://").map_or(url.as_ref(), |(_, rest)| rest).trim_end_matches('/');
                        if !shown.is_empty() && shown != url.trim_end_matches('/') && shown != bare {
                            self.text(&format!(" ({url})"), Marks { dim: true, ..marks });
                        }
                        return;
                    }
                    _ => {}
                }
            }
            MatrixElement::H(_) => {
                self.block_break();
                self.children(node, Marks { bold: true, ..m });
                return self.block_break();
            }
            MatrixElement::P | MatrixElement::Div(_) | MatrixElement::Details | MatrixElement::Caption => {
                self.block_break();
                self.children(node, m);
                return self.block_break();
            }
            MatrixElement::Summary => {
                self.block_break();
                self.children(node, Marks { bold: true, ..m });
                return self.block_break();
            }
            MatrixElement::Blockquote => {
                self.block_break();
                self.quote += 1;
                self.children(node, m);
                self.quote -= 1;
                return self.block_break();
            }
            MatrixElement::Ul | MatrixElement::Ol(_) => {
                let list = match element {
                    MatrixElement::Ol(data) => List { ordered: true, next: data.start.unwrap_or(1) },
                    _ => List { ordered: false, next: 1 },
                };
                self.block_break();
                self.lists.push(list);
                self.children(node, m);
                self.lists.pop();
                return self.block_break();
            }
            MatrixElement::Li => {
                self.block_break();
                if let Some(list) = self.lists.last_mut() {
                    self.bullet = Some(if list.ordered { format!("{}. ", list.next) } else { "• ".to_owned() });
                    list.next += 1;
                }
                self.children(node, m);
                self.bullet = None;
                return self.block_break();
            }
            MatrixElement::Pre => {
                self.block_break();
                self.pre += 1;
                let code = node_text(node);
                self.text(code.trim_end_matches('\n'), Marks { code: true, ..m });
                self.pre -= 1;
                return self.block_break();
            }
            MatrixElement::Br => return self.newline(),
            MatrixElement::Hr => {
                self.block_break();
                self.text("──────────", Marks { dim: true, ..m });
                return self.block_break();
            }
            MatrixElement::Img(img) => {
                let alt = img.alt.as_deref().or(img.title.as_deref()).unwrap_or("image");
                return self.text(&format!("[{alt}]"), Marks { dim: true, ..m });
            }
            MatrixElement::Tr => {
                self.block_break();
                self.children(node, m);
                return self.block_break();
            }
            MatrixElement::Td | MatrixElement::Th => {
                if !self.line_start && !self.pending_break && self.started {
                    self.text(" │ ", Marks { dim: true, ..m });
                }
                if matches!(element, MatrixElement::Th) {
                    m.bold = true;
                }
            }
            MatrixElement::MatrixReply => return,
            MatrixElement::Other(name)
                if matches!(name.local.as_ref(), "script" | "style" | "head" | "title" | "template" | "noscript") =>
            {
                return;
            }
            _ => {}
        }
        self.children(node, m);
    }

    fn finish(mut self) -> Vec<Span> {
        while let Some(last) = self.spans.last_mut() {
            let trimmed = last.text.trim_end_matches('\n').len();
            if trimmed == 0 {
                self.spans.pop();
            } else {
                last.text.truncate(trimmed);
                break;
            }
        }
        self.spans
    }
}

fn node_text(node: &NodeRef) -> String {
    let mut out = String::new();
    for child in node.children() {
        match child.data() {
            NodeData::Text(text) => out.push_str(&text.borrow()),
            NodeData::Element(element) if element.name.local.as_ref() == "br" => out.push('\n'),
            _ => out.push_str(&node_text(&child)),
        }
    }
    out
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = true;
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    if space {
        out.push(' ');
    }
    out
}

fn parse_color(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(html: &str) -> String {
        plain(&render(html).unwrap_or_default())
    }

    fn marked(html: &str, pick: fn(&Marks) -> bool) -> Vec<String> {
        render(html).unwrap().into_iter().filter(|s| pick(&s.marks)).map(|s| s.text).collect()
    }

    #[test]
    fn inline_styles_become_marks() {
        let html = "plain <b>bold</b> <em>it</em> <del>gone</del> <code>x = 1</code>";
        assert_eq!(text_of(html), "plain bold it gone x = 1");
        assert_eq!(marked(html, |m| m.bold), vec!["bold"]);
        assert_eq!(marked(html, |m| m.italic), vec!["it"]);
        assert_eq!(marked(html, |m| m.strike), vec!["gone"]);
        assert_eq!(marked(html, |m| m.code), vec!["x = 1"]);
    }

    #[test]
    fn plain_html_needs_no_rich_rendering() {
        assert!(render("just text").is_none());
    }

    #[test]
    fn blocks_become_lines_with_prefixes() {
        assert_eq!(text_of("<p>one</p>\n<p>two</p>"), "one\ntwo");
        assert_eq!(text_of("<blockquote>\n<p>quoted</p>\n</blockquote>\n<p>reply</p>"), "▎ quoted\nreply");
        assert_eq!(text_of("<ul>\n<li>a</li>\n<li>b</li>\n</ul>"), "• a\n• b");
        assert_eq!(text_of("<ol start=\"3\"><li>c</li><li>d</li></ol>"), "3. c\n4. d");
        assert_eq!(text_of("line<br>break"), "line\nbreak");
    }

    #[test]
    fn code_blocks_keep_whitespace() {
        let html = "<pre><code class=\"language-rust\">fn main() {\n    println!();\n}\n</code></pre>";
        assert_eq!(text_of(html), "  fn main() {\n      println!();\n  }");
        assert!(render(html).unwrap().iter().filter(|s| !s.text.trim().is_empty()).all(|s| s.marks.code));
    }

    #[test]
    fn links_mentions_and_spoilers() {
        let html = r#"hi <a href="https://matrix.to/#/@bob:x.org">Bob</a>, see <a href="https://example.org">the docs</a> and <span data-mx-spoiler="">it dies</span>"#;
        assert_eq!(text_of(html), "hi Bob, see the docs (https://example.org) and it dies");
        assert_eq!(marked(html, |m| m.mention), vec!["Bob"]);
        assert_eq!(marked(html, |m| m.link), vec!["the docs"]);
        assert_eq!(marked(html, |m| m.spoiler), vec!["it dies"]);
        // A bare link isn't repeated.
        assert_eq!(text_of(r#"<a href="https://example.org">https://example.org</a>"#), "https://example.org");
    }

    #[test]
    fn reply_fallbacks_and_unsafe_tags_are_dropped() {
        let html = "<mx-reply><blockquote>old</blockquote></mx-reply><script>x()</script><b>new</b>";
        assert_eq!(text_of(html), "new");
    }
}
