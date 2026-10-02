//! Background thread that owns pdfium (which is not thread-safe) and does all
//! rendering and text extraction, so the UI never blocks on it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use pdfium_render::prelude::*;

use crate::annot::geometry::PageGeom;

pub enum Req {
    /// Load `bytes`, removing the given `/Annots` indices per page from the
    /// in-memory copy so pdfium doesn't draw them.
    Load { generation: u64, bytes: Arc<Vec<u8>>, hide: Vec<Vec<usize>> },
    Render { generation: u64, page: usize, scale: f32 },
    Text { generation: u64, page: usize },
}

#[derive(Clone, Copy, Debug)]
pub struct TextChar {
    pub ch: char,
    /// `[x0, y0, x1, y1]` in user space.
    pub rect: [f32; 4],
}

pub enum Resp {
    Loaded { generation: u64, pages: Vec<PageGeom> },
    Failed { generation: u64, message: String },
    Rendered { generation: u64, page: usize, scale: f32, image: egui::ColorImage },
    Text { generation: u64, page: usize, chars: Vec<TextChar> },
}

pub struct Worker {
    tx: Sender<Req>,
    pub rx: Receiver<Resp>,
}

impl Worker {
    pub fn send(&self, req: Req) {
        let _ = self.tx.send(req);
    }

    pub fn spawn(ctx: egui::Context) -> Result<Worker> {
        let (tx, req_rx) = unbounded::<Req>();
        let (resp_tx, rx) = unbounded::<Resp>();
        let (init_tx, init_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);
        std::thread::Builder::new()
            .name("pdfium".into())
            .spawn(move || {
                let pdfium = match bind_pdfium() {
                    Ok(p) => {
                        let _ = init_tx.send(Ok(()));
                        p
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                run(&pdfium, req_rx, resp_tx, ctx);
            })?;
        init_rx.recv()?.map_err(|e| anyhow!(e))?;
        Ok(Worker { tx, rx })
    }
}

/// Looks for libpdfium in `$OCHRE_PDFIUM`, next to the executable (or in `lib/`
/// and `../lib/ochre` beside it), in `~/.local/lib/ochre`, in the
/// project's `vendor/pdfium/lib` (for `cargo run`), then the system library path.
fn bind_pdfium() -> Result<Pdfium> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("OCHRE_PDFIUM") {
        dirs.push(p.into());
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::Path::new(&home).join(".local/lib/ochre"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            dirs.push(dir.join("lib"));
            dirs.push(dir.join("../lib/ochre"));
            for up in dir.ancestors().take(4) {
                dirs.push(up.join("vendor/pdfium/lib"));
            }
        }
    for dir in &dirs {
        let lib = Pdfium::pdfium_platform_library_name_at_path(dir);
        if lib.exists()
            && let Ok(b) = Pdfium::bind_to_library(&lib) {
                return Ok(Pdfium::new(b));
            }
    }
    Pdfium::bind_to_system_library().map(Pdfium::new).map_err(|e| {
        anyhow!(
            "Could not load the pdfium library ({e}).\n\
             Install it (e.g. the AUR package `pdfium-binaries`), put libpdfium.so next to \
             the executable, or set OCHRE_PDFIUM to the folder containing it."
        )
    })
}

fn run(pdfium: &Pdfium, rx: Receiver<Req>, tx: Sender<Resp>, ctx: egui::Context) {
    let mut doc: Option<(u64, PdfDocument<'_>)> = None;
    let mut queue: std::collections::VecDeque<Req> = std::collections::VecDeque::new();
    loop {
        // Take everything that's waiting, but block only when idle.
        if queue.is_empty() {
            match rx.recv() {
                Ok(r) => queue.push_back(r),
                Err(_) => return,
            }
        }
        queue.extend(rx.try_iter());
        // A newer Load makes everything before it obsolete.
        if let Some(pos) = queue.iter().rposition(|r| matches!(r, Req::Load { .. })) {
            queue.drain(..pos);
        }
        // One job at a time, most urgent first: load, then the newest render
        // (the page in view), then text extraction for search/markup.
        let pick = queue
            .iter()
            .position(|r| matches!(r, Req::Load { .. }))
            .or_else(|| queue.iter().rposition(|r| matches!(r, Req::Render { .. })))
            .unwrap_or(0);
        let Some(req) = queue.remove(pick) else { continue };
        let resp = match req {
            Req::Load { generation, bytes, hide } => {
                doc = None;
                match load(pdfium, &bytes, &hide) {
                    Ok((d, pages)) => {
                        doc = Some((generation, d));
                        Resp::Loaded { generation, pages }
                    }
                    Err(e) => Resp::Failed { generation, message: e.to_string() },
                }
            }
            Req::Render { generation, page, scale } => {
                let Some((g, d)) = doc.as_ref().filter(|(g, _)| *g == generation) else { continue };
                match render(d, page, scale) {
                    Ok(image) => Resp::Rendered { generation: *g, page, scale, image },
                    Err(_) => continue,
                }
            }
            Req::Text { generation, page } => {
                let Some((g, d)) = doc.as_ref().filter(|(g, _)| *g == generation) else { continue };
                Resp::Text { generation: *g, page, chars: text(d, page).unwrap_or_default() }
            }
        };
        if tx.send(resp).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn load<'a>(pdfium: &'a Pdfium, bytes: &[u8], hide: &[Vec<usize>]) -> Result<(PdfDocument<'a>, Vec<PageGeom>)> {
    let doc = pdfium.load_pdf_from_byte_vec(bytes.to_vec(), None).map_err(|e| match e {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
            anyhow!("This PDF is password protected, which isn't supported yet.")
        }
        e => anyhow!("Could not open PDF: {e}"),
    })?;
    let mut pages = Vec::new();
    for (i, mut page) in doc.pages().iter().enumerate() {
        if let Some(indices) = hide.get(i).filter(|h| !h.is_empty()) {
            page.set_content_regeneration_strategy(PdfPageContentRegenerationStrategy::Manual);
            let annots = page.annotations_mut();
            for &idx in indices.iter().rev() {
                if let Ok(a) = annots.get(idx) {
                    let _ = annots.delete_annotation(a);
                }
            }
        }
        let b = page.boundaries().bounding().map(|b| b.bounds).unwrap_or(PdfRect::new_from_values(
            0.0,
            0.0,
            page.height().value,
            page.width().value,
        ));
        let rotation = match page.rotation().unwrap_or(PdfPageRenderRotation::None) {
            PdfPageRenderRotation::None => 0,
            PdfPageRenderRotation::Degrees90 => 90,
            PdfPageRenderRotation::Degrees180 => 180,
            PdfPageRenderRotation::Degrees270 => 270,
        };
        pages.push(PageGeom {
            bbox: [b.left().value, b.bottom().value, b.right().value, b.top().value],
            rotation,
        });
    }
    Ok((doc, pages))
}

fn render(doc: &PdfDocument, page: usize, scale: f32) -> Result<egui::ColorImage> {
    let page = doc.pages().get(page as PdfPageIndex)?;
    let cfg = PdfRenderConfig::new()
        .scale_page_by_factor(scale)
        .render_form_data(true)
        .render_annotations(true)
        .set_text_smoothing(true)
        .set_image_smoothing(true)
        .set_path_smoothing(true);
    let bitmap = page.render_with_config(&cfg)?;
    let (w, h) = (bitmap.width() as usize, bitmap.height() as usize);
    Ok(egui::ColorImage::from_rgba_unmultiplied([w, h], &bitmap.as_rgba_bytes()))
}

fn text(doc: &PdfDocument, page: usize) -> Result<Vec<TextChar>> {
    let page = doc.pages().get(page as PdfPageIndex)?;
    let text = page.text()?;
    Ok(text
        .chars()
        .iter()
        .filter_map(|c| {
            let ch = c.unicode_char()?;
            let r = c.loose_bounds().ok()?;
            Some(TextChar { ch, rect: [r.left().value, r.bottom().value, r.right().value, r.top().value] })
        })
        .collect())
}
