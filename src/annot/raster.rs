//! Anti-aliased rasterization of annotations with tiny-skia. Text boxes are
//! drawn by egui instead (tiny-skia has no text support).

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, PixmapMut, Stroke, Transform,
};

use super::geometry::{arrow_head, ellipse_points, mark_strokes};
use super::model::{Annotation, Kind, MarkupKind, Pt, ShapeKind, Style};

fn paint(style: &Style) -> Paint<'static> {
    let [r, g, b] = style.color;
    let mut p = Paint::default();
    p.set_color(Color::from_rgba(r, g, b, style.opacity.clamp(0.0, 1.0)).unwrap_or(Color::BLACK));
    p.anti_alias = true;
    p
}

fn stroke(width: f32, cap: LineCap, join: LineJoin) -> Stroke {
    Stroke { width: width.max(0.1), line_cap: cap, line_join: join, ..Default::default() }
}

pub fn bezier_path(curve: &[Pt]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    let first = curve.first()?;
    pb.move_to(first.x, first.y);
    for seg in curve[1..].chunks_exact(3) {
        pb.cubic_to(seg[0].x, seg[0].y, seg[1].x, seg[1].y, seg[2].x, seg[2].y);
    }
    pb.finish()
}

fn polyline_path(pts: &[Pt], close: bool) -> Option<Path> {
    let mut pb = PathBuilder::new();
    let first = pts.first()?;
    pb.move_to(first.x, first.y);
    for p in &pts[1..] {
        pb.line_to(p.x, p.y);
    }
    if close {
        pb.close();
    }
    pb.finish()
}

/// Draws one annotation. `t` maps user space to pixmap pixels.
pub fn draw(pm: &mut PixmapMut, a: &Annotation, t: Transform) {
    // A rotated box shape is drawn upright in a rotated frame.
    let t = if a.angle != 0.0 && a.is_box() {
        let c = a.center();
        t.pre_concat(Transform::from_rotate_at(a.angle.to_degrees(), c.x, c.y))
    } else {
        t
    };
    let paint = paint(&a.style);
    let w = a.style.width;
    match &a.kind {
        Kind::Ink { curve, .. } => {
            if curve.len() == 1 {
                if let Some(c) = PathBuilder::from_circle(curve[0].x, curve[0].y, w / 2.0) {
                    pm.fill_path(&c, &paint, FillRule::Winding, t, None);
                }
            } else if let Some(path) = bezier_path(curve) {
                pm.stroke_path(&path, &paint, &stroke(w, LineCap::Round, LineJoin::Round), t, None);
            }
        }
        Kind::Shape { shape, a: p, b: q } => {
            let path = match shape {
                ShapeKind::Rect => tiny_skia::Rect::from_ltrb(
                    p.x.min(q.x),
                    p.y.min(q.y),
                    p.x.max(q.x),
                    p.y.max(q.y),
                )
                .map(PathBuilder::from_rect),
                ShapeKind::Ellipse => polyline_path(&ellipse_points(*p, *q, 96), true),
                ShapeKind::Line => polyline_path(&[*p, *q], false),
                ShapeKind::Check | ShapeKind::Cross => {
                    let mut pb = PathBuilder::new();
                    for line in mark_strokes(*shape, *p, *q) {
                        pb.move_to(line[0].x, line[0].y);
                        for pt in &line[1..] {
                            pb.line_to(pt.x, pt.y);
                        }
                    }
                    pb.finish()
                }
                ShapeKind::Arrow => {
                    let [l, r] = arrow_head(*p, *q, w);
                    let mut pb = PathBuilder::new();
                    pb.move_to(p.x, p.y);
                    pb.line_to(q.x, q.y);
                    pb.move_to(l[1].x, l[1].y);
                    pb.line_to(q.x, q.y);
                    pb.line_to(r[1].x, r[1].y);
                    pb.finish()
                }
            };
            let join = if *shape == ShapeKind::Rect { LineJoin::Miter } else { LineJoin::Round };
            if let Some(path) = path {
                if let (Some(fill), ShapeKind::Rect | ShapeKind::Ellipse) = (a.style.fill, shape) {
                    let fill_paint = self::paint(&Style::new(fill, 0.0, a.style.fill_opacity));
                    pm.fill_path(&path, &fill_paint, FillRule::Winding, t, None);
                }
                pm.stroke_path(&path, &paint, &stroke(w, LineCap::Round, join), t, None);
            }
        }
        Kind::Markup { markup, quads } => {
            let mut pb = PathBuilder::new();
            for q in quads {
                match markup {
                    MarkupKind::Highlight => {
                        let [ul, ur, ll, lr] = *q;
                        pb.move_to(ul.x, ul.y);
                        pb.line_to(ur.x, ur.y);
                        pb.line_to(lr.x, lr.y);
                        pb.line_to(ll.x, ll.y);
                        pb.close();
                    }
                    MarkupKind::Underline | MarkupKind::StrikeOut => {
                        let (a, b) = markup_line(q, *markup);
                        pb.move_to(a.x, a.y);
                        pb.line_to(b.x, b.y);
                    }
                }
            }
            let Some(path) = pb.finish() else { return };
            match markup {
                MarkupKind::Highlight => pm.fill_path(&path, &paint, FillRule::Winding, t, None),
                _ => {
                    let width = quads.first().map(markup_thickness).unwrap_or(1.0);
                    pm.stroke_path(&path, &paint, &stroke(width, LineCap::Butt, LineJoin::Miter), t, None)
                }
            }
        }
        Kind::Text { .. } => {}
    }
}

pub fn markup_thickness(q: &[Pt; 4]) -> f32 {
    (q[0].dist(q[2]) * 0.07).max(0.5)
}

/// The line drawn for an underline or strike-out over quad `q`.
pub fn markup_line(q: &[Pt; 4], kind: MarkupKind) -> (Pt, Pt) {
    let [ul, ur, ll, lr] = *q;
    let f = if kind == MarkupKind::StrikeOut { 0.5 } else { 0.1 };
    (ll.lerp(ul, f), lr.lerp(ur, f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translucent_stroke_does_not_darken_where_it_crosses_itself() {
        let mut pm = tiny_skia::Pixmap::new(100, 100).unwrap();
        let a = Annotation::new(
            0,
            Style::new([1.0, 0.0, 0.0], 10.0, 0.5),
            Kind::Ink {
                // Two crossing lines in one stroke.
                curve: vec![
                    Pt::new(10.0, 10.0),
                    Pt::new(40.0, 40.0),
                    Pt::new(60.0, 60.0),
                    Pt::new(90.0, 90.0),
                    Pt::new(90.0, 50.0),
                    Pt::new(90.0, 30.0),
                    Pt::new(10.0, 90.0),
                ],
                highlighter: false,
            },
        );
        draw(&mut pm.as_mut(), &a, Transform::identity());
        let alpha = |x, y| pm.pixel(x, y).unwrap().alpha();
        // Alpha at the crossing equals alpha on a plain part of the stroke.
        let plain = alpha(25, 25);
        assert!(plain > 100 && plain < 140, "plain alpha {plain}");
        let cross_alpha = (40..=60).flat_map(|x| (40..=60).map(move |y| (x, y))).map(|(x, y)| alpha(x, y)).max().unwrap();
        assert!(cross_alpha <= plain + 2, "crossing alpha {cross_alpha} vs {plain}");
    }
}
