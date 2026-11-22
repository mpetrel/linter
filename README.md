# mdedit

A small Typora-style Markdown editor written in Rust with [Slint](https://slint.dev).

The document is shown rendered. Click a block, such as a heading, paragraph, list, quote,
code fence or table, and it turns into its raw Markdown in place, styled like the block.
Edit it, then click elsewhere or press `Esc`, and it renders again. For example, click a
rendered `## Heading`, add a `#`, and it becomes `### Heading` as you type.

Click empty space beside, above or below the text to stop editing and see the whole
document rendered. When nothing is being edited, a click below the last block continues
writing at the end.

The *Theme* menu at the right of the toolbar switches between the light and dark theme.
The choice is remembered in `~/.config/mdedit/settings` (`%APPDATA%\mdedit\settings` on
Windows, `~/Library/Application Support/mdedit/settings` on macOS).

```sh
cargo run --release -- path/to/file.md     # or start without a file
cargo run --release -- examples/sample.md  # a tour of what's supported
```

## Files

- **Open:** `Ctrl+O`, the *Open…* button, or drag a file onto the window.
- **Save:** `Ctrl+S`. **Save As:** `Ctrl+Shift+S`.
- You're asked before unsaved changes are discarded (on open or close).
- Opening and saving a file without edits writes it back byte for byte. Only the
  blocks you edit are reformatted. LF or CRLF line endings and a UTF-8 byte order mark
  are kept. A file that mixes line endings is written back unchanged until you edit it;
  after that, the more common ending is used throughout.
- Saving while a block is being edited writes that block as it will be when you leave
  it (for example, with its code fence closed).
- Saving writes a temporary file next to the original and renames it into place, so a
  failed save never leaves a truncated file.

## Editing keys

| Key | In the block being edited |
| --- | --- |
| `Enter` | New block. In lists: next item. In quotes: continue the quote. In code: new line. |
| `Enter` on an empty list item / quote line | End the list or quote. |
| `Shift+Enter` | Line break inside the block. |
| `Ctrl+Enter` | New paragraph below (for example, to leave a code block). |
| `Backspace` at the start / `Delete` at the end | Merge with the previous / next block. Merged list items and quote lines keep their own lines. |
| `↑` / `↓` on the first / last line | Move to the previous / next block. |
| `Ctrl+=` / `Ctrl+-` | Promote / demote a heading (fewer / more `#`). |
| `Ctrl+1` … `Ctrl+6`, `Ctrl+0` | Make the block a heading of that level, or a paragraph. |
| `Ctrl+B` / `Ctrl+I` | Toggle `**bold**` / `*italic*` around the selection. |
| `Tab` / `Shift+Tab` | Indent / outdent a list item, together with its sub-items. |
| `Esc` | Stop editing. |

Typing ` ```lang ` and pressing `Enter` creates a closed code block. An unclosed code fence
or HTML block (`<!--`, `<pre>`, `<script>`, …) is closed automatically when you leave the
block, so it can't swallow the rest of the file.

If an edit makes neighbouring blocks read differently as Markdown, they're shown the way
the saved file will be read back. For example, an indented paragraph right after a list
becomes part of the list.

## Supported Markdown

Headings (ATX and setext), paragraphs, emphasis, strong, strike-through, inline code,
links (including reference links), `<u>` underline, bullet/ordered/task lists, nesting,
block quotes, fenced and indented code, tables with column alignment, horizontal rules,
local images (paths relative to the file), YAML front matter, footnotes. HTML blocks and
link definitions are shown as raw text.

Several spaces in a row inside a line are kept and shown as typed. Markdown keeps them
too, but HTML viewers such as GitHub show them as one space; use `&nbsp;` for a gap
that has to survive there. Spaces at the start and end of a line follow Markdown: four
or more at the start make a code block, and trailing spaces are dropped (two or more
make a line break).

## Limitations

- **Drag and drop on Linux:** winit can't receive dropped files on Wayland. When XWayland
  is available (`DISPLAY` is set), the app runs through it so drops work. Set
  `MDEDIT_WAYLAND=1` to run as a native Wayland client instead, without drag and drop.
- **Input methods on Linux:** through XWayland, an IBus input method (for example Rime
  or Pinyin) can drop or delay keys, such as repeated spaces. `MDEDIT_WAYLAND=1` uses
  the Wayland input-method support instead.
- Links aren't opened on click (a click edits the block). Remote images aren't downloaded.
- Undo (`Ctrl+Z`) works within the block being edited, not across blocks. It starts over
  when that block scrolls out of view and back.
- Tables and code blocks are edited as plain text.

## Code layout

- `src/doc.rs`: splits a file into blocks, keeps the whitespace between them, and
  re-splits edited text.
- `src/render.rs`: turns a block into display data. Inline Markdown is re-emitted in the
  subset Slint's `StyledText` supports.
- `src/ops.rs`: keyboard commands (Enter, merge, heading levels, list indent, …) as pure
  functions.
- `src/main.rs`: Slint glue, files, drag and drop, unsaved-changes prompt.
- `ui/app.slint`: the UI. The document is a `ListView`, so only the blocks in view exist as
  UI elements, and memory use doesn't grow with the length of the file. The block being
  edited can scroll out of view: keys keep working, and typing brings it back.
- `assets/`: the app icon (`mdedit.svg` is the source of the `.png` and `.ico`).
- `packaging/windows/`: Windows resources and the installer script.

## Tests

```sh
cargo test                                      # unit tests
SHOT_DIR=/tmp/shots cargo test shots::shots -- --exact --ignored   # headless UI run
cargo test shots::shots_review -- --exact --ignored               # UI regression checks
cargo test shots::shots_large_document -- --exact --ignored       # editing a long document
SHOT_DIR=/tmp/shots cargo test shots::shots_theme -- --exact --ignored   # theme menu, dark theme
```

The ignored `shots*` tests drive the real UI with simulated clicks and keys through
Slint's software renderer, and write PNG screenshots (they need ImageMagick's `magick`).
Run each one on its own, as shown, because each sets up its own Slint platform.

## Windows installer

```sh
packaging/windows/build-installer.sh
```

This cross-compiles from Linux and writes two files to `target/windows/`:

- `mdedit-<version>-setup.exe` installs to Program Files with a Start menu shortcut and
  an uninstaller (listed under Settings → Apps). Optionally, it adds a desktop shortcut
  and lists mdedit under *Open with* for `.md` files, without changing the default
  program.
- `mdedit-<version>-windows-x64.zip` holds the same files, to run without installing.

It needs the Rust target (`rustup target add x86_64-pc-windows-gnu`), MinGW-w64
(`mingw64-gcc` on Fedora) and NSIS (`makensis`). The app runs on 64-bit Windows 10 or
later.
