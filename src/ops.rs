//! Keyboard commands for the block that is being edited.
//!
//! Every command looks at the raw text of the block and the cursor (UTF-8
//! byte offsets) and decides what should happen: let the text field handle
//! the key, rewrite the block in place, or restructure the block list.

use crate::doc::{self, Document, Kind, LinkRefs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    At(usize),
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Let the text field handle the key as usual.
    Pass,
    /// Swallow the key.
    Swallow,
    /// Rewrite the edited block in place.
    Replace { text: String, anchor: usize, cursor: usize },
    /// Replace `blocks[start..start + removed]` with `parts` and continue
    /// editing `parts[edit]` at `cursor`.
    Splice { start: usize, removed: usize, parts: Vec<String>, edit: usize, cursor: usize },
    /// Edit another block (index as it is before the current block is committed).
    Move { to: usize, cursor: Cursor },
    /// Leave edit mode.
    Finish,
}

/// `refs` are the document's link reference definitions: with them,
/// `![alt][label]` on its own is an image block, as it's shown.
pub fn run(doc: &Document, refs: &LinkRefs, i: usize, cmd: &str, text: &str, cursor: usize, anchor: usize) -> Action {
    let cursor = clamp(text, cursor);
    let anchor = clamp(text, anchor);
    let classify = |src: &str| doc::classify_with(src, refs);
    let kind = classify(text);
    match cmd {
        // Enter replaces the selection, as in any text field: it acts on the
        // text without it. (On `Pass` the text field deletes it itself.)
        "enter" if anchor != cursor => {
            let (a, b) = (anchor.min(cursor), anchor.max(cursor));
            let rest = format!("{}{}", &text[..a], &text[b..]);
            enter(i, classify(&rest), &rest, a)
        }
        "enter" => enter(i, kind, text, cursor),
        "ctrl-enter" => Action::Splice { start: i, removed: 1, parts: vec![text.into(), String::new()], edit: 1, cursor: 0 },
        "backspace" if cursor == 0 && anchor == 0 => merge_prev(doc, i, kind, text, &classify),
        "delete" if cursor == text.len() && anchor == cursor => merge_next(doc, i, kind, text, &classify),
        "backspace" | "delete" => Action::Pass,
        "up" if i > 0 => Action::Move { to: i - 1, cursor: Cursor::End },
        "down" if i + 1 < doc.blocks.len() => Action::Move { to: i + 1, cursor: Cursor::At(0) },
        "down" if !kind.is_prose() && kind != Kind::Image => {
            // Leave a trailing code block / table by opening a paragraph below.
            Action::Splice { start: i, removed: 1, parts: vec![text.into(), String::new()], edit: 1, cursor: 0 }
        }
        "up" | "down" => Action::Pass,
        "escape" => Action::Finish,
        "promote" | "demote" => match kind.heading_level() {
            Some(level) => {
                let new = match (cmd, level) {
                    ("promote", 0) => 6,
                    ("promote", l) => l.saturating_sub(1).max(1),
                    (_, 0) => return Action::Swallow,
                    (_, 6) => 0,
                    (_, l) => l + 1,
                };
                heading(text, cursor, anchor, new)
            }
            None => Action::Swallow,
        },
        c if c.len() == 2 && c.starts_with('h') => match (kind.heading_level(), c[1..].parse::<u8>()) {
            (Some(_), Ok(level)) if level <= 6 => heading(text, cursor, anchor, level),
            _ => Action::Swallow,
        },
        "bold" | "italic" if kind.is_prose() || kind == Kind::Image => {
            wrap(text, anchor, cursor, if cmd == "bold" { "**" } else { "*" })
        }
        "tab" | "backtab" => match kind {
            Kind::List => indent(text, cursor, anchor, cmd == "backtab"),
            Kind::Code { fenced } => code_indent(text, fenced, cursor, anchor, cmd == "backtab"),
            _ => Action::Swallow,
        },
        "pasted" => pasted(i, text, cursor),
        _ => Action::Swallow,
    }
}

/// The text field pasted text in front of `cursor`. When that leaves several
/// blocks in the text, as a Markdown file does, they're split up, so that the
/// others are shown rendered, and editing goes on in the one with the caret.
/// Line breaks pasted as CR LF (or CR) become LF, as in a file that's opened.
fn pasted(i: usize, text: &str, cursor: usize) -> Action {
    let (text, cursor, rewritten) = if text.contains('\r') {
        let before = unix_newlines(&text[..cursor]);
        (format!("{before}{}", unix_newlines(&text[cursor..])), before.len(), true)
    } else {
        (text.to_owned(), cursor, false)
    };
    let (parts, edit, at) = split_at_caret(&text, cursor);
    if parts.len() > 1 {
        Action::Splice { start: i, removed: 1, parts, edit, cursor: at }
    } else if rewritten {
        Action::Replace { text, anchor: cursor, cursor }
    } else {
        Action::Pass
    }
}

/// `text` with CR LF and lone CR line breaks as LF.
pub fn unix_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Splits `text` into its blocks: their sources, which of them holds byte
/// offset `caret`, and where it is in that one. A caret between two blocks
/// goes to the start of the next one, and one after the last block to its end.
pub fn split_at_caret(text: &str, caret: usize) -> (Vec<String>, usize, usize) {
    let (prefix, blocks) = doc::split_blocks(text);
    if blocks.is_empty() {
        return (vec![String::new()], 0, 0);
    }
    let mut start = prefix.len();
    let mut found = None;
    for (k, block) in blocks.iter().enumerate() {
        let end = start + block.src.len();
        if found.is_none() && caret <= end {
            found = Some((k, caret.saturating_sub(start)));
        }
        start = end + block.gap.len();
    }
    let last = blocks.len() - 1;
    let (edit, at) = found.unwrap_or((last, blocks[last].src.len()));
    (blocks.into_iter().map(|b| b.src).collect(), edit, at)
}

fn clamp(text: &str, pos: usize) -> usize {
    let mut pos = pos.min(text.len());
    while !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn line_bounds(text: &str, cursor: usize) -> (usize, usize) {
    let start = text[..cursor].rfind('\n').map_or(0, |p| p + 1);
    let end = text[cursor..].find('\n').map_or(text.len(), |p| cursor + p);
    (start, end)
}

fn insert(text: &str, cursor: usize, what: &str) -> Action {
    let new = format!("{}{}{}", &text[..cursor], what, &text[cursor..]);
    let c = cursor + what.len();
    Action::Replace { text: new, anchor: c, cursor: c }
}

/// Splits a block at the cursor; the second half is edited.
fn split(i: usize, text: &str, cursor: usize) -> Action {
    if cursor == 0 && !text.is_empty() {
        // Open a new block above and type there.
        return Action::Splice { start: i, removed: 1, parts: vec![String::new(), text.into()], edit: 0, cursor: 0 };
    }
    let before = text[..cursor].trim_end_matches([' ', '\t', '\n']);
    let after = text[cursor..].trim_start_matches([' ', '\t', '\n']);
    Action::Splice { start: i, removed: 1, parts: vec![before.into(), after.into()], edit: 1, cursor: 0 }
}

/// Removes the line holding the cursor and opens an empty paragraph in its
/// place, splitting the block around it (used to leave lists and quotes).
fn exit_block(i: usize, text: &str, cursor: usize) -> Action {
    let (ls, le) = line_bounds(text, cursor);
    let before = text[..ls].trim_end_matches('\n');
    let after = text[le..].trim_start_matches('\n');
    let mut parts = Vec::new();
    if !before.is_empty() {
        parts.push(before.to_owned());
    }
    let edit = parts.len();
    parts.push(String::new());
    if !after.is_empty() {
        parts.push(after.to_owned());
    }
    Action::Splice { start: i, removed: 1, parts, edit, cursor: 0 }
}

fn enter(i: usize, kind: Kind, text: &str, cursor: usize) -> Action {
    // "```lang" + Enter: create a closed code block and put the cursor inside.
    if !text.contains('\n') && cursor == text.len()
        && let Some(f) = doc::opening_fence(text) {
            let close = format!("{}{}", " ".repeat(f.indent), f.ch.to_string().repeat(f.len));
            return Action::Replace { text: format!("{text}\n\n{close}"), anchor: cursor + 1, cursor: cursor + 1 };
        }
    match kind {
        Kind::Code { fenced: true } => {
            let closed = doc::opening_fence(text).is_some_and(|f| doc::fence_closed(text, &f));
            if closed && cursor == text.len() {
                Action::Splice { start: i, removed: 1, parts: vec![text.into(), String::new()], edit: 1, cursor: 0 }
            } else {
                Action::Pass
            }
        }
        Kind::Code { fenced: false } | Kind::Table | Kind::Html | Kind::Meta | Kind::Raw => Action::Pass,
        Kind::List => list_enter(i, text, cursor),
        Kind::Quote => quote_enter(i, text, cursor),
        Kind::Heading(_) => {
            let prefix = heading_prefix(text).map_or(0, |(_, p)| p);
            // Before the heading text: open a block above. An empty heading has
            // nothing to push down, so the new block goes below.
            if cursor <= prefix && text.len() > prefix { split(i, text, 0) } else { split(i, text, cursor) }
        }
        Kind::Paragraph | Kind::Empty | Kind::Footnote | Kind::Rule | Kind::Image => split(i, text, cursor),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub indent: usize,
    pub bullet: Option<char>,
    pub number: Option<(u64, char)>,
    /// Byte offset of the item content (after the marker and one space).
    pub content: usize,
    /// Byte offset after the task box, if any.
    pub body: usize,
    pub task: Option<bool>,
}

/// Parses a list item marker at the start of a line.
pub fn parse_item(line: &str) -> Option<Item> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let (bullet, number, mlen) = match rest.chars().next()? {
        c @ ('-' | '*' | '+') => (Some(c), None, 1),
        _ => {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let delim = *rest.as_bytes().get(digits)?;
            if digits == 0 || digits > 9 || (delim != b'.' && delim != b')') {
                return None;
            }
            (None, Some((rest[..digits].parse().ok()?, delim as char)), digits + 1)
        }
    };
    let after = &rest[mlen..];
    if !(after.is_empty() || after.starts_with([' ', '\t'])) {
        return None;
    }
    let content = indent + mlen + usize::from(!after.is_empty());
    let c = &line[content..];
    let (task, body) = match c.get(..3) {
        Some("[ ]") => (Some(false), content + 3),
        Some("[x]" | "[X]") => (Some(true), content + 3),
        _ => (None, content),
    };
    let body = if task.is_some() && line[body..].starts_with(' ') { body + 1 } else { body };
    Some(Item { indent, bullet, number, content, body, task })
}

fn list_enter(i: usize, text: &str, cursor: usize) -> Action {
    let (ls, le) = line_bounds(text, cursor);
    let line = &text[ls..le];
    let Some(item) = parse_item(line) else {
        // A continuation line: keep its indentation.
        let ws = &line[..line.len() - line.trim_start().len()];
        return insert(text, cursor, &format!("\n{ws}"));
    };
    if cursor < ls + item.content {
        return Action::Pass;
    }
    if line[item.body..].trim().is_empty() {
        // Enter on an empty item: outdent it, or leave the list.
        return if item.indent > 0 { indent(text, cursor, cursor, true) } else { exit_block(i, text, cursor) };
    }
    let mut marker = match (item.bullet, item.number) {
        (Some(b), _) => format!("{b} "),
        (_, Some((n, d))) => format!("{}{d} ", n + 1),
        _ => unreachable!(),
    };
    if item.task.is_some() {
        marker.push_str("[ ] ");
    }
    let head = &text[..cursor];
    let tail = text[cursor..].trim_start_matches(' ');
    let new_line = format!("\n{}{marker}", " ".repeat(item.indent));
    let c = head.len() + new_line.len();
    Action::Replace { text: format!("{head}{new_line}{tail}"), anchor: c, cursor: c }
}

fn quote_enter(i: usize, text: &str, cursor: usize) -> Action {
    let (ls, le) = line_bounds(text, cursor);
    let line = &text[ls..le];
    let plen = line.len() - line.trim_start_matches([' ', '>']).len();
    if plen == 0 || cursor < ls + plen {
        return Action::Pass;
    }
    if line[plen..].trim().is_empty() {
        return exit_block(i, text, cursor);
    }
    let mut prefix = line[..plen].trim_end().to_owned();
    prefix.push(' ');
    insert(text, cursor, &format!("\n{prefix}"))
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Indents (or outdents) the list item on the cursor line by one level,
/// together with its children.
fn indent(text: &str, cursor: usize, anchor: usize, outdent: bool) -> Action {
    let (ls, le) = line_bounds(text, cursor);
    let line = &text[ls..le];
    let current = leading_spaces(line);
    let mut previous_items = text[..ls].lines().rev().filter_map(parse_item);
    let target = if outdent {
        if current == 0 {
            return Action::Swallow;
        }
        previous_items.find(|it| it.indent < current).map_or(0, |it| it.indent)
    } else {
        match previous_items.find(|it| it.indent <= current) {
            Some(parent) if parent.content > current => parent.content,
            _ => return Action::Swallow,
        }
    };

    // The item's subtree (the following lines indented deeper than it, and
    // blank lines between them) moves with it so children keep their parent.
    let mut lines = vec![ls];
    let mut end = le;
    while end < text.len() {
        let start = end + 1;
        end = text[start..].find('\n').map_or(text.len(), |p| start + p);
        let l = &text[start..end];
        if !l.trim().is_empty() {
            if leading_spaces(l) <= current {
                break;
            }
            lines.push(start);
        }
    }

    let (grow, cut) = (target.saturating_sub(current), current.saturating_sub(target));
    let mut new = String::with_capacity(text.len() + grow * lines.len());
    let mut copied = 0;
    for &s in &lines {
        new.push_str(&text[copied..s]);
        new.push_str(&" ".repeat(grow));
        copied = s + cut;
    }
    new.push_str(&text[copied..]);

    let shift = |pos: usize| {
        if pos < ls {
            return pos;
        }
        // In the indentation of the cursor line: the start of its content.
        let mut pos = pos.max(ls + current);
        let mut moved = 0;
        for &s in &lines {
            if s > pos {
                break;
            }
            pos = pos.max(s + cut);
            moved += 1;
        }
        pos + moved * grow - moved * cut
    };
    Action::Replace { text: new, anchor: shift(anchor), cursor: shift(cursor) }
}

/// Tab and Shift+Tab in a code block. Tab types four spaces, over the
/// selection if it's within a line; a selection over several lines is
/// indented by four spaces instead. Shift+Tab takes up to four spaces (or a
/// tab) off the start of the cursor's line, or of each line of the selection.
/// The fence lines of a fenced block stay as they are, and an indented block
/// keeps the four spaces that make it code.
fn code_indent(text: &str, fenced: bool, cursor: usize, anchor: usize, outdent: bool) -> Action {
    let (a, b) = (anchor.min(cursor), anchor.max(cursor));
    if !outdent && !text[a..b].contains('\n') {
        let new = format!("{}    {}", &text[..a], &text[b..]);
        return Action::Replace { text: new, anchor: a + 4, cursor: a + 4 };
    }
    // The lines of the selection, without one it only reaches the start of.
    let first = line_bounds(text, a).0;
    let last = if b > a && text[..b].ends_with('\n') { b - 1 } else { b };
    let code_lines = match doc::opening_fence(text).filter(|_| fenced) {
        Some(fence) => 1..doc::closing_fence_line(text, &fence).unwrap_or(usize::MAX),
        None => 0..usize::MAX,
    };
    // (at, removed, inserted): spaces taken off or put in at `at`.
    let mut edits: Vec<(usize, usize, usize)> = Vec::new();
    let starts = std::iter::once(0).chain(text.match_indices('\n').map(|(p, _)| p + 1));
    for (n, start) in starts.enumerate() {
        if start < first || !code_lines.contains(&n) {
            continue;
        }
        if start > last {
            break;
        }
        let line = &text[start..line_bounds(text, start).1];
        if !outdent {
            if !line.trim().is_empty() {
                edits.push((start, 0, 4));
            }
            continue;
        }
        let base = if fenced { 0 } else { code_indent_len(line) };
        let rest = &line[base..];
        let spaces = rest.bytes().take(4).take_while(|&c| c == b' ').count();
        let removed = if spaces == 0 && rest.starts_with('\t') { 1 } else { spaces };
        if removed > 0 {
            edits.push((start + base, removed, 0));
        }
    }
    if edits.is_empty() {
        return Action::Swallow;
    }
    let mut new = String::with_capacity(text.len() + 4 * edits.len());
    let mut copied = 0;
    for &(at, removed, inserted) in &edits {
        new.push_str(&text[copied..at]);
        new.push_str(&" ".repeat(inserted));
        copied = at + removed;
    }
    new.push_str(&text[copied..]);
    // A position moves with the text before it; one in spaces taken off
    // goes to where they were.
    let map = |pos: usize| {
        let mut moved = pos as isize;
        for &(at, removed, inserted) in edits.iter().take_while(|&&(at, ..)| at < pos) {
            moved += inserted as isize - removed.min(pos - at) as isize;
        }
        moved as usize
    };
    Action::Replace { text: new, anchor: map(anchor), cursor: map(cursor) }
}

/// The length of the indentation that makes `line` part of an indented code
/// block: up to four columns of spaces and tabs.
fn code_indent_len(line: &str) -> usize {
    let mut column = 0;
    let mut len = 0;
    for c in line.chars() {
        if column >= 4 {
            break;
        }
        match c {
            ' ' => column += 1,
            '\t' => column = (column / 4 + 1) * 4,
            _ => break,
        }
        len += 1;
    }
    len
}

/// Returns the level and prefix length of an ATX heading (`## `).
pub fn heading_prefix(src: &str) -> Option<(u8, usize)> {
    let indent = src.len() - src.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &src[indent..];
    let n = rest.len() - rest.trim_start_matches('#').len();
    let after = &rest[n..];
    if n == 0 || n > 6 || !(after.is_empty() || after.starts_with([' ', '\t', '\n'])) {
        return None;
    }
    let spaces = after.len() - after.trim_start_matches([' ', '\t']).len();
    Some((n as u8, indent + n + spaces))
}

/// Rewrites a paragraph/heading to the given level (0 = paragraph).
pub fn set_heading_level(src: &str, level: u8) -> (String, usize, usize) {
    let (start, body) = match heading_prefix(src) {
        Some((_, p)) => (p, &src[p..]),
        // Setext heading: drop the underline.
        None if matches!(doc::classify(src), Kind::Heading(_)) => (0, src.rsplit_once('\n').map_or(src, |(b, _)| b)),
        None => (0, src),
    };
    let body = if level > 0 { body.replace('\n', " ") } else { body.to_owned() };
    let prefix = if level == 0 { String::new() } else { format!("{} ", "#".repeat(level as usize)) };
    let plen = prefix.len();
    let blen = body.len();
    (prefix + &body, start, plen + blen)
}

fn heading(text: &str, cursor: usize, anchor: usize, level: u8) -> Action {
    let old_prefix = heading_prefix(text).map_or(0, |(_, p)| p);
    let (new, _, total) = set_heading_level(text, level);
    let new_prefix = heading_prefix(&new).map_or(0, |(_, p)| p);
    let map = |pos: usize| (pos.saturating_sub(old_prefix) + new_prefix).min(total);
    Action::Replace { text: new, anchor: map(anchor), cursor: map(cursor) }
}

/// Toggles a Markdown emphasis marker (`**` or `*`) around the selection.
fn wrap(text: &str, anchor: usize, cursor: usize, mark: &str) -> Action {
    let (mut a, mut b) = (anchor.min(cursor), anchor.max(cursor));
    // Emphasis can't begin or end with a space ("**word **" stays literal),
    // so whitespace at the ends of the selection stays outside the markers.
    let sel = &text[a..b];
    if !sel.trim().is_empty() {
        a += sel.len() - sel.trim_start().len();
        b -= sel.len() - sel.trim_end().len();
    }
    let m = mark.len();
    // A run of n asterisks is italic when n is odd and bold when n >= 2, so
    // `***x***` is both and `**x**` isn't italic.
    let has = |n: usize| if mark == "**" { n >= 2 } else { n % 2 == 1 };
    let stars_before = |s: &str| s.len() - s.trim_end_matches('*').len();
    let stars_after = |s: &str| s.len() - s.trim_start_matches('*').len();
    let replace = |new: String, na: usize, nb: usize| {
        let (anchor, cursor) = if anchor <= cursor { (na, nb) } else { (nb, na) };
        Action::Replace { text: new, anchor, cursor }
    };

    // Markers just outside the selection.
    let n = stars_before(&text[..a]);
    if n == stars_after(&text[b..]) && has(n) {
        let new = format!("{}{}{}", &text[..a - m], &text[a..b], &text[b + m..]);
        return replace(new, a - m, b - m);
    }
    // Markers at the ends of the selection.
    let sel = &text[a..b];
    let n = stars_after(sel);
    if n < sel.len() && n == stars_before(sel) && has(n) {
        let new = format!("{}{}{}", &text[..a], &sel[m..sel.len() - m], &text[b..]);
        return replace(new, a, b - 2 * m);
    }
    let new = format!("{}{mark}{sel}{mark}{}", &text[..a], &text[b..]);
    replace(new, a + m, b + m)
}

/// What goes between two prose blocks that are merged: list items and quote
/// lines keep their own lines, other text is joined directly.
fn joiner(first: Kind, second: Kind) -> &'static str {
    match (first, second) {
        (Kind::List, Kind::List) | (Kind::Quote, Kind::Quote) => "\n",
        _ => "",
    }
}

fn merge_prev(doc: &Document, i: usize, kind: Kind, text: &str, classify: &dyn Fn(&str) -> Kind) -> Action {
    if i == 0 {
        return if text.is_empty() && doc.blocks.len() > 1 {
            Action::Splice { start: 0, removed: 2, parts: vec![doc.blocks[1].src.clone()], edit: 0, cursor: 0 }
        } else {
            Action::Swallow
        };
    }
    let prev = &doc.blocks[i - 1].src;
    let prev_kind = classify(prev);
    if text.is_empty() {
        return Action::Splice { start: i - 1, removed: 2, parts: vec![prev.clone()], edit: 0, cursor: prev.len() };
    }
    if matches!(prev_kind, Kind::Rule | Kind::Empty) {
        return Action::Splice { start: i - 1, removed: 2, parts: vec![text.into()], edit: 0, cursor: 0 };
    }
    if !prev_kind.is_prose() || !kind.is_prose() {
        return Action::Move { to: i - 1, cursor: Cursor::End };
    }
    let sep = joiner(prev_kind, kind);
    let cursor = prev.len() + sep.len();
    Action::Splice { start: i - 1, removed: 2, parts: vec![format!("{prev}{sep}{text}")], edit: 0, cursor }
}

fn merge_next(doc: &Document, i: usize, kind: Kind, text: &str, classify: &dyn Fn(&str) -> Kind) -> Action {
    let Some(next) = doc.blocks.get(i + 1).map(|b| &b.src) else { return Action::Swallow };
    let next_kind = classify(next);
    if matches!(next_kind, Kind::Rule | Kind::Empty) {
        return Action::Splice { start: i, removed: 2, parts: vec![text.into()], edit: 0, cursor: text.len() };
    }
    if !next_kind.is_prose() || !kind.is_prose() {
        return Action::Swallow;
    }
    let sep = joiner(kind, next_kind);
    Action::Splice { start: i, removed: 2, parts: vec![format!("{text}{sep}{next}")], edit: 0, cursor: text.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document::parse(text)
    }

    /// `super::run`, in a document without link reference definitions.
    fn run(d: &Document, i: usize, cmd: &str, text: &str, cursor: usize, anchor: usize) -> Action {
        super::run(d, &LinkRefs::new(), i, cmd, text, cursor, anchor)
    }

    fn replace(a: Action) -> (String, usize) {
        match a {
            Action::Replace { text, cursor, .. } => (text, cursor),
            other => panic!("expected Replace, got {other:?}"),
        }
    }

    #[test]
    fn demote_and_promote_headings() {
        let d = doc("## Title\n");
        assert_eq!(replace(run(&d, 0, "demote", "## Title", 5, 5)), ("### Title".into(), 6));
        assert_eq!(replace(run(&d, 0, "promote", "## Title", 5, 5)), ("# Title".into(), 4));
        assert_eq!(replace(run(&d, 0, "promote", "Title", 0, 0)), ("###### Title".into(), 7));
        assert_eq!(replace(run(&d, 0, "demote", "###### Title", 7, 7)), ("Title".into(), 0));
        assert_eq!(replace(run(&d, 0, "h0", "## Title", 1, 1)), ("Title".into(), 0));
        assert_eq!(replace(run(&d, 0, "h2", "Title\n=====", 2, 2)), ("## Title".into(), 5));
        assert_eq!(run(&d, 0, "demote", "- list", 0, 0), Action::Swallow);
    }

    #[test]
    fn enter_splits_paragraphs() {
        let d = doc("hello world\n");
        assert_eq!(
            run(&d, 0, "enter", "hello world", 5, 5),
            Action::Splice { start: 0, removed: 1, parts: vec!["hello".into(), "world".into()], edit: 1, cursor: 0 }
        );
        assert_eq!(
            run(&d, 0, "enter", "## Title", 1, 1),
            Action::Splice { start: 0, removed: 1, parts: vec!["".into(), "## Title".into()], edit: 0, cursor: 0 }
        );
    }

    #[test]
    fn enter_replaces_the_selection() {
        let d = doc("hello big world\n");
        // All of it: an empty paragraph is left to type in.
        assert_eq!(
            run(&d, 0, "enter", "hello big world", 15, 0),
            Action::Splice { start: 0, removed: 1, parts: vec!["".into(), "".into()], edit: 1, cursor: 0 }
        );
        // Part of it, selected either way.
        for (cursor, anchor) in [(10, 6), (6, 10)] {
            assert_eq!(
                run(&d, 0, "enter", "hello big world", cursor, anchor),
                Action::Splice { start: 0, removed: 1, parts: vec!["hello".into(), "world".into()], edit: 1, cursor: 0 }
            );
        }
        // In a list, the next item starts where the selection was.
        assert_eq!(replace(run(&d, 0, "enter", "- one two", 9, 5)), ("- one\n- ".into(), 8));
        // A code block: the text field types the line break over it.
        assert_eq!(run(&d, 0, "enter", "```\nab\n```", 6, 4), Action::Pass);
    }

    #[test]
    fn enter_continues_lists() {
        let d = doc("- a\n");
        assert_eq!(replace(run(&d, 0, "enter", "- a", 3, 3)), ("- a\n- ".into(), 6));
        assert_eq!(replace(run(&d, 0, "enter", "9. a", 4, 4)), ("9. a\n10. ".into(), 9));
        assert_eq!(replace(run(&d, 0, "enter", "- [x] a", 7, 7)), ("- [x] a\n- [ ] ".into(), 14));
        assert_eq!(replace(run(&d, 0, "enter", "- a\n  - b", 9, 9)), ("- a\n  - b\n  - ".into(), 14));
        // Empty item leaves the list.
        assert_eq!(
            run(&d, 0, "enter", "- a\n- ", 6, 6),
            Action::Splice { start: 0, removed: 1, parts: vec!["- a".into(), "".into()], edit: 1, cursor: 0 }
        );
        // Empty nested item is outdented.
        assert_eq!(replace(run(&d, 0, "enter", "- a\n  - ", 8, 8)), ("- a\n- ".into(), 6));
    }

    #[test]
    fn tab_indents_list_items() {
        let d = doc("- a\n");
        assert_eq!(replace(run(&d, 0, "tab", "- a\n- b", 7, 7)), ("- a\n  - b".into(), 9));
        assert_eq!(replace(run(&d, 0, "tab", "1. a\n2. b", 9, 9)), ("1. a\n   2. b".into(), 12));
        assert_eq!(replace(run(&d, 0, "backtab", "- a\n  - b", 9, 9)), ("- a\n- b".into(), 7));
        assert_eq!(run(&d, 0, "tab", "- a", 3, 3), Action::Swallow);
    }

    #[test]
    fn enter_in_quotes() {
        let d = doc("> a\n");
        assert_eq!(replace(run(&d, 0, "enter", "> a", 3, 3)), ("> a\n> ".into(), 6));
        assert_eq!(
            run(&d, 0, "enter", "> a\n> ", 6, 6),
            Action::Splice { start: 0, removed: 1, parts: vec!["> a".into(), "".into()], edit: 1, cursor: 0 }
        );
    }

    #[test]
    fn fence_autocloses() {
        let d = doc("x\n");
        assert_eq!(replace(run(&d, 0, "enter", "```rust", 7, 7)), ("```rust\n\n```".into(), 8));
        assert_eq!(run(&d, 0, "enter", "```rust\nfn", 10, 10), Action::Pass);
    }

    #[test]
    fn backspace_merges() {
        let d = doc("one\n\ntwo\n\n---\n\nthree\n");
        assert_eq!(
            run(&d, 1, "backspace", "two", 0, 0),
            Action::Splice { start: 0, removed: 2, parts: vec!["onetwo".into()], edit: 0, cursor: 3 }
        );
        assert_eq!(
            run(&d, 3, "backspace", "three", 0, 0),
            Action::Splice { start: 2, removed: 2, parts: vec!["three".into()], edit: 0, cursor: 0 }
        );
        assert_eq!(run(&d, 1, "backspace", "two", 1, 1), Action::Pass);
        assert_eq!(
            run(&d, 0, "delete", "one", 3, 3),
            Action::Splice { start: 0, removed: 2, parts: vec!["onetwo".into()], edit: 0, cursor: 3 }
        );
    }

    /// Applies a splice action the way the editor does, then finishes editing.
    fn apply(d: &mut Document, a: Action) {
        let Action::Splice { start, removed, parts, edit, .. } = a else { panic!("expected Splice, got {a:?}") };
        let (_, keep) = d.splice(start, removed, parts, Some(edit));
        let k = keep.unwrap();
        let src = d.blocks[k].src.clone();
        d.splice(k, 1, vec![src], None);
    }

    #[test]
    fn enter_at_start_of_wrapped_line_makes_two_paragraphs() {
        let mut d = doc("hello\nworld\n");
        let a = run(&d, 0, "enter", "hello\nworld", 6, 6);
        assert_eq!(
            a,
            Action::Splice { start: 0, removed: 1, parts: vec!["hello".into(), "world".into()], edit: 1, cursor: 0 }
        );
        apply(&mut d, a);
        assert_eq!(d.to_text(), "hello\n\nworld\n");
        assert_eq!(Document::parse(&d.to_text()).blocks.len(), 2);
        // At the end of the first line the second half has no leading newline.
        assert_eq!(
            run(&d, 0, "enter", "hello\nworld", 5, 5),
            Action::Splice { start: 0, removed: 1, parts: vec!["hello".into(), "world".into()], edit: 1, cursor: 0 }
        );
    }

    #[test]
    fn enter_on_empty_heading_opens_block_below() {
        let d = doc("## \n");
        assert_eq!(
            run(&d, 0, "enter", "## ", 3, 3),
            Action::Splice { start: 0, removed: 1, parts: vec!["##".into(), "".into()], edit: 1, cursor: 0 }
        );
        // With text after the cursor, a block still opens above.
        assert_eq!(
            run(&d, 0, "enter", "## T", 3, 3),
            Action::Splice { start: 0, removed: 1, parts: vec!["".into(), "## T".into()], edit: 0, cursor: 0 }
        );
    }

    #[test]
    fn italic_and_bold_combine() {
        let d = doc("x\n");
        assert_eq!(replace(run(&d, 0, "italic", "a **word** b", 8, 4)), ("a ***word*** b".into(), 9));
        assert_eq!(replace(run(&d, 0, "italic", "a ***word*** b", 9, 5)), ("a **word** b".into(), 8));
        assert_eq!(replace(run(&d, 0, "bold", "a ***word*** b", 9, 5)), ("a *word* b".into(), 7));
        assert_eq!(replace(run(&d, 0, "bold", "a *word* b", 7, 3)), ("a ***word*** b".into(), 9));
        assert_eq!(replace(run(&d, 0, "italic", "a *word* b", 7, 3)), ("a word b".into(), 6));
        // Markers inside the selection.
        assert_eq!(replace(run(&d, 0, "italic", "a **word** b", 10, 2)), ("a ***word*** b".into(), 11));
        assert_eq!(replace(run(&d, 0, "bold", "a **word** b", 10, 2)), ("a word b".into(), 6));
    }

    #[test]
    fn emphasis_skips_surrounding_whitespace() {
        let d = doc("x\n");
        let (text, cursor) = replace(run(&d, 0, "bold", "a word b", 7, 2));
        assert_eq!(text, "a **word** b");
        assert_eq!(cursor, 8);
        assert_eq!(replace(run(&d, 0, "italic", "a  word  b", 1, 9)).0, "a  *word*  b");
    }

    #[test]
    fn merging_lists_and_quotes_keeps_lines() {
        let mut d = doc("- a\n\npara\n");
        d.blocks[1].src = "- c".into();
        assert_eq!(
            run(&d, 1, "backspace", "- c", 0, 0),
            Action::Splice { start: 0, removed: 2, parts: vec!["- a\n- c".into()], edit: 0, cursor: 4 }
        );
        assert_eq!(
            run(&d, 0, "delete", "- a", 3, 3),
            Action::Splice { start: 0, removed: 2, parts: vec!["- a\n- c".into()], edit: 0, cursor: 3 }
        );
        d.blocks[0].src = "> a".into();
        d.blocks[1].src = "> b".into();
        assert_eq!(
            run(&d, 1, "backspace", "> b", 0, 0),
            Action::Splice { start: 0, removed: 2, parts: vec!["> a\n> b".into()], edit: 0, cursor: 4 }
        );
        // A paragraph after a list still joins the last item's text.
        d.blocks[0].src = "- a".into();
        d.blocks[1].src = "b".into();
        assert_eq!(
            run(&d, 1, "backspace", "b", 0, 0),
            Action::Splice { start: 0, removed: 2, parts: vec!["- ab".into()], edit: 0, cursor: 3 }
        );
    }

    #[test]
    fn tab_moves_children_with_the_item() {
        let d = doc("- a\n");
        assert_eq!(replace(run(&d, 0, "tab", "- a\n- b\n  - c", 7, 7)), ("- a\n  - b\n    - c".into(), 9));
        assert_eq!(replace(run(&d, 0, "backtab", "- a\n  - b\n    - c", 9, 9)), ("- a\n- b\n  - c".into(), 7));
        // Siblings and blank lines between children are handled.
        assert_eq!(
            replace(run(&d, 0, "tab", "- a\n- b\n  - c\n\n  - d\n- e", 7, 7)).0,
            "- a\n  - b\n    - c\n\n    - d\n- e"
        );
        // A selection's anchor in a child line moves with it.
        assert_eq!(
            run(&d, 0, "tab", "- a\n- b\n  - c", 7, 13),
            Action::Replace { text: "- a\n  - b\n    - c".into(), anchor: 17, cursor: 9 }
        );
    }

    #[test]
    fn reference_images_merge_like_images() {
        let d = doc("![a][photo]\n\ntext\n\n[photo]: p.png\n");
        let refs = d.link_refs();
        // Backspace at the start of the next block moves into the image block,
        // as it does after `![a](p.png)`, instead of joining its text to it.
        assert_eq!(super::run(&d, &refs, 1, "backspace", "text", 0, 0), Action::Move { to: 0, cursor: Cursor::End });
        assert_eq!(super::run(&d, &refs, 0, "delete", "![a][photo]", 11, 11), Action::Swallow);
        // Without the definition it's a paragraph, and they're joined.
        assert!(matches!(run(&d, 1, "backspace", "text", 0, 0), Action::Splice { .. }));
    }

    #[test]
    fn tab_in_code() {
        let d = doc("x\n");
        let code = "```\na\n\n  b\n```";
        // No selection, or one within a line: four spaces are typed.
        assert_eq!(replace(run(&d, 0, "tab", code, 5, 5)), ("```\na    \n\n  b\n```".into(), 9));
        assert_eq!(replace(run(&d, 0, "tab", "```\nabc\n```", 6, 5)), ("```\na    c\n```".into(), 9));
        // Several lines: they're indented, blank lines and fences aside, and
        // the selection still covers them.
        assert_eq!(
            run(&d, 0, "tab", code, 11, 4),
            Action::Replace { text: "```\n    a\n\n      b\n```".into(), anchor: 4, cursor: 19 }
        );
        assert_eq!(
            run(&d, 0, "tab", code, code.len(), 0),
            Action::Replace { text: "```\n    a\n\n      b\n```".into(), anchor: 0, cursor: code.len() + 8 }
        );
        // A selection that ends at the start of a line leaves that line alone.
        assert_eq!(replace(run(&d, 0, "tab", code, 4, 6)).0, "```\n    a\n\n  b\n```");

        // Shift+Tab: the cursor's line, or each line of the selection.
        let indented = "```\n      a\n\tb\nc\n```";
        assert_eq!(replace(run(&d, 0, "backtab", indented, 10, 10)), ("```\n  a\n\tb\nc\n```".into(), 6));
        assert_eq!(
            run(&d, 0, "backtab", indented, 16, 6),
            Action::Replace { text: "```\n  a\nb\nc\n```".into(), anchor: 4, cursor: 11 }
        );
        assert_eq!(run(&d, 0, "backtab", indented, 15, 15), Action::Swallow, "nothing to take off");
        assert_eq!(run(&d, 0, "backtab", indented, 1, 1), Action::Swallow, "the fence stays");
        // An indented block keeps the indentation that makes it code.
        assert_eq!(replace(run(&d, 0, "backtab", "        a\n    b\n\t  c", 0, 19)).0, "    a\n    b\n\tc");
        assert_eq!(replace(run(&d, 0, "tab", "    a\n    b", 0, 11)).0, "        a\n        b");
    }

    #[test]
    fn pasted_markdown_is_split_into_blocks() {
        let d = doc("x\n");
        // A file pasted into an empty block: editing goes on at its end.
        let file = "# Title\n\nSome *text*.\n\n- a\n- b\n";
        assert_eq!(
            run(&d, 0, "pasted", file, file.len(), file.len()),
            Action::Splice {
                start: 0,
                removed: 1,
                parts: vec!["# Title".into(), "Some *text*.".into(), "- a\n- b".into()],
                edit: 2,
                cursor: 7
            }
        );
        // Pasted in the middle of a paragraph: the caret stays where it was,
        // in the block that holds the rest of the paragraph.
        let text = "before one\n\ntwo after";
        assert_eq!(
            run(&d, 0, "pasted", text, 15, 15),
            Action::Splice { start: 0, removed: 1, parts: vec!["before one".into(), "two after".into()], edit: 1, cursor: 3 }
        );
        // A caret between blocks is at the start of the next one.
        assert_eq!(
            run(&d, 3, "pasted", "a\n\n\n\nb", 4, 4),
            Action::Splice { start: 3, removed: 1, parts: vec!["a".into(), "b".into()], edit: 1, cursor: 0 }
        );
        // Still one block, such as code pasted into a code block: the text
        // field has already done it all.
        assert_eq!(run(&d, 0, "pasted", "```\none\n\ntwo\n```", 13, 13), Action::Pass);
        assert_eq!(run(&d, 0, "pasted", "one line", 8, 8), Action::Pass);
    }

    #[test]
    fn pasted_line_breaks_become_lf() {
        let d = doc("x\n");
        assert_eq!(run(&d, 0, "pasted", "a\r\nb|", 4, 4), Action::Replace { text: "a\nb|".into(), anchor: 3, cursor: 3 });
        assert_eq!(
            run(&d, 0, "pasted", "a\r\n\r\nb\rc", 9, 9),
            Action::Splice { start: 0, removed: 1, parts: vec!["a".into(), "b\nc".into()], edit: 1, cursor: 3 }
        );
    }

    #[test]
    fn split_at_caret_finds_the_block() {
        assert_eq!(split_at_caret("", 0), (vec!["".into()], 0, 0));
        assert_eq!(split_at_caret("\n\n  ", 2), (vec!["".into()], 0, 0));
        assert_eq!(split_at_caret("\n\none\n\ntwo\n\n", 0), (vec!["one".into(), "two".into()], 0, 0));
        assert_eq!(split_at_caret("one\n\ntwo\n\n", 10), (vec!["one".into(), "two".into()], 1, 3));
        assert_eq!(split_at_caret("one\n\ntwo", 3), (vec!["one".into(), "two".into()], 0, 3));
    }

    #[test]
    fn bold_toggles() {
        let d = doc("x\n");
        assert_eq!(replace(run(&d, 0, "bold", "a word", 6, 2)), ("a **word**".into(), 8));
        assert_eq!(replace(run(&d, 0, "bold", "a **word**", 8, 4)), ("a word".into(), 6));
        assert_eq!(replace(run(&d, 0, "italic", "ab", 1, 1)), ("a**b".into(), 2));
    }
}
