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
    /// Rotated counter-clockwise (y up) by `angle` radians about the origin.
    pub fn rotate(self, angle: f32) -> Pt {
        let (s, c) = angle.sin_cos();
        Pt::new(self.x * c - self.y * s, self.x * s + self.y * c)
    }
    pub fn rotate_about(self, center: Pt, angle: f32) -> Pt {
        self.sub(center).rotate(angle).add(center)
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
    /// Rotation (radians, counter-clockwise on screen) of a rectangle, ellipse, tick or
    /// cross about the center of its box `a`-`b`. Other kinds are rotated by moving
    /// their points instead, so it stays 0 for them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub angle: f32,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

impl Annotation {
    pub fn new(page: usize, style: Style, kind: Kind) -> Self {
        Self { id: new_id(), page, style, kind, note: String::new(), angle: 0.0 }
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

    /// Whether this is a box shape that rotates with [`Annotation::angle`].
    pub fn is_box(&self) -> bool {
        matches!(
            self.kind,
            Kind::Shape { shape: ShapeKind::Rect | ShapeKind::Ellipse | ShapeKind::Check | ShapeKind::Cross, .. }
        )
    }

    /// Center of rotation: the middle of a box shape's box, else of the bounds.
    pub fn center(&self) -> Pt {
        match &self.kind {
            Kind::Shape { a, b, .. } => a.lerp(*b, 0.5),
            _ => {
                let [x0, y0, x1, y1] = crate::annot::geometry::bounds(self);
                Pt::new((x0 + x1) / 2.0, (y0 + y1) / 2.0)
            }
        }
    }

    /// Rotated by `delta` radians about `c`. Box shapes change their angle; other
    /// kinds have their points (and a text box its direction) rotated. Text markup
    /// follows the page text and isn't rotated.
    pub fn rotated(&self, c: Pt, delta: f32) -> Annotation {
        let mut out = self.clone();
        if matches!(self.kind, Kind::Markup { .. }) {
            return out;
        }
        if self.is_box() {
            // The box keeps its shape; its center moves around `c` and it turns.
            let center = self.center();
            out.translate(center.rotate_about(c, delta).sub(center));
            out.angle = self.angle + delta;
        } else {
            out.map_points(|p| p.rotate_about(c, delta));
        }
        if let Kind::Text { right, down, .. } = &mut out.kind {
            *right = right.rotate(delta);
            *down = down.rotate(delta);
        }
        out
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
    /// Its object number (and those of its popups), which stay the same across
    /// our saves. None if it's written inline in the page's `/Annots` array.
    pub obj: Option<(u32, u16)>,
    pub popup_objs: Vec<(u32, u16)>,
    /// Whether it's in the page's `/Annots` in the loaded file. A deleted one stays
    /// known, detached, after saving, so undo can put it back.
    pub attached: bool,
    /// Its `/NM` (unique name), if it has one.
    pub nm: Option<String>,
    /// A text box's text (its `/Contents`, which isn't a comment there).
    pub text: Option<String>,
    /// The page object it's on.
    pub page_obj: (u32, u16),
}

impl Foreign {
    /// What kind of annotation it is, in plain words.
    pub fn kind_name(&self) -> &str {
        match self.subtype.as_str() {
            "FreeText" => "Text box",
            "Text" => "Sticky note",
            "Ink" => "Drawing",
            "Square" => "Rectangle",
            "Circle" => "Ellipse",
            "StrikeOut" => "Strike-out",
            "PolyLine" => "Polyline",
            "FileAttachment" => "Attachment",
            other => other,
        }
    }
}

/// A page of the document as shown: which page object, turned how far. Page
/// objects keep their numbers across our saves, so a layout stays meaningful
/// after saving (and a deleted page can be put back).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageSlot {
    pub obj: (u32, u16),
    /// Clockwise: 0, 90, 180 or 270.
    pub rotation: u16,
}

/// What identifies an annotation in a file apart from its object number, which a
/// program that rewrites the file may change or reuse for something else.
#[derive(Clone, Debug, PartialEq)]
pub struct AnnotIdentity {
    pub subtype: String,
    pub nm: Option<String>,
    /// `[x0, y0, x1, y1]` in user space.
    pub rect: [f32; 4],
}

impl AnnotIdentity {
    /// Whether two describe the same annotation: same unique name if both have one,
    /// otherwise same type and position.
    pub fn same(&self, other: &AnnotIdentity) -> bool {
        if self.subtype != other.subtype {
            return false;
        }
        match (&self.nm, &other.nm) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.rect.iter().zip(other.rect).all(|(a, b)| (a - b).abs() < 0.01),
            _ => false,
        }
    }
}

impl Foreign {
    pub fn identity(&self) -> AnnotIdentity {
        AnnotIdentity { subtype: self.subtype.clone(), nm: self.nm.clone(), rect: self.rect }
    }
}
