// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod doc;
mod images;
mod ops;
mod render;
#[cfg(test)]
mod shots;

use doc::{Change, Document, Kind, LinkRefs};
use ops::{Action, Cursor};
use slint::{CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, StyledText, VecModel};
use std::cell::{Cell as StdCell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::include_modules!();

/// Cursor request meaning "keep the cursor where the click put it".
const KEEP_CURSOR: i32 = -2;

/// How long a key held for the text field of the block being edited is kept:
/// bringing the block back takes a few frames, and a key replayed much later,
/// say after a dialog, would come as a surprise.
const HELD_KEY_LIFETIME: Duration = Duration::from_secs(1);

/// A key held for the text field of the block being edited.
struct HeldKey {
    text: SharedString,
    /// The modifiers it was pressed with: Ctrl+B is replayed as Ctrl+B even
    /// if Ctrl is let go by then.
    modifiers: Modifiers,
    at: Instant,
}

const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn", "txt"];

/// Which image blocks `refresh_images` renders again.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Refresh {
    /// The ones waiting for their image.
    Waiting,
    /// All of them.
    Shown,
    /// All of them, and their rows are set even if nothing changed, so that
    /// the ones in view ask for their image again.
    Ping,
}

enum Pending {
    Open(Option<PathBuf>),
    Close,
}

struct Editor {
    ui: slint::Weak<App>,
    doc: Document,
    refs: LinkRefs,
    model: Rc<VecModel<Block>>,
    /// Index of the block shown as raw Markdown.
    editing: Option<usize>,
    edit_seq: i32,
    /// Where the caret was asked to go when editing started (`KEEP_CURSOR`:
    /// where the click put it).
    edit_cursor: i32,
    /// Source of the edited block when editing started.
    original: String,
    path: Option<PathBuf>,
    /// The file as last read or written; `modified` compares against it.
    saved_text: String,
    modified: bool,
    pending: Option<Pending>,
    /// Words and characters in the document, kept up to date while typing.
    counts: (usize, usize),
    images: images::Images,
    /// `image_width` when `rescaled` last looked.
    rendered_width: StdCell<u32>,
    /// Keys pressed while the window held the focus for the block being
    /// edited that only its text field can handle, with the modifiers held
    /// and when they were pressed. They're replayed when it has the focus again.
    held_keys: Vec<HeldKey>,
}

impl Editor {
    fn new(ui: &App) -> Self {
        let model = Rc::new(VecModel::default());
        ui.set_blocks(ModelRc::from(model.clone()));
        let doc = Document::default();
        let mut editor = Editor {
            ui: ui.as_weak(),
            saved_text: doc.to_text(),
            doc,
            refs: LinkRefs::new(),
            model,
            editing: None,
            edit_seq: 0,
            edit_cursor: KEEP_CURSOR,
            original: String::new(),
            path: None,
            modified: false,
            pending: None,
            counts: (0, 0),
            images: images::Images::new(),
            rendered_width: StdCell::new(0),
            held_keys: Vec::new(),
        };
        editor.rendered_width.set(editor.image_width());
        editor.rebuild();
        editor
    }

    fn with_ui(&self, f: impl FnOnce(&App)) {
        if let Some(ui) = self.ui.upgrade() {
            f(&ui);
        }
    }

    fn base_dir(&self) -> PathBuf {
        self.path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    // ----- model synchronisation -------------------------------------------------

    fn render_row(&self, i: usize, base: &Path) -> Block {
        let src = &self.doc.blocks[i].src;
        let r = render::render(src, &self.refs);
        let image = r.image.as_ref().map(|(url, alt)| self.image(url, alt, base));
        to_ui(r, src, image)
    }

    /// The image to show for an image block, or why there's none (yet).
    fn image(&self, url: &str, alt: &str, base: &Path) -> (slint::Image, bool, SharedString) {
        let alt = if alt.is_empty() { "image" } else { alt };
        match self.images.get(url, base) {
            images::Lookup::Ready(image) => (image, true, SharedString::new()),
            images::Lookup::Loading => (slint::Image::default(), false, format!("{alt} — loading…").into()),
            images::Lookup::Failed(why) => (slint::Image::default(), false, format!("{alt} — {why}").into()),
        }
    }

    /// Widest an image is decoded at, in pixels: the document column at the
    /// window's scale, and at least at 2×, as the scale may not be known yet.
    fn image_width(&self) -> u32 {
        let (column, scale) = self
            .ui
            .upgrade()
            .map_or((820.0, 1.0), |ui| (ui.global::<Theme>().get_column(), ui.window().scale_factor()));
        (column * scale.max(2.0)).ceil() as u32
    }

    /// The list shows the row of block `i` (it was just created, or its data
    /// set): loads the image of an image block. Images load only for the
    /// blocks in view.
    fn want_image(&self, i: usize) {
        let Some(block) = self.doc.blocks.get(i) else { return };
        if let Some((url, _)) = render::render(&block.src, &self.refs).image {
            self.images.request(&url, &self.base_dir(), self.image_width());
        }
    }

    /// Shows the images that finished loading, and lets go of the ones
    /// dropped for the memory budget.
    fn images_ready(&self) {
        let collected = self.images.collect();
        if collected.sharper || collected.evicted {
            self.refresh_images(Refresh::Shown);
        } else if collected.new {
            self.refresh_images(Refresh::Waiting);
        }
    }

    /// Renders the image blocks again whose image may have changed.
    fn refresh_images(&self, which: Refresh) {
        let base = self.base_dir();
        for i in 0..self.model.row_count() {
            // The block being edited shows its image when editing ends.
            if Some(i) == self.editing {
                continue;
            }
            let Some(row) = self.model.row_data(i) else { continue };
            if row.kind == BlockKind::Image && (which != Refresh::Waiting || !row.has_image) {
                let new = self.render_row(i, &base);
                if which == Refresh::Ping || new.has_image != row.has_image || new.image != row.image || new.caption != row.caption {
                    self.model.set_row_data(i, new);
                }
            }
        }
    }

    /// Called when the window's scale may have changed: it isn't known before
    /// the window first shows, and changes on another screen. Images scaled
    /// down to the column are decoded again if it's wider in pixels now: the
    /// ones in view right away, the others when they come into view.
    fn rescaled(&self) {
        let width = self.image_width();
        if width > self.rendered_width.get() {
            self.rendered_width.set(width);
            self.refresh_images(Refresh::Ping);
        }
    }

    fn rebuild(&mut self) {
        self.refs = self.doc.link_refs();
        let base = self.base_dir();
        let rows: Vec<Block> = (0..self.doc.blocks.len()).map(|i| self.render_row(i, &base)).collect();
        self.model.set_vec(rows);
        self.refresh_meta();
    }

    fn rerender_all(&self) {
        let base = self.base_dir();
        for i in 0..self.doc.blocks.len() {
            self.model.set_row_data(i, self.render_row(i, &base));
        }
    }

    fn apply_change(&mut self, ch: Change) {
        let base = self.base_dir();
        let common = ch.removed.min(ch.inserted);
        for k in 0..common {
            self.model.set_row_data(ch.start + k, self.render_row(ch.start + k, &base));
        }
        for k in common..ch.inserted {
            self.model.insert(ch.start + k, self.render_row(ch.start + k, &base));
        }
        for _ in common..ch.removed {
            self.model.remove(ch.start + common);
        }
        let refs = self.doc.link_refs();
        if refs != self.refs {
            self.refs = refs;
            self.rerender_all();
        }
    }

    /// Cheap update while typing: only what the edit field needs for styling.
    fn update_live(&self, i: usize) {
        let src = &self.doc.blocks[i].src;
        let (kind, level) = ui_kind(doc::classify_with(src, &self.refs));
        if let Some(mut row) = self.model.row_data(i) {
            row.kind = kind;
            row.level = level;
            row.source = src.into();
            self.model.set_row_data(i, row);
        }
    }

    fn refresh_meta(&mut self) {
        self.counts = self.doc.word_count();
        let (words, chars) = self.counts;
        let name = self
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(|| "Untitled".to_owned(), |n| n.to_string_lossy().into_owned());
        let path = self.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default();
        let blank = self.doc.is_blank();
        let modified = self.modified;
        self.with_ui(|ui| {
            ui.set_file_name(name.into());
            ui.set_file_path(path.into());
            ui.set_modified(modified);
            ui.set_words(words as i32);
            ui.set_chars(chars as i32);
            ui.set_doc_blank(blank);
        });
    }

    /// The part of `refresh_meta` that typing in a block can change.
    fn refresh_typing(&self) {
        let (words, chars) = self.counts;
        let blank = self.doc.is_blank();
        let modified = self.modified;
        self.with_ui(|ui| {
            ui.set_modified(modified);
            ui.set_words(words as i32);
            ui.set_chars(chars as i32);
            ui.set_doc_blank(blank);
        });
    }

    /// Replaces the text of block `i` as it's typed. Updates the counts from
    /// this block alone, so a keystroke doesn't rescan the whole document.
    fn set_block_text(&mut self, i: usize, text: String) {
        let (old_words, old_chars) = doc::word_count(&self.doc.blocks[i].src);
        let (new_words, new_chars) = doc::word_count(&text);
        let (words, chars) = self.counts;
        self.counts = (words.saturating_sub(old_words) + new_words, chars.saturating_sub(old_chars) + new_chars);
        self.doc.blocks[i].src = text;
        self.modified = true;
        self.update_live(i);
        self.refresh_typing();
    }

    fn set_status(&self, text: impl Into<SharedString>, error: bool) {
        let text = text.into();
        self.with_ui(|ui| {
            ui.set_status(text);
            ui.set_status_error(error);
        });
    }

    // ----- edit mode -----------------------------------------------------------

    fn set_editing(&mut self, target: Option<usize>, cursor: i32) {
        self.editing = target;
        self.original = target.map(|t| self.doc.blocks[t].src.clone()).unwrap_or_default();
        self.held_keys.clear();
        self.edit_seq += 1;
        self.edit_cursor = cursor;
        let seq = self.edit_seq;
        self.with_ui(|ui| {
            ui.set_cursor_request(cursor);
            ui.set_editing_index(target.map_or(-1, |t| t as i32));
            ui.set_edit_seq(seq);
        });
    }

    /// Leaves edit mode for block `i`: normalises it and renders it.
    /// Returns how many blocks were added (negative: removed).
    fn commit(&mut self, i: usize) -> isize {
        self.editing = None;
        if i >= self.doc.blocks.len() {
            return 0;
        }
        let change = finish_block(&mut self.doc, i, &self.original);
        // Normalising can change the text (a fence gets closed), and edits
        // can be undone by hand, so compare with the file.
        self.update_modified();
        let Some(change) = change else {
            let base = self.base_dir();
            self.model.set_row_data(i, self.render_row(i, &base));
            self.refresh_meta();
            return 0;
        };
        self.apply_change(change);
        self.refresh_meta();
        change.inserted as isize - change.removed as isize
    }

    fn update_modified(&mut self) {
        self.modified = self.doc.to_text() != self.saved_text;
    }

    /// The text to save: the document as it will be once the block in edit
    /// mode is finished, so a raw block (an open fence, an empty placeholder)
    /// never reaches the file.
    fn file_text(&self) -> String {
        match self.editing {
            Some(i) if i < self.doc.blocks.len() => {
                let mut doc = self.doc.clone();
                finish_block(&mut doc, i, &self.original);
                doc.to_text()
            }
            _ => self.doc.to_text(),
        }
    }

    /// Switches edit mode to `target` (an index from before the current block
    /// is committed), or leaves edit mode with `None`.
    fn request_edit(&mut self, target: Option<usize>, cursor: Option<Cursor>) {
        if target.is_some() && target == self.editing {
            return;
        }
        let mut target = target;
        if let Some(i) = self.editing {
            let delta = self.commit(i);
            if let Some(t) = target.as_mut()
                && *t > i {
                    *t = t.saturating_add_signed(delta);
                }
        }
        let target = target.filter(|&t| t < self.doc.blocks.len());
        let cursor = match (cursor, target) {
            (None, _) | (_, None) => KEEP_CURSOR,
            (Some(Cursor::At(n)), _) => n as i32,
            (Some(Cursor::End), Some(t)) => self.doc.blocks[t].src.len() as i32,
        };
        self.set_editing(target, cursor);
        if target.is_none() {
            self.with_ui(|ui| ui.invoke_focus_root());
        }
    }

    fn edited(&mut self, i: usize, text: &str) {
        let Some(block) = self.doc.blocks.get(i) else { return };
        if block.src == text {
            return;
        }
        self.set_block_text(i, text.to_owned());
        self.set_status("", false);
    }

    /// A key pressed while the window holds the focus for the block being
    /// edited (it may be scrolled out of view, with no text field): text is
    /// typed at the saved caret, other keys are held for the text field, and
    /// any key but a lone modifier brings the block back into view. Returns
    /// whether the key was used.
    fn key_away(&mut self, text: &str, modifiers: Modifiers) -> bool {
        let Some(i) = self.editing.filter(|&i| i < self.doc.blocks.len()) else { return false };
        if is_modifier_key(text) {
            return false;
        }
        let Some(ui) = self.ui.upgrade() else { return false };
        self.held_keys.retain(|k| k.at.elapsed() < HELD_KEY_LIFETIME);
        let command = modifiers.control || modifiers.alt || modifiers.meta;
        if !self.held_keys.is_empty() || command || !is_typed_text(text) {
            // Backspace, the arrows, Ctrl+V and so on, and anything after
            // them, so that keys keep their order.
            self.held_keys.push(HeldKey { text: text.into(), modifiers, at: Instant::now() });
        } else {
            let src = &self.doc.blocks[i].src;
            let (caret, anchor) = if ui.get_caret_seq() == self.edit_seq {
                (ui.get_saved_caret().max(0) as usize, ui.get_saved_anchor().max(0) as usize)
            } else {
                // The block hasn't had a text field in this session yet (it's
                // coming into view): use the caret that editing asked for.
                let at = usize::try_from(self.edit_cursor).unwrap_or(src.len());
                (at, at)
            };
            let start = char_boundary(src, caret.min(anchor));
            let end = char_boundary(src, caret.max(anchor));
            let new = format!("{}{text}{}", &src[..start], &src[end..]);
            let at = (start + text.len()) as i32;
            self.set_block_text(i, new);
            self.set_status("", false);
            ui.set_saved_caret(at);
            ui.set_saved_anchor(at);
            ui.set_caret_seq(self.edit_seq);
        }
        ui.invoke_bring_back();
        true
    }

    /// The text field of the block being edited has the focus again: the keys
    /// held for it, but not ones so old that they'd come as a surprise.
    fn take_held_keys(&mut self) -> Vec<HeldKey> {
        let mut keys = std::mem::take(&mut self.held_keys);
        keys.retain(|k| k.at.elapsed() < HELD_KEY_LIFETIME);
        keys
    }

    /// Esc while the window holds the focus for the block being edited.
    fn finish_edit(&mut self) {
        if self.held_keys.iter().any(|k| k.at.elapsed() < HELD_KEY_LIFETIME) {
            // After the keys before it.
            let Some(ui) = self.ui.upgrade() else { return };
            self.key_away(&SharedString::from(slint::platform::Key::Escape), ui.get_modifiers());
        } else {
            self.request_edit(None, None);
        }
    }

    fn key_action(&mut self, i: usize, cmd: &str, text: &str, cursor: usize, anchor: usize) -> EditResult {
        let pass = EditResult { action: 0, ..Default::default() };
        let handled = EditResult { action: 1, ..Default::default() };
        if i >= self.doc.blocks.len() {
            return pass;
        }
        if self.doc.blocks[i].src != text {
            self.edited(i, text);
        }
        match ops::run(&self.doc, &self.refs, i, cmd, text, cursor, anchor) {
            Action::Pass => pass,
            Action::Swallow => handled,
            Action::Replace { text, anchor, cursor } => {
                self.set_block_text(i, text.clone());
                EditResult { action: 2, text: text.into(), anchor: anchor as i32, cursor: cursor as i32 }
            }
            Action::Splice { start, removed, parts, edit, cursor } => {
                self.editing = None;
                let (change, keep) = self.doc.splice(start, removed, parts, Some(edit));
                self.update_modified();
                self.apply_change(change);
                self.set_editing(keep, cursor as i32);
                // The kept block was inserted raw and never existed in the file
                // in this form: normalise it when editing finishes even if no
                // key is typed.
                self.original.clear();
                self.refresh_meta();
                handled
            }
            Action::Move { to, cursor } => {
                self.request_edit(Some(to), Some(cursor));
                handled
            }
            Action::Finish => {
                self.request_edit(None, None);
                handled
            }
        }
    }

    /// Click on empty space beside or above the document: stop editing, so
    /// the whole document is shown rendered.
    fn click_blank(&mut self) {
        if self.editing.is_some() {
            self.request_edit(None, None);
        }
    }

    /// Click below the last block: stop editing, like any other blank space.
    /// When nothing is being edited, continue writing at the end.
    fn click_below(&mut self) {
        if self.editing.is_some() {
            self.request_edit(None, None);
            return;
        }
        let last = self.doc.blocks.len() - 1;
        if doc::classify_with(&self.doc.blocks[last].src, &self.refs).is_prose() {
            self.request_edit(Some(last), Some(Cursor::End));
            return;
        }
        self.request_edit(None, None);
        let len = self.doc.blocks.len();
        let (change, keep) = self.doc.splice(len, 0, vec![String::new()], Some(0));
        self.apply_change(change);
        self.set_editing(keep, 0);
    }

    // ----- files -----------------------------------------------------------------

    fn load(&mut self, path: PathBuf) {
        let text = match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    self.set_status(format!("Can't open {}: not a UTF-8 text file", path.display()), true);
                    return;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.path.is_none() && self.doc.is_blank() => {
                // `linter new.md`: start a new file at that path.
                self.set_status(format!("New file: {}", path.display()), false);
                String::new()
            }
            Err(e) => {
                self.set_status(format!("Can't open {}: {e}", path.display()), true);
                return;
            }
        };
        self.editing = None;
        self.images.clear();
        self.doc = Document::parse(&text);
        self.saved_text = text;
        self.path = Some(std::path::absolute(&path).unwrap_or(path));
        self.modified = false;
        self.with_ui(|ui| ui.invoke_scroll_to_top());
        self.rebuild();
        if self.doc.is_blank() {
            self.set_editing(Some(0), 0);
        } else {
            self.set_editing(None, KEEP_CURSOR);
            self.with_ui(|ui| ui.invoke_focus_root());
            let name = self.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default();
            self.set_status(format!("Opened {name}"), false);
        }
    }

    fn start_blank(&mut self) {
        self.set_editing(Some(0), 0);
    }

    fn open_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open Markdown file")
            .add_filter("Markdown", MARKDOWN_EXTENSIONS)
            .add_filter("All files", &["*"])
            .set_directory(self.base_dir());
        if let Some(ui) = self.ui.upgrade() {
            dialog = dialog.set_parent(&ui.window().window_handle());
        }
        if let Some(path) = dialog.pick_file() {
            self.load(path);
        }
    }

    /// Saves the document; asks for a file name if needed. Returns success.
    fn save(&mut self, save_as: bool) -> bool {
        let path = match (&self.path, save_as) {
            (Some(p), false) => p.clone(),
            _ => {
                let name = self
                    .path
                    .as_deref()
                    .and_then(Path::file_name)
                    .map_or_else(|| "Untitled.md".to_owned(), |n| n.to_string_lossy().into_owned());
                let mut dialog = rfd::FileDialog::new()
                    .set_title("Save Markdown file")
                    .add_filter("Markdown", MARKDOWN_EXTENSIONS)
                    .set_directory(self.base_dir())
                    .set_file_name(name);
                if let Some(ui) = self.ui.upgrade() {
                    dialog = dialog.set_parent(&ui.window().window_handle());
                }
                match dialog.save_file() {
                    Some(p) => p,
                    None => return false,
                }
            }
        };
        let text = self.file_text();
        match write_atomic(&path, text.as_bytes()) {
            Ok(()) => {
                let moved = self.path.as_deref() != Some(path.as_path());
                self.path = Some(path);
                self.saved_text = text;
                self.modified = false;
                if moved {
                    // Relative image paths may resolve differently now.
                    self.rerender_all();
                }
                self.refresh_meta();
                let name = self.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default();
                self.set_status(format!("Saved {name}"), false);
                true
            }
            Err(e) => {
                self.set_status(format!("Can't save {}: {e}", path.display()), true);
                false
            }
        }
    }

    /// Runs `action`, first asking about unsaved changes if there are any.
    /// Returns true if the action ran right away.
    fn guard(&mut self, action: Pending) -> bool {
        if !self.modified {
            // A close request is answered by the caller; don't hide from inside it.
            if !matches!(action, Pending::Close) {
                self.perform(action);
            }
            return true;
        }
        let name = self
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(|| "Untitled".to_owned(), |n| n.to_string_lossy().into_owned());
        let what = match action {
            Pending::Open(_) => "opening another file",
            Pending::Close => "closing",
        };
        self.pending = Some(action);
        self.with_ui(|ui| {
            ui.set_confirm_text(format!("Save the changes to “{name}” before {what}?").into());
            ui.set_confirm_open(true);
        });
        false
    }

    fn perform(&mut self, action: Pending) {
        match action {
            Pending::Open(Some(path)) => self.load(path),
            Pending::Open(None) => self.open_dialog(),
            Pending::Close => self.with_ui(|ui| {
                let _ = ui.hide();
            }),
        }
    }

    fn confirm(&mut self, choice: i32) {
        self.with_ui(|ui| ui.set_confirm_open(false));
        let Some(action) = self.pending.take() else { return };
        match choice {
            0 => {
                if self.save(false) {
                    self.perform(action);
                }
            }
            // Don't save. `modified` stays set until a file actually replaces
            // the document, so cancelling the Open dialog or failing to load
            // keeps the prompt for the next close.
            1 => self.perform(action),
            _ => {}
        }
    }
}

/// Normalises block `i` of `doc` as when editing it finishes, unless it's
/// unchanged. Returns the change, if the document was modified.
fn finish_block(doc: &mut Document, i: usize, original: &str) -> Option<Change> {
    let src = doc.blocks[i].src.clone();
    let blank = src.trim().is_empty();
    if (src == original && !blank) || (blank && doc.blocks.len() == 1) {
        return None;
    }
    Some(doc.splice(i, 1, vec![src], None).0)
}

/// Writes `data` to a temporary file next to `path` and renames it over
/// `path`, so a failed save (disk full, I/O error, crash) never leaves the
/// existing file truncated.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // Replace the file a symlink points to, not the link.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.linter-tmp", std::process::id()));
    let existing = std::fs::metadata(&target).ok();
    let mut created = false;
    let result = (|| {
        let mut file = create_temp(&tmp, existing.is_some())?;
        created = true;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        // The file's own permissions, now that the temporary file holds
        // what the file will.
        #[cfg(unix)]
        if let Some(meta) = &existing {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Creates the temporary file for `write_atomic`. It's new, so that it can't
/// be a file or link someone else put there. On Unix, when it replaces an
/// existing file, only its owner can read it until the data is written and
/// the file's permissions are copied: the file may be private.
fn create_temp(tmp: &Path, replaces: bool) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A new file gets the usual permissions (the umask applies).
        options.mode(if replaces { 0o600 } else { 0o666 });
    }
    #[cfg(not(unix))]
    let _ = replaces;
    match options.open(tmp) {
        // Left behind by a save that crashed: replace it. Removing a link
        // removes the link, not what it points to.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(tmp)?;
            options.open(tmp)
        }
        result => result,
    }
}

// ----- conversion to UI data -------------------------------------------------------

fn ui_kind(kind: Kind) -> (BlockKind, i32) {
    match kind {
        Kind::Empty => (BlockKind::Empty, 0),
        Kind::Paragraph | Kind::Footnote => (BlockKind::Paragraph, 0),
        Kind::Heading(level) => (BlockKind::Heading, level as i32),
        Kind::Code { .. } => (BlockKind::Code, 0),
        Kind::Quote => (BlockKind::Quote, 0),
        Kind::List => (BlockKind::List, 0),
        Kind::Rule => (BlockKind::Rule, 0),
        Kind::Table => (BlockKind::Table, 0),
        Kind::Image => (BlockKind::Image, 0),
        Kind::Html | Kind::Meta | Kind::Raw => (BlockKind::Raw, 0),
    }
}

fn styled(md: &str, plain: &str) -> StyledText {
    StyledText::from_markdown(md).unwrap_or_else(|_| StyledText::from_plain_text(plain))
}

fn to_ui(r: render::Rendered, src: &str, image: Option<(slint::Image, bool, SharedString)>) -> Block {
    let (kind, level) = ui_kind(r.kind);
    let lines: Vec<Line> = r
        .lines
        .iter()
        .map(|l| Line {
            indent: l.indent as i32,
            marker: l.marker.as_str().into(),
            task: l.task as i32,
            text: styled(&l.md, &l.plain),
            code: l.code.as_deref().unwrap_or("").into(),
            is_code: l.code.is_some(),
        })
        .collect();
    let rows: Vec<Row> = r
        .rows
        .iter()
        .map(|row| Row {
            header: row.header,
            cells: ModelRc::new(VecModel::from(
                row.cells
                    .iter()
                    .map(|c| Cell {
                        text: styled(&c.md, &c.plain),
                        align: match c.align {
                            render::Align::Left => 0,
                            render::Align::Center => 1,
                            render::Align::Right => 2,
                        },
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect();

    let (image, has_image, caption) = image.unwrap_or_default();

    Block {
        kind,
        level,
        source: src.into(),
        lines: ModelRc::new(VecModel::from(lines)),
        code: r.code.into(),
        lang: r.lang.into(),
        rows: ModelRc::new(VecModel::from(rows)),
        col_weights: ModelRc::new(VecModel::from(r.col_weights)),
        image,
        has_image,
        caption,
    }
}

/// The largest char boundary of `s` at or before `pos`.
fn char_boundary(s: &str, pos: usize) -> usize {
    let mut pos = pos.min(s.len());
    while !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Slint's key codes for Shift, Control, Alt, AltGr, Caps Lock and Meta.
fn is_modifier_key(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| ('\u{10}'..='\u{18}').contains(&c))
}

/// Text a key press types, as opposed to a control character or one of the
/// private-use code points Slint uses for keys like the arrows and Home.
fn is_typed_text(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----- platform setup ----------------------------------------------------------------

/// winit can't receive dropped files on Wayland, so prefer XWayland when it
/// is available. Set `LINTER_WAYLAND=1` to run as a native Wayland client.
fn select_backend() {
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        use slint::winit_030::winit::event_loop::EventLoop;
        use slint::winit_030::winit::platform::x11::EventLoopBuilderExtX11;
        let var = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        if var("WAYLAND_DISPLAY") && var("DISPLAY") && !var("LINTER_WAYLAND") && !var("SLINT_BACKEND") {
            let mut builder = EventLoop::<slint::winit_030::SlintEvent>::with_user_event();
            builder.with_x11();
            if let Err(e) = slint::BackendSelector::new()
                .backend_name("winit".into())
                .with_winit_event_loop_builder(builder)
                .select()
            {
                // winit allows a single event loop per process, so retry as a
                // native Wayland client in a fresh process image.
                use std::os::unix::process::CommandExt;
                eprintln!("linter: X11 unavailable ({e}); restarting as a Wayland client");
                if let Ok(exe) = std::env::current_exe() {
                    let err = std::process::Command::new(exe)
                        .args(std::env::args_os().skip(1))
                        .env("LINTER_WAYLAND", "1")
                        .exec();
                    eprintln!("linter: restart failed: {err}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// Picks an installed monospace font for code.
fn monospace_family() -> Option<String> {
    if cfg!(target_os = "macos") {
        return Some("Menlo".into());
    }
    if cfg!(windows) {
        return Some("Consolas".into());
    }
    let out = std::process::Command::new("fc-match").args(["-f", "%{family}", "monospace"]).output().ok()?;
    let name = String::from_utf8(out.stdout).ok()?;
    let first = name.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

/// Window events that need winit: dropped files, the mouse wheel, the scale,
/// and the pointer position.
fn install_window_events(app: &App, editor: Rc<RefCell<Editor>>) {
    use slint::winit_030::winit::event::WindowEvent;
    use slint::winit_030::{EventResult, WinitWindowAccessor};

    let weak = app.as_weak();
    let last_drop: StdCell<Option<Instant>> = StdCell::new(None);
    let pointer: Rc<StdCell<Option<slint::LogicalPosition>>> = Rc::new(StdCell::new(None));
    let pointer_in = pointer.clone();
    app.window().on_winit_window_event(move |window, event| {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let scale = window.scale_factor() as f64;
                pointer_in.set(Some(slint::LogicalPosition::new((position.x / scale) as f32, (position.y / scale) as f32)));
            }
            WindowEvent::CursorLeft { .. } => pointer_in.set(None),
            // Before the list scrolls, and may drop the text field that has the
            // focus along with the keys: see park-focus.
            WindowEvent::MouseWheel { .. } => {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_park_focus();
                }
            }
            // The scale is known once the window is first sized (an image in a
            // file opened from the command line was decoded before that), and
            // changes on a screen with another scale. Look after Slint has seen
            // the event.
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                let editor = Rc::downgrade(&editor);
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(ed) = editor.upgrade()
                        && let Ok(ed) = ed.try_borrow()
                    {
                        ed.rescaled();
                    }
                });
            }
            WindowEvent::HoveredFile(_) => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_drop_hover(true);
                }
            }
            WindowEvent::HoveredFileCancelled => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_drop_hover(false);
                }
            }
            WindowEvent::DroppedFile(path) => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_drop_hover(false);
                }
                // Several files dropped at once arrive as separate events: open the first.
                let now = Instant::now();
                if last_drop.get().is_some_and(|t| now.duration_since(t) < Duration::from_millis(500)) {
                    return EventResult::Propagate;
                }
                last_drop.set(Some(now));
                let path = path.clone();
                let editor = editor.clone();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    let mut ed = editor.borrow_mut();
                    if ed.pending.is_none() {
                        ed.guard(Pending::Open(Some(path)));
                    }
                });
            }
            _ => {}
        }
        EventResult::Propagate
    });

    // Slint refreshes the hover state right after a wheel scroll, before the
    // list creates the rows the scroll brought into view. Move the pointer
    // over them once more after the next frame, so that a click lands on the
    // character under it without moving the mouse first.
    let weak = app.as_weak();
    let timer = Rc::new(slint::Timer::default());
    app.on_list_scrolled(move || {
        if pointer.get().is_none() {
            return;
        }
        let (weak, pointer) = (weak.clone(), pointer.clone());
        timer.start(slint::TimerMode::SingleShot, Duration::from_millis(20), move || {
            if let (Some(ui), Some(position)) = (weak.upgrade(), pointer.get()) {
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
            }
        });
    });
}

// ----- theme and settings ------------------------------------------------------------

fn apply_theme(app: &App, dark: bool) {
    app.invoke_apply_theme(dark);
    use slint::winit_030::WinitWindowAccessor;
    app.window().with_winit_window(|w| set_frame_theme(w, dark));
}

/// Asks for a light or dark title bar (honoured on Windows, and by GNOME for
/// X11 windows).
fn set_frame_theme(window: &slint::winit_030::winit::window::Window, dark: bool) {
    use slint::winit_030::winit::window::Theme as FrameTheme;
    window.set_theme(Some(if dark { FrameTheme::Dark } else { FrameTheme::Light }));
}

/// The theme menu: applies the choice and remembers it in `settings`.
fn wire_theme(app: &App, settings: Option<PathBuf>) {
    let weak = app.as_weak();
    app.on_choose_theme(move |dark| {
        let Some(app) = weak.upgrade() else { return };
        apply_theme(&app, dark);
        let Some(path) = &settings else { return };
        if let Err(e) = write_setting(path, "theme", if dark { "dark" } else { "light" }) {
            app.set_status(format!("Can't save the theme setting to {}: {e}", path.display()).into());
            app.set_status_error(true);
        }
    });
}

/// The directory for per-user settings: the program's name.
const SETTINGS_DIR: &str = env!("CARGO_PKG_NAME");

/// Per-user settings file: `key=value` lines.
fn settings_path() -> Option<PathBuf> {
    let env_dir = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        env_dir("APPDATA")
    } else if cfg!(target_os = "macos") {
        env_dir("HOME").map(|h| h.join("Library/Application Support"))
    } else {
        env_dir("XDG_CONFIG_HOME").or_else(|| env_dir("HOME").map(|h| h.join(".config")))
    };
    base.map(|b| b.join(SETTINGS_DIR).join("settings"))
}

fn read_setting(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_owned())
}

/// Sets `key` in the settings file, keeping the other lines.
fn write_setting(path: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let mut text: String = old
        .lines()
        .filter(|l| l.split_once('=').is_none_or(|(k, _)| k.trim() != key))
        .map(|l| format!("{l}\n"))
        .collect();
    text.push_str(&format!("{key}={value}\n"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(path, text.as_bytes())
}

fn wire(app: &App, editor: &Rc<RefCell<Editor>>) {
    // Images load on worker threads. The editor may be busy (in a file dialog,
    // say) when some arrive: they're shown on a later poll then.
    let ed = Rc::downgrade(editor);
    editor.borrow().images.set_on_ready(move || {
        if let Some(ed) = ed.upgrade()
            && let Ok(ed) = ed.try_borrow()
        {
            ed.images_ready();
        }
    });
    let ed = Rc::downgrade(editor);
    app.on_show_image(move |i| {
        let Some(editor) = ed.upgrade() else { return };
        if let Ok(ed) = editor.try_borrow() {
            ed.want_image(i as usize);
            return;
        }
        // The list created the row while the editor was busy: right after.
        let ed = Rc::downgrade(&editor);
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ed) = ed.upgrade()
                && let Ok(ed) = ed.try_borrow()
            {
                ed.want_image(i as usize);
            }
        });
    });
    let ed = editor.clone();
    app.on_edited(move |i, text| ed.borrow_mut().edited(i as usize, &text));
    let ed = editor.clone();
    app.on_key_action(move |i, cmd, text, cursor, anchor| {
        ed.borrow_mut().key_action(i as usize, &cmd, &text, cursor.max(0) as usize, anchor.max(0) as usize)
    });
    let ed = editor.clone();
    app.on_request_edit(move |i, cursor| {
        let cursor = match cursor {
            0 => Some(Cursor::At(0)),
            -1 => Some(Cursor::End),
            _ => None,
        };
        ed.borrow_mut().request_edit(Some(i as usize), cursor)
    });
    let ed = editor.clone();
    app.on_click_below(move || ed.borrow_mut().click_below());
    let ed = editor.clone();
    app.on_click_blank(move || ed.borrow_mut().click_blank());
    let ed = editor.clone();
    app.on_open_file(move || {
        ed.borrow_mut().guard(Pending::Open(None));
    });
    let ed = editor.clone();
    app.on_save_file(move || {
        ed.borrow_mut().save(false);
    });
    let ed = editor.clone();
    app.on_save_file_as(move || {
        ed.borrow_mut().save(true);
    });
    let ed = editor.clone();
    app.on_confirm_choice(move |choice| ed.borrow_mut().confirm(choice));
    let ed = editor.clone();
    app.on_finish_edit(move || ed.borrow_mut().finish_edit());
    let ed = editor.clone();
    app.on_key_away(move |text, modifiers| ed.borrow_mut().key_away(&text, modifiers));
    let ed = editor.clone();
    let weak = app.as_weak();
    app.on_edit_focused(move || {
        let keys = ed.borrow_mut().take_held_keys();
        if keys.is_empty() {
            return;
        }
        // Not from inside this callback: Slint calls it while it updates the UI.
        let weak = weak.clone();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                replay_keys(&ui, keys);
            }
        });
    });
    let ed = editor.clone();
    app.window().on_close_requested(move || {
        let Ok(mut ed) = ed.try_borrow_mut() else { return CloseRequestResponse::KeepWindowShown };
        if ed.guard(Pending::Close) { CloseRequestResponse::HideWindow } else { CloseRequestResponse::KeepWindowShown }
    });
}

/// Plays held keys to the text field that has the focus, each with the
/// modifiers it was pressed with, then puts back the ones held now.
fn replay_keys(ui: &App, keys: Vec<HeldKey>) {
    let window = ui.window();
    let held = ui.get_modifiers();
    let mut current = held.clone();
    for key in keys {
        set_modifiers(window, &current, &key.modifiers);
        window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: key.text });
        current = key.modifiers;
    }
    set_modifiers(window, &current, &held);
}

/// Makes the window see the modifiers `to` held instead of `from`, by
/// pressing or releasing the keys. A modifier held now is released on both
/// sides, and pressed back on the left: on release the platform reports the
/// modifiers held, which clears it.
fn set_modifiers(window: &slint::Window, from: &Modifiers, to: &Modifiers) {
    use slint::platform::{Key, WindowEvent};
    for (was, now, keys) in [
        (from.control, to.control, &[Key::Control, Key::ControlR][..]),
        (from.alt, to.alt, &[Key::Alt][..]),
        (from.shift, to.shift, &[Key::Shift, Key::ShiftR][..]),
        (from.meta, to.meta, &[Key::Meta, Key::MetaR][..]),
    ] {
        if now && !was {
            window.dispatch_event(WindowEvent::KeyPressed { text: keys[0].into() });
        } else if was && !now {
            for &key in keys {
                window.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    select_backend();
    let app = App::new()?;
    app.set_version(env!("CARGO_PKG_VERSION").into());
    if let Some(mono) = monospace_family() {
        app.global::<Theme>().set_mono(mono.into());
    }

    let settings = settings_path();
    let dark = settings.as_deref().and_then(|p| read_setting(p, "theme")).is_some_and(|t| t == "dark");
    apply_theme(&app, dark);
    // The window frame can only be themed once the window exists.
    let window = app.as_weak();
    let _ = slint::spawn_local(async move {
        let Some(app) = window.upgrade() else { return };
        if let Ok(w) = slint::winit_030::WinitWindowAccessor::winit_window(app.window()).await {
            set_frame_theme(&w, app.global::<Theme>().get_dark());
        }
    });

    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    match std::env::args_os().nth(1) {
        Some(path) => editor.borrow_mut().load(PathBuf::from(path)),
        None => editor.borrow_mut().start_blank(),
    }

    wire(&app, &editor);
    wire_theme(&app, settings);
    install_window_events(&app, editor.clone());

    app.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("linter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_replaces_and_cleans_up() {
        let dir = temp_dir("atomic");
        let file = dir.join("a.md");
        write_atomic(&file, b"new file").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new file");
        write_atomic(&file, b"second").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"second");
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["a.md"], "no temporary file is left behind");

        // A failed write reports the error and leaves no temporary file.
        assert!(write_atomic(&dir.join("missing-dir").join("b.md"), b"x").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_permissions_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("atomic-unix");
        let real = dir.join("real.md");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic(&link, b"new").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The temporary file that replaces a file is private from the start, and
    /// a file or link already at its path is replaced, not written through.
    #[cfg(unix)]
    #[test]
    fn atomic_write_temporary_file_is_private_and_new() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("atomic-private");
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let tmp = dir.join("tmp");
        drop(create_temp(&tmp, true).unwrap());
        assert_eq!(mode(&tmp), 0o600, "readable by others before the data is written");
        // Left behind: it's replaced, and made private again.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(create_temp(&tmp, true).unwrap());
        assert_eq!(mode(&tmp), 0o600);

        // A link where the temporary file goes: what it points to is untouched.
        let file = dir.join("notes.md");
        std::fs::write(&file, "old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let other = dir.join("other");
        std::fs::write(&other, "other").unwrap();
        std::os::unix::fs::symlink(&other, dir.join(format!(".notes.md.{}.linter-tmp", std::process::id()))).unwrap();
        write_atomic(&file, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(mode(&file), 0o640, "the file keeps its permissions");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "other");
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["notes.md", "other", "tmp"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_round_trip() {
        let dir = temp_dir("settings");
        let file = dir.join("sub").join("settings");
        assert_eq!(read_setting(&file, "theme"), None);
        write_setting(&file, "theme", "dark").unwrap();
        assert_eq!(read_setting(&file, "theme").as_deref(), Some("dark"));

        // Other keys are kept, and the key is replaced rather than repeated.
        std::fs::write(&file, "other = 1\ntheme=dark\n# note\n").unwrap();
        write_setting(&file, "theme", "light").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "other = 1\n# note\ntheme=light\n");
        assert_eq!(read_setting(&file, "theme").as_deref(), Some("light"));
        assert_eq!(read_setting(&file, "other").as_deref(), Some("1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_live_in_the_linter_directory() {
        if let Some(path) = settings_path() {
            assert!(path.ends_with(Path::new("linter").join("settings")), "{}", path.display());
        }
    }

    #[test]
    fn finish_block_leaves_untouched_blocks_alone() {
        let mut doc = Document::parse("```js\nunclosed at end of file\n");
        let src = doc.blocks[0].src.clone();
        assert_eq!(finish_block(&mut doc, 0, &src), None);
        assert!(finish_block(&mut doc, 0, "").is_some());
        assert_eq!(doc.to_text(), "```js\nunclosed at end of file\n```\n");
    }
}
