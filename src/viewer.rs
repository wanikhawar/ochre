//! The page canvas: layout, page/overlay textures, live stroke layer and tool
//! gestures.

use eframe::egui::{
    self, Color32, CursorIcon, Event, FontId, Key, Modifiers, Painter, PointerButton, Pos2, Rect,
    Sense, Stroke, StrokeKind, Vec2, pos2, vec2,
};

use crate::annot::geometry::{self, Affine, PageGeom, TEXT_ASCENT, TEXT_LINE_HEIGHT, TEXT_PAD};
use crate::annot::model::{Annotation, Kind, MarkupKind, Pt, ShapeKind, Style};
use crate::annot::raster;
use crate::annot::smoothing::{LazyBrush, catmull_rom, finish_stroke};
use crate::app::{App, Selection, Tool};
use crate::doc::Cmd;
use crate::pdf::worker::{Target, TextChar};

const MARGIN: f32 = 24.0;
/// Room above the first page and below the last for the floating bars.
pub const TOP_MARGIN: f32 = 24.0;
const BOTTOM_MARGIN: f32 = 56.0;
const GAP: f32 = 16.0;
/// Max pixels per page texture (~64 MB RGBA).
const MAX_PIXELS: f32 = 16.0e6;
const MAX_SIDE: f32 = 8192.0;

pub struct PageTex {
    pub tex: egui::TextureHandle,
    pub scale: f32,
    pub generation: u64,
}

pub struct Overlay {
    tex: egui::TextureHandle,
    scale: f32,
    rev: u64,
    exclude: Vec<String>,
}

pub struct TextEditState {
    /// The annotation being edited, or None for a new text box.
    pub id: Option<String>,
    pub page: usize,
    pub origin: Pt,
    pub right: Pt,
    pub down: Pt,
    pub text: String,
    pub style: Style,
    focus: bool,
}

/// A note being written for one of our annotations.
pub struct NoteEdit {
    pub id: String,
    pub text: String,
    pub focus: bool,
}

/// How a selected annotation is being changed by dragging.
#[derive(Clone, Copy, Debug)]
pub enum EditOp {
    /// Dragging the body; `start` is the press point (user space).
    Move { start: Pt },
    /// Dragging a corner handle. Display space: `anchor` is the opposite corner,
    /// `corner` the dragged one and `start` the press point.
    Scale { anchor: Pt, corner: Pt, start: Pt },
    /// Dragging one end (0 = `a`, 1 = `b`) of a line or arrow.
    Endpoint(usize),
    /// Dragging a corner of a rotated box shape (user space): `anchor` is the
    /// opposite corner, `corner` the dragged one (index `i` in box order).
    ScaleBox { anchor: Pt, corner: Pt, i: usize },
    /// Rotating about `center` (user space); `start` is the pointer's initial angle.
    Rotate { center: Pt, start: f32 },
}

/// A grab point on the selected annotation.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Handle {
    /// Corner of the bounding box, in screen order: top-left, top-right, bottom-left, bottom-right.
    Corner(usize),
    /// Corner of a rotated box shape's box, in [`geometry::box_corners`] order.
    BoxCorner(usize),
    Endpoint(usize),
    /// The round handle above the selection that rotates it.
    Rotate,
}

const HANDLE_SIZE: f32 = 8.0;
/// How far above the selection outline the rotate handle sits.
const ROTATE_HANDLE_GAP: f32 = 22.0;
const NOTE_BADGE_RADIUS: f32 = 8.0;

/// Where the note badge of an annotation with screen bounds `r` goes: just right of
/// its top edge, clear of the selection handles.
fn badge_center(r: Rect) -> Pos2 {
    pos2(r.right() + 4.0 + HANDLE_SIZE + NOTE_BADGE_RADIUS, r.top() + NOTE_BADGE_RADIUS - 4.0)
}

pub enum Gesture {
    None,
    Ink { page: usize, brush: LazyBrush, raw: Vec<Pt>, highlighter: bool, style: Style },
    Shape { page: usize, shape: ShapeKind, a: Pt, b: Pt, style: Style },
    Markup { page: usize, markup: MarkupKind, start: usize, end: usize, style: Style },
    Erase { removed: Vec<Cmd> },
    /// Dragging the selection: several annotations when moving a group, one otherwise.
    Edit { before: Vec<Annotation>, current: Vec<Annotation>, op: EditOp },
    /// Dragging a selection box (user space of `page`). With `add`, what it touches
    /// is added to `base`, the selection it started with.
    Marquee { page: usize, start: Pt, end: Pt, add: bool, base: Vec<String> },
    Pan { last: Pos2 },
    /// Pressed on a link: a click follows it; dragging instead selects text from
    /// `char` (Select tool) or pans (Hand tool).
    Link { target: Target, start: Pos2, page: usize, char: Option<usize> },
    /// Dragging out a text selection with the Select tool. `click` is what a plain
    /// click (no drag) selects instead, e.g. another app's highlight over the text.
    SelectText { click: Option<Selection> },
}

/// Selected page text: an inclusive range of pdfium character indices.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextSel {
    pub page: usize,
    pub start: usize,
    pub end: usize,
}

/// Vim-style motions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    NextPage,
    PrevPage,
    Top,
    Bottom,
    HalfDown,
    HalfUp,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Fit {
    Width,
    Page,
}

/// A scroll to a spot on a page: `y` is display points from the page top, and the
/// spot ends up `margin` pixels below the window top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Jump {
    pub page: usize,
    pub y: f32,
    pub margin: f32,
    pub x: JumpX,
    pub animate: bool,
}

/// Where a jump goes horizontally (page display points).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JumpX {
    /// Leave the horizontal scroll alone.
    Keep,
    /// Bring this span into view, centering it if it isn't already.
    Reveal(f32, f32),
    /// Center the window on this x.
    Center(f32),
    /// Put the window's left edge here (Back).
    Left(f32),
}

/// How many positions the Back history keeps.
const MAX_BACK: usize = 50;

pub struct View {
    pub zoom: f32,
    /// Sticky fit mode: re-applied when the window size changes, cleared by manual zoom.
    pub fit: Option<Fit>,
    fit_applied: Option<(Fit, Vec2)>,
    /// A keyboard motion to apply on the next frame.
    pub motion: Option<Motion>,
    /// Where an animated jump is heading.
    scroll_target: Option<Vec2>,
    /// Screen position for the text selection's action bar (this frame).
    pub sel_anchor: Option<Pos2>,
    /// Screen bounds of the selected annotation (this frame), for its action bar and note editor.
    pub annot_rect: Option<Rect>,
    /// Time and place of the last primary press, for double clicks.
    last_press: Option<(f64, Pos2)>,
    pub goto_page: Option<usize>,
    pub jump: Option<Jump>,
    /// Zoom and window size during the jump under way, and for how many frames they
    /// haven't changed: a jump only ends once they've settled.
    jump_layout: Option<(f32, Vec2)>,
    jump_stable: u8,
    /// Positions to return to with Back (page, display y, display x of the
    /// window's left edge), oldest first.
    pub back: Vec<(usize, f32, f32)>,
    /// Scroll so this point (user space) of a page is visible.
    pub reveal: Option<(usize, Pt)>,
    pub current_page: usize,
    pending_zoom: Option<f32>,
    offset: Vec2,
    set_offset: Option<Vec2>,
    /// Screen rect of the scroll area's visible part (last frame).
    viewport: Rect,
    zoom_changed_at: f64,
    /// Screen rect of every page (this frame).
    page_rects: Vec<Rect>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            fit: Some(Fit::Width),
            fit_applied: None,
            motion: None,
            scroll_target: None,
            sel_anchor: None,
            annot_rect: None,
            last_press: None,
            goto_page: None,
            jump: None,
            jump_layout: None,
            jump_stable: 0,
            back: Vec::new(),
            reveal: None,
            current_page: 0,
            pending_zoom: None,
            offset: Vec2::ZERO,
            set_offset: None,
            viewport: Rect::NOTHING,
            zoom_changed_at: 0.0,
            page_rects: Vec::new(),
        }
    }
}

impl View {
    pub fn zoom_by(&mut self, f: f32) {
        self.fit = None;
        self.pending_zoom = Some(self.pending_zoom.unwrap_or(self.zoom) * f);
    }

    /// Page `i` in user space -> screen.
    fn to_screen(&self, g: &PageGeom, i: usize) -> Affine {
        let r = self.page_rects[i];
        g.to_display().then_scale_translate(self.zoom, r.min.x, r.min.y)
    }

    fn to_user(&self, g: &PageGeom, i: usize, p: Pos2) -> Pt {
        g.to_user().apply(self.to_display(i, p))
    }

    /// Screen -> display points of page `i`.
    fn to_display(&self, i: usize, p: Pos2) -> Pt {
        let r = self.page_rects[i];
        Pt::new((p.x - r.min.x) / self.zoom, (p.y - r.min.y) / self.zoom)
    }

    /// Page at the top of the window and how far down it is (display points). The
    /// gap above a page counts as part of it (`y` is then negative).
    pub fn position(&self) -> Option<(usize, f32)> {
        let top = self.viewport.top();
        let page =
            self.page_rects.iter().position(|r| r.bottom() > top + TOP_MARGIN).or(self.page_rects.len().checked_sub(1))?;
        Some((page, (top - self.page_rects[page].top()) / self.zoom))
    }

    /// Remembers the current position for Back.
    pub fn push_back(&mut self) {
        if let Some((page, y)) = self.position() {
            let x = (self.viewport.left() - self.page_rects[page].left()) / self.zoom;
            self.back.push((page, y, x));
            if self.back.len() > MAX_BACK {
                self.back.remove(0);
            }
        }
    }

    /// Page under (or nearest to, vertically) a screen position.
    fn page_at(&self, p: Pos2, strict: bool) -> Option<usize> {
        if let Some(i) = self.page_rects.iter().position(|r| r.contains(p)) {
            return Some(i);
        }
        if strict {
            return None;
        }
        self.page_rects
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.distance_to_pos(p).total_cmp(&b.distance_to_pos(p)))
            .map(|(i, _)| i)
    }
}

struct Layout {
    origins: Vec<Vec2>,
    sizes: Vec<Vec2>,
    size: Vec2,
}

fn layout(pages: &[PageGeom], zoom: f32, viewport_w: f32) -> Layout {
    let sizes: Vec<Vec2> = pages
        .iter()
        .map(|g| {
            let (w, h) = g.display_size();
            vec2(w * zoom, h * zoom)
        })
        .collect();
    let max_w = sizes.iter().map(|s| s.x).fold(0.0, f32::max);
    let width = (max_w + 2.0 * MARGIN).max(viewport_w);
    let mut y = TOP_MARGIN;
    let origins = sizes
        .iter()
        .map(|s| {
            let o = vec2(((width - s.x) / 2.0).max(MARGIN), y);
            y += s.y + GAP;
            o
        })
        .collect();
    Layout { origins, sizes, size: vec2(width, y - GAP + BOTTOM_MARGIN) }
}

fn pos(p: Pt) -> Pos2 {
    pos2(p.x, p.y)
}

fn color(style: &Style) -> Color32 {
    let [r, g, b] = style.color;
    Color32::from_rgba_unmultiplied(
        (r * 255.0) as u8,
        (g * 255.0) as u8,
        (b * 255.0) as u8,
        (style.opacity * 255.0) as u8,
    )
}

/// Screen-space bounding box of an annotation.
fn screen_bounds(a: &Annotation, aff: &Affine) -> Rect {
    let [x0, y0, x1, y1] = geometry::bounds(a);
    Rect::from_points(&[
        pos(aff.apply(Pt::new(x0, y0))),
        pos(aff.apply(Pt::new(x1, y0))),
        pos(aff.apply(Pt::new(x0, y1))),
        pos(aff.apply(Pt::new(x1, y1))),
    ])
}

/// Scale to render page textures at. `max_side` is the largest texture the GPU
/// takes; beyond the caps, the texture is stretched (slightly blurry, no crash).
fn render_scale(g: &PageGeom, zoom: f32, ppp: f32, max_side: usize) -> f32 {
    let (w, h) = g.display_size();
    let want = zoom * ppp;
    // A couple of pixels short of the limit: sizes are rounded up.
    let side = MAX_SIDE.min(max_side as f32 - 2.0);
    let cap = (MAX_PIXELS / (w * h).max(1.0)).sqrt().min(side / w.max(h).max(1.0));
    // Quantize so tiny zoom jitter doesn't trigger re-renders (down, never past the cap).
    ((want.min(cap) * 100.0).floor() / 100.0).max(0.05)
}

fn shift_constrain(shape: ShapeKind, a: Pt, b: Pt) -> Pt {
    let d = b.sub(a);
    match shape {
        ShapeKind::Rect | ShapeKind::Ellipse | ShapeKind::Check | ShapeKind::Cross => {
            let s = d.x.abs().max(d.y.abs());
            Pt::new(a.x + s * d.x.signum(), a.y + s * d.y.signum())
        }
        ShapeKind::Line | ShapeKind::Arrow => {
            // Snap to multiples of 45 degrees.
            let ang = (d.y.atan2(d.x) / std::f32::consts::FRAC_PI_4).round() * std::f32::consts::FRAC_PI_4;
            let l = d.len();
            Pt::new(a.x + l * ang.cos(), a.y + l * ang.sin())
        }
    }
}

/// Final corners of a shape being drawn from `a` to `b`. A tick or cross placed with
/// a plain click gets a standard size centered on the click, and its corners are
/// stored as on-screen top-left / bottom-right so it stays upright (see `mark_strokes`).
fn shape_box(g: &PageGeom, shape: ShapeKind, a: Pt, b: Pt, style: &Style, zoom: f32) -> (Pt, Pt) {
    if !matches!(shape, ShapeKind::Check | ShapeKind::Cross) {
        return (a, b);
    }
    let (to_d, to_u) = (g.to_display(), g.to_user());
    let (da, db) = (to_d.apply(a), to_d.apply(b));
    let (mut min, mut max) = (Pt::new(da.x.min(db.x), da.y.min(db.y)), Pt::new(da.x.max(db.x), da.y.max(db.y)));
    if a.dist(b) * zoom < 3.0 {
        let half = (style.width * 6.0).max(14.0) / 2.0;
        min = da.sub(Pt::new(half, half));
        max = da.add(Pt::new(half, half));
    }
    (to_u.apply(min), to_u.apply(max))
}

/// `before` changed by a drag of `op` to user-space point `u`. Shift keeps a
/// resized annotation's proportions and snaps a line's end to 45°.
fn edit_annotation(before: &Annotation, g: &PageGeom, op: EditOp, u: Pt, shift: bool, zoom: f32) -> Annotation {
    match op {
        EditOp::Move { start } => {
            let mut moved = before.clone();
            moved.translate(u.sub(start));
            moved
        }
        EditOp::Endpoint(i) => {
            let mut out = before.clone();
            if let Kind::Shape { shape, a, b } = &mut out.kind {
                let (fixed, end) = if i == 0 { (*b, a) } else { (*a, b) };
                *end = if shift { shift_constrain(*shape, fixed, u) } else { u };
            }
            out
        }
        EditOp::Rotate { center, start } => {
            let d = u.sub(center);
            let mut delta = d.y.atan2(d.x) - start;
            if shift {
                // Snap the resulting angle to 15° steps.
                let step = 15f32.to_radians();
                let base = before.angle;
                delta = ((base + delta) / step).round() * step - base;
            }
            before.rotated(center, delta)
        }
        EditOp::ScaleBox { anchor, corner, i } => {
            // Work in the box's own frame, where it is upright.
            let angle = before.angle;
            let local = |v: Pt| v.sub(anchor).rotate(-angle);
            let (d0, d) = (local(corner), local(u));
            let min = 6.0 / zoom;
            let keep_side = |v: f32, v0: f32| if v0 >= 0.0 { v.max(min) } else { v.min(-min) };
            let (mut dx, mut dy) = (keep_side(d.x, d0.x), keep_side(d.y, d0.y));
            if shift && d0.x.abs() > 1e-3 && d0.y.abs() > 1e-3 {
                let s = (dx / d0.x).max(dy / d0.y);
                (dx, dy) = (d0.x * s, d0.y * s);
            }
            let moved = anchor.add(Pt::new(dx, dy).rotate(angle));
            let c = anchor.lerp(moved, 0.5);
            // Back to the unrotated box: corners `i` and the opposite one are known.
            let (ui, uo) = (moved.rotate_about(c, -angle), anchor.rotate_about(c, -angle));
            let (p, q) = match i {
                0 => (ui, uo),
                2 => (uo, ui),
                _ => {
                    let (c1, c3) = if i == 1 { (ui, uo) } else { (uo, ui) };
                    (Pt::new(c3.x, c1.y), Pt::new(c1.x, c3.y))
                }
            };
            let mut out = before.clone();
            if let Kind::Shape { a, b, .. } = &mut out.kind {
                (*a, *b) = (p, q);
            }
            out
        }
        EditOp::Scale { anchor, corner, start } => {
            let d = g.to_display().apply(u);
            let target = corner.add(d.sub(start));
            // Never flip or collapse: at least a few screen pixels on each side.
            let min = 6.0 / zoom;
            let factor = |t: f32, c: f32, a: f32| {
                let span = c - a;
                if span.abs() < 1e-3 { 1.0 } else { ((t - a) / span).max(min / span.abs()) }
            };
            let (mut sx, mut sy) = (factor(target.x, corner.x, anchor.x), factor(target.y, corner.y, anchor.y));
            if let Kind::Text { .. } = before.kind {
                // Text scales evenly (it's the font size), within the size setting's range.
                let w = before.style.width;
                sx = sx.max(sy).clamp(4.0 / w, 144.0 / w);
                sy = sx;
            } else if shift {
                sx = sx.max(sy);
                sy = sx;
            }
            geometry::scaled(before, g, anchor, sx, sy)
        }
    }
}

/// Index of the character at `p`, or the nearest one within `tol` (user units).
fn char_at(chars: &[TextChar], p: Pt, tol: f32) -> Option<usize> {
    let dist = |c: &TextChar| {
        let [x0, y0, x1, y1] = c.rect;
        let dx = (x0 - p.x).max(p.x - x1).max(0.0);
        let dy = (y0 - p.y).max(p.y - y1).max(0.0);
        (dx * dx + dy * dy).sqrt()
    };
    chars
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.ch.is_control() && c.rect[2] > c.rect[0])
        .map(|(i, c)| (i, dist(c)))
        .filter(|(_, d)| *d <= tol)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// The word (run of non-whitespace characters) around character `i`.
fn word_at(chars: &[TextChar], i: usize) -> (usize, usize) {
    let is_word = |k: usize| chars.get(k).is_some_and(|c| !c.ch.is_whitespace() && !c.ch.is_control());
    if !is_word(i) {
        return (i, i);
    }
    let mut s = i;
    while s > 0 && is_word(s - 1) {
        s -= 1;
    }
    let mut e = i;
    while is_word(e + 1) {
        e += 1;
    }
    (s, e)
}

/// Text of a character range, with pdfium's CR LF line breaks as plain newlines.
pub fn range_text(chars: &[TextChar], start: usize, end: usize) -> String {
    let (s, e) = (start.min(end), start.max(end));
    let raw: String = chars.get(s..=e).unwrap_or_default().iter().map(|c| c.ch).collect();
    raw.replace("\r\n", "\n").replace('\r', "\n").trim().to_string()
}

/// The page text under text markup: the characters whose centers fall in its quads.
pub fn markup_text(chars: &[TextChar], quads: &[[Pt; 4]]) -> String {
    let inside = |c: &TextChar| {
        let (x, y) = ((c.rect[0] + c.rect[2]) / 2.0, (c.rect[1] + c.rect[3]) / 2.0);
        quads.iter().any(|q| {
            let (xs, ys) = (q.map(|p| p.x), q.map(|p| p.y));
            let (x0, x1) = (xs.iter().copied().fold(f32::INFINITY, f32::min), xs.iter().copied().fold(f32::NEG_INFINITY, f32::max));
            let (y0, y1) = (ys.iter().copied().fold(f32::INFINITY, f32::min), ys.iter().copied().fold(f32::NEG_INFINITY, f32::max));
            x >= x0 && x <= x1 && y >= y0 && y <= y1
        })
    };
    let text: String = chars.iter().filter(|c| !c.ch.is_control() && inside(c)).map(|c| c.ch).collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One row of the annotations list.
#[derive(Clone, Debug)]
pub struct AnnotEntry {
    pub page: usize,
    /// Its box on the page (display points: x0, y0 top, x1, y1), to scroll to.
    pub bounds: [f32; 4],
    pub sel: Selection,
    pub icon: &'static str,
    pub color: Color32,
    pub title: String,
    pub note: Option<String>,
}

/// Groups the selected characters into one quad per text line.
fn markup_quads(chars: &[TextChar], start: usize, end: usize, g: &PageGeom) -> Vec<[Pt; 4]> {
    let (s, e) = (start.min(end), start.max(end));
    let to_d = g.to_display();
    let mut quads = Vec::new();
    // Current line: user-space rect and display-space vertical extent.
    let mut cur: Option<([f32; 4], f32, f32)> = None;
    let flush = |cur: &mut Option<([f32; 4], f32, f32)>, quads: &mut Vec<[Pt; 4]>| {
        if let Some(([x0, y0, x1, y1], _, _)) = cur.take() {
            quads.push([Pt::new(x0, y1), Pt::new(x1, y1), Pt::new(x0, y0), Pt::new(x1, y0)]);
        }
    };
    for c in chars.get(s..=e).unwrap_or_default() {
        if c.ch.is_control() || c.rect[2] <= c.rect[0] {
            if c.ch == '\n' || c.ch == '\r' {
                flush(&mut cur, &mut quads);
            }
            continue;
        }
        let [x0, y0, x1, y1] = c.rect;
        let a = to_d.apply(Pt::new(x0, y0));
        let b = to_d.apply(Pt::new(x1, y1));
        let (top, bottom) = (a.y.min(b.y), a.y.max(b.y));
        let same_line = cur.is_some_and(|(_, t, bt)| {
            let center = (top + bottom) / 2.0;
            center > t && center < bt
        });
        if !same_line {
            flush(&mut cur, &mut quads);
        }
        cur = Some(match cur {
            Some((r, t, bt)) => ([r[0].min(x0), r[1].min(y0), r[2].max(x1), r[3].max(y1)], t.min(top), bt.max(bottom)),
            None => ([x0, y0, x1, y1], top, bottom),
        });
    }
    flush(&mut cur, &mut quads);
    quads
}

impl App {
    pub fn viewer(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();
        let now = ctx.input(|i| i.time);
        let pages = self.doc.as_ref().map(|d| d.pages.clone()).unwrap_or_default();
        if self.view.viewport == Rect::NOTHING {
            self.view.viewport = ui.available_rect_before_wrap();
        }
        let vp = self.view.viewport;
        let max_w = pages.iter().map(|g| g.display_size().0).fold(1.0, f32::max);
        let max_h = pages.iter().map(|g| g.display_size().1).fold(1.0, f32::max);

        // ---- zoom
        let mut new_zoom: Option<(f32, Pos2)> = None;
        if let Some(fit) = self.view.fit {
            if self.view.fit_applied != Some((fit, vp.size())) {
                self.view.fit_applied = Some((fit, vp.size()));
                let width_zoom = (vp.width() - 2.0 * MARGIN - 14.0) / max_w;
                let z = match fit {
                    Fit::Width => width_zoom,
                    Fit::Page => width_zoom.min((vp.height() - TOP_MARGIN - BOTTOM_MARGIN) / max_h),
                };
                new_zoom = Some((z, pos2(vp.center().x, vp.top())));
            }
        } else {
            self.view.fit_applied = None;
        }
        if let Some(z) = self.view.pending_zoom.take() {
            new_zoom = Some((z, vp.center()));
        }
        let (zoom_delta, hover) = ctx.input(|i| (i.zoom_delta(), i.pointer.hover_pos()));
        if zoom_delta != 1.0
            && let Some(h) = hover.filter(|h| vp.contains(*h)) {
                self.view.fit = None;
                new_zoom = Some((self.view.zoom * zoom_delta, h));
            }
        if let Some((z, anchor)) = new_zoom {
            self.apply_zoom(&pages, z.clamp(0.1, 8.0), anchor, now);
        }
        let lay = layout(&pages, self.view.zoom, vp.width());
        self.navigate(&ctx, &lay, &pages);

        // Dragging belongs to the tools; scroll with the wheel and scroll bars.
        let mut source = egui::scroll_area::ScrollSource::ALL;
        source.drag = egui::scroll_area::ScrollSource::NONE.drag;
        // egui's edge fade uses the panel color, which shows as a band over the darker canvas.
        ui.spacing_mut().scroll.fade.strength = 0.0;
        let wide = lay.size.x > vp.width() + 0.5;
        let mut area = egui::ScrollArea::new([wide, true]).id_salt("pages").auto_shrink(false).scroll_source(source);
        if let Some(o) = self.view.set_offset.take() {
            let o = vec2(o.x.max(0.0), o.y.max(0.0));
            self.view.offset = o;
            area = area.scroll_offset(o);
        }
        let out = area.show_viewport(ui, |ui, viewport| {
            let (rect, resp) = ui.allocate_exact_size(lay.size, Sense::click_and_drag());
            self.canvas(ui, rect.min, viewport, &lay, &resp, ppp, now);
        });
        self.view.offset = out.state.offset;
        self.view.viewport = out.inner_rect;
    }

    fn apply_zoom(&mut self, pages: &[PageGeom], zoom: f32, anchor: Pos2, now: f64) {
        if (zoom - self.view.zoom).abs() < 1e-4 {
            return;
        }
        let vp = self.view.viewport;
        let old = layout(pages, self.view.zoom, vp.width());
        let c = anchor - vp.min + self.view.offset;
        let i = (0..pages.len())
            .min_by(|&a, &b| {
                let d = |k: usize| {
                    let (top, h) = (old.origins[k].y, old.sizes[k].y);
                    if c.y < top { top - c.y } else if c.y > top + h { c.y - top - h } else { 0.0 }
                };
                d(a).total_cmp(&d(b))
            })
            .unwrap_or(0);
        let rel = (c - old.origins[i]) / self.view.zoom;
        self.view.zoom = zoom;
        let new = layout(pages, zoom, vp.width());
        let nc = new.origins[i] + rel * zoom;
        self.view.set_offset = Some(nc - (anchor - vp.min));
        self.view.zoom_changed_at = now;
    }

    /// Keyboard motions and smooth scrolling. Jumps (page keys, h/l, gg/G, search
    /// results) animate towards a target; holding j/k scrolls at a steady speed.
    fn navigate(&mut self, ctx: &egui::Context, lay: &Layout, pages: &[PageGeom]) {
        let vp = self.view.viewport;
        let max = vec2((lay.size.x - vp.width()).max(0.0), (lay.size.y - vp.height()).max(0.0));
        let clamp = |v: Vec2| vec2(v.x.clamp(0.0, max.x), v.y.clamp(0.0, max.y));
        // Further jumps build on a jump still in progress (pressing l three times moves three pages).
        let base = self.view.scroll_target.unwrap_or(self.view.offset);
        let page_top = |i: usize| lay.origins[i].y - TOP_MARGIN;
        let mut target: Option<Vec2> = None;

        if let Some(p) = self.view.goto_page.take().filter(|p| *p < pages.len()) {
            target = Some(vec2(base.x, page_top(p)));
        }
        // A jump keeps its destination in page coordinates and is recomputed every
        // frame until it arrives, so a zoom or window change on the way (e.g. Fit
        // Width after the sidebar closes) doesn't send it to a stale spot.
        let zoom = self.view.zoom;
        let mut jump_target = None;
        if self.view.jump.is_some() {
            let layout = (zoom, vp.size());
            if self.view.jump_layout == Some(layout) {
                self.view.jump_stable = self.view.jump_stable.saturating_add(1);
            } else {
                (self.view.jump_layout, self.view.jump_stable) = (Some(layout), 0);
            }
            ctx.request_repaint();
        } else {
            self.view.jump_layout = None;
        }
        if let Some(j) = self.view.jump.as_mut().filter(|j| j.page < pages.len()) {
            let o = lay.origins[j.page];
            if let JumpX::Reveal(x0, x1) = j.x {
                // Scroll sideways only if the span isn't already in view.
                let (l, r) = (o.x + x0 * zoom, o.x + x1 * zoom);
                let off = self.view.offset.x;
                j.x = if l < off + 16.0 || r > off + vp.width() - 16.0 { JumpX::Center((x0 + x1) / 2.0) } else { JumpX::Keep };
            }
            let x = match j.x {
                JumpX::Center(c) => o.x + c * zoom - vp.width() / 2.0,
                JumpX::Left(l) => o.x + l * zoom,
                JumpX::Keep | JumpX::Reveal(..) => base.x,
            };
            let t = clamp(vec2(x, o.y + j.y * zoom - j.margin));
            if j.animate {
                jump_target = Some(t);
            } else {
                self.view.set_offset = Some(t);
                self.view.scroll_target = None;
                self.view.jump = None;
            }
        }
        if let Some((p, u)) = self.view.reveal.take()
            && let (Some(o), Some(g)) = (lay.origins.get(p), pages.get(p))
        {
            let d = g.to_display().apply(u);
            let (x, y) = (o.x + d.x * self.view.zoom, o.y + d.y * self.view.zoom);
            let mut off = base;
            if x < off.x || x > off.x + vp.width() - 40.0 {
                off.x = x - vp.width() / 3.0;
            }
            off.y = y - vp.height() / 3.0;
            target = Some(off);
        }

        let typing = ctx.egui_wants_keyboard_input() || self.editing.is_some();
        if !typing {
            let (pg_up, pg_dn, home, end) = ctx.input(|i| {
                (
                    i.key_pressed(Key::PageUp),
                    i.key_pressed(Key::PageDown),
                    i.key_pressed(Key::Home),
                    i.key_pressed(Key::End),
                )
            });
            let screen = vp.height() * 0.9;
            let motion = self.view.motion.take();
            let y = match motion {
                _ if pg_up => Some(base.y - screen),
                _ if pg_dn => Some(base.y + screen),
                _ if home => Some(0.0),
                _ if end => Some(max.y),
                Some(Motion::Top) => Some(0.0),
                Some(Motion::Bottom) => Some(max.y),
                Some(Motion::HalfDown) => Some(base.y + vp.height() / 2.0),
                Some(Motion::HalfUp) => Some(base.y - vp.height() / 2.0),
                Some(Motion::NextPage) => {
                    Some((0..pages.len()).map(page_top).find(|&t| t > base.y + 1.0).unwrap_or(max.y))
                }
                Some(Motion::PrevPage) => {
                    Some((0..pages.len()).map(page_top).rev().find(|&t| t < base.y - 1.0).unwrap_or(0.0))
                }
                None => None,
            };
            if let Some(y) = y {
                target = Some(vec2(base.x, y));
            }
        }
        // Any other navigation replaces a jump under way.
        if let Some(t) = target {
            self.view.scroll_target = Some(clamp(t));
            self.view.jump = None;
        } else if let Some(t) = jump_target {
            self.view.scroll_target = Some(t);
        }

        let dt = ctx.input(|i| i.stable_dt).min(1.0 / 20.0);
        // Wheel or scrollbar input cancels an animated jump.
        if ctx.input(|i| i.smooth_scroll_delta != Vec2::ZERO) {
            self.view.scroll_target = None;
            self.view.jump = None;
        }
        // Holding j / k or the arrow keys: continuous, frame-rate independent
        // scrolling. With annotations selected, the arrows nudge them instead.
        let nudging = self.tool == Tool::Select
            && matches!(self.selection, Some(crate::app::Selection::Ours(_) | crate::app::Selection::Many(_)));
        let dir = ctx.input(|i| {
            let plain = !i.modifiers.any();
            let held = |k: Key| plain && i.key_down(k);
            let arrows = plain && !nudging;
            let axis = |plus: bool, minus: bool| (plus as i32 - minus as i32) as f32;
            vec2(
                axis(arrows && held(Key::ArrowRight), arrows && held(Key::ArrowLeft)),
                axis(held(Key::J) || (arrows && held(Key::ArrowDown)), held(Key::K) || (arrows && held(Key::ArrowUp))),
            )
        });
        if !typing && dir != Vec2::ZERO {
            self.view.scroll_target = None;
            self.view.jump = None;
            self.view.set_offset = Some(clamp(self.view.offset + dir * 1100.0 * dt));
            ctx.request_repaint();
        } else if let Some(t) = self.view.scroll_target {
            let cur = self.view.offset;
            let next = cur + (t - cur) * (1.0 - (-dt * 22.0).exp());
            if (t - next).length() < 0.5 {
                self.view.set_offset = Some(t);
                self.view.scroll_target = None;
                // Arrived; but if the layout is still changing (e.g. the sidebar just
                // closed), keep the jump so it's recomputed in the new layout.
                if self.view.jump_stable >= 2 {
                    self.view.jump = None;
                }
            } else {
                self.view.set_offset = Some(next);
                ctx.request_repaint();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn canvas(
        &mut self,
        ui: &mut egui::Ui,
        origin: Pos2,
        viewport: Rect,
        lay: &Layout,
        resp: &egui::Response,
        ppp: f32,
        now: f64,
    ) {
        let painter = ui.painter().clone();
        let max_side = ui.ctx().input(|i| i.max_texture_side);
        let Some(doc) = &self.doc else { return };
        let pages = doc.pages.clone();
        let generation = doc.generation;
        let n = pages.len();

        // Pixel-align pages so textures map 1:1 to screen pixels.
        self.view.page_rects = (0..n)
            .map(|i| {
                let min = origin + lay.origins[i];
                let min = pos2((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp);
                Rect::from_min_size(min, lay.sizes[i])
            })
            .collect();
        let vis_area = viewport.expand2(vec2(0.0, viewport.height() * 0.5));
        let visible: Vec<usize> = (0..n)
            .filter(|&i| Rect::from_min_size(lay.origins[i].to_pos2(), lay.sizes[i]).intersects(vis_area))
            .collect();
        let probe = viewport.top() + viewport.height() * 0.3;
        self.view.current_page = (0..n)
            .find(|&i| lay.origins[i].y + lay.sizes[i].y + GAP > probe)
            .unwrap_or(0);

        let page_shadow = egui::Shadow {
            offset: [0, 2],
            blur: 12,
            spread: 0,
            color: Color32::from_black_alpha(if ui.visuals().dark_mode { 120 } else { 40 }),
        };
        self.view.sel_anchor = None;
        self.view.annot_rect = None;
        let settled = now - self.view.zoom_changed_at > 0.15;
        if !settled {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(160));
        }
        let exclude: Vec<String> = match &self.gesture {
            Gesture::Edit { before, .. } => before.iter().map(|a| a.id.clone()).collect(),
            _ => Vec::new(),
        };

        for &i in &visible {
            let rect = self.view.page_rects[i];
            painter.add(page_shadow.as_shape(rect, 0));
            painter.rect_filled(rect, 0.0, Color32::WHITE);
            let want = render_scale(&pages[i], self.view.zoom, ppp, max_side);
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            let need = match self.tex.get(&i) {
                Some(t) => {
                    painter.image(t.tex.id(), rect, uv, Color32::WHITE);
                    t.generation != generation || ((t.scale - want).abs() > 0.005 && settled)
                }
                None => true,
            };
            if need {
                self.request_render(i, want);
            }
            self.paint_overlay(&painter, i, rect, want, settled, &exclude, ui.ctx());
            self.paint_texts(&painter, i);
            self.paint_note_badges(&painter, i);
            self.paint_search(&painter, i, &self.view.to_screen(&pages[i], i));
            self.paint_text_selection(&painter, i, &pages[i]);
        }
        // Keep textures only around the visible pages.
        if let (Some(&first), Some(&last)) = (visible.first(), visible.last()) {
            let keep = first.saturating_sub(2)..=last + 2;
            self.tex.retain(|p, _| keep.contains(p));
            self.overlay.retain(|p, _| keep.contains(p));
        }

        self.handle_input(ui, resp);
        self.paint_selection(&painter);
        self.paint_marquee(&painter);
        self.paint_live(&painter, ui.ctx(), ppp);
        self.text_editor(ui);
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_overlay(
        &mut self,
        painter: &Painter,
        page: usize,
        rect: Rect,
        want: f32,
        settled: bool,
        exclude: &[String],
        ctx: &egui::Context,
    ) {
        let Some(doc) = &self.doc else { return };
        let annots: Vec<&Annotation> = doc
            .annots
            .iter()
            .filter(|a| a.page == page && !exclude.contains(&a.id) && !matches!(a.kind, Kind::Text { .. }))
            .collect();
        if annots.is_empty() {
            self.overlay.remove(&page);
            return;
        }
        let rev = doc.page_rev.get(page).copied().unwrap_or(0);
        let fresh = self.overlay.get(&page).is_some_and(|o| {
            o.rev == rev && o.exclude == exclude && ((o.scale - want).abs() < 0.005 || !settled)
        });
        if !fresh {
            let g = &doc.pages[page];
            let (w, h) = g.display_size();
            let (pw, ph) = ((w * want).ceil() as u32, (h * want).ceil() as u32);
            if let Some(mut pm) = tiny_skia::Pixmap::new(pw.max(1), ph.max(1)) {
                let t = g.to_display().then_scale_translate(want, 0.0, 0.0).to_skia();
                for a in &annots {
                    raster::draw(&mut pm.as_mut(), a, t);
                }
                let image = egui::ColorImage::from_rgba_premultiplied([pw as usize, ph as usize], pm.data());
                let opts = egui::TextureOptions::LINEAR;
                match self.overlay.get_mut(&page) {
                    Some(o) => {
                        o.tex.set(image, opts);
                        o.scale = want;
                        o.rev = rev;
                        o.exclude = exclude.to_vec();
                    }
                    None => {
                        let tex = ctx.load_texture(format!("overlay{page}"), image, opts);
                        self.overlay.insert(page, Overlay { tex, scale: want, rev, exclude: exclude.to_vec() });
                    }
                }
            }
        }
        if let Some(o) = self.overlay.get(&page) {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(o.tex.id(), rect, uv, Color32::WHITE);
        }
    }

    fn paint_texts(&self, painter: &Painter, page: usize) {
        let Some(doc) = &self.doc else { return };
        let g = &doc.pages[page];
        let aff = self.view.to_screen(g, page);
        let editing = self.editing.as_ref().and_then(|e| e.id.as_deref());

        for a in doc.annots.iter().filter(|a| a.page == page) {
            if Some(a.id.as_str()) == editing {
                continue;
            }
            let a = self.edited(&a.id).unwrap_or(a);
            if let Kind::Text { origin, right, down, text } = &a.kind {
                draw_text(painter, &aff, self.view.zoom, *origin, *right, *down, text, &a.style);
            }
        }
    }

    /// Annotation `id` as it currently looks while being dragged, if it is.
    fn edited(&self, id: &str) -> Option<&Annotation> {
        match &self.gesture {
            Gesture::Edit { current, .. } => current.iter().find(|a| a.id == id),
            _ => None,
        }
    }

    /// Annotation `id` as currently shown (dragged or not).
    fn shown(&self, id: &str) -> Option<&Annotation> {
        self.edited(id).or_else(|| self.doc.as_ref()?.get(id))
    }

    /// Our selected annotation, when exactly one is selected.
    fn selected_ours(&self) -> Option<&Annotation> {
        let Some(Selection::Ours(id)) = &self.selection else { return None };
        self.shown(id)
    }

    /// Ids of all our selected annotations.
    pub fn selected_ids(&self) -> Vec<String> {
        match &self.selection {
            Some(Selection::Ours(id)) => vec![id.clone()],
            Some(Selection::Many(ids)) => ids.clone(),
            _ => Vec::new(),
        }
    }

    /// Selects our annotations `ids`: none, one (with handles and note), or a group.
    pub fn select_ids(&mut self, mut ids: Vec<String>) {
        ids.dedup();
        self.select(match ids.len() {
            0 => None,
            1 => ids.pop().map(Selection::Ours),
            _ => Some(Selection::Many(ids)),
        });
    }

    /// Our annotations touched by the box `a`-`b` on `page` (user space).
    fn in_box(&self, page: usize, a: Pt, b: Pt) -> Vec<String> {
        let Some(doc) = &self.doc else { return Vec::new() };
        let (x0, y0, x1, y1) = (a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y));
        doc.annots
            .iter()
            .filter(|an| an.page == page)
            .filter(|an| {
                let [ax0, ay0, ax1, ay1] = geometry::bounds(an);
                ax0 <= x1 && ax1 >= x0 && ay0 <= y1 && ay1 >= y0
            })
            .map(|an| an.id.clone())
            .collect()
    }

    /// Screen bounds of a foreign annotation.
    fn foreign_rect(&self, i: usize) -> Option<Rect> {
        let doc = self.doc.as_ref()?;
        let f = doc.foreign.get(i)?;
        let page = doc.foreign_page(i).filter(|&p| p < doc.pages.len())?;
        let aff = self.view.to_screen(&doc.pages[page], page);
        let [x0, y0, x1, y1] = f.rect;
        Some(Rect::from_two_pos(pos(aff.apply(Pt::new(x0, y0))), pos(aff.apply(Pt::new(x1, y1)))))
    }

    /// Grab points of the selected annotation (screen space). Lines and arrows are
    /// changed by their ends; text markup follows the text, so it has none.
    fn handles(&self, a: &Annotation) -> Vec<(Handle, Pos2)> {
        let Some(doc) = &self.doc else { return Vec::new() };
        let aff = self.view.to_screen(&doc.pages[a.page], a.page);
        let outlined = screen_bounds(a, &aff).expand(4.0);
        let mut handles: Vec<(Handle, Pos2)> = match &a.kind {
            Kind::Markup { .. } => return Vec::new(),
            Kind::Shape { shape: ShapeKind::Line | ShapeKind::Arrow, a: p, b: q, .. } => {
                vec![(Handle::Endpoint(0), pos(aff.apply(*p))), (Handle::Endpoint(1), pos(aff.apply(*q)))]
            }
            _ if a.is_box() && a.angle != 0.0 => geometry::box_corners(a)
                .into_iter()
                .enumerate()
                .map(|(i, c)| (Handle::BoxCorner(i), pos(aff.apply(c))))
                .collect(),
            _ => [outlined.left_top(), outlined.right_top(), outlined.left_bottom(), outlined.right_bottom()]
                .into_iter()
                .enumerate()
                .map(|(i, c)| (Handle::Corner(i), c))
                .collect(),
        };
        handles.push((Handle::Rotate, pos2(outlined.center().x, outlined.top() - ROTATE_HANDLE_GAP)));
        handles
    }

    /// Handle of the selected annotation under a screen point (Select tool only).
    fn handle_at(&self, p: Pos2) -> Option<(Annotation, Handle)> {
        if self.tool != Tool::Select {
            return None;
        }
        let a = self.selected_ours()?;
        let reach = HANDLE_SIZE / 2.0 + 3.0;
        self.handles(a)
            .into_iter()
            .filter(|(_, c)| c.distance(p) <= reach)
            .min_by(|x, y| x.1.distance(p).total_cmp(&y.1.distance(p)))
            .map(|(h, _)| (a.clone(), h))
    }

    /// Starts dragging handle `h` of `a`, pressed at screen point `p`.
    fn start_handle_drag(&mut self, a: Annotation, h: Handle, p: Pos2) {
        let Some(g) = self.doc.as_ref().and_then(|d| d.pages.get(a.page)).copied() else { return };
        let op = match h {
            Handle::Endpoint(i) => EditOp::Endpoint(i),
            Handle::Rotate => {
                let center = a.center();
                let d = self.view.to_user(&g, a.page, p).sub(center);
                EditOp::Rotate { center, start: d.y.atan2(d.x) }
            }
            Handle::BoxCorner(i) => {
                let corners = geometry::box_corners(&a);
                EditOp::ScaleBox { anchor: corners[(i + 2) % 4], corner: corners[i], i }
            }
            Handle::Corner(i) => {
                let Some(doc) = &self.doc else { return };
                let r = screen_bounds(&a, &self.view.to_screen(&doc.pages[a.page], a.page));
                let corners = [r.left_top(), r.right_top(), r.left_bottom(), r.right_bottom()];
                let disp = |q: Pos2| self.view.to_display(a.page, q);
                EditOp::Scale { anchor: disp(corners[3 - i]), corner: disp(corners[i]), start: disp(p) }
            }
        };
        self.gesture = Gesture::Edit { before: vec![a.clone()], current: vec![a], op };
    }

    fn paint_selection(&mut self, painter: &Painter) {
        let accent = crate::ui::theme::ACCENT;
        let handle_stroke = Stroke::new(1.25, accent);
        match &self.selection {
            Some(Selection::Ours(_)) => {
                let Some(a) = self.selected_ours() else { return };
                let Some(doc) = &self.doc else { return };
                let aff = self.view.to_screen(&doc.pages[a.page], a.page);
                let outlined = screen_bounds(a, &aff).expand(4.0);
                let is_line = matches!(a.kind, Kind::Shape { shape: ShapeKind::Line | ShapeKind::Arrow, .. });
                if a.is_box() && a.angle != 0.0 {
                    // The rotated box itself.
                    let mut pts: Vec<Pos2> = geometry::box_corners(a).iter().map(|c| pos(aff.apply(*c))).collect();
                    pts.push(pts[0]);
                    painter.line(pts, handle_stroke);
                } else if !is_line {
                    painter.rect_stroke(outlined, 2.0, handle_stroke, StrokeKind::Middle);
                }
                let mut area = outlined;
                for (h, c) in self.handles(a) {
                    match h {
                        Handle::Rotate => {
                            painter.line_segment([pos2(c.x, outlined.top()), c], Stroke::new(1.0, accent));
                            painter.circle(c, HANDLE_SIZE / 2.0 + 1.0, Color32::WHITE, handle_stroke);
                            area = area.union(Rect::from_center_size(c, Vec2::splat(HANDLE_SIZE)));
                        }
                        Handle::Corner(_) | Handle::BoxCorner(_) => {
                            let hr = Rect::from_center_size(c, Vec2::splat(HANDLE_SIZE));
                            painter.rect_filled(hr, 1.5, Color32::WHITE);
                            painter.rect_stroke(hr, 1.5, handle_stroke, StrokeKind::Middle);
                        }
                        Handle::Endpoint(_) => {
                            painter.circle(c, HANDLE_SIZE / 2.0 + 0.5, Color32::WHITE, handle_stroke);
                        }
                    }
                }
                self.view.annot_rect = Some(area);
            }
            Some(Selection::Many(ids)) => {
                // A group: each one outlined, no handles (groups are moved, not resized).
                let Some(doc) = &self.doc else { return };
                let mut all: Option<Rect> = None;
                for a in ids.iter().filter_map(|id| self.shown(id)) {
                    let r = screen_bounds(a, &self.view.to_screen(&doc.pages[a.page], a.page)).expand(3.0);
                    painter.rect_stroke(r, 2.0, handle_stroke, StrokeKind::Middle);
                    all = Some(all.map_or(r, |u| u.union(r)));
                }
                self.view.annot_rect = all.map(|r| r.expand(1.0));
            }
            Some(Selection::Foreign(i)) => {
                // Other apps' annotations: dashed, without handles (they can't be changed).
                if let Some(r) = self.foreign_rect(*i) {
                    let r = r.expand(4.0);
                    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
                    painter.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.5, crate::ui::theme::WARN), 5.0, 3.0));
                }
            }
            None => {}
        }
    }

    fn paint_marquee(&self, painter: &Painter) {
        let (Gesture::Marquee { page, start, end, .. }, Some(doc)) = (&self.gesture, &self.doc) else { return };
        let aff = self.view.to_screen(&doc.pages[*page], *page);
        let r = Rect::from_two_pos(pos(aff.apply(*start)), pos(aff.apply(*end)));
        let accent = crate::ui::theme::ACCENT;
        painter.rect_filled(r, 0.0, accent.gamma_multiply(0.08));
        painter.rect_stroke(r, 0.0, Stroke::new(1.0, accent.gamma_multiply(0.9)), StrokeKind::Middle);
    }

    /// Note badges on one page: ours and other apps' annotations that carry a note.
    fn note_badges(&self, page: usize) -> Vec<(Pos2, Selection, String)> {
        let Some(doc) = &self.doc else { return Vec::new() };
        let aff = self.view.to_screen(&doc.pages[page], page);
        let ours = doc.annots.iter().filter(|a| a.page == page && !a.note.is_empty()).map(|a| {
            let a = self.edited(&a.id).unwrap_or(a);
            (badge_center(screen_bounds(a, &aff)), Selection::Ours(a.id.clone()), a.note.clone())
        });
        let foreign = doc.foreign.iter().enumerate().filter_map(|(i, f)| {
            let here = doc.foreign_page(i) == Some(page);
            let note = f.note.as_ref().filter(|_| here && f.selectable && !doc.deleted_foreign.contains(&i))?;
            Some((badge_center(self.foreign_rect(i)?), Selection::Foreign(i), note.clone()))
        });
        ours.chain(foreign).collect()
    }

    fn paint_note_badges(&self, painter: &Painter, page: usize) {
        let open = self.note_edit.as_ref().map(|n| n.id.as_str());
        for (c, sel, _) in self.note_badges(page) {
            let foreign = matches!(sel, Selection::Foreign(_));
            let fill = if foreign { crate::ui::theme::WARN } else { crate::ui::theme::ACCENT };
            let editing = matches!(&sel, Selection::Ours(id) if Some(id.as_str()) == open);
            painter.circle_filled(c + vec2(0.0, 1.0), NOTE_BADGE_RADIUS, Color32::from_black_alpha(40));
            painter.circle(c, NOTE_BADGE_RADIUS, fill, Stroke::new(if editing { 2.0 } else { 1.0 }, Color32::WHITE));
            painter.text(
                c,
                egui::Align2::CENTER_CENTER,
                egui_phosphor::regular::CHAT_TEXT,
                FontId::proportional(NOTE_BADGE_RADIUS * 1.3),
                Color32::WHITE,
            );
        }
    }

    /// The note badge under a screen point.
    fn badge_at(&self, p: Pos2) -> Option<(Selection, String)> {
        let page = self.view.page_at(p, false)?;
        // A badge can stick out past the page edge, so check the neighbours too.
        (page.saturating_sub(1)..=page + 1)
            .filter(|&i| i < self.view.page_rects.len())
            .flat_map(|i| self.note_badges(i))
            .find(|(c, _, _)| c.distance(p) <= NOTE_BADGE_RADIUS + 2.0)
            .map(|(_, sel, note)| (sel, note))
    }

    /// Note to show when hovering `p`: a badge, or (Select tool) an annotation with a note.
    fn note_at(&self, p: Pos2) -> Option<String> {
        if let Some((_, note)) = self.badge_at(p) {
            return Some(note);
        }
        if self.tool != Tool::Select {
            return None;
        }
        let doc = self.doc.as_ref()?;
        match self.hit(p)? {
            Selection::Ours(id) => Some(doc.get(&id)?.note.clone()).filter(|n| !n.is_empty()),
            Selection::Foreign(i) => doc.foreign.get(i)?.note.clone(),
            Selection::Many(_) => None,
        }
    }

    /// Rasterizes in-progress annotations with tiny-skia each frame, so they look
    /// exactly like the final result.
    fn paint_live(&mut self, painter: &Painter, ctx: &egui::Context, ppp: f32) {
        let Some(doc) = &self.doc else { return };
        let live: Option<Annotation> = match &self.gesture {
            Gesture::Ink { page, raw, highlighter, style, .. } if !raw.is_empty() => Some(Annotation {
                id: String::new(),
                page: *page,
                style: *style,
                kind: Kind::Ink { curve: catmull_rom(raw), highlighter: *highlighter },
                note: String::new(),
                angle: 0.0,
            }),
            Gesture::Shape { page, shape, a, b, style } => {
                let (a, b) = shape_box(&doc.pages[*page], *shape, *a, *b, style, self.view.zoom);
                Some(Annotation {
                    id: String::new(),
                    page: *page,
                    style: *style,
                    kind: Kind::Shape { shape: *shape, a, b },
                    note: String::new(),
                    angle: 0.0,
                })
            }
            Gesture::Markup { page, markup, start, end, style } => {
                let chars = self.text_chars.get(page).map(Vec::as_slice).unwrap_or_default();
                let quads = markup_quads(chars, *start, *end, &doc.pages[*page]);
                (!quads.is_empty()).then(|| Annotation {
                    id: String::new(),
                    page: *page,
                    style: *style,
                    kind: Kind::Markup { markup: *markup, quads },
                    note: String::new(),
                    angle: 0.0,
                })
            }
            _ => None,
        };
        // Everything being drawn or dragged, rasterized together into one texture.
        let live: Vec<Annotation> = match &self.gesture {
            Gesture::Edit { current, .. } => {
                current.iter().filter(|a| !matches!(a.kind, Kind::Text { .. })).cloned().collect()
            }
            _ => live.into_iter().collect(),
        };
        let affs: Vec<Affine> = live.iter().map(|a| self.view.to_screen(&doc.pages[a.page], a.page)).collect();
        let Some(bounds) = live.iter().zip(&affs).map(|(a, aff)| screen_bounds(a, aff)).reduce(|x, y| x.union(y)) else {
            return;
        };
        let bbox = bounds.expand(2.0).intersect(painter.clip_rect());
        if !bbox.is_positive() {
            return;
        }
        let (pw, ph) = ((bbox.width() * ppp).ceil() as u32, (bbox.height() * ppp).ceil() as u32);
        let Some(mut pm) = tiny_skia::Pixmap::new(pw.max(1), ph.max(1)) else { return };
        for (a, aff) in live.iter().zip(&affs) {
            let t = Affine {
                a: aff.a * ppp,
                b: aff.b * ppp,
                c: aff.c * ppp,
                d: aff.d * ppp,
                e: (aff.e - bbox.min.x) * ppp,
                f: (aff.f - bbox.min.y) * ppp,
            };
            raster::draw(&mut pm.as_mut(), a, t.to_skia());
        }
        let image = egui::ColorImage::from_rgba_premultiplied([pw as usize, ph as usize], pm.data());
        let opts = egui::TextureOptions::LINEAR;
        let tex = match &mut self.live_tex {
            Some(t) => {
                t.set(image, opts);
                t
            }
            None => self.live_tex.insert(ctx.load_texture("live", image, opts)),
        };
        let rect = Rect::from_min_size(bbox.min, vec2(pw as f32, ph as f32) / ppp);
        painter.image(tex.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    }

    // ------------------------------------------------------------- input

    fn handle_input(&mut self, ui: &egui::Ui, resp: &egui::Response) {
        let ctx = ui.ctx();
        let (events, space) = ctx.input(|i| (i.events.clone(), i.key_down(Key::Space)));
        let panning_key = space && !ctx.egui_wants_keyboard_input();
        let hovered = resp.hovered();

        for ev in events {
            match ev {
                Event::PointerButton { pos, button, pressed: true, modifiers } if hovered => {
                    if button == PointerButton::Extra1 {
                        self.go_back();
                    } else if button == PointerButton::Primary && !panning_key && self.tool == Tool::Hand
                        && let Some((target, page)) = self.link_at(pos)
                    {
                        self.commit_edits();
                        self.gesture = Gesture::Link { target, start: pos, page, char: None };
                    } else if button == PointerButton::Middle || (button == PointerButton::Primary && (panning_key || self.tool == Tool::Hand)) {
                        self.commit_edits();
                        self.gesture = Gesture::Pan { last: pos };
                    } else if button == PointerButton::Primary {
                        self.on_press(ui, pos, modifiers);
                    }
                }
                Event::PointerMoved(pos) => self.on_move(pos, ctx.input(|i| i.modifiers)),
                Event::PointerButton { pos, pressed: false, .. } => self.on_release(ctx, pos),
                Event::PointerGone => {
                    // Leaving the window is not a click on a link.
                    if matches!(self.gesture, Gesture::Link { .. }) {
                        self.gesture = Gesture::None;
                    }
                    self.on_release(ctx, Pos2::ZERO)
                }
                _ => {}
            }
        }
        if !matches!(self.gesture, Gesture::None) {
            ctx.request_repaint();
        }

        // Cursor and eraser preview.
        if hovered || !matches!(self.gesture, Gesture::None) {
            let icon = match (&self.gesture, self.tool) {
                (Gesture::Pan { .. }, _) => CursorIcon::Grabbing,
                (Gesture::Link { .. }, _) => CursorIcon::PointingHand,
                (Gesture::Marquee { .. }, _) => CursorIcon::Crosshair,
                (Gesture::None, Tool::Hand) if !panning_key && resp.hover_pos().is_some_and(|p| self.link_at(p).is_some()) => {
                    CursorIcon::PointingHand
                }
                _ if panning_key || self.tool == Tool::Hand => CursorIcon::Grab,
                (Gesture::Edit { op: EditOp::Move { .. }, .. }, _) => CursorIcon::Grabbing,
                (Gesture::Edit { op: EditOp::Endpoint(_), .. }, _) => CursorIcon::Crosshair,
                (Gesture::Edit { op: EditOp::Rotate { .. }, .. }, _) => CursorIcon::Grabbing,
                (Gesture::Edit { op: EditOp::ScaleBox { .. }, .. }, _) => CursorIcon::Move,
                (Gesture::Edit { op: EditOp::Scale { anchor, corner, .. }, .. }, _) => {
                    // Same diagonal as when the drag started (screen and display axes agree).
                    if (corner.x - anchor.x) * (corner.y - anchor.y) > 0.0 {
                        CursorIcon::ResizeNwSe
                    } else {
                        CursorIcon::ResizeNeSw
                    }
                }
                _ if resp.hover_pos().is_some_and(|p| self.badge_at(p).is_some()) => CursorIcon::PointingHand,
                (Gesture::Markup { .. }, _) => CursorIcon::Text,
                (_, Tool::Highlighter) => match resp.hover_pos() {
                    Some(p) if self.over_text(p) && !ctx.input(|i| i.modifiers.alt) => CursorIcon::Text,
                    _ => CursorIcon::Crosshair,
                },
                (_, Tool::Pen | Tool::Shape(_)) => CursorIcon::Crosshair,
                (_, Tool::Text | Tool::Markup(_)) => CursorIcon::Text,
                (_, Tool::Eraser) => CursorIcon::None,
                (Gesture::SelectText { .. }, _) => CursorIcon::Text,
                (_, Tool::Select) => match resp.hover_pos() {
                    Some(p) if self.handle_at(p).is_some() => match self.handle_at(p).map(|(_, h)| h) {
                        Some(Handle::Corner(0 | 3)) => CursorIcon::ResizeNwSe,
                        Some(Handle::Corner(_)) => CursorIcon::ResizeNeSw,
                        Some(Handle::BoxCorner(_)) => CursorIcon::Move,
                        Some(Handle::Rotate) => CursorIcon::Grab,
                        _ => CursorIcon::Crosshair,
                    },
                    Some(p) if self.hit(p).is_some() => CursorIcon::Move,
                    Some(p) if self.link_at(p).is_some() => CursorIcon::PointingHand,
                    Some(p) if self.over_text(p) => CursorIcon::Text,
                    _ => CursorIcon::Default,
                },
                _ => CursorIcon::Default,
            };
            ctx.set_cursor_icon(icon);
            if self.tool == Tool::Eraser && !panning_key
                && let Some(p) = resp.hover_pos().or(ctx.pointer_latest_pos()) {
                    let r = self.tool_style(Tool::Eraser).width / 2.0;
                    ui.painter().circle(p, r, Color32::from_white_alpha(60), Stroke::new(1.0, Color32::from_gray(90)));
                }
        }
        if matches!(self.tool, Tool::Markup(_) | Tool::Select | Tool::Highlighter)
            && let Some(p) = resp.hover_pos().and_then(|p| self.view.page_at(p, true)) {
                self.request_text(p);
            }
        // Hovering a note badge (or, with Select, an annotation with a note) shows the
        // note; hovering a link shows where it goes.
        let tip = resp.hover_pos().filter(|_| matches!(self.gesture, Gesture::None) && self.note_edit.is_none()).and_then(|p| {
            self.note_at(p).or_else(|| {
                let follows = matches!(self.tool, Tool::Select | Tool::Hand) && (self.tool == Tool::Hand || self.hit(p).is_none());
                self.link_at(p).filter(|_| follows && !panning_key).map(|(t, _)| match t {
                    Target::Page { page, .. } => format!("Go to page {}", page + 1),
                    Target::Uri(uri) => uri,
                })
            })
        });
        if let Some(note) = tip {
            resp.clone().on_hover_ui_at_pointer(|ui| {
                ui.set_max_width(320.0);
                ui.label(note);
            });
        }
    }

    /// The link under a screen point, and its page.
    fn link_at(&self, p: Pos2) -> Option<(Target, usize)> {
        let doc = self.doc.as_ref()?;
        let page = self.view.page_at(p, true)?;
        let u = self.view.to_user(&doc.pages[page], page, p);
        doc.links
            .get(page)?
            .iter()
            .find(|l| u.x >= l.rect[0] && u.x <= l.rect[2] && u.y >= l.rect[1] && u.y <= l.rect[3])
            .map(|l| (l.target.clone(), page))
    }

    fn over_text(&self, p: Pos2) -> bool {
        let (Some(doc), Some(page)) = (&self.doc, self.view.page_at(p, true)) else { return false };
        let u = self.view.to_user(&doc.pages[page], page, p);
        self.text_chars.get(&page).is_some_and(|c| char_at(c, u, 2.0 / self.view.zoom).is_some())
    }

    /// Copyable text of the current selection.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.text_sel?;
        Some(range_text(self.text_chars.get(&sel.page)?, sel.start, sel.end))
    }

    /// Quads (user space) of the current text selection.
    pub fn selection_quads(&self) -> Option<(usize, Vec<[Pt; 4]>)> {
        let sel = self.text_sel?;
        let doc = self.doc.as_ref()?;
        let chars = self.text_chars.get(&sel.page)?;
        Some((sel.page, markup_quads(chars, sel.start, sel.end, doc.pages.get(sel.page)?)))
    }

    fn paint_text_selection(&mut self, painter: &Painter, page: usize, g: &PageGeom) {
        let Some((p, quads)) = self.selection_quads().filter(|(p, _)| *p == page) else { return };
        let aff = self.view.to_screen(g, p);
        let fill = crate::ui::theme::ACCENT.gamma_multiply(0.3);
        let mut top: Option<Rect> = None;
        for q in &quads {
            let r = Rect::from_points(&q.map(|c| pos(aff.apply(c))));
            painter.rect_filled(r, 1.0, fill);
            top = Some(top.map_or(r, |t| if r.top() < t.top() { r } else { t }));
        }
        // Where the floating copy/highlight bar goes: just above the first line.
        self.view.sel_anchor = top.map(|r| pos2(r.center().x, r.top() - 6.0));
    }

    /// Every annotation (ours and other apps'), by page and position, for the list.
    pub fn annotation_entries(&self) -> Vec<AnnotEntry> {
        let Some(doc) = &self.doc else { return Vec::new() };
        let shown = |page: usize, r: [f32; 4]| {
            let g = doc.pages[page].to_display();
            let pts = [(r[0], r[1]), (r[2], r[1]), (r[0], r[3]), (r[2], r[3])].map(|(x, y)| g.apply(Pt::new(x, y)));
            let (xs, ys) = (pts.map(|p| p.x), pts.map(|p| p.y));
            let min = |v: [f32; 4]| v.iter().copied().fold(f32::INFINITY, f32::min);
            let max = |v: [f32; 4]| v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            [min(xs), min(ys), max(xs), max(ys)]
        };
        let mut out: Vec<AnnotEntry> = doc
            .annots
            .iter()
            .filter(|a| a.page < doc.pages.len())
            .map(|a| {
                let tool = crate::app::tool_of(a);
                let title = match &a.kind {
                    Kind::Markup { quads, .. } => match self.text_chars.get(&a.page).map(|c| markup_text(c, quads)) {
                        Some(t) if !t.is_empty() => format!("“{t}”"),
                        _ => tool.label().to_string(),
                    },
                    Kind::Text { text, .. } => text.lines().next().unwrap_or_default().to_string(),
                    _ => tool.label().to_string(),
                };
                AnnotEntry {
                    page: a.page,
                    bounds: shown(a.page, geometry::bounds(a)),
                    sel: Selection::Ours(a.id.clone()),
                    icon: crate::ui::chrome::tool_icon(tool),
                    color: crate::app::rgb(a.style.color),
                    title,
                    note: (!a.note.is_empty()).then(|| a.note.clone()),
                }
            })
            .collect();
        out.extend(doc.foreign.iter().enumerate().filter_map(|(i, f)| {
            let page = doc.foreign_page(i).filter(|&p| p < doc.pages.len() && f.selectable && !doc.deleted_foreign.contains(&i))?;
            Some(AnnotEntry {
                page,
                bounds: shown(page, f.rect),
                sel: Selection::Foreign(i),
                icon: egui_phosphor::regular::INFO,
                color: crate::ui::theme::WARN,
                title: match &f.text {
                    Some(t) => format!("{}: {}  · other app", f.kind_name(), t.lines().next().unwrap_or_default()),
                    None => format!("{}  · other app", f.kind_name()),
                },
                note: f.note.clone(),
            })
        }));
        out.sort_by(|a, b| a.page.cmp(&b.page).then(a.bounds[1].total_cmp(&b.bounds[1])));
        out
    }

    /// Scrolls to an annotation's box `bounds` (display points) on both axes (Back
    /// returns) and selects it, finishing any text or note being typed first.
    pub fn go_to_annotation(&mut self, page: usize, bounds: [f32; 4], sel: Selection) {
        self.commit_edits();
        self.view.push_back();
        let [x0, y0, x1, _] = bounds;
        self.view.jump = Some(Jump { page, y: y0.max(0.0), margin: 80.0, x: JumpX::Reveal(x0, x1), animate: true });
        if self.tool != Tool::Select {
            self.set_tool(Tool::Select);
        }
        self.text_sel = None;
        self.select(Some(sel));
    }

    /// Where pasted `items` go: the page in the middle of the window, and how far
    /// (user space) to move them there. They keep their place if it's in view,
    /// else they're centered in the visible part of the page.
    pub(crate) fn paste_target(&self, items: &[Annotation]) -> Option<(usize, Pt)> {
        let doc = self.doc.as_ref()?;
        let vp = self.view.viewport;
        let page = self.view.page_at(vp.center(), false).unwrap_or(self.view.current_page);
        let g = *doc.pages.get(page)?;
        let Some(shown) = self.view.page_rects.get(page).map(|r| r.intersect(vp)).filter(|r| r.is_positive()) else {
            return Some((page, Pt::default()));
        };
        let aff = self.view.to_screen(&g, page);
        let mut group = Rect::NOTHING;
        for a in items {
            let [x0, y0, x1, y1] = geometry::bounds(a);
            group.extend_with(pos(aff.apply(Pt::new(x0, y0))));
            group.extend_with(pos(aff.apply(Pt::new(x1, y1))));
        }
        if shown.contains_rect(group) {
            return Some((page, Pt::default()));
        }
        let shift = self.view.to_user(&g, page, shown.center()).sub(self.view.to_user(&g, page, group.center()));
        Some((page, shift))
    }

    /// Turns the text selection into a highlight / underline / strike-out annotation.
    pub fn markup_selection(&mut self, markup: MarkupKind) {
        let Some((page, quads)) = self.selection_quads().filter(|(_, q)| !q.is_empty()) else { return };
        let style = self.tool_style(Tool::Markup(markup));
        self.exec(vec![Cmd::Add(Annotation::new(page, style, Kind::Markup { markup, quads }))]);
        self.text_sel = None;
    }

    /// Topmost annotation under a screen point: ours first, then foreign.
    fn hit(&self, p: Pos2) -> Option<Selection> {
        let doc = self.doc.as_ref()?;
        let page = self.view.page_at(p, true)?;
        let g = &doc.pages[page];
        let u = self.view.to_user(g, page, p);
        let tol = 4.0 / self.view.zoom;
        if let Some(a) = doc.annots.iter().rev().find(|a| a.page == page && geometry::distance(a, u) <= tol) {
            return Some(Selection::Ours(a.id.clone()));
        }
        doc.foreign
            .iter()
            .enumerate()
            .rev()
            .find(|(i, f)| {
                doc.foreign_page(*i) == Some(page)
                    && f.selectable
                    && !doc.deleted_foreign.contains(i)
                    && u.x >= f.rect[0] - tol
                    && u.x <= f.rect[2] + tol
                    && u.y >= f.rect[1] - tol
                    && u.y <= f.rect[3] + tol
            })
            .map(|(i, _)| Selection::Foreign(i))
    }

    fn on_press(&mut self, ui: &egui::Ui, p: Pos2, modifiers: Modifiers) {
        // egui only reports double clicks on release; we need them on press.
        let now = ui.input(|i| i.time);
        let double = self.view.last_press.is_some_and(|(t, at)| now - t < 0.4 && at.distance(p) < 6.0);
        self.view.last_press = if double { None } else { Some((now, p)) };
        let was_editing = self.editing.is_some();
        self.commit_edits();
        // A note badge opens (ours) or selects (other apps') the annotation's note.
        if let Some((sel, _)) = self.badge_at(p) {
            self.text_sel = None;
            match sel {
                Selection::Ours(id) => {
                    self.set_tool(Tool::Select);
                    self.open_note(&id);
                }
                foreign => {
                    self.set_tool(Tool::Select);
                    self.select(Some(foreign));
                }
            }
            return;
        }
        let Some(doc) = &self.doc else { return };
        let Some(page) = self.view.page_at(p, true) else {
            if self.tool == Tool::Select {
                self.select(None);
            }
            return;
        };
        let g = doc.pages[page];
        let u = self.view.to_user(&g, page, p);
        if doc.read_only.is_some() && !matches!(self.tool, Tool::Select | Tool::Hand) {
            let msg = doc.read_only.clone().unwrap_or_default();
            self.set_status(ui.ctx(), msg, true);
            return;
        }
        let style = self.tool_style(self.tool);
        match self.tool {
            Tool::Pen | Tool::Highlighter => {
                // Smart highlighter: starting on text highlights the text in straight
                // lines (like text markup); anywhere else, or with Alt held, it's freehand.
                if self.tool == Tool::Highlighter && !modifiers.alt {
                    match self.text_chars.get(&page).and_then(|c| char_at(c, u, 2.0 / self.view.zoom)) {
                        Some(i) => {
                            self.gesture =
                                Gesture::Markup { page, markup: MarkupKind::Highlight, start: i, end: i, style };
                            return;
                        }
                        None => self.request_text(page),
                    }
                }
                let mut brush = LazyBrush::new(self.cfg.stabilizer);
                brush.update(Pt::new(p.x, p.y));
                self.gesture = Gesture::Ink {
                    page,
                    brush,
                    raw: vec![u],
                    highlighter: self.tool == Tool::Highlighter,
                    style,
                };
            }
            Tool::Shape(shape) => self.gesture = Gesture::Shape { page, shape, a: u, b: u, style },
            Tool::Markup(markup) => {
                let Some(chars) = self.text_chars.get(&page) else {
                    self.request_text(page);
                    return;
                };
                match char_at(chars, u, 6.0 / self.view.zoom) {
                    Some(i) => self.gesture = Gesture::Markup { page, markup, start: i, end: i, style },
                    None => self.set_status(ui.ctx(), "No text here (scanned pages have no selectable text)", false),
                }
            }
            Tool::Text => {
                if was_editing {
                    return; // the click just finished the previous text box
                }
                match self.hit(p) {
                    Some(Selection::Ours(id)) if matches!(doc.get(&id).map(|a| &a.kind), Some(Kind::Text { .. })) => {
                        self.edit_text(&id);
                    }
                    _ => {
                        let inv = g.to_user();
                        let size = style.width;
                        // Put the click roughly at the middle of the first line.
                        let origin = u
                            .sub(inv.apply_vec(Pt::new(0.0, 1.0)).scale(TEXT_PAD + size * TEXT_LINE_HEIGHT / 2.0))
                            .sub(inv.apply_vec(Pt::new(1.0, 0.0)).scale(TEXT_PAD));
                        self.editing = Some(TextEditState {
                            id: None,
                            page,
                            origin,
                            right: inv.apply_vec(Pt::new(1.0, 0.0)),
                            down: inv.apply_vec(Pt::new(0.0, 1.0)),
                            text: String::new(),
                            style,
                            focus: true,
                        });
                    }
                }
            }
            Tool::Eraser => {
                self.gesture = Gesture::Erase { removed: Vec::new() };
                self.erase_at(p);
            }
            Tool::Select => {
                if let Some((a, h)) = self.handle_at(p) {
                    self.start_handle_drag(a, h, p);
                    return;
                }
                let hit = self.hit(p);
                self.text_sel = None;
                // Shift+click adds an annotation to the selection, or takes it out.
                if modifiers.shift
                    && let Some(Selection::Ours(id)) = &hit
                {
                    let mut ids = self.selected_ids();
                    match ids.iter().position(|x| x == id) {
                        Some(k) => {
                            ids.remove(k);
                        }
                        None => ids.push(id.clone()),
                    }
                    self.select_ids(ids);
                    return;
                }
                // Grabbing one of a selected group moves the whole group (text markup stays put).
                if let (Some(Selection::Ours(id)), Some(Selection::Many(ids))) = (&hit, &self.selection)
                    && ids.contains(id)
                    && !double
                {
                    let mut group: Vec<Annotation> = ids
                        .iter()
                        .filter_map(|i| doc.get(i))
                        .filter(|a| !matches!(a.kind, Kind::Markup { .. }))
                        .cloned()
                        .collect();
                    // The grabbed one first: the drag is measured on its page.
                    if let Some(k) = group.iter().position(|a| a.id == *id) {
                        group.swap(0, k);
                        let op = EditOp::Move { start: u };
                        self.gesture = Gesture::Edit { before: group.clone(), current: group, op };
                        return;
                    }
                }
                // Dragging from empty space (or anywhere with Shift) draws a selection box.
                let on_text = self.text_chars.get(&page).is_some_and(|c| char_at(c, u, 8.0 / self.view.zoom).is_some());
                if (hit.is_none() || modifiers.shift) && (modifiers.shift || !on_text) && self.link_at(p).is_none() {
                    let base = if modifiers.shift { self.selected_ids() } else { Vec::new() };
                    if !modifiers.shift {
                        self.select(None);
                    }
                    self.request_text(page);
                    self.gesture = Gesture::Marquee { page, start: u, end: u, add: modifiers.shift, base };
                    return;
                }
                if hit.is_none()
                    && !double
                    && let Some((target, link_page)) = self.link_at(p)
                {
                    let char = self.text_chars.get(&link_page).and_then(|c| char_at(c, u, 8.0 / self.view.zoom));
                    self.gesture = Gesture::Link { target, start: p, page: link_page, char };
                    self.select(None);
                    return;
                }
                // Text markup sits on the text: dragging over it selects text, and a
                // plain click selects the markup. Other annotations move when dragged.
                let movable = match &hit {
                    Some(Selection::Ours(id)) => doc.get(id).filter(|a| !matches!(a.kind, Kind::Markup { .. })).cloned(),
                    _ => None,
                };
                if let Some(a) = &movable {
                    if double && matches!(a.kind, Kind::Text { .. }) {
                        self.edit_text(&a.id);
                    } else if double {
                        self.open_note(&a.id);
                        return;
                    } else {
                        let op = EditOp::Move { start: u };
                        self.gesture = Gesture::Edit { before: vec![a.clone()], current: vec![a.clone()], op };
                    }
                }
                if movable.is_none() {
                    match self.text_chars.get(&page).and_then(|c| char_at(c, u, 8.0 / self.view.zoom).map(|i| (c, i))) {
                        Some((chars, i)) => {
                            if double {
                                let (s, e) = word_at(chars, i);
                                self.text_sel = Some(TextSel { page, start: s, end: e });
                            } else {
                                self.text_sel = Some(TextSel { page, start: i, end: i });
                                self.gesture = Gesture::SelectText { click: hit };
                            }
                            self.select(None);
                            return;
                        }
                        None => self.request_text(page),
                    }
                }
                self.select(hit);
            }
            Tool::Hand => {}
        }
    }

    fn on_move(&mut self, p: Pos2, modifiers: Modifiers) {
        // Dragging from a link selects text (Select) or pans (Hand) instead of following it.
        if let Gesture::Link { start, page, char, .. } = self.gesture
            && start.distance(p) > 5.0
        {
            self.gesture = if self.tool == Tool::Hand {
                let base = self.view.set_offset.unwrap_or(self.view.offset);
                self.view.set_offset = Some(base - (p - start));
                Gesture::Pan { last: p }
            } else if let Some(i) = char {
                self.text_sel = Some(TextSel { page, start: i, end: i });
                Gesture::SelectText { click: None }
            } else {
                Gesture::None
            };
        }
        let Some(doc) = &self.doc else { return };
        let zoom = self.view.zoom;
        match &mut self.gesture {
            Gesture::None => {}
            Gesture::Ink { page, brush, raw, .. } => {
                if let Some(b) = brush.update(Pt::new(p.x, p.y)) {
                    let g = doc.pages[*page];
                    raw.push(self.view.to_user(&g, *page, pos2(b.x, b.y)));
                }
            }
            Gesture::Shape { page, shape, a, b, .. } => {
                let u = self.view.to_user(&doc.pages[*page], *page, p);
                *b = if modifiers.shift { shift_constrain(*shape, *a, u) } else { u };
            }
            Gesture::Markup { page, end, .. } => {
                let u = self.view.to_user(&doc.pages[*page], *page, p);
                if let Some(i) = self.text_chars.get(page).and_then(|c| char_at(c, u, f32::INFINITY)) {
                    *end = i;
                }
            }
            Gesture::Erase { .. } => self.erase_at(p),
            Gesture::Link { .. } => {}
            Gesture::Edit { before, current, op } => {
                let Some(first) = before.first() else { return };
                let g = doc.pages[first.page];
                let u = self.view.to_user(&g, first.page, p);
                *current = before
                    .iter()
                    .map(|b| edit_annotation(b, &doc.pages[b.page], *op, u, modifiers.shift, zoom))
                    .collect();
            }
            Gesture::Marquee { page, end, .. } => {
                *end = self.view.to_user(&doc.pages[*page], *page, p);
                // Selects live, so you see what the box catches.
                if let Gesture::Marquee { page, start, end, add, base } = &self.gesture {
                    let mut ids = if *add { base.clone() } else { Vec::new() };
                    for id in self.in_box(*page, *start, *end) {
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                    self.select_ids(ids);
                }
            }
            Gesture::SelectText { .. } => {
                if let Some(sel) = &mut self.text_sel {
                    let u = self.view.to_user(&doc.pages[sel.page], sel.page, p);
                    if let Some(i) = self.text_chars.get(&sel.page).and_then(|c| char_at(c, u, f32::INFINITY)) {
                        sel.end = i;
                    }
                }
            }
            Gesture::Pan { last } => {
                let d = p - *last;
                *last = p;
                // Several move events can arrive in one frame: accumulate them all.
                let base = self.view.set_offset.unwrap_or(self.view.offset);
                self.view.set_offset = Some(base - d);
                self.view.scroll_target = None;
            }
        }
    }

    fn on_release(&mut self, ctx: &egui::Context, _p: Pos2) {
        let gesture = std::mem::replace(&mut self.gesture, Gesture::None);
        let zoom = self.view.zoom;
        match gesture {
            Gesture::Ink { page, raw, highlighter, style, .. } => {
                let curve = finish_stroke(&raw, 1.0 / zoom);
                if !curve.is_empty() {
                    self.exec(vec![Cmd::Add(Annotation::new(page, style, Kind::Ink { curve, highlighter }))]);
                }
            }
            Gesture::Shape { page, shape, a, b, style } => {
                let is_mark = matches!(shape, ShapeKind::Check | ShapeKind::Cross);
                if (a.dist(b) * zoom >= 3.0 || is_mark)
                    && let Some(g) = self.doc.as_ref().map(|d| d.pages[page])
                {
                    let (a, b) = shape_box(&g, shape, a, b, &style, zoom);
                    self.exec(vec![Cmd::Add(Annotation::new(page, style, Kind::Shape { shape, a, b }))]);
                }
            }
            Gesture::Markup { page, markup, start, end, style } => {
                let Some(doc) = &self.doc else { return };
                let chars = self.text_chars.get(&page).map(Vec::as_slice).unwrap_or_default();
                let quads = markup_quads(chars, start, end, &doc.pages[page]);
                if !quads.is_empty() {
                    self.exec(vec![Cmd::Add(Annotation::new(page, style, Kind::Markup { markup, quads }))]);
                }
            }
            Gesture::Erase { removed } => {
                if let Some(doc) = &mut self.doc {
                    doc.record(removed);
                }
            }
            Gesture::Edit { before, current, .. } => {
                let cmds: Vec<Cmd> = before
                    .into_iter()
                    .zip(current)
                    .filter(|(b, a)| b != a)
                    .map(|(before, after)| Cmd::Modify { before, after })
                    .collect();
                self.exec(cmds);
            }
            Gesture::Marquee { .. } => {}
            Gesture::SelectText { click } => {
                // A plain click (no drag) selects no text, but whatever annotation was clicked.
                if self.text_sel.is_some_and(|s| s.start == s.end) {
                    self.text_sel = None;
                    self.select(click);
                }
            }
            Gesture::Link { target, .. } => self.follow(ctx, &target),
            Gesture::Pan { .. } | Gesture::None => {}
        }
    }

    /// Removes our annotations under the eraser. Foreign annotations are never erased.
    fn erase_at(&mut self, p: Pos2) {
        let radius = self.tool_style(Tool::Eraser).width / 2.0 / self.view.zoom;
        let Some(doc) = &mut self.doc else { return };
        let Some(page) = self.view.page_at(p, true) else { return };
        let u = self.view.to_user(&doc.pages[page], page, p);
        let Gesture::Erase { removed } = &mut self.gesture else { return };
        while let Some(index) = doc
            .annots
            .iter()
            .rposition(|a| a.page == page && geometry::distance(a, u) <= radius)
        {
            let cmd = Cmd::Remove { annot: doc.annots[index].clone(), index };
            doc.apply(&cmd, true);
            removed.push(cmd);
        }
    }

    // ------------------------------------------------------------- text

    pub fn edit_text(&mut self, id: &str) {
        let Some(a) = self.doc.as_ref().and_then(|d| d.get(id)) else { return };
        if let Kind::Text { origin, right, down, text } = &a.kind {
            self.editing = Some(TextEditState {
                id: Some(a.id.clone()),
                page: a.page,
                origin: *origin,
                right: *right,
                down: *down,
                text: text.clone(),
                style: a.style,
                focus: true,
            });
            self.selection = Some(Selection::Ours(a.id.clone()));
        }
    }

    /// Finishes any text box or note being typed.
    pub fn commit_edits(&mut self) {
        self.commit_note();
        self.commit_text();
    }

    /// Opens the note editor for one of our annotations (and selects it).
    pub fn open_note(&mut self, id: &str) {
        self.commit_edits();
        let Some(a) = self.doc.as_ref().and_then(|d| d.get(id)).filter(|a| a.takes_note()) else { return };
        let (id, text) = (a.id.clone(), a.note.clone());
        self.select(Some(Selection::Ours(id.clone())));
        self.note_edit = Some(NoteEdit { id, text, focus: true });
    }

    /// Saves the note being edited (as one undo step) and closes the editor.
    pub fn commit_note(&mut self) {
        let Some(n) = self.note_edit.take() else { return };
        let Some(doc) = &self.doc else { return };
        let Some(before) = doc.get(&n.id).cloned() else { return };
        let note = n.text.trim().to_string();
        if note != before.note {
            let after = Annotation { note, ..before.clone() };
            self.exec(vec![Cmd::Modify { before, after }]);
        }
    }

    fn commit_text(&mut self) {
        let Some(e) = self.editing.take() else { return };
        let Some(doc) = &self.doc else { return };
        let text = e.text.trim_end().to_string();
        let kind = Kind::Text { origin: e.origin, right: e.right, down: e.down, text: text.clone() };
        let cmd = match &e.id {
            Some(id) => {
                let Some(index) = doc.index_of(id) else { return };
                let before = doc.annots[index].clone();
                if text.is_empty() {
                    Some(Cmd::Remove { annot: before, index })
                } else {
                    let after = Annotation { kind, style: e.style, ..before.clone() };
                    (after != before).then_some(Cmd::Modify { before, after })
                }
            }
            None => (!text.is_empty()).then(|| Cmd::Add(Annotation::new(e.page, e.style, kind))),
        };
        if let Some(cmd) = cmd {
            self.exec(vec![cmd]);
        }
    }

    fn text_editor(&mut self, ui: &egui::Ui) {
        let tool_style = self.tool_style(Tool::Text);
        let zoom = self.view.zoom;
        let Some(doc) = &self.doc else { return };
        let Some(e) = &mut self.editing else { return };
        if e.id.is_none() {
            e.style = tool_style;
        }
        let aff = self.view.to_screen(&doc.pages[e.page], e.page);
        let top_left = aff.apply(e.origin.add(e.right.scale(TEXT_PAD)).add(e.down.scale(TEXT_PAD)));
        let size = e.style.width * zoom;
        let longest = e.text.split('\n').map(|l| geometry::helv_text_width(l, e.style.width)).fold(0.0, f32::max);
        let width = (longest * zoom + size).max(size * 4.0);
        let font = FontId::new(size, egui::FontFamily::Name("annot".into()));
        let mut escape = false;
        let area = egui::Area::new(egui::Id::new("text-annot-editor"))
            .fixed_pos(pos(top_left))
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                let id = egui::Id::new("text-annot-edit");
                if e.focus {
                    // Before adding the widget, so keys typed this frame already go into it.
                    // Repeated until it sticks: the click that opened the editor counts as a
                    // click elsewhere for the new field and would drop the focus again.
                    ui.memory_mut(|m| m.request_focus(id));
                }
                let edit = egui::TextEdit::multiline(&mut e.text)
                    .id(id)
                    .font(font)
                    .text_color(color(&e.style))
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::ZERO)
                    .desired_width(width)
                    .desired_rows(1);
                let r = ui.add(edit);
                if r.has_focus() && !ui.input(|i| i.pointer.any_down() || i.pointer.any_released()) {
                    e.focus = false;
                }
                if r.lost_focus() {
                    if ui.input(|i| i.key_pressed(Key::Escape)) {
                        escape = true;
                    } else {
                        // Focus moved to e.g. the color picker: keep editing.
                        e.focus = true;
                    }
                }
                r.rect
            });
        let box_rect = area.inner.expand(TEXT_PAD * zoom);
        ui.painter().rect_stroke(
            box_rect,
            2.0,
            Stroke::new(1.0, ui.visuals().selection.stroke.color),
            StrokeKind::Outside,
        );
        if escape {
            self.commit_edits();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(painter: &Painter, aff: &Affine, zoom: f32, origin: Pt, right: Pt, down: Pt, text: &str, style: &Style) {
    let size = style.width;
    let font = FontId::new(size * zoom, egui::FontFamily::Name("annot".into()));
    // Direction of the text on screen (a rotated text box is drawn at an angle).
    let screen_right = aff.apply_vec(right).normalized();
    let screen_down = Pt::new(-screen_right.y, screen_right.x);
    let angle = screen_right.y.atan2(screen_right.x);
    for (i, line) in text.split('\n').enumerate() {
        let base = origin
            .add(right.scale(TEXT_PAD))
            .add(down.scale(TEXT_PAD + TEXT_ASCENT * size + i as f32 * TEXT_LINE_HEIGHT * size));
        let base = aff.apply(base);
        let galley = painter.layout_no_wrap(line.to_owned(), font.clone(), color(style));
        let ascent = galley
            .rows
            .first()
            .and_then(|r| r.glyphs.first())
            .map(|g| g.font_ascent)
            .unwrap_or(size * zoom * TEXT_ASCENT);
        let top_left = base.sub(screen_down.scale(ascent));
        if angle.abs() < 1e-4 {
            painter.galley(pos(top_left), galley, color(style));
        } else {
            painter.add(egui::epaint::TextShape::new(pos(top_left), galley, color(style)).with_angle(angle));
        }
    }
}

/// Drives the real UI headless (egui frames with synthetic input) against pdfium.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;
    use eframe::egui::{Modifiers, RawInput};
    use std::collections::BTreeSet;

    /// One page with two lines of Helvetica text and a foreign square annotation.
    fn text_pdf(path: &std::path::Path) {
        use lopdf::{Object, Stream, dictionary};
        let mut doc = lopdf::Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
        });
        let content = b"BT /F1 14 Tf 72 700 Td (Hello world please highlight me) Tj ET\n\
                        BT /F1 14 Tf 72 680 Td (A second line of text) Tj ET"
            .to_vec();
        let content = doc.add_object(Stream::new(dictionary! {}, content));
        let ap = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()] },
            b"0 0.6 0 RG 2 w 1 1 98 98 re S".to_vec(),
        ));
        let foreign = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Square",
            "Rect" => vec![400.into(), 400.into(), 500.into(), 500.into()],
            "NM" => Object::string_literal("from-another-app"),
            "AP" => dictionary! { "N" => ap },
        });
        // Three identical pages; only the first has the foreign annotation.
        let kids: Vec<Object> = (0..3)
            .map(|i| {
                let mut page = dictionary! {
                    "Type" => "Page", "Parent" => pages_id, "Contents" => content,
                    "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                    "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
                };
                if i == 0 {
                    page.set("Annots", vec![foreign.into()]);
                }
                doc.add_object(page).into()
            })
            .collect();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 3 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        doc.save(path).unwrap();
    }

    struct Harness {
        ctx: egui::Context,
        app: App,
        t: f64,
        mods: Modifiers,
        /// Window size.
        size: Vec2,
    }

    impl Harness {
        fn frame(&mut self, events: Vec<Event>) {
            self.t += 1.0 / 60.0;
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
                time: Some(self.t),
                // Held modifiers, as a real window reports them.
                events: [vec![Event::ModifiersChanged(self.mods)], events].concat(),
                ..Default::default()
            };
            let app = &mut self.app;
            let mut out = self.ctx.run_ui(input, |ui| app.frame(ui));
            out.textures_delta.clear(); // no GPU here
        }

        fn wait_until(&mut self, what: &str, cond: impl Fn(&App) -> bool) {
            for _ in 0..500 {
                if cond(&self.app) {
                    return;
                }
                self.frame(vec![]);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("timed out waiting for {what}");
        }

        fn at(&self, x: f32, y: f32) -> Pos2 {
            let g = self.app.doc.as_ref().unwrap().pages[0];
            pos(self.app.view.to_screen(&g, 0).apply(Pt::new(x, y)))
        }

        fn button(&mut self, p: Pos2, pressed: bool) {
            let modifiers = self.mods;
            self.frame(vec![Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers }]);
        }

        fn drag(&mut self, pts: &[Pos2]) {
            self.frame(vec![Event::PointerMoved(pts[0])]);
            self.button(pts[0], true);
            for p in &pts[1..] {
                self.frame(vec![Event::PointerMoved(*p)]);
            }
            self.button(*pts.last().unwrap(), false);
        }

        fn click(&mut self, p: Pos2) {
            self.drag(&[p]);
        }

        /// A key tap: pressed and released, with `modifiers` held meanwhile.
        fn key(&mut self, key: Key, modifiers: Modifiers) {
            let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
            self.frame(vec![Event::ModifiersChanged(modifiers), ev(true), ev(false)]);
        }

        fn doc(&self) -> &Doc {
            self.app.doc.as_ref().unwrap()
        }
    }

    /// Six text pages with a table of contents and a link on page 1 to page 3.
    fn reading_pdf(path: &std::path::Path) {
        use lopdf::{Object, Stream, dictionary};
        let mut doc = lopdf::Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let content = doc.add_object(Stream::new(dictionary! {}, b"BT /F1 14 Tf 72 700 Td (Some text) Tj ET".to_vec()));
        let page_ids: Vec<_> = (0..6).map(|_| doc.new_object_id()).collect();
        let xyz = |page: usize, y: i64| vec![page_ids[page].into(), "XYZ".into(), Object::Null, y.into(), Object::Null];
        let link = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Link",
            "Rect" => vec![450.into(), 600.into(), 550.into(), 620.into()],
            "Dest" => xyz(2, 400),
        });
        for (i, id) in page_ids.iter().enumerate() {
            let mut page = dictionary! {
                "Type" => "Page", "Parent" => pages_id, "Contents" => content,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            };
            if i == 0 {
                page.set("Annots", vec![link.into()]);
            }
            doc.objects.insert(*id, Object::Dictionary(page));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => page_ids.iter().map(|&id| id.into()).collect::<Vec<Object>>(), "Count" => 6,
            }),
        );
        // Outlines: Intro (page 1), Chapter (page 3) with a child Section (page 3, y 400).
        let [outlines, intro, chapter, section] = [(); 4].map(|_| doc.new_object_id());
        let title = |t: &str| Object::string_literal(t);
        doc.objects.insert(outlines, Object::Dictionary(dictionary! {
            "Type" => "Outlines", "First" => intro, "Last" => chapter, "Count" => 3,
        }));
        doc.objects.insert(intro, Object::Dictionary(dictionary! {
            "Title" => title("Intro"), "Parent" => outlines, "Next" => chapter, "Dest" => xyz(0, 792),
        }));
        doc.objects.insert(chapter, Object::Dictionary(dictionary! {
            "Title" => title("Chapter"), "Parent" => outlines, "Prev" => intro,
            "First" => section, "Last" => section, "Count" => 1, "Dest" => xyz(2, 792),
        }));
        doc.objects.insert(section, Object::Dictionary(dictionary! {
            "Title" => title("Section"), "Parent" => chapter, "Dest" => xyz(2, 400),
        }));
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id, "Outlines" => outlines });
        doc.trailer.set("Root", catalog);
        doc.save(path).unwrap();
    }

    #[test]
    fn pasting_on_another_page_lands_in_view() {
        let dir = std::env::temp_dir().join(format!("ochre-paste-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paste.pdf");
        reading_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path.clone()));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);

        // A box near the top of page 1, copied.
        let rect = Annotation::new(0, Style::new([1.0, 0.0, 0.0], 2.0, 1.0), Kind::Shape { shape: ShapeKind::Rect, a: Pt::new(100.0, 680.0), b: Pt::new(200.0, 740.0) });
        h.app.exec(vec![Cmd::Add(rect.clone())]);
        h.app.select(Some(Selection::Ours(rect.id.clone())));
        let (clip, _) = h.app.clip_text().unwrap();
        let on_screen = |h: &Harness, a: &Annotation| {
            let g = h.doc().pages[a.page];
            let aff = h.app.view.to_screen(&g, a.page);
            let [x0, y0, x1, y1] = geometry::bounds(a);
            let r = Rect::from_two_pos(pos(aff.apply(Pt::new(x0, y0))), pos(aff.apply(Pt::new(x1, y1))));
            h.app.view.viewport.contains_rect(r)
        };

        // Looking at the middle of page 4, where the box's spot is out of view: the
        // copy goes to the middle of the window.
        h.app.view.jump = Some(Jump { page: 3, y: 350.0, margin: 0.0, x: JumpX::Keep, animate: false });
        for _ in 0..10 {
            h.frame(vec![]);
        }
        h.frame(vec![Event::Paste(clip.clone())]);
        let pasted = h.doc().annots.last().unwrap().clone();
        assert_eq!(pasted.page, 3);
        assert!(on_screen(&h, &pasted), "{:?}", geometry::bounds(&pasted));
        // Pasting again offsets the next copy from it.
        h.frame(vec![Event::Paste(clip.clone())]);
        let again = h.doc().annots.last().unwrap().clone();
        let [ax, ..] = geometry::bounds(&again);
        assert!((ax - geometry::bounds(&pasted)[0] - 12.0).abs() < 1e-3);

        // Where the box's spot is in view, the copy keeps it (offset as the third
        // paste in a row).
        h.app.view.jump = Some(Jump { page: 4, y: 0.0, margin: 0.0, x: JumpX::Keep, animate: false });
        for _ in 0..10 {
            h.frame(vec![]);
        }
        h.frame(vec![Event::Paste(clip)]);
        let kept = h.doc().annots.last().unwrap().clone();
        let [x0, y0, ..] = geometry::bounds(&rect);
        let [kx, ky, ..] = geometry::bounds(&kept);
        assert_eq!(kept.page, 4);
        assert!((kx - x0 - 24.0).abs() < 1e-3 && (ky - y0 + 24.0).abs() < 1e-3, "{:?}", geometry::bounds(&kept));
        assert!(on_screen(&h, &kept));
    }

    #[test]
    fn contents_links_and_reading_position() {
        use crate::pdf::worker::{Link, OutlineItem};
        let dir = std::env::temp_dir().join(format!("ochre-reading-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reading.pdf");
        reading_pdf(&path);

        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path.clone()));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);

        let page = |p: usize, y: f32| Some(Target::Page { page: p, x: None, y: Some(y) });
        let item = |title: &str, level, target| OutlineItem { title: title.into(), level, target };
        assert_eq!(
            h.doc().outline,
            [item("Intro", 0, page(0, 792.0)), item("Chapter", 0, page(2, 792.0)), item("Section", 1, page(2, 400.0))]
        );
        assert_eq!(h.doc().links[0], [Link { rect: [450.0, 600.0, 550.0, 620.0], target: page(2, 400.0).unwrap() }]);
        assert!(h.doc().links[1].is_empty());

        let pos = |h: &Harness| h.app.view.position().unwrap();
        let settle = |h: &mut Harness| {
            h.frame(vec![]);
            h.wait_until("scroll settles", |a| a.view.scroll_target.is_none());
            h.frame(vec![]);
        };
        let start = pos(&h);
        assert_eq!(start.0, 0);

        // Clicking the link (Select tool) goes to the spot on page 3, a little below the top.
        h.app.set_tool(Tool::Select);
        h.click(h.at(500.0, 610.0));
        settle(&mut h);
        let (p, y) = pos(&h);
        let zoom = h.app.view.zoom;
        assert_eq!(p, 2);
        assert!((y - (792.0 - 400.0 - 12.0 / zoom)).abs() < 1.0, "y {y}");
        // The contents highlight the section being read.
        assert_eq!(h.app.current_outline_item(), Some(2));

        // Alt+Left goes back.
        h.key(Key::ArrowLeft, Modifiers::ALT);
        settle(&mut h);
        assert_eq!(pos(&h).0, 0);
        assert!((pos(&h).1 - start.1).abs() < 1.0);
        assert!(h.app.view.back.is_empty());
        assert_eq!(h.app.current_outline_item(), Some(0));

        // With the Hand tool a click follows the link too, while a drag pans.
        h.app.set_tool(Tool::Hand);
        let at = h.at(500.0, 610.0);
        h.drag(&[at, at - vec2(0.0, 20.0), at - vec2(0.0, 40.0)]);
        h.frame(vec![]);
        assert!(h.app.view.back.is_empty(), "a drag doesn't follow the link");
        assert_eq!(pos(&h).0, 0);
        h.key(Key::G, Modifiers::NONE);
        h.key(Key::G, Modifiers::NONE);
        settle(&mut h);
        h.click(h.at(500.0, 610.0));
        settle(&mut h);
        assert_eq!(pos(&h).0, 2);

        // A contents entry without a y goes to the top of its page.
        let chapter = h.doc().outline[1].target.clone().unwrap();
        let ctx = h.ctx.clone();
        h.app.follow(&ctx, &Target::Page { page: 1, x: None, y: None });
        settle(&mut h);
        assert_eq!(pos(&h).0, 1);
        h.app.follow(&ctx, &chapter);
        settle(&mut h);
        assert_eq!(pos(&h).0, 2);
        assert_eq!(h.app.current_outline_item(), Some(1));

        // Only web and mail links are opened.
        h.app.follow(&ctx, &Target::Uri("file:///etc/passwd".into()));
        assert!(h.app.status.as_ref().is_some_and(|(m, _, err)| *err && m.starts_with("Not opening")));

        // Closing the tab and opening the file again returns to the same place and zoom.
        h.app.view.zoom_by(1.25);
        h.frame(vec![]);
        h.frame(vec![]);
        let (zoom, before) = (h.app.view.zoom, pos(&h));
        h.key(Key::W, Modifiers::COMMAND);
        assert!(h.app.doc.is_none() && h.app.tabs.is_empty());
        h.app.request_open(Some(path.clone()));
        assert!(h.app.view.position().is_none(), "fresh view");
        h.wait_until("pages again", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);
        h.frame(vec![]);
        assert_eq!(h.app.view.zoom, zoom);
        let after = pos(&h);
        assert_eq!(after.0, before.0);
        assert!((after.1 - before.1).abs() < 1.0, "{before:?} -> {after:?}");
    }

    #[test]
    fn tabs() {
        let dir = std::env::temp_dir().join(format!("ochre-tabs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.pdf"), dir.join("b.pdf"));
        text_pdf(&a);
        reading_pdf(&b);

        let ctx = egui::Context::default();
        let app = App::new(&ctx, [a.clone(), b.clone()]);
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        let name = |h: &Harness| h.doc().name();
        let loaded = |a: &App| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty());
        // Both files open in tabs; the last one is active.
        assert_eq!(h.app.tabs.len(), 2);
        assert_eq!((h.app.active, name(&h).as_str()), (1, "b.pdf"));
        h.wait_until("b loads", loaded);
        assert_eq!(h.doc().pages.len(), 6);
        assert_eq!(h.doc().outline.len(), 3);

        // Ctrl+Shift+Tab goes to the previous tab, which loads when shown.
        h.key(Key::Tab, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!((h.app.active, name(&h).as_str()), (0, "a.pdf"));
        h.wait_until("a loads", loaded);
        h.wait_until("a renders", |a| !a.tex.is_empty());
        assert_eq!(h.doc().pages.len(), 3);
        assert!(h.doc().outline.is_empty());

        // Each tab keeps its own state: annotations, selection, zoom.
        h.app.set_tool(Tool::Shape(ShapeKind::Rect));
        h.drag(&[h.at(100.0, 300.0), h.at(150.0, 325.0), h.at(200.0, 350.0)]);
        let rect = h.doc().annots[0].id.clone();
        h.app.set_tool(Tool::Select);
        h.click(h.at(100.0, 325.0));
        assert_eq!(h.app.selection, Some(Selection::Ours(rect.clone())));
        h.app.view.zoom_by(1.25);
        h.frame(vec![]);
        let zoom_a = h.app.view.zoom;
        assert!(h.doc().is_dirty());

        h.key(Key::Tab, Modifiers::CTRL);
        assert_eq!(name(&h), "b.pdf");
        h.frame(vec![]);
        assert!(h.doc().annots.is_empty() && h.app.selection.is_none());
        assert_ne!(h.app.view.zoom, zoom_a);

        // Opening a file that's already open switches to its tab.
        h.app.request_open(Some(a.clone()));
        assert_eq!((h.app.tabs.len(), h.app.active), (2, 0));
        assert_eq!(h.app.selection, Some(Selection::Ours(rect)));
        assert_eq!(h.app.view.zoom, zoom_a);
        h.frame(vec![]);
        h.wait_until("a renders again", |a| !a.tex.is_empty());

        // Closing a tab with unsaved changes asks first; Esc cancels.
        h.key(Key::W, Modifiers::COMMAND);
        assert!(h.app.pending.is_some());
        h.key(Key::Escape, Modifiers::NONE);
        assert!(h.app.pending.is_none());
        assert_eq!(h.app.tabs.len(), 2);
        // Don't save: the tab closes and its neighbour becomes active.
        h.app.pending = None;
        h.app.close_active_tab();
        assert_eq!((h.app.tabs.len(), h.app.active, name(&h).as_str()), (1, 0, "b.pdf"));
        h.frame(vec![]);
        h.wait_until("b renders", |a| !a.tex.is_empty());

        // Closing the last tab goes back to the welcome screen.
        h.key(Key::W, Modifiers::COMMAND);
        assert!(h.app.doc.is_none() && h.app.tabs.is_empty());
        h.frame(vec![]);
    }

    #[test]
    fn rotate_restyle_clipboard_and_undo_after_save() {
        let dir = std::env::temp_dir().join(format!("ochre-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edit.pdf");
        text_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path.clone()));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);
        h.app.set_tool(Tool::Shape(ShapeKind::Rect));
        h.drag(&[h.at(100.0, 300.0), h.at(150.0, 325.0), h.at(200.0, 350.0)]);
        h.app.set_tool(Tool::Pen);
        h.drag(&(0..30).map(|i| h.at(300.0 + i as f32 * 3.0, 200.0)).collect::<Vec<_>>());
        let (rect, ink) = (h.doc().annots[0].id.clone(), h.doc().annots[1].id.clone());
        let get = |h: &Harness, id: &str| h.doc().get(id).unwrap().clone();

        // Rotate the rectangle a quarter turn clockwise with the rotate handle (Shift snaps).
        h.app.set_tool(Tool::Select);
        h.app.select(Some(Selection::Ours(rect.clone())));
        h.frame(vec![]);
        let handle = h.app.handles(&get(&h, &rect)).into_iter().find(|x| x.0 == Handle::Rotate).unwrap().1;
        let right_of_center = h.at(240.0, 325.0);
        h.mods = Modifiers::SHIFT;
        h.drag(&[handle, h.at(200.0, 380.0), right_of_center + vec2(0.0, 3.0)]);
        h.mods = Modifiers::NONE;
        let r = get(&h, &rect);
        assert!((r.angle + std::f32::consts::FRAC_PI_2).abs() < 1e-4, "angle {}", r.angle);
        let [x0, y0, x1, y1] = geometry::bounds(&r);
        // Now 50 wide and 100 tall about the same center, plus the 2 pt stroke padding.
        let pad = r.style.width / 2.0 + 1.0;
        let expect = [125.0 - pad, 275.0 - pad, 175.0 + pad, 375.0 + pad];
        assert!([x0, y0, x1, y1].iter().zip(expect).all(|(v, e)| (v - e).abs() < 0.5), "{:?}", [x0, y0, x1, y1]);
        // It's hit where it now is, not where it was.
        assert_eq!(geometry::distance(&r, Pt::new(150.0, 375.0)), 0.0);
        assert!(geometry::distance(&r, Pt::new(200.0, 325.0)) > 10.0);
        // A rotated box is resized along its own sides.
        let corners = h.app.handles(&r);
        let (_, c0) = corners.iter().find(|x| x.0 == Handle::BoxCorner(0)).copied().unwrap();
        // Corner 0 is now at the top left on screen; dragging it up and left grows the box.
        h.drag(&[c0, c0 + vec2(-5.0, -10.0), c0 + vec2(-10.0, -20.0)]);
        let grown = get(&h, &rect);
        let Kind::Shape { a, b, .. } = grown.kind else { panic!() };
        assert!((b.x - a.x).abs() > 105.0, "long side grew: {a:?} {b:?}");
        assert_eq!(grown.angle, r.angle);
        h.key(Key::Z, Modifiers::COMMAND);

        // A group's color, width and opacity change together, in one undo step.
        let before = (get(&h, &rect), get(&h, &ink));
        h.app.select_ids(vec![rect.clone(), ink.clone()]);
        let g = h.app.group_style().unwrap();
        assert_eq!((g.count, g.width_label), (2, Some("Width")));
        let mut s = g.style;
        s.color = [0.9, 0.1, 0.1];
        h.app.apply_style(s);
        s.width = 6.0;
        h.app.apply_style(s);
        s.opacity = 0.5;
        h.app.apply_style(s);
        for id in [&rect, &ink] {
            let st = get(&h, id).style;
            assert_eq!((st.color, st.width, st.opacity), ([0.9, 0.1, 0.1], 6.0, 0.5));
        }
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!((get(&h, &rect), get(&h, &ink)), before);

        // Copy and paste: each paste lands a little further along; Ctrl+D duplicates.
        h.app.select(Some(Selection::Ours(rect.clone())));
        let (clip, n) = h.app.clip_text().unwrap();
        assert_eq!(n, 1);
        let left = |a: &Annotation| match a.kind {
            Kind::Shape { a, .. } => a.x,
            _ => panic!(),
        };
        let x = left(&get(&h, &rect));
        for k in 1..=2 {
            h.frame(vec![Event::Paste(clip.clone())]);
            let pasted = h.doc().annots.last().unwrap().clone();
            assert_ne!(pasted.id, rect);
            assert!((left(&pasted) - x - 12.0 * k as f32).abs() < 1e-3);
            assert_eq!(h.app.selection, Some(Selection::Ours(pasted.id.clone())));
        }
        h.key(Key::D, Modifiers::COMMAND);
        assert!((left(h.doc().annots.last().unwrap()) - x - 36.0).abs() < 1e-3);
        assert_eq!(h.doc().annots.len(), 5);
        // Cut removes it.
        h.frame(vec![Event::Cut]);
        assert_eq!(h.doc().annots.len(), 4);

        // Arrow keys nudge (Shift: 10); a quick run of nudges is one undo step.
        h.app.select(Some(Selection::Ours(rect.clone())));
        let start = get(&h, &rect);
        for _ in 0..3 {
            h.key(Key::ArrowRight, Modifiers::NONE);
        }
        h.key(Key::ArrowDown, Modifiers::SHIFT);
        let Kind::Shape { a: moved, .. } = get(&h, &rect).kind else { panic!() };
        let Kind::Shape { a: orig, .. } = start.kind else { panic!() };
        assert!((moved.x - orig.x - 3.0).abs() < 1e-3 && (moved.y - orig.y + 10.0).abs() < 1e-3, "{orig:?} -> {moved:?}");
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(get(&h, &rect), start);

        // Rotate again and save: the angle survives, written as a polygon; and undo
        // still works after saving.
        let rotated = get(&h, &rect).rotated(get(&h, &rect).center(), 0.5);
        h.app.exec(vec![Cmd::Modify { before: get(&h, &rect), after: rotated.clone() }]);
        let ctx = h.ctx.clone();
        assert!(h.app.save(&ctx, false));
        assert_eq!(get(&h, &rect), rotated);
        let d = lopdf::Document::load(&path).unwrap();
        assert!(d.objects.values().filter_map(|o| o.as_dict().ok()).any(|d| {
            d.get(b"Subtype").and_then(|s| s.as_name()).ok() == Some(b"Polygon")
        }));
        assert!(h.doc().can_undo(), "history kept after saving");
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(get(&h, &rect).angle, start.angle);
        assert!(h.doc().is_dirty());
        h.wait_until("reload after save", |a| a.sent_generation == a.doc.as_ref().unwrap().generation);
        h.frame(vec![]);
    }

    #[test]
    fn file_dialogs_dont_block_the_window() {
        use crate::app::FileDialog;
        let dir = std::env::temp_dir().join(format!("ochre-dialog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, copy) = (dir.join("a.pdf"), dir.join("b.pdf"), dir.join("a-copy.pdf"));
        text_pdf(&a);
        reading_pdf(&b);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(a.clone()));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));

        // A dialog that takes a while to answer: frames keep coming meanwhile.
        let picked = b.clone();
        h.app.show_dialog(FileDialog::Open, move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            vec![picked]
        });
        let start = std::time::Instant::now();
        h.frame(vec![]);
        assert!(start.elapsed() < std::time::Duration::from_millis(100), "a frame didn't wait for the dialog");
        assert_eq!(h.app.tabs.len(), 1);
        h.wait_until("the picked file opens", |a| a.tabs.len() == 2);
        assert_eq!(h.doc().name(), "b.pdf");

        // Save As for a.pdf, while b.pdf's tab is active when the answer comes.
        h.app.switch_tab(0);
        h.app.set_tool(Tool::Pen);
        h.drag(&[h.at(100.0, 500.0), h.at(150.0, 520.0), h.at(200.0, 500.0)]);
        let target = copy.clone();
        h.app.show_dialog(FileDialog::SaveAs(a.clone()), move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            vec![target]
        });
        h.app.switch_tab(1);
        h.wait_until("saved as", |a| !a.dialog_open() && a.doc.as_ref().is_some_and(|d| d.path == copy));
        assert_eq!(Doc::open(&copy).unwrap().annots.len(), 1);
        // Cancelling (no file picked) does nothing.
        h.app.show_dialog(FileDialog::Open, Vec::new);
        h.wait_until("cancelled", |a| !a.dialog_open());
        assert_eq!(h.app.tabs.len(), 2);
    }

    #[test]
    fn cancelling_the_eraser_puts_back_what_it_erased() {
        let dir = std::env::temp_dir().join(format!("ochre-erase-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("erase.pdf");
        text_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);
        h.app.set_tool(Tool::Shape(ShapeKind::Rect));
        h.drag(&[h.at(100.0, 300.0), h.at(150.0, 325.0), h.at(200.0, 350.0)]);
        let rect = h.doc().annots.clone();

        // Escape, a tool switch or a tab switch in the middle of erasing: nothing is lost.
        for cancel in [0, 1] {
            h.app.set_tool(Tool::Eraser);
            let at = h.at(100.0, 325.0);
            h.frame(vec![Event::PointerMoved(at)]);
            h.button(at, true);
            assert!(h.doc().annots.is_empty(), "erased while dragging");
            match cancel {
                0 => h.key(Key::Escape, Modifiers::NONE),
                _ => h.key(Key::P, Modifiers::NONE),
            }
            h.button(at, false);
            assert_eq!(h.doc().annots, rect, "put back (cancel {cancel})");
        }
        // A finished erase is still one undo step.
        h.app.set_tool(Tool::Eraser);
        h.drag(&[h.at(100.0, 325.0), h.at(101.0, 325.0)]);
        assert!(h.doc().annots.is_empty());
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.doc().annots, rect);
    }

    #[test]
    fn reading_defaults_arrow_keys_search_and_annotation_list() {
        let dir = std::env::temp_dir().join(format!("ochre-ui2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui2.pdf");
        text_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);
        // Documents open with Select, not a drawing tool.
        assert_eq!(h.app.tool, Tool::Select);

        // Holding the down arrow scrolls; releasing stops.
        let hold = |h: &mut Harness, key: Key, frames: usize| {
            let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };
            h.frame(vec![ev(true)]);
            for _ in 0..frames {
                h.frame(vec![]);
            }
            h.frame(vec![ev(false)]);
            h.frame(vec![]);
        };
        let y0 = h.app.view.offset.y;
        hold(&mut h, Key::ArrowDown, 8);
        let y1 = h.app.view.offset.y;
        assert!(y1 > y0 + 50.0, "scrolled {}", y1 - y0);
        hold(&mut h, Key::ArrowUp, 8);
        assert!(h.app.view.offset.y < y1 - 50.0);
        // With an annotation selected, the arrows move it instead of the view.
        h.app.set_tool(Tool::Shape(ShapeKind::Rect));
        h.drag(&[h.at(100.0, 300.0), h.at(150.0, 325.0), h.at(200.0, 350.0)]);
        let rect = h.doc().annots[0].clone();
        h.app.set_tool(Tool::Select);
        h.app.select(Some(Selection::Ours(rect.id.clone())));
        let y = h.app.view.offset.y;
        hold(&mut h, Key::ArrowDown, 8);
        assert_eq!(h.app.view.offset.y, y);
        assert_ne!(h.doc().annots[0], rect, "nudged");
        h.key(Key::Escape, Modifiers::NONE);

        // Ctrl+F opens search with the previous query selected, so typing replaces it.
        h.app.search.query = "highlight".into();
        h.key(Key::F, Modifiers::COMMAND);
        h.frame(vec![]);
        h.frame(vec![]);
        let state = egui::TextEdit::load_state(&h.ctx, egui::Id::new(crate::search::SEARCH_FIELD)).unwrap();
        let range = state.cursor.char_range().unwrap();
        let (a, b) = (usize::from(range.primary.index), usize::from(range.secondary.index));
        assert_eq!((a.min(b), a.max(b)), (0, 9), "whole query selected");
        h.key(Key::Escape, Modifiers::NONE);

        // The annotations list: page order, highlighted text, notes; clicking selects.
        h.frame(vec![Event::PointerMoved(h.at(80.0, 705.0))]);
        h.wait_until("text chars", |a| a.text_chars.contains_key(&0));
        h.app.set_tool(Tool::Markup(MarkupKind::Highlight));
        h.drag(&[h.at(73.0, 705.0), h.at(150.0, 705.0), h.at(262.0, 705.0)]);
        let mut noted = h.doc().annots[0].clone();
        noted.note = "look here".into();
        h.app.exec(vec![Cmd::Modify { before: h.doc().annots[0].clone(), after: noted.clone() }]);
        let entries = h.app.annotation_entries();
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles.len(), 3, "{titles:?}");
        assert!(titles[0].starts_with("“Hello world please highlight"), "top of the page first: {titles:?}");
        // The other app's square (y 400-500) is above the rectangle (y 300-350).
        assert_eq!(titles[1], "Rectangle  · other app");
        assert_eq!(titles[2], "Rectangle");
        assert_eq!(entries[2].note.as_deref(), Some("look here"));
        let e = entries[2].clone();
        h.app.go_to_annotation(e.page, e.bounds, e.sel.clone());
        assert_eq!((h.app.tool, h.app.selection.clone()), (Tool::Select, Some(Selection::Ours(rect.id.clone()))));
        assert_eq!(h.app.view.back.len(), 1, "Back returns from it");
    }

    #[test]
    fn going_to_an_annotation_from_the_list() {
        let dir = std::env::temp_dir().join(format!("ochre-goto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("goto.pdf");
        text_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);
        // A text box, and a rectangle near the right edge of the page.
        h.app.set_tool(Tool::Text);
        h.click(h.at(100.0, 200.0));
        h.frame(vec![]);
        h.frame(vec![Event::Text("First".into())]);
        h.app.set_tool(Tool::Shape(ShapeKind::Rect));
        h.drag(&[h.at(540.0, 100.0), h.at(560.0, 110.0), h.at(580.0, 120.0)]);
        h.app.set_tool(Tool::Select);
        let entry = |h: &Harness, title: &str| h.app.annotation_entries().into_iter().find(|e| e.title == title).unwrap();

        // Editing the text box, then picking the rectangle in the list: the edit is
        // finished, and typing no longer goes into the text box.
        let text = entry(&h, "First");
        h.app.edit_text(match &text.sel {
            Selection::Ours(id) => id,
            _ => unreachable!(),
        });
        h.frame(vec![]);
        assert!(h.app.editing.is_some());
        let rect = entry(&h, "Rectangle");
        h.app.go_to_annotation(rect.page, rect.bounds, rect.sel.clone());
        assert!(h.app.editing.is_none(), "the text box edit was finished");
        h.frame(vec![Event::Text(" typed later".into())]);
        assert_eq!(entry(&h, "First").title, "First");

        // Zoomed in with the view at the left edge: going to the rectangle on the
        // right also scrolls sideways, so it's actually on screen.
        h.app.view.zoom_by(3.0);
        h.frame(vec![]);
        h.frame(vec![]);
        h.app.view.set_offset = Some(vec2(0.0, h.app.view.offset.y));
        h.frame(vec![]);
        let rect = entry(&h, "Rectangle");
        h.app.go_to_annotation(rect.page, rect.bounds, rect.sel.clone());
        h.wait_until("scroll settles", |a| a.view.scroll_target.is_none());
        h.frame(vec![]);
        let shown = h.app.view.viewport;
        let on_screen = h.at(560.0, 110.0);
        assert!(shown.contains(on_screen), "{on_screen:?} not in {shown:?}");
    }

    #[test]
    fn jumps_land_after_the_layout_changes_and_back_restores_both_axes() {
        let dir = std::env::temp_dir().join(format!("ochre-jump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jump.pdf");
        reading_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(680.0, 500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        // A rectangle near the bottom right of page 3.
        let page = 2;
        h.app.exec(vec![Cmd::Add(Annotation::new(
            page,
            Style::new([0.9, 0.1, 0.1], 2.0, 1.0),
            Kind::Shape { shape: ShapeKind::Rect, a: Pt::new(500.0, 100.0), b: Pt::new(560.0, 140.0) },
        ))]);
        let entry = || -> Box<dyn Fn(&Harness) -> AnnotEntry> {
            Box::new(|h: &Harness| h.app.annotation_entries().into_iter().find(|e| e.title == "Rectangle").unwrap())
        };
        let entry = entry();
        let settle = |h: &mut Harness| {
            for _ in 0..3 {
                h.frame(vec![]);
            }
            h.wait_until("scroll settles", |a| a.view.scroll_target.is_none() && a.view.jump.is_none());
            h.frame(vec![]);
        };
        let visible = |h: &Harness| {
            let g = h.doc().pages[page];
            let on_screen = pos(h.app.view.to_screen(&g, page).apply(Pt::new(530.0, 120.0)));
            (h.app.view.viewport.contains(on_screen), on_screen, h.app.view.viewport)
        };

        // In a narrow window the sidebar closes after picking, so Fit Width rescales
        // the pages while the jump is under way: it still lands on the annotation.
        h.app.cfg.show_outline = true;
        settle(&mut h);
        let zoom_with_sidebar = h.app.view.zoom;
        let e = entry(&h);
        h.app.go_to_annotation(e.page, e.bounds, e.sel.clone());
        h.app.cfg.show_outline = false;
        settle(&mut h);
        assert!(h.app.view.zoom > zoom_with_sidebar, "the pages got wider");
        let (shown, at, vp) = visible(&h);
        assert!(shown, "{at:?} not in {vp:?}");

        // Zoomed in at the left edge: going to it scrolls sideways, and Back
        // returns to the left edge as well as the old line.
        h.key(Key::G, Modifiers::NONE);
        h.key(Key::G, Modifiers::NONE);
        settle(&mut h);
        h.app.view.zoom_by(3.0);
        settle(&mut h);
        h.app.view.set_offset = Some(vec2(0.0, h.app.view.offset.y));
        h.frame(vec![]);
        let before = h.app.view.offset;
        let e = entry(&h);
        h.app.go_to_annotation(e.page, e.bounds, e.sel.clone());
        settle(&mut h);
        assert!(visible(&h).0);
        assert!(h.app.view.offset.x > 100.0, "scrolled sideways");
        h.app.go_back();
        settle(&mut h);
        let after = h.app.view.offset;
        assert!((after - before).length() < 2.0, "{before:?} -> {after:?}");
    }

    #[test]
    fn a_jump_from_the_bottom_still_lands_after_the_sidebar_closes() {
        let dir = std::env::temp_dir().join(format!("ochre-bottom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bottom.pdf");
        reading_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(680.0, 500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        // An annotation near the bottom of the last page.
        let page = 5;
        h.app.exec(vec![Cmd::Add(Annotation::new(
            page,
            Style::new([0.9, 0.1, 0.1], 2.0, 1.0),
            Kind::Shape { shape: ShapeKind::Rect, a: Pt::new(100.0, 40.0), b: Pt::new(200.0, 80.0) },
        ))]);
        let settle = |h: &mut Harness| {
            for _ in 0..3 {
                h.frame(vec![]);
            }
            h.wait_until("scroll settles", |a| a.view.scroll_target.is_none() && a.view.jump.is_none());
            h.frame(vec![]);
        };
        // Sidebar open, scrolled to the very bottom, then pick it (the sidebar closes).
        h.app.cfg.show_outline = true;
        settle(&mut h);
        h.key(Key::G, Modifiers::SHIFT);
        settle(&mut h);
        let e = h.app.annotation_entries().into_iter().find(|e| e.title == "Rectangle").unwrap();
        h.app.go_to_annotation(e.page, e.bounds, e.sel.clone());
        h.app.cfg.show_outline = false;
        settle(&mut h);
        let g = h.doc().pages[page];
        let at = pos(h.app.view.to_screen(&g, page).apply(Pt::new(150.0, 60.0)));
        assert!(h.app.view.viewport.contains(at), "{at:?} not in {:?}", h.app.view.viewport);
    }

    #[test]
    fn page_tools_update_the_view_contents_links_and_thumbnails() {
        use crate::config::SidebarTab;
        let dir = std::env::temp_dir().join(format!("ochre-pagetools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pagetools.pdf");
        reading_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);
        let reloaded = |a: &App| a.sent_generation == a.doc.as_ref().unwrap().generation && a.doc.as_ref().unwrap().links.len() == a.doc.as_ref().unwrap().pages.len();
        let ctx = h.ctx.clone();
        // Before: "Chapter" is page 3, and page 1's link goes to page 3.
        assert_eq!(h.doc().outline[1].target, Some(Target::Page { page: 2, x: None, y: Some(792.0) }));

        // Move page 3 to the front: the contents and the link follow it.
        h.app.move_pages(&ctx, 2, 0);
        h.wait_until("reload", reloaded);
        h.frame(vec![]);
        h.wait_until("outline from the new order", |a| a.doc.as_ref().unwrap().outline.get(1).and_then(|o| o.target.clone()) == Some(Target::Page { page: 0, x: None, y: Some(792.0) }));
        assert_eq!(h.doc().links[1].len(), 1, "the link page is now second");
        assert_eq!(h.doc().links[1][0].target, Target::Page { page: 0, x: None, y: Some(400.0) });
        assert_eq!(h.app.pages_ui.selected, BTreeSet::from([0]), "the moved page stays selected");

        // Rotate it: the view shows it turned.
        h.app.rotate_pages(&ctx, true);
        h.wait_until("rotated", |a| a.doc.as_ref().unwrap().pages[0].rotation == 90 && a.sent_generation == a.doc.as_ref().unwrap().generation);
        h.wait_until("pdfium agrees", |a| a.doc.as_ref().unwrap().pages[0].display_size().0 > 700.0);

        // Thumbnails render in the Pages tab.
        h.app.cfg.show_outline = true;
        h.app.cfg.sidebar_tab = SidebarTab::Pages;
        h.wait_until("thumbnails", |a| a.pages_ui.thumbs.len() >= 2);

        // Delete, then undo.
        h.app.delete_pages(&ctx);
        assert_eq!(h.doc().pages.len(), 5);
        h.wait_until("reload", reloaded);
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.doc().pages.len(), 6);
        h.wait_until("reload", reloaded);
        for _ in 0..3 {
            h.frame(vec![]);
        }
    }

    #[test]
    fn page_selection_extract_and_navigation_edge_cases() {
        let dir = std::env::temp_dir().join(format!("ochre-pagecases-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (path, out) = (dir.join("cases.pdf"), dir.join("extracted.pdf"));
        reading_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);
        let ctx = h.ctx.clone();
        let objs = |h: &Harness| h.doc().layout.iter().map(|s| s.obj).collect::<Vec<_>>();
        let original = objs(&h);

        // Shift-selection after deleting the selected last page: only pages that exist.
        h.app.click_page(5, Modifiers::NONE);
        h.app.delete_pages(&ctx);
        h.frame(vec![]);
        h.app.click_page(0, Modifiers::SHIFT);
        assert_eq!(h.app.pages_ui.selected, BTreeSet::from([0]), "no stale anchor: just the clicked page");
        h.app.click_page(2, Modifiers::SHIFT);
        assert_eq!(h.app.pages_ui.selected, BTreeSet::from([0, 1, 2]));
        assert!(h.app.pages_ui.selected.iter().all(|&p| p < 5));
        h.key(Key::Z, Modifiers::COMMAND);
        h.frame(vec![]);

        // Extract page 1 (the only one with a link); move it to the end while the
        // save dialog is open: page 1 as it was is what's saved, not what's first now.
        h.app.click_page(0, Modifiers::NONE);
        let target = out.clone();
        h.app.extract_pages_with(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            vec![target]
        });
        h.app.move_pages(&ctx, 0, 6);
        h.wait_until("extracted", |_| out.exists());
        h.frame(vec![]);
        let extracted = lopdf::Document::load(&out).unwrap();
        assert_eq!(extracted.get_pages().len(), 1);
        let got = lopdf::Document::load(&out).unwrap();
        let page = *got.get_pages().values().next().unwrap();
        // reading_pdf's pages differ only by page 1's link.
        assert!(got.get_dictionary(page).unwrap().get(b"Annots").is_ok(), "page 1 (with its link), not the page now first");
        assert_ne!(objs(&h), original, "the reorder happened");

        // Editing a text box, then clicking a thumbnail: the edit is finished first.
        h.app.set_tool(Tool::Text);
        h.click(h.at(100.0, 200.0));
        h.frame(vec![]);
        h.frame(vec![Event::Text("Draft".into())]);
        assert!(h.app.editing.is_some());
        h.app.go_to_page(3);
        assert!(h.app.editing.is_none(), "finished before going to the page");
        h.frame(vec![Event::Text(" more".into())]);
        let texts: Vec<String> = h.doc().annots.iter().filter_map(|a| match &a.kind {
            Kind::Text { text, .. } => Some(text.clone()),
            _ => None,
        }).collect();
        assert_eq!(texts, ["Draft"]);
    }

    #[test]
    fn page_textures_never_exceed_the_gpu_limit() {
        let g = PageGeom { bbox: [0.0, 0.0, 612.0, 792.0], rotation: 0 };
        for max_side in [2048, 4096, 16384] {
            for zoom in [1.0, 3.0, 8.0] {
                let s = render_scale(&g, zoom, 2.0, max_side);
                assert!((792.0 * s).ceil() as usize <= max_side, "zoom {zoom}, limit {max_side}: {}", 792.0 * s);
            }
        }
    }

    #[test]
    fn status_bar_fits_a_narrow_window() {
        let dir = std::env::temp_dir().join(format!("ochre-narrow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("narrow.pdf");
        text_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        // The smallest window Ochre allows.
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(480.0, 360.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        for tool in [Tool::Select, Tool::Highlighter, Tool::Markup(MarkupKind::StrikeOut), Tool::Shape(ShapeKind::Arrow)] {
            h.app.set_tool(tool);
            for _ in 0..3 {
                h.frame(vec![]);
            }
            let nav: Rect = h.ctx.data(|d| d.get_temp(egui::Id::new(crate::ui::chrome::STATUS_NAV_RECT))).unwrap();
            assert!(nav.right() <= 480.0 + 0.5, "{tool:?}: page controls end at {}", nav.right());
            let tool_end: f32 = h.ctx.data(|d| d.get_temp(egui::Id::new(crate::ui::chrome::STATUS_TOOL_END))).unwrap();
            assert!(tool_end <= nav.left(), "{tool:?}: tool part ({tool_end}) runs into the page controls ({})", nav.left());
        }
    }

    #[test]
    fn box_selection() {
        let dir = std::env::temp_dir().join(format!("ochre-box-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("box.pdf");
        text_pdf(&path);
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.frame(vec![]);

        // Three rectangles, and a highlight on the first text line.
        h.app.set_tool(Tool::Shape(ShapeKind::Rect));
        for (x, y) in [(100.0, 300.0), (200.0, 300.0), (100.0, 150.0)] {
            h.drag(&[h.at(x, y), h.at(x + 25.0, y + 20.0), h.at(x + 50.0, y + 40.0)]);
        }
        h.app.set_tool(Tool::Markup(MarkupKind::Highlight));
        h.frame(vec![Event::PointerMoved(h.at(80.0, 705.0))]);
        h.wait_until("text chars", |a| a.text_chars.contains_key(&0));
        h.drag(&[h.at(73.0, 705.0), h.at(200.0, 705.0)]);
        let ids: Vec<String> = h.doc().annots.iter().map(|a| a.id.clone()).collect();
        assert_eq!(ids.len(), 4);
        let original = h.doc().annots.clone();

        // A box from empty space catches what it touches.
        h.app.set_tool(Tool::Select);
        h.drag(&[h.at(80.0, 360.0), h.at(150.0, 320.0), h.at(220.0, 320.0)]);
        assert_eq!(h.app.selection, Some(Selection::Many(vec![ids[0].clone(), ids[1].clone()])));
        assert!(h.app.view.annot_rect.is_some(), "group outline and action bar");

        // Dragging one of them moves both, as one undo step.
        h.drag(&[h.at(100.0, 320.0), h.at(110.0, 320.0), h.at(120.0, 320.0)]);
        let left = |h: &Harness, i: usize| match h.doc().annots[i].kind {
            Kind::Shape { a, .. } => a.x,
            _ => panic!(),
        };
        assert!((left(&h, 0) - 120.0).abs() < 1.0 && (left(&h, 1) - 220.0).abs() < 1.0);
        assert!((left(&h, 2) - 100.0).abs() < 1e-3, "unselected stays");
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.doc().annots, original);

        // Shift+click adds the third, and takes it out again.
        h.mods = Modifiers::SHIFT;
        h.click(h.at(100.0, 170.0));
        assert_eq!(h.app.selected_ids(), [ids[0].clone(), ids[1].clone(), ids[2].clone()]);
        h.click(h.at(100.0, 170.0));
        assert_eq!(h.app.selected_ids().len(), 2);
        // Shift+drag adds a box's catch to the selection, even starting over text.
        h.drag(&[h.at(90.0, 710.0), h.at(95.0, 700.0), h.at(100.0, 700.0)]);
        assert_eq!(h.app.selected_ids(), [ids[0].clone(), ids[1].clone(), ids[3].clone()]);
        h.mods = Modifiers::NONE;

        // Without Shift, dragging over text still selects text.
        h.drag(&[h.at(73.0, 685.0), h.at(150.0, 685.0)]);
        assert!(h.app.text_sel.is_some());
        assert_eq!(h.app.selection, None);

        // Ctrl+A selects everything on the page; Delete removes it all, one undo brings it back.
        h.key(Key::A, Modifiers::COMMAND);
        assert_eq!(h.app.selected_ids(), ids);
        h.key(Key::Delete, Modifiers::NONE);
        assert!(h.doc().annots.is_empty());
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.doc().annots, original);

        // A click on empty space clears the selection.
        h.key(Key::A, Modifiers::COMMAND);
        h.click(h.at(450.0, 250.0));
        assert_eq!(h.app.selection, None);
    }

    #[test]
    fn tools_end_to_end() {
        let dir = std::env::temp_dir().join(format!("ochre-ui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.pdf");
        text_pdf(&path);

        let ctx = egui::Context::default();
        let app = App::new(&ctx, Some(path.clone()));
        if app.worker.is_none() {
            eprintln!("pdfium not available, skipping: {:?}", app.worker_error);
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE, size: vec2(1000.0, 1500.0) };
        h.wait_until("pages", |a| a.doc.as_ref().is_some_and(|d| !d.pages.is_empty()));
        h.wait_until("first render", |a| !a.tex.is_empty());
        assert_eq!(h.doc().foreign.len(), 1);

        // Pen: a wavy stroke becomes one smooth Bézier ink annotation.
        h.app.set_tool(Tool::Pen);
        let pts: Vec<Pos2> =
            (0..80).map(|i| h.at(100.0 + i as f32 * 3.0, 500.0 + (i as f32 * 0.15).sin() * 20.0)).collect();
        h.drag(&pts);
        assert_eq!(h.doc().annots.len(), 1);
        let Kind::Ink { curve, .. } = &h.doc().annots[0].kind else { panic!("expected ink") };
        assert!(curve.len() >= 4 && (curve.len() - 1) % 3 == 0, "bezier chain: {}", curve.len());
        assert!(curve.len() < 80, "stroke should be simplified, got {}", curve.len());

        // Rectangle.
        h.app.set_tool(Tool::Shape(ShapeKind::Rect));
        let r = [h.at(100.0, 300.0), h.at(150.0, 325.0), h.at(200.0, 350.0)];
        h.drag(&r);
        let Kind::Shape { a, b, .. } = h.doc().annots[1].kind.clone() else { panic!("expected shape") };
        assert!(a.dist(Pt::new(100.0, 300.0)) < 1.0 && b.dist(Pt::new(200.0, 350.0)) < 1.0, "{a:?} {b:?}");

        // Text box: click, type, click away.
        h.app.set_tool(Tool::Text);
        h.click(h.at(100.0, 200.0));
        assert!(h.app.editing.is_some());
        h.frame(vec![]);
        h.frame(vec![Event::Text("Hi there".into())]);
        h.click(h.at(450.0, 150.0));
        assert!(h.app.editing.is_none());
        let texts: Vec<&str> = h
            .doc()
            .annots
            .iter()
            .filter_map(|a| match &a.kind {
                Kind::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Hi there"]);

        // Select + move the rectangle by its edge, then undo.
        h.app.set_tool(Tool::Select);
        h.drag(&[h.at(100.0, 325.0), h.at(105.0, 325.0), h.at(110.0, 325.0)]);
        let Kind::Shape { a: moved, .. } = h.doc().annots[1].kind.clone() else { panic!() };
        assert!((moved.x - 110.0).abs() < 1.0, "moved to {moved:?}");
        h.key(Key::Z, Modifiers::COMMAND);
        let Kind::Shape { a: back, .. } = h.doc().annots[1].kind.clone() else { panic!() };
        assert!((back.x - 100.0).abs() < 1.0, "undo moved back to {back:?}");

        // Resize the rectangle by its bottom-right handle; undo restores it.
        h.click(h.at(100.0, 325.0));
        let rect = h.doc().annots[1].clone();
        assert_eq!(h.app.selection, Some(Selection::Ours(rect.id.clone())));
        let corner = h.app.handles(&rect)[3];
        assert_eq!(corner.0, Handle::Corner(3));
        h.drag(&[corner.1, corner.1 + vec2(20.0, 10.0), corner.1 + vec2(40.0, 20.0)]);
        let Kind::Shape { a, b, .. } = h.doc().annots[1].kind.clone() else { panic!() };
        let zoom = h.app.view.zoom;
        assert!((a.x - 100.0).abs() < 1.0 && (b.y - 350.0).abs() < 1.0, "top-left stays: {a:?} {b:?}");
        assert!((b.x - 200.0 - 40.0 / zoom).abs() < 2.0, "right edge follows: {b:?}");
        assert!((a.y - 300.0 + 20.0 / zoom).abs() < 2.0, "bottom edge follows: {a:?}");
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.doc().annots[1], rect);

        // Resizing a text box changes its font size, keeping its proportions.
        let text = h.doc().annots.iter().find(|a| matches!(a.kind, Kind::Text { .. })).unwrap().clone();
        h.app.select(Some(Selection::Ours(text.id.clone())));
        h.frame(vec![]);
        let corner = h.app.handles(&text)[3].1;
        h.drag(&[corner, corner + vec2(30.0, 2.0)]);
        let grown = h.doc().get(&text.id).unwrap();
        assert!(grown.style.width > text.style.width * 1.2, "font {} -> {}", text.style.width, grown.style.width);
        h.key(Key::Z, Modifiers::COMMAND);

        // A note: Enter opens the editor, typing and clicking away saves it as one undo step.
        h.app.select(Some(Selection::Ours(rect.id.clone())));
        h.frame(vec![]);
        h.key(Key::Enter, Modifiers::NONE);
        assert!(h.app.note_edit.is_some());
        h.frame(vec![]);
        h.frame(vec![Event::Text("Check this figure".into())]);
        h.click(h.at(450.0, 150.0));
        assert!(h.app.note_edit.is_none());
        assert_eq!(h.doc().get(&rect.id).unwrap().note, "Check this figure");
        assert_eq!(h.app.note_at(badge_center(screen_bounds(&h.doc().annots[1], &h.app.view.to_screen(&h.doc().pages[0], 0)))).as_deref(), Some("Check this figure"));

        // Eraser removes our ink but never touches the foreign annotation.
        h.app.set_tool(Tool::Eraser);
        let across: Vec<Pos2> = (0..40).map(|i| h.at(160.0, 560.0 - i as f32 * 3.0)).collect();
        h.drag(&across);
        assert!(!h.doc().annots.iter().any(|a| matches!(a.kind, Kind::Ink { .. })), "ink erased");
        let over_foreign: Vec<Pos2> = (0..40).map(|i| h.at(390.0 + i as f32 * 3.0, 450.0)).collect();
        h.drag(&over_foreign);
        assert!(h.doc().deleted_foreign.is_empty());

        // Text highlight across the first line.
        h.app.set_tool(Tool::Markup(MarkupKind::Highlight));
        h.frame(vec![Event::PointerMoved(h.at(80.0, 705.0))]);
        h.wait_until("text chars", |a| a.text_chars.contains_key(&0));
        h.drag(&[h.at(73.0, 705.0), h.at(200.0, 705.0), h.at(300.0, 705.0)]);
        let quads = h.doc().annots.iter().find_map(|a| match &a.kind {
            Kind::Markup { quads, .. } => Some(quads.clone()),
            _ => None,
        });
        let quads = quads.expect("markup created");
        assert_eq!(quads.len(), 1);
        assert!(quads[0][0].x < 75.0 && quads[0][1].x > 260.0, "quad {:?}", quads[0]);

        // Foreign annotation: explicit select + Delete hides it; undo brings it back.
        h.app.set_tool(Tool::Select);
        h.click(h.at(450.0, 450.0));
        assert_eq!(h.app.selection, Some(Selection::Foreign(0)));
        let generation = h.doc().generation;
        h.key(Key::Delete, Modifiers::NONE);
        assert!(h.doc().deleted_foreign.contains(&0));
        assert_ne!(h.doc().generation, generation, "pdfium reloads without it");
        h.key(Key::Z, Modifiers::COMMAND);
        assert!(h.doc().deleted_foreign.is_empty());

        // Vim motions: l / h jump a page, G / gg go to the end / start.
        let top_gap = |h: &Harness, page: usize| h.app.view.page_rects[page].top() - h.app.view.viewport.top();
        h.key(Key::L, Modifiers::NONE);
        h.wait_until("l animates to page 2", |a| a.view.scroll_target.is_none());
        h.frame(vec![]);
        assert!((top_gap(&h, 1) - TOP_MARGIN).abs() < 2.0, "page 2 at top: {}", top_gap(&h, 1));
        h.key(Key::H, Modifiers::NONE);
        h.wait_until("h animates back", |a| a.view.scroll_target.is_none());
        h.frame(vec![]);
        assert!(h.app.view.offset.y.abs() < 1.0);
        h.key(Key::G, Modifiers::SHIFT);
        h.wait_until("G scrolls to the end", |a| a.view.scroll_target.is_none());
        let bottom = h.app.view.offset.y;
        assert!(bottom > 500.0);
        h.key(Key::G, Modifiers::NONE);
        h.key(Key::G, Modifiers::NONE);
        h.wait_until("gg scrolls to the top", |a| a.view.scroll_target.is_none());
        h.frame(vec![]);
        assert!(h.app.view.offset.y.abs() < 1.0);
        // Holding j scrolls down; releasing stops.
        let press = |key, pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };
        h.frame(vec![press(Key::J, true)]);
        for _ in 0..10 {
            h.frame(vec![]);
        }
        h.frame(vec![press(Key::J, false)]);
        let after_j = h.app.view.offset.y;
        assert!(after_j > 50.0, "j scrolled {after_j}");
        h.frame(vec![]);
        assert_eq!(h.app.view.offset.y, after_j);
        // Shift+H is the highlighter now.
        h.key(Key::H, Modifiers::SHIFT);
        assert_eq!(h.app.tool, Tool::Highlighter);
        h.app.set_tool(Tool::Select);

        // `/` search: smartcase, finds text across pdfium's line break, n steps
        // to the match on the next page.
        h.frame(vec![Event::Text("/".into())]);
        assert!(h.app.search.open);
        assert!(h.app.search.query.is_empty(), "the / itself isn't typed into the query");
        h.frame(vec![]);
        h.frame(vec![Event::Text("highlight me a second".into())]);
        h.wait_until("search matches on all pages", |a| a.search.total() == 3);
        assert_eq!(h.app.search.current_rects().unwrap().1.len(), 2, "one rect per line");
        h.key(Key::Enter, Modifiers::NONE);
        h.frame(vec![]);
        assert!(!h.ctx.egui_wants_keyboard_input(), "Enter returns to normal mode");
        let first = h.app.search.current.unwrap();
        h.key(Key::N, Modifiers::NONE);
        assert_eq!(h.app.search.current, Some(first + 1));
        h.key(Key::N, Modifiers::SHIFT);
        assert_eq!(h.app.search.current, Some(first));
        h.key(Key::Escape, Modifiers::NONE);
        assert!(!h.app.search.open);

        // Panning keeps up with fast drags: several moves in one frame all count.
        h.key(Key::G, Modifiers::NONE);
        h.key(Key::G, Modifiers::NONE);
        h.wait_until("back at the top", |a| a.view.scroll_target.is_none());
        h.app.set_tool(Tool::Hand);
        let start = h.at(300.0, 300.0);
        h.frame(vec![Event::PointerMoved(start)]);
        h.button(start, true);
        let before = h.app.view.offset.y;
        h.frame((1..=5).map(|i| Event::PointerMoved(start - vec2(0.0, 20.0 * i as f32))).collect());
        h.button(start - vec2(0.0, 100.0), false);
        h.frame(vec![]);
        let panned = h.app.view.offset.y - before;
        assert!((panned - 100.0).abs() < 1.0, "panned {panned}px for a 100px drag");
        h.key(Key::G, Modifiers::NONE);
        h.key(Key::G, Modifiers::NONE);
        h.wait_until("top again", |a| a.view.scroll_target.is_none());
        h.frame(vec![]);

        // Select tool selects page text; Ctrl+C copies it; it can become a highlight.
        h.app.set_tool(Tool::Select);
        h.frame(vec![Event::PointerMoved(h.at(80.0, 705.0))]);
        h.wait_until("text chars", |a| a.text_chars.contains_key(&0));
        h.drag(&[h.at(73.0, 705.0), h.at(150.0, 705.0), h.at(262.0, 705.0)]);
        let text = h.app.selected_text().unwrap_or_default();
        assert!(text.starts_with("Hello world please highlight"), "selected {text:?}");
        h.frame(vec![Event::Copy]);
        assert!(h.app.status.as_ref().is_some_and(|(m, _, _)| m.starts_with("Copied")));
        let n = h.doc().annots.len();
        h.app.markup_selection(MarkupKind::Highlight);
        assert_eq!(h.doc().annots.len(), n + 1);
        assert!(h.app.text_sel.is_none());
        // Double-click selects a word.
        h.click(h.at(120.0, 705.0));
        h.click(h.at(120.0, 705.0));
        assert_eq!(h.app.selected_text().as_deref(), Some("world"));
        h.key(Key::Escape, Modifiers::NONE);
        assert!(h.app.text_sel.is_none());

        // A filled rectangle: the fill is clickable and survives saving.
        let mut style = h.app.tool_style(Tool::Shape(ShapeKind::Rect));
        style.fill = Some([0.1, 0.35, 0.9]);
        style.fill_opacity = 0.3;
        h.app.cfg.styles.insert(Tool::Shape(ShapeKind::Rect).key(), style);
        h.app.set_tool(Tool::Shape(ShapeKind::Rect));
        h.drag(&[h.at(300.0, 150.0), h.at(350.0, 170.0), h.at(380.0, 190.0)]);
        let filled = h.doc().annots.last().unwrap().clone();
        assert_eq!(filled.style.fill, Some([0.1, 0.35, 0.9]));
        h.app.set_tool(Tool::Select);
        h.click(h.at(340.0, 170.0));
        assert_eq!(h.app.selection, Some(Selection::Ours(filled.id.clone())));

        // Smart highlighter: on text it highlights the text in straight lines, in
        // the highlighter's color; on blank space (or with Alt) it's freehand.
        h.app.set_tool(Tool::Highlighter);
        let hl_style = h.app.tool_style(Tool::Highlighter);
        let n = h.doc().annots.len();
        h.drag(&[h.at(73.0, 685.0), h.at(120.0, 690.0), h.at(200.0, 680.0)]);
        let a = h.doc().annots.last().unwrap().clone();
        assert_eq!(h.doc().annots.len(), n + 1);
        let Kind::Markup { markup: MarkupKind::Highlight, quads } = &a.kind else { panic!("expected text highlight, got {:?}", a.kind) };
        assert_eq!(quads.len(), 1, "one straight line");
        assert_eq!((a.style.color, a.style.opacity), (hl_style.color, hl_style.opacity));
        h.drag(&[h.at(100.0, 600.0), h.at(150.0, 620.0), h.at(200.0, 600.0)]);
        assert!(matches!(h.doc().annots.last().unwrap().kind, Kind::Ink { highlighter: true, .. }), "freehand off text");
        let alt = Modifiers { alt: true, ..Modifiers::NONE };
        let start = h.at(73.0, 685.0);
        h.frame(vec![Event::PointerMoved(start)]);
        h.frame(vec![Event::PointerButton { pos: start, button: PointerButton::Primary, pressed: true, modifiers: alt }]);
        h.frame(vec![Event::PointerMoved(h.at(150.0, 700.0))]);
        h.button(h.at(150.0, 700.0), false);
        assert!(matches!(h.doc().annots.last().unwrap().kind, Kind::Ink { highlighter: true, .. }), "Alt forces freehand");

        // Tick: a single click places a standard-size, upright mark.
        h.app.set_tool(Tool::Shape(ShapeKind::Check));
        h.click(h.at(480.0, 300.0));
        let tick = h.doc().annots.last().unwrap().clone();
        let Kind::Shape { shape: ShapeKind::Check, a, b } = tick.kind else { panic!("expected a tick") };
        assert!((b.x - a.x - 15.0).abs() < 0.5 && (a.y - b.y - 15.0).abs() < 0.5, "box {a:?} {b:?}");
        assert!(a.lerp(b, 0.5).dist(Pt::new(480.0, 300.0)) < 1.0);
        assert_eq!(tick.style.color, [0.15, 0.62, 0.25], "ticks default to green");

        // Lines and arrows are changed by dragging an end.
        h.app.set_tool(Tool::Shape(ShapeKind::Arrow));
        h.drag(&[h.at(300.0, 250.0), h.at(350.0, 250.0), h.at(400.0, 250.0)]);
        let arrow = h.doc().annots.last().unwrap().clone();
        h.app.set_tool(Tool::Select);
        h.app.select(Some(Selection::Ours(arrow.id.clone())));
        h.frame(vec![]);
        let handles = h.app.handles(&arrow);
        assert_eq!(
            handles.iter().map(|x| x.0).collect::<Vec<_>>(),
            [Handle::Endpoint(0), Handle::Endpoint(1), Handle::Rotate]
        );
        h.drag(&[handles[1].1, h.at(400.0, 230.0), h.at(400.0, 200.0)]);
        let Kind::Shape { a, b, .. } = h.doc().get(&arrow.id).unwrap().kind else { panic!() };
        assert!(a.dist(Pt::new(300.0, 250.0)) < 1.0 && b.dist(Pt::new(400.0, 200.0)) < 1.0, "{a:?} {b:?}");

        // Save and reopen: everything is there, foreign annotation untouched.
        let ctx = h.ctx.clone();
        assert!(h.app.save(&ctx, false));
        let reopened = Doc::open(&path).unwrap();
        assert_eq!(reopened.annots, h.doc().annots);
        assert_eq!(reopened.foreign.len(), 1);
        h.wait_until("reload after save", |a| a.sent_generation == a.doc.as_ref().unwrap().generation);
        h.frame(vec![]);
    }
}

