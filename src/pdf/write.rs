//! Converts model annotations into standard PDF annotation dictionaries with
//! appearance streams (`/AP`), so every viewer draws them the way we do.

use std::fmt::Write as _;

use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat, dictionary};
use serde::{Deserialize, Serialize};

use crate::annot::geometry::{
    TEXT_ASCENT, TEXT_LINE_HEIGHT, TEXT_PAD, arrow_head, bounds, box_corners, flatten_bezier, mark_lines,
    mark_strokes, text_lines,
};
use crate::annot::model::{Annotation, Kind, MarkupKind, Pt, ShapeKind};
use crate::annot::raster::{markup_line, markup_thickness};

/// Private key holding our editable model data as JSON.
pub const PRIVATE_KEY: &[u8] = b"OchreData";
/// Key used before the app was renamed (InkPDF); still read.
pub const LEGACY_PRIVATE_KEY: &[u8] = b"InkPDFData";

/// What we store under [`PRIVATE_KEY`]. `rect` and `written` record the
/// annotation as we wrote it: if another app later edits it, they no longer match
/// and we leave it to that app.
#[derive(Serialize, Deserialize)]
pub struct Private {
    pub v: u32,
    pub rect: [f32; 4],
    pub annot: Annotation,
    /// Absent in files saved before 1.2.1, which are checked by `/Rect` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<Fingerprint>,
}

/// Entries other viewers change when they edit an annotation: its comment, color,
/// modification date and appearance (which they redraw). The appearance is a hash
/// of its drawing, so a program that just renumbers or recompresses the file's
/// objects when saving doesn't count as an edit.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Fingerprint {
    pub contents: Option<String>,
    pub color: Option<Vec<f32>>,
    pub modified: Option<String>,
    pub appearance: Option<String>,
}

/// FNV-1a: a hash that stays the same across Rust versions and platforms.
fn stable_hash(bytes: &[u8]) -> String {
    let h = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    format!("{h:016x}")
}

/// Hash of the drawing of an annotation's normal appearance stream.
pub fn appearance_hash(doc: &Document, d: &Dictionary) -> Option<String> {
    let id = d.get(b"AP").and_then(Object::as_dict).ok()?.get(b"N").and_then(Object::as_reference).ok()?;
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let content = stream.decompressed_content().unwrap_or_else(|_| stream.content.clone());
    Some(stable_hash(&content))
}

impl Fingerprint {
    /// `appearance` is the [`appearance_hash`] of `d`.
    pub fn of(d: &Dictionary, appearance: Option<String>) -> Fingerprint {
        Fingerprint {
            contents: d.get(b"Contents").and_then(Object::as_str).ok().map(decode_text_string),
            color: d
                .get(b"C")
                .and_then(Object::as_array)
                .ok()
                .map(|a| a.iter().filter_map(|v| v.as_float().ok()).collect()),
            modified: d.get(b"M").and_then(Object::as_str).ok().map(|m| String::from_utf8_lossy(m).into_owned()),
            appearance,
        }
    }

    /// Equal, allowing for number rounding in the file.
    pub fn same(&self, other: &Fingerprint) -> bool {
        let colors = match (&self.color, &other.color) {
            (Some(a), Some(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3),
            (a, b) => a.is_none() && b.is_none(),
        };
        colors && self.contents == other.contents && self.modified == other.modified && self.appearance == other.appearance
    }
}

fn num(v: f32) -> String {
    let s = format!("{:.3}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" || s.is_empty() { "0".into() } else { s.into() }
}

fn real(v: f32) -> Object {
    Object::Real(v)
}

fn reals(vs: impl IntoIterator<Item = f32>) -> Object {
    Object::Array(vs.into_iter().map(real).collect())
}

/// PDF text string as UTF-16BE with BOM.
pub fn text_string(s: &str) -> Object {
    let mut bytes = vec![0xFE, 0xFF];
    for u in s.encode_utf16() {
        bytes.extend(u.to_be_bytes());
    }
    Object::String(bytes, StringFormat::Hexadecimal)
}

/// Decodes a PDF text string (UTF-16BE with BOM, UTF-8 with BOM, or PDFDocEncoding ~ Latin-1).
pub fn decode_text_string(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(rest).into_owned()
    } else {
        b.iter().map(|&c| c as char).collect()
    }
}

fn pdf_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("D:{y:04}{m:02}{d:02}{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Maps a char to WinAnsiEncoding (what the standard Helvetica font uses).
fn win_ansi(c: char) -> u8 {
    match c as u32 {
        v @ (32..=126 | 0xA0..=0xFF) => v as u8,
        _ => match c {
            '€' => 0x80,
            '…' => 0x85,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            '–' => 0x96,
            '—' => 0x97,
            '\t' => b' ',
            _ => b'?',
        },
    }
}

fn pdf_literal(s: &str) -> Vec<u8> {
    let mut out = vec![b'('];
    for c in s.chars() {
        let b = win_ansi(c);
        if matches!(b, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        if b >= 0x80 {
            out.extend(format!("\\{:03o}", b).bytes());
        } else {
            out.push(b);
        }
    }
    out.push(b')');
    out
}

fn bezier_ops(curve: &[Pt], out: &mut String) {
    let Some(p0) = curve.first() else { return };
    let _ = writeln!(out, "{} {} m", num(p0.x), num(p0.y));
    if curve.len() == 1 {
        let _ = writeln!(out, "{} {} l", num(p0.x), num(p0.y));
    }
    for s in curve[1..].chunks_exact(3) {
        let _ = writeln!(
            out,
            "{} {} {} {} {} {} c",
            num(s[0].x),
            num(s[0].y),
            num(s[1].x),
            num(s[1].y),
            num(s[2].x),
            num(s[2].y)
        );
    }
}

fn line_ops(a: Pt, b: Pt, out: &mut String) {
    let _ = writeln!(out, "{} {} m {} {} l", num(a.x), num(a.y), num(b.x), num(b.y));
}

/// Cubic Bézier approximation of the ellipse inscribed in the box `a`-`b`.
fn ellipse_ops(a: Pt, b: Pt, out: &mut String) {
    const K: f32 = 0.552_284_8;
    let c = a.lerp(b, 0.5);
    let (rx, ry) = ((b.x - a.x).abs() / 2.0, (b.y - a.y).abs() / 2.0);
    let (kx, ky) = (rx * K, ry * K);
    let curve = [
        Pt::new(c.x + rx, c.y),
        Pt::new(c.x + rx, c.y + ky),
        Pt::new(c.x + kx, c.y + ry),
        Pt::new(c.x, c.y + ry),
        Pt::new(c.x - kx, c.y + ry),
        Pt::new(c.x - rx, c.y + ky),
        Pt::new(c.x - rx, c.y),
        Pt::new(c.x - rx, c.y - ky),
        Pt::new(c.x - kx, c.y - ry),
        Pt::new(c.x, c.y - ry),
        Pt::new(c.x + kx, c.y - ry),
        Pt::new(c.x + rx, c.y - ky),
        Pt::new(c.x + rx, c.y),
    ];
    bezier_ops(&curve, out);
    out.push_str("h\n");
}

/// Adds the annotation (and its appearance stream) to `doc`; returns the annotation's object id.
pub fn add_annotation(doc: &mut Document, a: &Annotation, page: ObjectId) -> ObjectId {
    let rect = bounds(a);
    let [r, g, b] = a.style.color;
    let w = a.style.width;
    let color = format!("{} {} {}", num(r), num(g), num(b));
    let mut ops = String::from("q /GS0 gs\n");
    let mut multiply = false;
    let mut font = false;
    let mut fill_opacity: Option<f32> = None;

    let mut annot = dictionary! {
        "Type" => "Annot",
        "Rect" => reals(rect),
        "NM" => Object::string_literal(a.id.as_bytes().to_vec()),
        "F" => 4, // Print
        "M" => Object::string_literal(pdf_date()),
        "P" => page,
        "CA" => real(a.style.opacity),
    };
    let border = |w: f32| Object::Dictionary(dictionary! { "Type" => "Border", "W" => real(w), "S" => "S" });

    match &a.kind {
        Kind::Ink { curve, highlighter } => {
            multiply = *highlighter;
            let flat = flatten_bezier(curve, 8);
            annot.set("Subtype", "Ink");
            annot.set(
                "InkList",
                Object::Array(vec![reals(flat.iter().flat_map(|p| [p.x, p.y]))]),
            );
            annot.set("C", reals(a.style.color));
            annot.set("BS", border(w));
            let _ = writeln!(ops, "{color} RG {} w 1 J 1 j", num(w));
            bezier_ops(curve, &mut ops);
            ops.push_str("S\n");
        }
        Kind::Shape { shape, a: p, b: q } => {
            annot.set("C", reals(a.style.color));
            annot.set("BS", border(w));
            // A rotated box shape is drawn upright in a rotated frame (`cm` about its center).
            let rotated = a.angle != 0.0 && a.is_box();
            if rotated {
                let c = p.lerp(*q, 0.5);
                let (s, co) = a.angle.sin_cos();
                let (e, f) = (c.x - co * c.x + s * c.y, c.y - s * c.x - co * c.y);
                let _ = writeln!(ops, "{} {} {} {} {} {} cm", num(co), num(s), num(-s), num(co), num(e), num(f));
            }
            // Interior fill, drawn first with its own opacity (graphics state GS1).
            let fill = a.style.fill.filter(|_| matches!(shape, ShapeKind::Rect | ShapeKind::Ellipse));
            if let Some(f) = fill {
                fill_opacity = Some(a.style.fill_opacity);
                annot.set("IC", reals(f));
                let _ = writeln!(ops, "q /GS1 gs {} {} {} rg", num(f[0]), num(f[1]), num(f[2]));
                match shape {
                    ShapeKind::Rect => {
                        let _ = writeln!(
                            ops,
                            "{} {} {} {} re f",
                            num(p.x.min(q.x)),
                            num(p.y.min(q.y)),
                            num((p.x - q.x).abs()),
                            num((p.y - q.y).abs())
                        );
                    }
                    _ => {
                        ellipse_ops(*p, *q, &mut ops);
                        ops.push_str("f\n");
                    }
                }
                ops.push_str("Q\n");
            }
            let _ = writeln!(ops, "{color} RG {} w 1 J", num(w));
            match shape {
                ShapeKind::Rect if rotated => {
                    // A rotated rectangle is a polygon, so viewers that redraw
                    // annotations themselves still get it right.
                    annot.set("Subtype", "Polygon");
                    annot.set("Vertices", reals(box_corners(a).iter().flat_map(|v| [v.x, v.y])));
                    let (x, y) = (p.x.min(q.x), p.y.min(q.y));
                    let _ = writeln!(
                        ops,
                        "0 j {} {} {} {} re S",
                        num(x),
                        num(y),
                        num((p.x - q.x).abs()),
                        num((p.y - q.y).abs())
                    );
                }
                ShapeKind::Rect => {
                    annot.set("Subtype", "Square");
                    let (x, y) = (p.x.min(q.x), p.y.min(q.y));
                    let _ = writeln!(
                        ops,
                        "0 j {} {} {} {} re S",
                        num(x),
                        num(y),
                        num((p.x - q.x).abs()),
                        num((p.y - q.y).abs())
                    );
                }
                ShapeKind::Ellipse => {
                    annot.set("Subtype", "Circle");
                    ellipse_ops(*p, *q, &mut ops);
                    ops.push_str("S\n");
                }
                ShapeKind::Check | ShapeKind::Cross => {
                    // Ink, so viewers that redraw annotations themselves still show the mark.
                    // InkList in page coordinates (rotated); the drawing is in the rotated frame.
                    let strokes = mark_strokes(*shape, *p, *q);
                    annot.set("Subtype", "Ink");
                    annot.set(
                        "InkList",
                        Object::Array(mark_lines(a).iter().map(|l| reals(l.iter().flat_map(|p| [p.x, p.y]))).collect()),
                    );
                    ops.push_str("1 j\n");
                    for line in &strokes {
                        let _ = writeln!(ops, "{} {} m", num(line[0].x), num(line[0].y));
                        for pt in &line[1..] {
                            let _ = writeln!(ops, "{} {} l", num(pt.x), num(pt.y));
                        }
                    }
                    ops.push_str("S\n");
                }
                ShapeKind::Line | ShapeKind::Arrow => {
                    annot.set("Subtype", "Line");
                    annot.set("L", reals([p.x, p.y, q.x, q.y]));
                    ops.push_str("1 j\n");
                    line_ops(*p, *q, &mut ops);
                    if *shape == ShapeKind::Arrow {
                        annot.set("LE", Object::Array(vec!["None".into(), "OpenArrow".into()]));
                        let [l, rr] = arrow_head(*p, *q, w);
                        let _ = writeln!(
                            ops,
                            "{} {} m {} {} l {} {} l",
                            num(l[1].x),
                            num(l[1].y),
                            num(q.x),
                            num(q.y),
                            num(rr[1].x),
                            num(rr[1].y)
                        );
                    }
                    ops.push_str("S\n");
                }
            }
        }
        Kind::Markup { markup, quads } => {
            annot.set(
                "Subtype",
                match markup {
                    MarkupKind::Highlight => "Highlight",
                    MarkupKind::Underline => "Underline",
                    MarkupKind::StrikeOut => "StrikeOut",
                },
            );
            annot.set("C", reals(a.style.color));
            annot.set("QuadPoints", reals(quads.iter().flatten().flat_map(|p| [p.x, p.y])));
            if *markup == MarkupKind::Highlight {
                multiply = true;
                let _ = writeln!(ops, "{color} rg");
                for q in quads {
                    let [ul, ur, ll, lr] = *q;
                    let _ = writeln!(
                        ops,
                        "{} {} m {} {} l {} {} l {} {} l h",
                        num(ul.x),
                        num(ul.y),
                        num(ur.x),
                        num(ur.y),
                        num(lr.x),
                        num(lr.y),
                        num(ll.x),
                        num(ll.y)
                    );
                }
                ops.push_str("f\n");
            } else {
                let width = quads.first().map(markup_thickness).unwrap_or(1.0);
                let _ = writeln!(ops, "{color} RG {} w", num(width));
                for q in quads {
                    let (s, e) = markup_line(q, *markup);
                    line_ops(s, e, &mut ops);
                }
                ops.push_str("S\n");
            }
        }
        Kind::Text { origin, right, down, text, width, .. } => {
            font = true;
            annot.set("Subtype", "FreeText");
            annot.set("Contents", text_string(text));
            annot.set(
                "DA",
                Object::string_literal(format!("/Helv {} Tf {color} rg", num(w))),
            );
            annot.set("BS", border(0.0));
            annot.set("Q", 0);
            // Text matrix: x along screen-right, y along screen-up.
            let base = origin.add(right.scale(TEXT_PAD)).add(down.scale(TEXT_PAD + TEXT_ASCENT * w));
            let _ = writeln!(
                ops,
                "BT /Helv {} Tf {color} rg {} {} {} {} {} {} Tm",
                num(w),
                num(right.x),
                num(right.y),
                num(-down.x),
                num(-down.y),
                num(base.x),
                num(base.y)
            );
            let mut bytes = ops.into_bytes();
            for (i, (line, _)) in text_lines(text, w, *width).into_iter().enumerate() {
                if i > 0 {
                    bytes.extend(format!("0 {} Td ", num(-w * TEXT_LINE_HEIGHT)).bytes());
                }
                bytes.extend(pdf_literal(text[line].trim_end_matches(' ')));
                bytes.extend(b" Tj\n");
            }
            bytes.extend(b"ET\n");
            ops = String::from_utf8(bytes).unwrap_or_default();
        }
    }
    ops.push_str("Q\n");
    if a.takes_note() && !a.note.is_empty() {
        annot.set("Contents", text_string(&a.note));
    }

    let mut gs = dictionary! {
        "Type" => "ExtGState",
        "CA" => real(a.style.opacity),
        "ca" => real(a.style.opacity),
    };
    if multiply {
        gs.set("BM", "Multiply");
    }
    let mut states = dictionary! { "GS0" => gs };
    if let Some(o) = fill_opacity {
        states.set("GS1", dictionary! { "Type" => "ExtGState", "CA" => real(o), "ca" => real(o) });
    }
    let mut resources = dictionary! { "ExtGState" => states };
    if font {
        resources.set(
            "Font",
            dictionary! {
                "Helv" => dictionary! {
                    "Type" => "Font",
                    "Subtype" => "Type1",
                    "BaseFont" => "Helvetica",
                    "Encoding" => "WinAnsiEncoding",
                },
            },
        );
    }
    let drawing = stable_hash(ops.as_bytes());
    let ap = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => reals(rect),
            "Resources" => resources,
        },
        ops.into_bytes(),
    );
    let ap_id = doc.add_object(ap);
    annot.set("AP", dictionary! { "N" => ap_id });

    let private = Private { v: 1, rect, annot: a.clone(), written: Some(Fingerprint::of(&annot, Some(drawing))) };
    let json = serde_json::to_vec(&private).unwrap_or_default();
    annot.set(PRIVATE_KEY, Object::String(json, StringFormat::Hexadecimal));
    doc.add_object(annot)
}

pub fn dict_rect(d: &Dictionary) -> Option<[f32; 4]> {
    let arr = d.get(b"Rect").ok()?.as_array().ok()?;
    if arr.len() != 4 {
        return None;
    }
    let v: Vec<f32> = arr.iter().filter_map(|o| o.as_float().ok()).collect();
    (v.len() == 4).then(|| [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_compact() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(1.25), "1.25");
        assert_eq!(num(-0.0001), "0");
        assert_eq!(num(-3.5), "-3.5");
    }

    #[test]
    fn literal_escapes() {
        assert_eq!(pdf_literal("a(b)\\é"), b"(a\\(b\\)\\\\\\351)".to_vec());
    }

    #[test]
    fn text_string_round_trip() {
        let Object::String(b, _) = text_string("héllo ✓") else { panic!() };
        assert_eq!(decode_text_string(&b), "héllo ✓");
    }
}
