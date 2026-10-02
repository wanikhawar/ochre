//! Window chrome: app bar, tool rail, floating properties / page / search
//! pills, status toast and the welcome screen.

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Id, Layout, Pos2, Rect, Response, RichText, Sense, Stroke,
    Vec2, pos2, vec2,
};
use egui::{KeyboardShortcut, Modifiers, Popup, PopupCloseBehavior, SetOpenCommand};
use egui_phosphor::regular as ph;

use super::color;
use super::theme::{self, ACCENT, WARN};
use crate::annot::model::{MarkupKind, ShapeKind};
use crate::app::{App, PALETTE, Selection, Tool, rgb};
use crate::doc::Doc;
use crate::pdf::worker::Target;
use crate::viewer::{Fit, Gesture};

/// Vertical padding of the app bar; tabs extend over it to fill the bar's height.
const APP_BAR_MARGIN_Y: f32 = 3.0;

const SHAPES: [ShapeKind; 6] =
    [ShapeKind::Check, ShapeKind::Cross, ShapeKind::Rect, ShapeKind::Ellipse, ShapeKind::Line, ShapeKind::Arrow];
const MARKUPS: [MarkupKind; 3] = [MarkupKind::Highlight, MarkupKind::Underline, MarkupKind::StrikeOut];

pub fn tool_icon(tool: Tool) -> &'static str {
    match tool {
        Tool::Select => ph::CURSOR,
        Tool::Hand => ph::HAND,
        Tool::Pen => ph::PEN_NIB,
        Tool::Highlighter => ph::HIGHLIGHTER,
        Tool::Text => ph::TEXT_T,
        Tool::Shape(ShapeKind::Rect) => ph::RECTANGLE,
        Tool::Shape(ShapeKind::Ellipse) => ph::CIRCLE,
        Tool::Shape(ShapeKind::Line) => ph::LINE_SEGMENT,
        Tool::Shape(ShapeKind::Arrow) => ph::ARROW_UP_RIGHT,
        Tool::Shape(ShapeKind::Check) => ph::CHECK,
        Tool::Shape(ShapeKind::Cross) => ph::X,
        Tool::Markup(MarkupKind::Highlight) => ph::MARKER_CIRCLE,
        Tool::Markup(MarkupKind::Underline) => ph::TEXT_UNDERLINE,
        Tool::Markup(MarkupKind::StrikeOut) => ph::TEXT_STRIKETHROUGH,
        Tool::Eraser => ph::ERASER,
    }
}

/// A square, flat icon button: tinted when `active`, highlighted on hover.
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str, enabled: bool, active: bool, size: f32) -> Response {
    let p = theme::current(ui.ctx());
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), sense);
    if ui.is_rect_visible(rect) {
        let bg = if active {
            p.accent_tint
        } else if enabled && resp.is_pointer_button_down_on() {
            p.pressed
        } else if enabled && resp.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, bg);
        let color = if !enabled {
            p.weak.gamma_multiply(0.55)
        } else if active {
            ACCENT
        } else {
            p.text
        };
        painter.text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(size * 0.55), color);
    }
    resp.on_hover_text(tip)
}

pub fn primary_button(ui: &mut egui::Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE).strong())
            .fill(ACCENT)
            .corner_radius(6)
            .min_size(vec2(0.0, 28.0)),
    )
}

fn weak(ui: &egui::Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).color(theme::current(ui.ctx()).weak)
}

impl App {
    // ------------------------------------------------------------ app bar

    pub fn app_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let p = theme::current(&ctx);
        egui::Panel::top("appbar")
            .frame(egui::Frame::new().fill(p.bar).inner_margin(egui::Margin::symmetric(6, APP_BAR_MARGIN_Y as i8)))
            .show(ui, |ui| {
                let bar = ui.max_rect();
                let mut left_end = bar.left();
                let mut right_start = bar.right();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let has_doc = self.doc.is_some();
                    let dirty = self.doc.as_ref().is_some_and(Doc::is_dirty);
                    ui.label(RichText::new(ph::FILE_PDF).size(18.0).color(ACCENT));
                    ui.add_space(6.0);
                    if icon_button(ui, ph::FOLDER_OPEN, "Open… (Ctrl+O)", true, false, 28.0).clicked() {
                        self.request_open(None);
                    }
                    let recent = icon_button(ui, ph::CLOCK_COUNTER_CLOCKWISE, "Recent files", true, false, 28.0);
                    Popup::menu(&recent).width(320.0).show(|ui| self.recent_list(ui, true));
                    if icon_button(ui, ph::FLOPPY_DISK, "Save (Ctrl+S)", dirty, false, 28.0).clicked() {
                        self.save(&ctx, false);
                    }
                    if icon_button(ui, ph::FILE_ARROW_DOWN, "Save as… (Ctrl+Shift+S)", has_doc, false, 28.0).clicked() {
                        self.save(&ctx, true);
                    }
                    ui.add_space(4.0);
                    ui.separator();
                    let shown = has_doc && self.cfg.show_outline;
                    if icon_button(ui, ph::SIDEBAR_SIMPLE, "Contents (F9)", has_doc, shown, 28.0).clicked() {
                        self.cfg.show_outline = !self.cfg.show_outline;
                    }
                    left_end = ui.min_rect().right();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (can_undo, can_redo) =
                            self.doc.as_ref().map(|d| (d.can_undo(), d.can_redo())).unwrap_or_default();
                        if icon_button(ui, ph::MAGNIFYING_GLASS, "Search (/ or Ctrl+F)", has_doc, self.search.open, 28.0).clicked() {
                            if self.search.open {
                                self.close_search();
                            } else {
                                self.open_search(false);
                            }
                        }
                        ui.add_space(4.0);
                        ui.separator();
                        if icon_button(ui, ph::ARROW_U_UP_RIGHT, "Redo (Ctrl+Shift+Z)", can_redo, false, 28.0).clicked() {
                            self.redo();
                        }
                        if icon_button(ui, ph::ARROW_U_UP_LEFT, "Undo (Ctrl+Z)", can_undo, false, 28.0).clicked() {
                            self.undo();
                        }
                        right_start = ui.min_rect().left();
                    });
                });
                // Tabs fill the space between the button groups, top to bottom of the bar
                // (past the frame's margin).
                if !self.tabs.is_empty() {
                    let full = bar.expand2(vec2(0.0, APP_BAR_MARGIN_Y));
                    let strip = Rect::from_x_y_ranges(left_end + 10.0..=right_start - 10.0, full.y_range());
                    if strip.width() > 60.0 {
                        self.tab_strip(ui, strip);
                    }
                }
            });
    }

    /// One flat tab per open file, filling `strip`: click to switch, × or
    /// middle-click to close. The active tab is lighter, with a glowing accent bar
    /// along its top; a dot marks unsaved changes. Scrolls with the wheel when
    /// the tabs don't fit.
    fn tab_strip(&mut self, ui: &mut egui::Ui, strip: Rect) {
        let p = theme::current(ui.ctx());
        let n = self.tabs.len();
        let width = (strip.width() / n as f32).clamp(120.0, 220.0);
        let overflow = (width * n as f32 - strip.width()).max(0.0);
        if self.reveal_tab {
            // Bring the active tab into view.
            let (l, r) = (self.active as f32 * width, (self.active + 1) as f32 * width);
            self.tab_scroll = self.tab_scroll.min(l).max(r - strip.width());
            self.reveal_tab = false;
        }
        if ui.rect_contains_pointer(strip) {
            let d = ui.input(|i| i.smooth_scroll_delta);
            self.tab_scroll -= if d.x != 0.0 { d.x } else { d.y };
        }
        self.tab_scroll = self.tab_scroll.clamp(0.0, overflow);

        let painter = ui.painter().with_clip_rect(strip);
        let mut switch_to = None;
        let mut close = None;
        for i in 0..n {
            let Some(doc) = self.tab_doc(i) else { continue };
            let (name, dirty, path) = (doc.name(), doc.is_dirty(), doc.path.display().to_string());
            let active = i == self.active;
            let rect = Rect::from_min_size(
                pos2(strip.left() + i as f32 * width - self.tab_scroll, strip.top()),
                vec2(width, strip.height()),
            );
            let visible = rect.intersect(strip);
            if !visible.is_positive() {
                continue;
            }
            let resp = ui.interact(visible, Id::new(("tab", i)), Sense::click());
            let hovered = resp.hovered();
            let close_rect = Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), Vec2::splat(20.0));
            let on_close = hovered && resp.hover_pos().is_some_and(|q| close_rect.contains(q));

            if active {
                painter.rect_filled(rect, 0.0, p.hover);
                // Accent bar with a soft glow fading down into the tab.
                let bar = Rect::from_min_size(rect.min, vec2(rect.width(), 2.5));
                let glow = Rect::from_min_max(pos2(rect.left(), bar.bottom()), pos2(rect.right(), bar.bottom() + 10.0));
                let mut mesh = egui::Mesh::default();
                let (top, bottom) = (ACCENT.gamma_multiply(0.18), Color32::TRANSPARENT);
                let base = mesh.vertices.len() as u32;
                for (pos, color) in [
                    (glow.left_top(), top),
                    (glow.right_top(), top),
                    (glow.right_bottom(), bottom),
                    (glow.left_bottom(), bottom),
                ] {
                    mesh.colored_vertex(pos, color);
                }
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base, base + 2, base + 3);
                painter.add(mesh);
                painter.rect_filled(bar, 0.0, ACCENT);
            } else {
                if hovered {
                    painter.rect_filled(rect, 0.0, p.hover.gamma_multiply(0.6));
                }
                // Divider between two inactive tabs.
                if i + 1 < n && i + 1 != self.active {
                    let x = rect.right() - 0.5;
                    painter.vline(x, rect.y_range().shrink(9.0), Stroke::new(1.0, p.border));
                }
            }

            let text_color = if active { p.text } else { p.weak };
            let clip = Rect::from_min_max(pos2(rect.left() + 12.0, rect.top()), pos2(close_rect.left() - 2.0, rect.bottom()));
            let mut job = egui::text::LayoutJob::simple_singleline(name, FontId::proportional(13.0), text_color);
            job.wrap = egui::text::TextWrapping::truncate_at_width(clip.width());
            let galley = painter.layout_job(job);
            let at = pos2(clip.left(), rect.center().y - galley.size().y / 2.0);
            painter.with_clip_rect(clip.intersect(strip)).galley(at, galley, text_color);
            // The close button; an unsaved tab shows a dot there until hovered.
            if on_close {
                painter.rect_filled(close_rect, 4.0, p.pressed);
            }
            if hovered || (active && !dirty) {
                painter.text(close_rect.center(), Align2::CENTER_CENTER, ph::X, FontId::proportional(12.0), p.weak);
            } else if dirty {
                painter.circle_filled(close_rect.center(), 3.5, if active { ACCENT } else { p.weak });
            }
            let tip = if dirty { format!("{path}\nUnsaved changes") } else { path };
            let resp = resp.on_hover_text(tip);
            if resp.middle_clicked() || (resp.clicked() && on_close) {
                close = Some(i);
            } else if resp.clicked() {
                switch_to = Some(i);
            }
        }
        // Fade the edges when tabs are scrolled out of view.
        for (edge, shown) in [(strip.left(), self.tab_scroll > 0.5), (strip.right(), self.tab_scroll < overflow - 0.5)] {
            if shown && overflow > 0.0 {
                let dir = if edge == strip.left() { 1.0 } else { -1.0 };
                let fade = Rect::from_two_pos(pos2(edge, strip.top()), pos2(edge + dir * 16.0, strip.bottom()));
                let mut mesh = egui::Mesh::default();
                let (solid, clear) = (p.bar, p.bar.gamma_multiply(0.0));
                let (l, r) = if dir > 0.0 { (solid, clear) } else { (clear, solid) };
                for (pos, color) in [(fade.left_top(), l), (fade.right_top(), r), (fade.right_bottom(), r), (fade.left_bottom(), l)] {
                    mesh.colored_vertex(pos, color);
                }
                mesh.add_triangle(0, 1, 2);
                mesh.add_triangle(0, 2, 3);
                painter.add(mesh);
            }
        }
        if let Some(i) = close {
            self.close_tab(i);
        } else if let Some(i) = switch_to {
            self.switch_tab(i);
        }
    }

    fn recent_list(&mut self, ui: &mut egui::Ui, menu: bool) {
        if self.cfg.recent.is_empty() {
            ui.label(weak(ui, "No recent files"));
            return;
        }
        for path in self.cfg.recent.clone() {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = path.parent().map(|d| d.display().to_string()).unwrap_or_default();
            let p = theme::current(ui.ctx());
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(rect, 6.0, p.hover);
            }
            let painter = ui.painter();
            painter.text(
                rect.left_center() + vec2(10.0, 0.0),
                Align2::LEFT_CENTER,
                ph::FILE_PDF,
                FontId::proportional(20.0),
                ACCENT,
            );
            let text_x = rect.left() + 40.0;
            let clip = Rect::from_min_max(pos2(text_x, rect.top()), rect.right_bottom() - vec2(8.0, 0.0));
            let painter = painter.with_clip_rect(clip);
            painter.text(pos2(text_x, rect.top() + 7.0), Align2::LEFT_TOP, name, FontId::proportional(13.5), p.text);
            painter.text(pos2(text_x, rect.bottom() - 6.0), Align2::LEFT_BOTTOM, dir, FontId::proportional(11.0), p.weak);
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.request_open(Some(path));
                if menu {
                    ui.close();
                }
            }
        }
    }

    // ------------------------------------------------------------ tool rail

    pub fn tool_rail(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        egui::Panel::left("tools")
            .resizable(false)
            .exact_size(42.0)
            .frame(egui::Frame::new().fill(p.bar).inner_margin(egui::Margin::symmetric(6, 8)))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    self.rail_tool(ui, Tool::Select, &[]);
                    self.rail_tool(ui, Tool::Hand, &[]);
                    rail_separator(ui);
                    self.rail_tool(ui, Tool::Pen, &[]);
                    self.rail_tool(ui, Tool::Highlighter, &[]);
                    self.rail_tool(ui, Tool::Text, &[]);
                    self.rail_tool(ui, Tool::Shape(self.last_shape), &SHAPES.map(Tool::Shape));
                    self.rail_tool(ui, Tool::Markup(self.last_markup), &MARKUPS.map(Tool::Markup));
                    rail_separator(ui);
                    self.rail_tool(ui, Tool::Eraser, &[]);
                    self.settings_button(ui);
                });
            });
    }

    /// A rail button. Tools with `variants` show a corner caret; clicking the caret,
    /// right-clicking, or clicking the already-active tool opens a flyout.
    fn rail_tool(&mut self, ui: &mut egui::Ui, tool: Tool, variants: &[Tool]) {
        let active = std::mem::discriminant(&self.tool) == std::mem::discriminant(&tool);
        let tip = if variants.is_empty() {
            format!("{}  ({})", tool.label(), tool.shortcut())
        } else {
            format!("{}  ({})\nClick again or right-click for more", tool.label(), tool.shortcut())
        };
        let resp = icon_button(ui, tool_icon(tool), &tip, true, active, 30.0);
        if variants.is_empty() {
            if resp.clicked() {
                self.set_tool(tool);
            }
            return;
        }
        let r = resp.rect;
        let p = theme::current(ui.ctx());
        ui.painter().text(
            r.right_bottom() - vec2(3.0, 2.0),
            Align2::RIGHT_BOTTOM,
            ph::CARET_DOWN,
            FontId::proportional(8.0),
            if active { ACCENT } else { p.weak },
        );
        let corner = Rect::from_min_max(r.right_bottom() - vec2(13.0, 13.0), r.right_bottom());
        let on_caret = resp.clicked() && resp.interact_pointer_pos().is_some_and(|pos| corner.contains(pos));
        let toggle = on_caret || resp.secondary_clicked() || (resp.clicked() && active);
        if resp.clicked() && !active {
            self.set_tool(tool);
        }
        Popup::from_response(&resp)
            .open_memory(toggle.then_some(SetOpenCommand::Toggle))
            .close_behavior(PopupCloseBehavior::CloseOnClick)
            .align(egui::RectAlign::RIGHT_START)
            .gap(8.0)
            .show(|ui| {
                ui.set_min_width(170.0);
                for &v in variants {
                    let text = format!("{}   {}", tool_icon(v), v.label());
                    if ui.selectable_label(self.tool == v, text).clicked() {
                        self.set_tool(v);
                    }
                }
            });
    }

    // ------------------------------------------------------------ contents

    /// Sidebar with the document's table of contents. The entry for the part being
    /// read is highlighted; top-level entries start expanded.
    pub fn outline_panel(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        let ctx = ui.ctx().clone();
        egui::Panel::left("outline")
            .resizable(true)
            .default_size(260.0)
            .size_range(160.0..=520.0)
            .frame(egui::Frame::new().fill(p.bar).inner_margin(egui::Margin::symmetric(6, 8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("Contents").strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, ph::X, "Close (F9)", true, false, 22.0).clicked() {
                            self.cfg.show_outline = false;
                        }
                    });
                });
                ui.add_space(4.0);
                let Some(doc) = &self.doc else { return };
                if doc.outline.is_empty() {
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| ui.label(weak(ui, "This PDF has no table of contents.")));
                    return;
                }
                let items = doc.outline.clone();
                let current = self.current_outline_item();
                let mut clicked: Option<usize> = None;
                let mut toggled: Option<usize> = None;
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    // Entries below a collapsed one are skipped until the level comes back up.
                    let mut hidden_below: Option<usize> = None;
                    for (i, item) in items.iter().enumerate() {
                        if hidden_below.is_some_and(|l| item.level > l) {
                            continue;
                        }
                        hidden_below = None;
                        let has_children = items.get(i + 1).is_some_and(|n| n.level > item.level);
                        let open = has_children && ((item.level == 0) != self.outline_toggled.contains(&i));
                        if has_children && !open {
                            hidden_below = Some(item.level);
                        }
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
                        // A collapsed entry stands in for the current entry hidden inside it.
                        let contains_current = current.is_some_and(|c| {
                            c > i && items[i + 1..=c].iter().all(|n| n.level > item.level)
                        });
                        let active = current == Some(i) || (has_children && !open && contains_current);
                        let bg = if active {
                            p.accent_tint
                        } else if resp.hovered() {
                            p.hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        ui.painter().rect_filled(rect, 5.0, bg);
                        let x = rect.left() + 4.0 + item.level as f32 * 14.0;
                        let caret = Rect::from_min_size(pos2(x, rect.top()), vec2(16.0, rect.height()));
                        if has_children {
                            let icon = if open { ph::CARET_DOWN } else { ph::CARET_RIGHT };
                            ui.painter().text(caret.center(), Align2::CENTER_CENTER, icon, FontId::proportional(11.0), p.weak);
                        }
                        let text_x = caret.right() + 2.0;
                        let page = match &item.target {
                            Some(Target::Page { page, .. }) => format!("{}", page + 1),
                            _ => String::new(),
                        };
                        let painter = ui.painter();
                        let page_w = painter
                            .text(rect.right_center() - vec2(6.0, 0.0), Align2::RIGHT_CENTER, &page, FontId::proportional(11.5), p.weak)
                            .width();
                        let title_clip = Rect::from_min_max(pos2(text_x, rect.top()), pos2(rect.right() - page_w - 12.0, rect.bottom()));
                        let color = if active { ACCENT } else { p.text };
                        let galley = painter.layout_no_wrap(item.title.clone(), FontId::proportional(13.0), color);
                        let truncated = galley.size().x > title_clip.width();
                        painter.with_clip_rect(title_clip).galley(
                            pos2(text_x, rect.center().y - galley.size().y / 2.0),
                            galley,
                            color,
                        );
                        let resp = if truncated { resp.on_hover_text(&item.title) } else { resp };
                        if resp.clicked() {
                            let on_caret = resp.interact_pointer_pos().is_some_and(|q| caret.contains(q));
                            if has_children && (on_caret || item.target.is_none()) {
                                toggled = Some(i);
                            } else {
                                clicked = Some(i);
                            }
                        }
                    }
                });
                if let Some(i) = toggled
                    && !self.outline_toggled.remove(&i)
                {
                    self.outline_toggled.insert(i);
                }
                if let Some(t) = clicked.and_then(|i| items[i].target.clone()) {
                    self.follow(&ctx, &t);
                }
            });
    }

    /// The outline entry for the part being read: the last one starting at or above
    /// the top of the window.
    pub(crate) fn current_outline_item(&self) -> Option<usize> {
        let doc = self.doc.as_ref()?;
        let (page, y) = self.view.position()?;
        let here = (page, y + 40.0 / self.view.zoom);
        doc.outline
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match &item.target {
                Some(Target::Page { page, x, y }) => {
                    let g = doc.pages.get(*page)?;
                    let top = y.map_or(0.0, |y| g.to_display().apply(crate::annot::model::Pt::new(x.unwrap_or(g.bbox[0]), y)).y);
                    Some((i, (*page, top)))
                }
                _ => None,
            })
            .filter(|(_, at)| at.0 < here.0 || (at.0 == here.0 && at.1 <= here.1))
            .max_by(|a, b| a.1.0.cmp(&b.1.0).then(a.1.1.total_cmp(&b.1.1)).then(a.0.cmp(&b.0)))
            .map(|(i, _)| i)
    }

    // ------------------------------------------------------------ floating UI

    /// Pills and toast drawn over the page canvas.
    pub fn canvas_overlays(&mut self, ctx: &egui::Context, canvas: Rect) {
        let narrow = canvas.width() < 980.0;
        if self.properties_visible() {
            self.pill(ctx, "props", canvas, Align2::CENTER_TOP, pos2(canvas.center().x, canvas.top() + 8.0), |app, ui| {
                app.properties(ui);
            });
        }
        if self.search.open {
            let y = canvas.top() + if narrow && self.properties_visible() { 46.0 } else { 8.0 };
            self.pill(ctx, "search", canvas, Align2::RIGHT_TOP, pos2(canvas.right() - 10.0, y), |app, ui| {
                app.search_bar(ui);
            });
        }
        if self.doc.as_ref().is_some_and(|d| !d.pages.is_empty()) {
            self.pill(ctx, "nav", canvas, Align2::RIGHT_BOTTOM, pos2(canvas.right() - 10.0, canvas.bottom() - 10.0), |app, ui| {
                app.nav(ui);
            });
        }
        if let Some(at) = self.view.sel_anchor.filter(|p| canvas.contains(*p)) {
            self.pill(ctx, "text-sel", canvas, Align2::CENTER_BOTTOM, at, |app, ui| {
                if icon_button(ui, ph::COPY, "Copy (Ctrl+C)", true, false, 24.0).clicked() {
                    app.copy_selection(ui.ctx());
                }
                ui.separator();
                app.markup_color_button(ui);
                for (m, label) in [
                    (MarkupKind::Highlight, "Highlight"),
                    (MarkupKind::Underline, "Underline"),
                    (MarkupKind::StrikeOut, "Strike out"),
                ] {
                    let resp = icon_button(ui, "", label, true, false, 24.0);
                    // Icon drawn in the color it will use.
                    let c = rgb(app.tool_style(Tool::Markup(m)).color);
                    ui.painter().text(resp.rect.center(), Align2::CENTER_CENTER, tool_icon(Tool::Markup(m)), FontId::proportional(13.0), c);
                    if resp.clicked() {
                        app.markup_selection(m);
                    }
                }
            });
        }
        if let Some(r) = self.view.annot_rect.filter(|r| canvas.intersects(*r)) {
            if self.note_edit.is_some() {
                self.note_editor(ctx, canvas, r);
            } else if matches!(self.gesture, Gesture::None) && self.editing.is_none() {
                // Above the selection, clear of its handles.
                let at = pos2(r.center().x, r.top() - 10.0);
                self.pill(ctx, "annot-actions", canvas, Align2::CENTER_BOTTOM, at, |app, ui| app.annot_actions(ui));
            }
        }
        let toast_y = canvas.bottom() - if narrow { 52.0 } else { 14.0 };
        self.toast(ctx, pos2(canvas.center().x, toast_y));
    }

    #[allow(clippy::too_many_arguments)]
    fn pill(
        &mut self,
        ctx: &egui::Context,
        id: &str,
        canvas: Rect,
        pivot: Align2,
        pos: Pos2,
        content: impl FnOnce(&mut Self, &mut egui::Ui),
    ) {
        egui::Area::new(Id::new(id))
            .order(egui::Order::Middle)
            .pivot(pivot)
            .fixed_pos(pos)
            .constrain_to(canvas.shrink(8.0))
            .show(ctx, |ui| {
                theme::pill(ctx).show(ui, |ui| {
                    // Wrap onto a second row rather than overflow a narrow window.
                    ui.set_max_width(canvas.width() - 40.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        content(self, ui);
                    });
                });
            });
    }

    /// Action bar of our selected annotation: note and delete.
    fn annot_actions(&mut self, ui: &mut egui::Ui) {
        if let Some(Selection::Many(ids)) = &self.selection {
            ui.label(RichText::new(format!("{} annotations", ids.len())).size(13.0));
            ui.separator();
            if icon_button(ui, ph::TRASH, "Delete all (Del)", true, false, 24.0).clicked() {
                self.delete_selection();
            }
            return;
        }
        let Some(Selection::Ours(id)) = self.selection.clone() else { return };
        let Some(a) = self.doc.as_ref().and_then(|d| d.get(&id)) else { return };
        if a.takes_note() {
            let label = if a.note.is_empty() { "Add note" } else { "Edit note" };
            let text = RichText::new(format!("{}  {label}", ph::CHAT_TEXT)).size(13.0);
            if ui.add(egui::Button::new(text).frame(false).min_size(vec2(0.0, 24.0))).on_hover_text("Enter").clicked() {
                self.open_note(&id);
            }
            ui.separator();
        }
        if icon_button(ui, ph::TRASH, "Delete (Del)", true, false, 24.0).clicked() {
            self.delete_selection();
        }
    }

    /// Editor for the selected annotation's note, under the annotation.
    fn note_editor(&mut self, ctx: &egui::Context, canvas: Rect, r: Rect) {
        let Some(n) = &mut self.note_edit else { return };
        let mut done = false;
        let mut remove = false;
        let below = r.bottom() + 220.0 < canvas.bottom();
        let (pivot, at) = if below {
            (Align2::CENTER_TOP, pos2(r.center().x, r.bottom() + 10.0))
        } else {
            (Align2::CENTER_BOTTOM, pos2(r.center().x, r.top() - 10.0))
        };
        egui::Area::new(Id::new("note-editor"))
            .order(egui::Order::Foreground)
            .pivot(pivot)
            .fixed_pos(at)
            .constrain_to(canvas.shrink(8.0))
            .show(ctx, |ui| {
                theme::pill(ctx).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                    ui.set_width(280.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(ph::CHAT_TEXT).color(ACCENT));
                        ui.label(RichText::new("Note").strong());
                    });
                    ui.add_space(4.0);
                    let id = Id::new("note-edit");
                    if n.focus {
                        // Repeated until it sticks (see the text box editor).
                        ui.memory_mut(|m| m.request_focus(id));
                    }
                    let edit = egui::TextEdit::multiline(&mut n.text)
                        .id(id)
                        .hint_text("Write a note…")
                        .desired_width(f32::INFINITY)
                        .desired_rows(3);
                    let resp = ui.add(edit);
                    if resp.has_focus() && !ui.input(|i| i.pointer.any_down() || i.pointer.any_released()) {
                        n.focus = false;
                    }
                    let submit = KeyboardShortcut::new(Modifiers::COMMAND, egui::Key::Enter);
                    if resp.has_focus() && ui.input_mut(|i| i.consume_shortcut(&submit)) {
                        done = true;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        done = true;
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if primary_button(ui, "Done").on_hover_text("Ctrl+Enter").clicked() {
                            done = true;
                        }
                        if !n.text.trim().is_empty() && ui.button("Remove note").clicked() {
                            remove = true;
                        }
                    });
                });
            });
        if remove && let Some(n) = &mut self.note_edit {
            n.text.clear();
            done = true;
        }
        if done {
            self.commit_note();
        }
    }

    /// Only annotations from other apps get a floating note; tool settings live in
    /// the rail's settings popup.
    fn properties_visible(&self) -> bool {
        self.doc.is_some() && matches!(self.selection, Some(Selection::Foreign(_)))
    }

    fn properties(&mut self, ui: &mut egui::Ui) {
        if let (Some(Selection::Foreign(i)), Some(doc)) = (&self.selection, &self.doc) {
            let subtype = doc.foreign.get(*i).map(|f| f.subtype.clone()).unwrap_or_default();
            ui.label(RichText::new(ph::INFO).size(14.0).color(WARN));
            ui.label(format!("{subtype} annotation from another app"));
            ui.label(weak(ui, "· read-only · Delete removes it"));
        }
    }

    /// Color button in the text selection bar: sets the color used by highlight,
    /// underline and strike-out (each keeps its own opacity).
    fn markup_color_button(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        let current = self.tool_style(Tool::Markup(MarkupKind::Highlight)).color;
        let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 24.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 6.0, p.hover);
        }
        let c = pos2(rect.left() + 11.0, rect.center().y);
        ui.painter().circle_filled(c, 7.0, rgb(current));
        ui.painter().circle_stroke(c, 7.5, Stroke::new(1.0, p.weak));
        ui.painter().text(rect.right_center() - vec2(6.0, 0.0), Align2::CENTER_CENTER, ph::CARET_DOWN, FontId::proportional(9.0), p.weak);
        let resp = resp.on_hover_text("Markup color").on_hover_cursor(egui::CursorIcon::PointingHand);
        Popup::menu(&resp).close_behavior(PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            ui.set_width(236.0);
            let mut color = current;
            let (_, custom_changed) = color::palette(ui, &mut color, &PALETTE, &mut self.cfg.palette);
            ui.add_space(6.0);
            color::picker(ui, &mut color);
            if custom_changed {
                self.cfg.save();
            }
            if color != current {
                for m in MARKUPS {
                    let mut s = self.tool_style(Tool::Markup(m));
                    s.color = color;
                    self.cfg.styles.insert(Tool::Markup(m).key(), s);
                }
            }
        });
    }

    /// Rail button showing the current color; opens the settings popup for the
    /// active tool (or the selected annotation).
    fn settings_button(&mut self, ui: &mut egui::Ui) {
        let (style, sel_tool) = self.shown_style();
        let tool = sel_tool.unwrap_or(self.tool);
        if !tool.has_color() && tool.width_label().is_none() {
            return;
        }
        rail_separator(ui);
        let p = theme::current(ui.ctx());
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
        let open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&resp));
        let bg = if open {
            p.accent_tint
        } else if resp.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, bg);
        if tool.has_color() {
            painter.circle_filled(rect.center(), 8.0, rgb(style.color));
            painter.circle_stroke(rect.center(), 8.5, Stroke::new(1.0, p.weak));
        } else {
            painter.text(rect.center(), Align2::CENTER_CENTER, ph::SLIDERS_HORIZONTAL, FontId::proportional(16.0), p.text);
        }
        let what = if sel_tool.is_some() { "selected annotation" } else { tool.label() };
        let resp = resp.on_hover_text(format!("Settings: {what}"));
        Popup::menu(&resp)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .align(egui::RectAlign::RIGHT_END)
            .gap(8.0)
            .show(|ui| {
                ui.set_width(236.0);
                self.settings_panel(ui);
            });
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        let (mut style, sel_tool) = self.shown_style();
        let tool = sel_tool.unwrap_or(self.tool);
        let original = style;
        let group = self.group_style();

        ui.horizontal(|ui| {
            match &group {
                Some(g) => {
                    ui.label(RichText::new(ph::SELECTION_ALL).size(15.0).color(ACCENT));
                    ui.label(RichText::new(format!("{} annotations", g.count)).strong());
                }
                None => {
                    ui.label(RichText::new(tool_icon(tool)).size(15.0).color(ACCENT));
                    ui.label(RichText::new(tool.label()).strong());
                }
            }
            if sel_tool.is_some() {
                ui.label(weak(ui, "· selected").small());
            }
        });
        ui.add_space(4.0);
        if tool.has_color() {
            let (_, custom_changed) = color::palette(ui, &mut style.color, &PALETTE, &mut self.cfg.palette);
            if custom_changed {
                self.cfg.save();
            }
            ui.add_space(6.0);
            color::picker(ui, &mut style.color);
            ui.add_space(4.0);
            ui.separator();
        }
        let fillable = group.as_ref().map_or(matches!(tool, Tool::Shape(ShapeKind::Rect | ShapeKind::Ellipse)), |g| g.fillable);
        let width_label = group.as_ref().map_or(tool.width_label(), |g| g.width_label);
        let text_size = tool == Tool::Text || group.as_ref().is_some_and(|g| g.text_size);
        if fillable {
            ui.label(weak(ui, "Fill"));
            color::fill_presets(ui, &mut style.fill, style.color, &PALETTE);
            ui.add_space(2.0);
            ui.separator();
        }
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        // Number fields get a visible field background so it's clear you can type in them.
        let p = theme::current(ui.ctx());
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.weak_bg_fill = p.field;
        w.inactive.bg_stroke = Stroke::new(1.0, p.border);
        w.hovered.bg_stroke = Stroke::new(1.0, p.weak);
        egui::Grid::new("tool-settings").num_columns(3).spacing(vec2(8.0, 6.0)).show(ui, |ui| {
            if let Some(label) = width_label {
                let range = match tool {
                    _ if text_size => 4.0..=144.0,
                    Tool::Highlighter if group.is_none() => 2.0..=60.0,
                    Tool::Eraser => 4.0..=80.0,
                    _ => 0.1..=40.0,
                };
                setting_row(ui, label, &mut style.width, range, " pt", 2, true);
            }
            if tool.has_color() {
                let mut pct = style.opacity * 100.0;
                let label = if fillable { "Outline opacity" } else { "Opacity" };
                setting_row(ui, label, &mut pct, 5.0..=100.0, " %", 0, false);
                style.opacity = pct / 100.0;
            }
            if fillable && style.fill.is_some() {
                let mut pct = style.fill_opacity * 100.0;
                setting_row(ui, "Fill opacity", &mut pct, 5.0..=100.0, " %", 0, false);
                style.fill_opacity = pct / 100.0;
            }
            if sel_tool.is_none() && matches!(tool, Tool::Pen | Tool::Highlighter) {
                let mut level = stabilizer_level(self.cfg.stabilizer);
                setting_row(ui, "Smoothing", &mut level, 1.0..=10.0, "", 0, false);
                self.cfg.stabilizer = stabilizer_radius(level);
            }
        });
        if style != original {
            self.apply_style(style);
        }
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        let Some(n) = self.doc.as_ref().map(|d| d.pages.len()) else { return };
        let cur = self.view.current_page + 1;
        if let Some(&(page, _)) = self.view.back.last() {
            let tip = format!("Back to page {} (Alt+←)", page + 1);
            if icon_button(ui, ph::ARROW_BEND_UP_LEFT, &tip, true, false, 22.0).clicked() {
                self.go_back();
            }
            ui.separator();
        }
        if icon_button(ui, ph::CARET_LEFT, "Previous page", cur > 1, false, 22.0).clicked() {
            self.view.goto_page = Some(cur - 2);
        }
        let id = Id::new("page-field");
        if !ui.memory(|m| m.has_focus(id)) {
            self.page_input = cur.to_string();
        }
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.page_input)
                .id(id)
                .desired_width(30.0)
                .horizontal_align(Align::Center),
        );
        if r.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && let Ok(page) = self.page_input.trim().parse::<usize>()
        {
            self.view.goto_page = Some(page.clamp(1, n) - 1);
        }
        ui.label(weak(ui, format!("/ {n}")));
        if icon_button(ui, ph::CARET_RIGHT, "Next page", cur < n, false, 22.0).clicked() {
            self.view.goto_page = Some(cur);
        }
        ui.separator();
        if icon_button(ui, ph::MINUS, "Zoom out (Ctrl+−)", true, false, 22.0).clicked() {
            self.view.zoom_by(0.8);
        }
        ui.add_sized(vec2(40.0, 18.0), egui::Label::new(format!("{:.0}%", self.view.zoom * 100.0)));
        if icon_button(ui, ph::PLUS, "Zoom in (Ctrl++)", true, false, 22.0).clicked() {
            self.view.zoom_by(1.25);
        }
        ui.separator();
        let fit = self.view.fit;
        if icon_button(ui, ph::ARROWS_OUT_LINE_HORIZONTAL, "Fit width (Ctrl+0)", true, fit == Some(Fit::Width), 22.0)
            .clicked()
        {
            self.view.fit = Some(Fit::Width);
        }
        if icon_button(ui, ph::CORNERS_OUT, "Fit page", true, fit == Some(Fit::Page), 22.0).clicked() {
            self.view.fit = Some(Fit::Page);
        }
    }

    /// Transient status message, plus a persistent notice for read-only files.
    fn toast(&mut self, ctx: &egui::Context, pos: Pos2) {
        let now = ctx.input(|i| i.time);
        let mut items: Vec<(&str, Color32, String, f32)> = Vec::new();
        if let Some(why) = self.doc.as_ref().and_then(|d| d.read_only.clone()) {
            items.push((ph::WARNING_CIRCLE, WARN, why, 1.0));
        }
        if let Some((msg, t, err)) = &self.status {
            let age = (now - t) as f32;
            if age < 4.0 {
                let (icon, color) = if *err {
                    (ph::WARNING_CIRCLE, ctx.global_style().visuals.error_fg_color)
                } else {
                    (ph::CHECK_CIRCLE, ACCENT)
                };
                items.push((icon, color, msg.clone(), ((4.0 - age) / 0.5).clamp(0.0, 1.0)));
                if age > 3.5 {
                    ctx.request_repaint(); // fading out
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f32(3.5 - age));
                }
            }
        }
        for (i, (icon, color, msg, alpha)) in items.into_iter().enumerate() {
            egui::Area::new(Id::new(("toast", i)))
                .order(egui::Order::Tooltip)
                .interactable(false)
                .pivot(Align2::CENTER_BOTTOM)
                .fixed_pos(pos - vec2(0.0, i as f32 * 44.0))
                .show(ctx, |ui| {
                    ui.multiply_opacity(alpha);
                    theme::pill(ctx).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon).size(14.0).color(color));
                            ui.label(msg);
                        });
                    });
                });
        }
    }

    // ------------------------------------------------------------ welcome

    pub fn welcome(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let p = theme::current(&ctx);
        let width = 440.0f32.min(ui.available_width() - 32.0);
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.16).max(16.0));
            egui::Frame::new()
                .fill(p.bar)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(14)
                .shadow(theme::soft_shadow(ui.visuals().dark_mode))
                .inner_margin(egui::Margin::same(28))
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(ph::FILE_PDF).size(52.0).color(ACCENT));
                        ui.add_space(4.0);
                        ui.label(RichText::new("Ochre").size(24.0).strong());
                        ui.label(weak(ui, "Read and annotate PDFs"));
                        ui.add_space(14.0);
                        for err in [&self.worker_error, &self.load_error].into_iter().flatten() {
                            ui.label(
                                RichText::new(format!("{}  {err}", ph::WARNING_CIRCLE))
                                    .color(ui.visuals().error_fg_color),
                            );
                            ui.add_space(10.0);
                        }
                        if self.worker.is_some() {
                            if primary_button(ui, &format!("{}  Open file…", ph::FOLDER_OPEN)).clicked() {
                                self.request_open(None);
                            }
                            ui.add_space(6.0);
                            ui.label(weak(ui, "or drop a PDF here  ·  Ctrl+O").small());
                        }
                    });
                    if self.worker.is_some() && !self.cfg.recent.is_empty() {
                        ui.add_space(18.0);
                        ui.separator();
                        ui.add_space(6.0);
                        ui.label(weak(ui, "RECENT").small().strong());
                        ui.add_space(2.0);
                        self.recent_list(ui, false);
                    }
                });
        });
    }
}

/// A grid row: label, slider, and a number field you can drag or type into.
fn setting_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
    decimals: usize,
    log: bool,
) {
    ui.label(weak(ui, label));
    let slider = egui::Slider::new(value, range.clone()).show_value(false).logarithmic(log);
    ui.add_sized(vec2(96.0, 18.0), slider);
    let speed = if decimals == 0 { 0.2 } else { 0.05 };
    ui.add_sized(
        vec2(64.0, 20.0),
        egui::DragValue::new(value).range(range).speed(speed).max_decimals(decimals).suffix(suffix),
    )
    .on_hover_text("Drag, or click and type a value");
    ui.end_row();
}

/// Smoothing levels 1–10. Level 1 is a 3 px stabilizer; each level adds 1.5 px.
pub const MIN_STABILIZER: f32 = 3.0;
const STABILIZER_STEP: f32 = 1.5;

fn stabilizer_level(radius: f32) -> f32 {
    (((radius - MIN_STABILIZER) / STABILIZER_STEP).round() + 1.0).clamp(1.0, 10.0)
}

fn stabilizer_radius(level: f32) -> f32 {
    MIN_STABILIZER + (level.round().clamp(1.0, 10.0) - 1.0) * STABILIZER_STEP
}

fn rail_separator(ui: &mut egui::Ui) {
    let p = theme::current(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(22.0, 7.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.border));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_levels_start_at_three_pixels() {
        assert_eq!(stabilizer_radius(1.0), 3.0);
        assert_eq!(stabilizer_level(3.0), 1.0);
        // Older, weaker settings show as the lowest level.
        assert_eq!(stabilizer_level(2.5), 1.0);
        for level in 1..=10 {
            assert_eq!(stabilizer_level(stabilizer_radius(level as f32)), level as f32);
        }
        assert_eq!(stabilizer_radius(10.0), 16.5);
    }
}
