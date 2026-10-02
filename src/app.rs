//! Application state, file handling, shortcuts and dialogs. The page canvas
//! lives in `viewer.rs`, the window chrome in `ui/`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers};

use crate::annot::model::{Annotation, Kind, MarkupKind, ShapeKind, Style};
use crate::config::{Config, ReadPos};
use crate::doc::{Cmd, Doc};
use crate::pdf::worker::{Req, Resp, Target, TextChar, Worker};
use crate::search::Search;
use crate::ui::theme;
use crate::viewer::{Fit, Gesture, Jump, Motion, NoteEdit, Overlay, PageTex, TextEditState, TextSel, View};

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

/// The tool that makes annotations like `a`.
pub(crate) fn tool_of(a: &Annotation) -> Tool {
    match &a.kind {
        Kind::Ink { highlighter: true, .. } => Tool::Highlighter,
        Kind::Ink { .. } => Tool::Pen,
        Kind::Text { .. } => Tool::Text,
        Kind::Shape { shape, .. } => Tool::Shape(*shape),
        Kind::Markup { markup, .. } => Tool::Markup(*markup),
    }
}

fn fillable(a: &Annotation) -> bool {
    matches!(a.kind, Kind::Shape { shape: ShapeKind::Rect | ShapeKind::Ellipse, .. })
}

/// Whether a group's width setting changes `a`: the font size when the group is
/// all text boxes, otherwise the line width of strokes and shapes.
fn width_applies(a: &Annotation, text_size: bool) -> bool {
    match a.kind {
        Kind::Text { .. } => text_size,
        Kind::Ink { .. } | Kind::Shape { .. } => !text_size,
        Kind::Markup { .. } => false,
    }
}

/// Style settings shown for a selected group.
pub(crate) struct GroupStyle {
    pub count: usize,
    /// The first member's style, with width and fill taken from members they apply to.
    pub style: Style,
    pub width_label: Option<&'static str>,
    /// The group is all text boxes, so width means font size.
    pub text_size: bool,
    pub fillable: bool,
}

/// What the selection is, if anything.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    /// One of our annotations, by id.
    Ours(String),
    /// Several of ours (box selection or Shift+click): moved and deleted together.
    Many(Vec<String>),
    /// An annotation from other software, by index in `Doc::foreign`.
    Foreign(usize),
}

pub(crate) enum Pending {
    /// Closing the window with unsaved changes in one or more tabs.
    Close,
    /// Closing the active tab, which has unsaved changes.
    CloseTab,
}

/// A document open in a background tab. The active tab's state lives in `App`'s
/// own fields; switching tabs swaps it with one of these.
#[derive(Default)]
pub struct Tab {
    doc: Option<Doc>,
    sent_generation: u64,
    view: View,
    tex: HashMap<usize, PageTex>,
    in_flight: HashMap<usize, (f32, u64)>,
    overlay: HashMap<usize, Overlay>,
    text_chars: HashMap<usize, Vec<TextChar>>,
    text_requested: HashSet<usize>,
    selection: Option<Selection>,
    text_sel: Option<TextSel>,
    search: Search,
    outline_toggled: HashSet<usize>,
}

/// Where `doc` is scrolled to in `view`, to remember for next time.
fn read_pos(doc: &Doc, view: &View) -> Option<ReadPos> {
    let (page, y) = view.position()?;
    if doc.pages.is_empty() {
        return None;
    }
    let path = doc.path.canonicalize().unwrap_or_else(|_| doc.path.clone());
    Some(ReadPos { path, page, y, zoom: view.zoom, fit: view.fit })
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
    /// Note being written for the selected annotation.
    pub note_edit: Option<NoteEdit>,
    pub selection: Option<Selection>,
    /// Selected page text (Select tool).
    pub text_sel: Option<TextSel>,
    pub search: Search,
    /// Id of the annotation whose style the toolbar is currently editing (merges undo steps).
    style_edit: Option<String>,
    pub(crate) status: Option<(String, f64, bool)>,
    pub(crate) pending: Option<Pending>,
    allow_close: bool,
    title: String,
    pub(crate) page_input: String,
    /// Time of the last lone `g` press (for `gg`).
    g_pressed_at: f64,
    /// Outline entries whose expanded state differs from the default (top level open).
    pub(crate) outline_toggled: HashSet<usize>,
    /// Open tabs, in order. The active one (`tabs[active]`) is an empty placeholder:
    /// its state is in the fields above. Empty when no file is open.
    pub tabs: Vec<Tab>,
    pub active: usize,
    /// Scroll the tab strip to the active tab (after switching or opening).
    pub(crate) reveal_tab: bool,
    /// How far the tab strip is scrolled (when the tabs don't fit).
    pub(crate) tab_scroll: f32,
    /// The last clipboard text pasted and how many times, to step repeated pastes.
    last_paste: Option<(String, u32)>,
    /// The last arrow-key nudge: selection, time and undo depth, to merge a run of
    /// nudges into one undo step.
    last_nudge: Option<(Vec<String>, f64, usize)>,
    /// A file dialog that's open, and where its answer arrives.
    dialog: Option<(FileDialog, crossbeam_channel::Receiver<Vec<PathBuf>>)>,
    ctx: egui::Context,
}

/// What an open file dialog is for.
#[derive(Clone, Debug)]
pub(crate) enum FileDialog {
    Open,
    /// Save As for the document at this path (its tab may not be active by then).
    SaveAs(PathBuf),
}

/// Marks clipboard text holding copied annotations (JSON after it).
const CLIP_PREFIX: &str = "ochre-annotations:v1\n";
/// How far a pasted or duplicated copy is offset (display points).
const PASTE_OFFSET: f32 = 12.0;

/// Annotations on the clipboard, with where they were copied from.
#[derive(serde::Serialize, serde::Deserialize)]
struct Clip {
    path: PathBuf,
    items: Vec<Annotation>,
}

impl App {
    pub fn new(ctx: &egui::Context, paths: impl IntoIterator<Item = PathBuf>) -> Self {
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
            note_edit: None,
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
            outline_toggled: HashSet::new(),
            tabs: Vec::new(),
            active: 0,
            reveal_tab: false,
            tab_scroll: 0.0,
            last_paste: None,
            last_nudge: None,
            dialog: None,
            ctx: ctx.clone(),
        };
        for p in paths {
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
        self.commit_edits();
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
            self.commit_note();
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
        self.commit_edits();
        if let Some(doc) = &mut self.doc {
            doc.undo();
        }
        self.after_history_jump();
    }

    pub fn redo(&mut self) {
        self.commit_edits();
        if let Some(doc) = &mut self.doc {
            doc.redo();
        }
        self.after_history_jump();
    }

    fn after_history_jump(&mut self) {
        self.style_edit = None;
        let valid = match (&self.selection, &self.doc) {
            (Some(Selection::Ours(id)), Some(doc)) => doc.get(id).is_some(),
            (Some(Selection::Many(ids)), Some(doc)) => {
                // Keep whatever still exists.
                let ids: Vec<String> = ids.iter().filter(|id| doc.get(id).is_some()).cloned().collect();
                self.select_ids(ids);
                return;
            }
            (Some(Selection::Foreign(i)), Some(doc)) => !doc.deleted_foreign.contains(i),
            _ => true,
        };
        if !valid {
            self.selection = None;
        }
    }

    // ---------------------------------------------------------------- clipboard

    /// Our selected annotations, in document order.
    fn selected_annots(&self) -> Vec<Annotation> {
        let ids = self.selected_ids();
        self.doc.iter().flat_map(|d| d.annots.iter()).filter(|a| ids.contains(&a.id)).cloned().collect()
    }

    /// Puts the selected annotations on the system clipboard (as tagged JSON, so they
    /// can be pasted into another tab or Ochre window).
    pub fn copy_annotations(&mut self, ctx: &egui::Context) -> bool {
        let Some((text, n)) = self.clip_text() else { return false };
        ctx.copy_text(text);
        self.last_paste = None;
        self.set_status(ctx, if n == 1 { "Copied 1 annotation".into() } else { format!("Copied {n} annotations") }, false);
        true
    }

    /// Clipboard text for the selected annotations, and how many there are.
    pub(crate) fn clip_text(&self) -> Option<(String, usize)> {
        let items = self.selected_annots();
        let doc = self.doc.as_ref()?;
        if items.is_empty() {
            return None;
        }
        let n = items.len();
        let json = serde_json::to_string(&Clip { path: doc.path.clone(), items }).ok()?;
        Some((format!("{CLIP_PREFIX}{json}"), n))
    }

    /// Pastes annotations from clipboard `text` onto the page in view. Pasting
    /// where they came from offsets each paste a little further, so copies don't
    /// hide the original.
    pub fn paste_annotations(&mut self, text: &str) -> bool {
        let Some(clip) = text.strip_prefix(CLIP_PREFIX).and_then(|j| serde_json::from_str::<Clip>(j).ok()) else {
            return false;
        };
        let Some(doc) = &self.doc else { return false };
        let page = self.view.current_page.min(doc.pages.len().saturating_sub(1));
        let count = match &self.last_paste {
            Some((t, n)) if t == text => n + 1,
            _ => 1,
        };
        self.last_paste = Some((text.to_owned(), count));
        let same_place = clip.path == doc.path && clip.items.iter().all(|a| a.page == page);
        let steps = if same_place { count } else { count - 1 };
        self.place_copies(clip.items, page, steps as f32);
        true
    }

    /// Duplicates the selection next to itself (Ctrl+D).
    pub fn duplicate_selection(&mut self) {
        let items = self.selected_annots();
        if let Some(page) = items.first().map(|a| a.page) {
            self.place_copies(items, page, 1.0);
        }
    }

    /// Adds copies of `items` (new ids) on `page`, moved `steps` paste offsets
    /// right and down on screen, as one undo step, and selects them.
    fn place_copies(&mut self, items: Vec<Annotation>, page: usize, steps: f32) {
        let Some(g) = self.doc.as_ref().and_then(|d| d.pages.get(page)).copied() else { return };
        let shift = g.to_user().apply_vec(crate::annot::model::Pt::new(PASTE_OFFSET, PASTE_OFFSET)).scale(steps);
        let copies: Vec<Annotation> = items
            .into_iter()
            .map(|mut a| {
                a.id = crate::annot::model::new_id();
                a.page = page;
                a.translate(shift);
                a
            })
            .collect();
        let ids = copies.iter().map(|a| a.id.clone()).collect();
        self.set_tool(Tool::Select);
        self.exec(copies.into_iter().map(Cmd::Add).collect());
        self.select_ids(ids);
    }

    /// Moves the selected annotations by `(dx, dy)` display points (arrow keys).
    /// Text markup stays on its text. A quick run of nudges is one undo step.
    fn nudge(&mut self, dx: f32, dy: f32, now: f64) {
        let ids = self.selected_ids();
        let Some(doc) = &mut self.doc else { return };
        let moved: Vec<(Annotation, Annotation)> = ids
            .iter()
            .filter_map(|id| doc.get(id))
            .filter(|a| !matches!(a.kind, Kind::Markup { .. }))
            .map(|a| {
                let mut m = a.clone();
                m.translate(doc.pages[a.page].to_user().apply_vec(crate::annot::model::Pt::new(dx, dy)));
                (a.clone(), m)
            })
            .collect();
        if moved.is_empty() {
            return;
        }
        let continuing = self
            .last_nudge
            .as_ref()
            .is_some_and(|(last, t, depth)| *last == ids && now - t < 1.0 && *depth == doc.undo_depth());
        if continuing {
            doc.amend_last_modifies(moved.into_iter().map(|(_, m)| m).collect());
        } else {
            doc.exec(moved.into_iter().map(|(before, after)| Cmd::Modify { before, after }).collect());
            self.style_edit = None;
        }
        self.last_nudge = Some((ids, now, doc.undo_depth()));
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
            Some(Selection::Many(ids)) => {
                // Highest index first, so each recorded index is right when undo
                // re-inserts them in reverse order.
                let mut removed: Vec<Cmd> = ids
                    .iter()
                    .filter_map(|id| Some(Cmd::Remove { annot: doc.get(id)?.clone(), index: doc.index_of(id)? }))
                    .collect();
                removed.sort_by_key(|c| match c {
                    Cmd::Remove { index, .. } => std::cmp::Reverse(*index),
                    _ => std::cmp::Reverse(0),
                });
                self.exec(removed);
                self.selection = None;
                return;
            }
            None => return,
        };
        self.exec(vec![cmd]);
        self.selection = None;
    }

    /// Style shown in the toolbar: the selected annotation's, else the tool's.
    pub(crate) fn shown_style(&self) -> (Style, Option<Tool>) {
        if let Some(g) = self.group_style() {
            return (g.style, Some(Tool::Pen));
        }
        if let (Some(Selection::Ours(id)), Some(doc)) = (&self.selection, &self.doc)
            && let Some(a) = doc.get(id) {
                return (a.style, Some(tool_of(a)));
            }
        (self.tool_style(self.tool), None)
    }

    /// Style settings of a selected group, if one is selected.
    pub(crate) fn group_style(&self) -> Option<GroupStyle> {
        let (Some(Selection::Many(ids)), Some(doc)) = (&self.selection, &self.doc) else { return None };
        let members: Vec<&Annotation> = ids.iter().filter_map(|id| doc.get(id)).collect();
        let first = members.first()?;
        let text_size = members.iter().all(|a| matches!(a.kind, Kind::Text { .. }));
        let mut style = first.style;
        let sized = members.iter().find(|a| width_applies(a, text_size));
        if let Some(a) = sized {
            style.width = a.style.width;
        }
        let filled = members.iter().find(|a| fillable(a));
        if let Some(a) = filled {
            (style.fill, style.fill_opacity) = (a.style.fill, a.style.fill_opacity);
        }
        let width_label = match (text_size, sized) {
            (true, _) => Some("Size"),
            (false, Some(_)) => Some("Width"),
            _ => None,
        };
        Some(GroupStyle { count: members.len(), style, width_label, text_size, fillable: filled.is_some() })
    }

    /// Applies what changed between `old` and `new` to every member of the group:
    /// color and opacity to all, width only where it means something, fill to
    /// rectangles and ellipses. Repeated changes (a slider drag) are one undo step.
    fn apply_group_style(&mut self, old: Style, new: Style, text_size: bool) {
        let ids = self.selected_ids();
        let Some(doc) = &mut self.doc else { return };
        let mut changes = Vec::new();
        for a in ids.iter().filter_map(|id| doc.get(id)) {
            let mut s = a.style;
            if new.color != old.color {
                s.color = new.color;
            }
            if new.opacity != old.opacity {
                s.opacity = new.opacity;
            }
            if new.width != old.width && width_applies(a, text_size) {
                s.width = new.width;
            }
            if fillable(a) {
                if new.fill != old.fill {
                    s.fill = new.fill;
                }
                if new.fill_opacity != old.fill_opacity {
                    s.fill_opacity = new.fill_opacity;
                }
            }
            if s != a.style {
                changes.push((a.clone(), Annotation { style: s, ..a.clone() }));
            }
        }
        let key = format!("group:{}", ids.join(","));
        if self.style_edit.as_deref() == Some(key.as_str()) {
            doc.amend_last_modifies(changes.into_iter().map(|(_, after)| after).collect());
        } else {
            doc.exec(changes.into_iter().map(|(before, after)| Cmd::Modify { before, after }).collect());
            self.style_edit = Some(key);
        }
    }

    pub(crate) fn apply_style(&mut self, style: Style) {
        if let Some(g) = self.group_style() {
            self.apply_group_style(g.style, style, g.text_size);
            return;
        }
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

    /// Opens a file (asking for one if `path` is None) in a new tab, or switches to
    /// its tab if it's already open.
    pub fn request_open(&mut self, path: Option<PathBuf>) {
        self.commit_edits();
        self.open(path);
    }

    /// Records where the active file is scrolled to (written to disk with the config).
    pub(crate) fn remember_position(&mut self) {
        if let Some(pos) = self.doc.as_ref().and_then(|d| read_pos(d, &self.view)) {
            self.cfg.set_position(pos);
        }
    }

    /// Records the position of every open file.
    fn remember_all_positions(&mut self) {
        let background: Vec<ReadPos> =
            self.tabs.iter().filter_map(|t| read_pos(t.doc.as_ref()?, &t.view)).collect();
        for pos in background {
            self.cfg.set_position(pos);
        }
        self.remember_position();
    }

    // ---------------------------------------------------------------- tabs

    /// Moves the active tab's state out of `App` (leaving it empty).
    fn take_tab(&mut self) -> Tab {
        Tab {
            doc: self.doc.take(),
            sent_generation: std::mem::take(&mut self.sent_generation),
            view: std::mem::take(&mut self.view),
            tex: std::mem::take(&mut self.tex),
            in_flight: std::mem::take(&mut self.in_flight),
            overlay: std::mem::take(&mut self.overlay),
            text_chars: std::mem::take(&mut self.text_chars),
            text_requested: std::mem::take(&mut self.text_requested),
            selection: self.selection.take(),
            text_sel: self.text_sel.take(),
            search: std::mem::take(&mut self.search),
            outline_toggled: std::mem::take(&mut self.outline_toggled),
        }
    }

    /// Makes `t` the active tab's state.
    fn put_tab(&mut self, t: Tab) {
        self.doc = t.doc;
        self.sent_generation = t.sent_generation;
        self.view = t.view;
        self.tex = t.tex;
        self.in_flight = t.in_flight;
        self.overlay = t.overlay;
        self.text_chars = t.text_chars;
        self.text_requested = t.text_requested;
        self.selection = t.selection;
        self.text_sel = t.text_sel;
        self.search = t.search;
        self.outline_toggled = t.outline_toggled;
        self.gesture = Gesture::None;
        self.editing = None;
        self.note_edit = None;
        self.style_edit = None;
    }

    /// The document in tab `i`.
    pub fn tab_doc(&self, i: usize) -> Option<&Doc> {
        if i == self.active { self.doc.as_ref() } else { self.tabs.get(i)?.doc.as_ref() }
    }

    fn tab_of(&self, path: &std::path::Path) -> Option<usize> {
        (0..self.tabs.len()).find(|&i| {
            self.tab_doc(i).is_some_and(|d| d.path == path || d.path.canonicalize().is_ok_and(|c| c == path))
        })
    }

    pub fn switch_tab(&mut self, i: usize) {
        if i == self.active || i >= self.tabs.len() {
            return;
        }
        self.commit_edits();
        let current = self.take_tab();
        self.tabs[self.active] = current;
        let next = std::mem::take(&mut self.tabs[i]);
        self.put_tab(next);
        self.active = i;
        self.reveal_tab = true;
    }

    /// Switches to the next (`step` 1) or previous (-1) tab, wrapping around.
    pub fn cycle_tab(&mut self, step: isize) {
        let n = self.tabs.len() as isize;
        if n > 1 {
            self.switch_tab((self.active as isize + step).rem_euclid(n) as usize);
        }
    }

    /// Closes tab `i`, first asking about unsaved changes.
    pub fn close_tab(&mut self, i: usize) {
        self.switch_tab(i);
        self.commit_edits();
        if self.doc.as_ref().is_some_and(Doc::is_dirty) {
            self.pending = Some(Pending::CloseTab);
        } else {
            self.close_active_tab();
        }
    }

    /// Closes the active tab without asking; its right-hand neighbour becomes active.
    pub(crate) fn close_active_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        self.remember_position();
        self.cfg.save();
        let closed = self.take_tab();
        if let Some(w) = &self.worker
            && closed.sent_generation != 0
        {
            w.send(Req::Close { generation: closed.sent_generation });
        }
        self.tabs.remove(self.active);
        if self.tabs.is_empty() {
            self.active = 0;
            self.put_tab(Tab::default());
        } else {
            self.active = self.active.min(self.tabs.len() - 1);
            let next = std::mem::take(&mut self.tabs[self.active]);
            self.put_tab(next);
        }
    }

    fn any_dirty(&self) -> bool {
        (0..self.tabs.len()).any(|i| self.tab_doc(i).is_some_and(Doc::is_dirty))
    }

    /// Saves every tab with unsaved changes; returns false if one failed.
    fn save_all(&mut self, ctx: &egui::Context) -> bool {
        for i in 0..self.tabs.len() {
            if self.tab_doc(i).is_some_and(Doc::is_dirty) {
                self.switch_tab(i);
                if !self.save(ctx, false) {
                    return false;
                }
            }
        }
        true
    }

    /// Goes to a link or table-of-contents target. Page jumps can be undone with Back.
    pub fn follow(&mut self, ctx: &egui::Context, target: &Target) {
        match target {
            Target::Page { page, x, y } => {
                let Some(g) = self.doc.as_ref().and_then(|d| d.pages.get(*page)).copied() else { return };
                self.view.push_back();
                let jump = match y {
                    Some(y) => {
                        let at = g.to_display().apply(crate::annot::model::Pt::new(x.unwrap_or(g.bbox[0]), *y));
                        Jump { page: *page, y: at.y.max(0.0), margin: 12.0, animate: true }
                    }
                    None => Jump { page: *page, y: 0.0, margin: crate::viewer::TOP_MARGIN, animate: true },
                };
                self.view.jump = Some(jump);
            }
            Target::Uri(uri) => {
                let scheme = uri.split(':').next().unwrap_or_default().to_ascii_lowercase();
                if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
                    self.set_status(ctx, format!("Not opening this kind of link: {uri}"), true);
                    return;
                }
                match std::process::Command::new("xdg-open").arg(uri).spawn() {
                    Ok(_) => self.set_status(ctx, format!("Opening {uri}"), false),
                    Err(e) => self.set_status(ctx, format!("Could not open {uri}: {e}"), true),
                }
            }
        }
    }

    /// Returns to where the last link or contents jump started.
    pub fn go_back(&mut self) {
        if let Some((page, y)) = self.view.back.pop() {
            self.view.jump = Some(Jump { page, y, margin: 0.0, animate: true });
        }
    }

    fn open(&mut self, path: Option<PathBuf>) {
        match path {
            Some(p) => {
                self.remember_position();
                self.open_now(p);
            }
            None => {
                let mut d = rfd::FileDialog::new().add_filter("PDF", &["pdf", "PDF"]);
                if let Some(dir) = self.doc.as_ref().and_then(|d| d.path.parent().map(|p| p.to_path_buf())) {
                    d = d.set_directory(dir);
                }
                self.show_dialog(FileDialog::Open, move || d.pick_files().unwrap_or_default());
            }
        }
    }

    /// Runs a file dialog on its own thread. Waiting for it on the UI thread would
    /// stop the window from responding, and the desktop offers to kill the app.
    /// The answer is picked up by [`App::poll_dialog`].
    pub(crate) fn show_dialog(&mut self, what: FileDialog, run: impl FnOnce() -> Vec<PathBuf> + Send + 'static) {
        if self.dialog.is_some() {
            return; // one at a time
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new().name("file dialog".into()).spawn(move || {
            let _ = tx.send(run());
            ctx.request_repaint();
        });
        if spawned.is_ok() {
            self.dialog = Some((what, rx));
        }
    }

    #[cfg(test)]
    pub(crate) fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// Acts on a file dialog's answer once it's there.
    fn poll_dialog(&mut self, ctx: &egui::Context) {
        let Some((what, rx)) = &self.dialog else { return };
        let paths = match rx.try_recv() {
            Ok(paths) => paths,
            Err(crossbeam_channel::TryRecvError::Empty) => return, // still open
            Err(crossbeam_channel::TryRecvError::Disconnected) => Vec::new(), // the dialog thread died
        };
        let what = what.clone();
        self.dialog = None;
        match what {
            FileDialog::Open => {
                for p in paths {
                    self.open(Some(p));
                }
            }
            FileDialog::SaveAs(doc_path) => {
                let Some(target) = paths.into_iter().next() else { return };
                let Some(i) = (0..self.tabs.len()).find(|&i| self.tab_doc(i).is_some_and(|d| d.path == doc_path)) else {
                    self.set_status(ctx, "Not saved: that file was closed", true);
                    return;
                };
                self.switch_tab(i);
                self.save_to(ctx, target);
            }
        }
    }

    fn open_now(&mut self, path: PathBuf) {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if let Some(i) = self.tab_of(&canonical) {
            self.switch_tab(i);
            return;
        }
        match Doc::open(&path) {
            Ok(doc) => {
                // A new tab right after the current one.
                if self.doc.is_some() {
                    self.commit_edits();
                    let current = self.take_tab();
                    self.tabs[self.active] = current;
                    self.active += 1;
                    self.tabs.insert(self.active, Tab::default());
                } else {
                    self.tabs = vec![Tab::default()];
                    self.active = 0;
                }
                self.put_tab(Tab { doc: Some(doc), ..Default::default() });
                self.reveal_tab = true;
                self.load_error = None;
                // Pick up where this file was left.
                if let Some(p) = self.cfg.position(&canonical) {
                    self.view.fit = p.fit;
                    if p.fit.is_none() {
                        self.view.zoom = p.zoom.clamp(0.1, 8.0);
                    }
                    self.view.jump = Some(Jump { page: p.page, y: p.y, margin: 0.0, animate: false });
                }
                self.cfg.add_recent(canonical);
                self.cfg.save();
            }
            Err(e) => {
                self.load_error = Some(format!("Could not open {}: {e}", path.display()));
            }
        }
    }

    /// Saves the active file; returns true on success. Save As asks for a file
    /// name first (without blocking) and saves when one is picked, returning false.
    pub(crate) fn save(&mut self, ctx: &egui::Context, save_as: bool) -> bool {
        self.commit_edits();
        let Some(doc) = &self.doc else { return false };
        if save_as {
            let mut d = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(doc.name());
            if let Some(dir) = doc.path.parent() {
                d = d.set_directory(dir);
            }
            self.show_dialog(FileDialog::SaveAs(doc.path.clone()), move || d.save_file().into_iter().collect());
            return false;
        }
        let target = doc.path.clone();
        self.save_to(ctx, target)
    }

    /// Saves the active file to `target`; returns true on success.
    fn save_to(&mut self, ctx: &egui::Context, target: PathBuf) -> bool {
        self.commit_edits();
        let Some(doc) = &mut self.doc else { return false };
        let kept = match &self.selection {
            Some(Selection::Foreign(_)) | None => None,
            ours => ours.clone(),
        };
        let result = doc.save_to(&target);
        match result {
            Ok(()) => {
                // Foreign indices change after a save; our ids don't.
                self.selection = kept;
                self.style_edit = None;
                self.overlay.clear();
                self.cfg.add_recent(target.canonicalize().unwrap_or(target));
                self.remember_position();
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
        if let Some(doc) = &mut self.doc
            && doc.generation != self.sent_generation {
                if self.sent_generation != 0 {
                    worker.send(Req::Close { generation: self.sent_generation });
                }
                worker.send(Req::Load {
                    generation: doc.generation,
                    bytes: doc.render_bytes(),
                    hide: doc.hidden(),
                });
                self.sent_generation = doc.generation;
                self.in_flight.clear();
            }
        let responses: Vec<Resp> = worker.rx.try_iter().collect();
        for resp in responses {
            // Results for a background tab are kept there (renders are redone on return).
            let generation = resp.generation();
            if self.doc.as_ref().is_none_or(|d| d.generation != generation) {
                let tab = self.tabs.iter_mut().find(|t| t.doc.as_ref().is_some_and(|d| d.generation == generation));
                if let Some(t) = tab
                    && let Some(doc) = &mut t.doc
                {
                    match resp {
                        Resp::Loaded { pages, outline, links, .. } => {
                            if doc.pages != pages {
                                doc.set_pages(pages);
                            }
                            doc.outline = outline;
                            doc.links = links;
                        }
                        Resp::Text { page, chars, .. } => {
                            t.text_chars.insert(page, chars);
                        }
                        Resp::Rendered { page, .. } => {
                            t.in_flight.remove(&page);
                        }
                        Resp::Failed { .. } => {}
                    }
                }
                continue;
            }
            let Some(doc) = &mut self.doc else { continue };
            match resp {
                Resp::Loaded { generation, pages, outline, links } if generation == doc.generation => {
                    if doc.pages != pages {
                        doc.set_pages(pages);
                    }
                    doc.outline = outline;
                    doc.links = links;
                }
                Resp::Failed { generation, message } if generation == doc.generation => {
                    self.load_error = Some(message);
                    self.close_active_tab();
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
            if pressed(sc(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab)) || pressed(sc(Modifiers::CTRL, Key::PageUp)) {
                self.cycle_tab(-1);
            } else if pressed(sc(Modifiers::CTRL, Key::Tab)) || pressed(sc(Modifiers::CTRL, Key::PageDown)) {
                self.cycle_tab(1);
            }
            if pressed(sc(cmd, Key::W)) && !self.tabs.is_empty() {
                self.close_tab(self.active);
            }
            // Ctrl+A: all our annotations on the current page (Select tool).
            if self.tool == Tool::Select && pressed(sc(cmd, Key::A))
                && let Some(doc) = &self.doc
            {
                let page = self.view.current_page;
                let ids = doc.annots.iter().filter(|a| a.page == page).map(|a| a.id.clone()).collect();
                self.text_sel = None;
                self.select_ids(ids);
            }
            if pressed(sc(Modifiers::ALT, Key::ArrowLeft)) {
                self.go_back();
            }
            if pressed(sc(Modifiers::NONE, Key::F9)) && self.doc.is_some() {
                self.cfg.show_outline = !self.cfg.show_outline;
            }
            let none = Modifiers::NONE;
            if pressed(sc(none, Key::Delete)) || pressed(sc(none, Key::Backspace)) {
                self.delete_selection();
            }
            // Enter opens the selected annotation's note (or edits a text box).
            if let Some(Selection::Ours(id)) = self.selection.clone()
                && self.tool == Tool::Select
                && pressed(sc(none, Key::Enter))
            {
                match self.doc.as_ref().and_then(|d| d.get(&id)).map(|a| a.takes_note()) {
                    Some(true) => self.open_note(&id),
                    Some(false) => self.edit_text(&id),
                    None => {}
                }
            }
            // Ctrl+C / X / V arrive as events rather than key presses. Copy takes the
            // page text if some is selected, else the selected annotations.
            let clip_events: Vec<egui::Event> = ctx.input(|i| {
                i.events.iter().filter(|e| matches!(e, egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_))).cloned().collect()
            });
            for e in clip_events {
                match e {
                    egui::Event::Copy if self.text_sel.is_some() => self.copy_selection(ctx),
                    egui::Event::Copy => {
                        self.copy_annotations(ctx);
                    }
                    egui::Event::Cut => {
                        if self.copy_annotations(ctx) {
                            self.delete_selection();
                        }
                    }
                    egui::Event::Paste(text) => {
                        self.paste_annotations(&text);
                    }
                    _ => {}
                }
            }
            // With annotations selected, Ctrl+D duplicates them (otherwise it's half a page down)
            // and the arrow keys nudge them (Shift: further).
            let has_ours = matches!(self.selection, Some(Selection::Ours(_) | Selection::Many(_)));
            if has_ours && self.tool == Tool::Select {
                if pressed(sc(cmd, Key::D)) {
                    self.duplicate_selection();
                }
                let now = ctx.input(|i| i.time);
                let arrows: Vec<(Key, bool)> = ctx.input(|i| {
                    i.events
                        .iter()
                        .filter_map(|e| match e {
                            egui::Event::Key { key, pressed: true, modifiers, .. }
                                if !modifiers.alt && !modifiers.command && !modifiers.ctrl =>
                            {
                                Some((*key, modifiers.shift))
                            }
                            _ => None,
                        })
                        .collect()
                });
                for (key, shift) in arrows {
                    let step = if shift { 10.0 } else { 1.0 };
                    let d = match key {
                        Key::ArrowLeft => (-step, 0.0),
                        Key::ArrowRight => (step, 0.0),
                        Key::ArrowUp => (0.0, -step),
                        Key::ArrowDown => (0.0, step),
                        _ => continue,
                    };
                    self.nudge(d.0, d.1, now);
                }
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
        let dirty: Vec<String> = match pending {
            Pending::CloseTab => self.doc.iter().map(Doc::name).collect(),
            Pending::Close => {
                (0..self.tabs.len()).filter_map(|i| self.tab_doc(i).filter(|d| d.is_dirty()).map(Doc::name)).collect()
            }
        };
        let mut choice = None;
        egui::Modal::new(egui::Id::new("unsaved")).show(ctx, |ui| {
            ui.set_max_width(360.0);
            ui.heading("Unsaved changes");
            let when = match pending {
                Pending::Close => "closing",
                Pending::CloseTab => "closing it",
            };
            if let [name] = dirty.as_slice() {
                ui.label(format!("Save your annotations to “{name}” before {when}?"));
            } else {
                ui.label(format!("Save your annotations to these files before {when}?"));
                for name in &dirty {
                    ui.label(format!("  •  {name}"));
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let save = if dirty.len() > 1 { "Save all" } else { "Save" };
                if crate::ui::chrome::primary_button(ui, save).clicked() {
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
        if choice == 2 {
            return;
        }
        match pending {
            Pending::Close => {
                if choice == 0 && !self.save_all(ctx) {
                    return;
                }
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Pending::CloseTab => {
                if choice == 0 && !self.save(ctx, false) {
                    return;
                }
                self.close_active_tab();
            }
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
        self.remember_all_positions();
        self.cfg.save();
    }
}

impl App {
    /// One UI frame (separate from `eframe::App` so tests can drive it headless).
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.pump_worker(&ctx);
        self.poll_dialog(&ctx);

        // Each dropped file opens in its own tab.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for path in dropped {
            self.request_open(Some(path));
        }
        // A file that failed to open while others are open is reported in a toast.
        if self.doc.is_some()
            && let Some(e) = self.load_error.take()
        {
            self.set_status(&ctx, e, true);
        }
        if self.pending.is_none() {
            self.shortcuts(&ctx);
        }

        self.app_bar(ui);
        if self.doc.is_some() {
            self.tool_rail(ui);
            if self.cfg.show_outline {
                self.outline_panel(ui);
            }
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
            self.commit_edits();
            if self.any_dirty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.pending = Some(Pending::Close);
            }
        }
    }
}
