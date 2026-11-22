//! The document model.
//!
//! A Markdown file is split into top-level *blocks* (paragraph, heading, list,
//! code fence, ...). Every block keeps its exact source text plus the
//! whitespace that followed it, so a file that is opened and saved without
//! edits is written back byte-for-byte.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;
use std::ops::Range;

/// The Markdown dialect used everywhere in the editor.
pub fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// Raw Markdown of the block, without trailing newlines.
    pub src: String,
    /// Whitespace between this block and the next one (or the end of file).
    pub gap: String,
}

impl Block {
    pub fn new(src: impl Into<String>) -> Self {
        Block { src: src.into(), gap: "\n\n".into() }
    }
}

/// What a block is, as far as rendering and editing are concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Empty,
    Paragraph,
    Heading(u8),
    Code { fenced: bool },
    Quote,
    List,
    Rule,
    Table,
    /// A paragraph that consists of a single image.
    Image,
    Html,
    /// YAML front matter.
    Meta,
    Footnote,
    /// Text that produces no output, such as link reference definitions.
    Raw,
}

impl Kind {
    /// Blocks whose text flows like prose and can be merged with neighbours.
    pub fn is_prose(self) -> bool {
        matches!(
            self,
            Kind::Empty | Kind::Paragraph | Kind::Heading(_) | Kind::Quote | Kind::List | Kind::Footnote
        )
    }

    /// The heading level for headings, 0 for paragraphs, `None` for anything else.
    pub fn heading_level(self) -> Option<u8> {
        match self {
            Kind::Heading(l) => Some(l),
            Kind::Paragraph | Kind::Empty => Some(0),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Document {
    /// Whitespace before the first block.
    pub prefix: String,
    pub blocks: Vec<Block>,
    /// Most lines of the file ended in CRLF; edited text is saved that way.
    pub crlf: bool,
    /// The file started with a UTF-8 byte order mark; restored on save.
    pub bom: bool,
    /// For files the flags above can't reproduce (mixed line endings): the
    /// text as parsed (LF) and the file as read. While the document still
    /// matches, the file is written back unchanged.
    verbatim: Option<(String, String)>,
}

/// Which rows of the block list were replaced by an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub start: usize,
    pub removed: usize,
    pub inserted: usize,
}

impl Default for Document {
    fn default() -> Self {
        Document {
            prefix: String::new(),
            blocks: vec![Block { src: String::new(), gap: "\n".into() }],
            crlf: false,
            bom: false,
            verbatim: None,
        }
    }
}

impl Document {
    pub fn parse(file: &str) -> Self {
        let (bom, text) = match file.strip_prefix('\u{feff}') {
            Some(rest) => (true, rest),
            None => (false, file),
        };
        let crlfs = text.matches("\r\n").count();
        let crlf = crlfs > 0 && crlfs * 2 > text.matches('\n').count();
        let text = text.replace("\r\n", "\n");
        let (prefix, blocks) = split_blocks(&text);
        let mut doc = if blocks.is_empty() {
            // Only whitespace: keep it so an untouched file round-trips.
            Document { blocks: vec![Block { src: String::new(), gap: prefix }], ..Document::default() }
        } else {
            Document { prefix, blocks, ..Document::default() }
        };
        doc.crlf = crlf;
        doc.bom = bom;
        if doc.encode(&text) != file {
            doc.verbatim = Some((text, file.to_owned()));
        }
        doc
    }

    pub fn to_text(&self) -> String {
        let mut out = self.prefix.clone();
        for b in &self.blocks {
            out.push_str(&b.src);
            out.push_str(&b.gap);
        }
        match &self.verbatim {
            Some((parsed, file)) if *parsed == out => file.clone(),
            _ => self.encode(&out),
        }
    }

    /// Applies the file's line endings and byte order mark to LF text.
    fn encode(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 3);
        if self.bom {
            out.push('\u{feff}');
        }
        if self.crlf {
            out.push_str(&text.replace('\n', "\r\n"));
        } else {
            out.push_str(text);
        }
        out
    }

    pub fn is_blank(&self) -> bool {
        self.blocks.iter().all(|b| b.src.trim().is_empty())
    }

    pub fn word_count(&self) -> (usize, usize) {
        self.blocks.iter().map(|b| word_count(&b.src)).fold((0, 0), |(w, c), (bw, bc)| (w + bw, c + bc))
    }

    /// Replaces `blocks[start..start + removed]` with `parts`.
    ///
    /// Every part except `keep` is normalised: unclosed code fences and HTML
    /// blocks are closed and text that contains several blocks is split up
    /// (an empty part disappears). `keep` is the part that stays in edit mode;
    /// it's inserted as typed. Afterwards the blocks around the edit are
    /// re-read the way the saved file will be, so the model never shows blocks
    /// that would merge (or split) on reload. Returns the change and the final
    /// index of `keep`.
    pub fn splice(
        &mut self,
        start: usize,
        removed: usize,
        parts: Vec<String>,
        keep: Option<usize>,
    ) -> (Change, Option<usize>) {
        let removed = removed.min(self.blocks.len() - start);
        let tail_gap = if removed > 0 {
            self.blocks[start + removed - 1].gap.clone()
        } else if start > 0 {
            std::mem::replace(&mut self.blocks[start - 1].gap, "\n\n".into())
        } else {
            "\n\n".into()
        };

        let mut new_blocks: Vec<Block> = Vec::new();
        let mut keep_at = None;
        for (k, part) in parts.into_iter().enumerate() {
            if Some(k) == keep {
                keep_at = Some(start + new_blocks.len());
                new_blocks.push(Block::new(part));
            } else {
                new_blocks.extend(normalize(&part));
            }
        }

        if let Some(last) = new_blocks.last_mut() {
            last.gap = tail_gap;
        } else if start > 0 {
            // Pure removal: the previous block takes over the separator.
            self.blocks[start - 1].gap = tail_gap;
        }
        let inserted = new_blocks.len();
        self.blocks.splice(start..start + removed, new_blocks);

        // Edited blocks must stay separate blocks when the file is re-read:
        // the block before the edit and every inserted block end in a blank line.
        let len = self.blocks.len();
        for k in start.saturating_sub(1)..(start + inserted).min(len) {
            ensure_separator(&mut self.blocks[k].gap, k + 1 == len);
        }

        let mut change = Change { start, removed, inserted };
        self.reconcile(&mut change, &mut keep_at);
        if self.blocks.is_empty() {
            self.blocks.push(Block { src: String::new(), gap: "\n".into() });
            change.inserted += 1;
            keep_at = Some(0);
        }
        (change, keep_at)
    }

    /// Re-reads the blocks around a change (one block either side) as the
    /// parser will see them in the saved file and adopts that split where it
    /// differs, e.g. an indented paragraph after a list becomes part of the
    /// list. A merged block can reach further, so this repeats with the
    /// widened change until nothing moves. The block in edit mode is left
    /// alone; it's checked when editing finishes.
    fn reconcile(&mut self, change: &mut Change, keep_at: &mut Option<usize>) {
        // Each round that fixes something widens the window, so this ends
        // after at most one round per block.
        for _ in 0..=self.blocks.len() {
            if self.blocks.is_empty() {
                return;
            }
            let lo = change.start.saturating_sub(1);
            let hi = (change.start + change.inserted + 1).min(self.blocks.len());
            let segments: [Range<usize>; 2] = match *keep_at {
                Some(k) if (lo..hi).contains(&k) => [lo..k, k + 1..hi],
                _ => [lo..lo, lo..hi],
            };
            let mut fixed_any = false;
            // Last segment first, so the earlier one's indices stay valid.
            for seg in segments.into_iter().rev() {
                if seg.len() < 2 {
                    continue;
                }
                let Some(mut fixed) = self.reparse(seg.clone()) else { continue };
                // Only replace the blocks that actually differ.
                let current = &self.blocks[seg.clone()];
                let same_head = fixed.iter().zip(current).take_while(|(a, b)| a == b).count();
                let same_tail = fixed[same_head..]
                    .iter()
                    .rev()
                    .zip(current[same_head..].iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count();
                fixed.truncate(fixed.len() - same_tail);
                fixed.drain(..same_head);
                let seg = seg.start + same_head..seg.end - same_tail;
                let new_len = fixed.len();
                self.blocks.splice(seg.clone(), fixed);
                fixed_any = true;

                // `change` maps old rows to new rows; widen it to cover `seg`.
                let new_lo = change.start.min(seg.start);
                let new_hi = (change.start + change.inserted).max(seg.end);
                let old_hi = new_hi - change.inserted + change.removed;
                let new_hi = new_hi - seg.len() + new_len;
                *change = Change { start: new_lo, removed: old_hi - new_lo, inserted: new_hi - new_lo };
                if let Some(k) = keep_at.as_mut()
                    && *k >= seg.end
                {
                    *k = *k + new_len - seg.len();
                }
            }
            if !fixed_any {
                return;
            }
        }
    }

    /// Parses `blocks[range]` together. Returns the resulting blocks if they
    /// don't match the model.
    fn reparse(&self, range: Range<usize>) -> Option<Vec<Block>> {
        // Front matter is only recognised at the very start of the file, so
        // anywhere else put a paragraph in front.
        const PROBE: &str = "x\n\n";
        let probe = range.start > 0 || !self.prefix.is_empty();
        let mut text = if probe { PROBE.to_owned() } else { String::new() };
        for b in &self.blocks[range.clone()] {
            text.push_str(&b.src);
            text.push_str(&b.gap);
        }
        let (prefix, mut blocks) = split_blocks(&text);
        if !prefix.is_empty() {
            return None;
        }
        if probe {
            if blocks.first().map(|b| b.src.as_str()) != Some("x") {
                return None;
            }
            blocks.remove(0);
        }
        let current = &self.blocks[range];
        let same = blocks.len() == current.len() && blocks.iter().zip(current).all(|(a, b)| a.src == b.src);
        if same || blocks.is_empty() {
            return None;
        }
        for b in &mut blocks {
            if b.gap.is_empty() {
                b.gap = "\n\n".into();
            }
        }
        // Keep the separator that followed the range.
        blocks.last_mut().unwrap().gap = current.last().unwrap().gap.clone();
        Some(blocks)
    }

    /// Collects link reference definitions (`[label]: url "title"`) so that
    /// reference links can be resolved while rendering a single block.
    pub fn link_refs(&self) -> LinkRefs {
        let mut refs = LinkRefs::new();
        for b in &self.blocks {
            if !b.src.contains("]:") {
                continue;
            }
            let parser = Parser::new_ext(&b.src, options());
            for (label, def) in parser.reference_definitions().iter() {
                refs.entry(normalize_label(label))
                    .or_insert_with(|| (def.dest.to_string(), def.title.as_deref().unwrap_or("").to_string()));
            }
        }
        refs
    }
}

pub type LinkRefs = HashMap<String, (String, String)>;

/// Words and non-whitespace characters in `text`.
pub fn word_count(text: &str) -> (usize, usize) {
    (text.split_whitespace().count(), text.chars().filter(|c| !c.is_whitespace()).count())
}

pub fn normalize_label(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn ensure_separator(gap: &mut String, is_file_end: bool) {
    let newlines = gap.matches('\n').count();
    if is_file_end {
        if newlines == 0 {
            *gap = "\n".into();
        }
    } else if newlines < 2 {
        *gap = "\n\n".into();
    }
}

/// Re-splits edited text into blocks, closing unterminated code fences and
/// HTML blocks.
pub fn normalize(text: &str) -> Vec<Block> {
    let (_, mut blocks) = split_blocks(text);
    for b in &mut blocks {
        close_open_block(&mut b.src);
        // Spaces after the last newline would indent whatever follows.
        b.gap.truncate(b.gap.trim_end_matches([' ', '\t']).len());
        if b.gap.is_empty() {
            b.gap = "\n\n".into();
        }
    }
    blocks
}

/// Closes a code fence or HTML block that would otherwise run on to the end
/// of the file and swallow every block after it.
fn close_open_block(src: &mut String) {
    if let Some(fence) = opening_fence(src) {
        if !fence_closed(src, &fence) {
            src.push('\n');
            src.push_str(&" ".repeat(fence.indent));
            src.push_str(&fence.ch.to_string().repeat(fence.len));
        }
    } else if let Some(end) = html_terminator(src) {
        src.push('\n');
        src.push_str(&end);
    }
}

/// For an HTML block that only ends at an end marker (CommonMark HTML block
/// types 1–5: `<pre>`, `<!--`, `<?`, `<!X`, `<![CDATA[`), returns that marker
/// if the block doesn't contain it yet.
fn html_terminator(src: &str) -> Option<String> {
    let first = src.lines().next()?;
    let indent = first.len() - first.trim_start_matches(' ').len();
    if indent > 3 || !first[indent..].starts_with('<') || classify(src) != Kind::Html {
        return None;
    }
    let line = first[indent..].to_ascii_lowercase();
    let end = if line.starts_with("<!--") {
        "-->".to_owned()
    } else if line.starts_with("<?") {
        "?>".to_owned()
    } else if line.starts_with("<![cdata[") {
        "]]>".to_owned()
    } else if line.starts_with("<!") && line[2..].starts_with(|c: char| c.is_ascii_alphabetic()) {
        ">".to_owned()
    } else {
        let tag = ["script", "pre", "style", "textarea"].into_iter().find(|tag| {
            line[1..]
                .strip_prefix(tag)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '>']))
        })?;
        format!("</{tag}>")
    };
    (!src.to_ascii_lowercase().contains(&end)).then_some(end)
}

/// Splits text into its leading whitespace and top-level blocks.
pub fn split_blocks(text: &str) -> (String, Vec<Block>) {
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut pos = 0;
    for r in block_ranges(text) {
        push_loose(text, pos..r.start, &mut spans);
        pos = r.end;
        spans.push(r);
    }
    push_loose(text, pos..text.len(), &mut spans);

    let prefix_end = spans.first().map_or(text.len(), |r| r.start);
    let mut blocks = Vec::with_capacity(spans.len());
    for (k, r) in spans.iter().enumerate() {
        let next = spans.get(k + 1).map_or(text.len(), |n| n.start);
        blocks.push(Block { src: text[r.clone()].to_owned(), gap: text[r.end..next].to_owned() });
    }
    (text[..prefix_end].to_owned(), blocks)
}

/// Text between two parsed blocks that isn't whitespace (e.g. link reference
/// definitions) becomes a block of its own so it's never lost.
fn push_loose(text: &str, range: Range<usize>, spans: &mut Vec<Range<usize>>) {
    let s = &text[range.clone()];
    let Some(first) = s.find(|c: char| !c.is_whitespace()) else { return };
    let (last, ch) = s.char_indices().rev().find(|(_, c)| !c.is_whitespace()).unwrap();
    let start = line_start(text, range.start + first).max(range.start);
    spans.push(start..range.start + last + ch.len_utf8());
}

fn line_start(text: &str, pos: usize) -> usize {
    text[..pos].rfind('\n').map_or(0, |p| p + 1)
}

/// Byte ranges of the top-level blocks, trimmed of trailing whitespace and
/// extended back to the start of their first line.
fn block_ranges(text: &str) -> Vec<Range<usize>> {
    let mut raw = Vec::new();
    let mut depth = 0usize;
    for (event, range) in Parser::new_ext(text, options()).into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    raw.push(range);
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ if depth == 0 => raw.push(range),
            _ => {}
        }
    }

    let bytes = text.as_bytes();
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in raw {
        let floor = out.last().map_or(0, |l| l.end);
        let mut s = r.start.max(floor);
        let ls = line_start(text, s);
        if ls >= floor && text[ls..s].bytes().all(|b| b == b' ' || b == b'\t') {
            s = ls;
        }
        let mut e = r.end.max(s);
        while e > s && bytes[e - 1].is_ascii_whitespace() {
            e -= 1;
        }
        if e <= s {
            continue;
        }
        match out.last_mut() {
            Some(last) if s < last.end => last.end = last.end.max(e),
            _ => out.push(s..e),
        }
    }
    out
}

/// Determines the kind of a block from its source.
pub fn classify(src: &str) -> Kind {
    if src.trim().is_empty() {
        return Kind::Empty;
    }
    let mut events = Parser::new_ext(src, options());
    let tag = loop {
        match events.next() {
            Some(Event::Start(tag)) => break tag,
            Some(Event::Rule) => return Kind::Rule,
            Some(_) => continue,
            None => return Kind::Raw,
        }
    };
    match tag {
        Tag::Heading { level, .. } => Kind::Heading(level as u8),
        Tag::Paragraph => {
            if lone_image(&mut events).is_some() { Kind::Image } else { Kind::Paragraph }
        }
        Tag::CodeBlock(CodeBlockKind::Fenced(_)) => Kind::Code { fenced: true },
        Tag::CodeBlock(CodeBlockKind::Indented) => Kind::Code { fenced: false },
        Tag::BlockQuote(_) => Kind::Quote,
        Tag::List(_) => Kind::List,
        Tag::Table(_) => Kind::Table,
        Tag::HtmlBlock => Kind::Html,
        Tag::MetadataBlock(_) => Kind::Meta,
        Tag::FootnoteDefinition(_) => Kind::Footnote,
        _ => Kind::Paragraph,
    }
}

/// If the rest of a paragraph is exactly one image (optionally wrapped in a
/// link), returns its URL and alt text.
pub fn lone_image<'a>(events: &mut impl Iterator<Item = Event<'a>>) -> Option<(String, String)> {
    let mut found: Option<(String, String)> = None;
    let mut in_image = false;
    for event in events {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) if found.is_none() => {
                found = Some((dest_url.to_string(), String::new()));
                in_image = true;
            }
            Event::End(TagEnd::Image) => in_image = false,
            Event::Text(t) | Event::Code(t) if in_image => found.as_mut()?.1.push_str(&t),
            Event::Text(t) if t.trim().is_empty() => {}
            Event::Start(Tag::Link { .. }) | Event::End(TagEnd::Link) | Event::SoftBreak => {}
            Event::End(TagEnd::Paragraph) => return found,
            _ => return None,
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fence {
    pub ch: char,
    pub len: usize,
    pub indent: usize,
}

/// Parses an opening code fence on the first line.
pub fn opening_fence(src: &str) -> Option<Fence> {
    let line = src.lines().next()?;
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = rest.len() - rest.trim_start_matches(ch).len();
    if len < 3 || (ch == '`' && rest[len..].contains('`')) {
        return None;
    }
    Some(Fence { ch, len, indent })
}

pub fn fence_closed(src: &str, fence: &Fence) -> bool {
    src.lines().skip(1).any(|line| {
        let t = line.trim_start_matches(' ');
        line.len() - t.len() <= 3 && {
            let run = t.len() - t.trim_start_matches(fence.ch).len();
            run >= fence.len && t[run..].trim().is_empty()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\ntitle: x\n---\n\n# Title\n\nSome *text*\nmore.\n\n\n- a\n- b\n\n```rust\nfn main() {}\n```\n\n> quote\n\n[ref]: http://example.com\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\n    indented\n";

    #[test]
    fn round_trip_is_exact() {
        for text in [
            SAMPLE,
            "",
            "\n\n",
            "no newline",
            "a\r\n\r\nb\r\n",
            "  # indented heading\n",
            "para\n- list\n",
            "a\r\n\r\nb\nc\n",
            "\u{feff}# Title\r\n\r\ntext\r\n",
            "\u{feff}",
        ] {
            assert_eq!(Document::parse(text).to_text(), text, "{text:?}");
        }
    }

    #[test]
    fn splits_into_blocks() {
        let doc = Document::parse(SAMPLE);
        let kinds: Vec<Kind> = doc.blocks.iter().map(|b| classify(&b.src)).collect();
        assert_eq!(
            kinds,
            vec![
                Kind::Meta,
                Kind::Heading(1),
                Kind::Paragraph,
                Kind::List,
                Kind::Code { fenced: true },
                Kind::Quote,
                Kind::Raw,
                Kind::Table,
                Kind::Rule,
                Kind::Code { fenced: false },
            ]
        );
        assert_eq!(doc.blocks[2].src, "Some *text*\nmore.");
        assert_eq!(doc.blocks[2].gap, "\n\n\n");
        assert_eq!(doc.blocks[9].src, "    indented");
    }

    #[test]
    fn link_refs_are_collected() {
        let doc = Document::parse(SAMPLE);
        assert_eq!(doc.link_refs().get("ref").unwrap().0, "http://example.com");
    }

    #[test]
    fn classify_image() {
        assert_eq!(classify("![alt](a.png)"), Kind::Image);
        assert_eq!(classify("[![alt](a.png)](http://x)"), Kind::Image);
        assert_eq!(classify("text ![alt](a.png)"), Kind::Paragraph);
    }

    #[test]
    fn splice_normalizes_and_closes_fences() {
        let mut doc = Document::parse("one\n\ntwo\n");
        let (change, _) = doc.splice(0, 1, vec!["# a\n\nb".into()], None);
        assert_eq!(change, Change { start: 0, removed: 1, inserted: 2 });
        assert_eq!(doc.to_text(), "# a\n\nb\n\ntwo\n");

        let (change, _) = doc.splice(2, 1, vec!["```js\nlet x".into()], None);
        assert_eq!(change.inserted, 1);
        assert_eq!(doc.to_text(), "# a\n\nb\n\n```js\nlet x\n```\n");
    }

    #[test]
    fn splice_removes_empty_and_keeps_one_block() {
        let mut doc = Document::parse("one\n\ntwo\n");
        doc.splice(1, 1, vec!["".into()], None);
        assert_eq!(doc.to_text(), "one\n");
        let (change, keep) = doc.splice(0, 1, vec![], None);
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(change.inserted, 1);
        assert_eq!(keep, Some(0));
    }

    #[test]
    fn splice_keeps_edit_part_raw() {
        let mut doc = Document::parse("one\n");
        let (_, keep) = doc.splice(0, 1, vec!["one".into(), "".into()], Some(1));
        assert_eq!(keep, Some(1));
        assert_eq!(doc.blocks.len(), 2);
        doc.blocks[1].src = "two".into();
        assert_eq!(doc.to_text(), "one\n\ntwo\n");
    }

    #[test]
    fn separators_are_enforced_after_edit() {
        // "para\n- list" is two blocks with a single newline between them.
        let mut doc = Document::parse("para\n- list\n");
        assert_eq!(doc.blocks.len(), 2);
        doc.splice(1, 1, vec!["plain".into()], None);
        assert_eq!(doc.to_text(), "para\n\nplain\n");
    }

    /// The blocks the saved file will have when it's opened again.
    fn reloaded(doc: &Document) -> Vec<String> {
        Document::parse(&doc.to_text()).blocks.into_iter().map(|b| b.src).collect()
    }

    fn srcs(doc: &Document) -> Vec<String> {
        doc.blocks.iter().map(|b| b.src.clone()).collect()
    }

    #[test]
    fn bom_is_kept_out_of_the_first_block() {
        let doc = Document::parse("\u{feff}# Title\n\ntext\n");
        assert_eq!(classify(&doc.blocks[0].src), Kind::Heading(1));
        let mut doc = doc;
        doc.splice(1, 1, vec!["more".into()], None);
        assert_eq!(doc.to_text(), "\u{feff}# Title\n\nmore\n");
    }

    #[test]
    fn mixed_line_endings_survive_until_edited() {
        let mut doc = Document::parse("a\r\n\r\nb\nc\r\n\r\nd\r\n");
        assert_eq!(doc.to_text(), "a\r\n\r\nb\nc\r\n\r\nd\r\n");
        // Once edited, the majority line ending (CRLF here) is used throughout.
        doc.splice(2, 1, vec!["e".into()], None);
        assert_eq!(doc.to_text(), "a\r\n\r\nb\r\nc\r\n\r\ne\r\n");
        let mut lf = Document::parse("a\n\nb\r\nc\n");
        lf.splice(0, 1, vec!["x".into()], None);
        assert_eq!(lf.to_text(), "x\n\nb\nc\n");
    }

    #[test]
    fn fence_opened_after_the_first_line_is_closed() {
        let mut doc = Document::parse("one\n\ntwo\n\nthree\n");
        doc.splice(1, 1, vec!["intro\n\n```js\ncode".into()], None);
        assert_eq!(doc.to_text(), "one\n\nintro\n\n```js\ncode\n```\n\nthree\n");
        assert_eq!(reloaded(&doc), srcs(&doc));
    }

    #[test]
    fn open_html_blocks_are_closed() {
        for (typed, closed) in [
            ("<!-- c", "<!-- c\n-->"),
            ("<pre>", "<pre>\n</pre>"),
            ("<script>\nlet x", "<script>\nlet x\n</script>"),
            ("<STYLE>", "<STYLE>\n</style>"),
            ("<?php", "<?php\n?>"),
            ("<!DOCTYPE html", "<!DOCTYPE html\n>"),
            ("<![CDATA[ x", "<![CDATA[ x\n]]>"),
            ("<!-- done -->", "<!-- done -->"),
            ("<div>", "<div>"),
        ] {
            let mut doc = Document::parse("one\n\ntwo\n\nthree\n");
            doc.splice(1, 1, vec![typed.into()], None);
            assert_eq!(doc.blocks[1].src, closed, "{typed:?}");
            assert_eq!(reloaded(&doc), srcs(&doc), "{typed:?}");
        }
    }

    #[test]
    fn every_inserted_block_is_separated() {
        let mut doc = Document::parse("x\n\ny\n");
        // A part ending in a single newline must not run into the next part.
        doc.splice(0, 1, vec!["hello\n".into(), "world".into()], Some(1));
        assert_eq!(doc.to_text(), "hello\n\nworld\n\ny\n");
    }

    #[test]
    fn indented_paragraph_after_a_list_joins_the_list() {
        let mut doc = Document::parse("- a\n\nb\n\nc\n");
        let (change, _) = doc.splice(1, 1, vec!["  b".into()], None);
        assert_eq!(srcs(&doc), ["- a\n\n  b", "c"]);
        assert_eq!(change, Change { start: 0, removed: 2, inserted: 1 });
        assert_eq!(reloaded(&doc), srcs(&doc));
    }

    #[test]
    fn mid_document_front_matter_lookalike_matches_reload() {
        let mut doc = Document::parse("intro\n\nend\n");
        doc.splice(1, 1, vec!["---\ntitle: x\n---".into()], None);
        assert_eq!(reloaded(&doc), srcs(&doc));
    }

    #[test]
    fn keep_index_follows_reconciled_blocks() {
        // The edit before the kept block merges into the list above it.
        let mut doc = Document::parse("- a\n\nb\n");
        let (_, keep) = doc.splice(1, 1, vec!["  b".into(), "".into()], Some(1));
        assert_eq!(keep, Some(1));
        assert_eq!(doc.blocks[1].src, "");
        assert_eq!(doc.blocks[0].src, "- a\n\n  b");
    }

    /// Random edits never leave the model disagreeing with the saved file.
    #[test]
    fn edits_always_match_reload() {
        const SNIPPETS: &[&str] = &[
            "para", "  b", "- item", "    code", "```js\ncode", "~~~\nx", "<!-- c", "<pre>", "<div>", "# h",
            "---", "> q", "text\n===", "| a |\n|---|\n| 1 |", "1. x", "", "a\n- b", "[r]: http://x", "* * *",
            "<script>", "foo\n\n```", "x  \ny", "- a\n  - b", "   - c", "\tcode", "[^1]: note", "***",
            "a\n\n\n\nb", "> a\n> b", "hello\nworld", "  ", "-", "1)", "<",
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for round in 0..400 {
            let mut doc = Document::parse("start\n\nmiddle\n\nend\n");
            for step in 0..12 {
                let len = doc.blocks.len();
                let start = next(len + 1).min(len);
                let removed = if start == len { 0 } else { next(3).min(len - start) };
                let parts: Vec<String> = (0..next(3) + 1)
                    .map(|_| {
                        let mut s = SNIPPETS[next(SNIPPETS.len())].to_owned();
                        if next(3) == 0 {
                            s.push_str("\n\n");
                            s.push_str(SNIPPETS[next(SNIPPETS.len())]);
                        }
                        s
                    })
                    .collect();
                let before = doc.clone();
                // Half the time one part stays in edit mode and is finished
                // (normalised) afterwards, like leaving the block.
                let keep = (next(2) == 0).then(|| next(parts.len()));
                let (_, kept) = doc.splice(start, removed, parts.clone(), keep);
                if let Some(k) = kept {
                    let src = doc.blocks[k].src.clone();
                    doc.splice(k, 1, vec![src], None);
                }
                let model = srcs(&doc).into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
                let reload = reloaded(&doc).into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
                assert_eq!(
                    reload,
                    model,
                    "round {round} step {step}: splice({start}, {removed}, {parts:?}, {keep:?}) on {:?}",
                    before.to_text()
                );
            }
        }
    }

    #[test]
    fn fences() {
        let f = opening_fence("```rust\nx").unwrap();
        assert_eq!(f, Fence { ch: '`', len: 3, indent: 0 });
        assert!(!fence_closed("```rust\nx", &f));
        assert!(fence_closed("```rust\nx\n```", &f));
        assert!(!fence_closed("````\nx\n```", &opening_fence("````\nx").unwrap()));
        assert!(opening_fence("``not`").is_none());
    }
}
