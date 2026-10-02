//! Annotation model. All coordinates are in PDF user space (points, y up),
//! so stored data never depends on zoom or page rotation.

use serde::{Deserialize, Serialize};

/// Prefix of the `/NM` entry of every annotation this app writes. Annotations
/// without it (or without our private data) belong to other software and are
/// never rewritten.
pub const NM_PREFIX: &str = "ochre-";
/// Prefix used before the app was renamed; still recognized as ours.
pub const LEGACY_NM_PREFIX: &str = "inkpdf-";

#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

impl Pt {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    pub fn dist(self, o: Pt) -> f32 {
        ((self.x - o.x).powi(2) + (self.y - o.y).powi(2)).sqrt()
    }
    pub fn add(self, o: Pt) -> Pt {
        Pt::new(self.x + o.x, self.y + o.y)
    }
    pub fn sub(self, o: Pt) -> Pt {
        Pt::new(self.x - o.x, self.y - o.y)
    }
    pub fn scale(self, s: f32) -> Pt {
        Pt::new(self.x * s, self.y * s)
    }
    pub fn dot(self, o: Pt) -> f32 {
        self.x * o.x + self.y * o.y
    }
    pub fn len(self) -> f32 {
        self.dot(self).sqrt()
    }
    pub fn normalized(self) -> Pt {
        let l = self.len();
        if l > 1e-9 { self.scale(1.0 / l) } else { Pt::default() }
    }
    pub fn lerp(self, o: Pt, t: f32) -> Pt {
        self.add(o.sub(self).scale(t))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Style {
    /// RGB in 0..=1.
    pub color: [f32; 3],
    /// Line width in points (font size for text).
    pub width: f32,
    /// 0..=1
    pub opacity: f32,
    /// Interior color of rectangles and ellipses (None = no fill).
    #[serde(default)]
    pub fill: Option<[f32; 3]>,
    /// 0..=1, independent of the outline's opacity.
    #[serde(default = "default_fill_opacity")]
    pub fill_opacity: f32,
}

fn default_fill_opacity() -> f32 {
    0.25
}

impl Style {
    pub const fn new(color: [f32; 3], width: f32, opacity: f32) -> Self {
        Self { color, width, opacity, fill: None, fill_opacity: 0.25 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShapeKind {
    Rect,
    Ellipse,
    Line,
    Arrow,
    /// Tick mark in the box `a`-`b` (see [`crate::annot::geometry::mark_strokes`]).
    Check,
    Cross,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarkupKind {
    Highlight,
    Underline,
    StrikeOut,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Kind {
    /// Freehand stroke as a cubic Bézier chain: `[p0, c1, c2, p1, c1, c2, p2, ...]`
    /// (length `3n + 1`). A single point is a dot.
    Ink { curve: Vec<Pt>, highlighter: bool },
    /// Text box. `origin` is the top-left corner as seen on screen, `right`/`down`
    /// are unit vectors in user space pointing right/down on screen, so text stays
    /// upright on rotated pages. `style.width` is the font size.
    Text { origin: Pt, right: Pt, down: Pt, text: String },
    Shape { shape: ShapeKind, a: Pt, b: Pt },
    /// Quads as `[upper-left, upper-right, lower-left, lower-right]` (PDF QuadPoints order).
    Markup { markup: MarkupKind, quads: Vec<[Pt; 4]> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: String,
    pub page: usize,
    pub style: Style,
    pub kind: Kind,
    /// Comment attached to the annotation (written as `/Contents`). Unused for text boxes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl Annotation {
    pub fn new(page: usize, style: Style, kind: Kind) -> Self {
        Self { id: new_id(), page, style, kind, note: String::new() }
    }

    /// Applies `f` to every point. Direction vectors (a text box's `right`/`down`) are kept.
    pub fn map_points(&mut self, f: impl Fn(Pt) -> Pt) {
        let mv = |p: &mut Pt| *p = f(*p);
        match &mut self.kind {
            Kind::Ink { curve, .. } => curve.iter_mut().for_each(mv),
            Kind::Text { origin, .. } => mv(origin),
            Kind::Shape { a, b, .. } => {
                mv(a);
                mv(b);
            }
            Kind::Markup { quads, .. } => quads.iter_mut().flatten().for_each(mv),
        }
    }

    pub fn translate(&mut self, d: Pt) {
        self.map_points(|p| p.add(d));
    }

    /// Whether a note can be attached (text boxes are text already).
    pub fn takes_note(&self) -> bool {
        !matches!(self.kind, Kind::Text { .. })
    }
}

pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{NM_PREFIX}{t:x}-{n:x}-{:x}", std::process::id())
}

/// An annotation created by other software. We only remember enough to show,
/// select and (on explicit request) delete it; its PDF object is never rewritten.
#[derive(Clone, Debug)]
pub struct Foreign {
    pub page: usize,
    /// Index in the page's `/Annots` array in the loaded file.
    pub index: usize,
    /// `/Annots` indices of its popup(s), removed together with it.
    pub popups: Vec<usize>,
    pub subtype: String,
    /// `[x0, y0, x1, y1]` in user space.
    pub rect: [f32; 4],
    /// Hidden widgets, links and popups are not selectable.
    pub selectable: bool,
    /// Its `/Contents` comment, shown read-only.
    pub note: Option<String>,
}
