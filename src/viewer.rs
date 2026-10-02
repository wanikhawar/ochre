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
use crate::pdf::worker::TextChar;

const MARGIN: f32 = 24.0;
/// Room above the first page and below the last for the floating bars.
const TOP_MARGIN: f32 = 24.0;
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
    exclude: Option<String>,
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

pub enum Gesture {
    None,
    Ink { page: usize, brush: LazyBrush, raw: Vec<Pt>, highlighter: bool, style: Style },
    Shape { page: usize, shape: ShapeKind, a: Pt, b: Pt, style: Style },
    Markup { page: usize, markup: MarkupKind, start: usize, end: usize, style: Style },
    Erase { removed: Vec<Cmd> },
    Move { before: Annotation, start: Pt, current: Annotation },
    Pan { last: Pos2 },
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fit {
    Width,
    Page,
}

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
    /// Time and place of the last primary press, for double clicks.
    last_press: Option<(f64, Pos2)>,
    pub goto_page: Option<usize>,
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
            last_press: None,
            goto_page: None,
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
        let r = self.page_rects[i];
        g.to_user().apply(Pt::new((p.x - r.min.x) / self.zoom, (p.y - r.min.y) / self.zoom))
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

fn render_scale(g: &PageGeom, zoom: f32, ppp: f32) -> f32 {
    let (w, h) = g.display_size();
    let want = zoom * ppp;
    let cap = (MAX_PIXELS / (w * h).max(1.0)).sqrt().min(MAX_SIDE / w.max(h).max(1.0));
    // Quantize so tiny zoom jitter doesn't trigger re-renders.
    ((want.min(cap) * 100.0).round() / 100.0).max(0.05)
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
        if let Some(t) = target {
            self.view.scroll_target = Some(clamp(t));
        }

        let dt = ctx.input(|i| i.stable_dt).min(1.0 / 20.0);
        // Wheel or scrollbar input cancels an animated jump.
        if ctx.input(|i| i.smooth_scroll_delta != Vec2::ZERO) {
            self.view.scroll_target = None;
        }
        // Holding j / k: continuous, frame-rate independent scrolling.
        let (j, k) = ctx.input(|i| {
            let plain = !i.modifiers.any();
            (plain && i.key_down(Key::J), plain && i.key_down(Key::K))
        });
        if !typing && (j != k) {
            self.view.scroll_target = None;
            let dir = if j { 1.0 } else { -1.0 };
            self.view.set_offset = Some(clamp(self.view.offset + vec2(0.0, dir * 1100.0 * dt)));
            ctx.request_repaint();
        } else if let Some(t) = self.view.scroll_target {
            let cur = self.view.offset;
            let next = cur + (t - cur) * (1.0 - (-dt * 22.0).exp());
            if (t - next).length() < 0.5 {
                self.view.set_offset = Some(t);
                self.view.scroll_target = None;
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
        let settled = now - self.view.zoom_changed_at > 0.15;
        if !settled {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(160));
        }
        let exclude = match &self.gesture {
            Gesture::Move { before, .. } => Some(before.id.clone()),
            _ => None,
        };

        for &i in &visible {
            let rect = self.view.page_rects[i];
            painter.add(page_shadow.as_shape(rect, 0));
            painter.rect_filled(rect, 0.0, Color32::WHITE);
            let want = render_scale(&pages[i], self.view.zoom, ppp);
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
            self.paint_overlay(&painter, i, rect, want, settled, exclude.as_deref(), ui.ctx());
            self.paint_texts(&painter, i);
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
        exclude: Option<&str>,
        ctx: &egui::Context,
    ) {
        let Some(doc) = &self.doc else { return };
        let annots: Vec<&Annotation> = doc
            .annots
            .iter()
            .filter(|a| a.page == page && Some(a.id.as_str()) != exclude && !matches!(a.kind, Kind::Text { .. }))
            .collect();
        if annots.is_empty() {
            self.overlay.remove(&page);
            return;
        }
        let rev = doc.page_rev.get(page).copied().unwrap_or(0);
        let fresh = self.overlay.get(&page).is_some_and(|o| {
            o.rev == rev && o.exclude.as_deref() == exclude && ((o.scale - want).abs() < 0.005 || !settled)
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
                        o.exclude = exclude.map(str::to_owned);
                    }
                    None => {
                        let tex = ctx.load_texture(format!("overlay{page}"), image, opts);
                        self.overlay.insert(page, Overlay { tex, scale: want, rev, exclude: exclude.map(str::to_owned) });
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
        let moving = match &self.gesture {
            Gesture::Move { current, .. } => Some(current),
            _ => None,
        };
        for a in doc.annots.iter().filter(|a| a.page == page) {
            if Some(a.id.as_str()) == editing {
                continue;
            }
            let a = match moving {
                Some(m) if m.id == a.id => m,
                _ => a,
            };
            if let Kind::Text { origin, right, down, text } = &a.kind {
                draw_text(painter, &aff, self.view.zoom, *origin, *right, *down, text, &a.style);
            }
        }
    }

    fn paint_selection(&self, painter: &Painter) {
        let Some(doc) = &self.doc else { return };
        let accent = crate::ui::theme::ACCENT;
        // Our annotations: solid outline with corner handles.
        let outline = |r: Rect, c: Color32| {
            let r = r.expand(4.0);
            painter.rect_stroke(r, 2.0, Stroke::new(1.25, c), StrokeKind::Middle);
            for corner in [r.left_top(), r.right_top(), r.left_bottom(), r.right_bottom()] {
                let h = Rect::from_center_size(corner, vec2(7.0, 7.0));
                painter.rect_filled(h, 1.5, Color32::WHITE);
                painter.rect_stroke(h, 1.5, Stroke::new(1.25, c), StrokeKind::Middle);
            }
        };
        // Other apps' annotations: dashed, without handles (they can't be moved).
        let dashed = |r: Rect, c: Color32| {
            let r = r.expand(4.0);
            let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
            painter.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.5, c), 5.0, 3.0));
        };
        match &self.selection {
            Some(Selection::Ours(id)) => {
                let a = match &self.gesture {
                    Gesture::Move { current, .. } if current.id == *id => Some(current),
                    _ => doc.get(id),
                };
                if let Some(a) = a {
                    outline(screen_bounds(a, &self.view.to_screen(&doc.pages[a.page], a.page)), accent);
                }
            }
            Some(Selection::Foreign(i)) => {
                if let Some(f) = doc.foreign.get(*i) {
                    let aff = self.view.to_screen(&doc.pages[f.page], f.page);
                    let [x0, y0, x1, y1] = f.rect;
                    let r = Rect::from_two_pos(pos(aff.apply(Pt::new(x0, y0))), pos(aff.apply(Pt::new(x1, y1))));
                    dashed(r, crate::ui::theme::WARN);
                }
            }
            None => {}
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
            }),
            Gesture::Shape { page, shape, a, b, style } => {
                let (a, b) = shape_box(&doc.pages[*page], *shape, *a, *b, style, self.view.zoom);
                Some(Annotation { id: String::new(), page: *page, style: *style, kind: Kind::Shape { shape: *shape, a, b } })
            }
            Gesture::Markup { page, markup, start, end, style } => {
                let chars = self.text_chars.get(page).map(Vec::as_slice).unwrap_or_default();
                let quads = markup_quads(chars, *start, *end, &doc.pages[*page]);
                (!quads.is_empty()).then(|| Annotation {
                    id: String::new(),
                    page: *page,
                    style: *style,
                    kind: Kind::Markup { markup: *markup, quads },
                })
            }
            Gesture::Move { current, .. } if !matches!(current.kind, Kind::Text { .. }) => Some(current.clone()),
            _ => None,
        };
        let Some(a) = live else { return };
        let aff = self.view.to_screen(&doc.pages[a.page], a.page);
        let bbox = screen_bounds(&a, &aff).expand(2.0).intersect(painter.clip_rect());
        if !bbox.is_positive() {
            return;
        }
        let (pw, ph) = ((bbox.width() * ppp).ceil() as u32, (bbox.height() * ppp).ceil() as u32);
        let Some(mut pm) = tiny_skia::Pixmap::new(pw.max(1), ph.max(1)) else { return };
        let t = Affine {
            a: aff.a * ppp,
            b: aff.b * ppp,
            c: aff.c * ppp,
            d: aff.d * ppp,
            e: (aff.e - bbox.min.x) * ppp,
            f: (aff.f - bbox.min.y) * ppp,
        };
        raster::draw(&mut pm.as_mut(), &a, t.to_skia());
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
                    if button == PointerButton::Middle || (button == PointerButton::Primary && (panning_key || self.tool == Tool::Hand)) {
                        self.commit_text();
                        self.gesture = Gesture::Pan { last: pos };
                    } else if button == PointerButton::Primary {
                        self.on_press(ui, pos, modifiers);
                    }
                }
                Event::PointerMoved(pos) => self.on_move(pos, ctx.input(|i| i.modifiers)),
                Event::PointerButton { pos, pressed: false, .. } => self.on_release(pos),
                Event::PointerGone => self.on_release(Pos2::ZERO),
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
                _ if panning_key || self.tool == Tool::Hand => CursorIcon::Grab,
                (Gesture::Move { .. }, _) => CursorIcon::Grabbing,
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
                    Some(p) if self.hit(p).is_some() => CursorIcon::Move,
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
                f.page == page
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
        self.commit_text();
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
                let hit = self.hit(p);
                self.text_sel = None;
                // Text markup sits on the text: dragging over it selects text, and a
                // plain click selects the markup. Other annotations move when dragged.
                let movable = match &hit {
                    Some(Selection::Ours(id)) => doc.get(id).filter(|a| !matches!(a.kind, Kind::Markup { .. })).cloned(),
                    _ => None,
                };
                if let Some(a) = &movable {
                    if double && matches!(a.kind, Kind::Text { .. }) {
                        self.edit_text(&a.id);
                    } else {
                        self.gesture = Gesture::Move { start: u, current: a.clone(), before: a.clone() };
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
        let Some(doc) = &self.doc else { return };
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
            Gesture::Move { before, start, current } => {
                let u = self.view.to_user(&doc.pages[before.page], before.page, p);
                let mut moved = before.clone();
                moved.translate(u.sub(*start));
                *current = moved;
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

    fn on_release(&mut self, _p: Pos2) {
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
            Gesture::Move { before, current, .. } => {
                if before != current {
                    self.exec(vec![Cmd::Modify { before, after: current }]);
                }
            }
            Gesture::SelectText { click } => {
                // A plain click (no drag) selects no text, but whatever annotation was clicked.
                if self.text_sel.is_some_and(|s| s.start == s.end) {
                    self.text_sel = None;
                    self.select(click);
                }
            }
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

    fn edit_text(&mut self, id: &str) {
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

    pub fn commit_text(&mut self) {
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
            self.commit_text();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(painter: &Painter, aff: &Affine, zoom: f32, origin: Pt, right: Pt, down: Pt, text: &str, style: &Style) {
    let size = style.width;
    let font = FontId::new(size * zoom, egui::FontFamily::Name("annot".into()));
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
        painter.galley(pos2(base.x, base.y - ascent), galley, color(style));
    }
}

/// Drives the real UI headless (egui frames with synthetic input) against pdfium.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;
    use eframe::egui::{Modifiers, RawInput};

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
    }

    impl Harness {
        fn frame(&mut self, events: Vec<Event>) {
            self.t += 1.0 / 60.0;
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 1500.0))),
                time: Some(self.t),
                events,
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

        fn key(&mut self, key: Key, modifiers: Modifiers) {
            self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
        }

        fn doc(&self) -> &Doc {
            self.app.doc.as_ref().unwrap()
        }
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
            eprintln!("pdfium not available, skipping");
            return;
        }
        let mut h = Harness { ctx, app, t: 0.0, mods: Modifiers::NONE };
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
