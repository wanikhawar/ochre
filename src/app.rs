//! Application state, file handling, shortcuts and dialogs. The page canvas
//! lives in `viewer.rs`, the window chrome in `ui/`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers};

use crate::annot::model::{Kind, MarkupKind, ShapeKind, Style};
use crate::config::Config;
use crate::doc::{Cmd, Doc};
use crate::pdf::worker::{Req, Resp, TextChar, Worker};
use crate::search::Search;
use crate::ui::theme;
use crate::viewer::{Fit, Gesture, Motion, Overlay, PageTex, TextEditState, TextSel, View};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Select,
    Hand,
    Pen,
    Highlighter,
    Text,
    Shape(ShapeKind),
    Markup(MarkupKind),
    Eraser,
}

impl Tool {
    pub fn key(self) -> String {
        format!("{self:?}")
    }

    pub fn default_style(self) -> Style {
        match self {
            Tool::Pen => Style::new([0.08, 0.08, 0.1], 2.0, 1.0),
            Tool::Highlighter => Style::new([1.0, 0.86, 0.0], 14.0, 0.4),
            Tool::Text => Style::new([0.08, 0.08, 0.1], 14.0, 1.0),
            Tool::Shape(ShapeKind::Check) => Style::new([0.15, 0.62, 0.25], 2.5, 1.0),
            Tool::Shape(ShapeKind::Cross) => Style::new([0.86, 0.15, 0.15], 2.5, 1.0),
            Tool::Shape(_) => Style::new([0.86, 0.15, 0.15], 2.0, 1.0),
            Tool::Markup(MarkupKind::Highlight) => Style::new([1.0, 0.86, 0.0], 1.0, 0.4),
            Tool::Markup(MarkupKind::Underline) => Style::new([0.1, 0.35, 0.9], 1.0, 1.0),
            Tool::Markup(MarkupKind::StrikeOut) => Style::new([0.86, 0.15, 0.15], 1.0, 1.0),
            Tool::Eraser => Style::new([0.5; 3], 12.0, 1.0),
            Tool::Select | Tool::Hand => Style::new([0.0; 3], 1.0, 1.0),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Hand => "Pan",
            Tool::Pen => "Pen",
            Tool::Highlighter => "Highlighter",
            Tool::Text => "Text",
            Tool::Shape(ShapeKind::Rect) => "Rectangle",
            Tool::Shape(ShapeKind::Ellipse) => "Ellipse",
            Tool::Shape(ShapeKind::Line) => "Line",
            Tool::Shape(ShapeKind::Arrow) => "Arrow",
            Tool::Shape(ShapeKind::Check) => "Tick",
            Tool::Shape(ShapeKind::Cross) => "Cross",
            Tool::Markup(MarkupKind::Highlight) => "Highlight text",
            Tool::Markup(MarkupKind::Underline) => "Underline text",
            Tool::Markup(MarkupKind::StrikeOut) => "Strike out text",
            Tool::Eraser => "Eraser",
        }
    }

    pub(crate) fn shortcut(self) -> &'static str {
        match self {
            Tool::Select => "V",
            Tool::Hand => "Space (hold)",
            Tool::Pen => "P",
            Tool::Highlighter => "Shift+H",
            Tool::Text => "T",
            Tool::Shape(_) => "S",
            Tool::Markup(_) => "M",
            Tool::Eraser => "E",
        }
    }

    /// Whether the width slider means something for this tool, and what it's called.
    pub(crate) fn width_label(self) -> Option<&'static str> {
        match self {
            Tool::Pen | Tool::Highlighter | Tool::Shape(_) => Some("Width"),
            Tool::Text => Some("Size"),
            Tool::Eraser => Some("Size"),
            _ => None,
        }
    }

    pub(crate) fn has_color(self) -> bool {
        !matches!(self, Tool::Select | Tool::Hand | Tool::Eraser)
    }
}

/// What the selection is, if anything.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    /// One of our annotations, by id.
    Ours(String),
    /// An annotation from other software, by index in `Doc::foreign`.
    Foreign(usize),
}

enum Pending {
    Close,
    Open(Option<PathBuf>),
}

pub(crate) const PALETTE: [[f32; 3]; 10] = [
    [0.08, 0.08, 0.1],
    [0.45, 0.45, 0.48],
    [1.0, 1.0, 1.0],
    [0.86, 0.15, 0.15],
    [0.96, 0.5, 0.05],
    [1.0, 0.86, 0.0],
    [0.15, 0.65, 0.25],
    [0.1, 0.35, 0.9],
    [0.55, 0.25, 0.85],
    [0.93, 0.35, 0.6],
];

pub struct App {
    pub worker: Option<Worker>,
    pub worker_error: Option<String>,
    pub cfg: Config,
    pub doc: Option<Doc>,
    pub load_error: Option<String>,
    pub sent_generation: u64,
    pub view: View,
    pub tool: Tool,
    pub last_shape: ShapeKind,
    pub last_markup: MarkupKind,
    pub tex: HashMap<usize, PageTex>,
    pub in_flight: HashMap<usize, (f32, u64)>,
    pub overlay: HashMap<usize, Overlay>,
    pub live_tex: Option<egui::TextureHandle>,
    pub text_chars: HashMap<usize, Vec<TextChar>>,
    pub text_requested: HashSet<usize>,
    pub gesture: Gesture,
    pub editing: Option<TextEditState>,
    pub selection: Option<Selection>,
    /// Selected page text (Select tool).
    pub text_sel: Option<TextSel>,
    pub search: Search,
    /// Id of the annotation whose style the toolbar is currently editing (merges undo steps).
    style_edit: Option<String>,
    pub(crate) status: Option<(String, f64, bool)>,
    pending: Option<Pending>,
    allow_close: bool,
    title: String,
    pub(crate) page_input: String,
    /// Time of the last lone `g` press (for `gg`).
    g_pressed_at: f64,
}

impl App {
    pub fn new(ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        setup_fonts(ctx);
        theme::install(ctx);
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let (worker, worker_error) = match Worker::spawn(ctx.clone()) {
            Ok(w) => (Some(w), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let mut app = App {
            worker,
            worker_error,
            cfg: Config::load(),
            doc: None,
            load_error: None,
            sent_generation: 0,
            view: View::default(),
            tool: Tool::Pen,
            last_shape: ShapeKind::Rect,
            last_markup: MarkupKind::Highlight,
            tex: HashMap::new(),
            in_flight: HashMap::new(),
            overlay: HashMap::new(),
            live_tex: None,
            text_chars: HashMap::new(),
            text_requested: HashSet::new(),
            gesture: Gesture::None,
            editing: None,
            selection: None,
            text_sel: None,
            search: Search::default(),
            style_edit: None,
            status: None,
            pending: None,
            allow_close: false,
            title: String::new(),
            page_input: String::new(),
            g_pressed_at: f64::NEG_INFINITY,
        };
        if let Some(p) = path {
            app.open_now(p);
        }
        app
    }

    pub fn tool_style(&self, tool: Tool) -> Style {
        self.cfg.styles.get(&tool.key()).copied().unwrap_or_else(|| tool.default_style())
    }

    pub fn set_status(&mut self, ctx: &egui::Context, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), ctx.input(|i| i.time), error));
    }

    pub fn set_tool(&mut self, tool: Tool) {
        self.commit_text();
        self.text_sel = None;
        self.gesture = Gesture::None;
        if tool != Tool::Select {
            self.select(None);
        }
        match tool {
            Tool::Shape(s) => self.last_shape = s,
            Tool::Markup(m) => self.last_markup = m,
            _ => {}
        }
        self.tool = tool;
    }

    pub fn select(&mut self, sel: Option<Selection>) {
        if self.selection != sel {
            self.style_edit = None;
        }
        self.selection = sel;
    }

    /// Runs commands as one undo step and reloads pdfium if needed.
    pub fn exec(&mut self, cmds: Vec<Cmd>) {
        if let Some(doc) = &mut self.doc {
            doc.exec(cmds);
        }
        self.style_edit = None;
    }

    pub fn undo(&mut self) {
        self.commit_text();
        if let Some(doc) = &mut self.doc {
            doc.undo();
        }
        self.after_history_jump();
    }

    pub fn redo(&mut self) {
        self.commit_text();
        if let Some(doc) = &mut self.doc {
            doc.redo();
        }
        self.after_history_jump();
    }

    fn after_history_jump(&mut self) {
        self.style_edit = None;
        let valid = match (&self.selection, &self.doc) {
            (Some(Selection::Ours(id)), Some(doc)) => doc.get(id).is_some(),
            (Some(Selection::Foreign(i)), Some(doc)) => !doc.deleted_foreign.contains(i),
            _ => true,
        };
        if !valid {
            self.selection = None;
        }
    }

    pub fn copy_selection(&mut self, ctx: &egui::Context) {
        if let Some(text) = self.selected_text().filter(|t| !t.is_empty()) {
            let n = text.chars().count();
            ctx.copy_text(text);
            self.set_status(ctx, format!("Copied {n} characters"), false);
        }
    }

    pub fn delete_selection(&mut self) {
        let Some(doc) = &self.doc else { return };
        let cmd = match &self.selection {
            Some(Selection::Ours(id)) => {
                let Some(index) = doc.index_of(id) else { return };
                Cmd::Remove { annot: doc.annots[index].clone(), index }
            }
            Some(Selection::Foreign(i)) => Cmd::DeleteForeign(*i),
            None => return,
        };
        self.exec(vec![cmd]);
        self.selection = None;
    }

    /// Style shown in the toolbar: the selected annotation's, else the tool's.
    pub(crate) fn shown_style(&self) -> (Style, Option<Tool>) {
        if let (Some(Selection::Ours(id)), Some(doc)) = (&self.selection, &self.doc)
            && let Some(a) = doc.get(id) {
                let tool = match &a.kind {
                    Kind::Ink { highlighter: true, .. } => Tool::Highlighter,
                    Kind::Ink { .. } => Tool::Pen,
                    Kind::Text { .. } => Tool::Text,
                    Kind::Shape { shape, .. } => Tool::Shape(*shape),
                    Kind::Markup { markup, .. } => Tool::Markup(*markup),
                };
                return (a.style, Some(tool));
            }
        (self.tool_style(self.tool), None)
    }

    pub(crate) fn apply_style(&mut self, style: Style) {
        let sel = match &self.selection {
            Some(Selection::Ours(id)) => Some(id.clone()),
            _ => None,
        };
        let Some(id) = sel else {
            self.cfg.styles.insert(self.tool.key(), style);
            return;
        };
        if let Some(e) = &mut self.editing
            && e.id.as_deref() == Some(id.as_str()) {
                e.style = style;
            }
        let Some(doc) = &mut self.doc else { return };
        let Some(i) = doc.index_of(&id) else { return };
        let before = doc.annots[i].clone();
        let mut after = before.clone();
        after.style = style;
        if self.style_edit.as_deref() == Some(id.as_str()) {
            // Same editing session (e.g. dragging a slider): update in place,
            // keeping the original "before" in the existing undo step.
            doc.amend_last_modify(after);
        } else {
            doc.exec(vec![Cmd::Modify { before, after }]);
            self.style_edit = Some(id);
        }
    }

    // ---------------------------------------------------------------- files

    pub fn request_open(&mut self, path: Option<PathBuf>) {
        self.commit_text();
        if self.doc.as_ref().is_some_and(Doc::is_dirty) {
            self.pending = Some(Pending::Open(path));
        } else {
            self.open(path);
        }
    }

    fn open(&mut self, path: Option<PathBuf>) {
        let path = path.or_else(|| {
            let mut d = rfd::FileDialog::new().add_filter("PDF", &["pdf", "PDF"]);
            if let Some(dir) = self.doc.as_ref().and_then(|d| d.path.parent().map(|p| p.to_path_buf())) {
                d = d.set_directory(dir);
            }
            d.pick_file()
        });
        if let Some(p) = path {
            self.open_now(p);
        }
    }

    fn open_now(&mut self, path: PathBuf) {
        match Doc::open(&path) {
            Ok(doc) => {
                self.doc = Some(doc);
                self.load_error = None;
                self.tex.clear();
                self.in_flight.clear();
                self.overlay.clear();
                self.text_chars.clear();
                self.text_requested.clear();
                self.gesture = Gesture::None;
                self.editing = None;
                self.selection = None;
                self.style_edit = None;
                self.search = Search::default();
                self.view = View::default();
                let canonical = path.canonicalize().unwrap_or(path);
                self.cfg.add_recent(canonical);
                self.cfg.save();
            }
            Err(e) => {
                self.load_error = Some(format!("Could not open {}: {e}", path.display()));
            }
        }
    }

    /// Saves; returns true on success.
    pub(crate) fn save(&mut self, ctx: &egui::Context, save_as: bool) -> bool {
        self.commit_text();
        let Some(doc) = &mut self.doc else { return false };
        let target = if save_as {
            let mut d = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(doc.name());
            if let Some(dir) = doc.path.parent() {
                d = d.set_directory(dir);
            }
            match d.save_file() {
                Some(p) => p,
                None => return false,
            }
        } else {
            doc.path.clone()
        };
        let sel_id = match &self.selection {
            Some(Selection::Ours(id)) => Some(id.clone()),
            _ => None,
        };
        let result = doc.save_to(&target);
        match result {
            Ok(()) => {
                // Foreign indices change after a save; our ids don't.
                self.selection = sel_id.map(Selection::Ours);
                self.style_edit = None;
                self.overlay.clear();
                self.cfg.add_recent(target.canonicalize().unwrap_or(target));
                self.cfg.save();
                self.set_status(ctx, "Saved", false);
                true
            }
            Err(e) => {
                self.set_status(ctx, format!("Save failed: {e:#}"), true);
                false
            }
        }
    }

    // ---------------------------------------------------------------- worker

    fn pump_worker(&mut self, ctx: &egui::Context) {
        let Some(worker) = &self.worker else { return };
        if let Some(doc) = &self.doc
            && doc.generation != self.sent_generation {
                worker.send(Req::Load {
                    generation: doc.generation,
                    bytes: Arc::clone(&doc.bytes),
                    hide: doc.hidden(),
                });
                self.sent_generation = doc.generation;
                self.in_flight.clear();
            }
        let responses: Vec<Resp> = worker.rx.try_iter().collect();
        for resp in responses {
            let Some(doc) = &mut self.doc else { continue };
            match resp {
                Resp::Loaded { generation, pages } if generation == doc.generation => {
                    if doc.pages != pages {
                        doc.set_pages(pages);
                    }
                }
                Resp::Failed { generation, message } if generation == doc.generation => {
                    self.load_error = Some(message);
                    self.doc = None;
                }
                Resp::Rendered { generation, page, scale, image } => {
                    self.in_flight.remove(&page);
                    if generation != doc.generation {
                        continue;
                    }
                    let opts = egui::TextureOptions::LINEAR;
                    match self.tex.get_mut(&page) {
                        Some(t) => {
                            t.tex.set(image, opts);
                            t.scale = scale;
                            t.generation = generation;
                        }
                        None => {
                            let tex = ctx.load_texture(format!("page{page}"), image, opts);
                            self.tex.insert(page, PageTex { tex, scale, generation });
                        }
                    }
                }
                Resp::Text { generation, page, chars } if generation == doc.generation => {
                    self.text_chars.insert(page, chars);
                }
                _ => {}
            }
        }
    }

    pub fn request_render(&mut self, page: usize, scale: f32) {
        let (Some(worker), Some(doc)) = (&self.worker, &self.doc) else { return };
        if self.in_flight.get(&page) == Some(&(scale, doc.generation)) {
            return;
        }
        // One request per page at a time keeps the queue short while zooming.
        if self.in_flight.contains_key(&page) {
            return;
        }
        worker.send(Req::Render { generation: doc.generation, page, scale });
        self.in_flight.insert(page, (scale, doc.generation));
    }

    pub fn request_text(&mut self, page: usize) {
        let (Some(worker), Some(doc)) = (&self.worker, &self.doc) else { return };
        if self.text_chars.contains_key(&page) || !self.text_requested.insert(page) {
            return;
        }
        worker.send(Req::Text { generation: doc.generation, page });
    }

    // ---------------------------------------------------------------- input

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let sc = |m, k| KeyboardShortcut::new(m, k);
        let pressed = |s: KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(&s));
        if pressed(sc(cmd, Key::F)) && self.doc.is_some() {
            self.open_search(false);
        }
        if pressed(sc(cmd, Key::O)) {
            self.request_open(None);
        }
        if pressed(sc(cmd | Modifiers::SHIFT, Key::S)) {
            self.save(ctx, true);
        } else if pressed(sc(cmd, Key::S)) {
            self.save(ctx, false);
        }
        let typing = ctx.egui_wants_keyboard_input();
        if !typing {
            if pressed(sc(cmd | Modifiers::SHIFT, Key::Z)) || pressed(sc(cmd, Key::Y)) {
                self.redo();
            } else if pressed(sc(cmd, Key::Z)) {
                self.undo();
            }
            if pressed(sc(cmd, Key::Plus)) || pressed(sc(cmd, Key::Equals)) {
                self.view.zoom_by(1.25);
            }
            if pressed(sc(cmd, Key::Minus)) {
                self.view.zoom_by(0.8);
            }
            if pressed(sc(cmd, Key::Num0)) {
                self.view.fit = Some(Fit::Width);
            }
            let none = Modifiers::NONE;
            if pressed(sc(none, Key::Delete)) || pressed(sc(none, Key::Backspace)) {
                self.delete_selection();
            }
            // Ctrl+C arrives as a Copy event rather than a key press.
            if self.text_sel.is_some() && ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
                self.copy_selection(ctx);
            }
            if pressed(sc(none, Key::Escape)) {
                self.gesture = Gesture::None;
                self.select(None);
                self.text_sel = None;
                if self.search.open {
                    self.close_search();
                }
            }
            self.vim_keys(ctx);
        }
    }

    /// Single-key commands, matched with exact modifiers (h and Shift+H differ).
    fn vim_keys(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let (keys, slash) = ctx.input(|i| {
            let keys: Vec<(Key, Modifiers)> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect();
            // `/` via the text event, so it works on any keyboard layout.
            let slash = i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "/"));
            (keys, slash)
        });
        let has_doc = self.doc.is_some();
        if slash && has_doc {
            self.open_search(true);
            return;
        }
        for (key, m) in keys {
            if m.alt || m.mac_cmd {
                continue;
            }
            if m.ctrl {
                match key {
                    Key::D => self.view.motion = Some(Motion::HalfDown),
                    Key::U => self.view.motion = Some(Motion::HalfUp),
                    _ => {}
                }
                continue;
            }
            match (key, m.shift) {
                (Key::H, false) => self.view.motion = Some(Motion::PrevPage),
                (Key::L, false) => self.view.motion = Some(Motion::NextPage),
                (Key::G, true) => self.view.motion = Some(Motion::Bottom),
                (Key::G, false) => {
                    if now - self.g_pressed_at < 0.6 {
                        self.view.motion = Some(Motion::Top);
                        self.g_pressed_at = f64::NEG_INFINITY;
                    } else {
                        self.g_pressed_at = now;
                    }
                }
                (Key::N, shift) if has_doc && !self.search.query.trim().is_empty() => {
                    if !self.search.open {
                        // Like Vim, n / N reuse the last search.
                        self.search.open = true;
                    } else {
                        self.search_step(ctx, if shift { -1 } else { 1 });
                    }
                }
                (Key::H, true) => self.set_tool(Tool::Highlighter),
                (Key::V, false) => self.set_tool(Tool::Select),
                (Key::P, false) => self.set_tool(Tool::Pen),
                (Key::T, false) => self.set_tool(Tool::Text),
                (Key::S, false) => self.set_tool(Tool::Shape(self.last_shape)),
                (Key::M, false) => self.set_tool(Tool::Markup(self.last_markup)),
                (Key::E, false) => self.set_tool(Tool::Eraser),
                _ => {}
            }
        }
    }

    // ---------------------------------------------------------------- UI

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending else { return };
        let name = self.doc.as_ref().map(Doc::name).unwrap_or_default();
        let mut choice = None;
        egui::Modal::new(egui::Id::new("unsaved")).show(ctx, |ui| {
            ui.set_max_width(360.0);
            ui.heading("Unsaved changes");
            ui.label(format!("Save your annotations to “{name}” before {}?", match pending {
                Pending::Close => "closing",
                Pending::Open(_) => "opening another file",
            }));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if crate::ui::chrome::primary_button(ui, "Save").clicked() {
                    choice = Some(0);
                }
                if ui.button("Don't save").clicked() {
                    choice = Some(1);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    choice = Some(2);
                }
            });
        });
        let Some(choice) = choice else { return };
        let pending = self.pending.take().unwrap();
        if choice == 2 || (choice == 0 && !self.save(ctx, false)) {
            return;
        }
        match pending {
            Pending::Close => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Pending::Open(p) => self.open(p),
        }
    }
}

pub fn rgb(c: [f32; 3]) -> Color32 {
    Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
}

/// Uses Liberation Sans (metric-compatible with Helvetica, which we write into
/// the PDF) for text annotations when available, so on-screen text matches.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut family = fonts.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default();
    let candidates = [
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
        "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
    ];
    if let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) {
        fonts.font_data.insert("liberation".into(), Arc::new(egui::FontData::from_owned(bytes)));
        family.insert(0, "liberation".into());
    }
    fonts.families.insert(egui::FontFamily::Name("annot".into()), family);
    // Icons, as a fallback of the normal UI font so they mix with text.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.cfg.save();
    }
}

impl App {
    /// One UI frame (separate from `eframe::App` so tests can drive it headless).
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.pump_worker(&ctx);

        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.request_open(Some(path));
        }
        if self.pending.is_none() {
            self.shortcuts(&ctx);
        }

        self.app_bar(ui);
        if self.doc.is_some() {
            self.tool_rail(ui);
        }
        let canvas = egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::current(&ctx).canvas))
            .show(ui, |ui| {
                if self.doc.as_ref().is_some_and(|d| !d.pages.is_empty()) {
                    self.viewer(ui);
                } else if self.doc.is_some() {
                    ui.centered_and_justified(|ui| ui.spinner());
                } else {
                    self.welcome(ui);
                }
            })
            .response
            .rect;
        self.canvas_overlays(&ctx, canvas);
        self.dialogs(&ctx);

        // Window title and close handling.
        let title = match &self.doc {
            Some(d) => format!("{}{} — Ochre", if d.is_dirty() { "• " } else { "" }, d.name()),
            None => "Ochre".into(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            self.commit_text();
            if self.doc.as_ref().is_some_and(Doc::is_dirty) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.pending = Some(Pending::Close);
            }
        }
    }
}
