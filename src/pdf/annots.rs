//! Reading annotations from a PDF and saving ours back as an incremental update.
//!
//! Annotations made by other software are never rewritten: on save, the page's
//! `/Annots` array keeps their original object references, and the original
//! file bytes are kept verbatim with our changes appended after them.

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId};

use super::write::{LEGACY_PRIVATE_KEY, PRIVATE_KEY, Private, add_annotation, dict_rect};
use crate::annot::model::{Annotation, Foreign, LEGACY_NM_PREFIX, NM_PREFIX};

#[derive(Default)]
pub struct Scan {
    /// Our annotations, editable.
    pub managed: Vec<Annotation>,
    /// Everyone else's annotations, preserved as-is.
    pub foreign: Vec<Foreign>,
    /// Per page: `/Annots` indices of our annotations.
    pub managed_idx: Vec<Vec<usize>>,
}

impl Scan {
    /// Per page: `/Annots` indices that must not be drawn by pdfium (ours, which we
    /// draw ourselves, and foreign ones the user deleted).
    pub fn hidden(&self, deleted_foreign: &BTreeSet<usize>) -> Vec<Vec<usize>> {
        let mut hide = self.managed_idx.clone();
        for &i in deleted_foreign {
            let f = &self.foreign[i];
            if let Some(h) = hide.get_mut(f.page) {
                h.push(f.index);
                h.extend(&f.popups);
            }
        }
        for h in &mut hide {
            h.sort_unstable();
            h.dedup();
        }
        hide
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

fn read_managed(d: &Dictionary, page: usize) -> Option<Annotation> {
    let nm = d.get(b"NM").ok()?.as_str().ok()?;
    if !nm.starts_with(NM_PREFIX.as_bytes()) && !nm.starts_with(LEGACY_NM_PREFIX.as_bytes()) {
        return None;
    }
    let raw = d.get(PRIVATE_KEY).or_else(|_| d.get(LEGACY_PRIVATE_KEY)).ok()?.as_str().ok()?;
    let private: Private = serde_json::from_slice(raw).ok()?;
    // If another app moved/resized it, /Rect changed: treat it as theirs now.
    let rect = dict_rect(d)?;
    if rect.iter().zip(private.rect).any(|(a, b)| (a - b).abs() > 0.01) {
        return None;
    }
    let mut annot = private.annot;
    annot.page = page;
    annot.id = String::from_utf8(nm.to_vec()).ok()?;
    Some(annot)
}

pub fn scan(doc: &Document) -> Scan {
    let mut scan = Scan::default();
    for (page, (_, page_id)) in doc.get_pages().into_iter().enumerate() {
        let mut managed_idx = Vec::new();
        let Ok(page_dict) = doc.get_dictionary(page_id) else {
            scan.managed_idx.push(managed_idx);
            continue;
        };
        let entries = annots_array(doc, page_dict);
        let refs: Vec<Option<ObjectId>> = entries.iter().map(|o| o.as_reference().ok()).collect();
        for (index, entry) in entries.iter().enumerate() {
            let Some(d) = resolve(doc, entry) else { continue };
            if let Some(a) = read_managed(d, page) {
                scan.managed.push(a);
                managed_idx.push(index);
                continue;
            }
            let subtype = d
                .get(b"Subtype")
                .and_then(Object::as_name)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            let flags = d.get(b"F").and_then(Object::as_i64).unwrap_or(0);
            let hidden = flags & 2 != 0;
            let popups = d
                .get(b"Popup")
                .and_then(Object::as_reference)
                .ok()
                .and_then(|id| refs.iter().position(|r| *r == Some(id)))
                .into_iter()
                .collect();
            scan.foreign.push(Foreign {
                page,
                index,
                popups,
                selectable: !hidden && !matches!(subtype.as_str(), "Link" | "Widget" | "Popup"),
                subtype,
                rect: dict_rect(d).unwrap_or([0.0; 4]),
            });
        }
        scan.managed_idx.push(managed_idx);
    }
    scan
}

/// Writes `annots` into the PDF `original` and saves it to `out`.
///
/// Only pages whose annotations changed are touched. On those pages the
/// `/Annots` array keeps every foreign entry (by reference) except ones in
/// `deleted_foreign`, drops our old entries and appends our current ones.
pub fn save(
    original: &[u8],
    scan: &Scan,
    annots: &[Annotation],
    dirty_pages: &BTreeSet<usize>,
    deleted_foreign: &BTreeSet<usize>,
    out: &Path,
) -> Result<()> {
    let bytes = if dirty_pages.is_empty() {
        original.to_vec()
    } else {
        let mut inc: IncrementalDocument =
            original.try_into().context("could not parse the PDF for saving")?;
        if inc.get_prev_documents().xref_start == 0 {
            bail!("this PDF is damaged (no valid cross-reference table), so it can't be updated safely");
        }
        let pages = inc.get_prev_documents().get_pages();
        let hidden = scan.hidden(deleted_foreign);
        for &page in dirty_pages {
            let page_id = *pages.values().nth(page).context("page missing")?;
            let drop: HashSet<usize> = hidden.get(page).into_iter().flatten().copied().collect();
            let prev = inc.get_prev_documents();
            let entries = annots_array(prev, prev.get_dictionary(page_id)?);
            let mut new_list: Vec<Object> = entries
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !drop.contains(i))
                .map(|(_, o)| o)
                .collect();
            for a in annots.iter().filter(|a| a.page == page) {
                new_list.push(Object::Reference(add_annotation(&mut inc.new_document, a, page_id)));
            }
            inc.opt_clone_object_to_new_document(page_id)?;
            inc.new_document.get_dictionary_mut(page_id)?.set("Annots", Object::Array(new_list));
        }
        let mut buf = Vec::with_capacity(original.len() + 64 * 1024);
        inc.save_to(&mut buf).context("could not write the PDF")?;
        buf
    };
    write_atomically(out, &bytes)
}

/// Writes to a temp file next to `path`, then renames over it, so a crash never
/// leaves a half-written PDF.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
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
