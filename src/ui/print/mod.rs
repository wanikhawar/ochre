//! The print screen (Ctrl+P), from Print Studio: a live preview of the exact
//! sheets in print order, with the settings in a sidebar on the right.
//!
//! What prints is the document as it is now, unsaved annotations included.
//! Other open tabs and PDF files can be added to the same job.

mod art;
mod view;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;

use crate::doc::next_generation;
use crate::pdf::worker::{Req, Resp, Worker};
use crate::print::config::{Config, PpdValues, merge_job};
use crate::print::cups::{self, JobState, PrinterInfo};
use crate::print::geometry::SheetLayout;
use crate::print::pipeline::{self, BLANK, JobOptions, Pass, Side};
use crate::print::ppd::Tone;
use crate::print::render::SheetSpec;
use crate::print::{pdfout, testpdf};

/// Thumbnail size limits (longer edge, in points) and the zoom step between them.
const MIN_THUMB: f32 = 90.0;
const MAX_THUMB: f32 = 720.0;
const THUMB_STEP: f32 = 1.2;

/// What the print screen asks of the app.
pub enum Outcome {
    /// Close the print screen.
    Close,
    /// The job went to the printer: close, and say this.
    Printed(String),
    /// Add the document in this tab to the job (see [`PrintScreen::add_document`]).
    AddTab(usize),
}

/// A document in the job.
struct Entry {
    title: String,
    bytes: Arc<Vec<u8>>,
    /// Known once it has been read.
    pages: Option<usize>,
}

/// What gets printed: the documents, flattened and joined into one, and loaded into pdfium.
struct Source {
    generation: u64,
    /// The document list's revision this was made from.
    rev: u64,
    doc: Arc<lopdf::Document>,
    /// Page sizes as displayed, in points.
    sizes: Vec<(f64, f64)>,
    /// Page count of each document, in order.
    doc_pages: Vec<usize>,
}

/// The joined documents while pdfium loads them.
struct Loading {
    generation: u64,
    rev: u64,
    doc: Arc<lopdf::Document>,
    doc_pages: Vec<usize>,
}

/// What the preview shows (and what Print prints).
#[derive(Clone)]
struct Output {
    job: JobOptions,
    passes: Vec<Pass>,
    /// Each side to print, with its extra rotation.
    sheets: Vec<(Side, u32)>,
    /// The sheets as the preview shows them: like `sheets`, except that booklet
    /// sheets are turned to read like the open booklet.
    view: Vec<(Side, u32)>,
    /// "Front 2" and "pp. 3–4" for each sheet.
    labels: Vec<(String, String)>,
    layout: SheetLayout,
    fallback: (f64, f64),
    /// The colors as the printer driver will print them.
    tone: Tone,
}

impl Output {
    /// Sheets of paper used.
    fn paper(&self) -> usize {
        self.passes[0].sides.len()
    }

    /// Pages printed, counting copies (one per sheet if printed plainly).
    fn page_count(&self) -> usize {
        self.sheets.iter().map(|(s, _)| s.iter().filter(|&&p| p != BLANK).count()).sum()
    }

    fn spec(&self, i: usize) -> Option<SheetSpec> {
        let (side, rotation) = self.view.get(i)?;
        let (layout, fallback, tone) = (self.layout, self.fallback, self.tone);
        Some(SheetSpec { side: side.clone(), rotation: *rotation, layout, fallback, tone })
    }
}

/// What the sheets depend on: the job, the paper and its margin, the tone, and the document.
type OutputKey = (JobOptions, Option<(f64, f64)>, f64, Tone, u64);

/// The documents joined into one: the document, as PDF bytes, and each one's page count.
type Joined = Box<(lopdf::Document, Vec<u8>, Vec<usize>)>;

/// Which thumbnails the preview shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Sheets,
    Pages,
}

/// Rendered thumbnails for one tab, and what's been asked for.
#[derive(Default)]
struct Thumbs {
    tex: HashMap<usize, (egui::TextureHandle, u32)>,
    pending: HashMap<usize, u32>,
}

impl Thumbs {
    fn clear(&mut self) {
        self.tex.clear();
        self.pending.clear();
    }
}

/// What a sheet render request was for.
#[derive(Clone, Copy)]
enum Wanted {
    /// Thumbnail `index` of the tab, for output revision / source generation `rev`, at `px`.
    Thumb { tab: Tab, rev: u64, index: usize, px: u32 },
    /// The page viewer's image.
    View { tab: Tab, rev: u64, index: usize, scale: f32 },
}

/// The full-size viewer, opened by double-clicking a thumbnail.
struct Viewer {
    tab: Tab,
    index: usize,
    /// Relative to fitting the whole sheet in the window.
    zoom: f32,
    tex: Option<(egui::TextureHandle, usize, f32)>,
    pending: Option<(usize, f32)>,
}

/// How a job is sent, once its files are written.
#[derive(Clone)]
struct SendPlan {
    printer: String,
    title: String,
    options: Vec<(String, String)>,
    /// Laid out on known paper: sent print-ready, past CUPS's pdftopdf.
    ready: bool,
    two_sided: bool,
    /// Sheets of paper, for the reload instructions.
    sheets: usize,
    after: After,
}

/// What follows once a two-sided job's backs are sent.
#[derive(Clone, Copy)]
enum After {
    Done,
    Fold { booklets: usize, per: usize, copies: u32 },
    Calibration,
}

/// Work done off the UI thread, reported back.
enum Done {
    /// The documents are flattened and joined (for document revision `rev`).
    Joined {
        rev: u64,
        result: Result<Joined, String>,
    },
    Printers(Result<(Vec<String>, Option<String>), String>),
    Info(PrinterInfo),
    /// Print files written.
    Prepared(Result<Vec<PathBuf>, String>, SendPlan),
    /// Pass `pass` sent: the CUPS job id.
    Sent {
        result: Result<i32, String>,
        files: Vec<PathBuf>,
        pass: usize,
        send: SendPlan,
    },
    Saved(Result<String, String>),
    /// Files picked to add to the job.
    Picked(Vec<PathBuf>),
    /// Where to save the PDF.
    SaveTo(Option<PathBuf>),
    Read(Vec<(String, Result<Vec<u8>, String>)>),
}

/// The "reload the stack" step of two-sided printing.
struct Reload {
    files: Vec<PathBuf>,
    send: SendPlan,
    job_id: i32,
    state: JobState,
    poll: Receiver<JobState>,
}

enum Dialog {
    SavePreset(String),
    DeletePreset(String),
    Reload(Reload),
    /// How to fold the booklet; `message` is said when it's closed.
    Fold {
        booklets: usize,
        per: usize,
        copies: u32,
        message: String,
    },
    Error(String, String),
}

pub struct PrintScreen {
    ctx: egui::Context,
    worker: Option<Worker>,
    cfg: Config,
    entries: Vec<Entry>,
    /// Bumped whenever the documents change, so a slow join that's been overtaken is dropped.
    doc_rev: u64,
    source: Option<Source>,
    /// Joined, waiting for pdfium to load it.
    loading: Option<Loading>,
    load_error: Option<String>,

    printers: Vec<String>,
    printer: Option<String>,
    info: PrinterInfo,
    /// Why there's nothing to print to.
    printer_error: Option<String>,
    /// The selected printer's settings are being read.
    info_loading: bool,
    ppd: PpdValues,
    presets: Vec<String>,
    preset: Option<String>,
    job: JobOptions,
    note: String,
    /// Two-sided setting from before booklet mode switched it on.
    duplex_before_booklet: Option<bool>,

    output: Option<Output>,
    /// Why there's nothing to show (e.g. a bad page range).
    output_error: Option<String>,
    output_key: Option<OutputKey>,
    output_rev: u64,
    /// The tone of the Pages tab's pages, and how many times it changed.
    pages_tone: Tone,
    tone_rev: u64,
    tab: Tab,
    thumb_size: f32,
    show_settings: bool,
    sheets: Thumbs,
    pages: Thumbs,
    /// Pages ticked on the Pages tab, and where a Shift+click range starts.
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    viewer: Option<Viewer>,
    requests: HashMap<u64, Wanted>,
    next_request: u64,

    tasks: (Sender<Done>, Receiver<Done>),
    /// What's happening in the background, shown in the footer.
    busy: Option<String>,
    dialog: Option<Dialog>,
    toast: Option<(String, f64)>,
    /// A file dialog is open.
    picking: bool,
    /// Where print files are written: a private folder, made when first needed.
    spool: Option<PathBuf>,
    /// Open tabs that can be added: (tab index, name).
    tabs: Vec<(usize, String)>,
    /// What the app should do after this frame.
    outcome: Option<Outcome>,
    /// When the screen opened (Ctrl+P right after that is the one that opened it).
    opened: f64,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Joins `bytes` (each a document) into one to print from, annotations drawn into the pages.
fn join(docs: &[Arc<Vec<u8>>], ctx: &egui::Context) -> Result<Joined, String> {
    let mut loaded = Vec::with_capacity(docs.len());
    for bytes in docs {
        let mut doc = pdfout::parse(bytes)?;
        // Filled-in form fields may have nothing to print until they're drawn.
        if pdfout::needs_appearances(&doc) {
            doc = pdfout::parse(&with_appearances(bytes, ctx)?)?;
        }
        pdfout::flatten_annotations(&mut doc);
        loaded.push(doc);
    }
    let doc_pages: Vec<usize> = loaded.iter().map(|d| d.get_pages().len()).collect();
    if doc_pages.contains(&0) {
        return Err("A document has no pages".into());
    }
    let mut doc = match loaded.len() {
        1 => loaded.into_iter().next().unwrap(),
        _ => pdfout::merge(&loaded)?,
    };
    let bytes = pdfout::to_bytes(&mut doc)?;
    Ok(Box::new((doc, bytes, doc_pages)))
}

/// `scale` (pixels per point) for a sheet of `size` points, kept within what
/// the GPU takes and what's worth the memory (as for the page view): bigger
/// zooms show the image stretched.
fn capped_scale((w, h): (f64, f64), scale: f32, max_texture_side: usize) -> f32 {
    let side = crate::viewer::MAX_SIDE.min(max_texture_side as f32 - 2.0) as f64;
    let cap = (crate::viewer::MAX_PIXELS as f64 / (w * h).max(1.0)).sqrt().min(side / w.max(h).max(1.0));
    scale.min(cap as f32)
}

/// `bytes` with the form fields' appearances drawn by pdfium, which the form asks for.
fn with_appearances(bytes: &Arc<Vec<u8>>, ctx: &egui::Context) -> Result<Vec<u8>, String> {
    let failed = |e: String| format!("Couldn't draw the form fields: {e}");
    let worker = Worker::spawn(ctx.clone()).map_err(|e| failed(e.to_string()))?;
    let generation = next_generation();
    worker.send(Req::Appearances { generation, bytes: bytes.clone() });
    match worker.rx.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(Resp::Appearances { result, .. }) => result.map_err(failed),
        _ => Err(failed("pdfium didn't answer".into())),
    }
}

/// A new folder that only this user can open, for print files: made in one
/// step, so nothing someone else put in the temp folder beforehand is used.
fn private_dir() -> std::io::Result<PathBuf> {
    use std::hash::{BuildHasher, Hasher};
    let base = std::env::temp_dir();
    for _ in 0..64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let dir = base.join(format!("ochre-print-{}-{:016x}", std::process::id(), h.finish()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name for a temporary folder"))
}

impl PrintScreen {
    /// A print screen for the document `title` (as PDF bytes, unsaved changes
    /// included). `tabs`: other open documents that can be added, by tab index.
    pub fn new(ctx: &egui::Context, title: String, bytes: Arc<Vec<u8>>, tabs: Vec<(usize, String)>) -> PrintScreen {
        let cfg = Config::load();
        let thumb_size = cfg.ui("thumb_size").and_then(|v| v.as_f64()).unwrap_or(210.0) as f32;
        let tab = if cfg.ui("tab").and_then(|v| v.as_str()) == Some("pages") { Tab::Pages } else { Tab::Sheets };
        let (worker, load_error) = match Worker::spawn(ctx.clone()) {
            Ok(w) => (Some(w), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let mut s = PrintScreen {
            ctx: ctx.clone(),
            worker,
            cfg,
            entries: Vec::new(),
            doc_rev: 0,
            source: None,
            loading: None,
            load_error,
            printers: Vec::new(),
            printer: None,
            info: PrinterInfo::default(),
            printer_error: None,
            info_loading: false,
            ppd: PpdValues::new(),
            presets: Vec::new(),
            preset: None,
            job: JobOptions::default(),
            note: String::new(),
            duplex_before_booklet: None,
            output: None,
            output_error: None,
            output_key: None,
            output_rev: 0,
            pages_tone: Tone::default(),
            tone_rev: 0,
            tab,
            thumb_size: thumb_size.clamp(MIN_THUMB, MAX_THUMB),
            show_settings: true,
            sheets: Thumbs::default(),
            pages: Thumbs::default(),
            selected: BTreeSet::new(),
            anchor: None,
            viewer: None,
            requests: HashMap::new(),
            next_request: 1,
            tasks: unbounded(),
            busy: None,
            dialog: None,
            toast: None,
            picking: false,
            spool: None,
            tabs,
            outcome: None,
            opened: ctx.input(|i| i.time),
        };
        s.refresh_presets(None);
        s.load_printers();
        s.add_document(title, bytes);
        s
    }

    /// Runs `work` on its own thread; its answer comes back through [`Self::poll_tasks`].
    fn spawn(&self, work: impl FnOnce() -> Done + std::marker::Send + 'static) {
        let (tx, ctx) = (self.tasks.0.clone(), self.ctx.clone());
        let _ = std::thread::Builder::new().name("print".into()).spawn(move || {
            let _ = tx.send(work());
            ctx.request_repaint();
        });
    }

    fn toast(&mut self, text: impl Into<String>) {
        let now = self.ctx.input(|i| i.time);
        self.toast = Some((text.into(), now));
    }

    fn error(&mut self, heading: &str, message: impl Into<String>) {
        self.dialog = Some(Dialog::Error(heading.into(), message.into()));
    }

    /// Change the settings on disk. Re-read first: another Ochre window may have saved since.
    fn update_config(&mut self, f: impl FnOnce(&mut Config)) {
        let mut fresh =
            if self.cfg.path.exists() { Config::load_from(self.cfg.path.clone()) } else { self.cfg.clone() };
        f(&mut fresh);
        if let Err(e) = fresh.save() {
            eprintln!("Couldn't save print settings: {e}");
        }
        self.cfg = fresh;
    }

    // ------------------------------------------------------------ documents

    /// Adds a document (PDF bytes) to the job.
    pub fn add_document(&mut self, title: String, bytes: Arc<Vec<u8>>) {
        if !self.entries.is_empty() {
            self.toast(format!("Added “{title}”"));
        }
        self.entries.push(Entry { title, bytes, pages: None });
        self.reload_docs();
    }

    /// Asks for PDF files to add.
    fn pick_files(&mut self) {
        if self.picking {
            return;
        }
        self.picking = true;
        self.spawn(|| {
            let files = rfd::FileDialog::new().add_filter("PDF", &["pdf", "PDF"]).pick_files().unwrap_or_default();
            Done::Picked(files)
        });
    }

    /// Reads files to add to the job.
    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.spawn(move || {
            Done::Read(
                paths
                    .into_iter()
                    .map(|p| {
                        let title = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let bytes = std::fs::read(&p).map_err(|e| format!("{title}: {e}"));
                        (title, bytes)
                    })
                    .collect(),
            )
        });
    }

    fn remove_doc(&mut self, index: usize) {
        if index >= self.entries.len() || self.entries.len() == 1 {
            return;
        }
        let entry = self.entries.remove(index);
        self.clear_ranges();
        self.toast(format!("Removed “{}”", entry.title));
        self.reload_docs();
    }

    fn move_doc(&mut self, from: usize, to: usize) {
        let n = self.entries.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        self.clear_ranges();
        self.reload_docs();
    }

    /// Page numbers shift when documents move, so an old selection would print the wrong pages.
    fn clear_ranges(&mut self) {
        self.job.page_ranges.clear();
        self.selected.clear();
    }

    /// Makes the document to print from the entries: the only one, or all of them joined.
    fn reload_docs(&mut self) {
        self.doc_rev += 1;
        // A load still under way is for the old list.
        if let (Some(old), Some(w)) = (self.loading.take(), &self.worker) {
            w.send(Req::Close { generation: old.generation });
        }
        let rev = self.doc_rev;
        let docs: Vec<Arc<Vec<u8>>> = self.entries.iter().map(|e| e.bytes.clone()).collect();
        self.busy = Some(if docs.len() > 1 { "Joining documents…" } else { "Reading the document…" }.into());
        let ctx = self.ctx.clone();
        self.spawn(move || Done::Joined { rev, result: join(&docs, &ctx) });
    }

    /// The job's title: the document, or the first and how many more.
    fn title(&self) -> String {
        match self.entries.as_slice() {
            [] => String::new(),
            [one] => one.title.clone(),
            [first, rest @ ..] => format!("{} + {} more", first.title, rest.len()),
        }
    }

    // ------------------------------------------------------------ printers and presets

    fn load_printers(&mut self) {
        if cfg!(test) {
            return; // tests don't talk to the printing system
        }
        self.spawn(|| {
            Done::Printers(cups::available().and_then(|_| {
                cups::refresh();
                let names = cups::printers()?;
                let default = cups::default_printer(&names);
                Ok((names, default))
            }))
        });
    }

    fn select_printer(&mut self, name: String) {
        self.printer = Some(name.clone());
        self.info_loading = true;
        self.spawn(move || Done::Info(cups::printer_info(&name)));
    }

    /// The selected printer's options arrived: apply its remembered settings.
    fn on_printer_info(&mut self, info: PrinterInfo) {
        if self.printer.as_deref() != Some(info.name.as_str()) {
            return; // another printer was picked meanwhile
        }
        self.info_loading = false;
        let (mut job, saved) = self.cfg.printer_settings(&info.name);
        // These belong to the document, not the printer.
        job.page_ranges = std::mem::take(&mut self.job.page_ranges);
        job.copies = self.job.copies;
        job.booklet = self.job.booklet;
        self.apply_job(job);
        // This printer's options first: its remembered settings are checked against them.
        self.ppd = info.options.iter().map(|o| (o.keyword.clone(), o.default.clone())).collect();
        self.note = self.cfg.printer_note(&info.name);
        self.info = info;
        self.apply_ppd(&saved);
        // Color or grayscale is chosen for each print, like copies: it starts in color.
        crate::print::ppd::in_color(&self.info.options, &mut self.ppd);
    }

    fn apply_job(&mut self, mut job: JobOptions) {
        if job.booklet {
            if !job.duplex {
                self.duplex_before_booklet = Some(false);
            }
            job.duplex = true;
        }
        self.job = job;
    }

    /// Takes the values that this printer offers.
    fn apply_ppd(&mut self, values: &PpdValues) {
        for opt in &self.info.options {
            if let Some(v) = values.get(&opt.keyword).filter(|v| opt.choices.iter().any(|(c, _)| c == *v)) {
                self.ppd.insert(opt.keyword.clone(), v.clone());
            }
        }
        // Before the printer's options are known, keep them all.
        if self.info.options.is_empty() {
            self.ppd.extend(values.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }

    /// How the chosen driver options change the colors.
    fn tone(&self) -> Tone {
        crate::print::ppd::tone(&self.info.options, &self.ppd)
    }

    fn paper(&self) -> Option<(f64, f64)> {
        self.info.paper(self.ppd.get("PageSize").map(String::as_str))
    }

    /// Unprintable border for the chosen paper, from the printer driver.
    fn margin(&self) -> f64 {
        self.info.margin(self.ppd.get("PageSize").map(String::as_str))
    }

    fn refresh_presets(&mut self, select: Option<&str>) {
        self.presets = self.cfg.preset_names();
        self.preset = select.filter(|s| self.presets.iter().any(|n| n == s)).map(str::to_string);
    }

    fn apply_preset(&mut self, name: &str) {
        let (fields, ppd) = self.cfg.preset(name);
        let job = merge_job(&self.job, &fields);
        if job.booklet && !self.job.booklet {
            self.duplex_before_booklet = Some(self.job.duplex);
        }
        self.apply_job(job);
        self.apply_ppd(&ppd);
        self.preset = Some(name.to_string());
        self.toast(format!("Preset “{name}” applied"));
    }

    fn save_preset(&mut self, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let (job, ppd) = (self.job.clone(), self.ppd.clone());
        self.update_config(|c| c.set_preset(&name, &job, &ppd));
        self.refresh_presets(Some(&name));
        self.toast(format!("Preset “{name}” saved"));
    }

    fn delete_preset(&mut self, name: &str) {
        self.update_config(|c| c.delete_preset(name));
        self.refresh_presets(None);
    }

    fn remember_printer_settings(&mut self) {
        let Some(name) = self.printer.clone() else { return };
        let (mut job, ppd, note) = (self.job.clone(), self.ppd.clone(), self.note.trim().to_string());
        // Two-sided was only switched on for the booklet; don't make it this printer's default.
        if let (true, Some(before)) = (job.booklet, self.duplex_before_booklet) {
            job.duplex = before;
        }
        self.update_config(|c| {
            c.set_last_printer(&name);
            c.set_printer_settings(&name, &job, &ppd);
            c.set_printer_note(&name, &note);
        });
    }

    /// Booklets are always two-sided: switching one on turns two-sided on, and
    /// switching it off puts two-sided back as it was.
    fn set_booklet(&mut self, on: bool) {
        if on == self.job.booklet {
            return;
        }
        if on {
            self.duplex_before_booklet = Some(self.job.duplex);
            self.job.duplex = true;
        } else if let Some(before) = self.duplex_before_booklet.take() {
            self.job.duplex = before;
        }
        self.job.booklet = on;
    }

    fn set_duplex(&mut self, on: bool) {
        if !on && self.job.booklet {
            // Booklets can't be printed on one side.
            self.duplex_before_booklet = None;
            self.job.booklet = false;
            self.toast("Booklet turned off: booklets need two-sided printing");
        }
        self.job.duplex = on;
    }

    // ------------------------------------------------------------ preview

    /// Works out the sheets again if anything they depend on changed.
    fn update_output(&mut self) {
        let Some(src) = &self.source else {
            self.output = None;
            return;
        };
        let margin = self.margin();
        let tone = self.tone();
        if tone != self.pages_tone {
            // The pages are shown as they'll print too.
            self.pages_tone = tone;
            self.tone_rev += 1;
            self.pages.clear();
            if let Some(v) = &mut self.viewer
                && v.tab == Tab::Pages
            {
                v.tex = None;
                v.pending = None;
            }
        }
        let key = (self.job.clone(), self.paper(), margin, tone, src.generation);
        if self.output_key.as_ref() == Some(&key) {
            return;
        }
        self.output_key = Some(key);
        self.output_rev += 1;
        self.sheets.clear();
        if let Some(v) = &mut self.viewer
            && v.tab == Tab::Sheets
        {
            v.tex = None;
            v.pending = None;
        }
        let job = self.job.clone();
        let result = pipeline::plan_docs(&src.doc_pages, &job).map(|passes| {
            let sheets = pipeline::flatten(&passes, &job);
            let labels = pipeline::side_labels(&passes, job.two_sided())
                .into_iter()
                .map(|l| match l.split_once('\t') {
                    Some((a, b)) => (a.to_string(), b.to_string()),
                    None => (l, String::new()),
                })
                .collect();
            let first = pipeline::first_page(sheets.iter().map(|(s, _)| s)).unwrap_or(0);
            let fallback = src.sizes.get(first).copied().unwrap_or((612.0, 792.0));
            let layout = SheetLayout::new(&job, self.paper(), margin);
            // Booklet spreads lie sideways on the paper (top edge to the left); turn them upright.
            let view = if job.booklet {
                sheets.iter().map(|(s, _)| (s.clone(), (job.rotate + 90) % 360)).collect()
            } else {
                sheets.clone()
            };
            Output { job, passes, sheets, view, labels, layout, fallback, tone }
        });
        match result {
            Ok(out) => {
                self.output = Some(out);
                self.output_error = None;
            }
            Err(e) => {
                self.output = None;
                self.output_error = Some(e);
            }
        }
    }

    /// Asks pdfium for a sheet (`tab` Sheets) or page (`tab` Pages).
    fn request(&mut self, wanted: Wanted) {
        let (Some(worker), Some(src)) = (&self.worker, &self.source) else { return };
        let (tab, index, scale) = match wanted {
            Wanted::Thumb { tab, index, px, .. } => {
                let Some(spec) = self.spec(tab, index) else { return };
                let (w, h) = spec.displayed_size(&src.sizes);
                (tab, index, px as f32 / w.max(h).max(1.0) as f32)
            }
            Wanted::View { tab, index, scale, .. } => (tab, index, scale),
        };
        let Some(spec) = self.spec(tab, index) else { return };
        let max_side = self.ctx.input(|i| i.max_texture_side);
        let scale = capped_scale(spec.displayed_size(&src.sizes), scale, max_side);
        let id = self.next_request;
        self.next_request += 1;
        self.requests.insert(id, wanted);
        worker.send(Req::Sheet { generation: src.generation, id, spec, scale });
    }

    fn spec(&self, tab: Tab, index: usize) -> Option<SheetSpec> {
        match tab {
            Tab::Sheets => self.output.as_ref()?.spec(index),
            Tab::Pages => (index < self.source.as_ref()?.sizes.len())
                .then(|| SheetSpec { tone: self.pages_tone, ..SheetSpec::page(index) }),
        }
    }

    /// Displayed size of thumbnail `index`, in points.
    fn thumb_size_of(&self, tab: Tab, index: usize) -> (f64, f64) {
        let sizes = self.source.as_ref().map(|s| s.sizes.as_slice()).unwrap_or_default();
        self.spec(tab, index).map(|s| s.displayed_size(sizes)).unwrap_or((612.0, 792.0))
    }

    /// Revision a thumbnail of `tab` is for.
    fn rev(&self, tab: Tab) -> u64 {
        match tab {
            Tab::Sheets => self.output_rev,
            Tab::Pages => self.source.as_ref().map_or(0, |s| s.generation + (self.tone_rev << 40)),
        }
    }

    /// Shows thumbnail `index` (in view) at `px` device pixels: asks for it if
    /// there's none yet, or it's much smaller or bigger than needed.
    fn want_thumb(&mut self, tab: Tab, index: usize, px: u32) {
        let thumbs = if tab == Tab::Sheets { &self.sheets } else { &self.pages };
        let have = thumbs.tex.get(&index).map(|(_, p)| *p);
        let asked = thumbs.pending.get(&index).copied();
        let fine = |p: u32| p as f32 >= px as f32 * 0.8 && p as f32 <= px as f32 * 1.6;
        if have.is_some_and(fine) || asked.is_some_and(fine) {
            return;
        }
        let rev = self.rev(tab);
        let thumbs = if tab == Tab::Sheets { &mut self.sheets } else { &mut self.pages };
        thumbs.pending.insert(index, px);
        self.request(Wanted::Thumb { tab, rev, index, px });
    }

    fn pump_worker(&mut self) {
        let Some(worker) = &self.worker else { return };
        let responses: Vec<Resp> = worker.rx.try_iter().collect();
        for resp in responses {
            match resp {
                Resp::Loaded { generation, pages, .. } => {
                    let Some(Loading { generation: g, rev, doc, doc_pages }) =
                        self.loading.take_if(|l| l.generation == generation)
                    else {
                        continue;
                    };
                    let sizes = pages.iter().map(|p| p.display_size()).map(|(w, h)| (w as f64, h as f64)).collect();
                    if let (Some(old), Some(w)) = (&self.source, &self.worker) {
                        w.send(Req::Close { generation: old.generation });
                    }
                    for (e, &n) in self.entries.iter_mut().zip(&doc_pages) {
                        e.pages = Some(n);
                    }
                    self.source = Some(Source { generation: g, rev, doc, sizes, doc_pages });
                    self.pages.clear();
                    self.viewer = None;
                    self.busy = None;
                    self.select_from_ranges();
                }
                Resp::Failed { generation, message } => {
                    if self.loading.as_ref().is_some_and(|l| l.generation == generation) {
                        self.loading = None;
                        self.busy = None;
                        self.error("Can't Print This Document", message);
                    }
                }
                Resp::Sheet { id, image, .. } => {
                    let Some(wanted) = self.requests.remove(&id) else { continue };
                    match wanted {
                        Wanted::Thumb { tab, rev, index, px } => {
                            if rev != self.rev(tab) {
                                continue;
                            }
                            let thumbs = if tab == Tab::Sheets { &mut self.sheets } else { &mut self.pages };
                            if thumbs.pending.get(&index) == Some(&px) {
                                thumbs.pending.remove(&index);
                            }
                            let opts = egui::TextureOptions::LINEAR;
                            match thumbs.tex.get_mut(&index) {
                                Some((tex, p)) => {
                                    tex.set(image, opts);
                                    *p = px;
                                }
                                None => {
                                    let tex = self.ctx.load_texture(format!("print-{index}"), image, opts);
                                    thumbs.tex.insert(index, (tex, px));
                                }
                            }
                        }
                        Wanted::View { tab, rev, index, scale } => {
                            let current = self.rev(tab);
                            let Some(v) = &mut self.viewer else { continue };
                            if rev != current || v.tab != tab {
                                continue;
                            }
                            if v.pending == Some((index, scale)) {
                                v.pending = None;
                            }
                            let tex = self.ctx.load_texture("print-view", image, egui::TextureOptions::LINEAR);
                            v.tex = Some((tex, index, scale));
                        }
                    }
                }
                Resp::Rendered { .. } | Resp::Text { .. } | Resp::Appearances { .. } => {}
            }
        }
    }

    // ------------------------------------------------------------ page selection

    /// Ticks the pages the range field names.
    fn select_from_ranges(&mut self) {
        let n = self.source.as_ref().map_or(0, |s| s.sizes.len());
        if self.job.page_ranges.trim().is_empty() {
            self.selected.clear();
        } else if let Ok(p) = pipeline::parse_ranges(&self.job.page_ranges, n) {
            self.selected = p.into_iter().collect();
        } // else: half-typed; keep the current selection
    }

    /// The range field from the ticked pages (empty when none or all are).
    fn ranges_from_selection(&mut self) {
        let n = self.source.as_ref().map_or(0, |s| s.sizes.len());
        let pages: Vec<usize> = self.selected.iter().copied().collect();
        self.job.page_ranges =
            if pages.is_empty() || pages.len() == n { String::new() } else { pipeline::format_ranges(&pages) };
    }

    fn click_page(&mut self, i: usize, shift: bool) {
        match (shift, self.anchor) {
            (true, Some(a)) => {
                let on = !self.selected.contains(&i) || self.selected.contains(&a);
                for p in a.min(i)..=a.max(i) {
                    if on {
                        self.selected.insert(p);
                    } else {
                        self.selected.remove(&p);
                    }
                }
            }
            _ => {
                if !self.selected.remove(&i) {
                    self.selected.insert(i);
                }
            }
        }
        self.anchor = Some(i);
        self.ranges_from_selection();
    }

    // ------------------------------------------------------------ printing

    /// Printer options for CUPS. Scaling and orientation are already done:
    /// the pages we send are laid out on the chosen paper. For manual
    /// two-sided printing, fronts and backs are sent separately, so the
    /// printer must print on one side only, whatever its duplex setting says.
    fn cups_options(&self, manual_two_sided: bool) -> Vec<(String, String)> {
        let mut values = self.ppd.clone();
        if manual_two_sided {
            crate::print::ppd::one_sided(&self.info.options, &mut values);
            values.insert("sides".into(), "one-sided".into());
        }
        let mut options: Vec<_> = values.into_iter().collect();
        options.sort();
        options
    }

    /// The preview is of the documents as listed now (not of a list since changed).
    fn current(&self) -> bool {
        self.loading.is_none() && self.source.as_ref().is_some_and(|s| s.rev == self.doc_rev)
    }

    fn can_print(&self) -> bool {
        self.busy.is_none() && self.current() && self.output.is_some() && self.printer.is_some() && !self.info_loading
    }

    /// The private folder for print files, made the first time.
    fn spool_dir(&mut self) -> Option<PathBuf> {
        if self.spool.is_none() {
            match private_dir() {
                Ok(dir) => self.spool = Some(dir),
                Err(e) => self.error("Print Failed", format!("Couldn't make a temporary folder: {e}")),
            }
        }
        self.spool.clone()
    }

    fn print(&mut self) {
        if !self.can_print() {
            return;
        }
        let (Some(printer), Some(out), Some(src)) = (self.printer.clone(), self.output.clone(), &self.source) else {
            return;
        };
        let doc = src.doc.clone();
        let Some(tmp) = self.spool_dir() else { return };
        self.remember_printer_settings();
        let after = if out.job.booklet {
            let per = out.job.booklet_sheets as usize;
            let paper = out.paper() / out.job.copies.max(1) as usize;
            let booklets = if per == 0 { 1 } else { paper.div_ceil(per) };
            After::Fold { booklets, per, copies: out.job.copies }
        } else {
            After::Done
        };
        let send = SendPlan {
            printer,
            title: self.title(),
            options: self.cups_options(out.job.two_sided()),
            ready: !out.layout.passthrough(),
            two_sided: out.job.two_sided(),
            sheets: out.paper(),
            after,
        };
        self.busy = Some("Preparing pages…".into());
        self.spawn(move || {
            let files = pdfout::write_passes(&doc, &out.passes, &out.job, &out.layout, &tmp, "job");
            Done::Prepared(files, send)
        });
    }

    fn print_calibration(&mut self) {
        let Some(printer) = self.printer.clone() else { return };
        if self.busy.is_some() {
            return;
        }
        let job = JobOptions {
            reverse: self.job.reverse,
            duplex: true,
            backs_reverse: self.job.backs_reverse,
            backs_rotate: self.job.backs_rotate,
            ..JobOptions::default()
        };
        let paper = self.paper().unwrap_or(testpdf::A4);
        let Some(tmp) = self.spool_dir() else { return };
        let send = SendPlan {
            printer,
            title: "Two-sided calibration".into(),
            options: self.cups_options(true),
            ready: false,
            two_sided: true,
            sheets: 2,
            after: After::Calibration,
        };
        self.remember_printer_settings();
        self.busy = Some("Preparing the test…".into());
        self.spawn(move || {
            let doc = testpdf::duplex_test(paper);
            // The test sheets are already made at paper size.
            let layout = SheetLayout::new(&job, None, 0.0);
            let files = pipeline::plan(4, &job)
                .and_then(|passes| pdfout::write_passes(&doc, &passes, &job, &layout, &tmp, "calibration"));
            Done::Prepared(files, send)
        });
    }

    /// Sends pass `pass` of `files` (fronts are 0, backs 1) without blocking the window.
    fn submit(&mut self, files: Vec<PathBuf>, pass: usize, send: SendPlan) {
        self.busy = Some(
            match (send.two_sided, pass) {
                (false, _) => "Sending to the printer…",
                (true, 0) => "Sending front sides…",
                _ => "Sending back sides…",
            }
            .into(),
        );
        let path = files[pass].clone();
        let title = match (send.two_sided, pass) {
            (false, _) => send.title.clone(),
            (true, 0) => format!("{} (fronts)", send.title),
            _ => format!("{} (backs)", send.title),
        };
        self.spawn(move || {
            let format = send.ready.then_some(cups::FORMAT_PRINT_READY_PDF);
            let result = cups::print_file(&send.printer, &path, &title, &send.options, format);
            Done::Sent { result, files, pass, send }
        });
    }

    /// Asks where to save the output as a PDF.
    fn save_pdf(&mut self) {
        if self.picking || self.output.is_none() || !self.current() {
            return;
        }
        let stem = Path::new(&self.title())
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let suffix = if self.job.booklet { "booklet" } else { "print" };
        let name = format!("{stem}-{suffix}.pdf");
        self.picking = true;
        self.spawn(move || {
            Done::SaveTo(rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(name).save_file())
        });
    }

    fn poll_tasks(&mut self) {
        let done: Vec<Done> = self.tasks.1.try_iter().collect();
        for d in done {
            match d {
                Done::Joined { rev, result } => {
                    if rev != self.doc_rev {
                        continue; // the documents changed again meanwhile
                    }
                    match result {
                        Ok(joined) => {
                            let (doc, bytes, doc_pages) = *joined;
                            let generation = next_generation();
                            if let Some(w) = &self.worker {
                                w.send(Req::Load { generation, bytes: Arc::new(bytes), hide: Vec::new() });
                            }
                            self.loading = Some(Loading { generation, rev, doc: Arc::new(doc), doc_pages });
                        }
                        Err(e) => {
                            self.busy = None;
                            self.error("Can't Print This Document", e);
                        }
                    }
                }
                Done::Printers(Ok((names, default))) => {
                    let last = self.cfg.last_printer().filter(|p| names.contains(p));
                    self.printers = names;
                    match last.or(default) {
                        Some(p) => self.select_printer(p),
                        None => {
                            self.printer_error =
                                Some("No printers found. Add one in your system's printer settings.".into())
                        }
                    }
                }
                Done::Printers(Err(e)) => self.printer_error = Some(e),
                Done::Info(info) => self.on_printer_info(info),
                Done::Prepared(Ok(files), send) => self.submit(files, 0, send),
                Done::Prepared(Err(e), _) => {
                    self.busy = None;
                    self.error("Print Failed", e);
                }
                Done::Sent { result: Err(e), pass, send, .. } => {
                    self.busy = None;
                    let what = match (send.two_sided, pass) {
                        (false, _) => e,
                        (true, 0) => format!("Couldn't print the front sides:\n{e}"),
                        _ => format!("Couldn't print the back sides:\n{e}"),
                    };
                    self.error("Print Failed", what);
                }
                Done::Sent { result: Ok(job_id), files, pass, send } => {
                    self.busy = None;
                    if send.two_sided && pass == 0 {
                        let poll = watch_job(job_id, &self.ctx);
                        self.dialog =
                            Some(Dialog::Reload(Reload { files, send, job_id, state: JobState::Queued, poll }));
                        continue;
                    }
                    match send.after {
                        After::Fold { booklets, per, copies } => {
                            let message = format!("Sent “{}” to {}", send.title, send.printer);
                            self.dialog = Some(Dialog::Fold { booklets, per, copies, message });
                        }
                        After::Calibration => self.toast(
                            "Calibration printed. Check both sheets and adjust the two-sided options if needed.",
                        ),
                        _ => {
                            self.outcome = Some(Outcome::Printed(format!("Sent “{}” to {}", send.title, send.printer)))
                        }
                    }
                }
                Done::Saved(result) => {
                    self.busy = None;
                    match result {
                        Ok(name) => self.toast(format!("Saved “{name}”")),
                        Err(e) => self.error("Couldn't Save the PDF", e),
                    }
                }
                Done::Picked(files) => {
                    self.picking = false;
                    self.add_paths(files);
                }
                Done::Read(files) => {
                    for (title, bytes) in files {
                        match bytes {
                            Ok(b) => self.add_document(title, Arc::new(b)),
                            Err(e) => self.error("Can't Open File", e),
                        }
                    }
                }
                Done::SaveTo(target) => {
                    self.picking = false;
                    let (Some(dest), Some(out), Some(src)) = (target, self.output.clone(), &self.source) else {
                        continue;
                    };
                    if !self.current() {
                        self.error("Couldn't Save the PDF", "The documents changed meanwhile. Try again.");
                        continue;
                    }
                    let doc = src.doc.clone();
                    self.busy = Some("Saving…".into());
                    self.spawn(move || {
                        let shown = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let result = pdfout::write_combined(&doc, &out.passes, &out.job, &out.layout, &dest);
                        Done::Saved(result.map(|_| shown))
                    });
                }
            }
        }
    }

    /// Runs the print screen for one frame; returns what the app should do.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<Outcome> {
        self.poll_tasks();
        self.pump_worker();
        self.update_output();
        if let Some(Dialog::Reload(r)) = &mut self.dialog
            && let Some(state) = r.poll.try_iter().last()
        {
            r.state = state;
        }
        // Files dropped on the window join the job.
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect()
        });
        self.add_paths(dropped);
        self.draw(ui);
        self.outcome.take()
    }
}

/// When the screen goes: remember the view, and free pdfium's copy and the temporary files.
impl Drop for PrintScreen {
    fn drop(&mut self) {
        let (size, tab) = (self.thumb_size as f64, if self.tab == Tab::Pages { "pages" } else { "preview" });
        self.update_config(|c| {
            c.set_ui("thumb_size", size.into());
            c.set_ui("tab", tab.into());
        });
        if let Some(w) = &self.worker {
            let loading = self.loading.as_ref().map(|l| l.generation);
            for generation in self.source.as_ref().map(|s| s.generation).into_iter().chain(loading) {
                w.send(Req::Close { generation });
            }
        }
        if let Some(dir) = &self.spool {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Reports a CUPS job's state every second until it's over (or nobody's listening).
fn watch_job(job_id: i32, ctx: &egui::Context) -> Receiver<JobState> {
    let (tx, rx) = unbounded();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("print job".into()).spawn(move || {
        loop {
            let state = cups::job_state(job_id);
            if tx.send(state).is_err() {
                return;
            }
            ctx.request_repaint();
            if state.is_final() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
    rx
}

#[cfg(test)]
impl PrintScreen {
    /// Page count of each document in the job, once it's loaded as listed now.
    pub fn loaded_pages(&self) -> Option<Vec<usize>> {
        self.source.as_ref().filter(|_| self.current()).map(|s| s.doc_pages.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::testpdf::{A4, numbered};

    /// A print screen for a `pages`-page document, or None without pdfium.
    fn screen(pages: usize) -> Option<(egui::Context, PrintScreen)> {
        let ctx = egui::Context::default();
        Worker::spawn(ctx.clone()).ok()?;
        let bytes = pdfout::to_bytes(&mut numbered(pages, A4)).unwrap();
        let s = PrintScreen::new(&ctx, "doc.pdf".into(), Arc::new(bytes), Vec::new());
        Some((ctx, s))
    }

    fn frame(ctx: &egui::Context, s: &mut PrintScreen, events: Vec<egui::Event>) -> Option<Outcome> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
            events,
            ..Default::default()
        };
        let mut outcome = None;
        let mut out = ctx.run_ui(input, |ui| outcome = s.show(ui));
        out.textures_delta.clear(); // no GPU here
        outcome
    }

    fn wait_loaded(ctx: &egui::Context, s: &mut PrintScreen, doc_pages: &[usize]) {
        for _ in 0..500 {
            frame(ctx, s, Vec::new());
            if s.loaded_pages().as_deref() == Some(doc_pages) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the documents didn't load: {:?}", s.loaded_pages());
    }

    #[test]
    fn lays_out_the_sheets_as_the_options_change() {
        let Some((ctx, mut s)) = screen(5) else { return };
        wait_loaded(&ctx, &mut s, &[5]);
        assert_eq!(s.output.as_ref().unwrap().paper(), 5);
        s.job.nup = 2;
        frame(&ctx, &mut s, Vec::new());
        assert_eq!(s.output.as_ref().unwrap().paper(), 3);
        s.job.page_ranges = "9".into();
        frame(&ctx, &mut s, Vec::new());
        assert!(s.output.is_none() && s.output_error.is_some(), "no pages selected");
    }

    #[test]
    fn a_grayscale_driver_option_grays_the_preview() {
        let Some((ctx, mut s)) = screen(2) else { return };
        wait_loaded(&ctx, &mut s, &[2]);
        let spec = |s: &PrintScreen, tab| s.spec(tab, 0).unwrap().tone.gray;
        assert!(!spec(&s, Tab::Sheets) && !spec(&s, Tab::Pages));
        let choices = vec![("Color".to_string(), "Color".to_string()), ("Mono".to_string(), "Grayscale".to_string())];
        s.info.options = vec![crate::print::ppd::PrinterOption {
            keyword: "BRMonoColor".into(),
            label: "Color / Grayscale".into(),
            group: "General".into(),
            default: "Color".into(),
            choices,
            common: true,
        }];
        s.ppd.insert("BRMonoColor".into(), "Mono".into());
        frame(&ctx, &mut s, Vec::new());
        assert!(spec(&s, Tab::Sheets) && spec(&s, Tab::Pages));
    }

    #[test]
    fn printing_starts_in_color_even_after_a_grayscale_print() {
        let Some((_, mut s)) = screen(1) else { return };
        let choices = vec![("Color".to_string(), "Color".to_string()), ("Mono".to_string(), "Grayscale".to_string())];
        let option = crate::print::ppd::PrinterOption {
            keyword: "BRMonoColor".into(),
            label: "Color / Grayscale".into(),
            group: "General".into(),
            default: "Color".into(),
            choices,
            common: true,
        };
        let last = PpdValues::from([("BRMonoColor".to_string(), "Mono".to_string())]);
        s.cfg.set_printer_settings("P", &JobOptions::default(), &last);
        s.printer = Some("P".into());
        s.on_printer_info(PrinterInfo { name: "P".into(), options: vec![option], ..Default::default() });
        assert_eq!(s.ppd["BRMonoColor"], "Color");
        assert!(!s.tone().gray);
    }

    #[test]
    fn previews_stay_within_gpu_and_memory_limits() {
        // A4 at 2x and full zoom wants a 6620x9368 image.
        let k = capped_scale(A4, 11.1, 16384);
        assert!(595.0 * 842.0 * (k as f64).powi(2) <= 16.0e6 + 1.0, "{k}");
        assert!(842.0 * capped_scale(A4, 11.1, 4096) as f64 <= 4094.0);
        assert_eq!(capped_scale(A4, 0.5, 16384), 0.5, "small ones as asked");
    }

    #[test]
    fn print_files_go_in_a_private_new_folder() {
        let a = private_dir().unwrap();
        let b = private_dir().unwrap();
        assert_ne!(a, b);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        }
        std::fs::remove_dir(a).unwrap();
        std::fs::remove_dir(b).unwrap();
    }

    #[test]
    fn a_changed_document_list_cant_print_until_its_preview_is_ready() {
        let Some((ctx, mut s)) = screen(3) else { return };
        s.printer = Some("P".into());
        wait_loaded(&ctx, &mut s, &[3]);
        assert!(s.can_print());
        // A document that can't be read: the old preview stays, but isn't printed.
        s.add_document("broken.pdf".into(), Arc::new(b"not a pdf".to_vec()));
        assert!(!s.can_print(), "while joining");
        for _ in 0..200 {
            frame(&ctx, &mut s, Vec::new());
            if s.busy.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(matches!(s.dialog, Some(Dialog::Error(..))));
        assert!(!s.can_print() && !s.current(), "the preview is of the old list");
        s.dialog = None;
        s.remove_doc(1);
        wait_loaded(&ctx, &mut s, &[3]);
        assert!(s.can_print());
    }

    fn quality(keyword: &str) -> crate::print::ppd::PrinterOption {
        crate::print::ppd::PrinterOption {
            keyword: keyword.into(),
            label: "Print Quality".into(),
            group: "General".into(),
            default: "Normal".into(),
            choices: ["Normal", "Best"].iter().map(|c| (c.to_string(), c.to_string())).collect(),
            common: true,
        }
    }

    #[test]
    fn switching_printers_keeps_each_ones_settings() {
        let Some((_, mut s)) = screen(1) else { return };
        for (name, keyword) in [("A", "AQuality"), ("B", "BQuality")] {
            let best = PpdValues::from([(keyword.to_string(), "Best".to_string())]);
            s.cfg.set_printer_settings(name, &JobOptions::default(), &best);
        }
        for (name, keyword) in [("A", "AQuality"), ("B", "BQuality")] {
            s.printer = Some(name.into());
            s.on_printer_info(PrinterInfo { name: name.into(), options: vec![quality(keyword)], ..Default::default() });
            assert_eq!(s.ppd[keyword], "Best", "{name}");
        }
    }

    #[test]
    fn manual_two_sided_passes_are_sent_one_sided() {
        let Some((_, mut s)) = screen(1) else { return };
        let duplex = crate::print::ppd::PrinterOption {
            keyword: "Duplex".into(),
            label: "2-Sided Printing".into(),
            choices: vec![("None".into(), "Off".into()), ("DuplexNoTumble".into(), "Long Edge".into())],
            ..quality("Duplex")
        };
        s.info.options = vec![duplex];
        s.ppd.insert("Duplex".into(), "DuplexNoTumble".into());
        let get = |o: &[(String, String)], k: &str| o.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        let plain = s.cups_options(false);
        assert_eq!(get(&plain, "Duplex").as_deref(), Some("DuplexNoTumble"), "the printer's own duplex otherwise");
        let manual = s.cups_options(true);
        assert_eq!(get(&manual, "Duplex").as_deref(), Some("None"));
        assert_eq!(get(&manual, "sides").as_deref(), Some("one-sided"));
    }

    #[test]
    fn closing_while_loading_frees_pdfiums_copy() {
        let Some((ctx, mut s)) = screen(2) else { return };
        // Until the join is done and pdfium was asked to load it.
        for _ in 0..200 {
            s.poll_tasks();
            if s.loading.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let generation = s.loading.as_ref().unwrap().generation;
        drop(s);
        // pdfium forgot it: a render request for it gets no answer.
        let w = Worker::spawn(ctx).unwrap();
        w.send(Req::Sheet { generation, id: 1, spec: SheetSpec::page(0), scale: 0.1 });
        assert!(w.rx.recv_timeout(std::time::Duration::from_millis(500)).is_err());
    }

    #[test]
    fn booklets_are_always_two_sided() {
        let Some((_, mut s)) = screen(1) else { return };
        s.set_booklet(true);
        assert!(s.job.duplex);
        s.set_booklet(false);
        assert!(!s.job.duplex, "back as it was");
        s.set_duplex(true);
        s.set_booklet(true);
        s.set_booklet(false);
        assert!(s.job.duplex, "it was on before");
        s.set_booklet(true);
        s.set_duplex(false);
        assert!(!s.job.booklet, "one-sided booklets can't be made");
    }

    #[test]
    fn ticked_pages_and_the_range_stay_in_step() {
        let Some((ctx, mut s)) = screen(10) else { return };
        wait_loaded(&ctx, &mut s, &[10]);
        s.click_page(1, false);
        s.click_page(4, true);
        assert_eq!(s.job.page_ranges, "2-5");
        s.click_page(2, false);
        assert_eq!(s.job.page_ranges, "2, 4-5");
        s.job.page_ranges = "1, 3".into();
        s.select_from_ranges();
        assert_eq!(s.selected.iter().copied().collect::<Vec<_>>(), [0, 2]);
        // All ticked prints everything: the range is left empty.
        s.selected = (0..10).collect();
        s.ranges_from_selection();
        assert_eq!(s.job.page_ranges, "");
    }

    #[test]
    fn documents_join_the_job_and_can_be_reordered() {
        let Some((ctx, mut s)) = screen(3) else { return };
        wait_loaded(&ctx, &mut s, &[3]);
        let other = pdfout::to_bytes(&mut numbered(2, A4)).unwrap();
        s.add_document("other.pdf".into(), Arc::new(other));
        wait_loaded(&ctx, &mut s, &[3, 2]);
        assert_eq!(s.title(), "doc.pdf + 1 more");
        s.job.page_ranges = "4".into();
        s.move_doc(1, 0);
        assert_eq!(s.job.page_ranges, "", "page numbers moved, so the range goes");
        wait_loaded(&ctx, &mut s, &[2, 3]);
        s.remove_doc(0);
        wait_loaded(&ctx, &mut s, &[3]);
        s.remove_doc(0);
        assert_eq!(s.entries.len(), 1, "the last one stays");
    }

    #[test]
    fn escape_goes_back_and_a_quick_second_ctrl_p_does_not_print() {
        let Some((ctx, mut s)) = screen(1) else { return };
        let key = |key, modifiers| {
            let ev = |pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
            vec![ev(true), ev(false)]
        };
        s.printer = Some("Somewhere".into());
        wait_loaded(&ctx, &mut s, &[1]);
        // Loading can take many frames, each moving egui's clock on: open it now.
        s.opened = ctx.input(|i| i.time);
        frame(&ctx, &mut s, key(egui::Key::P, egui::Modifiers::COMMAND));
        assert!(s.busy.is_none(), "too soon after opening");
        let out = frame(&ctx, &mut s, key(egui::Key::Escape, egui::Modifiers::NONE));
        assert!(matches!(out, Some(Outcome::Close)));
    }
}
