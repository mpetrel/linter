//! Turns the source of one block into display data.
//!
//! Inline content is re-emitted as a restricted Markdown string that Slint's
//! `StyledText` understands (emphasis, strong, strike-through, code, links,
//! underline). Block structure (headings, lists, quotes, tables, code) is
//! described separately and laid out by the UI.

use crate::doc::{self, Kind, LinkRefs, parser};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Tag, TagEnd};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub indent: u32,
    /// Bullet or number shown in the marker column; " " reserves the column.
    pub marker: String,
    /// 0 = no checkbox, 1 = open task, 2 = done task.
    pub task: u8,
    /// Inline content as `StyledText` Markdown.
    pub md: String,
    /// The same content as plain text (fallback).
    pub plain: String,
    /// Code shown verbatim (code blocks nested in lists or quotes).
    pub code: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub md: String,
    pub plain: String,
    pub align: Align,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub header: bool,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rendered {
    pub kind: Kind,
    pub lines: Vec<Line>,
    pub code: String,
    pub lang: String,
    pub rows: Vec<Row>,
    /// Relative column widths for tables (sum to 1).
    pub col_weights: Vec<f32>,
    /// URL and alt text of an image block.
    pub image: Option<(String, String)>,
}

pub fn render(src: &str, refs: &LinkRefs) -> Rendered {
    let kind = doc::classify_with(src, refs);
    let mut out = Rendered {
        kind,
        lines: Vec::new(),
        code: String::new(),
        lang: String::new(),
        rows: Vec::new(),
        col_weights: Vec::new(),
        image: None,
    };
    match kind {
        Kind::Paragraph | Kind::Heading(_) | Kind::Quote | Kind::List | Kind::Footnote => {
            out.lines = Flow::new(kind).run(parser(src, refs));
        }
        Kind::Code { .. } => {
            for event in parser(src, refs) {
                match event {
                    Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                        out.lang = info.split_whitespace().next().unwrap_or("").to_owned();
                    }
                    Event::Text(t) => out.code.push_str(&t),
                    _ => {}
                }
            }
            if out.code.ends_with('\n') {
                out.code.pop();
            }
        }
        Kind::Html | Kind::Meta | Kind::Raw => {
            out.code = src.to_owned();
            out.lang = match kind {
                Kind::Html => "html",
                Kind::Meta => "front matter",
                _ => "",
            }
            .into();
        }
        Kind::Table => table(src, refs, &mut out),
        Kind::Image => {
            let mut events = parser(src, refs);
            if matches!(events.next(), Some(Event::Start(Tag::Paragraph))) {
                out.image = doc::lone_image(&mut events);
            }
        }
        Kind::Rule | Kind::Empty => {}
    }
    out
}

fn table<'a>(src: &'a str, refs: &'a LinkRefs, out: &mut Rendered) {
    let mut aligns: Vec<Alignment> = Vec::new();
    let mut row: Option<Row> = None;
    let mut cell: Option<Inline> = None;
    for event in parser(src, refs) {
        match event {
            Event::Start(Tag::Table(a)) => aligns = a,
            Event::Start(Tag::TableHead) => row = Some(Row { header: true, cells: Vec::new() }),
            Event::Start(Tag::TableRow) => row = Some(Row { header: false, cells: Vec::new() }),
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => out.rows.extend(row.take()),
            Event::Start(Tag::TableCell) => {
                let header = row.as_ref().is_some_and(|r| r.header);
                cell = Some(Inline { bold: header, ..Default::default() });
            }
            Event::End(TagEnd::TableCell) => {
                if let (Some(r), Some(c)) = (row.as_mut(), cell.take()) {
                    let align = match aligns.get(r.cells.len()) {
                        Some(Alignment::Center) => Align::Center,
                        Some(Alignment::Right) => Align::Right,
                        _ => Align::Left,
                    };
                    let (md, plain) = c.finish();
                    r.cells.push(Cell { md, plain, align });
                }
            }
            other => {
                if let Some(c) = cell.as_mut() {
                    c.event(other);
                }
            }
        }
    }

    let columns = out.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
    for r in &mut out.rows {
        while r.cells.len() < columns {
            r.cells.push(Cell { md: String::new(), plain: String::new(), align: Align::Left });
        }
    }
    let widths: Vec<f32> = (0..columns)
        .map(|c| {
            let longest = out.rows.iter().map(|r| r.cells[c].plain.chars().count()).max().unwrap_or(0);
            (longest as f32).clamp(4.0, 40.0)
        })
        .collect();
    let total: f32 = widths.iter().sum::<f32>().max(1.0);
    out.col_weights = widths.iter().map(|w| w / total).collect();
}

/// Walks prose-like blocks (paragraphs, headings, lists, quotes) and
/// produces one display line per paragraph / list item.
struct Flow {
    kind: Kind,
    lines: Vec<Line>,
    cur: Option<(Line, Inline)>,
    lists: Vec<Option<u64>>,
    pending_marker: Option<String>,
    pending_task: u8,
    quote_depth: u32,
    bold: bool,
    code: Option<String>,
}

impl Flow {
    fn new(kind: Kind) -> Self {
        Flow {
            kind,
            lines: Vec::new(),
            cur: None,
            lists: Vec::new(),
            pending_marker: None,
            pending_task: 0,
            quote_depth: 0,
            bold: false,
            code: None,
        }
    }

    fn run<'a>(mut self, events: impl Iterator<Item = Event<'a>>) -> Vec<Line> {
        for event in events {
            self.event(event);
        }
        self.flush();
        if self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.lines
    }

    fn indent(&self) -> u32 {
        let outer_quote = u32::from(self.kind == Kind::Quote);
        self.lists.len().saturating_sub(1) as u32 + self.quote_depth.saturating_sub(outer_quote)
    }

    fn begin_line(&mut self) -> &mut (Line, Inline) {
        if self.cur.is_none() {
            let marker = self
                .pending_marker
                .take()
                .unwrap_or_else(|| if self.lists.is_empty() { String::new() } else { " ".into() });
            let line = Line { indent: self.indent(), marker, task: std::mem::take(&mut self.pending_task), ..Default::default() };
            self.cur = Some((line, Inline { bold: self.bold, ..Default::default() }));
        }
        self.cur.as_mut().unwrap()
    }

    fn flush(&mut self) {
        if let Some((mut line, inline)) = self.cur.take() {
            let (md, plain) = inline.finish();
            line.md = md;
            line.plain = plain;
            self.lines.push(line);
        }
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some(code) = self.code.as_mut() {
            match event {
                Event::Text(t) | Event::Html(t) => code.push_str(&t),
                Event::End(TagEnd::CodeBlock | TagEnd::HtmlBlock) => {
                    let mut code = self.code.take().unwrap();
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    self.begin_line().0.code = Some(code);
                    self.flush();
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(Tag::Paragraph) | Event::End(TagEnd::Paragraph) => self.flush(),
            Event::Start(Tag::Heading { .. }) => {
                self.flush();
                self.bold = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                self.flush();
                self.bold = false;
            }
            Event::Start(Tag::List(start)) => {
                self.flush();
                self.lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                self.flush();
                self.lists.pop();
            }
            Event::Start(Tag::Item) => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}.");
                        *n += 1;
                        m
                    }
                    _ => ["•", "◦", "▪"][depth % 3].to_owned(),
                };
                self.pending_marker = Some(marker);
            }
            Event::End(TagEnd::Item) => {
                self.flush();
                if self.pending_marker.is_some() {
                    // Empty list item: still show its bullet.
                    self.begin_line();
                    self.flush();
                }
                self.pending_marker = None;
                self.pending_task = 0;
            }
            Event::TaskListMarker(done) => {
                let task = if done { 2 } else { 1 };
                match self.cur.as_mut() {
                    Some((line, _)) => line.task = task,
                    None => self.pending_task = task,
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush();
                self.quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock) => {
                self.flush();
                self.code = Some(String::new());
            }
            Event::Start(Tag::FootnoteDefinition(label)) => {
                self.flush();
                self.pending_marker = Some(format!("[{label}]"));
            }
            Event::End(TagEnd::FootnoteDefinition) => self.flush(),
            Event::Rule => {
                self.flush();
                let (_, inline) = self.begin_line();
                inline.text("———");
                self.flush();
            }
            Event::Start(Tag::Table(_)) | Event::End(TagEnd::Table) => self.flush(),
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => self.flush(),
            Event::Start(Tag::TableCell) => {
                let (_, inline) = self.begin_line();
                if !inline.plain.is_empty() {
                    inline.text(" | ");
                }
            }
            Event::Start(Tag::TableHead | Tag::TableRow) | Event::End(TagEnd::TableCell) => {}
            Event::Start(Tag::MetadataBlock(_)) | Event::End(TagEnd::MetadataBlock(_)) => {}
            other => self.begin_line().1.event(other),
        }
    }
}

/// Accumulates inline events into `StyledText` Markdown plus plain text.
#[derive(Default)]
struct Inline {
    md: String,
    plain: String,
    /// The whole run is bold (headings, table headers): `**` markers inside are dropped.
    bold: bool,
    links: Vec<String>,
    image_alt: Option<String>,
    underline_depth: i32,
}

impl Inline {
    fn text(&mut self, t: &str) {
        if let Some(alt) = self.image_alt.as_mut() {
            alt.push_str(t);
            return;
        }
        escape_into(&mut self.md, t);
        self.plain.push_str(t);
    }

    fn line_break(&mut self) {
        if self.image_alt.is_none() && !self.plain.is_empty() {
            self.md.push('\n');
            self.plain.push('\n');
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                if let Some(alt) = self.image_alt.as_mut() {
                    alt.push_str(&t);
                } else if !t.is_empty() {
                    push_code_span(&mut self.md, &t);
                    self.plain.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => self.line_break(),
            Event::Start(Tag::Emphasis) | Event::End(TagEnd::Emphasis) => self.marker("*"),
            Event::Start(Tag::Strong) | Event::End(TagEnd::Strong) => {
                if !self.bold {
                    self.marker("**");
                }
            }
            Event::Start(Tag::Strikethrough) | Event::End(TagEnd::Strikethrough) => self.marker("~~"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                if self.image_alt.is_none() {
                    self.md.push('[');
                    self.links.push(dest_url.to_string());
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some(url) = self.links.pop() {
                    self.md.push_str("](");
                    self.md.push_str(&encode_url(&url));
                    self.md.push(')');
                }
            }
            Event::Start(Tag::Image { .. }) => self.image_alt = Some(String::new()),
            Event::End(TagEnd::Image) => {
                let alt = self.image_alt.take().unwrap_or_default();
                let label = if alt.trim().is_empty() { "image".to_owned() } else { alt };
                self.text(&format!("[{label}]"));
            }
            Event::InlineHtml(html) => {
                let tag = html.trim().to_ascii_lowercase();
                match tag.as_str() {
                    "<u>" => {
                        self.underline_depth += 1;
                        self.md.push_str("<u>");
                    }
                    "</u>" if self.underline_depth > 0 => {
                        self.underline_depth -= 1;
                        self.md.push_str("</u>");
                    }
                    t if t.starts_with("<br") => self.line_break(),
                    _ => {}
                }
            }
            Event::FootnoteReference(label) => self.text(&format!("[{label}]")),
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                push_code_span(&mut self.md, &t);
                self.plain.push_str(&t);
            }
            Event::Html(t) => self.text(t.trim_end()),
            _ => {}
        }
    }

    fn marker(&mut self, m: &str) {
        if self.image_alt.is_none() {
            self.md.push_str(m);
        }
    }

    fn finish(mut self) -> (String, String) {
        for _ in 0..self.underline_depth {
            self.md.push_str("</u>");
        }
        let mut md = self.md.trim_end_matches('\n').to_owned();
        if self.bold && !self.plain.trim().is_empty() {
            md = format!("**{md}**");
        }
        (md, self.plain.trim_end_matches('\n').to_owned())
    }
}

/// Backslash-escapes all ASCII punctuation so text is never re-interpreted.
fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            // Slint's interpolation placeholder.
            '\u{e541}' => {}
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
}

fn push_code_span(out: &mut String, code: &str) {
    let mut longest = 0;
    let mut run = 0;
    for c in code.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    let pad = code.starts_with('`') || code.ends_with('`') || (code.starts_with(' ') && code.ends_with(' '));
    out.push_str(&fence);
    if pad {
        out.push(' ');
    }
    out.push_str(&code.replace('\u{e541}', ""));
    if pad {
        out.push(' ');
    }
    out.push_str(&fence);
}

fn encode_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\u{e541}' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(src: &str) -> Rendered {
        render(src, &LinkRefs::new())
    }

    #[test]
    fn heading_is_bold_without_markers() {
        let out = r("## Hello **big** world");
        assert_eq!(out.kind, Kind::Heading(2));
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].md, "**Hello big world**");
        assert_eq!(out.lines[0].plain, "Hello big world");
    }

    #[test]
    fn inline_styles_are_reemitted() {
        let out = r("A *b* **c** ~~d~~ `e` [f](<http://x.y/a b>) 1. #");
        assert_eq!(out.lines[0].md, "A *b* **c** ~~d~~ `e` [f](http://x.y/a%20b) 1\\. \\#");
    }

    #[test]
    fn lists_and_tasks() {
        let out = r("- a\n  - b\n- [x] c\n\n1. one\n2. two");
        let markers: Vec<_> = out.lines.iter().map(|l| (l.indent, l.marker.as_str(), l.task, l.plain.as_str())).collect();
        assert_eq!(
            markers,
            vec![(0, "•", 0, "a"), (1, "◦", 0, "b"), (0, "•", 2, "c"), (0, "1.", 0, "one"), (0, "2.", 0, "two")]
        );

        let out = r("3. one\n4. two");
        let markers: Vec<_> = out.lines.iter().map(|l| l.marker.as_str()).collect();
        assert_eq!(markers, vec!["3.", "4."]);
    }

    #[test]
    fn quote_lines() {
        let out = r("> a\n> b\n>\n> - c");
        assert_eq!(out.kind, Kind::Quote);
        let lines: Vec<_> = out.lines.iter().map(|l| (l.indent, l.marker.as_str(), l.plain.as_str())).collect();
        assert_eq!(lines, vec![(0, "", "a\nb"), (0, "•", "c")]);
    }

    #[test]
    fn code_block() {
        let out = r("```rust\nfn main() {}\n```");
        assert_eq!(out.code, "fn main() {}");
        assert_eq!(out.lang, "rust");
    }

    #[test]
    fn table_rows() {
        let out = r("| a | b |\n|:-:|--:|\n| 1 | **2** |");
        assert_eq!(out.rows.len(), 2);
        assert!(out.rows[0].header);
        assert_eq!(out.rows[0].cells[0].md, "**a**");
        assert_eq!(out.rows[1].cells[1].md, "**2**");
        assert_eq!(out.rows[1].cells[1].align, Align::Right);
        assert_eq!(out.rows[1].cells[0].align, Align::Center);
        assert!((out.col_weights.iter().sum::<f32>() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn reference_links_resolve() {
        let mut refs = LinkRefs::new();
        refs.insert("home".into(), ("http://h".into(), String::new()));
        let out = render("see [x][home]", &refs);
        assert_eq!(out.lines[0].md, "see [x](http://h)");
    }

    #[test]
    fn image_block() {
        let out = r("![A cat](cat.png)");
        assert_eq!(out.image, Some(("cat.png".into(), "A cat".into())));
        let out = r("![a *styled* label](photo.png)");
        assert_eq!(out.kind, Kind::Image);
        assert_eq!(out.image, Some(("photo.png".into(), "a styled label".into())));

        // A reference image, defined in another block.
        let mut refs = LinkRefs::new();
        refs.insert("photo".into(), ("photo.png".into(), String::new()));
        let out = render("![alt][photo]", &refs);
        assert_eq!(out.kind, Kind::Image);
        assert_eq!(out.image, Some(("photo.png".into(), "alt".into())));
    }

    #[test]
    fn styled_text_accepts_output() {
        // Everything we emit must be accepted by Slint's StyledText parser.
        for src in [
            "## Hello **big** world",
            "A *b* **c** ~~d~~ `e` [f](http://x) <u>u</u> <span>x</span> ![i](p.png)",
            "text with `` ` `` backtick and \\* stars \\_ and 1. 2) # - + > | ~ ! [x]",
            "soft\nbreak and hard  \nbreak",
            "- a\n- b\n\n  para in item",
            "<u>unterminated",
            "**bold *nested* bold** and ***both***",
        ] {
            for line in r(src).lines {
                slint::StyledText::from_markdown(&line.md)
                    .unwrap_or_else(|e| panic!("{src:?} -> {:?}: {e}", line.md));
            }
        }
    }
}
