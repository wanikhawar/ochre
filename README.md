<p align="center"><img src="packaging/ochre.svg" width="96" alt="Ochre icon"></p>

# Ochre

A fast, lightweight PDF reader and annotator for Linux, written in Rust (egui + pdfium).

*Ochre* is an earth pigment made of iron oxide (rust, more or less). Scribes and artists have marked up pages with it for centuries.

## Features

- **Freehand ink** (pen and highlighter) with smoothing: a stabilizer while drawing, then fitted to Bézier curves, with anti-aliased rendering (tiny-skia).
- **Smart highlighter**: a stroke that starts on text highlights the text in straight lines; anywhere else (or with Alt held) it draws freehand.
- **Text boxes**, **shapes** and **text markup** (highlight, underline, strike out).
  - Shapes: tick, cross, rectangle, ellipse, line and arrow.
  - A tick or cross is placed with a single click.
  - Rectangles and ellipses can be filled, with a separate fill opacity.
- **Tool settings** in a popup from the tool rail: palette, custom color picker with hex input, and width, size and opacity that you can drag or type.
- **Select**, move, restyle and delete annotations, plus an eraser and undo/redo.
  - The Select tool also selects page text: drag, or double-click a word.
  - Copy selected text with Ctrl+C, or turn it into a highlight, underline or strike-out.
- **Vim-style navigation** and `/` search (smartcase, n / N).
- Zoom (fit width / fit page / Ctrl+wheel), recent files, and light and dark themes that follow the system.

## Annotations are standard PDF annotations

Saving writes regular `/Ink`, `/FreeText`, `/Square`, `/Circle`, `/Line`, `/Highlight`, `/Underline` and `/StrikeOut` annotations, each with an appearance stream. Okular, Firefox, Chrome, Acrobat and others display them the same way.

Annotations made by other software are preserved:

- Saving appends an **incremental update**: the original file bytes are kept unchanged and only the changes are added after them.
- Other apps' annotations are never rewritten. They can be removed only by selecting one and pressing Delete, and that deletion can be undone.
- Ochre's annotations are tagged with a `/NM` starting with `ochre-` (older ones with `inkpdf-`). If another app later edits one of them, it's treated as that app's annotation and left alone.

## Install

### From a release (Linux x86_64)

Download `ochre-1.0.0-x86_64-linux.tar.gz` from the [releases page](https://github.com/wanikhawar/ochre/releases), then:

```sh
tar -xzf ochre-1.0.0-x86_64-linux.tar.gz
cd ochre-1.0.0-x86_64-linux
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
| j / k (hold) | Scroll down / up |
| l / h | Next / previous page |
| Ctrl+D / Ctrl+U | Half a screen down / up |
| gg / G | First page / end of document |
| / | Search (Enter to confirm, Esc to cancel) |
| n / N | Next / previous match (wraps around) |
| V / P / Shift+H / T / S / M / E | Select / Pen / Highlighter / Text / Shape / Text markup / Eraser |
| Space + drag, middle drag | Pan |
| Shift while drawing a shape | Square / circle / 45° lines |
| Alt while highlighting | Freehand even over text |
| Double-click a text box (Select tool) | Edit it |
| Delete | Delete selection |
| Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y | Undo, redo |
| Ctrl+S, Ctrl+Shift+S | Save, save as |
| Ctrl+O, Ctrl+F | Open, search (keeps the last query) |
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
- Undo history is cleared after saving.

## License

MIT. See [LICENSE](LICENSE). Release archives also bundle [pdfium](https://pdfium.googlesource.com/pdfium/) (BSD-3-Clause and others; see `licenses/` in the archive).
