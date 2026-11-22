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
}

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
}

struct Harness {
    window: Rc<MinimalSoftwareWindow>,
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

    fn scroll(&self, delta_y: f32) {
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
        self.window.dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
        self.window.dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
        self.settle();
    }

    fn key(&self, text: impl Into<SharedString>) {
        let text = text.into();
        self.window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        self.window.dispatch_event(WindowEvent::KeyReleased { text });
        self.settle();
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
    slint::platform::set_platform(Box::new(TestPlatform { window: window.clone(), start: Instant::now() })).unwrap();

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
    let h = Harness { window, dir };

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
    h.window.dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(300.0, 400.0), delta_x: 0.0, delta_y: -400.0 });
    h.snap("09-scrolled");
    h.window.dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(300.0, 400.0), delta_x: 0.0, delta_y: 400.0 });
    h.settle();
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
    slint::platform::set_platform(Box::new(TestPlatform { window: window.clone(), start: Instant::now() })).unwrap();
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    editor.borrow_mut().start_blank();
    app.show().unwrap();
    let h = Harness { window, dir };
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
    slint::platform::set_platform(Box::new(TestPlatform { window: window.clone(), start: Instant::now() })).unwrap();
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    let sample = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/sample.md")).unwrap();
    let big = sample.repeat(60);
    let path = std::env::temp_dir().join("mdedit-perf.md");
    std::fs::write(&path, &big).unwrap();
    let t = Instant::now();
    editor.borrow_mut().load(path);
    app.show().unwrap();
    let h = Harness { window, dir: std::env::temp_dir() };
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
}

/// Fixes from the code review that need the real UI. No screenshots, so it
/// doesn't need ImageMagick.
#[test]
#[ignore]
fn shots_review() {
    let dir = std::env::temp_dir().join(format!("mdedit-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform { window: window.clone(), start: Instant::now() })).unwrap();
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, dir: dir.clone() };
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
    let dir = std::env::temp_dir().join(format!("mdedit-large-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform { window: window.clone(), start: Instant::now() })).unwrap();
    let app = App::new().unwrap();
    window.set_size(PhysicalSize::new(W, H));
    let editor = Rc::new(RefCell::new(Editor::new(&app)));
    wire(&app, &editor);
    app.show().unwrap();
    let h = Harness { window, dir: dir.clone() };
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

/// The theme menu, the dark theme, and leaving edit mode by clicking blank
/// space. Writes screenshots to `SHOT_DIR`.
#[test]
#[ignore]
fn shots_theme() {
    let dir = PathBuf::from(std::env::var("SHOT_DIR").unwrap_or_else(|_| "target/shots".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform { window: window.clone(), start: Instant::now() })).unwrap();
    let app = App::new().unwrap();
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
    let h = Harness { window, dir };
    let dark = || app.global::<Theme>().get_dark();
    let (menu_x, menu_y) = (W as f32 - 52.0, 22.0);
    let (item_x, light_y, dark_y) = (W as f32 - 100.0, 59.0, 89.0);

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
    h.window.dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(300.0, 400.0), delta_x: 0.0, delta_y: -100000.0 });
    h.settle();
    h.click(500.0, H as f32 - 60.0);
    assert_eq!(editor.borrow().editing, None, "a click below the document leaves edit mode");
    h.snap("t6-dark-bottom");
    h.click(500.0, H as f32 - 60.0);
    assert!(editor.borrow().editing.is_some(), "a second click below continues writing at the end");
    h.key(SharedString::from(Key::Escape));
    h.window.dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(300.0, 400.0), delta_x: 0.0, delta_y: 100000.0 });
    h.settle();

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
}
