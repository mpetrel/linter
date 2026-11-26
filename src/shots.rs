//! Headless screenshots of the real UI, driven by simulated input.
//! Run with: `SHOT_DIR=/some/dir cargo test shots -- --ignored --nocapture`

use super::*;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::Rgb8Pixel;
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, PhysicalSize};

const W: u32 = 1040;
const H: u32 = 900;

struct TestPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
    /// The clipboard, which the text fields copy to and paste from.
    clipboard: RefCell<String>,
}

thread_local! {
    /// The X11/Wayland primary selection of the test platform: what a text
    /// field selects with the mouse, and a middle click pastes.
    static PRIMARY: RefCell<String> = RefCell::default();
}

impl TestPlatform {
    fn install(window: &Rc<MinimalSoftwareWindow>) {
        let platform = TestPlatform { window: window.clone(), start: Instant::now(), clipboard: RefCell::default() };
        slint::platform::set_platform(Box::new(platform)).unwrap();
    }
}

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
    fn set_clipboard_text(&self, text: &str, clipboard: slint::platform::Clipboard) {
        match clipboard {
            slint::platform::Clipboard::DefaultClipboard => *self.clipboard.borrow_mut() = text.to_owned(),
            slint::platform::Clipboard::SelectionClipboard => PRIMARY.with(|p| *p.borrow_mut() = text.to_owned()),
            _ => {}
        }
    }
    fn clipboard_text(&self, clipboard: slint::platform::Clipboard) -> Option<String> {
        match clipboard {
            slint::platform::Clipboard::DefaultClipboard => Some(self.clipboard.borrow().clone()),
            slint::platform::Clipboard::SelectionClipboard => Some(PRIMARY.with(|p| p.borrow().clone())),
            _ => None,
        }
    }
}

struct Harness {
    window: Rc<MinimalSoftwareWindow>,
    app: slint::Weak<App>,
    dir: PathBuf,
}

impl Harness {
    /// Runs timers and draws frames, as the event loop would. Drawing matters:
    /// the document list creates the rows in view while it lays out.
    fn settle(&self) {
        let mut buf = vec![Rgb8Pixel::default(); (W * H) as usize];
        for _ in 0..4 {
            slint::platform::update_timers_and_animations();
            std::thread::sleep(Duration::from_millis(5));
            self.window.request_redraw();
            self.window.draw_if_needed(|r| {
                r.render(&mut buf, W as usize);
            });
        }
    }

    /// A mouse wheel turn over the document. Parks the focus first, as the
    /// winit event hook does in the app.
    fn scroll(&self, delta_y: f32) {
        if let Some(app) = self.app.upgrade() {
            app.invoke_park_focus();
        }
        let position = LogicalPosition::new(300.0, 400.0);
        self.window.dispatch_event(WindowEvent::PointerScrolled { position, delta_x: 0.0, delta_y });
        self.settle();
    }

    fn snap(&self, name: &str) {
        self.settle();
        let mut buf = vec![Rgb8Pixel::default(); (W * H) as usize];
        self.window.request_redraw();
        self.window.draw_if_needed(|r| {
            r.render(&mut buf, W as usize);
        });
        let mut ppm = format!("P6 {W} {H} 255\n").into_bytes();
        for p in &buf {
            ppm.extend([p.r, p.g, p.b]);
        }
        let ppm_path = self.dir.join(format!("{name}.ppm"));
        std::fs::write(&ppm_path, ppm).unwrap();
        let png = self.dir.join(format!("{name}.png"));
        std::process::Command::new("magick").arg(&ppm_path).arg(&png).status().unwrap();
        std::fs::remove_file(ppm_path).unwrap();
        println!("wrote {}", png.display());
    }

    fn click(&self, x: f32, y: f32) {
        let position = LogicalPosition::new(x, y);
        self.window.dispatch_event(WindowEvent::PointerMoved { position });
        self.settle();
        self.button(position, true);
        self.button(position, false);
        self.settle();
    }

    /// The left button goes down or up at `position`. Tells the window first,
    /// as the winit event hook does in the app.
    fn button(&self, position: LogicalPosition, down: bool) {
        let app = self.app.upgrade().unwrap();
        let button = PointerEventButton::Left;
        if down {
            app.invoke_pointer_pressed(true);
            self.window.dispatch_event(WindowEvent::PointerPressed { position, button });
        } else {
            app.invoke_pointer_released();
            self.window.dispatch_event(WindowEvent::PointerReleased { position, button });
        }
    }

    /// A middle click at `position`, which pastes the primary selection.
    fn middle_click(&self, x: f32, y: f32) {
        let position = LogicalPosition::new(x, y);
        let button = PointerEventButton::Middle;
        self.window.dispatch_event(WindowEvent::PointerMoved { position });
        self.settle();
        self.app.upgrade().unwrap().invoke_pointer_pressed(false);
        self.window.dispatch_event(WindowEvent::PointerPressed { position, button });
        self.settle();
        self.window.dispatch_event(WindowEvent::PointerReleased { position, button });
        self.settle();
    }

    /// Presses the left button at the first point, moves through the others
    /// in steps, running the event loop after each, and lets go at the last.
    fn drag(&self, path: &[(f32, f32)]) {
        let at = |(x, y): (f32, f32)| LogicalPosition::new(x, y);
        self.window.dispatch_event(WindowEvent::PointerMoved { position: at(path[0]) });
        self.settle();
        self.button(at(path[0]), true);
        self.settle();
        for pair in path.windows(2) {
            let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
            for step in 1..=6 {
                let t = step as f32 / 6.0;
                self.window.dispatch_event(WindowEvent::PointerMoved { position: at((x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)) });
                self.settle();
            }
        }
        self.button(at(path[path.len() - 1]), false);
        self.settle();
    }

    /// Holds `key` (Shift, say) while `f` runs.
    fn holding(&self, key: Key, f: impl FnOnce()) {
        let key: SharedString = key.into();
        self.window.dispatch_event(WindowEvent::KeyPressed { text: key.clone() });
        f();
        self.window.dispatch_event(WindowEvent::KeyReleased { text: key });
        self.settle();
    }

    fn key(&self, text: impl Into<SharedString>) {
        self.press(text);
        self.settle();
    }

    /// A key, without running the event loop after it.
    fn press(&self, text: impl Into<SharedString>) {
        let text = text.into();
        self.window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        self.window.dispatch_event(WindowEvent::KeyReleased { text });
    }

    /// Runs the event loop for a while: long enough for the window to see
    /// that the list stopped moving.
    fn wait(&self) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            self.settle();
        }
    }

    fn ctrl(&self, text: &str) {
        let ctrl: SharedString = Key::Control.into();
        self.window.dispatch_event(WindowEvent::KeyPressed { text: ctrl.clone() });
        self.key(text);
        self.window.dispatch_event(WindowEvent::KeyReleased { text: ctrl });
        self.settle();
    }

    fn type_text(&self, s: &str) {
        for c in s.chars() {
            self.key(c.to_string());
        }
    }
}

#[test]
#[ignore]
fn shots() {
    let dir = PathBuf::from(std::env::var("SHOT_DIR").unwrap_or_else(|_| "target/shots".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);

    let app = App::new().unwrap();
    if let Some(mono) = monospace_family() {
        app.global::<Theme>().set_mono(mono.into());
    }
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/sample.md");
    let original = std::fs::read_to_string(&sample).unwrap();
    editor.borrow_mut().load(sample);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir };

    h.snap("01-rendered");

    // Click into the "## A second-level heading" block.
    let pos = std::env::var("CLICK").ok();
    let (x, y) = pos
        .as_deref()
        .and_then(|p| p.split_once(','))
        .map(|(x, y)| (x.parse().unwrap(), y.parse().unwrap()))
        .unwrap_or((232.0, 237.0));
    h.click(x, y);
    h.snap("02-heading-raw");
    println!("editing = {:?}", editor.borrow().editing);
    h.type_text("X");
    println!("after click+X: {:?}", editor.borrow().editing.map(|i| editor.borrow().doc.blocks[i].src.clone()));
    h.key(SharedString::from(Key::Backspace));

    // Type another '#' at the start: H2 -> H3.
    h.key(SharedString::from(Key::Home));
    h.type_text("#");
    h.snap("03-typed-hash");
    println!("block src = {:?}", editor.borrow().editing.map(|i| editor.borrow().doc.blocks[i].src.clone()));

    // Leave edit mode.
    h.key(SharedString::from(Key::Escape));
    h.snap("04-rerendered");

    // Ctrl+= promotes back to H2 after re-entering.
    h.click(x, y);
    h.ctrl("=");
    h.snap("05-promoted");
    println!("block src = {:?}", editor.borrow().editing.map(|i| editor.borrow().doc.blocks[i].src.clone()));
    h.key(SharedString::from(Key::Escape));

    // List: click "with nesting", go to the end, Enter continues the list.
    h.click(200.0, 361.0);
    h.snap("06-list-raw");
    h.key(SharedString::from(Key::End));
    h.key(SharedString::from(Key::Return));
    h.type_text("typed item");
    h.snap("07-list-new-item");
    h.key(SharedString::from(Key::Escape));
    h.snap("08-list-rendered");
    // Mouse wheel over a block scrolls the document.
    h.scroll(-400.0);
    h.snap("09-scrolled");
    h.scroll(400.0);
    let list_src = editor.borrow().doc.blocks[4].src.clone();
    println!("list src = {list_src:?}");
    assert!(list_src.contains("  - with nesting\n  - typed item"));
    // Undo that edit through the text field to keep the round-trip check.
    h.click(200.0, 361.0);
    h.key(SharedString::from(Key::End));
    for _ in 0.."\n  - typed item".len() {
        h.key(SharedString::from(Key::Delete));
    }
    h.key(SharedString::from(Key::Escape));

    let text = editor.borrow().doc.to_text();
    println!("round trip unchanged: {}", text == original);
    if text != original {
        for (a, b) in text.lines().zip(original.lines()) {
            if a != b {
                println!("- {b:?}\n+ {a:?}");
            }
        }
    }
}

#[test]
#[ignore]
fn shots_typing() {
    let dir = PathBuf::from(std::env::var("SHOT_DIR").unwrap_or_else(|_| "target/shots".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    editor.borrow_mut().start_blank();
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir };
    h.snap("10-blank");

    h.type_text("# My notes");
    h.key(SharedString::from(Key::Return));
    h.type_text("First paragraph with **bold** text.");
    h.key(SharedString::from(Key::Return));
    h.type_text("- one");
    h.key(SharedString::from(Key::Return));
    h.type_text("two");
    h.key(SharedString::from(Key::Return));
    h.key(SharedString::from(Key::Return)); // empty item ends the list
    h.type_text("> quoted");
    h.key(SharedString::from(Key::Return));
    h.key(SharedString::from(Key::Return));
    h.type_text("```python");
    h.key(SharedString::from(Key::Return));
    h.type_text("print(1)");
    h.snap("11-typing-code");
    h.ctrl("\n");
    h.key(SharedString::from(Key::DownArrow));
    h.type_text("After the code.");
    h.key(SharedString::from(Key::Escape));
    h.snap("12-typed-doc");
    let text = editor.borrow().doc.to_text();
    println!("{text}");
    assert_eq!(
        text,
        "# My notes\n\nFirst paragraph with **bold** text.\n\n- one\n- two\n\n> quoted\n\n```python\nprint(1)\n```\n\nAfter the code.\n"
    );

    // Arrow navigation: from the last block, Up moves into the code block.
    h.click(300.0, 60.0 + 36.0);
    h.key(SharedString::from(Key::Home));
    h.type_text("#");
    println!("heading now: {:?}", editor.borrow().doc.blocks[0].src);
    h.key(SharedString::from(Key::DownArrow));
    println!("after down, editing = {:?}", editor.borrow().editing);
    h.key(SharedString::from(Key::Home));
    h.key(SharedString::from(Key::Backspace)); // merge paragraph into heading
    println!("after merge: {:?}", editor.borrow().doc.blocks[0].src);
    h.snap("13-merged");

    // Ctrl+S writes the file (path preset so no dialog opens).
    let out = h.dir.join("saved.md");
    editor.borrow_mut().path = Some(out.clone());
    h.ctrl("s");
    let saved = std::fs::read_to_string(&out).unwrap();
    assert_eq!(saved, editor.borrow().doc.to_text());
    assert!(!editor.borrow().modified);
    println!("saved ok: {} bytes", saved.len());

    // Closing with unsaved changes shows the confirmation dialog.
    h.type_text(" more");
    h.window.dispatch_event(WindowEvent::CloseRequested);
    h.snap("14-confirm");
    assert!(app.get_confirm_open());
    h.click(W as f32 / 2.0 - 150.0, H as f32 / 2.0 + 40.0); // somewhere on the dialog card, not a button
    assert!(app.get_confirm_open());
}

#[test]
#[ignore]
fn perf_large_document() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    let sample = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/sample.md")).unwrap();
    let big = sample.repeat(60);
    let path = std::env::temp_dir().join(format!("linter-perf-{}.md", std::process::id()));
    std::fs::write(&path, &big).unwrap();
    let t = Instant::now();
    editor.borrow_mut().load(path.clone());
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir: std::env::temp_dir() };
    h.settle();
    println!("{} blocks, {} KB: load + first frame {:?}", editor.borrow().doc.blocks.len(), big.len() / 1024, t.elapsed());
    h.click(232.0, 237.0);
    let t = Instant::now();
    for _ in 0..20 {
        h.key("x");
    }
    println!("20 keystrokes (incl. 4x settle each): {:?}", t.elapsed());
    let t = Instant::now();
    h.key(SharedString::from(Key::Escape));
    println!("commit: {:?}", t.elapsed());

    // The clipboard goes through a hidden text field, which lays the text out.
    for (name, text) in [("sample x60", big.clone()), ("sample x600", sample.repeat(600))] {
        std::fs::write(&path, &text).unwrap();
        editor.borrow_mut().load(path.clone());
        h.settle();
        let t = Instant::now();
        h.ctrl("a");
        println!("{name}, {} KB: Ctrl+A {:?}", text.len() / 1024, t.elapsed());
        let t = Instant::now();
        h.ctrl("c");
        println!("  Ctrl+C {:?}", t.elapsed());
        let shift: SharedString = Key::Shift.into();
        h.window.dispatch_event(WindowEvent::KeyPressed { text: shift.clone() });
        let t = Instant::now();
        h.ctrl("c");
        println!("  Ctrl+Shift+C {:?}", t.elapsed());
        h.window.dispatch_event(WindowEvent::KeyReleased { text: shift });
        app.invoke_copy_text(text.as_str().into());
        let t = Instant::now();
        h.ctrl("v");
        println!("  Ctrl+V over it all {:?}, {} blocks", t.elapsed(), editor.borrow().doc.blocks.len());
    }
    std::fs::remove_file(&path).unwrap();
}

/// Fixes from the code review that need the real UI. No screenshots, so it
/// doesn't need ImageMagick.
#[test]
#[ignore]
fn shots_review() {
    let dir = std::env::temp_dir().join(format!("linter-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir: dir.clone() };
    let open = |name: &str, text: &str| {
        let file = dir.join(name);
        std::fs::write(&file, text).unwrap();
        editor.borrow_mut().load(file.clone());
        h.settle();
        file
    };
    let edit = |i: usize| {
        editor.borrow_mut().request_edit(Some(i), Some(Cursor::End));
        h.settle();
    };

    // After Delete merges the next block in, the caret is on the first line:
    // Down moves inside the merged block instead of jumping to the next one.
    open("merge.md", "intro\n\na long line\nsecond line\n\nthird\n");
    edit(0);
    h.key(Key::Delete);
    assert_eq!(editor.borrow().doc.blocks[0].src, "introa long line\nsecond line");
    h.key(Key::DownArrow);
    assert_eq!(editor.borrow().editing, Some(0), "Down jumped out of the merged block");
    h.key(Key::DownArrow);
    assert_eq!(editor.borrow().editing, Some(1), "Down on the last line moves to the next block");
    h.key(Key::Escape);

    // Ctrl+S while a code block is open writes it with its fence closed, and
    // finishing the edit afterwards doesn't leave unsaved changes behind.
    let file = open("fence.md", "```js\ncode\n```\n\nafter\n");
    edit(0);
    for _ in 0..4 {
        h.key(Key::Backspace);
    }
    h.key(Key::Return);
    h.type_text("more");
    assert_eq!(editor.borrow().doc.blocks[0].src, "```js\ncode\nmore");
    h.ctrl("s");
    let expected = "```js\ncode\nmore\n```\n\nafter\n";
    assert_eq!(std::fs::read_to_string(&file).unwrap(), expected);
    assert!(!editor.borrow().modified);
    h.key(Key::Escape);
    assert_eq!(editor.borrow().doc.to_text(), expected);
    assert!(!editor.borrow().modified);

    // Undoing an edit by hand clears the modified flag when the block is left.
    edit(1);
    h.type_text("x");
    assert!(editor.borrow().modified);
    h.key(Key::Backspace);
    h.key(Key::Escape);
    assert!(!editor.borrow().modified);

    // "Don't Save" keeps the changes marked as unsaved when the other file
    // can't be opened, so closing still asks.
    edit(1);
    h.type_text("!");
    h.key(Key::Escape);
    let binary = dir.join("binary.md");
    std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
    assert!(!editor.borrow_mut().guard(Pending::Open(Some(binary))));
    assert!(app.get_confirm_open());
    editor.borrow_mut().confirm(1);
    assert!(app.get_status_error());
    assert!(editor.borrow().modified);
    assert_eq!(editor.borrow().doc.blocks[1].src, "after!");
    assert!(!editor.borrow_mut().guard(Pending::Close), "closing must ask again");
    editor.borrow_mut().confirm(2);

    // Tab over the lines of a code block indents them, the fences aside, and
    // keeps them selected; Shift+Tab takes the indentation off again.
    open("tab.md", "```\na\nb\n```\n");
    edit(0);
    h.ctrl("a");
    h.key(Key::Tab);
    assert_eq!(editor.borrow().doc.blocks[0].src, "```\n    a\n    b\n```");
    let shift = SharedString::from(Key::Shift);
    h.window.dispatch_event(WindowEvent::KeyPressed { text: shift.clone() });
    h.key(Key::Tab);
    h.window.dispatch_event(WindowEvent::KeyReleased { text: shift });
    assert_eq!(editor.borrow().doc.blocks[0].src, "```\na\nb\n```");
    h.key(Key::Escape);

    std::fs::remove_dir_all(&dir).unwrap();
    println!("review checks passed");
}

/// The document list only creates the rows in view. Editing has to keep
/// working across that: moving to blocks that have no row yet, scrolling the
/// block being edited out of view and typing, saving or leaving edit mode
/// meanwhile, and scrolling it back. No screenshots.
#[test]
#[ignore]
fn shots_large_document() {
    let dir = std::env::temp_dir().join(format!("linter-large-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir: dir.clone() };
    let src = |i: usize| editor.borrow().doc.blocks[i].src.clone();
    let editing = || editor.borrow().editing;

    // A table taller than 32767 pixels: drawing it with a rounded clip made the
    // software renderer panic.
    let mut table = String::from("| a | b |\n| - | - |\n");
    for i in 0..1200 {
        table.push_str(&format!("| {i} | x |\n"));
    }
    let file = dir.join("table.md");
    std::fs::write(&file, format!("{table}\nafter\n")).unwrap();
    editor.borrow_mut().load(file);
    h.settle();
    h.scroll(-30000.0);
    h.scroll(-1_000_000.0);

    let text: String = (0..400).map(|i| format!("Paragraph {i}.\n\n")).collect();
    let file = dir.join("large.md");
    std::fs::write(&file, &text).unwrap();
    editor.borrow_mut().load(file.clone());
    h.settle();

    // A key typed right after moving down, before the list has drawn the block
    // moved to, goes where the move put the caret: at the start.
    editor.borrow_mut().request_edit(Some(0), Some(Cursor::End));
    h.settle();
    let press = |text: SharedString| {
        h.window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        h.window.dispatch_event(WindowEvent::KeyReleased { text });
    };
    for i in 1..=40 {
        press(Key::DownArrow.into());
        press("K".into());
        h.settle();
        assert_eq!(editing(), Some(i));
        assert_eq!(src(i), format!("KParagraph {i}."));
        h.key(Key::Backspace);
        assert_eq!(src(i), format!("Paragraph {i}."));
    }
    // The same when the block has no row at all yet: the key is typed in by
    // Rust, then the block comes into view with its caret after the key.
    editor.borrow_mut().request_edit(Some(300), Some(Cursor::At(0)));
    slint::platform::update_timers_and_animations(); // change handlers, but no layout yet
    press("K".into());
    h.settle();
    assert_eq!(src(300), "KParagraph 300.");
    h.type_text("L");
    assert_eq!(src(300), "KLParagraph 300.", "the text field has the focus, after the K");
    h.key(Key::Escape);

    // Down from the first block walks into blocks far below the first screen.
    editor.borrow_mut().request_edit(Some(0), Some(Cursor::End));
    h.settle();
    for _ in 0..80 {
        h.key(Key::DownArrow);
    }
    assert_eq!(editing(), Some(80));
    h.type_text("X");
    assert_eq!(src(80), "XParagraph 80.", "the text field of the block moved to has the focus");
    for _ in 0..30 {
        h.key(Key::UpArrow);
    }
    assert_eq!(editing(), Some(50));
    h.type_text("Y");
    assert_eq!(src(50), "Paragraph 50.Y");

    // Scrolled far away, the block has no text field. Typing goes in at its
    // caret and brings it back; then its text field has the focus again.
    h.scroll(-20000.0);
    h.type_text("Z");
    assert_eq!(src(50), "Paragraph 50.YZ");
    h.type_text("W");
    assert_eq!(src(50), "Paragraph 50.YZW", "typing continues in the text field");

    // Ctrl+S and Esc work while the block is out of view.
    h.scroll(-20000.0);
    h.ctrl("s");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), editor.borrow().doc.to_text());
    assert!(!editor.borrow().modified);
    assert_eq!(editing(), Some(50), "saving keeps editing");
    h.key(Key::Escape);
    assert_eq!(editing(), None);

    // Scrolling away and back resumes at the same caret.
    editor.borrow_mut().request_edit(Some(10), Some(Cursor::End));
    h.settle();
    h.type_text("Q");
    h.key(Key::LeftArrow);
    h.scroll(-20000.0);
    h.scroll(1_000_000.0);
    h.type_text("R");
    assert_eq!(src(10), "Paragraph 10.RQ");
    h.key(Key::Escape);

    // A click on the page below a short document continues writing at its end.
    let file = dir.join("short.md");
    std::fs::write(&file, "one\n\ntwo\n").unwrap();
    editor.borrow_mut().load(file);
    h.settle();
    h.click(500.0, H as f32 - 80.0);
    assert_eq!(editing(), Some(1), "a click below a short document edits its last block");

    std::fs::remove_dir_all(&dir).unwrap();
    println!("large document checks passed");
}

/// Whether the badge of the block being edited ("text", "code", …) is on
/// screen. It's drawn in the accent colour in the left margin, so it shows
/// that the block has a row and that the row is in view.
fn edit_badge_on_screen(h: &Harness) -> bool {
    h.settle();
    let mut buf = vec![Rgb8Pixel::default(); (W * H) as usize];
    h.window.request_redraw();
    h.window.draw_if_needed(|r| {
        r.render(&mut buf, W as usize);
    });
    let (toolbar, status_bar) = (44, 26);
    (toolbar..H as usize - status_bar).any(|y| {
        buf[y * W as usize..][..130].iter().any(|p| (p.r, p.g, p.b) == (0x09, 0x69, 0xda))
    })
}

/// In a document whose blocks differ in height, the list places the rows it
/// creates by estimated heights, so a block's position changes as the list
/// scrolls. Editing must not rely on where a block was: walking down with the
/// arrow keys and typing after scrolling the block away both bring it into
/// view, and no key is lost. No screenshots.
#[test]
#[ignore]
fn shots_mixed_heights() {
    let dir = std::env::temp_dir().join(format!("linter-mixed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir: dir.clone() };
    let editing = || editor.borrow().editing;

    // A paragraph and a 25-line code block, 100 times.
    let mut text = String::new();
    for i in 0..100 {
        text.push_str(&format!("Paragraph {i}.\n\n```\n"));
        for l in 0..25 {
            text.push_str(&format!("code {i}.{l}\n"));
        }
        text.push_str("```\n\n");
    }
    let file = dir.join("mixed.md");
    std::fs::write(&file, &text).unwrap();
    let open = || {
        editor.borrow_mut().load(file.clone());
        h.settle();
    };

    // Down from the top: the block moved to is always on screen. This used to
    // get stuck about 20 blocks down, with every key swallowed.
    open();
    editor.borrow_mut().request_edit(Some(0), Some(Cursor::End));
    h.settle();
    for n in 1..=320 {
        h.key(Key::DownArrow);
        if n % 10 == 0 {
            assert!(edit_badge_on_screen(&h), "after {n} presses, editing {:?} is off screen", editing());
        }
    }
    assert!(editing().is_some_and(|i| i >= 22), "the walk got stuck at {:?}", editing());
    h.key(Key::Escape);

    // Edit a block, scroll it far away (down, or up to the top), and type:
    // the key goes in at its caret, the block comes back, and the next key
    // goes in after it.
    for (block, delta) in [(80, -40000.0), (160, 1_000_000.0)] {
        open();
        editor.borrow_mut().request_edit(Some(block), Some(Cursor::End));
        h.settle();
        assert!(edit_badge_on_screen(&h), "block {block} is brought into view to edit it");
        h.scroll(delta);
        assert!(!edit_badge_on_screen(&h), "block {block} scrolled away");
        h.type_text("Z");
        assert!(edit_badge_on_screen(&h), "typing brings block {block} back");
        h.type_text("W");
        let src = editor.borrow().doc.blocks[block].src.clone();
        assert_eq!(src, format!("Paragraph {}.ZW", block / 2), "no key is lost");
        h.key(Key::Escape);
    }

    std::fs::remove_dir_all(&dir).unwrap();
    println!("mixed heights checks passed");
}

/// Where the keys go while editing, around scrolling and the dialogs: the
/// window holds the focus for the block being edited while the list may
/// move, and no key may be lost or land somewhere else. No screenshots.
#[test]
#[ignore]
fn shots_focus() {
    let dir = std::env::temp_dir().join(format!("linter-focus-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir: dir.clone() };
    let src = |i: usize| editor.borrow().doc.blocks[i].src.clone();
    let open = |name: &str, text: &str| {
        let file = dir.join(name);
        std::fs::write(&file, text).unwrap();
        editor.borrow_mut().load(file);
        h.settle();
    };
    let edit = |i: usize| {
        editor.borrow_mut().request_edit(Some(i), Some(Cursor::End));
        h.settle();
    };
    let long: String = (0..400).map(|i| format!("Paragraph {i}.\n\n")).collect();

    // A wheel turn that doesn't move the list (the document is shorter than
    // the window): the text field takes the focus back, and handles the next
    // key itself.
    open("short.md", "one\n\ntwo\n");
    edit(0);
    h.scroll(-120.0);
    h.wait();
    h.press(Key::Backspace);
    assert_eq!(src(0), "on", "Backspace went to the text field");
    h.key(Key::Escape);

    // The block scrolled partly out of view keeps no focus (taking it would
    // scroll the list), but keys other than text aren't lost: they're played
    // to its text field once it's back, in order.
    let lines: String = (1..=25).map(|n| format!("Line {n} of a long paragraph\n")).collect();
    let more: String = (0..30).map(|i| format!("\nMore {i}.\n")).collect();
    open("partial.md", &format!("# Top\n\n{lines}end.\n{more}"));
    edit(1);
    h.scroll(-200.0);
    h.wait();
    h.press(Key::LeftArrow);
    h.press("X");
    h.wait();
    assert!(src(1).ends_with("endX."), "{:?}", src(1));
    h.key(Key::Backspace);
    assert!(src(1).ends_with("end."), "{:?}", src(1));
    h.key(Key::Escape);

    // A scroll that didn't start with a wheel turn (the scroll bar, say) drops
    // the row with the text field that has the focus. A key typed right after
    // is typed in, not lost.
    open("long.md", &long);
    edit(10);
    h.window.dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(300.0, 400.0), delta_x: 0.0, delta_y: -20000.0 });
    h.settle();
    h.press("Z");
    h.wait();
    assert_eq!(src(10), "Paragraph 10.Z");
    h.key(Key::Escape);

    // Ctrl+B pressed and let go while the block is scrolled away: once the
    // block is back, it's played to the text field as Ctrl+B, which makes the
    // selection bold instead of typing a "b" over it. Ctrl is up again after.
    edit(20);
    h.ctrl("a");
    h.scroll(-20000.0);
    let (ctrl, shift): (SharedString, SharedString) = (Key::Control.into(), Key::Shift.into());
    h.window.dispatch_event(WindowEvent::KeyPressed { text: ctrl.clone() });
    h.press("b");
    h.window.dispatch_event(WindowEvent::KeyReleased { text: ctrl.clone() });
    h.wait();
    assert_eq!(src(20), "**Paragraph 20.**");
    assert!(!app.get_modifiers().control);
    h.type_text("x");
    assert_eq!(src(20), "**x**", "typing replaces the selection, without Ctrl");
    h.key(Key::Escape);

    // The other way round: Left pressed alone, then Shift held before the
    // block is back. Left moves the caret instead of selecting, and Shift is
    // still held after.
    edit(21);
    h.scroll(-20000.0);
    h.press(Key::LeftArrow);
    h.window.dispatch_event(WindowEvent::KeyPressed { text: shift.clone() });
    h.wait();
    assert!(app.get_modifiers().shift);
    h.press("X");
    h.window.dispatch_event(WindowEvent::KeyReleased { text: shift });
    h.settle();
    assert_eq!(src(21), "Paragraph 21X.");
    h.key(Key::Escape);

    // Closing the About dialog leaves the list where it is.
    edit(10);
    h.scroll(-20000.0);
    assert!(!edit_badge_on_screen(&h));
    h.click(1000.0, 22.0);
    assert!(app.get_about_open(), "the About button opened the dialog");
    h.key(Key::Escape);
    assert!(!app.get_about_open());
    h.wait();
    assert!(!edit_badge_on_screen(&h), "closing About didn't scroll to the block being edited");
    h.type_text("Y");
    assert!(edit_badge_on_screen(&h), "typing did");
    assert_eq!(src(10), "Paragraph 10.ZY");
    h.key(Key::Escape);

    // The unsaved changes dialog: it replaces the About dialog, nothing is
    // typed behind it, even after a wheel turn, and Esc cancels it.
    open("short.md", "one\n\ntwo\n");
    edit(0);
    h.type_text("A");
    h.click(1000.0, 22.0);
    assert!(app.get_about_open());
    assert!(!editor.borrow_mut().guard(Pending::Close));
    h.settle();
    assert!(app.get_confirm_open());
    assert!(!app.get_about_open(), "one dialog at a time");
    h.scroll(-300.0);
    h.wait();
    h.type_text("Q");
    assert_eq!(src(0), "oneA", "nothing is typed behind the dialog");
    h.key(Key::Escape);
    assert!(!app.get_confirm_open());
    assert!(editor.borrow().pending.is_none());
    h.wait();
    h.type_text("R");
    assert_eq!(src(0), "oneAR");

    std::fs::remove_dir_all(&dir).unwrap();
    println!("focus checks passed");
}

/// The theme menu, the dark theme, and leaving edit mode by clicking blank
/// space. Writes screenshots to `SHOT_DIR`.
#[test]
#[ignore]
fn shots_theme() {
    let dir = PathBuf::from(std::env::var("SHOT_DIR").unwrap_or_else(|_| "target/shots".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    app.set_version(env!("CARGO_PKG_VERSION").into());
    if let Some(mono) = monospace_family() {
        app.global::<Theme>().set_mono(mono.into());
    }
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    let settings = dir.join("settings-test").join("settings");
    let _ = std::fs::remove_file(&settings);
    wire_theme(&app, Some(settings.clone()));
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/sample.md");
    editor.borrow_mut().load(sample);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir };
    let dark = || app.global::<Theme>().get_dark();
    // The About button is at the right end of the toolbar, then Settings…,
    // then the Theme menu.
    let (about_x, about_y) = (W as f32 - 43.0, 22.0);
    let (menu_x, menu_y) = (W as f32 - 201.0, 22.0);
    let (item_x, light_y, dark_y) = (W as f32 - 248.0, 59.0, 89.0);

    h.snap("t1-light");
    h.click(menu_x, menu_y);
    h.snap("t2-menu-light");
    h.click(item_x, dark_y);
    assert!(dark(), "choosing Dark switches the theme");
    assert_eq!(read_setting(&settings, "theme").as_deref(), Some("dark"));
    h.snap("t3-dark");

    // Editing in the dark theme, then a click in the margin shows the
    // document fully rendered again.
    h.click(232.0, 237.0);
    assert!(editor.borrow().editing.is_some());
    h.snap("t4-dark-editing");
    h.click(30.0, 400.0);
    assert_eq!(editor.borrow().editing, None, "a click beside the document leaves edit mode");
    h.snap("t5-dark-preview");

    // Below the document: the first click leaves edit mode, the next one
    // continues writing at the end.
    h.click(232.0, 237.0);
    h.scroll(-100000.0);
    h.click(500.0, H as f32 - 60.0);
    assert_eq!(editor.borrow().editing, None, "a click below the document leaves edit mode");
    h.snap("t6-dark-bottom");
    h.click(500.0, H as f32 - 60.0);
    assert!(editor.borrow().editing.is_some(), "a second click below continues writing at the end");
    h.key(SharedString::from(Key::Escape));
    h.scroll(100000.0);

    h.click(menu_x, menu_y);
    h.snap("t7-menu-dark");
    h.click(item_x, light_y);
    assert!(!dark(), "choosing Light switches back");
    assert_eq!(read_setting(&settings, "theme").as_deref(), Some("light"));
    h.click(menu_x, menu_y);
    h.click(item_x, dark_y);

    // The unsaved-changes dialog in the dark theme.
    editor.borrow_mut().request_edit(Some(1), Some(Cursor::End));
    h.settle();
    h.type_text("!");
    h.window.dispatch_event(WindowEvent::CloseRequested);
    h.snap("t8-dark-dialog");
    editor.borrow_mut().confirm(2);

    // The About dialog, opened while a block is being edited: keys don't
    // reach the block, and closing it gives the block the focus back.
    let before = editor.borrow().doc.blocks[1].src.clone();
    h.click(about_x, about_y);
    assert!(app.get_about_open(), "the About button opens the dialog");
    h.snap("t9-about-dark");
    h.type_text("x");
    assert_eq!(editor.borrow().doc.blocks[1].src, before, "keys don't reach the block behind the dialog");
    h.key(SharedString::from(Key::Escape));
    assert!(!app.get_about_open(), "Esc closes the dialog");
    assert_eq!(editor.borrow().editing, Some(1), "closing the dialog doesn't stop editing");
    h.type_text("?");
    assert_eq!(editor.borrow().doc.blocks[1].src, format!("{before}?"), "the block gets the keys again");

    h.click(menu_x, menu_y);
    h.click(item_x, light_y);
    assert!(!dark());
    h.click(about_x, about_y);
    h.snap("t10-about-light");
    h.click(30.0, 400.0);
    assert!(!app.get_about_open(), "a click beside the dialog closes it");
}

/// Images in each format, from files and over HTTP, with the placeholder
/// while one loads and the reasons when one can't be shown.
#[test]
#[ignore]
fn shots_images() {
    use crate::images::tests::{encode, gradient, webp_bytes};
    use image::{DynamicImage, ImageFormat};
    use std::io::{Read, Write};
    use std::sync::{Arc, Condvar, Mutex};

    let dir = PathBuf::from(std::env::var("SHOT_DIR").unwrap_or_else(|_| "target/shots".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let docs = std::env::temp_dir().join(format!("linter-shots-images-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&docs);
    std::fs::create_dir_all(&docs).unwrap();
    let rgb = DynamicImage::ImageRgb8(gradient(320, 60));
    let rgba = DynamicImage::ImageRgba8(rgb.to_rgba8());
    for (name, image, format) in [("a.png", &rgba, ImageFormat::Png), ("b.jpg", &rgb, ImageFormat::Jpeg), ("c.gif", &rgba, ImageFormat::Gif), ("d.bmp", &rgb, ImageFormat::Bmp)] {
        std::fs::write(docs.join(name), encode(image, format)).unwrap();
    }
    std::fs::write(docs.join("e.webp"), webp_bytes(320, 60, true)).unwrap();
    std::fs::write(docs.join("f.svg"), r##"<svg xmlns="http://www.w3.org/2000/svg" width="320" height="60"><rect width="320" height="60" rx="8" fill="#e8a33d"/></svg>"##).unwrap();
    // A big photo is scaled down while it's decoded.
    std::fs::write(docs.join("g.jpg"), encode(&DynamicImage::ImageRgb8(gradient(4000, 1000)), ImageFormat::Jpeg)).unwrap();

    // A server that answers the first request after a pause, so the
    // placeholder shows, and the second with a 404.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let png = encode(&DynamicImage::ImageRgb8(gradient(320, 60)), ImageFormat::Png);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let gate = release.clone();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let (gate, png) = (gate.clone(), png.clone());
            // Each on its own thread: the paused one mustn't hold up the other.
            std::thread::spawn(move || {
                let mut request = [0u8; 4096];
                let n = stream.read(&mut request).unwrap_or(0);
                if request[..n].starts_with(b"GET /remote.png") {
                    let (lock, cvar) = &*gate;
                    let _go = cvar.wait_while(lock.lock().unwrap(), |go| !*go).unwrap();
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", png.len());
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&png);
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            });
        }
    });

    let md = format!(
        "# Images\n\n![png](a.png)\n\n![jpeg](b.jpg)\n\n![gif](c.gif)\n\n![bmp](d.bmp)\n\n![webp](e.webp)\n\n![svg](f.svg)\n\n\
         ![remote]({server}/remote.png)\n\n![gone]({server}/gone.png)\n\n![missing](nope.png)\n\n![photo](g.jpg)\n"
    );
    let file = docs.join("images.md");
    std::fs::write(&file, md).unwrap();

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    editor.borrow_mut().load(file);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir };

    let row = |i: usize| editor.borrow().model.row_data(i).unwrap();
    let wait = |done: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "images didn't load");
            h.settle();
        }
    };
    // Blocks: 0 heading, then the images in order.
    wait(&|| (1..=6).all(|i| row(i).has_image) && !row(8).caption.contains("loading"));
    assert!(row(7).caption.ends_with("— loading…"), "{}", row(7).caption);
    h.snap("i1-loading");

    let (lock, cvar) = &*release;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
    wait(&|| row(7).has_image && row(10).has_image);
    assert!(row(8).caption.ends_with("/gone.png: the server answered 404 Not Found"), "{}", row(8).caption);
    assert!(row(9).caption.ends_with("nope.png: file not found"), "{}", row(9).caption);
    let photo = row(10).image.size();
    assert_eq!((photo.width, photo.height), (1640, 410), "the photo is scaled to the column at 2x");
    h.snap("i2-loaded");
    h.scroll(-100000.0);
    h.snap("i3-bottom");

    // On a screen at 3x, the photo is decoded again for the wider column,
    // and the old one is shown meanwhile.
    h.window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 3.0 });
    // The same pixels, for the frame buffer.
    h.window.set_size(PhysicalSize::new(W, H));
    editor.borrow().rescaled();
    assert!(row(10).has_image);
    wait(&|| row(10).image.size().width == 2460);
    assert_eq!(row(10).image.size().height, 615);
    let _ = std::fs::remove_dir_all(&docs);
}

/// Images load when their block comes into view, and beyond the memory
/// budget the ones out of view longest are dropped, from the blocks too. No
/// screenshots.
#[test]
#[ignore]
fn shots_images_in_view() {
    use crate::images::tests::{encode, gradient};
    use image::{DynamicImage, ImageFormat};

    let dir = std::env::temp_dir().join(format!("linter-images-in-view-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 400 × 300, shown as they are: 360 000 bytes each.
    for n in 0..4 {
        std::fs::write(dir.join(format!("{n}.png")), encode(&DynamicImage::ImageRgb8(gradient(400, 300)), ImageFormat::Png)).unwrap();
    }
    let paragraphs: String = (0..400).map(|i| format!("Paragraph {i}.\n\n")).collect();
    let file = dir.join("long.md");
    std::fs::write(&file, format!("# Top\n\n![0](0.png)\n\n![1](1.png)\n\n{paragraphs}![2](2.png)\n\n![3](3.png)\n")).unwrap();
    // Blocks: 0 the heading, 1 and 2 the images at the top, 403 and 404 the ones at the end.
    let (top, end) = ([1, 2], [403, 404]);

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    // Room for three of them.
    editor.borrow_mut().images = images::Images::with_budget(3 * 360_000 + 1000);
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir: dir.clone() };
    let shown = |i: usize| editor.borrow().model.row_data(i).unwrap().has_image;
    let wait = |done: &dyn Fn() -> bool, what: &str| {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            h.settle();
        }
    };

    editor.borrow_mut().load(file);
    wait(&|| top.iter().all(|&i| shown(i)), "the images in view load");
    h.wait();
    assert_eq!(editor.borrow().images.requested(), 2, "only the images in view were asked for");
    assert!(!end.iter().any(|&i| shown(i)));

    // At the end, its images load, and one shown at the top is let go of.
    h.scroll(-1_000_000.0);
    wait(&|| end.iter().all(|&i| shown(i)), "the images at the end load");
    wait(&|| top.iter().filter(|&&i| shown(i)).count() == 1, "one image at the top is dropped");

    // Back at the top, both load again; one at the end makes room.
    h.scroll(1_000_000.0);
    wait(&|| top.iter().all(|&i| shown(i)), "the images at the top load again");
    assert_eq!(end.iter().filter(|&&i| shown(i)).count(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
    println!("images in view checks passed");
}

/// The Settings dialog: Referer rules for web images, saved to the settings
/// file and used at once, so an image refused before shows.
#[test]
#[ignore]
fn shots_settings() {
    // The Settings button, and with two rules the first one's ×.
    const SETTINGS_X: f32 = W as f32 - 120.0;
    const REMOVE_X: f32 = 803.0;
    const FIRST_ROW_Y: f32 = 452.0;
    use crate::images::tests::{encode, gradient};
    use image::{DynamicImage, ImageFormat};
    use std::io::{Read, Write};

    let dir = PathBuf::from(std::env::var("SHOT_DIR").unwrap_or_else(|_| "target/shots".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let docs = std::env::temp_dir().join(format!("linter-shots-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&docs);
    std::fs::create_dir_all(&docs).unwrap();
    let settings = docs.join("config").join("settings");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "theme=light\n# hand-written\nreferer.example-cdn.net=https://blog.example.com/\n").unwrap();

    // Serves the image only with the Referer of one blog page.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let png = encode(&DynamicImage::ImageRgb8(gradient(320, 60)), ImageFormat::Png);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]).to_ascii_lowercase();
            if request.contains("\r\nreferer: https://blog.example/post\r\n") {
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", png.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&png);
            } else {
                let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        }
    });
    let file = docs.join("settings.md");
    std::fs::write(&file, format!("# Settings\n\nSome text.\n\n![blog]({server}/page.png)\n")).unwrap();

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    wire_settings(&app, &editor, Some(settings.clone()));
    editor.borrow_mut().load(file);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir };

    let row = |i: usize| editor.borrow().model.row_data(i).unwrap();
    let wait = |done: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "images didn't load");
            h.settle();
        }
    };
    // Blocks: 0 heading, 1 paragraph, 2 the image.
    wait(&|| row(2).caption.contains("403"));
    h.snap("s0-refused");
    assert!(row(2).caption.ends_with("in Settings, add 127.0.0.1 with the Referer it wants"), "{}", row(2).caption);

    // Opening Settings while a block is being edited stops editing, and the
    // rule from the file is shown.
    editor.borrow_mut().request_edit(Some(1), Some(Cursor::End));
    h.settle();
    let paragraph = editor.borrow().doc.blocks[1].src.clone();
    h.click(SETTINGS_X, 22.0);
    assert!(app.get_settings_open(), "the Settings button opens the dialog");
    assert_eq!(editor.borrow().editing, None);
    let rules = || {
        let model = app.get_referer_rules();
        model.iter().map(|r| (r.domain.to_string(), r.referer.to_string())).collect::<Vec<_>>()
    };
    let rule = |d: &str, r: &str| (d.to_owned(), r.to_owned());
    assert_eq!(rules(), [rule("example-cdn.net", "https://blog.example.com/")]);
    h.snap("s1-settings");

    // Add Site gives the new row's domain field the focus; Tab moves to its
    // Referer, Enter saves. The other lines of the file are kept.
    h.click(260.0, 513.0);
    h.type_text("127.0.0.1");
    h.key(SharedString::from(Key::Tab));
    h.type_text("https://blog.example/post");
    assert_eq!(rules()[1], rule("127.0.0.1", "https://blog.example/post"));
    h.snap("s2-added");
    h.key(SharedString::from(Key::Return));
    assert!(!app.get_settings_open(), "Enter saves and closes the dialog");
    let saved = "theme=light\n# hand-written\nreferer.example-cdn.net=https://blog.example.com/\nreferer.127.0.0.1=https://blog.example/post\n";
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), saved);
    // The image refused before is asked for again, with the new Referer.
    wait(&|| row(2).has_image);
    h.snap("s3-shown");

    // A rule without a Referer isn't saved. Tab doesn't take the keys to the
    // document behind the dialog, and Esc leaves the file as it was.
    h.click(SETTINGS_X, 22.0);
    assert_eq!(rules().len(), 2, "read back from the file");
    app.invoke_add_referer_rule();
    h.settle();
    h.type_text("cdn.example.org");
    h.key(SharedString::from(Key::Return));
    assert!(app.get_settings_open());
    assert_eq!(app.get_settings_error(), "Give the Referer to send with the images of cdn.example.org.");
    h.snap("s4-error");
    for _ in 0..8 {
        h.key(SharedString::from(Key::Tab));
        h.type_text("z");
    }
    h.key(SharedString::from(Key::Escape));
    assert!(!app.get_settings_open(), "Esc closes the dialog");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), saved);
    assert_eq!(editor.borrow().doc.blocks[1].src, paragraph, "keys don't reach the document");

    // × removes a rule. In the dark theme.
    app.invoke_apply_theme(true);
    h.click(SETTINGS_X, 22.0);
    assert_eq!(rules().len(), 2, "what wasn't saved is gone");
    h.snap("s5-dark");
    h.click(REMOVE_X, FIRST_ROW_Y);
    assert_eq!(rules(), [rule("127.0.0.1", "https://blog.example/post")]);
    app.invoke_save_settings();
    h.settle();
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        "theme=light\n# hand-written\nreferer.127.0.0.1=https://blog.example/post\n"
    );
    let _ = std::fs::remove_dir_all(&docs);
}

/// Selecting blocks as a whole: dragging across them, Ctrl+A twice,
/// Shift+click and Shift+Up/Down; what Ctrl+C, Ctrl+Shift+C, Ctrl+X, Delete
/// and typing do with them; and pasting Markdown, which is split into blocks.
/// Screenshots only with `SHOT_DIR`.
#[test]
#[ignore]
fn shots_selection() {
    let shots = std::env::var_os("SHOT_DIR").map(PathBuf::from);
    let dir = std::env::temp_dir().join(format!("linter-selection-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir: shots.clone().unwrap_or_else(|| dir.clone()) };
    let snap = |name: &str| {
        if shots.is_some() {
            h.snap(name);
        }
    };
    let open = |text: &str| {
        let file = dir.join("selection.md");
        std::fs::write(&file, text).unwrap();
        editor.borrow_mut().load(file);
        h.settle();
    };
    let selected = || editor.borrow().selected();
    let editing = || editor.borrow().editing;
    let srcs = || editor.borrow().doc.blocks.iter().map(|b| b.src.clone()).collect::<Vec<_>>();
    let clipboard = || app.invoke_clipboard_text().to_string();
    let shift = |f: &dyn Fn()| h.holding(Key::Shift, f);
    let status = || app.get_status().to_string();

    // Blocks, as rendered, at x = 300: 0 heading at y 82–150, 1 paragraph
    // 154–185, 2 list 186–241, 3 quote 242–273, 4 code 274–329, 5 paragraph below.
    let text = "# Title\n\nFirst paragraph with **bold** text.\n\n- one\n- two\n\n> quoted\n\n```\ncode\n```\n\nLast paragraph.\n";
    let y = [110.0, 170.0, 205.0, 258.0, 300.0, 345.0];
    open(text);

    // Ctrl+A in a block selects its text, and once more the whole document.
    h.click(300.0, y[1]);
    assert_eq!(editing(), Some(1));
    h.ctrl("a");
    assert_eq!(selected(), None, "the first Ctrl+A selects the text of the block");
    assert_eq!(status(), "Press Ctrl+A again to select the whole document");
    h.ctrl("a");
    assert_eq!(selected(), Some(0..=5));
    assert_eq!(editing(), None);
    assert_eq!(app.get_sel_first(), 0);
    assert_eq!(app.get_sel_last(), 5);
    snap("sel1-all");
    // Ctrl+C copies the Markdown, Ctrl+Shift+C the text as it reads.
    h.ctrl("c");
    assert_eq!(clipboard(), text.trim_end());
    assert_eq!(status(), "Copied 6 blocks as Markdown");
    shift(&|| h.ctrl("c"));
    assert_eq!(clipboard(), "Title\n\nFirst paragraph with bold text.\n\n• one\n• two\n\nquoted\n\ncode\n\nLast paragraph.");
    assert_eq!(status(), "Copied 6 blocks as plain text");
    h.key(Key::Escape);
    assert_eq!(selected(), None);
    assert_eq!(app.get_sel_first(), -1);

    // A drag from the text of one block on to others selects them whole.
    h.drag(&[(300.0, y[1]), (300.0, y[3])]);
    assert_eq!(selected(), Some(1..=3));
    assert_eq!(editing(), None, "editing stopped once the drag left the block");
    snap("sel2-dragged");
    app.invoke_apply_theme(true);
    snap("sel2-dragged-dark");
    app.invoke_apply_theme(false);
    h.ctrl("c");
    assert_eq!(clipboard(), "First paragraph with **bold** text.\n\n- one\n- two\n\n> quoted");
    // Shift+Down and Shift+Up move its end.
    shift(&|| h.key(Key::DownArrow));
    assert_eq!(selected(), Some(1..=4));
    shift(&|| {
        for _ in 0..4 {
            h.key(Key::UpArrow);
        }
    });
    assert_eq!(editor.borrow().selection, Some((1, 0)), "past the start of the drag");
    // Tab doesn't move the focus on to a block.
    h.key(Key::Tab);
    assert_eq!((editor.borrow().selection, editing()), (Some((1, 0)), None));
    // Down without Shift edits the last block selected.
    h.key(Key::DownArrow);
    assert_eq!((selected(), editing()), (None, Some(1)));
    h.key(Key::Escape);

    // Within one block, a drag selects text, as before, and Ctrl+Shift+C
    // copies it without the Markdown.
    h.drag(&[(200.0, y[1]), (420.0, y[1])]);
    assert_eq!((selected(), editing()), (None, Some(1)));
    shift(&|| h.ctrl("c"));
    let copied = clipboard();
    assert!(copied.contains("bold") && !copied.contains('*'), "{copied:?}");
    assert_eq!(status(), "Copied the selection as plain text");
    h.key(Key::Escape);

    // A drag from the margin that comes back to its block ends with that
    // block selected: the release isn't a click on the margin.
    h.drag(&[(40.0, y[1]), (40.0, y[3]), (40.0, y[1] + 4.0)]);
    assert_eq!(selected(), Some(1..=1));
    h.click(40.0, y[1]);
    assert_eq!(selected(), None, "a click in the margin drops the selection");

    // Shift+click extends from the block being edited, and again from the
    // same start; a click without Shift edits the block clicked.
    h.click(300.0, y[5]);
    assert_eq!(editing(), Some(5));
    shift(&|| h.click(300.0, y[1]));
    assert_eq!(editor.borrow().selection, Some((5, 1)));
    shift(&|| h.click(300.0, y[3]));
    assert_eq!(editor.borrow().selection, Some((5, 3)));
    // The text field clicked on doesn't keep the focus: keys act on the
    // selection, and none reaches the block's hidden text.
    shift(&|| h.key(Key::UpArrow));
    assert_eq!(editor.borrow().selection, Some((5, 2)));
    h.click(300.0, y[3]);
    assert_eq!((selected(), editing()), (None, Some(3)));
    assert_eq!(srcs(), text.trim_end().split("\n\n").collect::<Vec<_>>());
    h.key(Key::Escape);
    // Shift+click in the margin extends too.
    h.click(300.0, y[1]);
    shift(&|| h.click(40.0, y[2]));
    assert_eq!(editor.borrow().selection, Some((1, 2)));
    h.key(Key::Escape);

    // Shift+Down at the end of a block selects it and the next one.
    h.click(300.0, y[1]);
    h.key(Key::End);
    shift(&|| h.key(Key::DownArrow));
    assert_eq!(editor.borrow().selection, Some((1, 2)));
    shift(&|| h.key(Key::DownArrow));
    assert_eq!(editor.borrow().selection, Some((1, 3)));

    // Delete removes them, leaving an empty paragraph to type in.
    h.key(Key::Delete);
    assert_eq!(srcs(), ["# Title", "", "```\ncode\n```", "Last paragraph."]);
    assert_eq!(editing(), Some(1));
    h.type_text("New");
    assert_eq!(srcs()[1], "New");
    // Ctrl+Z: the text field undoes the typing, then the blocks come back,
    // selected.
    h.ctrl("z");
    assert_eq!(srcs()[1], "");
    h.ctrl("z");
    assert_eq!(srcs(), text.trim_end().split("\n\n").collect::<Vec<_>>());
    assert_eq!((editor.borrow().selection, editing()), (Some((1, 3)), None));
    assert!(!editor.borrow().modified);
    h.ctrl("z");
    assert_eq!(srcs().len(), 6, "once");
    h.key(Key::Escape);

    // Ctrl+X cuts; the empty paragraph left goes once editing stops.
    open(text);
    h.drag(&[(300.0, y[2]), (300.0, y[4])]);
    assert_eq!(selected(), Some(2..=4));
    h.ctrl("x");
    assert_eq!(clipboard(), "- one\n- two\n\n> quoted\n\n```\ncode\n```");
    assert_eq!(status(), "Cut 3 blocks");
    assert_eq!(srcs(), ["# Title", "First paragraph with **bold** text.", "", "Last paragraph."]);
    h.key(Key::Escape);
    assert_eq!(srcs(), ["# Title", "First paragraph with **bold** text.", "Last paragraph."]);
    assert!(editor.borrow().modified);
    // Ctrl+Z after leaving the block brings them back too.
    h.ctrl("z");
    assert_eq!(srcs().len(), 6);
    assert_eq!(selected(), Some(2..=4));
    h.ctrl("x");
    h.key(Key::Escape);

    // Typing replaces the blocks selected.
    h.drag(&[(300.0, y[1]), (300.0, 200.0)]);
    assert_eq!(selected(), Some(1..=2));
    h.type_text("ZY");
    assert_eq!(srcs(), ["# Title", "ZY"]);
    assert_eq!(editing(), Some(1));

    // Ctrl+V over them: the text pasted, as blocks. Editing goes on at its end.
    app.invoke_copy_text("## Pasted\r\n\r\nOne *two*\r\n\r\n- three".into());
    h.ctrl("a");
    h.ctrl("a");
    assert_eq!(selected(), Some(0..=1));
    h.ctrl("v");
    assert_eq!(srcs(), ["## Pasted", "One *two*", "- three"]);
    assert_eq!(editing(), Some(2));
    h.type_text("!");
    assert_eq!(srcs()[2], "- three!");
    h.key(Key::Escape);

    // A Markdown file pasted into a new document is shown as its blocks,
    // rendered but for the last one, where editing goes on.
    open("");
    assert_eq!(editing(), Some(0));
    let file = "# Notes\n\nA paragraph with `code`.\n\n1. first\n2. second\n\n```js\nlet x = 1;\n```\n\nThe end.\n";
    app.invoke_copy_text(file.into());
    h.ctrl("v");
    assert_eq!(srcs(), ["# Notes", "A paragraph with `code`.", "1. first\n2. second", "```js\nlet x = 1;\n```", "The end."]);
    assert_eq!(editing(), Some(4));
    // Ctrl+Z takes the paste back, and it can be pasted again.
    h.ctrl("z");
    assert_eq!((srcs(), editing()), (vec![String::new()], Some(0)));
    h.ctrl("v");
    assert_eq!(srcs().len(), 5);
    let kinds: Vec<_> = (0..5).map(|i| editor.borrow().model.row_data(i).unwrap().kind).collect();
    assert_eq!(kinds, [BlockKind::Heading, BlockKind::Paragraph, BlockKind::List, BlockKind::Code, BlockKind::Paragraph]);
    h.type_text("..");
    assert_eq!(srcs()[4], "The end...");
    snap("sel3-pasted");
    h.key(Key::Escape);
    assert_eq!(editor.borrow().doc.to_text(), file.replace("The end.", "The end..."));

    // Pasted into the middle of a paragraph: the caret stays after what was pasted.
    open("Hello world.\n");
    h.click(300.0, 82.0);
    h.key(Key::Home);
    for _ in 0.."Hello ".len() {
        h.key(Key::RightArrow);
    }
    app.invoke_copy_text("there.\n\n## Middle\n\nAnd ".into());
    h.ctrl("v");
    assert_eq!(srcs(), ["Hello there.", "## Middle", "And world."]);
    assert_eq!(editing(), Some(2));
    h.type_text("X");
    assert_eq!(srcs()[2], "And Xworld.");
    // Undone, the caret is back where the paste went.
    h.ctrl("z");
    h.ctrl("z");
    assert_eq!((srcs(), editing()), (vec!["Hello world.".to_owned()], Some(0)));
    h.type_text("big ");
    assert_eq!(srcs()[0], "Hello big world.");
    h.key(Key::Escape);

    // Code pasted into a code block stays code.
    open("```\n\n```\n");
    h.click(300.0, 82.0);
    h.key(Key::Home);
    h.key(Key::DownArrow);
    app.invoke_copy_text("a\n\nb".into());
    h.ctrl("v");
    assert_eq!(srcs(), ["```\na\n\nb\n```"]);
    h.key(Key::Escape);

    // A drag below the list scrolls it, and the selection grows with it.
    let long: String = (0..400).map(|i| format!("Paragraph {i}.\n\n")).collect();
    open(&long);
    let start = LogicalPosition::new(300.0, 100.0);
    h.window.dispatch_event(WindowEvent::PointerMoved { position: start });
    h.settle();
    h.button(start, true);
    h.settle();
    h.window.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(300.0, 700.0) });
    h.settle();
    let before = *selected().expect("blocks are selected").end();
    h.window.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(300.0, H as f32 + 60.0) });
    h.wait();
    let after = *selected().unwrap().end();
    assert!(after > before + 5, "the list scrolled while the button was held: {before} -> {after}");
    h.button(LogicalPosition::new(300.0, H as f32 + 60.0), false);
    h.settle();
    assert_eq!(*selected().unwrap().start(), 0);
    let end = *selected().unwrap().end();
    h.wait();
    assert_eq!(*selected().unwrap().end(), end, "scrolling stops with the release");

    // Dragging the scroll bar scrolls without selecting.
    h.key(Key::Escape);
    h.drag(&[(W as f32 - 5.0, 120.0), (W as f32 - 5.0, 400.0)]);
    assert_eq!(selected(), None);

    // Ctrl+A with nothing edited selects the document: here a table taller
    // than the software renderer can clip, which is drawn selected while
    // the list scrolls past it.
    let mut table = String::from("| a | b |\n| - | - |\n");
    for i in 0..1200 {
        table.push_str(&format!("| {i} | x |\n"));
    }
    open(&format!("{table}\nafter\n"));
    assert_eq!(editing(), None);
    h.ctrl("a");
    assert_eq!(selected(), Some(0..=1));
    h.scroll(-30000.0);
    h.scroll(-1_000_000.0);
    h.scroll(1_000_000.0);
    assert_eq!(selected(), Some(0..=1));

    std::fs::remove_dir_all(&dir).unwrap();
    println!("selection checks passed");
}

/// Fixes from the review of selecting blocks, pasting and undoing. No
/// screenshots.
#[test]
#[ignore]
fn shots_selection_review() {
    let dir = std::env::temp_dir().join(format!("linter-selection-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    TestPlatform::install(&window);
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, app: app.as_weak(), dir: dir.clone() };
    let open = |text: &str| {
        let file = dir.join("review.md");
        std::fs::write(&file, text).unwrap();
        editor.borrow_mut().load(file);
        h.settle();
    };
    let selected = || editor.borrow().selected();
    let editing = || editor.borrow().editing;
    let srcs = || editor.borrow().doc.blocks.iter().map(|b| b.src.clone()).collect::<Vec<_>>();
    let shift = |f: &dyn Fn()| h.holding(Key::Shift, f);
    let primary = || PRIMARY.with(|p| p.borrow().clone());
    // The rows show the document as it is.
    let rows_match = || {
        let ed = editor.borrow();
        ed.model.row_count() == ed.doc.blocks.len()
            && ed.doc.blocks.iter().enumerate().all(|(i, b)| ed.model.row_data(i).unwrap().source.as_str() == b.src)
    };

    // Shift+Up from a new, empty block selects the block above it, and only
    // that one: the empty block is gone once editing stops.
    open("A\n\nB\n\nC\n");
    h.click(300.0, 82.0);
    assert_eq!(editing(), Some(0));
    h.key(Key::End);
    h.key(Key::Return);
    assert_eq!(srcs(), ["A", "", "B", "C"]);
    assert_eq!(editing(), Some(1));
    shift(&|| h.key(Key::UpArrow));
    assert_eq!(selected(), Some(0..=0));
    assert_eq!(srcs(), ["A", "B", "C"]);
    h.key(Key::Delete);
    assert_eq!(srcs(), ["", "B", "C"]);
    h.key(Key::Escape);

    // So does a drag upward from it, and it goes on from where that block is.
    open("A\n\nB\n\nC\n");
    editor.borrow_mut().request_edit(Some(0), Some(Cursor::End));
    h.settle();
    h.key(Key::Return);
    assert_eq!(editing(), Some(1));
    editor.borrow_mut().button_pressed(true);
    editor.borrow_mut().drag_over(1);
    editor.borrow_mut().drag_over(0);
    assert_eq!(selected(), Some(0..=0));
    editor.borrow_mut().drag_over(0);
    assert_eq!(selected(), Some(0..=0));
    editor.borrow_mut().drag_over(1);
    assert_eq!(selected(), Some(0..=1));
    assert_eq!(srcs(), ["A", "B", "C"]);
    h.key(Key::Escape);

    // Ctrl+Z after a paste that split a block puts the block back as it was,
    // and it's finished as it would have been: the fence typed in it closes.
    open("");
    h.type_text("```js");
    h.key(Key::Home);
    app.invoke_copy_text("Intro\n\n".into());
    h.ctrl("v");
    assert_eq!(srcs(), ["Intro", "```js"]);
    h.ctrl("z");
    assert_eq!(srcs(), ["```js"]);
    assert_eq!(editing(), Some(0));
    assert!(rows_match());
    h.key(Key::Escape);
    assert_eq!(editor.borrow().doc.to_text(), "```js\n```\n");

    // A paste whose CR LF line breaks became LF stays one block, and Ctrl+Z
    // takes it back, after what was typed since.
    open("Hello world.\n");
    h.click(300.0, 82.0);
    h.key(Key::End);
    app.invoke_copy_text(" a\r\nb".into());
    h.ctrl("v");
    assert_eq!(srcs(), ["Hello world. a\nb"]);
    h.type_text("X");
    h.ctrl("z");
    assert_eq!(srcs(), ["Hello world. a\nb"]);
    h.ctrl("z");
    assert_eq!(srcs(), ["Hello world."]);
    assert_eq!(editing(), Some(0));
    h.type_text("!");
    assert_eq!(srcs(), ["Hello world.!"], "the caret is back where the paste went");
    h.key(Key::Escape);

    // A character deleted and typed again is two edits for Ctrl+Z to undo
    // before it undoes the paste.
    open("");
    app.invoke_copy_text("# Notes\n\nText".into());
    h.ctrl("v");
    assert_eq!(srcs(), ["# Notes", "Text"]);
    h.key(Key::Backspace);
    h.type_text("t");
    h.ctrl("z");
    assert_eq!(srcs(), ["# Notes", "Tex"], "Ctrl+Z undid the last keystroke");
    h.ctrl("z");
    assert_eq!(srcs(), ["# Notes", "Text"]);
    h.ctrl("z");
    assert_eq!(srcs(), [""]);
    assert!(rows_match());

    // A paste that no longer applies (the block was changed in a way the text
    // field can't undo) is forgotten.
    app.invoke_copy_text("A\n\nB".into());
    h.ctrl("v");
    assert_eq!(srcs(), ["A", "B"]);
    editor.borrow_mut().request_edit(Some(0), Some(Cursor::End));
    h.settle();
    h.ctrl("b");
    assert_eq!(srcs(), ["A****", "B"]);
    h.ctrl("z");
    assert_eq!(srcs(), ["A****", "B"]);
    assert!(editor.borrow().undo.is_none());
    h.key(Key::Escape);

    // Blocks, as rendered, at x = 300 (see shots_selection).
    let text = "# Title\n\nFirst paragraph with **bold** text.\n\n- one\n- two\n\n> quoted\n\n```\ncode\n```\n\nLast paragraph.\n";
    let y = [110.0, 170.0, 205.0, 258.0, 300.0, 345.0];
    open(text);

    // A drag from the middle of a block's text on to other blocks leaves the
    // primary selection as it was.
    PRIMARY.with(|p| *p.borrow_mut() = "before".into());
    h.drag(&[(200.0, y[1]), (380.0, y[3])]);
    assert_eq!(selected(), Some(1..=3));
    assert_eq!(primary(), "before");
    // A middle click after it edits the block clicked, and pastes there.
    h.middle_click(300.0, y[5]);
    assert_eq!((selected(), editing()), (None, Some(5)));
    assert!(srcs()[5].contains("before"), "{:?}", srcs()[5]);
    h.key(Key::Escape);

    // Shift+middle click selects up to the block instead of editing it, and
    // what it pastes into the block's hidden text doesn't reach the document.
    h.click(300.0, y[5]);
    let before = srcs();
    shift(&|| h.middle_click(300.0, y[1]));
    assert_eq!((selected(), editing()), (Some(1..=5), None));
    assert_eq!(srcs(), before);
    h.key(Key::Escape);

    // With no scroll bar, a drag can start at the right edge too, and a click
    // there after a drag drops the selection.
    h.drag(&[(W as f32 - 5.0, y[1]), (W as f32 - 5.0, y[3])]);
    assert_eq!(selected(), Some(1..=3));
    h.click(W as f32 - 5.0, y[5]);
    assert_eq!((selected(), editing()), (None, None));

    std::fs::remove_dir_all(&dir).unwrap();
    println!("selection review checks passed");
}
