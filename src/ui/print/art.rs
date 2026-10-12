//! Small illustrations for the two-sided and booklet instructions, drawn in
//! the current text color so they suit light and dark mode.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, pos2, vec2};

use crate::ui::theme::{self, ACCENT};

/// A sheet of paper, with a few lines of "text" if `printed`.
fn sheet(painter: &egui::Painter, r: Rect, printed: bool) {
    painter.rect(r, 3.0, Color32::WHITE, Stroke::new(1.0, Color32::from_black_alpha(90)), egui::StrokeKind::Inside);
    if printed {
        painter.rect_filled(Rect::from_min_size(r.min + vec2(5.0, 6.0), vec2(r.width() - 10.0, 5.0)), 0.0, ACCENT);
        // Paper is white in both modes, so the "text" is always dark.
        let ink = Color32::from_rgba_unmultiplied(77, 77, 84, 140);
        let mut y = r.top() + 16.0;
        while y < r.bottom() - 6.0 {
            let w = (r.width() - 10.0) * if ((y as i32) / 5) % 3 == 2 { 0.6 } else { 1.0 };
            painter.rect_filled(Rect::from_min_size(pos2(r.left() + 5.0, y), vec2(w, 1.6)), 0.0, ink);
            y += 5.0;
        }
    }
}

/// A curved arrow from `a` to `b`, bowed up by `bend`.
fn arrow(painter: &egui::Painter, a: Pos2, b: Pos2, bend: f32) {
    let stroke = Stroke::new(2.5, ACCENT);
    let mid = pos2((a.x + b.x) / 2.0, (a.y + b.y) / 2.0 - bend);
    let curve =
        egui::epaint::CubicBezierShape::from_points_stroke([a, mid, mid, b], false, Color32::TRANSPARENT, stroke);
    painter.add(Shape::CubicBezier(curve));
    // Arrow head along the last segment's direction.
    let angle = (b.y - mid.y).atan2(b.x - mid.x);
    for turn in [-0.45f32, 0.45] {
        let back = pos2(b.x - 10.0 * (angle + turn).cos(), b.y - 10.0 * (angle + turn).sin());
        painter.line_segment([b, back], stroke);
    }
}

fn caption(painter: &egui::Painter, text: &str, center_x: f32, y: f32, color: Color32) {
    painter.text(
        pos2(center_x, y),
        Align2::CENTER_BOTTOM,
        text,
        FontId::proportional(11.0),
        color.gamma_multiply(0.75),
    );
}

fn area(ui: &mut egui::Ui, width: f32, height: f32) -> (egui::Painter, Rect) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width().max(width), height), Sense::hover());
    let left = rect.center().x - width / 2.0;
    (ui.painter_at(rect), Rect::from_min_size(pos2(left, rect.top()), vec2(width, height)))
}

/// The printed stack goes from the output tray back into the paper tray.
pub fn reload(ui: &mut egui::Ui) {
    let ink = theme::current(ui.ctx()).text;
    let (painter, r) = area(ui, 300.0, 130.0);
    let (cx, top) = (r.center().x, r.top());
    // Output tray: a stack of printed fronts.
    for i in 0..4 {
        let o = i as f32 * 3.0;
        sheet(&painter, Rect::from_min_size(pos2(cx - 120.0 + o, top + 18.0 + o), vec2(62.0, 84.0)), i == 3);
    }
    caption(&painter, "Printed fronts", cx - 86.0, top + 126.0, ink);
    arrow(&painter, pos2(cx - 40.0, top + 46.0), pos2(cx + 42.0, top + 46.0), 34.0);
    // Paper tray: the same stack, blank sides up.
    for i in 0..4 {
        let o = i as f32 * 3.0;
        sheet(&painter, Rect::from_min_size(pos2(cx + 52.0 + o, top + 18.0 + o), vec2(62.0, 84.0)), false);
    }
    caption(&painter, "Back in the tray", cx + 86.0, top + 126.0, ink);
}

/// A flat sheet with its fold line, then the folded booklet(s).
pub fn fold(ui: &mut egui::Ui, booklets: usize) {
    let ink = theme::current(ui.ctx()).text;
    let (painter, r) = area(ui, 320.0, 140.0);
    let (cx, top) = (r.center().x, r.top());
    // Open landscape sheet, pages 8 | 1.
    let (sx, sy, sw, sh) = (cx - 150.0, top + 26.0, 124.0, 84.0);
    sheet(&painter, Rect::from_min_size(pos2(sx, sy), vec2(sw / 2.0, sh)), true);
    sheet(&painter, Rect::from_min_size(pos2(sx + sw / 2.0, sy), vec2(sw / 2.0, sh)), true);
    let fold = [pos2(sx + sw / 2.0, sy - 8.0), pos2(sx + sw / 2.0, sy + sh + 8.0)];
    painter.extend(Shape::dashed_line(&fold, Stroke::new(1.5, ACCENT), 4.0, 3.0));
    caption(&painter, "Fold along the middle", sx + sw / 2.0, top + 136.0, ink);
    arrow(&painter, pos2(cx - 14.0, top + 62.0), pos2(cx + 34.0, top + 62.0), 22.0);
    // The folded booklet, slightly open: back cover behind, front cover in front.
    let (bx, by, bw, bh) = (cx + 58.0, top + 22.0, 62.0, 88.0);
    for k in 0..booklets.clamp(1, 3) {
        let o = vec2(k as f32 * 6.0, -(k as f32) * 6.0);
        let back = vec![
            pos2(bx, by) + o,
            pos2(bx + bw * 0.92, by - 6.0) + o,
            pos2(bx + bw * 0.92, by + bh - 6.0) + o,
            pos2(bx, by + bh) + o,
        ];
        let outline = Stroke::new(1.0, Color32::from_black_alpha(90));
        painter.add(Shape::convex_polygon(back, Color32::from_rgb(237, 237, 242), outline));
        sheet(&painter, Rect::from_min_size(pos2(bx, by) + o, vec2(bw, bh)), true);
    }
    let text = if booklets > 1 { "Fold each booklet, then stack them" } else { "Your booklet" };
    caption(&painter, text, bx + bw / 2.0, top + 136.0, ink);
}
