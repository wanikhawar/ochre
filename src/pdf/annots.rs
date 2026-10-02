//! Reading annotations from a PDF and saving ours back as an incremental update.
//!
//! Annotations made by other software are never rewritten: on save, the page's
//! `/Annots` array keeps their original object references, and the original
//! file bytes are kept verbatim with our changes appended after them.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId};

use super::write::{Fingerprint, LEGACY_PRIVATE_KEY, appearance_hash, PRIVATE_KEY, Private, add_annotation, decode_text_string, dict_rect};
use crate::annot::model::{AnnotIdentity, Annotation, Foreign, Kind, LEGACY_NM_PREFIX, NM_PREFIX, PageSlot};

#[derive(Default)]
pub struct Scan {
    /// Our annotations, editable.
    pub managed: Vec<Annotation>,
    /// Everyone else's annotations, preserved as-is.
    pub foreign: Vec<Foreign>,
    /// Per page: `/Annots` indices of our annotations.
    pub managed_idx: Vec<Vec<usize>>,
    /// Every annotation object in the file by object number, listed on a page or
    /// not, to check what a remembered object number refers to now.
    pub objects: HashMap<ObjectId, AnnotIdentity>,
    /// The file's pages, in order.
    pub pages: Vec<PageSlot>,
    /// `/Annots` indices of our annotations on pages that are in the file but not
    /// in its page tree (deleted earlier; undo can bring them back).
    pub orphans: HashMap<ObjectId, Vec<usize>>,
}

/// Page attributes a page can inherit from its ancestors in the page tree.
const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// `key` of a page, or of the nearest ancestor that has it.
fn inherited(doc: &Document, page: ObjectId, key: &[u8]) -> Option<Object> {
    let mut d = doc.get_dictionary(page).ok()?;
    for _ in 0..64 {
        if let Ok(v) = d.get(key) {
            return Some(v.clone());
        }
        d = doc.get_dictionary(d.get(b"Parent").and_then(Object::as_reference).ok()?).ok()?;
    }
    None
}

fn page_rotation(doc: &Document, page: ObjectId) -> u16 {
    let r = inherited(doc, page, b"Rotate").and_then(|o| o.as_i64().ok()).unwrap_or(0);
    (r.rem_euclid(360) / 90 * 90) as u16
}

fn identity(d: &Dictionary) -> Option<AnnotIdentity> {
    let subtype = String::from_utf8_lossy(d.get(b"Subtype").and_then(Object::as_name).ok()?).into_owned();
    Some(AnnotIdentity {
        subtype,
        nm: d.get(b"NM").and_then(Object::as_str).ok().map(|n| String::from_utf8_lossy(n).into_owned()),
        rect: dict_rect(d)?,
    })
}

impl Scan {
    /// Per page of `layout`: `/Annots` indices that must not be drawn by pdfium
    /// (ours, which we draw ourselves, and other apps' ones the user deleted).
    pub fn hidden(&self, foreign: &[Foreign], deleted_foreign: &BTreeSet<usize>, layout: &[PageSlot]) -> Vec<Vec<usize>> {
        layout
            .iter()
            .map(|slot| {
                let file_index = self.pages.iter().position(|p| p.obj == slot.obj);
                let mut h = match file_index {
                    Some(i) => self.managed_idx.get(i).cloned().unwrap_or_default(),
                    None => self.orphans.get(&slot.obj).cloned().unwrap_or_default(),
                };
                for f in deleted_foreign.iter().map(|&i| &foreign[i]) {
                    // Detached ones aren't in the file's /Annots anyway.
                    if f.attached && f.page_obj == slot.obj {
                        h.push(f.index);
                        h.extend(&f.popups);
                    }
                }
                h.sort_unstable();
                h.dedup();
                h
            })
            .collect()
    }
}

fn annots_array(doc: &Document, page: &Dictionary) -> Vec<Object> {
    let Ok(obj) = page.get(b"Annots") else { return Vec::new() };
    match doc.dereference(obj) {
        Ok((_, Object::Array(a))) => a.clone(),
        _ => Vec::new(),
    }
}

fn resolve<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Dictionary> {
    match doc.dereference(o) {
        Ok((_, Object::Dictionary(d))) => Some(d),
        _ => None,
    }
}

fn read_managed(doc: &Document, d: &Dictionary, page: usize) -> Option<Annotation> {
    let nm = d.get(b"NM").ok()?.as_str().ok()?;
    if !nm.starts_with(NM_PREFIX.as_bytes()) && !nm.starts_with(LEGACY_NM_PREFIX.as_bytes()) {
        return None;
    }
    let raw = d.get(PRIVATE_KEY).or_else(|_| d.get(LEGACY_PRIVATE_KEY)).ok()?.as_str().ok()?;
    let private: Private = serde_json::from_slice(raw).ok()?;
    // If another app edited it (moved it, changed its comment or color, redrew it),
    // it's theirs now: we'd otherwise rewrite it from our copy and undo their edit.
    let rect = dict_rect(d)?;
    if rect.iter().zip(private.rect).any(|(a, b)| (a - b).abs() > 0.01) {
        return None;
    }
    let unedited = match &private.written {
        Some(w) => w.same(&Fingerprint::of(d, appearance_hash(doc, d))),
        // Saved before fingerprints (Ochre 1.2.0 and older): compare the comment and
        // color with what we wrote for this annotation.
        None => unedited_legacy(&private.annot, d),
    };
    if !unedited {
        return None;
    }
    let mut annot = private.annot;
    annot.page = page;
    annot.id = String::from_utf8(nm.to_vec()).ok()?;
    Some(annot)
}

/// Whether `d`'s comment and color are still what Ochre writes for `a`.
fn unedited_legacy(a: &Annotation, d: &Dictionary) -> bool {
    let found = Fingerprint::of(d, None);
    let (contents, color) = match &a.kind {
        // A text box's /Contents is its text, and it has no /C.
        Kind::Text { text, .. } => (Some(text.clone()), None),
        _ => ((!a.note.is_empty()).then(|| a.note.clone()), Some(a.style.color.to_vec())),
    };
    found.same(&Fingerprint { contents, color, modified: found.modified.clone(), appearance: None })
}

/// What's on one page: our annotations (with their `/Annots` indices) and other
/// apps' ones.
fn scan_page(doc: &Document, page: usize, page_id: ObjectId) -> (Vec<(usize, Annotation)>, Vec<Foreign>) {
    let (mut ours, mut theirs) = (Vec::new(), Vec::new());
    let Ok(page_dict) = doc.get_dictionary(page_id) else { return (ours, theirs) };
    let entries = annots_array(doc, page_dict);
    let refs: Vec<Option<ObjectId>> = entries.iter().map(|o| o.as_reference().ok()).collect();
    for (index, entry) in entries.iter().enumerate() {
        let Some(d) = resolve(doc, entry) else { continue };
        if let Some(a) = read_managed(doc, d, page) {
            ours.push((index, a));
            continue;
        }
        let subtype = d
            .get(b"Subtype")
            .and_then(Object::as_name)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        let flags = d.get(b"F").and_then(Object::as_i64).unwrap_or(0);
        let hidden = flags & 2 != 0;
        let popup_obj = d.get(b"Popup").and_then(Object::as_reference).ok();
        let popups = popup_obj.and_then(|id| refs.iter().position(|r| *r == Some(id))).into_iter().collect();
        // A FreeText's /Contents is its visible text, not a comment.
        let contents = d
            .get(b"Contents")
            .and_then(Object::as_str)
            .ok()
            .map(|b| decode_text_string(b).replace("\r\n", "\n").replace('\r', "\n").trim().to_string())
            .filter(|n| !n.is_empty());
        let (note, text) = if subtype == "FreeText" { (None, contents) } else { (contents, None) };
        theirs.push(Foreign {
            index,
            popups,
            selectable: !hidden && !matches!(subtype.as_str(), "Link" | "Widget" | "Popup"),
            subtype,
            rect: dict_rect(d).unwrap_or([0.0; 4]),
            note,
            obj: refs[index],
            popup_objs: popup_obj.into_iter().collect(),
            attached: true,
            nm: d.get(b"NM").and_then(Object::as_str).ok().map(|n| String::from_utf8_lossy(n).into_owned()),
            text,
            page_obj: page_id,
        });
    }
    (ours, theirs)
}

pub fn scan(doc: &Document) -> Scan {
    let objects = doc.objects.iter().filter_map(|(id, o)| Some((*id, identity(o.as_dict().ok()?)?))).collect();
    let mut scan = Scan { objects, ..Scan::default() };
    for (page, (_, page_id)) in doc.get_pages().into_iter().enumerate() {
        scan.pages.push(PageSlot { obj: page_id, rotation: page_rotation(doc, page_id) });
        let (ours, theirs) = scan_page(doc, page, page_id);
        scan.managed_idx.push(ours.iter().map(|(i, _)| *i).collect());
        scan.managed.extend(ours.into_iter().map(|(_, a)| a));
        scan.foreign.extend(theirs);
    }
    // Pages not in the tree any more (deleted by an earlier save), in case undo
    // puts one back: where our annotations sit on them (ours come back from the
    // undo history), and other apps' annotations (known, but not shown, until
    // their page is back).
    let listed: HashSet<ObjectId> = scan.pages.iter().map(|p| p.obj).collect();
    let unlisted: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(id, o)| {
            !listed.contains(id) && o.as_dict().is_ok_and(|d| d.get(b"Type").and_then(Object::as_name).ok() == Some(b"Page"))
        })
        .map(|(id, _)| *id)
        .collect();
    for id in unlisted {
        let (ours, theirs) = scan_page(doc, 0, id);
        if !ours.is_empty() {
            scan.orphans.insert(id, ours.into_iter().map(|(i, _)| i).collect());
        }
        scan.foreign.extend(theirs);
    }
    scan
}

/// Object references of detached other-app annotations (and their popups) on the
/// page object `page` that are no longer deleted, i.e. restored by undo.
fn restored_refs(foreign: &[Foreign], deleted_foreign: &BTreeSet<usize>, page: ObjectId) -> Vec<Object> {
    foreign
        .iter()
        .enumerate()
        .filter(|(i, f)| f.page_obj == page && !f.attached && !deleted_foreign.contains(i))
        .flat_map(|(_, f)| f.obj.iter().chain(&f.popup_objs).map(|&id| Object::Reference(id)).collect::<Vec<_>>())
        .collect()
}

/// Everything that differs from the loaded file: the page layout, our
/// annotations and other apps' deleted or restored ones.
pub struct Edits<'a> {
    pub scan: &'a Scan,
    pub foreign: &'a [Foreign],
    pub deleted_foreign: &'a BTreeSet<usize>,
    pub annots: &'a [Annotation],
    /// The pages as shown, in order.
    pub layout: &'a [PageSlot],
    /// Pages (indices into `layout`) whose annotations changed.
    pub dirty: &'a BTreeSet<usize>,
}

/// The PDF `original` with `edits` appended as an incremental update; the original
/// bytes are kept as they are.
///
/// Saving (`preview` false): on each changed page the `/Annots` array keeps every
/// other app's entry (by reference) except deleted ones, gets restored ones back,
/// drops our old entries and appends our current ones. If the layout changed, the
/// page tree is rebuilt in that order with those rotations (pages left out stay in
/// the file, unlisted, so undo can bring them back).
///
/// Preview (`preview` true): what pdfium shows before saving. Same page tree, and
/// restored annotations are added after the existing entries; nothing is dropped,
/// so `/Annots` indices (which pdfium is told to hide) don't change.
pub fn build(original: &[u8], edits: &Edits, preview: bool) -> Result<Vec<u8>> {
    let layout_changed = edits.layout != edits.scan.pages.as_slice();
    let restored: BTreeSet<usize> = (0..edits.layout.len())
        .filter(|&d| !restored_refs(edits.foreign, edits.deleted_foreign, edits.layout[d].obj).is_empty())
        .collect();
    let touched: BTreeSet<usize> = if preview { restored } else { edits.dirty.union(&restored).copied().collect() };
    if !layout_changed && touched.is_empty() {
        return Ok(original.to_vec());
    }
    let mut inc: IncrementalDocument = original.try_into().context("could not parse the PDF for saving")?;
    if inc.get_prev_documents().xref_start == 0 {
        bail!("this PDF is damaged (no valid cross-reference table), so it can't be updated safely");
    }
    let hidden = edits.scan.hidden(edits.foreign, edits.deleted_foreign, edits.layout);
    for &page in &touched {
        let page_id = edits.layout.get(page).context("page missing")?.obj;
        let drop: HashSet<usize> = if preview { HashSet::new() } else { hidden[page].iter().copied().collect() };
        let prev = inc.get_prev_documents();
        let entries = annots_array(prev, prev.get_dictionary(page_id)?);
        let mut new_list: Vec<Object> =
            entries.into_iter().enumerate().filter(|(i, _)| !drop.contains(i)).map(|(_, o)| o).collect();
        new_list.extend(restored_refs(edits.foreign, edits.deleted_foreign, page_id));
        if !preview {
            for a in edits.annots.iter().filter(|a| a.page == page) {
                new_list.push(Object::Reference(add_annotation(&mut inc.new_document, a, page_id)));
            }
        }
        inc.opt_clone_object_to_new_document(page_id)?;
        inc.new_document.get_dictionary_mut(page_id)?.set("Annots", Object::Array(new_list));
    }
    if layout_changed {
        // One flat list of pages under the root of the page tree. Attributes a
        // page inherited from an intermediate node are copied onto it first.
        let prev = inc.get_prev_documents();
        let root = prev.catalog()?.get(b"Pages").and_then(Object::as_reference).context("no page tree")?;
        type Inherited<'k> = Vec<(&'k [u8], Object)>;
        let mut copied: Vec<(ObjectId, Inherited)> = Vec::new();
        for slot in edits.layout {
            let page = prev.get_dictionary(slot.obj)?;
            let attrs = INHERITABLE[..3]
                .iter()
                .filter(|k| !page.has(k))
                .filter_map(|k| Some((*k, inherited(prev, slot.obj, k)?)))
                .collect();
            copied.push((slot.obj, attrs));
        }
        for (slot, (id, attrs)) in edits.layout.iter().zip(copied) {
            inc.opt_clone_object_to_new_document(id)?;
            let d = inc.new_document.get_dictionary_mut(id)?;
            for (k, v) in attrs {
                d.set(k, v);
            }
            d.set("Parent", Object::Reference(root));
            d.set("Rotate", i64::from(slot.rotation));
        }
        inc.opt_clone_object_to_new_document(root)?;
        let tree = inc.new_document.get_dictionary_mut(root)?;
        tree.set("Kids", Object::Array(edits.layout.iter().map(|s| Object::Reference(s.obj)).collect()));
        tree.set("Count", edits.layout.len() as i64);
    }
    let mut buf = Vec::with_capacity(original.len() + 64 * 1024);
    inc.save_to(&mut buf).context("could not write the PDF")?;
    Ok(buf)
}

/// Saves `original` with `edits` to `out`.
pub fn save(original: &[u8], edits: &Edits, out: &Path) -> Result<()> {
    write_atomically(out, &build(original, edits, false)?)
}

/// Writes to a temp file next to `path`, then renames over it, so a crash never
/// leaves a half-written PDF.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().context("invalid file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.ochre-tmp"));
    let res = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.with_context(|| format!("could not save {}", path.display()))
}
