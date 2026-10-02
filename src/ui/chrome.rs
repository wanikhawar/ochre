//! Window chrome: app bar, tool rail, floating properties / page / search
//! pills, status toast and the welcome screen.

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Id, Layout, Pos2, Rect, Response, RichText, Sense, Stroke,
    Vec2, pos2, vec2,
};
use egui::{Popup, PopupCloseBehavior, SetOpenCommand};
use egui_phosphor::regular as ph;

use super::color;
use super::theme::{self, ACCENT, WARN};
use crate::annot::model::{MarkupKind, ShapeKind};
use crate::app::{App, PALETTE, Selection, Tool, rgb};
use crate::doc::Doc;
use crate::viewer::Fit;

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
            .frame(egui::Frame::new().fill(p.bar).inner_margin(egui::Margin::symmetric(6, 3)))
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
                // Document name, centered in the free space between the button groups.
                if let Some(doc) = &self.doc {
                    let room = right_start - left_end - 32.0;
                    if room > 60.0 {
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&doc.name(), 0.0, egui::TextFormat::simple(FontId::proportional(14.0), p.text));
                        if doc.is_dirty() {
                            job.append("  •  Edited", 0.0, egui::TextFormat::simple(FontId::proportional(12.5), p.weak));
                        }
                        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
                        let galley = ui.painter().layout_job(job);
                        let w = galley.size().x;
                        let cx = bar.center().x.clamp(left_end + 16.0 + w / 2.0, right_start - 16.0 - w / 2.0);
                        let pos = pos2(cx - w / 2.0, bar.center().y - galley.size().y / 2.0);
                        ui.painter().galley(pos, galley, p.text);
                    }
                }
            });
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

        ui.horizontal(|ui| {
            ui.label(RichText::new(tool_icon(tool)).size(15.0).color(ACCENT));
            ui.label(RichText::new(tool.label()).strong());
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
        let fillable = matches!(tool, Tool::Shape(ShapeKind::Rect | ShapeKind::Ellipse));
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
            if let Some(label) = tool.width_label() {
                let range = match tool {
                    Tool::Text => 4.0..=144.0,
                    Tool::Highlighter => 2.0..=60.0,
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
