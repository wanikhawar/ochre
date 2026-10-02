<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="packaging/ochre-wordmark-dark.svg">
    <img src="packaging/ochre-wordmark.svg" width="300" alt="Ochre">
  </picture>
</h1>

A fast, lightweight PDF reader and annotator for Linux, written in Rust (egui + pdfium).

*Ochre* is an earth pigment made of iron oxide (rust, more or less). Scribes and artists have marked up pages with it for centuries.

## Features

- **Freehand ink** (pen and highlighter) with smoothing: a stabilizer while drawing, then fitted to Bézier curves, with anti-aliased rendering (tiny-skia).
- **Smart highlighter**: a stroke that starts on text highlights the text in straight lines; anywhere else (or with Alt held) it draws freehand.
- **Text boxes**, **shapes** and **text markup** (highlight, underline, strike out).
  - Shapes: tick, cross, rectangle, ellipse, line and arrow.
  - A tick or cross is placed with a single click.
  - Rectangles and ellipses can be filled, with a separate fill opacity.
- **Tool settings** in a popup from the status bar's settings button: palette, custom color picker with hex input, and width, size and opacity that you can drag or type.
- **Select**, move, resize, restyle and delete annotations, plus an eraser and undo/redo.
  - Drag a corner handle to resize (Shift keeps the proportions). Resizing a text box changes its font size.
  - Drag either end of a line or arrow.
  - Rotate with the round handle above the selection (Shift snaps to 15°). Text markup follows the page text, so it doesn't rotate.
  - Select several at once: drag a box from empty space (Shift+drag adds to the selection, and also works over text), Shift+click to add or remove one, or Ctrl+A for everything on the page. A group moves, restyles (color, width, opacity, fill) and deletes together.
  - Copy, cut and paste annotations (also between tabs and Ochre windows), duplicate them with Ctrl+D, and nudge them with the arrow keys.
  - The Select tool also selects page text: drag, or double-click a word.
  - Copy selected text with Ctrl+C, or turn it into a highlight, underline or strike-out.
- **Notes** on any annotation: select it and choose "Add note", press Enter, or double-click it.
  - Annotations with a note show a badge; hover over it to read the note.
  - Notes are saved as standard `/Contents` comments, so Acrobat, Okular and others show them too. Notes on other apps' annotations are shown read-only.
- **Vim-style navigation** and `/` search (smartcase, n / N).
- **Page tools** in the sidebar's Pages tab: thumbnails (with your annotations) to go to a page, and to rotate, delete, reorder (drag) or extract pages to a new PDF. Select several with Ctrl+click / Shift+click; the tools act on the selection, or on the page in view. Annotations, the contents and links follow their pages, and it's all undoable, even after saving.
- **Sidebar** (F9) with three tabs (contents, annotations, pages); the first two: the PDF's **contents**, with the section you're reading highlighted, and a list of all **annotations** by page (highlighted text, notes, other apps' annotations). Click one to go to it.
- A **status bar** shows the active tool with a button for its settings (color, width, opacity), and the page and zoom controls, without covering the page.
- **Links** inside the document work with the Select and Hand tools. Web and email links open in your browser or mail app. Alt+← or the Back button returns to where you were.
- **Tabs:** every file opens in its own tab, from the Open dialog, Recent files, drag and drop (several at once), or the command line (`ochre a.pdf b.pdf`). Opening a file that's already open switches to its tab. Each tab keeps its own zoom, position, selection and search.
- **Remembers your place:** each file reopens at the page and zoom where you left it.
- Zoom (fit width / fit page / Ctrl+wheel), recent files, and light and dark themes that follow the system.

## Annotations are standard PDF annotations

Saving writes regular `/Ink`, `/FreeText`, `/Square`, `/Circle`, `/Line`, `/Highlight`, `/Underline` and `/StrikeOut` annotations, each with an appearance stream. Okular, Firefox, Chrome, Acrobat and others display them the same way.

Annotations made by other software are preserved:

- Saving appends an **incremental update**: the original file bytes are kept unchanged and only the changes are added after them.
- Other apps' annotations are never rewritten, and saving or undoing never removes them. They can be removed only by selecting one and pressing Delete, and that deletion can be undone, even after saving (the next save puts it back).
- Ochre's annotations are tagged with a `/NM` starting with `ochre-` (older ones with `inkpdf-`). If another app later edits one of them, it's treated as that app's annotation and left alone.

## Install

### From a release (Linux x86_64)

Download the latest `ochre-<version>-x86_64-linux.tar.gz` from the [releases page](https://github.com/wanikhawar/ochre/releases), then:

```sh
tar -xzf ochre-*-x86_64-linux.tar.gz
cd ochre-*-x86_64-linux
./install.sh          # installs to ~/.local (set PREFIX to change)
```

### From source

```sh
git clone https://github.com/wanikhawar/ochre
cd ochre
./install.sh          # builds with cargo and downloads pdfium if needed
```

Either way you get:

- `ochre` in `~/.local/bin`;
- pdfium in `~/.local/lib/ochre`;
- a launcher entry with an icon, offered under "Open with" for PDFs.

`./uninstall.sh` removes them again.

Run `ochre file.pdf`, or start it from your app launcher.

### Developing

```sh
mkdir -p vendor/pdfium
curl -L https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-x64.tgz | tar -xz -C vendor/pdfium
cargo run --release -- some.pdf
```

Ochre looks for `libpdfium.so` in these places, in order:

1. `$OCHRE_PDFIUM`
2. Next to the executable, in `lib/` beside it, or in `../lib/ochre`
3. `~/.local/lib/ochre`
4. This project's `vendor/pdfium/lib`
5. The system library path (e.g. AUR `pdfium-binaries`)

## Shortcuts

Vim-style navigation works whenever you're not typing in a text field.

| Key | Action |
|---|---|
| j / k, arrow keys (hold) | Scroll (arrows nudge selected annotations instead) |
| l / h | Next / previous page |
| Ctrl+D / Ctrl+U | Half a screen down / up |
| gg / G | First page / end of document |
| / | Search (Enter to confirm, Esc to cancel). Ctrl+F reopens it with the last query selected |
| n / N | Next / previous match (wraps around) |
| V / P / Shift+H / T / S / M / E | Select / Pen / Highlighter / Text / Shape / Text markup / Eraser |
| Space + drag, middle drag | Pan |
| Shift while drawing a shape | Square / circle / 45° lines |
| Alt while highlighting | Freehand even over text |
| Double-click a text box (Select tool) | Edit it |
| Double-click an annotation, or Enter (Select tool) | Add or edit its note |
| Ctrl+Enter, Esc | Finish the note |
| Shift while resizing | Keep proportions / 45° line ends |
| Drag on empty space, Shift+drag, Shift+click (Select tool) | Box-select, add to selection, add/remove one |
| Ctrl+A (Select tool) | Select every annotation on the page |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / cut / paste annotations (Ctrl+C copies page text when text is selected) |
| Ctrl+D (with annotations selected) | Duplicate them (otherwise half a page down) |
| Arrow keys, Shift+arrows (with annotations selected) | Nudge 1 pt / 10 pt |
| Delete | Delete selection |
| Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y | Undo, redo |
| Ctrl+S, Ctrl+Shift+S | Save, save as |
| Ctrl+O, Ctrl+F | Open, search (keeps the last query) |
| F9 | Show or hide the contents sidebar |
| Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+PageDown / Ctrl+PageUp | Next / previous tab |
| Ctrl+W, middle-click a tab | Close the tab |
| Alt+←, mouse back button | Back (after following a link or a contents entry) |
| Ctrl+wheel, Ctrl+= / Ctrl+-, Ctrl+0 | Zoom, fit width |
| PageUp / PageDown / Home / End | Scroll |

Search uses smartcase: an all-lowercase query ignores case, while a query containing a capital letter matches case exactly.

## Tests

```sh
cargo test
```

The tests cover:

- geometry and stroke smoothing (including overshoot);
- search;
- the save round-trip, including preserving other apps' annotations and the original bytes;
- a headless end-to-end run of every tool.

To compare rendering with other viewers by eye:

```sh
OCHRE_SAMPLE=in.pdf OCHRE_OUT=/tmp/out cargo test visual -- --ignored
pdftoppm -png /tmp/out/annotated.pdf /tmp/out/poppler   # how Okular renders it
```

## Known limitations

- Password-protected or encrypted PDFs open read-only (no annotating).
- Text boxes use the standard Helvetica font in the PDF. Characters outside Latin-1 display in Ochre but appear as `?` in other viewers.

## License

MIT. See [LICENSE](LICENSE). Release archives also bundle [pdfium](https://pdfium.googlesource.com/pdfium/) (BSD-3-Clause and others; see `licenses/` in the archive).
