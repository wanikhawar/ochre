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
    /// `thumb`: for the page thumbnails, not the page view.
    Render { generation: u64, page: usize, scale: f32, thumb: bool },
    Text { generation: u64, page: usize },
    /// Forget a loaded document (its tab was closed or it was reloaded).
    Close { generation: u64 },
}

#[derive(Clone, Copy, Debug)]
pub struct TextChar {
    pub ch: char,
    /// `[x0, y0, x1, y1]` in user space.
    pub rect: [f32; 4],
}

/// Where a link or table-of-contents entry leads.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A page, optionally scrolled to a point (user space; either coordinate may be unknown).
    Page { page: usize, x: Option<f32>, y: Option<f32> },
    /// A web address or other URI.
    Uri(String),
}

/// One entry of the document outline (table of contents), flattened depth-first.
#[derive(Clone, Debug, PartialEq)]
pub struct OutlineItem {
    pub title: String,
    pub level: usize,
    pub target: Option<Target>,
}

/// A link area on a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    /// `[x0, y0, x1, y1]` in user space.
    pub rect: [f32; 4],
    pub target: Target,
}

pub enum Resp {
    Loaded { generation: u64, pages: Vec<PageGeom>, outline: Vec<OutlineItem>, links: Vec<Vec<Link>> },
    Failed { generation: u64, message: String },
    Rendered { generation: u64, page: usize, scale: f32, image: egui::ColorImage, thumb: bool },
    Text { generation: u64, page: usize, chars: Vec<TextChar> },
}

impl Resp {
    pub fn generation(&self) -> u64 {
        match self {
            Resp::Loaded { generation, .. }
            | Resp::Failed { generation, .. }
            | Resp::Rendered { generation, .. }
            | Resp::Text { generation, .. } => *generation,
        }
    }
}

/// A request, with where its answer goes.
struct Job {
    req: Req,
    reply: Sender<Resp>,
    ctx: egui::Context,
}

/// The one pdfium thread of the process (pdfium-render can bind the library only
/// once per process), started on first use. Its documents are keyed by generation,
/// which is unique across the process, so several clients can share it.
static SERVICE: std::sync::OnceLock<Result<Sender<Job>, String>> = std::sync::OnceLock::new();

/// A client of the pdfium thread with its own answer channel.
pub struct Worker {
    tx: Sender<Job>,
    reply: Sender<Resp>,
    pub rx: Receiver<Resp>,
    ctx: egui::Context,
}

impl Worker {
    pub fn send(&self, req: Req) {
        let _ = self.tx.send(Job { req, reply: self.reply.clone(), ctx: self.ctx.clone() });
    }

    pub fn spawn(ctx: egui::Context) -> Result<Worker> {
        let tx = SERVICE.get_or_init(start_service).clone().map_err(|e| anyhow!(e))?;
        let (reply, rx) = unbounded::<Resp>();
        Ok(Worker { tx, reply, rx, ctx })
    }
}

fn start_service() -> Result<Sender<Job>, String> {
    let (tx, jobs) = unbounded::<Job>();
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
            run(&pdfium, jobs);
        })
        .map_err(|e| e.to_string())?;
    init_rx.recv().map_err(|e| e.to_string())??;
    Ok(tx)
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

/// Serves requests for any number of open documents (one per tab), keyed by generation.
fn run(pdfium: &Pdfium, rx: Receiver<Job>) {
    let mut docs: std::collections::HashMap<u64, PdfDocument<'_>> = std::collections::HashMap::new();
    let mut queue: std::collections::VecDeque<Job> = std::collections::VecDeque::new();
    loop {
        // Take everything that's waiting, but block only when idle.
        if queue.is_empty() {
            match rx.recv() {
                Ok(j) => queue.push_back(j),
                Err(_) => return,
            }
        }
        queue.extend(rx.try_iter());
        // Closing frees memory and makes queued work for that document moot.
        let closed: Vec<u64> = queue
            .iter()
            .filter_map(|j| match j.req {
                Req::Close { generation } => Some(generation),
                _ => None,
            })
            .collect();
        if !closed.is_empty() {
            for g in &closed {
                docs.remove(g);
            }
            queue.retain(|j| match &j.req {
                Req::Load { generation, .. } | Req::Render { generation, .. } | Req::Text { generation, .. } => {
                    !closed.contains(generation)
                }
                Req::Close { .. } => false,
            });
        }
        // One job at a time, most urgent first: load, then the newest render
        // (the page in view), then text extraction for search/markup.
        let pick = queue
            .iter()
            .position(|j| matches!(j.req, Req::Load { .. }))
            .or_else(|| queue.iter().rposition(|j| matches!(j.req, Req::Render { .. })))
            .unwrap_or(0);
        let Some(Job { req, reply, ctx }) = queue.remove(pick) else { continue };
        let resp = match req {
            Req::Load { generation, bytes, hide } => match load(pdfium, &bytes, &hide) {
                Ok((d, pages)) => {
                    let outline = outline(&d);
                    let links = (0..pages.len()).map(|i| links(&d, i)).collect();
                    docs.insert(generation, d);
                    Resp::Loaded { generation, pages, outline, links }
                }
                Err(e) => Resp::Failed { generation, message: e.to_string() },
            },
            Req::Render { generation, page, scale, thumb } => {
                let Some(d) = docs.get(&generation) else { continue };
                match render(d, page, scale) {
                    Ok(image) => Resp::Rendered { generation, page, scale, image, thumb },
                    Err(_) => continue,
                }
            }
            Req::Text { generation, page } => {
                let Some(d) = docs.get(&generation) else { continue };
                Resp::Text { generation, page, chars: text(d, page).unwrap_or_default() }
            }
            Req::Close { .. } => continue,
        };
        // A client that went away (e.g. a finished test) just doesn't get its answer.
        if reply.send(resp).is_ok() {
            ctx.request_repaint();
        }
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

fn dest_target(d: &PdfDestination) -> Option<Target> {
    let page = d.page_index().ok()? as usize;
    let (x, y) = match d.view_settings() {
        Ok(PdfDestinationViewSettings::SpecificCoordinatesAndZoom(x, y, _)) => (x.map(|v| v.value), y.map(|v| v.value)),
        Ok(PdfDestinationViewSettings::FitPageHorizontallyToWindow(y)) => (None, y.map(|v| v.value)),
        Ok(PdfDestinationViewSettings::FitPageVerticallyToWindow(x)) => (x.map(|v| v.value), None),
        Ok(PdfDestinationViewSettings::FitPageToRectangle(r)) => (Some(r.left().value), Some(r.top().value)),
        _ => (None, None),
    };
    Some(Target::Page { page, x, y })
}

fn action_target(a: &PdfAction) -> Option<Target> {
    match a {
        PdfAction::LocalDestination(l) => dest_target(&l.destination().ok()?),
        PdfAction::Uri(u) => u.uri().ok().filter(|s| !s.trim().is_empty()).map(Target::Uri),
        _ => None,
    }
}

/// The outline as a flat depth-first list. Guards against cyclic (broken) outlines.
fn outline(doc: &PdfDocument) -> Vec<OutlineItem> {
    const MAX_ITEMS: usize = 5000;
    fn walk(b: Option<PdfBookmark>, level: usize, out: &mut Vec<OutlineItem>) {
        let mut cur = b;
        while let Some(b) = cur {
            if out.len() >= MAX_ITEMS || level > 32 {
                return;
            }
            let title = b.title().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
            let target = b.destination().as_ref().and_then(dest_target).or_else(|| b.action().as_ref().and_then(action_target));
            out.push(OutlineItem { title, level, target });
            walk(b.first_child(), level + 1, out);
            cur = b.next_sibling();
        }
    }
    let mut out = Vec::new();
    let bookmarks = doc.bookmarks();
    walk(bookmarks.root(), 0, &mut out);
    out
}

fn links(doc: &PdfDocument, page: usize) -> Vec<Link> {
    let Ok(p) = doc.pages().get(page as PdfPageIndex) else { return Vec::new() };
    p.links()
        .iter()
        .filter_map(|l| {
            let target = l.destination().as_ref().and_then(dest_target).or_else(|| l.action().as_ref().and_then(action_target))?;
            let r = l.rect().ok()?;
            let [x0, y0, x1, y1] = [r.left().value, r.bottom().value, r.right().value, r.top().value];
            Some(Link { rect: [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)], target })
        })
        .collect()
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

