//! Build print-ready PDFs with lopdf.
//!
//! Output pages are built inside a copy of the source document, so nothing
//! has to be copied between documents: 1-up pages are re-parented clones of
//! the originals, n-up sheets draw the originals as form XObjects, and
//! anything no longer reachable is pruned before saving.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lopdf::xref::XrefType;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::print::geometry::{Matrix, SheetLayout, display_rotation, rotated_size};
use crate::print::pipeline::{BLANK, JobOptions, Pass, Side, first_page, flatten};

const LETTER: [f64; 4] = [0.0, 0.0, 612.0, 792.0];
/// Catalog entries that point at the original pages and would keep them alive
/// (form fields lead back to their pages through their widgets' `/P`).
const STALE_CATALOG_KEYS: [&[u8]; 8] =
    [b"Outlines", b"OpenAction", b"Names", b"StructTreeRoot", b"PageLabels", b"Dests", b"AcroForm", b"Threads"];

/// Reads a PDF (decrypting it if it only has an owner password).
pub fn parse(bytes: &[u8]) -> Result<Document, String> {
    let mut doc = Document::load_mem(bytes).map_err(|e| format!("Couldn't read the PDF: {e}"))?;
    if doc.is_encrypted() {
        doc.decrypt("").map_err(|_| "The PDF is password protected".to_string())?;
    }
    Ok(doc)
}

/// A document to print from: annotations are drawn into the pages (see
/// [`flatten_annotations`]).
#[cfg(test)]
pub fn load_bytes(bytes: &[u8]) -> Result<Document, String> {
    let mut doc = parse(bytes)?;
    flatten_annotations(&mut doc);
    Ok(doc)
}

/// Whether form fields need their appearances drawn before they can be
/// printed: the form asks viewers to draw them (`/NeedAppearances`), or a
/// printable field has none. Viewers show such fields anyway, from their values.
pub fn needs_appearances(doc: &Document) -> bool {
    let Ok(catalog) = doc.catalog() else { return false };
    let Some(form) = catalog.get(b"AcroForm").ok().and_then(|f| resolve(doc, f).as_dict().ok()) else {
        return false;
    };
    if form.get(b"NeedAppearances").ok().and_then(|n| resolve(doc, n).as_bool().ok()).unwrap_or(false) {
        return true;
    }
    doc.get_pages().into_values().any(|page| {
        let Some(annots) = doc.get_dictionary(page).ok().and_then(|p| p.get(b"Annots").ok()) else { return false };
        let Ok(annots) = resolve(doc, annots).as_array() else { return false };
        annots.iter().filter_map(|a| resolve(doc, a).as_dict().ok()).any(|a| {
            a.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Widget".as_slice())
                && printable(doc, a)
                && normal_appearance(doc, a).is_none()
        })
    })
}

/// The document as PDF bytes, with a classic cross-reference table (see [`build`]).
pub fn to_bytes(doc: &mut Document) -> Result<Vec<u8>, String> {
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    let mut buf = Vec::new();
    doc.save_to(&mut buf).map_err(|e| format!("Couldn't write the PDF: {e}"))?;
    Ok(buf)
}

struct PageInfo {
    id: ObjectId,
    /// The page dictionary with inherited attributes filled in and no /Parent.
    dict: Dictionary,
    bbox: [f64; 4],
    rotation: u32,
}

impl PageInfo {
    fn display_size(&self) -> (f64, f64) {
        let [llx, lly, urx, ury] = self.bbox;
        rotated_size(urx - llx, ury - lly, self.rotation)
    }
}

fn number(obj: &Object) -> Option<f64> {
    match obj {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    match obj {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(obj),
        _ => obj,
    }
}

fn rect(doc: &Document, obj: &Object) -> Option<[f64; 4]> {
    let arr = resolve(doc, obj).as_array().ok()?;
    let v: Vec<f64> = arr.iter().filter_map(|o| number(resolve(doc, o))).collect();
    let [x0, y0, x1, y1] = v.as_slice() else { return None };
    Some([x0.min(*x1), y0.min(*y1), x0.max(*x1), y0.max(*y1)])
}

fn inherited(doc: &Document, page: &Dictionary, key: &[u8]) -> Option<Object> {
    let mut dict = page;
    for _ in 0..64 {
        if let Ok(v) = dict.get(key) {
            return Some(v.clone());
        }
        let parent = dict.get(b"Parent").ok()?.as_reference().ok()?;
        dict = doc.get_dictionary(parent).ok()?;
    }
    None
}

fn page_info(doc: &Document, id: ObjectId) -> Result<PageInfo, String> {
    let original = doc.get_dictionary(id).map_err(|e| format!("Broken page object: {e}"))?;
    let mut dict = original.clone();
    for key in [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"] {
        if !dict.has(key)
            && let Some(v) = inherited(doc, original, key)
        {
            dict.set(key, v);
        }
    }
    dict.remove(b"Parent");
    let media = dict.get(b"MediaBox").ok().and_then(|o| rect(doc, o)).unwrap_or(LETTER);
    // What shows is the crop box within the media box.
    let crop = dict.get(b"CropBox").ok().and_then(|o| rect(doc, o)).unwrap_or(media);
    let bbox = [crop[0].max(media[0]), crop[1].max(media[1]), crop[2].min(media[2]), crop[3].min(media[3])];
    let bbox = if bbox[0] < bbox[2] && bbox[1] < bbox[3] { bbox } else { media };
    let rotation = dict.get(b"Rotate").ok().and_then(|o| number(resolve(doc, o))).unwrap_or(0.0);
    let rotation = (((rotation as i64 / 90) * 90).rem_euclid(360)) as u32;
    Ok(PageInfo { id, dict, bbox, rotation })
}

fn real(x: f64) -> Object {
    Object::Real(x as f32)
}

fn form_xobject(doc: &mut Document, info: &PageInfo) -> ObjectId {
    let content = doc.get_page_content(info.id);
    let [llx, lly, urx, ury] = info.bbox;
    let mut dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "BBox" => vec![real(llx), real(lly), real(urx), real(ury)],
    };
    if let Ok(res) = info.dict.get(b"Resources") {
        dict.set("Resources", res.clone());
    }
    doc.add_object(Stream::new(dict, content))
}

/// Annotation flags (PDF 32000-1, 12.5.3).
const ANNOT_HIDDEN: i64 = 2;
const ANNOT_PRINT: i64 = 4;

/// Draws each page's printable annotations (their normal appearance) into the
/// page itself and drops `/Annots`. Printed sheets only take a page's own
/// drawing, so this is how highlights, ink and filled-in forms get printed.
/// Annotations without the Print flag (most sticky-note icons) don't print,
/// as in other viewers.
pub fn flatten_annotations(doc: &mut Document) {
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for page in pages {
        flatten_page(doc, page);
    }
    // The form's fields are drawn now; what's left of it would only lead back to the pages.
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"AcroForm");
    }
}

fn flatten_page(doc: &mut Document, page_id: ObjectId) {
    let Ok(page) = doc.get_dictionary(page_id) else { return };
    let Some(annots) = page.get(b"Annots").ok().and_then(|a| resolve(doc, a).as_array().ok()).cloned() else {
        return;
    };
    let mut ops = String::new();
    let mut forms: Vec<(String, ObjectId)> = Vec::new();
    for (i, entry) in annots.iter().enumerate() {
        let Ok(annot) = resolve(doc, entry).as_dict() else { continue };
        if let Some((form, [a, b, c, d, e, f])) = appearance(doc, annot) {
            let name = format!("OchreAnnot{i}");
            ops += &format!("q {a:.6} {b:.6} {c:.6} {d:.6} {e:.4} {f:.4} cm /{name} Do Q\n");
            forms.push((name, form));
        }
    }
    let Ok(page) = doc.get_dictionary(page_id) else { return };
    let mut resources = match inherited(doc, page, b"Resources") {
        Some(r) => resolve(doc, &r).as_dict().cloned().unwrap_or_default(),
        None => Dictionary::new(),
    };
    let mut xobjects = match resources.get(b"XObject") {
        Ok(x) => resolve(doc, x).as_dict().cloned().unwrap_or_default(),
        Err(_) => Dictionary::new(),
    };
    // The page's own drawing, wrapped so whatever state it leaves doesn't move the annotations.
    let mut contents: Vec<Object> = match page.get(b"Contents") {
        Ok(Object::Array(items)) => items.clone(),
        Ok(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Array(items)) => items.clone(),
            _ => vec![Object::Reference(*id)],
        },
        _ => Vec::new(),
    };
    for (_, form) in &forms {
        // Appearance streams are form XObjects, even when they don't say so.
        if let Ok(Object::Stream(s)) = doc.get_object_mut(*form) {
            s.dict.set("Type", "XObject");
            s.dict.set("Subtype", "Form");
        }
    }
    for (name, form) in forms {
        xobjects.set(name, form);
    }
    resources.set("XObject", xobjects);
    if !ops.is_empty() {
        let before = doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
        let after = doc.add_object(Stream::new(Dictionary::new(), format!("Q\n{ops}").into_bytes()));
        contents.insert(0, before.into());
        contents.push(after.into());
    }
    let Ok(page) = doc.get_dictionary_mut(page_id) else { return };
    if !ops.is_empty() {
        page.set("Contents", contents);
        page.set("Resources", resources);
    }
    page.remove(b"Annots");
}

/// Whether an annotation prints: it has the Print flag, isn't hidden, and isn't a pop-up note.
fn printable(doc: &Document, annot: &Dictionary) -> bool {
    let flags = annot.get(b"F").ok().and_then(|f| resolve(doc, f).as_i64().ok()).unwrap_or(0);
    let popup = annot.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Popup".as_slice());
    flags & ANNOT_HIDDEN == 0 && flags & ANNOT_PRINT != 0 && !popup
}

/// The stream an annotation is normally drawn with, if it has a usable one.
fn normal_appearance(doc: &Document, annot: &Dictionary) -> Option<ObjectId> {
    let ap = resolve(doc, annot.get(b"AP").ok()?).as_dict().ok()?;
    let normal = ap.get(b"N").ok()?;
    // Check boxes and radio buttons keep one appearance per state.
    let id = match resolve(doc, normal) {
        Object::Stream(_) => normal.as_reference().ok()?,
        Object::Dictionary(states) => {
            let state = annot.get(b"AS").ok()?.as_name().ok()?;
            states.get(state).ok()?.as_reference().ok()?
        }
        _ => return None,
    };
    doc.get_object(id).ok()?.as_stream().ok()?;
    Some(id)
}

/// The normal appearance of a printable annotation, and where it goes on the
/// page: its bounding box (after its own `/Matrix`) mapped onto `/Rect`.
fn appearance(doc: &Document, annot: &Dictionary) -> Option<(ObjectId, [f64; 6])> {
    if !printable(doc, annot) {
        return None;
    }
    let id = normal_appearance(doc, annot)?;
    let form = doc.get_object(id).ok()?.as_stream().ok()?;
    let [bx0, by0, bx1, by1] = rect(doc, form.dict.get(b"BBox").ok()?)?;
    let m = match form.dict.get(b"Matrix").ok().map(|m| resolve(doc, m)) {
        Some(Object::Array(a)) => {
            let v: Vec<f64> = a.iter().filter_map(|o| number(resolve(doc, o))).collect();
            Matrix(v.try_into().ok()?)
        }
        _ => Matrix::IDENTITY,
    };
    let corners = [(bx0, by0), (bx1, by0), (bx0, by1), (bx1, by1)].map(|(x, y)| m.apply(x, y));
    let (x0, x1) = corners.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.0), hi.max(p.0)));
    let (y0, y1) = corners.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.1), hi.max(p.1)));
    let [rx0, ry0, rx1, ry1] = rect(doc, annot.get(b"Rect").ok()?)?;
    if x1 - x0 < 1e-6 || y1 - y0 < 1e-6 || rx1 - rx0 < 1e-6 || ry1 - ry0 < 1e-6 {
        return None;
    }
    let (sx, sy) = ((rx1 - rx0) / (x1 - x0), (ry1 - ry0) / (y1 - y0));
    Some((id, [sx, 0.0, 0.0, sy, rx0 - x0 * sx, ry0 - y0 * sy]))
}

/// Who the PDF is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Sent straight to the printer driver: every sheet is a portrait page
    /// of the paper size, with orientation and rotation already applied, so
    /// nothing downstream needs to turn or scale anything.
    Printer,
    /// Saved for viewing: landscape sheets are marked as rotated so they
    /// show the right way up in PDF viewers.
    File,
}

/// Build the output document for `sides` (each with its extra clockwise rotation).
pub fn build(src: &Document, sides: &[(Side, u32)], layout: &SheetLayout, target: Target) -> Result<Document, String> {
    let mut doc = src.clone();
    let infos: Vec<PageInfo> = doc.get_pages().values().map(|id| page_info(&doc, *id)).collect::<Result<_, _>>()?;
    if infos.is_empty() {
        return Err("The document has no pages".into());
    }
    let first = first_page(sides.iter().map(|(s, _)| s)).unwrap_or(0);
    let fallback = infos.get(first).ok_or("Page out of range")?.display_size();

    let pages_id = doc.new_object_id();
    let mut kids: Vec<Object> = Vec::with_capacity(sides.len());
    let mut forms: HashMap<usize, ObjectId> = HashMap::new();

    for (side, rot) in sides {
        let first_page = side.iter().find(|&&i| i != BLANK).and_then(|&i| infos.get(i)).map(PageInfo::display_size);
        let sheet = layout.sheet_size(first_page, fallback);
        let passthrough = layout.passthrough() && !side.is_empty();
        // The sheet as it looks on paper, then turned upright onto portrait paper.
        let to_display = display_rotation(sheet.0, sheet.1, *rot);
        let (dw, dh) = rotated_size(sheet.0, sheet.1, *rot);
        let turned = dw > dh;
        let (media, to_paper) = if turned {
            // (x, y) on the landscape sheet -> (dh - y, x): its top edge runs along the paper's left edge.
            ((dh, dw), Matrix([0.0, 1.0, -1.0, 0.0, dh, 0.0]))
        } else {
            ((dw, dh), Matrix::IDENTITY)
        };
        let mut page = if side.is_empty() {
            dictionary! {
                "Type" => "Page",
                "MediaBox" => vec![0.into(), 0.into(), real(media.0), real(media.1)],
                "Resources" => dictionary! { "ProcSet" => vec![Object::Name(b"PDF".to_vec())] },
            }
        } else if passthrough {
            // No paper size known: the original page, untouched.
            let info = infos.get(side[0]).ok_or("Page out of range")?;
            let mut d = info.dict.clone();
            d.set("Rotate", ((info.rotation + rot) % 360) as i64);
            d
        } else {
            let mut ops = String::new();
            let mut xobjects = Dictionary::new();
            for (slot, &idx) in side.iter().enumerate() {
                if idx == BLANK {
                    continue;
                }
                let info = infos.get(idx).ok_or("Page out of range")?;
                let form = match forms.get(&idx) {
                    Some(id) => *id,
                    None => {
                        let id = form_xobject(&mut doc, info);
                        forms.insert(idx, id);
                        id
                    }
                };
                let [llx, lly, urx, ury] = info.bbox;
                let (w, h) = (urx - llx, ury - lly);
                let m = Matrix::translate(-llx, -lly)
                    .then(display_rotation(w, h, info.rotation))
                    .then(layout.place(slot, sheet, rotated_size(w, h, info.rotation)))
                    .then(to_display)
                    .then(to_paper);
                let [a, b, c, d, e, f] = m.0;
                ops += &format!("q {a:.6} {b:.6} {c:.6} {d:.6} {e:.4} {f:.4} cm /P{slot} Do Q\n");
                xobjects.set(format!("P{slot}"), form);
            }
            let content = doc.add_object(Stream::new(Dictionary::new(), ops.into_bytes()));
            dictionary! {
                "Type" => "Page",
                "MediaBox" => vec![0.into(), 0.into(), real(media.0), real(media.1)],
                "Resources" => dictionary! { "XObject" => xobjects },
                "Contents" => content,
            }
        };
        if !passthrough && turned && target == Target::File {
            page.set("Rotate", 90); // undo the turn for on-screen viewing
        }
        page.set("Parent", pages_id);
        kids.push(doc.add_object(page).into());
    }

    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids, "Count" => count,
        }),
    );
    let catalog = doc.catalog_mut().map_err(|e| format!("Broken PDF catalog: {e}"))?;
    catalog.set("Pages", pages_id);
    for key in STALE_CATALOG_KEYS {
        catalog.remove(key);
    }
    doc.prune_objects();
    doc.renumber_objects();
    doc.compress();
    // Always write a classic cross-reference table. A source that used an
    // xref stream would otherwise be saved as one, and lopdf writes wrong
    // offsets there; poppler quietly repairs that, but CUPS's pdftopdf
    // loses the page contents and the printer gets a blank page.
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    // The trailer came from the source; drop what only belonged to its xref stream.
    for key in [b"Type".as_slice(), b"W", b"Index", b"Filter", b"DecodeParms", b"Length", b"Prev", b"XRefStm"] {
        doc.trailer.remove(key);
    }
    Ok(doc)
}

fn save(doc: &mut Document, path: &Path) -> Result<(), String> {
    doc.save(path).map(|_| ()).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

/// Join several PDFs into one, in order (for jobs made of several documents).
pub fn merge(sources: &[Document]) -> Result<Document, String> {
    let mut out = Document::with_version("1.5");
    let pages_id = out.new_object_id();
    let mut kids: Vec<Object> = Vec::new();
    for src in sources {
        let mut doc = src.clone();
        doc.renumber_objects_with(out.max_id + 1);
        let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
        // Pages inherit some attributes from the page tree we're about to
        // drop, so give every page its own copy first.
        let infos: Vec<PageInfo> = pages.iter().map(|id| page_info(&doc, *id)).collect::<Result<_, _>>()?;
        out.max_id = out.max_id.max(doc.max_id);
        out.objects.extend(doc.objects);
        for info in infos {
            let mut dict = info.dict;
            dict.set("Parent", pages_id);
            out.objects.insert(info.id, Object::Dictionary(dict));
            kids.push(info.id.into());
        }
    }
    if kids.is_empty() {
        return Err("The documents have no pages".into());
    }
    let count = kids.len() as i64;
    out.objects
        .insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }));
    let catalog = out.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    out.trailer.set("Root", catalog);
    // The old catalogs and page trees are no longer reachable.
    out.prune_objects();
    out.renumber_objects();
    out.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    Ok(out)
}

/// One PDF per pass, named `<stem>-<n>.pdf` in `dir`.
pub fn write_passes(
    src: &Document,
    passes: &[Pass],
    opts: &JobOptions,
    layout: &SheetLayout,
    dir: &Path,
    stem: &str,
) -> Result<Vec<PathBuf>, String> {
    // Printer-ready unless there's no paper to lay out on (then CUPS does the fitting).
    let target = if layout.passthrough() { Target::File } else { Target::Printer };
    passes
        .iter()
        .enumerate()
        .map(|(i, pass)| {
            let one = std::slice::from_ref(pass);
            let mut doc = build(src, &flatten(one, opts), layout, target)?;
            let path = dir.join(format!("{stem}-{i}.pdf"));
            save(&mut doc, &path)?;
            Ok(path)
        })
        .collect()
}

/// Every pass in one file (for "Save as PDF").
pub fn write_combined(
    src: &Document,
    passes: &[Pass],
    opts: &JobOptions,
    layout: &SheetLayout,
    path: &Path,
) -> Result<(), String> {
    let mut doc = build(src, &flatten(passes, opts), layout, Target::File)?;
    save(&mut doc, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::pipeline::plan;
    use crate::print::testpdf::{A4, numbered};

    fn round_trip(mut doc: Document) -> Document {
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        Document::load_mem(&buf).unwrap()
    }

    fn texts(doc: &Document) -> Vec<String> {
        doc.get_pages().keys().map(|n| doc.extract_text(&[*n]).unwrap_or_default().trim().to_string()).collect()
    }

    fn opts() -> JobOptions {
        JobOptions { reverse: false, ..Default::default() }
    }

    fn out(src: &Document, o: &JobOptions, pass: usize, paper: Option<(f64, f64)>) -> Document {
        let passes = plan(src.get_pages().len(), o).unwrap();
        let layout = SheetLayout::new(o, paper, 0.0);
        round_trip(build(src, &flatten(&passes[pass..=pass], o), &layout, Target::Printer).unwrap())
    }

    #[test]
    fn reverse_and_copies() {
        let src = round_trip(numbered(3, A4));
        assert_eq!(texts(&out(&src, &JobOptions { reverse: true, ..opts() }, 0, None)), ["P3", "P2", "P1"]);
        assert_eq!(texts(&out(&src, &JobOptions { copies: 2, ..opts() }, 0, None)).len(), 6);
    }

    #[test]
    fn duplex_backs_blank_and_rotated() {
        let src = round_trip(numbered(3, A4));
        let o = JobOptions { duplex: true, backs_rotate: true, ..opts() };
        let backs = out(&src, &o, 1, None);
        assert_eq!(texts(&backs), ["P2", ""]);
        // Without a known paper size the original page goes through, turned by /Rotate.
        let first = backs.get_dictionary(*backs.get_pages().get(&1).unwrap()).unwrap();
        assert_eq!(first.get(b"Rotate").unwrap().as_i64().unwrap(), 180);
    }

    #[test]
    fn nup_sheets_use_paper_size() {
        let src = round_trip(numbered(5, A4));
        let o = JobOptions { nup: 4, ..opts() };
        let sheets = out(&src, &o, 0, Some((612.0, 792.0)));
        assert_eq!(sheets.get_pages().len(), 2);
        let first = sheets.get_dictionary(*sheets.get_pages().get(&1).unwrap()).unwrap();
        let media = rect(&sheets, first.get(b"MediaBox").unwrap()).unwrap();
        assert_eq!(media, [0.0, 0.0, 612.0, 792.0]);
        let xobjects = first.get(b"Resources").unwrap().as_dict().unwrap().get(b"XObject").unwrap().as_dict().unwrap();
        assert_eq!(xobjects.len(), 4);
    }

    fn first_page(doc: &Document) -> (Dictionary, [f64; 4]) {
        let page = doc.get_dictionary(*doc.get_pages().get(&1).unwrap()).unwrap().clone();
        let media = rect(doc, page.get(b"MediaBox").unwrap()).unwrap();
        (page, media)
    }

    #[test]
    fn printer_pages_are_always_portrait_paper() {
        let src = round_trip(crate::print::testpdf::numbered(1, (1000.0, 700.0)));
        for o in [
            opts(),
            JobOptions { orientation: crate::print::pipeline::Orientation::Portrait, ..opts() },
            JobOptions { rotate: 90, ..opts() },
            JobOptions { nup: 2, ..opts() },
        ] {
            let (page, media) = first_page(&out(&src, &o, 0, Some(A4)));
            assert_eq!(media, [0.0, 0.0, 595.0, 842.0], "{o:?}");
            assert!(page.get(b"Rotate").is_err(), "nothing left for the driver to rotate: {o:?}");
        }
    }

    #[test]
    fn saved_landscape_sheets_show_upright() {
        let src = round_trip(crate::print::testpdf::numbered(1, (1000.0, 700.0)));
        let o = opts();
        let passes = plan(1, &o).unwrap();
        let doc = build(&src, &flatten(&passes, &o), &SheetLayout::new(&o, Some(A4), 9.0), Target::File).unwrap();
        let (page, media) = first_page(&round_trip(doc));
        assert_eq!(media, [0.0, 0.0, 595.0, 842.0]);
        assert_eq!(page.get(b"Rotate").unwrap().as_i64().unwrap(), 90);
    }

    /// Every entry in the written cross-reference table must point at "N G obj".
    fn assert_xref_valid(bytes: &[u8]) {
        let marker = bytes.windows(9).rposition(|w| w == b"startxref").expect("startxref");
        let tail = String::from_utf8_lossy(&bytes[marker + 9..]);
        let start: usize = tail.split_whitespace().next().unwrap().parse().unwrap();
        assert!(bytes[start..].starts_with(b"xref"), "startxref must point at a classic xref table");
        // The table itself is plain ASCII.
        let table = String::from_utf8_lossy(&bytes[start..marker]);
        let mut lines = table.lines().skip(1);
        let header = lines.next().unwrap();
        let (first, count): (usize, usize) = {
            let mut it = header.split_whitespace().map(|n| n.parse().unwrap());
            (it.next().unwrap(), it.next().unwrap())
        };
        for (i, line) in lines.take(count).enumerate() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts[2] != "n" {
                continue;
            }
            let offset: usize = parts[0].parse().unwrap();
            let id = first + i;
            let expected = format!("{id} {} obj", parts[1].parse::<u32>().unwrap());
            assert!(
                bytes[offset..].starts_with(expected.as_bytes()),
                "object {id}: xref offset {offset} doesn't point at it"
            );
        }
    }

    #[test]
    fn xref_is_valid_even_when_the_source_used_an_xref_stream() {
        let mut src = crate::print::testpdf::numbered(3, (1000.0, 700.0));
        src.reference_table.cross_reference_type = XrefType::CrossReferenceStream;
        let src = round_trip(src);
        for (nup, paper) in [(1, Some(A4)), (1, None), (2, Some(A4))] {
            let o = JobOptions { nup, ..opts() };
            let passes = plan(3, &o).unwrap();
            let mut doc =
                build(&src, &flatten(&passes, &o), &SheetLayout::new(&o, paper, 9.0), Target::Printer).unwrap();
            let mut bytes = Vec::new();
            doc.save_to(&mut bytes).unwrap();
            assert_xref_valid(&bytes);
        }
    }

    #[test]
    fn merged_documents_keep_their_pages_in_order() {
        let a = round_trip(numbered(3, A4));
        // The second document keeps its paper size on the page tree, not the page.
        let mut b = numbered(2, (612.0, 792.0));
        let pages_root = b.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
        for id in b.get_pages().into_values() {
            b.get_dictionary_mut(id).unwrap().remove(b"MediaBox");
        }
        b.get_dictionary_mut(pages_root).unwrap().set("MediaBox", vec![0.into(), 0.into(), 612.into(), 792.into()]);
        let merged = round_trip(merge(&[a, round_trip(b)]).unwrap());
        assert_eq!(texts(&merged), ["P1", "P2", "P3", "P1", "P2"]);
        let fourth = merged.get_dictionary(*merged.get_pages().get(&4).unwrap()).unwrap();
        assert_eq!(rect(&merged, fourth.get(b"MediaBox").unwrap()).unwrap(), [0.0, 0.0, 612.0, 792.0]);
        // And it prints like any other document.
        let o = JobOptions { booklet: true, ..opts() };
        assert_eq!(texts(&out(&merged, &o, 0, Some(A4))).len(), 2);
    }

    #[test]
    fn original_pages_are_pruned() {
        let src = round_trip(numbered(10, A4));
        let o = JobOptions { page_ranges: "2".into(), ..opts() };
        let one = out(&src, &o, 0, None);
        assert_eq!(texts(&one), ["P2"]);
        let page_objects = one.objects.values().filter(|o| {
            o.as_dict()
                .map(|d| d.get(b"Type").and_then(|t| t.as_name()).ok() == Some(b"Page".as_slice()))
                .unwrap_or(false)
        });
        assert_eq!(page_objects.count(), 1);
    }
}
