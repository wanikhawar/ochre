//! Drawing the print screen: the preview in the middle, settings on the
//! right, and the dialogs of the two-sided and booklet steps.

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Id, Key, KeyboardShortcut, Label, Layout, Margin, Modifiers, Rect, RichText,
    Sense, Stroke, Vec2, pos2, text::LayoutJob, vec2,
};
use egui::{Popup, UiBuilder};
use egui_phosphor::regular as ph;

use super::{Dialog, MAX_THUMB, MIN_THUMB, Outcome, PrintScreen, THUMB_STEP, Tab, Viewer, Wanted, art, plural};
use crate::print::cups::JobState;
use crate::print::pipeline::{NUP_CHOICES, Orientation, PageSet, Scaling};
use crate::ui::chrome::{icon_button, primary_button};
use crate::ui::theme::{self, ACCENT, WARN};

const PAGE_SETS: [(&str, PageSet); 3] =
    [("All pages", PageSet::All), ("Odd pages only", PageSet::Odd), ("Even pages only", PageSet::Even)];
const ROTATIONS: [(&str, u32); 4] = [("None", 0), ("90° clockwise", 90), ("180°", 180), ("90° counter-clockwise", 270)];
const ORIENTATIONS: [(&str, Orientation); 3] =
    [("Automatic", Orientation::Auto), ("Portrait", Orientation::Portrait), ("Landscape", Orientation::Landscape)];
const SCALINGS: [(&str, Scaling); 3] = [
    ("Fit to printable area", Scaling::Fit),
    ("Shrink only if too big", Scaling::Shrink),
    ("Actual size", Scaling::Actual),
];
/// Sheets folded into one booklet (0: all of them).
const BOOKLET_SHEETS: [(&str, u32); 8] =
    [("All in one", 0), ("1", 1), ("2", 2), ("3", 3), ("4", 4), ("5", 5), ("6", 6), ("8", 8)];
const BINDINGS: [(&str, bool); 2] = [("Left (left-to-right text)", false), ("Right (right-to-left text)", true)];
/// Steps of the reload instructions; `**` marks bold.
const INSTRUCTIONS: [&str; 4] = [
    "Wait until **all front sides** have finished printing.",
    "Take the whole stack out of the output tray. **Don't reorder it.**",
    "Put it back in the paper tray so the **blank sides** get printed next.",
    "Click **Print Back Sides**.",
];
const FIRST_TIME: &str = "First time with this printer? Use **Print test** under **Manual two-sided printing** to check how the stack goes back in.";
/// Thumbnails kept beyond those in view, before the rest are dropped.
const KEEP_THUMBS: usize = 120;
/// Width of the controls on the right of a settings row.
const CONTROL: f32 = 156.0;

fn weak(ui: &egui::Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).color(theme::current(ui.ctx()).weak)
}

/// `text` with `**bold**` parts (shown in full-strength text), wrapped to `width`.
fn marked(ui: &egui::Ui, text: &str, width: f32) -> LayoutJob {
    let p = theme::current(ui.ctx());
    let mut job = LayoutJob::default();
    for (i, part) in text.split("**").enumerate() {
        let color = if i % 2 == 1 { p.text } else { p.text.gamma_multiply(0.8) };
        job.append(part, 0.0, egui::TextFormat { font_id: FontId::proportional(13.0), color, ..Default::default() });
    }
    job.wrap.max_width = width;
    job
}

/// `text` on one line, cut short with "…" if it's wider than `width`.
fn one_line(
    painter: &egui::Painter,
    text: String,
    size: f32,
    color: Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text, FontId::proportional(size), color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width);
    painter.layout_job(job)
}

/// An on/off switch.
fn switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let p = theme::current(ui.ctx());
    let (rect, mut resp) = ui.allocate_exact_size(vec2(34.0, 18.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let off = if ui.is_enabled() { p.border } else { p.border.gamma_multiply(0.6) };
    let track = egui::lerp(egui::Rgba::from(off)..=egui::Rgba::from(ACCENT), t);
    let track = if ui.is_enabled() { Color32::from(track) } else { Color32::from(track).gamma_multiply(0.5) };
    ui.painter().rect_filled(rect, 9.0, track);
    let x = egui::lerp(rect.left() + 9.0..=rect.right() - 9.0, t);
    ui.painter().circle_filled(pos2(x, rect.center().y), 7.0, Color32::WHITE);
    resp
}

/// A settings row: title (and a hint under it) on the left, the control on the
/// right, both centered on the row. Their sizes from the last frame lay it out.
fn row<R>(ui: &mut egui::Ui, title: &str, hint: &str, control: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let id = ui.id().with(("row", title, hint));
    let (label_h, control_h, control_w): (f32, f32, f32) = ui.data(|d| d.get_temp(id)).unwrap_or((18.0, 22.0, CONTROL));
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, label_h.max(control_h)), Sense::hover());
    let label_w = (width - control_w - 12.0).max(80.0);
    let at = |h: f32| rect.center().y - h / 2.0;
    let label = Rect::from_min_size(pos2(rect.left(), at(label_h)), vec2(label_w, label_h));
    let label = ui.scope_builder(UiBuilder::new().max_rect(label).layout(Layout::top_down(Align::Min)), |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.add(Label::new(title).wrap());
        if !hint.is_empty() {
            ui.add(Label::new(weak(ui, hint).small()).wrap());
        }
    });
    let right =
        Rect::from_min_max(pos2(rect.right() - CONTROL, at(control_h)), pos2(rect.right(), at(control_h) + control_h));
    let control =
        ui.scope_builder(UiBuilder::new().max_rect(right).layout(Layout::right_to_left(Align::Center)), control);
    // The cursor follows the last of the two; the row goes on to its own bottom.
    ui.advance_cursor_after_rect(rect);
    let r = control.response.rect;
    let measured = (label.response.rect.height(), r.height(), r.width().min(CONTROL));
    if (measured.0 - label_h).abs() > 0.5
        || (measured.1 - control_h).abs() > 0.5
        || (measured.2 - control_w).abs() > 0.5
    {
        ui.data_mut(|d| d.insert_temp(id, measured));
        ui.ctx().request_repaint();
    }
    control.inner
}

fn divider(ui: &mut egui::Ui) {
    let p = theme::current(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 7.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.border));
}

/// A titled card of settings rows; `suffix` goes on the right of the title.
fn section<R>(
    ui: &mut egui::Ui,
    title: &str,
    suffix: impl FnOnce(&mut egui::Ui) -> R,
    add: impl FnOnce(&mut egui::Ui),
) -> R {
    let p = theme::current(ui.ctx());
    ui.add_space(12.0);
    let r = ui
        .horizontal(|ui| {
            ui.label(weak(ui, title.to_uppercase()).small().strong());
            ui.with_layout(Layout::right_to_left(Align::Center), suffix).inner
        })
        .inner;
    ui.add_space(2.0);
    egui::Frame::new()
        .fill(p.field)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    r
}

/// A drop-down list; true if the choice changed.
fn choice<T: PartialEq + Copy>(ui: &mut egui::Ui, id: &str, value: &mut T, options: &[(&str, T)]) -> bool {
    let shown = options.iter().find(|(_, v)| v == value).map(|(l, _)| *l).unwrap_or("");
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(shown).width(CONTROL).truncate().show_ui(ui, |ui| {
        for (label, v) in options {
            changed |= ui.selectable_value(value, *v, *label).changed();
        }
    });
    changed
}

impl PrintScreen {
    pub(super) fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.keys(&ctx);
        self.top_bar(ui);
        if self.show_settings {
            self.settings(ui);
        }
        let p = theme::current(&ctx);
        let canvas = egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.canvas))
            .show(ui, |ui| {
                if self.viewer.is_some() {
                    self.viewer_ui(ui);
                } else {
                    self.preview(ui);
                }
            })
            .response
            .rect;
        self.dialogs(&ctx);
        self.draw_toast(&ctx, pos2(canvas.center().x, canvas.bottom() - 56.0));
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() {
            return;
        }
        let pressed = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));
        let typing = ctx.egui_wants_keyboard_input();
        let none = Modifiers::NONE;
        // Not straight after opening: that's the Ctrl+P that opened it, pressed twice.
        if pressed(Modifiers::COMMAND, Key::P) && ctx.input(|i| i.time) - self.opened > 1.0 {
            self.print();
        }
        if pressed(none, Key::F9) {
            self.show_settings = !self.show_settings;
        }
        if pressed(Modifiers::COMMAND, Key::O) {
            self.pick_files();
        }
        if typing {
            return;
        }
        let zoom_in = pressed(Modifiers::COMMAND, Key::Plus) || pressed(Modifiers::COMMAND, Key::Equals);
        let zoom_out = pressed(Modifiers::COMMAND, Key::Minus);
        if let Some(v) = &mut self.viewer {
            let count = match v.tab {
                Tab::Sheets => self.output.as_ref().map_or(0, |o| o.view.len()),
                Tab::Pages => self.source.as_ref().map_or(0, |s| s.sizes.len()),
            };
            if pressed(none, Key::Escape) {
                self.viewer = None;
                return;
            }
            if pressed(none, Key::ArrowRight) || pressed(none, Key::PageDown) || pressed(none, Key::Space) {
                v.index = (v.index + 1).min(count.saturating_sub(1));
            }
            if pressed(none, Key::ArrowLeft) || pressed(none, Key::PageUp) {
                v.index = v.index.saturating_sub(1);
            }
            if pressed(none, Key::Home) {
                v.index = 0;
            }
            if pressed(none, Key::End) {
                v.index = count.saturating_sub(1);
            }
            if zoom_in {
                v.zoom = (v.zoom * 1.25).min(6.0);
            }
            if zoom_out {
                v.zoom = (v.zoom / 1.25).max(0.25);
            }
            if pressed(Modifiers::COMMAND, Key::Num0) {
                v.zoom = 1.0;
            }
            return;
        }
        if pressed(none, Key::Escape) {
            self.outcome = Some(Outcome::Close);
        }
        if zoom_in {
            self.thumb_size = (self.thumb_size * THUMB_STEP).min(MAX_THUMB);
        }
        if zoom_out {
            self.thumb_size = (self.thumb_size / THUMB_STEP).max(MIN_THUMB);
        }
    }

    // ------------------------------------------------------------ top bar

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        egui::Panel::top("print-bar").frame(egui::Frame::new().fill(p.bar).inner_margin(Margin::symmetric(6, 4))).show(
            ui,
            |ui| {
                let bar = ui.max_rect();
                let tabs = Rect::from_center_size(bar.center(), vec2(190.0, 26.0));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if icon_button(ui, ph::ARROW_LEFT, "Back to the document (Esc)", true, false, 28.0).clicked() {
                        self.outcome = Some(Outcome::Close);
                    }
                    ui.label(RichText::new("Print").strong().size(14.0));
                    let room = (tabs.left() - ui.cursor().min.x - 12.0).max(0.0);
                    ui.allocate_ui(vec2(room, 24.0), |ui| {
                        ui.add(Label::new(weak(ui, self.title())).truncate());
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let tip = if self.show_settings { "Hide settings (F9)" } else { "Show settings (F9)" };
                        if icon_button(ui, ph::SIDEBAR_SIMPLE, tip, true, self.show_settings, 28.0).clicked() {
                            self.show_settings = !self.show_settings;
                        }
                        let add =
                            icon_button(ui, ph::FILE_PLUS, "Add a document to this job (Ctrl+O)", true, false, 28.0);
                        Popup::menu(&add).width(260.0).show(|ui| self.add_menu(ui));
                    });
                });
                if self.source.is_some() {
                    ui.scope_builder(UiBuilder::new().max_rect(tabs), |ui| self.tab_switch(ui));
                }
                ui.painter().hline(bar.x_range(), bar.bottom() + 4.0, Stroke::new(1.0, p.border));
            },
        );
    }

    /// Other open tabs and PDF files that can join the job.
    fn add_menu(&mut self, ui: &mut egui::Ui) {
        for (i, name) in self.tabs.clone() {
            if ui.button(format!("{}  {name}", ph::FILE_PDF)).clicked() {
                self.outcome = Some(Outcome::AddTab(i));
                ui.close();
            }
        }
        if !self.tabs.is_empty() {
            ui.separator();
        }
        if ui.button(format!("{}  PDF file…", ph::FOLDER_OPEN)).clicked() {
            self.pick_files();
            ui.close();
        }
    }

    fn tab_switch(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (tab, icon, label, tip) in [
                (Tab::Sheets, ph::PRINTER, "Preview", "The sheets as they will print"),
                (Tab::Pages, ph::FILES, "Pages", "Pick the pages to print"),
            ] {
                let on = self.tab == tab;
                let text = RichText::new(format!("{icon}  {label}")).color(if on { p.text } else { p.weak });
                let text = if on { text.strong() } else { text };
                let button =
                    egui::Button::new(text).fill(if on { p.hover } else { Color32::TRANSPARENT }).corner_radius(6);
                if ui.add(button).on_hover_text(tip).clicked() {
                    self.tab = tab;
                    self.viewer = None;
                }
            }
        });
    }

    // ------------------------------------------------------------ preview

    fn preview(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        for problem in [&self.printer_error, &self.output_error].into_iter().flatten() {
            egui::Frame::new().fill(WARN.gamma_multiply(0.18)).inner_margin(Margin::symmetric(12, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("{}  {problem}", ph::WARNING_CIRCLE)).color(p.text));
            });
        }
        if self.source.is_none() {
            ui.centered_and_justified(|ui| match &self.load_error {
                Some(e) => {
                    ui.label(RichText::new(format!("{}  {e}", ph::WARNING_CIRCLE)).color(ui.visuals().error_fg_color));
                }
                None => {
                    ui.spinner();
                }
            });
            return;
        }
        let tab = self.tab;
        egui::Panel::bottom("print-zoom")
            .frame(egui::Frame::new().fill(p.bar).inner_margin(Margin::symmetric(10, 4)))
            .show(ui, |ui| self.bottom_strip(ui, tab));
        self.grid(ui, tab);
    }

    /// The line under the thumbnails: what they show, page selection and zoom.
    fn bottom_strip(&mut self, ui: &mut egui::Ui, tab: Tab) {
        let p = theme::current(ui.ctx());
        ui.painter().hline(ui.max_rect().x_range(), ui.max_rect().top() - 4.0, Stroke::new(1.0, p.border));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let n = self.source.as_ref().map_or(0, |s| s.sizes.len());
            if tab == Tab::Pages {
                let selected = self.selected.len();
                let what = if selected == 0 || selected == n {
                    format!("All {n} pages will print")
                } else {
                    format!("{selected} of {n} pages selected")
                };
                ui.label(weak(ui, what));
                ui.add_space(6.0);
                if ui.button("Select all").clicked() {
                    self.selected = (0..n).collect();
                    self.ranges_from_selection();
                }
                if ui.button("Clear").clicked() {
                    self.selected.clear();
                    self.ranges_from_selection();
                }
                if ui.button("Invert").clicked() {
                    self.selected = (0..n).filter(|i| !self.selected.contains(i)).collect();
                    self.ranges_from_selection();
                }
            } else {
                ui.add(Label::new(weak(ui, self.info())).truncate());
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, ph::PLUS, "Bigger (Ctrl++)", true, false, 22.0).clicked() {
                    self.thumb_size = (self.thumb_size * THUMB_STEP).min(MAX_THUMB);
                }
                ui.spacing_mut().slider_width = 110.0;
                ui.add(
                    egui::Slider::new(&mut self.thumb_size, MIN_THUMB..=MAX_THUMB).show_value(false).logarithmic(true),
                );
                if icon_button(ui, ph::MINUS, "Smaller (Ctrl+−)", true, false, 22.0).clicked() {
                    self.thumb_size = (self.thumb_size / THUMB_STEP).max(MIN_THUMB);
                }
            });
        });
    }

    /// What the preview shows.
    fn info(&self) -> String {
        let docs = self.source.as_ref().map_or(0, |s| s.doc_pages.len());
        let docs = if docs > 1 { format!("{} · ", plural(docs, "document", "documents")) } else { String::new() };
        match &self.output {
            Some(out) if out.job.booklet => format!("{docs}Booklet spreads, fronts then backs"),
            Some(out) if out.job.two_sided() => format!("{docs}All fronts, then all backs, in print order"),
            Some(_) => format!("{docs}Sheets in print order · double-click one to zoom in"),
            None => String::new(),
        }
    }

    /// Captions of thumbnail `i`.
    fn caption(&self, tab: Tab, i: usize) -> (String, String) {
        match tab {
            Tab::Sheets => self.output.as_ref().and_then(|o| o.labels.get(i).cloned()).unwrap_or_default(),
            Tab::Pages => {
                let Some(src) = &self.source else { return Default::default() };
                if src.doc_pages.len() < 2 {
                    return ((i + 1).to_string(), String::new());
                }
                // The page number in the job, and the document's own (first, so it isn't cut off).
                let mut start = 0;
                for (e, &n) in self.entries.iter().zip(&src.doc_pages) {
                    if i < start + n {
                        let stem = std::path::Path::new(&e.title).file_stem().map(|s| s.to_string_lossy().into_owned());
                        return ((i + 1).to_string(), format!("p. {} · {}", i - start + 1, stem.unwrap_or_default()));
                    }
                    start += n;
                }
                ((i + 1).to_string(), String::new())
            }
        }
    }

    fn grid(&mut self, ui: &mut egui::Ui, tab: Tab) {
        let p = theme::current(ui.ctx());
        let count = match tab {
            Tab::Sheets => self.output.as_ref().map_or(0, |o| o.view.len()),
            Tab::Pages => self.source.as_ref().map_or(0, |s| s.sizes.len()),
        };
        if count == 0 {
            return;
        }
        // Ctrl+scroll (or a pinch) zooms.
        if ui.rect_contains_pointer(ui.max_rect()) {
            let zoom = ui.input(|i| i.zoom_delta());
            if zoom != 1.0 {
                self.thumb_size = (self.thumb_size * zoom).clamp(MIN_THUMB, MAX_THUMB);
            }
        }
        let thumb = self.thumb_size;
        let cell = vec2(thumb + 28.0, thumb + 50.0);
        let width = ui.available_width();
        let cols = ((width - 24.0) / cell.x).floor().max(1.0) as usize;
        let rows = count.div_ceil(cols);
        let pad = ((width - cols as f32 * cell.x) / 2.0).max(0.0);
        let ppp = ui.ctx().pixels_per_point();
        let mut visible = Vec::new();
        let mut clicked = None;
        let mut opened = None;
        egui::ScrollArea::vertical().id_salt(("print-grid", tab == Tab::Pages)).auto_shrink(false).show_rows(
            ui,
            cell.y,
            rows + 1,
            |ui, range| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                for r in range {
                    ui.horizontal(|ui| {
                        ui.add_space(pad);
                        for c in 0..cols {
                            let i = r * cols + c;
                            if i >= count {
                                ui.allocate_exact_size(cell, Sense::hover());
                                continue;
                            }
                            let (rect, resp) = ui.allocate_exact_size(cell, Sense::click());
                            if !ui.is_rect_visible(rect) {
                                continue;
                            }
                            visible.push(i);
                            let selected = tab == Tab::Pages && self.selected.contains(&i);
                            if selected {
                                ui.painter().rect_filled(rect.shrink(3.0), 8.0, p.accent_tint);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(rect.shrink(3.0), 8.0, p.hover);
                            }
                            let (w, h) = self.thumb_size_of(tab, i);
                            let k = thumb / w.max(h).max(1.0) as f32;
                            let size = vec2(w as f32 * k, h as f32 * k);
                            // Sitting on the caption, whatever their shape.
                            let base = pos2(rect.center().x, rect.top() + 12.0 + thumb);
                            let frame = Rect::from_min_size(base - vec2(size.x / 2.0, size.y), size);
                            let painter = ui.painter();
                            painter.rect_filled(frame.translate(vec2(0.0, 1.5)), 1.0, Color32::from_black_alpha(30));
                            painter.rect_filled(frame, 0.0, Color32::WHITE);
                            let thumbs = if tab == Tab::Sheets { &self.sheets } else { &self.pages };
                            if let Some((tex, _)) = thumbs.tex.get(&i) {
                                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                                painter.image(tex.id(), frame, uv, Color32::WHITE);
                            }
                            let border = if selected { Stroke::new(2.0, ACCENT) } else { Stroke::new(1.0, p.border) };
                            painter.rect_stroke(frame, 0.0, border, egui::StrokeKind::Outside);
                            if tab == Tab::Pages {
                                let at = frame.right_top() + vec2(-11.0, 11.0);
                                let icon = if selected { ph::CHECK_CIRCLE } else { ph::CIRCLE };
                                if selected {
                                    painter.circle_filled(at, 8.0, Color32::WHITE);
                                }
                                let color = if selected { ACCENT } else { Color32::from_black_alpha(90) };
                                painter.text(at, Align2::CENTER_CENTER, icon, FontId::proportional(18.0), color);
                            }
                            let (a, b) = self.caption(tab, i);
                            let y = rect.top() + 16.0 + thumb;
                            for (text, dy, size, color) in [(a, 0.0, 12.0, p.text), (b, 15.0, 11.0, p.weak)] {
                                let g = one_line(painter, text, size, color, cell.x - 8.0);
                                painter.galley(pos2(rect.center().x - g.size().x / 2.0, y + dy), g, color);
                            }
                            if resp.double_clicked() {
                                opened = Some(i);
                            } else if resp.clicked() {
                                clicked = Some((i, ui.input(|inp| inp.modifiers.shift)));
                            }
                        }
                    });
                }
            },
        );
        // Long documents: only what's in view stays in memory.
        let thumbs = if tab == Tab::Sheets { &mut self.sheets } else { &mut self.pages };
        if thumbs.tex.len() > KEEP_THUMBS {
            thumbs.tex.retain(|i, _| visible.contains(i));
        }
        let px = (thumb * ppp).round() as u32;
        for i in visible {
            self.want_thumb(tab, i, px);
        }
        if let Some((i, shift)) = clicked
            && tab == Tab::Pages
        {
            self.click_page(i, shift);
        }
        if let Some(i) = opened {
            if tab == Tab::Pages {
                // The double click's first click ticked it; leave it as it was.
                self.click_page(i, false);
            }
            self.viewer = Some(Viewer { tab, index: i, zoom: 1.0, tex: None, pending: None });
        }
    }

    // ------------------------------------------------------------ viewer

    fn viewer_ui(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        let Some(v) = &self.viewer else { return };
        let (tab, index) = (v.tab, v.index);
        let count = match tab {
            Tab::Sheets => self.output.as_ref().map_or(0, |o| o.view.len()),
            Tab::Pages => self.source.as_ref().map_or(0, |s| s.sizes.len()),
        };
        if index >= count {
            self.viewer = None;
            return;
        }
        let (a, b) = self.caption(tab, index);
        let what = if tab == Tab::Sheets { "Sheet" } else { "Page" };
        let mut close = false;
        egui::Panel::top("print-viewer-bar")
            .frame(egui::Frame::new().fill(p.bar).inner_margin(Margin::symmetric(6, 4)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    close = icon_button(ui, ph::ARROW_LEFT, "Back to all of them (Esc)", true, false, 26.0).clicked();
                    let v = self.viewer.as_mut().unwrap();
                    if icon_button(ui, ph::CARET_LEFT, "Previous (←)", index > 0, false, 26.0).clicked() {
                        v.index -= 1;
                    }
                    ui.label(format!("{what} {} of {count}", index + 1));
                    if icon_button(ui, ph::CARET_RIGHT, "Next (→)", index + 1 < count, false, 26.0).clicked() {
                        v.index += 1;
                    }
                    // "Front 2 · pp. 3–4"; plain sheet numbers are said already.
                    let detail =
                        if b.is_empty() || a == (index + 1).to_string() { b.clone() } else { format!("{a} · {b}") };
                    ui.label(weak(ui, detail));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, ph::ARROWS_OUT_SIMPLE, "Fit (Ctrl+0)", true, v.zoom == 1.0, 26.0).clicked() {
                            v.zoom = 1.0;
                        }
                        if icon_button(ui, ph::MAGNIFYING_GLASS_PLUS, "Zoom in (Ctrl++)", true, false, 26.0).clicked() {
                            v.zoom = (v.zoom * 1.25).min(6.0);
                        }
                        ui.label(weak(ui, format!("{:.0}%", v.zoom * 100.0)));
                        if icon_button(ui, ph::MAGNIFYING_GLASS_MINUS, "Zoom out (Ctrl+−)", true, false, 26.0).clicked()
                        {
                            v.zoom = (v.zoom / 1.25).max(0.25);
                        }
                    });
                });
            });
        if close {
            self.viewer = None;
            return;
        }
        let Some(v) = &self.viewer else { return };
        let (index, zoom) = (v.index, v.zoom);
        let (w, h) = self.thumb_size_of(tab, index);
        let avail = ui.available_size();
        let fit = ((avail.x - 48.0) / w as f32).min((avail.y - 48.0) / h as f32).max(0.01);
        let size = vec2(w as f32, h as f32) * fit * zoom;
        // Rendered in steps, so zooming doesn't ask for a new image every frame.
        let scale = (fit * zoom * ui.ctx().pixels_per_point() * 8.0).round() / 8.0;
        let rev = self.rev(tab);
        let v = self.viewer.as_mut().unwrap();
        let have = v.tex.as_ref().is_some_and(|(_, i, s)| *i == index && *s == scale);
        if !have && v.pending != Some((index, scale)) {
            v.pending = Some((index, scale));
            self.request(Wanted::View { tab, rev, index, scale });
        }
        let v = self.viewer.as_ref().unwrap();
        // While a sharper image is coming, show what there is of this sheet.
        let tex = v.tex.as_ref().filter(|(_, i, _)| *i == index).map(|(t, ..)| t.id());
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            let space = vec2(ui.available_width().max(size.x + 48.0), ui.available_height().max(size.y + 48.0));
            let (area, _) = ui.allocate_exact_size(space, Sense::hover());
            let r = Rect::from_center_size(area.center(), size);
            let painter = ui.painter();
            painter.rect_filled(r.translate(vec2(0.0, 2.0)), 1.0, Color32::from_black_alpha(40));
            painter.rect_filled(r, 0.0, Color32::WHITE);
            if let Some(id) = tex {
                painter.image(id, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
        });
    }

    // ------------------------------------------------------------ settings

    fn settings(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        let window = ui.ctx().content_rect().width();
        let max = (window * 0.5).max(280.0);
        egui::Panel::right("print-settings")
            .resizable(true)
            .default_size(370f32.min(max))
            .size_range(280f32.min(max)..=max.min(520.0))
            .frame(egui::Frame::new().fill(p.bar))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().vline(r.left(), r.y_range(), Stroke::new(1.0, p.border));
                egui::Panel::bottom("print-footer")
                    .frame(egui::Frame::new().fill(p.bar).inner_margin(Margin::same(12)))
                    .show(ui, |ui| self.footer(ui));
                egui::CentralPanel::default().frame(egui::Frame::new().inner_margin(Margin::symmetric(12, 0))).show(
                    ui,
                    |ui| {
                        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            // Fields and drop-downs get a visible box so it's clear they can be changed.
                            ui.visuals_mut().text_edit_bg_color = Some(p.bar);
                            let w = &mut ui.visuals_mut().widgets;
                            w.inactive.weak_bg_fill = p.bar;
                            w.inactive.bg_stroke = Stroke::new(1.0, p.border);
                            w.hovered.bg_stroke = Stroke::new(1.0, p.weak);
                            w.hovered.weak_bg_fill = p.hover;
                            self.printer_section(ui);
                            self.documents_section(ui);
                            self.pages_section(ui);
                            self.layout_section(ui);
                            self.two_sided_section(ui);
                            self.driver_sections(ui);
                            ui.add_space(12.0);
                        });
                    },
                );
            });
    }

    fn printer_section(&mut self, ui: &mut egui::Ui) {
        section(
            ui,
            "Printer",
            |_| {},
            |ui| {
                row(ui, "Printer", "", |ui| {
                    let shown = self.printer.clone().unwrap_or_else(|| "None".into());
                    let mut picked = None;
                    egui::ComboBox::from_id_salt("printer").selected_text(shown).width(CONTROL).truncate().show_ui(
                        ui,
                        |ui| {
                            for name in &self.printers {
                                if ui.selectable_label(self.printer.as_ref() == Some(name), name).clicked() {
                                    picked = Some(name.clone());
                                }
                            }
                        },
                    );
                    if let Some(name) = picked.filter(|n| self.printer.as_ref() != Some(n)) {
                        self.select_printer(name);
                    }
                    if self.info_loading {
                        ui.spinner();
                    }
                });
                divider(ui);
                row(ui, "Preset", "", |ui| {
                    let has = self.preset.clone();
                    if icon_button(ui, ph::TRASH, "Delete this preset", has.is_some(), false, 22.0).clicked()
                        && let Some(name) = has
                    {
                        self.dialog = Some(Dialog::DeletePreset(name));
                    }
                    if icon_button(ui, ph::FLOPPY_DISK, "Save the current settings as a preset", true, false, 22.0)
                        .clicked()
                    {
                        self.dialog = Some(Dialog::SavePreset(self.preset.clone().unwrap_or_default()));
                    }
                    let mut picked = None;
                    let shown = self.preset.clone().unwrap_or_else(|| "None".into());
                    egui::ComboBox::from_id_salt("preset")
                        .selected_text(shown)
                        .width(CONTROL - 52.0)
                        .truncate()
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(self.preset.is_none(), "None").clicked() {
                                picked = Some(None);
                            }
                            for name in &self.presets {
                                if ui.selectable_label(self.preset.as_ref() == Some(name), name).clicked() {
                                    picked = Some(Some(name.clone()));
                                }
                            }
                        });
                    match picked {
                        Some(Some(name)) => self.apply_preset(&name),
                        Some(None) => self.preset = None,
                        None => {}
                    }
                });
            },
        );
    }

    fn documents_section(&mut self, ui: &mut egui::Ui) {
        let several = self.entries.len() > 1;
        let mut remove = None;
        let mut moved = None;
        let add = section(
            ui,
            "Documents",
            |ui| icon_button(ui, ph::PLUS, "Add a document to this job (Ctrl+O)", true, false, 20.0),
            |ui| {
                let drag_id = Id::new("print-doc-drag");
                let dragging: Option<usize> = ui.data(|d| d.get_temp(drag_id));
                let mut rows = Vec::new();
                for (i, e) in self.entries.iter().enumerate() {
                    if i > 0 {
                        divider(ui);
                    }
                    let r = ui.horizontal(|ui| {
                        if several {
                            let handle =
                                ui.add(Label::new(weak(ui, ph::DOTS_SIX_VERTICAL).size(16.0)).sense(Sense::drag()));
                            if handle.drag_started() {
                                ui.data_mut(|d| d.insert_temp(drag_id, i));
                            }
                            if handle.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                            }
                            handle.on_hover_text("Drag to reorder");
                        }
                        ui.vertical(|ui| {
                            ui.set_width((ui.available_width() - 30.0).max(60.0));
                            ui.add(Label::new(&e.title).truncate());
                            let pages = e.pages.map(|n| plural(n, "page", "pages")).unwrap_or_else(|| "…".into());
                            ui.label(weak(ui, pages).small());
                        });
                        if several {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if icon_button(ui, ph::TRASH, "Remove from this job", true, false, 22.0).clicked() {
                                    remove = Some(i);
                                }
                            });
                        }
                    });
                    rows.push(r.response.rect);
                }
                // Dragging: a line shows where the document will go; letting go moves it.
                if let (Some(from), Some(at)) = (dragging, ui.input(|i| i.pointer.interact_pos())) {
                    let to = rows.iter().position(|r| at.y < r.bottom()).unwrap_or(rows.len() - 1);
                    let y = if to > from { rows[to].bottom() + 3.0 } else { rows[to].top() - 3.0 };
                    ui.painter().hline(rows[0].x_range(), y, Stroke::new(2.0, ACCENT));
                    if ui.input(|i| i.pointer.any_released()) {
                        ui.data_mut(|d| d.remove::<usize>(drag_id));
                        moved = Some((from, to));
                    }
                } else if dragging.is_some() {
                    ui.data_mut(|d| d.remove::<usize>(drag_id));
                }
                if several {
                    divider(ui);
                    row(ui, "Start each on a new sheet", "So documents never share a sheet of paper", |ui| {
                        switch(ui, &mut self.job.separate_docs);
                    });
                    ui.label(weak(ui, "Printed in this order.").small());
                }
            },
        );
        Popup::menu(&add).width(260.0).show(|ui| self.add_menu(ui));
        if let Some(i) = remove {
            self.remove_doc(i);
        }
        if let Some((from, to)) = moved {
            self.move_doc(from, to);
        }
    }

    fn pages_section(&mut self, ui: &mut egui::Ui) {
        section(
            ui,
            "Pages",
            |_| {},
            |ui| {
                row(ui, "Pages", "e.g. 1-3, 7, 10-", |ui| {
                    let edit =
                        egui::TextEdit::singleline(&mut self.job.page_ranges).hint_text("All").desired_width(CONTROL);
                    if ui.add(edit).changed() {
                        self.select_from_ranges();
                    }
                });
                divider(ui);
                ui.add_enabled_ui(!self.job.booklet, |ui| {
                    row(ui, "Print", "", |ui| choice(ui, "page-set", &mut self.job.page_set, &PAGE_SETS));
                });
                divider(ui);
                row(ui, "Copies", "", |ui| {
                    ui.add(egui::DragValue::new(&mut self.job.copies).range(1..=999).speed(0.1));
                });
                divider(ui);
                row(ui, "Collate", "1, 2, 3, 1, 2, 3 instead of 1, 1, 2, 2, 3, 3", |ui| {
                    switch(ui, &mut self.job.collate);
                });
                divider(ui);
                row(ui, "Reverse order", "Last page first, for printers that stack pages face up", |ui| {
                    switch(ui, &mut self.job.reverse);
                });
            },
        );
    }

    fn layout_section(&mut self, ui: &mut egui::Ui) {
        section(
            ui,
            "Layout",
            |_| {},
            |ui| {
                let booklet = self.job.booklet;
                let one_up = self.job.nup == 1 && !booklet;
                ui.add_enabled_ui(!booklet, |ui| {
                    let hint = if booklet { "2, side by side, while making a booklet" } else { "" };
                    row(ui, "Pages per sheet", hint, |ui| {
                        let options: Vec<(String, usize)> = NUP_CHOICES.iter().map(|n| (n.to_string(), *n)).collect();
                        let options: Vec<(&str, usize)> = options.iter().map(|(l, n)| (l.as_str(), *n)).collect();
                        choice(ui, "nup", &mut self.job.nup, &options)
                    });
                });
                divider(ui);
                ui.add_enabled_ui(one_up, |ui| {
                    row(ui, "Orientation", "Automatic turns the paper to match each page", |ui| {
                        choice(ui, "orientation", &mut self.job.orientation, &ORIENTATIONS)
                    });
                    divider(ui);
                    row(ui, "Scaling", "", |ui| choice(ui, "scaling", &mut self.job.scaling, &SCALINGS));
                });
                divider(ui);
                row(ui, "Rotate", "", |ui| choice(ui, "rotate", &mut self.job.rotate, &ROTATIONS));
                divider(ui);
                let mut on = self.job.booklet;
                row(ui, "Booklet", "Two pages per side, ordered so the folded stack reads like a book", |ui| {
                    switch(ui, &mut on);
                });
                self.set_booklet(on);
                if self.job.booklet {
                    ui.indent("booklet", |ui| {
                        divider(ui);
                        row(ui, "Sheets per booklet", "Thick booklets fold better as several thin ones", |ui| {
                            choice(ui, "booklet-sheets", &mut self.job.booklet_sheets, &BOOKLET_SHEETS)
                        });
                        divider(ui);
                        row(ui, "Binding", "", |ui| choice(ui, "binding", &mut self.job.booklet_rtl, &BINDINGS));
                        divider(ui);
                        row(ui, "Gutter", "Extra space at the fold", |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.job.gutter_mm)
                                    .range(0.0..=30.0)
                                    .speed(0.2)
                                    .suffix(" mm"),
                            );
                        });
                    });
                }
            },
        );
    }

    fn two_sided_section(&mut self, ui: &mut egui::Ui) {
        section(
            ui,
            "Two-sided",
            |_| {},
            |ui| {
                let mut on = self.job.duplex;
                row(
                    ui,
                    "Manual two-sided printing",
                    "Prints the fronts, then walks you through reloading the stack for the backs",
                    |ui| {
                        switch(ui, &mut on);
                    },
                );
                if on != self.job.duplex {
                    self.set_duplex(on);
                }
                if self.job.duplex {
                    ui.indent("duplex", |ui| {
                        divider(ui);
                        row(ui, "Reverse order of back sides", "", |ui| {
                            switch(ui, &mut self.job.backs_reverse);
                        });
                        divider(ui);
                        row(ui, "Rotate back sides 180°", "", |ui| {
                            switch(ui, &mut self.job.backs_rotate);
                        });
                        divider(ui);
                        ui.label("Your reloading note");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.note)
                                .hint_text("e.g. printed side up, top edge first")
                                .desired_width(f32::INFINITY),
                        );
                        divider(ui);
                        row(
                            ui,
                            "Calibration test",
                            "Prints 2 sheets that show whether the settings above are right",
                            |ui| {
                                let enabled = self.printer.is_some() && self.busy.is_none();
                                if ui.add_enabled(enabled, egui::Button::new("Print test")).clicked() {
                                    self.print_calibration();
                                }
                            },
                        );
                    });
                }
            },
        );
    }

    /// The printer driver's options: the everyday ones, then the rest by group.
    fn driver_sections(&mut self, ui: &mut egui::Ui) {
        if self.printer.is_none() {
            return;
        }
        let options = self.info.options.clone();
        section(
            ui,
            "Printer settings",
            |_| {},
            |ui| {
                if self.info_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(weak(ui, "Asking the printer…"));
                    });
                    return;
                }
                if options.is_empty() {
                    ui.label(weak(ui, "This printer doesn't list any options."));
                }
                for (n, opt) in options.iter().filter(|o| o.common).enumerate() {
                    if n > 0 {
                        divider(ui);
                    }
                    self.driver_option(ui, opt);
                }
            },
        );
        let advanced: Vec<_> = options.iter().filter(|o| !o.common).collect();
        if advanced.is_empty() || self.info_loading {
            return;
        }
        section(
            ui,
            "Advanced",
            |_| {},
            |ui| {
                let mut groups: Vec<&str> = Vec::new();
                for o in &advanced {
                    if !groups.contains(&o.group.as_str()) {
                        groups.push(&o.group);
                    }
                }
                for group in groups {
                    egui::CollapsingHeader::new(group).id_salt(("print-group", group)).default_open(false).show(
                        ui,
                        |ui| {
                            for (n, opt) in advanced.iter().filter(|o| o.group == group).enumerate() {
                                if n > 0 {
                                    divider(ui);
                                }
                                self.driver_option(ui, opt);
                            }
                        },
                    );
                }
            },
        );
    }

    fn driver_option(&mut self, ui: &mut egui::Ui, opt: &crate::print::ppd::PrinterOption) {
        row(ui, &opt.label, "", |ui| {
            let current = self.ppd.get(&opt.keyword).cloned().unwrap_or_else(|| opt.default.clone());
            let shown = opt.choices.iter().find(|(v, _)| *v == current).map(|(_, l)| l.clone()).unwrap_or_default();
            let mut picked = None;
            egui::ComboBox::from_id_salt(("ppd", &opt.keyword)).selected_text(shown).width(CONTROL).truncate().show_ui(
                ui,
                |ui| {
                    for (value, label) in &opt.choices {
                        if ui.selectable_label(*value == current, label).clicked() {
                            picked = Some(value.clone());
                        }
                    }
                },
            );
            if let Some(v) = picked {
                self.ppd.insert(opt.keyword.clone(), v);
            }
        });
    }

    /// How much paper, and the buttons.
    fn footer(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        let r = ui.max_rect();
        ui.painter().hline(r.x_range(), r.top() - 12.0, Stroke::new(1.0, p.border));
        let (headline, detail) = self.summary();
        ui.horizontal(|ui| {
            let (icon, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
            ui.painter().circle_filled(icon.center(), 18.0, p.accent_tint);
            ui.painter().text(icon.center(), Align2::CENTER_CENTER, ph::PRINTER, FontId::proportional(18.0), ACCENT);
            ui.vertical(|ui| {
                ui.set_width((ui.available_width() - 24.0).max(60.0));
                ui.label(RichText::new(headline).strong().size(15.0));
                ui.add(Label::new(weak(ui, detail).small()).wrap());
            });
            if self.busy.is_some() {
                ui.spinner();
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let more = icon_button(ui, ph::DOTS_THREE, "More", true, false, 28.0);
            Popup::menu(&more).width(180.0).show(|ui| {
                let can = self.output.is_some() && self.busy.is_none() && self.current();
                if ui.add_enabled(can, egui::Button::new(format!("{}  Save as PDF…", ph::FILE_ARROW_DOWN))).clicked()
                {
                    self.save_pdf();
                    ui.close();
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can = self.can_print();
                let print = ui.add_enabled_ui(can, |ui| primary_button(ui, &format!("{}  Print", ph::PRINTER))).inner;
                if print.on_hover_text("Ctrl+P").clicked() {
                    self.print();
                }
                if ui.add(egui::Button::new("Cancel").min_size(vec2(0.0, 28.0))).clicked() {
                    self.outcome = Some(Outcome::Close);
                }
            });
        });
    }

    /// "3 sheets of paper" and what's special about them.
    fn summary(&self) -> (String, String) {
        if let Some(busy) = &self.busy {
            let headline = self.output.as_ref().map(|o| plural(o.paper(), "sheet of paper", "sheets of paper"));
            return (headline.unwrap_or_else(|| "Getting ready".into()), busy.clone());
        }
        let Some(out) = &self.output else {
            return ("Nothing to print".into(), String::new());
        };
        let paper = out.paper();
        let mut detail = if out.job.booklet {
            let per = out.job.booklet_sheets as usize;
            if per > 0 && paper > per {
                format!("{} to fold · printed in two passes", plural(paper.div_ceil(per), "booklet", "booklets"))
            } else {
                "One booklet to fold · printed in two passes".to_string()
            }
        } else if out.job.duplex {
            "Printed in two passes: fronts, then backs".to_string()
        } else {
            plural(out.page_count(), "page", "pages")
        };
        let saved = out.page_count().saturating_sub(paper);
        if saved > 0 {
            detail += &format!(" · saves {}", plural(saved, "sheet", "sheets"));
        }
        if let Some(printer) = &self.printer {
            detail += &format!(" · on {printer}");
        }
        (plural(paper, "sheet of paper", "sheets of paper"), detail)
    }

    // ------------------------------------------------------------ dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else { return };
        let escape = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        let enter = ctx.input(|i| i.key_pressed(Key::Enter));
        // What was chosen: None while still open; Some(true) for the main action.
        let mut chosen: Option<bool> = escape.then_some(false);
        let note = self.note.trim().to_string();
        egui::Modal::new(Id::new("print-dialog")).show(ctx, |ui| match dialog {
            Dialog::SavePreset(name) => {
                ui.set_width(340.0);
                ui.heading("Save Preset");
                ui.label("Saves the current page and printer settings under a name.");
                ui.add_space(6.0);
                let edit = ui.add(
                    egui::TextEdit::singleline(name)
                        .hint_text("e.g. Draft, grayscale, fast")
                        .desired_width(f32::INFINITY),
                );
                edit.request_focus();
                ui.add_space(8.0);
                buttons(ui, "Save", false, &mut chosen);
                if enter && !name.trim().is_empty() {
                    chosen = Some(true);
                }
            }
            Dialog::DeletePreset(name) => {
                ui.set_width(320.0);
                ui.heading("Delete Preset?");
                ui.label(format!("“{name}” will be removed."));
                ui.add_space(8.0);
                buttons(ui, "Delete", true, &mut chosen);
            }
            Dialog::Reload(r) => {
                ui.set_width(420.0);
                ui.heading(format!("Printing Front Sides ({})", plural(r.send.sheets, "sheet", "sheets")));
                if note.is_empty() {
                    ui.label(marked(ui, FIRST_TIME, 420.0));
                }
                ui.add_space(8.0);
                art::reload(ui);
                for (i, step) in INSTRUCTIONS.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 11.0, ACCENT);
                        let n = (i + 1).to_string();
                        ui.painter().text(
                            dot.center(),
                            Align2::CENTER_CENTER,
                            n,
                            FontId::proportional(12.0),
                            Color32::WHITE,
                        );
                        ui.label(marked(ui, step, 380.0));
                    });
                }
                if !note.is_empty() {
                    ui.add_space(6.0);
                    let p = theme::current(ui.ctx());
                    egui::Frame::new().fill(p.field).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("Your note for this printer").small().strong().color(ACCENT));
                        ui.label(&note);
                    });
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let job = r.job_id;
                    let text = match r.state {
                        JobState::Done => {
                            format!("✔ Job {job}: front sides printed. Reload the stack, then print the back sides.")
                        }
                        JobState::Printing => format!("Job {job}: printing front sides…"),
                        s => format!("Job {job}: {}", s.describe()),
                    };
                    if !r.state.is_final() {
                        ui.spinner();
                    }
                    ui.add(Label::new(weak(ui, text)).wrap());
                });
                ui.add_space(8.0);
                buttons(ui, "Print Back Sides", false, &mut chosen);
            }
            Dialog::Fold { booklets, per, copies, .. } => {
                ui.set_width(400.0);
                ui.heading("Fold Your Booklet");
                let mut body = String::from(
                    "When the back sides have printed, keep the sheets in the order they came out.\n\n\
                     The sheet with **page 1** on it goes on the outside. ",
                );
                if *booklets > 1 {
                    body += &format!(
                        "The stack makes **{booklets} booklets of {per} sheet{}**: take them off {per} at a time, \
                         fold each group in half, then stack the folded booklets in order.",
                        if *per == 1 { "" } else { "s" }
                    );
                } else {
                    body += "Fold the whole stack in half along the middle, and staple it along the fold if you like.";
                }
                if *copies > 1 {
                    body += &format!("\n\nYou printed {copies} copies: each copy comes out as its own stack.");
                }
                ui.label(marked(ui, &body, 400.0));
                ui.add_space(8.0);
                art::fold(ui, *booklets);
                ui.add_space(8.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary_button(ui, "Done").clicked() || enter {
                        chosen = Some(true);
                    }
                });
            }
            Dialog::Error(heading, message) => {
                ui.set_width(360.0);
                ui.heading(heading.as_str());
                ui.add(Label::new(message.as_str()).wrap());
                ui.add_space(8.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary_button(ui, "OK").clicked() || enter {
                        chosen = Some(true);
                    }
                });
            }
        });
        let Some(main) = chosen else { return };
        match self.dialog.take() {
            Some(Dialog::SavePreset(name)) if main => self.save_preset(&name),
            Some(Dialog::DeletePreset(name)) if main => self.delete_preset(&name),
            Some(Dialog::Reload(r)) if main => self.submit(r.files, 1, r.send),
            Some(Dialog::Fold { message, .. }) => self.outcome = Some(Outcome::Printed(message)),
            _ => {}
        }
    }

    fn draw_toast(&mut self, ctx: &egui::Context, at: egui::Pos2) {
        let Some((text, t)) = &self.toast else { return };
        let age = (ctx.input(|i| i.time) - t) as f32;
        if age > 4.0 {
            self.toast = None;
            return;
        }
        ctx.request_repaint();
        egui::Area::new(Id::new("print-toast"))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(at)
            .show(ctx, |ui| {
                ui.multiply_opacity(((4.0 - age) / 0.5).clamp(0.0, 1.0));
                theme::pill(ctx).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(ph::CHECK_CIRCLE).size(14.0).color(ACCENT));
                        ui.label(text.as_str());
                    });
                });
            });
    }
}

/// Cancel and the main action, on the right; `danger` colors the action red.
fn buttons(ui: &mut egui::Ui, action: &str, danger: bool, chosen: &mut Option<bool>) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let main = if danger {
            let red = ui.visuals().error_fg_color;
            ui.add(
                egui::Button::new(RichText::new(action).color(Color32::WHITE).strong())
                    .fill(red)
                    .min_size(vec2(0.0, 28.0)),
            )
        } else {
            primary_button(ui, action)
        };
        if main.clicked() {
            *chosen = Some(true);
        }
        if ui.add(egui::Button::new("Cancel").min_size(vec2(0.0, 28.0))).clicked() {
            *chosen = Some(false);
        }
    });
}
