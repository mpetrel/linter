# linter

A small Typora-style Markdown editor for the desktop, written in Rust with
[Slint](https://slint.dev).

There's no separate preview pane: you read and edit the document rendered. Click a block
and it turns into its Markdown source right where it was. Click elsewhere and it renders
again.

## Features

- **Edit in place:** headings, lists, quotes, code blocks, tables and images are shown
  rendered. Only the block you're editing shows its Markdown.
- **Careful with your files:** a file you open and save without edits is written back
  byte for byte. Line endings and the byte order mark are kept, and a failed save never
  leaves a truncated file.
- **Images** from files and the web, in all the common formats, loaded in the background.
- **Long documents:** only the blocks in view are built, so memory use doesn't grow with
  the length of the file.
- **Keyboard shortcuts** for headings, bold and italic, list indent, and moving between
  blocks.
- **Light and dark themes**, and a Windows installer.

## Quick start

You need [Rust](https://rustup.rs), a C compiler (for libwebp) and, on Linux, the OpenSSL
headers (`openssl-devel` on Fedora, `libssl-dev` on Debian and Ubuntu).

```sh
git clone https://github.com/mpetrel/linter.git
cd linter
cargo run --release -- examples/sample.md  # a tour of what's supported
cargo run --release -- path/to/file.md     # or start without a file
```

## How editing works

Click a block, such as a heading, paragraph, list, quote,
code fence or table, and it turns into its raw Markdown in place, styled like the block.
Edit it, then click elsewhere or press `Esc`, and it renders again. For example, click a
rendered `## Heading`, add a `#`, and it becomes `### Heading` as you type.

Click empty space beside, above or below the text to stop editing and see the whole
document rendered. When nothing is being edited, a click below the last block continues
writing at the end.

The *Theme* menu at the right of the toolbar switches between the light and dark theme.
The choice is remembered in `~/.config/linter/settings` (`%APPDATA%\linter\settings` on
Windows, `~/Library/Application Support/linter/settings` on macOS). *About* shows the
version and the Slint attribution.

## Files

- **Open:** `Ctrl+O`, the *Open…* button, or drag a file onto the window.
- **Save:** `Ctrl+S`. **Save As:** `Ctrl+Shift+S`.
- You're asked before unsaved changes are discarded (on open or close): `Enter` saves,
  `Esc` cancels.
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
| `Tab` / `Shift+Tab` | Indent / outdent a list item, together with its sub-items. In code: `Tab` types four spaces, or indents the selected lines; `Shift+Tab` outdents the line or the selected lines. |
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
images, YAML front matter, footnotes. HTML blocks and link definitions are shown as raw
text.

### Images

An image on its own line (`![alt](path-or-url)`, or `![alt][label]` with `[label]: path-or-url`
anywhere in the file) is shown in place of the block:

- **Formats:** PNG, JPEG, GIF, WebP, SVG, BMP, ICO, TIFF, TGA, PNM and QOI. Animated GIF
  and WebP show their first frame. JPEG photos are turned upright by their Exif
  orientation. SVG (and `.svgz`) up to 8 MB. AVIF, HEIC and JPEG XL aren't supported,
  and say so.
- **Files:** paths are relative to the Markdown file (to the current directory for a file
  that hasn't been saved yet). `file://` links and `%20`-style escapes work too.
- **Web:** `http://` and `https://` images are downloaded, at most 64 MB each, using the
  system's certificates. The proxy comes from `HTTPS_PROXY` / `ALL_PROXY` or, on Windows,
  the system proxy setting; images from this machine (`localhost`, `127.0.0.1`) bypass
  it. Downloads aren't kept on disk: opening the file again downloads them again.

Images load in the background, once their block is in view, the newest first: a block
shows *loading…* until its image is ready, or the reason it can't be shown. Decoded images
are kept up to 256 MB; beyond that the ones shown longest ago are let go of, and load again
when they come back into view. Downloads still running for a file that was closed don't
hold up the images of the next one. Images wider than the column can show (at the screen's scale,
and at least 2×) are scaled down while they're decoded, so large photos are quick to show
and don't use much memory; on a screen with a larger scale they're decoded again.
Decoders: zune-jpeg, or jpeg-decoder for photos at least twice too large, since it can
decode at 1/2, 1/4 or 1/8 size; libwebp, about 4× faster than the pure Rust WebP decoder
and able to scale while decoding; the `image` crate for the rest; Slint (resvg) for SVG.

Several spaces in a row inside a line are kept and shown as typed. Markdown keeps them
too, but HTML viewers such as GitHub show them as one space; use `&nbsp;` for a gap
that has to survive there. Spaces at the start and end of a line follow Markdown: four
or more at the start make a code block, and trailing spaces are dropped (two or more
make a line break).

## Limitations

- **Drag and drop on Linux:** winit can't receive dropped files on Wayland. When XWayland
  is available (`DISPLAY` is set), the app runs through it so drops work. Set
  `LINTER_WAYLAND=1` to run as a native Wayland client instead, without drag and drop.
- **Input methods on Linux:** through XWayland, an IBus input method (for example Rime
  or Pinyin) can drop or delay keys, such as repeated spaces. `LINTER_WAYLAND=1` uses
  the Wayland input-method support instead.
- Links aren't opened on click (a click edits the block). An image inside a line of text
  is shown as its alt text.
- Undo (`Ctrl+Z`) works within the block being edited, not across blocks. It starts over
  when that block scrolls out of view and back.
- Tables and code blocks are edited as plain text.

## Code layout

- `src/doc.rs`: splits a file into blocks, keeps the whitespace between them, and
  re-splits edited text.
- `src/images.rs`: loads, downloads and decodes images on worker threads.
- `src/render.rs`: turns a block into display data. Inline Markdown is re-emitted in the
  subset Slint's `StyledText` supports.
- `src/ops.rs`: keyboard commands (Enter, merge, heading levels, list indent, …) as pure
  functions.
- `src/main.rs`: Slint glue, files, drag and drop, unsaved-changes prompt.
- `ui/app.slint`: the UI. The document is a `ListView`, so only the blocks in view exist as
  UI elements, and memory use doesn't grow with the length of the file. The block being
  edited can scroll out of view: keys keep working, and typing brings it back.
- `assets/`: the app icon (`linter.svg` is the source of the `.png` and `.ico`).
- `packaging/windows/`: Windows resources and the installer script.

## Tests

```sh
cargo test                                      # unit tests
SHOT_DIR=/tmp/shots cargo test shots::shots -- --exact --ignored   # headless UI run
cargo test shots::shots_review -- --exact --ignored               # UI regression checks
cargo test shots::shots_large_document -- --exact --ignored       # editing a long document
cargo test shots::shots_mixed_heights -- --exact --ignored        # blocks of different heights
cargo test shots::shots_focus -- --exact --ignored                # keys while the block is out of view
cargo test shots::shots_images_in_view -- --exact --ignored       # images load in view, memory budget
SHOT_DIR=/tmp/shots cargo test shots::shots_theme -- --exact --ignored   # theme menu, dark theme, About
SHOT_DIR=/tmp/shots cargo test shots::shots_images -- --exact --ignored  # images: formats, web, errors
```

The ignored `shots*` tests drive the real UI with simulated clicks and keys through
Slint's software renderer, and write PNG screenshots (they need ImageMagick's `magick`).
Run each one on its own, as shown, because each sets up its own Slint platform.

## License

linter is released under the [MIT License](LICENSE).

Slint is used under the [Slint Royalty-free Desktop, Mobile, and Web Applications
License 2.0](https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md).
It asks for the `AboutSlint` widget in an About dialog that's reachable from the top
level menu, so keep it in the dialog behind the toolbar's *About* button.

## Windows installer

```sh
packaging/windows/build-installer.sh
```

This cross-compiles from Linux and writes two files to `target/windows/`:

- `linter-<version>-setup.exe` installs to Program Files with a Start menu shortcut and
  an uninstaller (listed under Settings → Apps). Optionally, it adds a desktop shortcut
  and lists linter under *Open with* for `.md` files, without changing the default
  program. Running a newer `setup.exe` updates an existing install in its folder and
  keeps the settings; it asks you to close linter first if it's running. Bump `version`
  in `Cargo.toml` for each release so Settings → Apps shows which one is installed.
  Uninstalling asks whether to delete your settings too, and keeps them by default.
- `linter-<version>-windows-x64.zip` holds the same files, to run without installing.

It needs the Rust target (`rustup target add x86_64-pc-windows-gnu`), MinGW-w64
(`mingw64-gcc` on Fedora, which also builds libwebp) and NSIS (`makensis`). The app runs on 64-bit Windows 10 or
later.
