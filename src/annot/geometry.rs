//! Page <-> display transforms and hit testing.
//!
//! "Display" space is the page as shown on screen: points, origin top-left,
//! y down, page rotation applied. User space is PDF user space (y up).

use super::model::{Annotation, Kind, Pt, ShapeKind};

/// Affine transform `(x, y) -> (a x + c y + e, b x + d y + f)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Affine {
    pub fn apply(&self, p: Pt) -> Pt {
        Pt::new(self.a * p.x + self.c * p.y + self.e, self.b * p.x + self.d * p.y + self.f)
    }
    pub fn apply_vec(&self, p: Pt) -> Pt {
        Pt::new(self.a * p.x + self.c * p.y, self.b * p.x + self.d * p.y)
    }
    /// `self` followed by scale `s` and translation `(tx, ty)`.
    pub fn then_scale_translate(&self, s: f32, tx: f32, ty: f32) -> Affine {
        Affine {
            a: self.a * s,
            b: self.b * s,
            c: self.c * s,
            d: self.d * s,
            e: self.e * s + tx,
            f: self.f * s + ty,
        }
    }
    pub fn to_skia(self) -> tiny_skia::Transform {
        tiny_skia::Transform::from_row(self.a, self.b, self.c, self.d, self.e, self.f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageGeom {
    /// Effective (crop) box `[x0, y0, x1, y1]` in user space.
    pub bbox: [f32; 4],
    /// Clockwise display rotation: 0, 90, 180 or 270.
    pub rotation: u16,
}

impl PageGeom {
    fn wh(&self) -> (f32, f32) {
        (self.bbox[2] - self.bbox[0], self.bbox[3] - self.bbox[1])
    }

    /// Size on screen in points.
    pub fn display_size(&self) -> (f32, f32) {
        let (w, h) = self.wh();
        if self.rotation % 180 == 90 { (h, w) } else { (w, h) }
    }

    /// User space -> display points.
    pub fn to_display(self) -> Affine {
        let [x0, y0, x1, y1] = self.bbox;
        // u0 = x - x0, v0 = y1 - y is the unrotated top-left/y-down view;
        // rotating that image clockwise gives the cases below.
        match self.rotation {
            90 => Affine { a: 0.0, b: 1.0, c: 1.0, d: 0.0, e: -y0, f: -x0 },
            180 => Affine { a: -1.0, b: 0.0, c: 0.0, d: 1.0, e: x1, f: -y0 },
            270 => Affine { a: 0.0, b: -1.0, c: -1.0, d: 0.0, e: y1, f: x1 },
            _ => Affine { a: 1.0, b: 0.0, c: 0.0, d: -1.0, e: -x0, f: y1 },
        }
    }

    /// Display points -> user space.
    pub fn to_user(self) -> Affine {
        let t = self.to_display();
        // Linear part is a signed permutation matrix, so its inverse is its transpose.
        let (a, b, c, d) = (t.a, t.c, t.b, t.d);
        let e = -(a * t.e + c * t.f);
        let f = -(b * t.e + d * t.f);
        Affine { a, b, c, d, e, f }
    }
}

pub fn dist_to_segment(p: Pt, a: Pt, b: Pt) -> f32 {
    let ab = b.sub(a);
    let l2 = ab.dot(ab);
    if l2 < 1e-12 {
        return p.dist(a);
    }
    let t = (p.sub(a).dot(ab) / l2).clamp(0.0, 1.0);
    p.dist(a.add(ab.scale(t)))
}

pub fn dist_to_polyline(p: Pt, pts: &[Pt]) -> f32 {
    match pts {
        [] => f32::INFINITY,
        [only] => p.dist(*only),
        _ => pts.windows(2).map(|w| dist_to_segment(p, w[0], w[1])).fold(f32::INFINITY, f32::min),
    }
}

/// Flattens a cubic Bézier chain `[p0, c1, c2, p1, ...]` into a polyline.
pub fn flatten_bezier(curve: &[Pt], steps_per_segment: usize) -> Vec<Pt> {
    let mut out = Vec::new();
    if let Some(first) = curve.first() {
        out.push(*first);
    }
    for seg in curve.windows(4).step_by(3) {
        let [p0, c1, c2, p1] = [seg[0], seg[1], seg[2], seg[3]];
        for i in 1..=steps_per_segment {
            let t = i as f32 / steps_per_segment as f32;
            let mt = 1.0 - t;
            out.push(Pt::new(
                mt * mt * mt * p0.x + 3.0 * mt * mt * t * c1.x + 3.0 * mt * t * t * c2.x + t * t * t * p1.x,
                mt * mt * mt * p0.y + 3.0 * mt * mt * t * c1.y + 3.0 * mt * t * t * c2.y + t * t * t * p1.y,
            ));
        }
    }
    out
}

pub fn ellipse_points(a: Pt, b: Pt, n: usize) -> Vec<Pt> {
    let c = a.lerp(b, 0.5);
    let (rx, ry) = ((b.x - a.x).abs() / 2.0, (b.y - a.y).abs() / 2.0);
    (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32 * std::f32::consts::TAU;
            Pt::new(c.x + rx * t.cos(), c.y + ry * t.sin())
        })
        .collect()
}

pub fn rect_points(a: Pt, b: Pt) -> Vec<Pt> {
    vec![a, Pt::new(b.x, a.y), b, Pt::new(a.x, b.y), a]
}

/// Strokes of a tick or cross drawn in the box `a`-`b`.
///
/// `a` and `b` are the box's top-left and bottom-right corners *as seen on screen*,
/// so the signs of `b - a` tell which user-space axes point right and down, and
/// the mark stays upright on rotated pages.
pub fn mark_strokes(shape: ShapeKind, a: Pt, b: Pt) -> Vec<Vec<Pt>> {
    let d = b.sub(a);
    let (right, down) = match (d.x >= 0.0, d.y >= 0.0) {
        (true, false) => (Pt::new(1.0, 0.0), Pt::new(0.0, -1.0)), // not rotated
        (true, true) => (Pt::new(0.0, 1.0), Pt::new(1.0, 0.0)),   // 90°
        (false, true) => (Pt::new(-1.0, 0.0), Pt::new(0.0, 1.0)), // 180°
        (false, false) => (Pt::new(0.0, -1.0), Pt::new(-1.0, 0.0)), // 270°
    };
    let (w, h) = (d.dot(right).abs(), d.dot(down).abs());
    let at = |fx: f32, fy: f32| a.add(right.scale(fx * w)).add(down.scale(fy * h));
    match shape {
        ShapeKind::Check => vec![vec![at(0.0, 0.55), at(0.36, 0.95), at(1.0, 0.0)]],
        ShapeKind::Cross => vec![vec![at(0.1, 0.1), at(0.9, 0.9)], vec![at(0.9, 0.1), at(0.1, 0.9)]],
        _ => Vec::new(),
    }
}

/// The two short lines of an open arrow head at `b`, pointing away from `a`.
pub fn arrow_head(a: Pt, b: Pt, width: f32) -> [[Pt; 2]; 2] {
    let len = (width * 4.0).max(8.0);
    let dir = b.sub(a).normalized();
    let (s, c) = (0.5f32.sin(), 0.5f32.cos()); // ~29 degrees
    let back = dir.scale(-len);
    let l = Pt::new(back.x * c - back.y * s, back.x * s + back.y * c);
    let r = Pt::new(back.x * c + back.y * s, -back.x * s + back.y * c);
    [[b, b.add(l)], [b, b.add(r)]]
}

/// Helvetica advance widths (1/1000 em) for ASCII 32..=126.
const HELV_WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // ' '..'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // 0-9
    278, 278, 584, 584, 584, 556, 1015, // ':'..'@'
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667,
    611, 722, 667, 944, 667, 667, 611, // A-Z
    278, 278, 278, 469, 556, 333, // '['..'`'
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500,
    278, 556, 500, 722, 500, 500, 500, // a-z
    334, 260, 334, 584, // '{'..'~'
];

/// Width of `s` in Helvetica at `size` points.
pub fn helv_text_width(s: &str, size: f32) -> f32 {
    s.chars()
        .map(|ch| match ch as u32 {
            c @ 32..=126 => HELV_WIDTHS[(c - 32) as usize] as f32,
            _ => 556.0,
        })
        .sum::<f32>()
        * size
        / 1000.0
}

pub const TEXT_LINE_HEIGHT: f32 = 1.2;
pub const TEXT_PAD: f32 = 2.0;
/// Baseline offset of the first line below the box top, as a fraction of the font size.
pub const TEXT_ASCENT: f32 = 0.9;

/// The lines a text box shows, as byte ranges of `text`, each with whether a line
/// break (`\n`, not included) ends it. With a `width`, lines wrap at spaces (which
/// stay at the end of their line), and a word longer than the width is split.
pub fn text_lines(text: &str, size: f32, width: Option<f32>) -> Vec<(std::ops::Range<usize>, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (n, para) in text.split('\n').enumerate() {
        if n > 0 {
            start += 1;
        }
        let end = start + para.len();
        let hard = end < text.len();
        let Some(width) = width else {
            out.push((start..end, hard));
            start = end;
            continue;
        };
        let fits = |a: usize, b: usize| helv_text_width(text[a..b].trim_end_matches(' '), size) <= width;
        let (mut line, mut i, mut split) = (start, start, false);
        while i < end {
            // The next word and the spaces after it.
            let rest = &text[i..end];
            let word = rest.find(' ').unwrap_or(rest.len());
            let next = i + word + rest[word..].len() - rest[word..].trim_start_matches(' ').len();
            split = false;
            if fits(line, i + word) {
                i = next;
            } else if line < i {
                out.push((line..i, false));
                line = i;
            } else {
                // A word too long for a line on its own: as many characters as fit (at least one).
                let mut cut = i + rest.chars().next().map_or(1, char::len_utf8);
                for (k, ch) in rest[..word].char_indices().skip(1) {
                    if !fits(line, i + k + ch.len_utf8()) {
                        break;
                    }
                    cut = i + k + ch.len_utf8();
                }
                out.push((line..cut, false));
                (line, i, split) = (cut, cut, true);
            }
        }
        // A split that ended the paragraph already made its last line.
        if !split {
            out.push((line..end, hard));
        } else if let Some(last) = out.last_mut() {
            last.1 = hard;
        }
        start = end;
    }
    out
}

/// Size `(width, height)` of a text box in points, in its own (screen-aligned) frame.
pub fn text_box_size(text: &str, size: f32, width: Option<f32>, height: Option<f32>) -> (f32, f32) {
    let lines = text_lines(text, size, width);
    let w = width.unwrap_or_else(|| lines.iter().map(|(r, _)| helv_text_width(&text[r.clone()], size)).fold(0.0, f32::max));
    let h = (lines.len() as f32 * size * TEXT_LINE_HEIGHT).max(height.unwrap_or(0.0));
    (w + 2.0 * TEXT_PAD, h + 2.0 * TEXT_PAD)
}

/// The four corners (user space) of a text annotation box.
#[allow(clippy::too_many_arguments)]
pub fn text_corners(
    origin: Pt,
    right: Pt,
    down: Pt,
    text: &str,
    size: f32,
    width: Option<f32>,
    height: Option<f32>,
) -> [Pt; 4] {
    let (w, h) = text_box_size(text, size, width, height);
    let r = right.scale(w);
    let d = down.scale(h);
    [origin, origin.add(r), origin.add(r).add(d), origin.add(d)]
}

/// Corners of a box shape's (rotated) box, in order around it.
pub fn box_corners(a: &Annotation) -> [Pt; 4] {
    let (p, q) = match a.kind {
        Kind::Shape { a, b, .. } => (a, b),
        _ => (Pt::default(), Pt::default()),
    };
    let c = p.lerp(q, 0.5);
    [p, Pt::new(q.x, p.y), q, Pt::new(p.x, q.y)].map(|v| v.rotate_about(c, a.angle))
}

/// The strokes of a tick or cross, rotated with the annotation.
pub fn mark_lines(a: &Annotation) -> Vec<Vec<Pt>> {
    let Kind::Shape { shape, a: p, b: q } = a.kind else { return Vec::new() };
    let c = p.lerp(q, 0.5);
    mark_strokes(shape, p, q).into_iter().map(|l| l.into_iter().map(|v| v.rotate_about(c, a.angle)).collect()).collect()
}

/// Axis-aligned bounds `[x0, y0, x1, y1]` (user space) including stroke width.
pub fn bounds(a: &Annotation) -> [f32; 4] {
    let pts: Vec<Pt> = match &a.kind {
        Kind::Shape { shape: ShapeKind::Rect, .. } => box_corners(a).to_vec(),
        Kind::Shape { shape: ShapeKind::Ellipse, a: p, b: q } => {
            // Exact extent of the rotated ellipse.
            let c = p.lerp(*q, 0.5);
            let (rx, ry) = ((q.x - p.x).abs() / 2.0, (q.y - p.y).abs() / 2.0);
            let (s, co) = a.angle.sin_cos();
            let (hx, hy) = ((rx * co).hypot(ry * s), (rx * s).hypot(ry * co));
            vec![Pt::new(c.x - hx, c.y - hy), Pt::new(c.x + hx, c.y + hy)]
        }
        Kind::Shape { shape: ShapeKind::Check | ShapeKind::Cross, .. } => mark_lines(a).concat(),
        Kind::Ink { curve, .. } => curve.clone(),
        Kind::Text { origin, right, down, text, width, height } => {
            text_corners(*origin, *right, *down, text, a.style.width, *width, *height).to_vec()
        }
        Kind::Shape { shape, a: p, b: q } => {
            let mut v = vec![*p, *q];
            if *shape == ShapeKind::Arrow {
                v.extend(arrow_head(*p, *q, a.style.width).iter().flatten());
            }
            v
        }
        Kind::Markup { quads, .. } => quads.iter().flatten().copied().collect(),
    };
    let pad = match a.kind {
        Kind::Text { .. } | Kind::Markup { .. } => 0.0,
        _ => a.style.width / 2.0 + 1.0,
    };
    let mut r = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for p in pts {
        r = [r[0].min(p.x), r[1].min(p.y), r[2].max(p.x), r[3].max(p.y)];
    }
    if !r[0].is_finite() {
        return [0.0; 4];
    }
    [r[0] - pad, r[1] - pad, r[2] + pad, r[3] + pad]
}

/// `a` scaled by `(sx, sy)` along the page's display axes about `anchor` (display
/// space), so resizing follows the screen on rotated pages. A text box scales its
/// font size by `sx` (callers pass `sx == sy` for text).
pub fn scaled(a: &Annotation, g: &PageGeom, anchor: Pt, sx: f32, sy: f32) -> Annotation {
    let (to_d, to_u) = (g.to_display(), g.to_user());
    let mut out = a.clone();
    out.map_points(|p| {
        let d = to_d.apply(p);
        to_u.apply(Pt::new(anchor.x + (d.x - anchor.x) * sx, anchor.y + (d.y - anchor.y) * sy))
    });
    if let Kind::Text { width, height, .. } = &mut out.kind {
        out.style.width = a.style.width * sx;
        *width = width.map(|w| w * sx);
        *height = height.map(|h| h * sx);
    }
    out
}

fn point_in_quad(p: Pt, q: &[Pt; 4]) -> bool {
    // QuadPoints order is UL, UR, LL, LR; walk it as a polygon UL-UR-LR-LL.
    let poly = [q[0], q[1], q[3], q[2]];
    let mut inside = false;
    for i in 0..4 {
        let (a, b) = (poly[i], poly[(i + 1) % 4]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
    }
    inside
}

/// Distance (user space) from `p` to the visible part of the annotation; 0 if inside.
pub fn distance(a: &Annotation, p: Pt) -> f32 {
    let half = a.style.width / 2.0;
    match &a.kind {
        Kind::Ink { curve, .. } => (dist_to_polyline(p, &flatten_bezier(curve, 8)) - half).max(0.0),
        Kind::Text { origin, right, down, text, width, height } => {
            let c = text_corners(*origin, *right, *down, text, a.style.width, *width, *height);
            if point_in_quad(p, &[c[0], c[1], c[3], c[2]]) {
                0.0
            } else {
                dist_to_polyline(p, &[c[0], c[1], c[2], c[3], c[0]])
            }
        }
        Kind::Shape { a: s, b: e, .. } if a.angle != 0.0 && a.is_box() => {
            // Measure in the shape's own (unrotated) frame.
            let c = s.lerp(*e, 0.5);
            let upright = Annotation { angle: 0.0, ..a.clone() };
            distance(&upright, p.rotate_about(c, -a.angle))
        }
        Kind::Shape { shape, a: s, b: e } => {
            let filled = a.style.fill.is_some();
            let inside = match shape {
                ShapeKind::Rect => {
                    p.x >= s.x.min(e.x) && p.x <= s.x.max(e.x) && p.y >= s.y.min(e.y) && p.y <= s.y.max(e.y)
                }
                ShapeKind::Ellipse => {
                    let c = s.lerp(*e, 0.5);
                    let (rx, ry) = (((e.x - s.x) / 2.0).abs().max(1e-3), ((e.y - s.y) / 2.0).abs().max(1e-3));
                    ((p.x - c.x) / rx).powi(2) + ((p.y - c.y) / ry).powi(2) <= 1.0
                }
                _ => false,
            };
            if filled && inside {
                return 0.0;
            }
            let d = match shape {
                ShapeKind::Rect => dist_to_polyline(p, &rect_points(*s, *e)),
                ShapeKind::Ellipse => dist_to_polyline(p, &ellipse_points(*s, *e, 64)),
                ShapeKind::Line => dist_to_segment(p, *s, *e),
                ShapeKind::Check | ShapeKind::Cross => mark_strokes(*shape, *s, *e)
                    .iter()
                    .map(|l| dist_to_polyline(p, l))
                    .fold(f32::INFINITY, f32::min),
                ShapeKind::Arrow => {
                    let head = arrow_head(*s, *e, a.style.width);
                    dist_to_segment(p, *s, *e)
                        .min(dist_to_segment(p, head[0][0], head[0][1]))
                        .min(dist_to_segment(p, head[1][0], head[1][1]))
                }
            };
            (d - half).max(0.0)
        }
        Kind::Markup { quads, .. } => {
            if quads.iter().any(|q| point_in_quad(p, q)) {
                0.0
            } else {
                quads
                    .iter()
                    .map(|q| dist_to_polyline(p, &[q[0], q[1], q[3], q[2], q[0]]))
                    .fold(f32::INFINITY, f32::min)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Pt, b: Pt) -> bool {
        a.dist(b) < 1e-3
    }

    #[test]
    fn text_wraps_at_spaces_and_splits_long_words() {
        let shown = |text: &str, width| -> Vec<(String, bool)> {
            text_lines(text, 10.0, width).into_iter().map(|(r, hard)| (text[r].to_string(), hard)).collect()
        };
        let line = |s: &str, hard| (s.to_string(), hard);
        // Without a width only line breaks split it.
        assert_eq!(shown("one two\nthree", None), [line("one two", true), line("three", false)]);
        // "one two" is 35.02 points wide in 10 pt Helvetica; spaces stay at the end of their line.
        assert_eq!(shown("one two three", Some(36.0)), [line("one two ", false), line("three", false)]);
        assert_eq!(shown("one two three", Some(30.0)), [line("one ", false), line("two ", false), line("three", false)]);
        // A word longer than the width is split; at least one letter goes on each line.
        assert_eq!(shown("abcdefgh", Some(22.0)), [line("abcd", false), line("efgh", false)]);
        assert_eq!(shown("WW", Some(1.0)), [line("W", false), line("W", false)]);
        // Empty lines and a trailing break are kept.
        assert_eq!(shown("a\n\nb\n", Some(50.0)), [line("a", true), line("", true), line("b", true), line("", false)]);
        // Lines and breaks account for every byte, so the editor's cursor lines up.
        let text = "Ünïcode wörds wrap\ntoo, even sehrlangewörter";
        let lines = text_lines(text, 10.0, Some(40.0));
        let joined: String = lines.iter().map(|(r, hard)| format!("{}{}", &text[r.clone()], if *hard { "\n" } else { "" })).collect();
        assert_eq!(joined, text);
        // The box is as wide as set, and as tall as its lines.
        let (w, h) = text_box_size("one two three", 10.0, Some(30.0), None);
        assert_eq!((w, h), (30.0 + 2.0 * TEXT_PAD, 3.0 * 10.0 * TEXT_LINE_HEIGHT + 2.0 * TEXT_PAD));
        // A set height is the least it's tall: more lines still grow it.
        assert_eq!(text_box_size("one two three", 10.0, Some(30.0), Some(100.0)).1, 100.0 + 2.0 * TEXT_PAD);
        assert_eq!(text_box_size("one two three", 10.0, Some(30.0), Some(5.0)).1, h);
    }

    #[test]
    fn transforms_round_trip_all_rotations() {
        for rotation in [0, 90, 180, 270] {
            let g = PageGeom { bbox: [10.0, 20.0, 610.0, 820.0], rotation };
            let p = Pt::new(123.0, 456.0);
            assert!(close(g.to_user().apply(g.to_display().apply(p)), p), "rotation {rotation}");
        }
    }

    #[test]
    fn corners_map_to_display_corners() {
        let bbox = [10.0, 20.0, 610.0, 820.0];
        // The user-space corner that appears at the top-left of the screen.
        let cases = [
            (0, Pt::new(10.0, 820.0)),
            (90, Pt::new(10.0, 20.0)),
            (180, Pt::new(610.0, 20.0)),
            (270, Pt::new(610.0, 820.0)),
        ];
        for (rotation, top_left) in cases {
            let g = PageGeom { bbox, rotation };
            assert!(close(g.to_display().apply(top_left), Pt::new(0.0, 0.0)), "rotation {rotation}");
            let (w, h) = g.display_size();
            let opposite = Pt::new(bbox[0] + bbox[2] - top_left.x, bbox[1] + bbox[3] - top_left.y);
            assert!(close(g.to_display().apply(opposite), Pt::new(w, h)), "rotation {rotation}");
        }
    }

    #[test]
    fn ticks_stay_upright_on_rotated_pages() {
        for rotation in [0, 90, 180, 270] {
            let g = PageGeom { bbox: [0.0, 0.0, 600.0, 800.0], rotation };
            // A 20pt box on screen, given as on-screen top-left / bottom-right.
            let (a, b) = (g.to_user().apply(Pt::new(100.0, 100.0)), g.to_user().apply(Pt::new(120.0, 120.0)));
            let tick = mark_strokes(ShapeKind::Check, a, b).remove(0);
            let on_screen: Vec<Pt> = tick.iter().map(|p| g.to_display().apply(*p)).collect();
            // Short stroke down to the lowest point, then a long stroke up to the top right.
            assert!(on_screen[1].y > on_screen[0].y && on_screen[1].y > on_screen[2].y, "rotation {rotation}");
            assert!(on_screen[2].x > on_screen[1].x && on_screen[1].x > on_screen[0].x, "rotation {rotation}");
            assert!((on_screen[2].x - 120.0).abs() < 1e-3 && (on_screen[2].y - 100.0).abs() < 1e-3, "rotation {rotation}");
        }
    }

    #[test]
    fn scaling_follows_the_screen_on_rotated_pages() {
        let style = super::super::model::Style::new([0.0; 3], 2.0, 1.0);
        for rotation in [0, 90, 180, 270] {
            let g = PageGeom { bbox: [0.0, 0.0, 600.0, 800.0], rotation };
            let u = |x, y| g.to_user().apply(Pt::new(x, y));
            let a = Annotation::new(0, style, Kind::Shape { shape: ShapeKind::Rect, a: u(100.0, 100.0), b: u(200.0, 150.0) });
            // Twice as wide on screen, same height, anchored at the on-screen top-left.
            let s = scaled(&a, &g, Pt::new(100.0, 100.0), 2.0, 1.0);
            let Kind::Shape { a: p, b: q, .. } = s.kind else { panic!() };
            let (p, q) = (g.to_display().apply(p), g.to_display().apply(q));
            assert!(close(p, Pt::new(100.0, 100.0)) && close(q, Pt::new(300.0, 150.0)), "rotation {rotation}: {p:?} {q:?}");
        }
    }

    #[test]
    fn hit_testing_ink() {
        let a = Annotation::new(
            0,
            super::super::model::Style::new([0.0; 3], 4.0, 1.0),
            Kind::Ink {
                curve: vec![Pt::new(0.0, 0.0), Pt::new(10.0, 0.0), Pt::new(20.0, 0.0), Pt::new(30.0, 0.0)],
                highlighter: false,
            },
        );
        assert_eq!(distance(&a, Pt::new(15.0, 1.0)), 0.0);
        assert!((distance(&a, Pt::new(15.0, 10.0)) - 8.0).abs() < 1e-3);
    }
}
