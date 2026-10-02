//! Compact color picker: palette swatches, a saturation/value square, a hue
//! bar and a hex field.

use eframe::egui::{
    self, Color32, Id, Mesh, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, ecolor::Hsva, pos2, vec2,
};

use super::theme::{self, ACCENT};
use crate::app::rgb;

const SWATCH: f32 = 26.0;

pub fn to_rgb(c: Color32) -> [f32; 3] {
    [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0]
}

fn same(a: &[f32; 3], b: &[f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.004)
}

/// One round swatch; returns its response.
fn swatch(ui: &mut egui::Ui, c: [f32; 3], selected: bool) -> egui::Response {
    let p = theme::current(ui.ctx());
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(SWATCH), Sense::click());
    let painter = ui.painter();
    if selected {
        painter.circle_stroke(r.center(), 11.5, Stroke::new(2.0, ACCENT));
    } else if resp.hovered() {
        painter.circle_stroke(r.center(), 11.5, Stroke::new(2.0, p.border));
    }
    painter.circle_filled(r.center(), 8.5, rgb(c));
    painter.circle_stroke(r.center(), 8.5, Stroke::new(1.0, p.weak.gamma_multiply(0.6)));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Palette grid. Custom colors can be removed with a right-click; the last
/// cell saves the current color. Returns true if `color` changed.
pub fn palette(ui: &mut egui::Ui, color: &mut [f32; 3], builtin: &[[f32; 3]], custom: &mut Vec<[f32; 3]>) -> (bool, bool) {
    let mut changed = false;
    let mut custom_changed = false;
    let cols = 7;
    let all: Vec<([f32; 3], Option<usize>)> =
        builtin.iter().map(|c| (*c, None)).chain(custom.iter().enumerate().map(|(i, c)| (*c, Some(i)))).collect();
    let known = all.iter().any(|(c, _)| same(c, color));
    let mut cells: Vec<Option<([f32; 3], Option<usize>)>> = all.into_iter().map(Some).collect();
    if !known {
        cells.push(None); // "save current color"
    }
    let mut remove = None;
    ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
    for row in cells.chunks(cols) {
        ui.horizontal(|ui| {
            for cell in row {
                match cell {
                    Some((c, custom_index)) => {
                        let r = swatch(ui, *c, same(c, color));
                        let r = match custom_index {
                            Some(i) => {
                                let r = r.on_hover_text("Right-click to remove");
                                r.context_menu(|ui| {
                                    if ui.button("Remove from palette").clicked() {
                                        remove = Some(*i);
                                    }
                                });
                                r
                            }
                            None => r,
                        };
                        if r.clicked() {
                            *color = *c;
                            changed = true;
                        }
                    }
                    None => {
                        let p = theme::current(ui.ctx());
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(SWATCH), Sense::click());
                        let hovered = resp.hovered();
                        let painter = ui.painter();
                        painter.circle_stroke(r.center(), 8.5, Stroke::new(1.0, if hovered { ACCENT } else { p.weak }));
                        painter.text(
                            r.center(),
                            egui::Align2::CENTER_CENTER,
                            egui_phosphor::regular::PLUS,
                            egui::FontId::proportional(11.0),
                            if hovered { ACCENT } else { p.weak },
                        );
                        if resp.on_hover_text("Save this color to the palette").clicked() {
                            custom.push(*color);
                            custom_changed = true;
                        }
                    }
                }
            }
        });
    }
    if let Some(i) = remove {
        custom.remove(i);
        custom_changed = true;
    }
    (changed, custom_changed)
}

/// Saturation/value square, hue bar and hex field. Returns true if `color` changed.
pub fn picker(ui: &mut egui::Ui, color: &mut [f32; 3]) -> bool {
    let id = Id::new("ochre-color-picker");
    // Keep hue/saturation across frames, so dragging to black or gray doesn't lose them.
    let mut hsva: Hsva = ui
        .data(|d| d.get_temp::<(Hsva, [f32; 3])>(id))
        .filter(|(_, last)| same(last, color))
        .map_or_else(|| Hsva::from(rgb(*color)), |(h, _)| h);
    let mut changed = false;
    let w = ui.available_width();

    // Saturation (x) / value (y) square.
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 112.0), Sense::click_and_drag());
    let hue = Color32::from(Hsva::new(hsva.h, 1.0, 1.0, 1.0));
    let mut mesh = Mesh::default();
    quad(&mut mesh, rect, [Color32::WHITE, hue, hue, Color32::WHITE]);
    quad(&mut mesh, rect, [Color32::TRANSPARENT, Color32::TRANSPARENT, Color32::BLACK, Color32::BLACK]);
    let painter = ui.painter();
    painter.add(mesh);
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, Color32::from_black_alpha(40)), StrokeKind::Inside);
    if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.is_pointer_button_down_on()) {
        hsva.s = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        hsva.v = 1.0 - ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        changed = true;
    }
    let handle = pos2(rect.left() + hsva.s * rect.width(), rect.top() + (1.0 - hsva.v) * rect.height());
    knob(ui, handle, Color32::from(Hsva { a: 1.0, ..hsva }));

    ui.add_space(4.0);
    // Hue bar.
    let (bar, resp) = ui.allocate_exact_size(vec2(w, 12.0), Sense::click_and_drag());
    let mut mesh = Mesh::default();
    for i in 0..6 {
        let x0 = bar.left() + bar.width() * i as f32 / 6.0;
        let x1 = bar.left() + bar.width() * (i + 1) as f32 / 6.0;
        let c0 = Color32::from(Hsva::new(i as f32 / 6.0, 1.0, 1.0, 1.0));
        let c1 = Color32::from(Hsva::new((i + 1) as f32 / 6.0, 1.0, 1.0, 1.0));
        quad(&mut mesh, Rect::from_x_y_ranges(x0..=x1, bar.y_range()), [c0, c1, c1, c0]);
    }
    ui.painter().add(mesh);
    if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.is_pointer_button_down_on()) {
        hsva.h = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 0.9999);
        changed = true;
    }
    knob(ui, pos2(bar.left() + hsva.h * bar.width(), bar.center().y), hue_of(hsva.h));

    if changed {
        *color = to_rgb(Color32::from(Hsva { a: 1.0, ..hsva }));
    }

    ui.add_space(6.0);
    // Preview + hex field (type e.g. "#1473E6" or "1473e6").
    ui.horizontal(|ui| {
        let (pr, _) = ui.allocate_exact_size(vec2(26.0, 20.0), Sense::hover());
        ui.painter().rect_filled(pr, 5.0, rgb(*color));
        ui.painter().rect_stroke(pr, 5.0, Stroke::new(1.0, Color32::from_black_alpha(40)), StrokeKind::Inside);
        let hex_id = id.with("hex");
        let field_id = id.with("hex-field");
        let focused = ui.memory(|m| m.has_focus(field_id));
        let mut text: String = ui
            .data(|d| d.get_temp::<String>(hex_id))
            .filter(|_| focused)
            .unwrap_or_else(|| rgb(*color).to_hex().trim_start_matches('#')[..6].to_uppercase());
        let r = ui.add(
            egui::TextEdit::singleline(&mut text)
                .id(field_id)
                .font(egui::TextStyle::Monospace)
                .desired_width(70.0)
                .char_limit(7)
                .prefix(egui::RichText::new("#").weak()),
        );
        if r.changed() {
            let t = text.trim().trim_start_matches('#');
            if t.len() == 6
                && let Ok(c) = Color32::from_hex(&format!("#{t}"))
            {
                *color = to_rgb(c);
                hsva = Hsva::from(c);
                changed = true;
            }
        }
        ui.data_mut(|d| d.insert_temp(hex_id, text));
    });
    if changed {
        hsva = Hsva { a: 1.0, ..hsva };
    }
    ui.data_mut(|d| d.insert_temp(id, (hsva, *color)));
    changed
}

fn hue_of(h: f32) -> Color32 {
    Color32::from(Hsva::new(h, 1.0, 1.0, 1.0))
}

/// Round handle with a white ring and a subtle dark outline (visible on any color).
fn knob(ui: &egui::Ui, at: Pos2, fill: Color32) {
    let painter = ui.painter();
    painter.circle_filled(at, 7.0, fill);
    painter.circle_stroke(at, 7.0, Stroke::new(2.0, Color32::WHITE));
    painter.circle_stroke(at, 8.5, Stroke::new(1.0, Color32::from_black_alpha(70)));
}

/// Adds a quad with corner colors in order top-left, top-right, bottom-right, bottom-left.
fn quad(mesh: &mut Mesh, r: Rect, c: [Color32; 4]) {
    let base = mesh.vertices.len() as u32;
    mesh.colored_vertex(r.left_top(), c[0]);
    mesh.colored_vertex(r.right_top(), c[1]);
    mesh.colored_vertex(r.right_bottom(), c[2]);
    mesh.colored_vertex(r.left_bottom(), c[3]);
    mesh.add_triangle(base, base + 1, base + 2);
    mesh.add_triangle(base, base + 2, base + 3);
}

/// Fill presets for shapes: none, the outline's color, then the palette.
/// Returns true if `fill` changed.
pub fn fill_presets(ui: &mut egui::Ui, fill: &mut Option<[f32; 3]>, outline: [f32; 3], builtin: &[[f32; 3]]) -> bool {
    let p = theme::current(ui.ctx());
    let mut changed = false;
    let mut cells: Vec<(Option<[f32; 3]>, &str)> = vec![(None, "No fill"), (Some(outline), "Same as outline")];
    cells.extend(builtin.iter().filter(|c| !same(c, &outline)).map(|c| (Some(*c), "")));
    ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
    for row in cells.chunks(7) {
        ui.horizontal(|ui| {
            for (c, tip) in row {
                let selected = match (fill.as_ref(), c) {
                    (None, None) => true,
                    (Some(a), Some(b)) => same(a, b),
                    _ => false,
                };
                let resp = match c {
                    Some(c) => swatch(ui, *c, selected),
                    None => {
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(SWATCH), Sense::click());
                        let painter = ui.painter();
                        if selected {
                            painter.circle_stroke(r.center(), 11.5, Stroke::new(2.0, ACCENT));
                        } else if resp.hovered() {
                            painter.circle_stroke(r.center(), 11.5, Stroke::new(2.0, p.border));
                        }
                        painter.circle_stroke(r.center(), 8.5, Stroke::new(1.0, p.weak));
                        let d = vec2(6.0, -6.0);
                        painter.line_segment([r.center() - d, r.center() + d], Stroke::new(1.5, Color32::from_rgb(0xDC, 0x3C, 0x3C)));
                        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                    }
                };
                let resp = if tip.is_empty() { resp } else { resp.on_hover_text(*tip) };
                if resp.clicked() && !selected {
                    *fill = *c;
                    changed = true;
                }
            }
        });
    }
    changed
}
