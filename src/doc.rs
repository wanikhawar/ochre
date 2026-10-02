//! An open document: original bytes, our editable annotations, foreign
//! annotation bookkeeping and undo/redo.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::annot::geometry::PageGeom;
use crate::annot::model::{Annotation, Foreign, PageSlot};
use crate::pdf::annots::{Edits, Scan, build, save, scan};
use crate::pdf::worker::{Link, OutlineItem};

#[derive(Clone, Debug)]
pub enum Cmd {
    Add(Annotation),
    Remove { annot: Annotation, index: usize },
    Modify { before: Annotation, after: Annotation },
    /// Hide an annotation made by other software (removed from the file on save).
    DeleteForeign(usize),
    /// Rotate, delete or reorder pages: the page layout goes from `before` to
    /// `after`. `removed` are our annotations on deleted pages (with their indices
    /// in the list), put back on undo. Annotations' page numbers are renumbered.
    Pages { before: Vec<PageSlot>, after: Vec<PageSlot>, removed: Vec<(usize, Annotation)> },
}

/// Three-way merge of one annotation changed here (`base` -> `mine`) and elsewhere
/// (`base` -> `theirs`): each part (color, width, opacity, fill, shape, note,
/// rotation, page) takes whichever side changed it. None if both sides changed the
/// same part differently.
fn merge3(base: &Annotation, mine: &Annotation, theirs: &Annotation) -> Option<Annotation> {
    fn pick<T: PartialEq + Clone>(base: &T, mine: &T, theirs: &T) -> Option<T> {
        if mine == base || mine == theirs {
            Some(theirs.clone())
        } else if theirs == base {
            Some(mine.clone())
        } else {
            None
        }
    }
    let (b, m, t) = (&base.style, &mine.style, &theirs.style);
    Some(Annotation {
        id: mine.id.clone(),
        page: pick(&base.page, &mine.page, &theirs.page)?,
        style: crate::annot::model::Style {
            color: pick(&b.color, &m.color, &t.color)?,
            width: pick(&b.width, &m.width, &t.width)?,
            opacity: pick(&b.opacity, &m.opacity, &t.opacity)?,
            fill: pick(&b.fill, &m.fill, &t.fill)?,
            fill_opacity: pick(&b.fill_opacity, &m.fill_opacity, &t.fill_opacity)?,
        },
        kind: pick(&base.kind, &mine.kind, &theirs.kind)?,
        note: pick(&base.note, &mine.note, &theirs.note)?,
        angle: pick(&base.angle, &mine.angle, &theirs.angle)?,
    })
}

/// `cur` with the parts that differ between `from` and `to` set to `to`'s, and
/// the rest left as they are. When `cur` is `from` this is just `to`.
fn patch(cur: &Annotation, from: &Annotation, to: &Annotation) -> Annotation {
    fn part<T: PartialEq + Clone>(cur: &T, from: &T, to: &T) -> T {
        if from == to { cur.clone() } else { to.clone() }
    }
    let (c, f, t) = (&cur.style, &from.style, &to.style);
    Annotation {
        id: to.id.clone(),
        page: part(&cur.page, &from.page, &to.page),
        style: crate::annot::model::Style {
            color: part(&c.color, &f.color, &t.color),
            width: part(&c.width, &f.width, &t.width),
            opacity: part(&c.opacity, &f.opacity, &t.opacity),
            fill: part(&c.fill, &f.fill, &t.fill),
            fill_opacity: part(&c.fill_opacity, &f.fill_opacity, &t.fill_opacity),
        },
        kind: part(&cur.kind, &from.kind, &to.kind),
        note: part(&cur.note, &from.note, &to.note),
        angle: part(&cur.angle, &from.angle, &to.angle),
    }
}

/// What a save did besides writing the file.
#[derive(Debug, Default, PartialEq)]
pub struct SaveOutcome {
    /// The file had been changed on disk meanwhile, and this window's changes were
    /// combined with that version.
    pub combined: bool,
    /// Annotations edited both here and elsewhere: both versions were kept, this
    /// window's under a new name.
    pub conflicts: usize,
    /// Pages were rearranged elsewhere while this window's undo history had page
    /// edits of its own, which can't be carried over, so the history was cleared.
    pub history_cleared: bool,
}

pub struct Doc {
    pub path: PathBuf,
    pub bytes: Arc<Vec<u8>>,
    /// Bumped whenever pdfium must reload (open, save, foreign delete/undo).
    pub generation: u64,
    pub pages: Vec<PageGeom>,
    pub annots: Vec<Annotation>,
    pub foreign: Vec<Foreign>,
    pub deleted_foreign: BTreeSet<usize>,
    /// Why annotations can't be saved, if they can't (e.g. unparseable file).
    pub read_only: Option<String>,
    /// Per page revision counter, bumped when its annotations change.
    pub page_rev: Vec<u64>,
    /// The pages as shown: which page objects, in what order, turned how far. The
    /// file's own layout is `scan.pages`; they differ after page edits until saved.
    pub layout: Vec<PageSlot>,
    /// Bumped whenever the layout changes, so caches keyed by page number reset.
    pub layout_rev: u64,
    /// Page boxes by page object, for pages edits bring back before pdfium reloads.
    boxes: HashMap<(u32, u16), [f32; 4]>,
    /// What pdfium loads when other apps' annotations were restored by undo after a
    /// save (see [`Doc::render_bytes`]), for the generation it was made for.
    preview: Option<(u64, Arc<Vec<u8>>)>,
    /// Table of contents, from pdfium.
    pub outline: Vec<OutlineItem>,
    /// Link areas per page, from pdfium.
    pub links: Vec<Vec<Link>>,
    scan: Scan,
    /// Annotations as last loaded/saved, to detect changes.
    saved: Vec<Annotation>,
    undo: Vec<Vec<Cmd>>,
    redo: Vec<Vec<Cmd>>,
}

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Doc {
    pub fn open(path: &Path) -> Result<Doc> {
        Ok(Doc::from_bytes(path, std::fs::read(path)?))
    }

    fn from_bytes(path: &Path, bytes: Vec<u8>) -> Doc {
        let (scan, read_only) = match lopdf::Document::load_mem(&bytes) {
            Ok(d) if d.is_encrypted() => {
                (Scan::default(), Some("Encrypted PDFs can be viewed but not annotated yet.".into()))
            }
            Ok(d) => (scan(&d), None),
            Err(e) => (Scan::default(), Some(format!("Annotations can't be saved in this file: {e}"))),
        };
        Doc {
            path: path.to_path_buf(),
            bytes: Arc::new(bytes),
            generation: next_generation(),
            pages: Vec::new(),
            annots: scan.managed.clone(),
            foreign: scan.foreign.clone(),
            saved: scan.managed.clone(),
            read_only,
            deleted_foreign: BTreeSet::new(),
            page_rev: Vec::new(),
            layout: scan.pages.clone(),
            layout_rev: 0,
            boxes: HashMap::new(),
            preview: None,
            outline: Vec::new(),
            links: Vec::new(),
            scan,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// `/Annots` indices pdfium must not draw, per page.
    pub fn hidden(&self) -> Vec<Vec<usize>> {
        self.scan.hidden(&self.foreign, &self.deleted_foreign, &self.layout)
    }

    /// The page (as shown) other-app annotation `i` is on; None if its page was deleted.
    pub fn foreign_page(&self, i: usize) -> Option<usize> {
        let obj = self.foreign.get(i)?.page_obj;
        self.layout.iter().position(|s| s.obj == obj)
    }

    /// Where the file's page `file_index` is shown; None if it was deleted.
    fn shown_page(&self, file_index: usize) -> Option<usize> {
        let obj = self.scan.pages.get(file_index)?.obj;
        self.layout.iter().position(|s| s.obj == obj)
    }

    /// Whether pages were rotated, deleted or reordered since loading.
    pub fn pages_edited(&self) -> bool {
        self.layout != self.scan.pages
    }

    fn edits<'a>(&'a self, dirty: &'a BTreeSet<usize>) -> Edits<'a> {
        Edits {
            scan: &self.scan,
            foreign: &self.foreign,
            deleted_foreign: &self.deleted_foreign,
            annots: &self.annots,
            layout: &self.layout,
            dirty,
        }
    }

    /// The document as it is now (unsaved changes included), as PDF bytes.
    pub fn current_bytes(&self) -> Result<Vec<u8>> {
        build(&self.bytes, &self.edits(&self.dirty_pages()), false)
    }

    /// Runs a page layout change as one undo step; annotations on pages left out
    /// are removed with it (and come back on undo).
    fn exec_layout(&mut self, after: Vec<PageSlot>) {
        if after == self.layout {
            return;
        }
        let kept: Vec<_> = after.iter().map(|s| s.obj).collect();
        let removed = self
            .annots
            .iter()
            .enumerate()
            .filter(|(_, a)| self.layout.get(a.page).is_some_and(|s| !kept.contains(&s.obj)))
            .map(|(i, a)| (i, a.clone()))
            .collect();
        self.exec(vec![Cmd::Pages { before: self.layout.clone(), after, removed }]);
    }

    /// Turns pages (as shown) a quarter turn clockwise, or counter-clockwise.
    pub fn rotate_pages(&mut self, pages: &BTreeSet<usize>, clockwise: bool) {
        let mut after = self.layout.clone();
        for &p in pages {
            if let Some(s) = after.get_mut(p) {
                s.rotation = (s.rotation + if clockwise { 90 } else { 270 }) % 360;
            }
        }
        self.exec_layout(after);
    }

    /// Deletes pages (as shown), keeping at least one; returns false if that would
    /// have deleted every page (nothing is done then).
    pub fn delete_pages(&mut self, pages: &BTreeSet<usize>) -> bool {
        let after: Vec<PageSlot> =
            self.layout.iter().enumerate().filter(|(i, _)| !pages.contains(i)).map(|(_, s)| *s).collect();
        if after.is_empty() {
            return false;
        }
        self.exec_layout(after);
        true
    }

    /// Moves pages (as shown), keeping their order, to just before page `to`
    /// (`to` = page count: the end).
    pub fn move_pages(&mut self, pages: &BTreeSet<usize>, to: usize) {
        let moving: Vec<PageSlot> = pages.iter().filter_map(|&p| self.layout.get(p).copied()).collect();
        let mut after: Vec<PageSlot> =
            self.layout.iter().enumerate().filter(|(i, _)| !pages.contains(i)).map(|(_, s)| *s).collect();
        let at = (0..to.min(self.layout.len())).filter(|i| !pages.contains(i)).count();
        after.splice(at..at, moving);
        self.exec_layout(after);
    }

    /// Writes pages (as shown, unsaved changes included) to a new PDF at `out`.
    #[cfg(test)]
    pub fn extract_pages(&self, pages: &BTreeSet<usize>, out: &Path) -> Result<()> {
        crate::pdf::annots::write_atomically(out, &self.extract_bytes(pages)?)
    }

    /// Pages (as shown, unsaved changes included) as a new PDF.
    pub fn extract_bytes(&self, pages: &BTreeSet<usize>) -> Result<Vec<u8>> {
        let mut d = lopdf::Document::load_mem(&self.current_bytes()?)?;
        let drop: Vec<u32> = (0..self.layout.len()).filter(|p| !pages.contains(p)).map(|p| p as u32 + 1).collect();
        d.delete_pages(&drop);
        // The table of contents would point at pages that aren't there.
        if let Ok(catalog) = d.catalog_mut() {
            catalog.remove(b"Outlines");
        }
        d.prune_objects();
        let mut buf = Vec::new();
        d.save_to(&mut buf)?;
        Ok(buf)
    }

    /// Whether other-app annotation `i` is shown: in the file and not deleted, or
    /// detached by a save and restored by undo.
    fn foreign_shown(&self, i: usize) -> bool {
        !self.deleted_foreign.contains(&i)
    }

    /// The bytes pdfium should show: the file, plus (in memory only) the page
    /// layout and any other-app annotations that undo restored after a save
    /// removed them.
    pub fn render_bytes(&mut self) -> Arc<Vec<u8>> {
        let restored = self.foreign.iter().enumerate().any(|(i, f)| !f.attached && self.foreign_shown(i));
        if !restored && !self.pages_edited() {
            return Arc::clone(&self.bytes);
        }
        if let Some((g, b)) = &self.preview
            && *g == self.generation
        {
            return Arc::clone(b);
        }
        let none = BTreeSet::new();
        let bytes = build(&self.bytes, &self.edits(&none), true).map(Arc::new).unwrap_or_else(|_| Arc::clone(&self.bytes));
        self.preview = Some((self.generation, Arc::clone(&bytes)));
        bytes
    }

    pub fn set_pages(&mut self, pages: Vec<PageGeom>) {
        self.page_rev = vec![0; pages.len()];
        if pages.len() == self.layout.len() {
            for (slot, g) in self.layout.iter().zip(&pages) {
                self.boxes.insert(slot.obj, g.bbox);
            }
        }
        self.pages = pages;
    }

    fn on_page(list: &[Annotation], page: usize) -> Vec<&Annotation> {
        list.iter().filter(|a| a.page == page).collect()
    }

    /// Pages (as shown) whose annotations differ from the file.
    pub fn dirty_pages(&self) -> BTreeSet<usize> {
        // Pages where an other-app annotation's shown state differs from the file.
        let mut pages: BTreeSet<usize> = (0..self.foreign.len())
            .filter(|&i| self.foreign[i].attached != self.foreign_shown(i))
            .filter_map(|i| self.foreign_page(i))
            .collect();
        // The file's annotations, numbered as shown (those on deleted pages drop out).
        let saved: Vec<Annotation> = self
            .saved
            .iter()
            .filter_map(|a| Some(Annotation { page: self.shown_page(a.page)?, ..a.clone() }))
            .collect();
        let touched: BTreeSet<usize> = self.annots.iter().chain(&saved).map(|a| a.page).collect();
        for p in touched {
            if Self::on_page(&self.annots, p) != Self::on_page(&saved, p) {
                pages.insert(p);
            }
        }
        pages
    }

    pub fn is_dirty(&self) -> bool {
        self.pages_edited() || !self.dirty_pages().is_empty()
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.annots.iter().position(|a| a.id == id)
    }

    pub fn get(&self, id: &str) -> Option<&Annotation> {
        self.annots.iter().find(|a| a.id == id)
    }

    fn touch(&mut self, page: usize) {
        if let Some(r) = self.page_rev.get_mut(page) {
            *r += 1;
        }
    }

    /// Applies a command without recording it; returns true if pdfium needs to reload.
    pub fn apply(&mut self, cmd: &Cmd, forward: bool) -> bool {
        match (cmd, forward) {
            (Cmd::Add(a), true) | (Cmd::Remove { annot: a, .. }, false) => {
                if self.index_of(&a.id).is_some() {
                    return false; // already there (e.g. a deletion a merge didn't carry out)
                }
                let index = match cmd {
                    Cmd::Remove { index, .. } => (*index).min(self.annots.len()),
                    _ => self.annots.len(),
                };
                self.annots.insert(index, a.clone());
                self.touch(a.page);
            }
            (Cmd::Add(a), false) | (Cmd::Remove { annot: a, .. }, true) => {
                if let Some(i) = self.index_of(&a.id) {
                    self.annots.remove(i);
                }
                self.touch(a.page);
            }
            (Cmd::Modify { before, after }, fwd) => {
                // Only the parts this step changed, so undoing it after a merge keeps
                // what came from elsewhere.
                let (from, to) = if fwd { (before, after) } else { (after, before) };
                if let Some(i) = self.index_of(&from.id) {
                    self.annots[i] = patch(&self.annots[i], from, to);
                }
                self.touch(from.page);
                self.touch(to.page);
            }
            (Cmd::DeleteForeign(i), fwd) => {
                if fwd {
                    self.deleted_foreign.insert(*i);
                } else {
                    self.deleted_foreign.remove(i);
                }
                self.generation = next_generation();
                return true;
            }
            (Cmd::Pages { before, after, removed }, fwd) => {
                let (from, to) = if fwd { (before, after) } else { (after, before) };
                if fwd {
                    for (_, a) in removed {
                        if let Some(i) = self.index_of(&a.id) {
                            self.annots.remove(i);
                        }
                    }
                }
                let renumber = |p: usize| from.get(p).and_then(|f| to.iter().position(|t| t.obj == f.obj));
                for a in &mut self.annots {
                    if let Some(p) = renumber(a.page) {
                        a.page = p;
                    }
                }
                if !fwd {
                    for (i, a) in removed {
                        if self.index_of(&a.id).is_none() {
                            let i = (*i).min(self.annots.len());
                            self.annots.insert(i, a.clone());
                        }
                    }
                }
                // Page geometry and links follow right away (pdfium reloads after).
                if self.pages.len() == from.len() {
                    let fallback = PageGeom { bbox: [0.0, 0.0, 612.0, 792.0], rotation: 0 };
                    let pages: Vec<PageGeom> = to
                        .iter()
                        .map(|t| {
                            let bbox = self.boxes.get(&t.obj).copied().unwrap_or(fallback.bbox);
                            PageGeom { bbox, rotation: t.rotation }
                        })
                        .collect();
                    let links = to
                        .iter()
                        .map(|t| from.iter().position(|f| f.obj == t.obj).and_then(|i| self.links.get(i).cloned()).unwrap_or_default())
                        .collect();
                    self.set_pages(pages);
                    self.links = links;
                }
                self.layout = to.clone();
                self.layout_rev += 1;
                self.generation = next_generation();
                return true;
            }
        }
        false
    }

    /// Runs and records a group of commands as one undo step. Returns true if pdfium must reload.
    pub fn exec(&mut self, cmds: Vec<Cmd>) -> bool {
        if cmds.is_empty() {
            return false;
        }
        let mut reload = false;
        for c in &cmds {
            reload |= self.apply(c, true);
        }
        self.undo.push(cmds);
        self.redo.clear();
        reload
    }

    /// Records commands that were already applied (e.g. by a live gesture).
    pub fn record(&mut self, cmds: Vec<Cmd>) {
        if !cmds.is_empty() {
            self.undo.push(cmds);
            self.redo.clear();
        }
    }

    /// Replaces the `after` of the last undo step if it is a single `Modify` of the
    /// same annotation, and applies it. Used to merge e.g. slider drags into one step.
    pub fn amend_last_modify(&mut self, after: Annotation) {
        if let Some([Cmd::Modify { after: last, .. }]) = self.undo.last_mut().map(Vec::as_mut_slice)
            && last.id == after.id {
                *last = after.clone();
            }
        if let Some(i) = self.index_of(&after.id) {
            self.annots[i] = after.clone();
            self.touch(after.page);
        }
    }

    /// Like [`Doc::amend_last_modify`] for several annotations: each `after` replaces
    /// the result of its annotation's `Modify` in the last undo step (added if missing).
    pub fn amend_last_modifies(&mut self, afters: Vec<Annotation>) {
        for after in afters {
            let Some(i) = self.index_of(&after.id) else { continue };
            if let Some(step) = self.undo.last_mut() {
                let existing = step.iter_mut().find_map(|c| match c {
                    Cmd::Modify { after: a, .. } if a.id == after.id => Some(a),
                    _ => None,
                });
                match existing {
                    Some(a) => *a = after.clone(),
                    None => step.push(Cmd::Modify { before: self.annots[i].clone(), after: after.clone() }),
                }
            }
            self.touch(after.page);
            self.annots[i] = after;
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(cmds) = self.undo.pop() else { return false };
        let mut reload = false;
        for c in cmds.iter().rev() {
            reload |= self.apply(c, false);
        }
        self.redo.push(cmds);
        reload
    }

    pub fn redo(&mut self) -> bool {
        let Some(cmds) = self.redo.pop() else { return false };
        let mut reload = false;
        for c in &cmds {
            reload |= self.apply(c, true);
        }
        self.undo.push(cmds);
        reload
    }

    /// Number of steps that can be undone.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Saves to `out` (which may be the current path) and reopens from it. If the
    /// file changed on disk since it was loaded (saved from another window or app),
    /// this window's changes are combined with that version instead of replacing
    /// it; returns whether that happened.
    pub fn save_to(&mut self, out: &Path) -> Result<SaveOutcome> {
        if let Some(why) = &self.read_only {
            anyhow::bail!("{why}");
        }
        let same_file = out == self.path || out.canonicalize().ok() == self.path.canonicalize().ok();
        let mut outcome = SaveOutcome::default();
        if same_file
            && let Ok(disk) = std::fs::read(out)
            && disk != *self.bytes
        {
            let (conflicts, history_cleared) = self.rebase(disk)?;
            outcome = SaveOutcome { combined: true, conflicts, history_cleared };
        }
        let dirty = self.dirty_pages();
        save(&self.bytes, &self.edits(&dirty), out)?;
        let reopened = Doc::open(out)?;
        self.adopt(reopened);
        Ok(outcome)
    }

    /// Moves this window's unsaved changes onto `disk`, a newer version of the file:
    /// what it added or changed replaces or joins what's there, what it deleted is
    /// removed. Changes saved elsewhere are kept: an annotation changed on both
    /// sides is merged part by part (see [`merge3`]). Where that's not possible
    /// (both changed the same part, or another app now owns it) both versions are
    /// kept, ours under a new name; one deleted here but edited elsewhere is kept.
    /// Returns how many such conflicts there were, and whether the undo history
    /// had to be cleared (see [`SaveOutcome::history_cleared`]).
    fn rebase(&mut self, disk: Vec<u8>) -> Result<(usize, bool)> {
        let fresh = Doc::from_bytes(&self.path, disk);
        if let Some(why) = fresh.read_only.clone() {
            anyhow::bail!("the file was changed on disk and can't be combined with your changes: {why}");
        }
        // Pages are matched by page object: the file this window loaded (`base`),
        // this window's layout, and the new version may number them differently.
        let (base_pages, my_layout, new_pages) = (self.scan.pages.clone(), self.layout.clone(), fresh.scan.pages.clone());
        let pages_edited_here = my_layout != base_pages;
        let cant = |why: &str| anyhow::anyhow!("the file was changed elsewhere and can't be combined with your changes: {why}. Use Save As to keep your version");
        if pages_edited_here && new_pages != base_pages {
            return Err(cant("pages were rearranged both here and there"));
        }
        let new_index = |obj| new_pages.iter().position(|s: &PageSlot| s.obj == obj);
        // This window's annotations numbered like the loaded file, and like the new version.
        let as_base = |a: &Annotation| -> Option<Annotation> {
            let obj = my_layout.get(a.page)?.obj;
            Some(Annotation { page: base_pages.iter().position(|s| s.obj == obj)?, ..a.clone() })
        };
        let base_to_new = |a: &Annotation| -> Option<Annotation> {
            Some(Annotation { page: new_index(base_pages.get(a.page)?.obj)?, ..a.clone() })
        };
        let mine: Vec<Annotation> = self.annots.iter().map(|a| as_base(a).ok_or_else(|| cant("a page was brought back here"))).collect::<Result<_>>()?;
        // This window's changes, each with the version it started from.
        let base = |id: &str| self.saved.iter().find(|s| s.id == id).and_then(base_to_new);
        let removed: Vec<Annotation> =
            self.saved.iter().filter(|s| self.get(&s.id).is_none()).filter_map(base_to_new).collect();
        let changed: Vec<(Annotation, Option<Annotation>)> = mine
            .iter()
            .filter(|a| self.saved.iter().find(|s| s.id == a.id) != Some(*a))
            .map(|a| Ok((base_to_new(a).ok_or_else(|| cant("a page you annotated was deleted there"))?, base(&a.id))))
            .collect::<Result<_>>()?;
        if pages_edited_here {
            // Their new or changed annotations on pages deleted here would be lost.
            let kept: Vec<_> = my_layout.iter().map(|s| s.obj).collect();
            let theirs_lost = fresh.annots.iter().any(|a| {
                !kept.contains(&new_pages[a.page].obj) && self.saved.iter().find(|s| s.id == a.id) != Some(a)
            });
            if theirs_lost {
                return Err(cant("annotations were added there to a page deleted here"));
            }
        }
        self.adopt(fresh);
        let mut conflicts = 0;
        let mut kept: Vec<String> = Vec::new();
        for gone in removed {
            match self.index_of(&gone.id) {
                // Edited elsewhere meanwhile: keep that edit rather than delete it, and
                // drop the deletion from the history (undoing it would add a stale copy).
                Some(i) if self.annots[i] != gone => {
                    conflicts += 1;
                    kept.push(gone.id.clone());
                }
                Some(i) => {
                    self.annots.remove(i);
                }
                None => {}
            }
        }
        let mut renamed = std::collections::HashMap::new();
        for (mut a, base) in changed {
            let merged = match (self.index_of(&a.id), &base) {
                (Some(i), Some(base)) => match merge3(base, &a, &self.annots[i]) {
                    Some(m) => Ok((i, m)),
                    None => Err(()),
                },
                (Some(i), None) => Ok((i, a.clone())),
                // Gone from the new version: deleted there (ours wins, nothing is lost),
                // or edited by another app and now theirs (keep both).
                (None, _) => {
                    if self.foreign.iter().any(|f| f.attached && f.nm.as_deref() == Some(a.id.as_str())) {
                        Err(())
                    } else {
                        self.annots.push(a);
                        continue;
                    }
                }
            };
            match merged {
                Ok((i, m)) => self.annots[i] = m,
                Err(()) => {
                    let id = crate::annot::model::new_id();
                    renamed.insert(a.id.clone(), id.clone());
                    a.id = id;
                    self.annots.push(a);
                    conflicts += 1;
                }
            }
        }
        // Pages rearranged there: the history's page numbers follow (it's all in the
        // file's numbering, unless it has page edits of its own; then it's cleared).
        let mut history_cleared = false;
        if !pages_edited_here && new_pages != base_pages {
            if self.undo.iter().chain(&self.redo).flatten().any(|c| matches!(c, Cmd::Pages { .. })) {
                (self.undo, self.redo) = (Vec::new(), Vec::new());
                history_cleared = true;
            } else {
                let renumber = |a: &mut Annotation| -> bool {
                    match base_pages.get(a.page).and_then(|s| new_index(s.obj)) {
                        Some(p) => {
                            a.page = p;
                            true
                        }
                        None => false, // its page was deleted there
                    }
                };
                for steps in [&mut self.undo, &mut self.redo] {
                    for step in steps.iter_mut() {
                        step.retain_mut(|c| match c {
                            Cmd::Add(a) | Cmd::Remove { annot: a, .. } => renumber(a),
                            Cmd::Modify { before, after } => renumber(before) && renumber(after),
                            Cmd::DeleteForeign(_) | Cmd::Pages { .. } => true,
                        });
                    }
                    steps.retain(|s| !s.is_empty());
                }
            }
        }
        if pages_edited_here {
            // Back to this window's page layout.
            for a in &mut self.annots {
                if let Some(p) = my_layout.iter().position(|s| s.obj == new_pages[a.page].obj) {
                    a.page = p;
                }
            }
            self.layout = my_layout;
            self.layout_rev += 1;
            self.generation = next_generation();
        }
        for steps in [&mut self.undo, &mut self.redo] {
            for step in steps.iter_mut() {
                step.retain(|c| !matches!(c, Cmd::Remove { annot, .. } if kept.contains(&annot.id)));
            }
            steps.retain(|s| !s.is_empty());
        }
        // The history follows the renames.
        for step in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            for cmd in step {
                let annots: Vec<&mut Annotation> = match cmd {
                    Cmd::Add(a) | Cmd::Remove { annot: a, .. } => vec![a],
                    Cmd::Modify { before, after } => vec![before, after],
                    Cmd::Pages { removed, .. } => removed.iter_mut().map(|(_, a)| a).collect(),
                    Cmd::DeleteForeign(_) => vec![],
                };
                for a in annots {
                    if let Some(id) = renamed.get(&a.id) {
                        a.id = id.clone();
                    }
                }
            }
        }
        Ok((conflicts, history_cleared))
    }

    /// Switches to `fresh`, a newly loaded version of the file, keeping the page
    /// layout, contents, links and undo history. Other apps' annotations are
    /// matched up by their unique name, or failing that object number plus type and
    /// position, so a renumbered or reused object number never points a deletion at
    /// the wrong one. Pending deletions stay pending; ones the new version doesn't
    /// list (deleted by a save, or restored by undo but not yet saved) stay known,
    /// detached, so undo can bring them back, as long as their object is verifiably
    /// still in the file. Anything uncertain is left alone.
    fn adopt(&mut self, fresh: Doc) {
        let pages = std::mem::take(&mut self.pages);
        let (outline, links) = (std::mem::take(&mut self.outline), std::mem::take(&mut self.links));
        let (old_foreign, old_deleted) = (std::mem::take(&mut self.foreign), std::mem::take(&mut self.deleted_foreign));
        let (undo, redo) = (std::mem::take(&mut self.undo), std::mem::take(&mut self.redo));
        let (boxes, layout_rev) = (std::mem::take(&mut self.boxes), self.layout_rev + 1);
        *self = fresh;
        (self.boxes, self.layout_rev) = (boxes, layout_rev);
        // Until pdfium reloads, page geometry must at least match the new layout.
        let pages = if pages.len() == self.layout.len() {
            pages
        } else {
            self.layout
                .iter()
                .map(|s| PageGeom { bbox: self.boxes.get(&s.obj).copied().unwrap_or([0.0, 0.0, 612.0, 792.0]), rotation: s.rotation })
                .collect()
        };
        self.set_pages(pages);
        (self.outline, self.links) = (outline, links);
        let mut map = std::collections::HashMap::new();
        for (i, f) in old_foreign.into_iter().enumerate() {
            let deleted = old_deleted.contains(&i);
            let id = f.identity();
            let named = f.nm.is_some();
            let in_file = self
                .foreign
                .iter()
                .position(|g| g.attached && g.identity().same(&id) && (named || (f.obj.is_some() && g.obj == f.obj)));
            if let Some(j) = in_file {
                map.insert(i, j);
                if deleted {
                    self.deleted_foreign.insert(j);
                }
                continue;
            }
            if !(deleted || !f.attached) {
                continue;
            }
            // Detached: its object must still be in the file and still be it.
            let objects = &self.scan.objects;
            let same_obj = f.obj.filter(|o| objects.get(o).is_some_and(|x| x.same(&id)));
            let moved = || named.then(|| objects.iter().find(|(_, x)| x.same(&id)).map(|(o, _)| *o)).flatten();
            let Some(obj) = same_obj.or_else(moved) else { continue };
            // Its popups only if it kept its number (otherwise theirs may have changed too).
            let popup_objs = if Some(obj) == f.obj {
                f.popup_objs.iter().copied().filter(|p| objects.get(p).is_some_and(|x| x.subtype == "Popup")).collect()
            } else {
                Vec::new()
            };
            let j = self.foreign.len();
            map.insert(i, j);
            if deleted {
                self.deleted_foreign.insert(j);
            }
            self.foreign.push(Foreign { attached: false, index: 0, popups: Vec::new(), obj: Some(obj), popup_objs, ..f });
        }
        let remap = |steps: Vec<Vec<Cmd>>| -> Vec<Vec<Cmd>> {
            steps
                .into_iter()
                .map(|s| {
                    s.into_iter()
                        .filter_map(|c| match c {
                            Cmd::DeleteForeign(i) => map.get(&i).map(|&j| Cmd::DeleteForeign(j)),
                            c => Some(c),
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|s| !s.is_empty())
                .collect()
        };
        (self.undo, self.redo) = (remap(undo), remap(redo));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annot::model::{Kind, MarkupKind, Pt, ShapeKind, Style};

    /// A minimal one-page PDF with one foreign (not ours) square annotation.
    fn sample_pdf() -> Vec<u8> {
        use lopdf::{Object, Stream, dictionary};
        let mut doc = lopdf::Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let content = doc.add_object(Stream::new(dictionary! {}, b"0 0 1 rg 100 100 50 50 re f".to_vec()));
        let foreign = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Square",
            "Rect" => vec![10.into(), 10.into(), 60.into(), 60.into()],
            "NM" => Object::string_literal("okular-123"),
            "C" => vec![1.into(), 0.into(), 0.into()],
            "T" => Object::string_literal("Someone"),
            "Contents" => Object::string_literal("Looks wrong"),
        });
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Annots" => vec![foreign.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn all_kinds() -> Vec<Annotation> {
        let s = Style::new([0.2, 0.4, 0.8], 3.0, 0.75);
        vec![
            Annotation::new(
                0,
                s,
                Kind::Ink {
                    curve: vec![Pt::new(10.0, 10.0), Pt::new(20.0, 30.0), Pt::new(40.0, 30.0), Pt::new(50.0, 10.0)],
                    highlighter: false,
                },
            ),
            Annotation::new(0, s, Kind::Ink { curve: vec![Pt::new(5.0, 5.0)], highlighter: true }),
            Annotation::new(
                0,
                Style::new([0.0; 3], 14.0, 1.0),
                Kind::Text {
                    origin: Pt::new(100.0, 700.0),
                    right: Pt::new(1.0, 0.0),
                    down: Pt::new(0.0, -1.0),
                    text: "Hello (world)\nÜnïcode ✓".into(),
                },
            ),
            Annotation::new(0, s, Kind::Shape { shape: ShapeKind::Rect, a: Pt::new(1.0, 2.0), b: Pt::new(30.0, 40.0) }),
            Annotation::new(0, s, Kind::Shape { shape: ShapeKind::Ellipse, a: Pt::new(1.0, 2.0), b: Pt::new(30.0, 40.0) }),
            Annotation::new(0, s, Kind::Shape { shape: ShapeKind::Arrow, a: Pt::new(1.0, 2.0), b: Pt::new(30.0, 40.0) }),
            Annotation::new(0, s, Kind::Shape { shape: ShapeKind::Check, a: Pt::new(50.0, 70.0), b: Pt::new(66.0, 54.0) }),
            Annotation::new(0, s, Kind::Shape { shape: ShapeKind::Cross, a: Pt::new(80.0, 70.0), b: Pt::new(96.0, 54.0) }),
            Annotation::new(
                0,
                s,
                Kind::Markup {
                    markup: MarkupKind::Highlight,
                    quads: vec![[Pt::new(0.0, 10.0), Pt::new(50.0, 10.0), Pt::new(0.0, 0.0), Pt::new(50.0, 0.0)]],
                },
            ),
        ]
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ochre-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn round_trip_keeps_foreign_annotations_and_original_bytes() {
        let path = tmp("round_trip.pdf");
        let original = sample_pdf();
        std::fs::write(&path, &original).unwrap();

        let mut doc = Doc::open(&path).unwrap();
        assert!(doc.annots.is_empty());
        assert_eq!(doc.foreign.len(), 1);
        assert_eq!(doc.foreign[0].note.as_deref(), Some("Looks wrong"));
        let mut created = all_kinds();
        let noted = created.len() - 1;
        created[noted].note = "Why? ✓".into();
        doc.exec(created.iter().cloned().map(Cmd::Add).collect());
        assert!(doc.is_dirty());
        doc.save_to(&path).unwrap();

        // Incremental update: the original file is an untouched prefix.
        let saved = std::fs::read(&path).unwrap();
        assert!(saved.starts_with(&original));

        // Our annotations come back identical and editable; the foreign one is untouched.
        assert_eq!(doc.annots, created);
        // The note is a standard /Contents comment that other viewers show.
        let d = lopdf::Document::load(&path).unwrap();
        let contents: Vec<String> = d
            .objects
            .values()
            .filter_map(|o| o.as_dict().ok())
            .filter(|d| d.get(b"Subtype").and_then(|s| s.as_name()).ok() == Some(b"Highlight"))
            .filter_map(|d| d.get(b"Contents").and_then(|c| c.as_str()).ok())
            .map(crate::pdf::write::decode_text_string)
            .collect();
        assert_eq!(contents, ["Why? ✓"]);
        assert_eq!(doc.foreign.len(), 1);
        assert_eq!(doc.foreign[0].subtype, "Square");
        assert!(!doc.is_dirty());

        // Edit one, delete another, save again.
        let mut moved = doc.annots[0].clone();
        moved.translate(Pt::new(5.0, 5.0));
        let removed = doc.annots[1].clone();
        doc.exec(vec![
            Cmd::Modify { before: doc.annots[0].clone(), after: moved.clone() },
            Cmd::Remove { annot: removed, index: 1 },
        ]);
        doc.save_to(&path).unwrap();
        assert_eq!(doc.annots.len(), created.len() - 1);
        assert_eq!(doc.annots[0], moved);
        assert_eq!(doc.foreign.len(), 1);

        // Deleting the foreign annotation is explicit and undoable.
        doc.exec(vec![Cmd::DeleteForeign(0)]);
        doc.undo();
        assert!(!doc.is_dirty());
        doc.redo();
        doc.save_to(&path).unwrap();
        // Gone from the page; only a detached record remains, for undo.
        assert!(Doc::open(&path).unwrap().foreign.is_empty());
        assert!(doc.foreign.iter().enumerate().all(|(i, f)| !f.attached && !doc.foreign_shown(i)));
        assert_eq!(doc.annots.len(), created.len() - 1);
    }

    #[test]
    fn saving_and_undoing_never_remove_other_apps_annotations() {
        let path = tmp("keep_foreign.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let foreign_in_file = |path: &Path| {
            let d = lopdf::Document::load(path).unwrap();
            let (_, page) = d.get_pages().into_iter().next().unwrap();
            let annots = d.get_dictionary(page).unwrap().get(b"Annots").unwrap().as_array().unwrap().clone();
            annots
                .iter()
                .filter_map(|o| d.dereference(o).ok()?.1.as_dict().ok().cloned())
                .filter(|a| a.get(b"NM").and_then(|n| n.as_str()).ok() == Some(b"okular-123"))
                .count()
        };
        let mut doc = Doc::open(&path).unwrap();
        let mut kinds = all_kinds();
        kinds.truncate(3);
        // Several saves with edits on the foreign annotation's page, then undo
        // everything (across the saves) and save again.
        for a in kinds {
            doc.exec(vec![Cmd::Add(a)]);
            doc.save_to(&path).unwrap();
            assert_eq!(foreign_in_file(&path), 1);
        }
        while doc.can_undo() {
            doc.undo();
            assert_eq!(doc.foreign.len(), 1);
            assert!(doc.deleted_foreign.is_empty());
        }
        assert!(doc.annots.is_empty());
        doc.save_to(&path).unwrap();
        assert_eq!(foreign_in_file(&path), 1, "still in the page's annotation list");
        let reopened = Doc::open(&path).unwrap();
        assert_eq!(reopened.foreign.len(), 1);
        assert!(reopened.annots.is_empty());
        // Redo it all and save: ours come back next to it.
        let mut doc = reopened;
        doc.exec(all_kinds().into_iter().map(Cmd::Add).collect());
        doc.save_to(&path).unwrap();
        assert_eq!(foreign_in_file(&path), 1);
    }

    /// One page with another app's square (green outline, at 400..500) that has a popup.
    fn pdf_with_popup() -> Vec<u8> {
        use lopdf::{Object, Stream, dictionary};
        let mut doc = lopdf::Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let ap = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()] },
            b"0 0.6 0 RG 4 w 2 2 96 96 re S".to_vec(),
        ));
        let square = doc.new_object_id();
        let popup = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Popup", "Parent" => square,
            "Rect" => vec![500.into(), 500.into(), 600.into(), 560.into()],
        });
        doc.objects.insert(square, Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Square", "NM" => Object::string_literal("onlyoffice-1"),
            "Rect" => vec![400.into(), 400.into(), 500.into(), 500.into()],
            "Contents" => Object::string_literal("Check this"), "Popup" => popup,
            "AP" => dictionary! { "N" => ap },
        }));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Annots" => vec![square.into(), popup.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn deleting_another_apps_annotation_can_be_undone_after_saving() {
        use crate::pdf::worker::{Req, Resp, Worker};
        let path = tmp("undo_foreign.pdf");
        std::fs::write(&path, pdf_with_popup()).unwrap();
        // References in the page's /Annots in the saved file.
        let in_file = |path: &Path| -> usize {
            let d = lopdf::Document::load(path).unwrap();
            let (_, page) = d.get_pages().into_iter().next().unwrap();
            d.get_dictionary(page).unwrap().get(b"Annots").and_then(|a| a.as_array().map(Vec::len)).unwrap_or(0)
        };
        // Whether pdfium draws the square's green outline (left edge at x = 402).
        let worker = Worker::spawn(eframe::egui::Context::default());
        let drawn = |doc: &mut Doc| -> Option<bool> {
            let w = worker.as_ref().ok()?;
            let wait = || w.rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            w.send(Req::Load { generation: doc.generation, bytes: doc.render_bytes(), hide: doc.hidden() });
            let Resp::Loaded { .. } = wait() else { panic!("load failed") };
            w.send(Req::Render { generation: doc.generation, page: 0, scale: 1.0, thumb: false });
            let Resp::Rendered { image, .. } = wait() else { panic!("render failed") };
            let px = image.pixels[(792 - 450) * image.size[0] + 402];
            Some(px.g() > 100 && px.r() < 100)
        };

        let mut doc = Doc::open(&path).unwrap();
        let square = doc.foreign.iter().position(|f| f.subtype == "Square").unwrap();
        assert!(drawn(&mut doc).unwrap_or(true));
        doc.exec(vec![Cmd::DeleteForeign(square)]);
        assert!(!drawn(&mut doc).unwrap_or(false));
        doc.save_to(&path).unwrap();
        assert_eq!(in_file(&path), 0, "the save removed it and its popup");
        assert!(!doc.is_dirty());

        // Undo after saving brings it back: shown right away, written on the next save.
        assert!(doc.can_undo());
        doc.undo();
        assert!(doc.is_dirty());
        assert!(drawn(&mut doc).unwrap_or(true));
        let shown = doc.foreign.iter().enumerate().filter(|(i, f)| f.subtype == "Square" && doc.foreign_shown(*i));
        assert_eq!(shown.count(), 1);
        doc.save_to(&path).unwrap();
        assert_eq!(in_file(&path), 2, "square and popup are back on the page");
        let reopened = Doc::open(&path).unwrap();
        let back = reopened.foreign.iter().find(|f| f.subtype == "Square").unwrap();
        assert!(back.attached);
        assert_eq!(back.note.as_deref(), Some("Check this"));

        // Redo deletes it again, and that too survives a save and an undo.
        doc.redo();
        doc.save_to(&path).unwrap();
        assert_eq!(in_file(&path), 0);
        doc.undo();
        doc.save_to(&path).unwrap();
        assert_eq!(in_file(&path), 2);
        assert!(drawn(&mut doc).unwrap_or(true));
    }

    #[test]
    fn saving_keeps_changes_saved_meanwhile_by_another_window() {
        let path = tmp("two_windows.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut kinds = all_kinds().into_iter();
        let mut a = Doc::open(&path).unwrap();
        let mut b = Doc::open(&path).unwrap();
        // Window A adds one and saves; then window B, opened before that, adds one and saves.
        let (from_a, from_b) = (kinds.next().unwrap(), kinds.next().unwrap());
        a.exec(vec![Cmd::Add(from_a.clone())]);
        a.save_to(&path).unwrap();
        b.exec(vec![Cmd::Add(from_b.clone())]);
        b.save_to(&path).unwrap();
        let on_disk = Doc::open(&path).unwrap();
        let ids: Vec<&str> = on_disk.annots.iter().map(|x| x.id.as_str()).collect();
        assert!(ids.contains(&from_a.id.as_str()), "A's annotation survived B's save: {ids:?}");
        assert!(ids.contains(&from_b.id.as_str()));
        assert_eq!(on_disk.foreign.len(), 1);
        // B now shows both, and its own change is still undoable.
        assert_eq!(b.annots.len(), 2);
        assert!(!b.is_dirty());
        b.undo();
        assert_eq!(b.annots.len(), 1);
        assert_eq!(b.annots[0].id, from_a.id);

        // A deleting its annotation and saving, after B's save, keeps B's.
        a.exec(vec![Cmd::Remove { annot: from_a.clone(), index: 0 }]);
        a.save_to(&path).unwrap();
        let on_disk = Doc::open(&path).unwrap();
        assert_eq!(on_disk.annots.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), [from_b.id.as_str()]);
    }

    #[test]
    fn an_annotation_edited_by_another_app_is_not_reverted() {
        let path = tmp("edited_comment.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        let mut ink = all_kinds().remove(0);
        ink.note = "original".into();
        doc.exec(vec![Cmd::Add(ink.clone())]);
        doc.save_to(&path).unwrap();

        // Another viewer edits just the comment (same /Rect).
        let mut d = lopdf::Document::load(&path).unwrap();
        for obj in d.objects.values_mut() {
            let Ok(dict) = obj.as_dict_mut() else { continue };
            if dict.get(b"NM").and_then(|n| n.as_str()).ok() == Some(ink.id.as_bytes()) {
                dict.set("Contents", crate::pdf::write::text_string("edited elsewhere"));
            }
        }
        d.save(&path).unwrap();

        // Ochre leaves it to that app now, so saving changes on the page keeps the edit.
        let mut doc = Doc::open(&path).unwrap();
        assert!(doc.annots.is_empty(), "no longer treated as Ochre's");
        assert_eq!(doc.foreign.len(), 2);
        doc.exec(vec![Cmd::Add(all_kinds().remove(3))]);
        doc.save_to(&path).unwrap();
        let reopened = Doc::open(&path).unwrap();
        let notes: Vec<Option<String>> = reopened.foreign.iter().map(|f| f.note.clone()).collect();
        assert!(notes.contains(&Some("edited elsewhere".into())), "{notes:?}");
    }

    #[test]
    fn a_program_rewriting_the_whole_file_doesnt_take_our_annotations() {
        let path = tmp("rewritten.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        doc.exec(all_kinds().into_iter().map(Cmd::Add).collect());
        doc.save_to(&path).unwrap();
        // Another program loads and saves the whole file: objects renumbered and
        // streams compressed, but no annotation changed.
        let mut d = lopdf::Document::load(&path).unwrap();
        d.renumber_objects_with(1000);
        d.compress();
        d.save(&path).unwrap();
        let reopened = Doc::open(&path).unwrap();
        assert_eq!(reopened.annots, doc.annots, "still Ochre's, still editable");
        assert_eq!(reopened.foreign.len(), 1);
    }

    /// The `/NM`s of every annotation on page 1 of the file, in order.
    fn names_on_disk(path: &Path) -> Vec<String> {
        let d = lopdf::Document::load(path).unwrap();
        let (_, page) = d.get_pages().into_iter().next().unwrap();
        let annots = d.get_dictionary(page).unwrap().get(b"Annots").and_then(|a| a.as_array().cloned()).unwrap_or_default();
        annots
            .iter()
            .filter_map(|o| d.dereference(o).ok()?.1.as_dict().ok().cloned())
            .map(|a| String::from_utf8_lossy(a.get(b"NM").and_then(|n| n.as_str()).unwrap_or(b"")).into_owned())
            .collect()
    }

    /// Rewrites the file the way another program might, applying `f` to every
    /// annotation dictionary (with its object id).
    fn rewrite(path: &Path, mut f: impl FnMut(lopdf::ObjectId, &mut lopdf::Dictionary)) {
        let mut d = lopdf::Document::load(path).unwrap();
        for (id, obj) in d.objects.iter_mut() {
            if let Ok(dict) = obj.as_dict_mut()
                && dict.get(b"Type").and_then(|t| t.as_name()).ok() == Some(b"Annot")
            {
                f(*id, dict);
            }
        }
        d.save(path).unwrap();
    }

    #[test]
    fn a_reused_object_number_never_deletes_the_wrong_annotation() {
        let path = tmp("reused_number.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut b = Doc::open(&path).unwrap();
        // B deletes the other app's square, but hasn't saved.
        b.exec(vec![Cmd::DeleteForeign(0)]);
        // Meanwhile a program rewrites the file: the square is gone and a different
        // annotation now has its object number.
        rewrite(&path, |_, dict| {
            dict.set("NM", lopdf::Object::string_literal("someone-else-7"));
            dict.set("Rect", vec![200.into(), 200.into(), 260.into(), 240.into()]);
        });
        b.save_to(&path).unwrap();
        assert_eq!(names_on_disk(&path), ["someone-else-7"], "the unrelated annotation survives");

        // A deletion already saved (kept detached for undo) isn't restored onto
        // whatever reuses its number after a rewrite either.
        let path = tmp("reused_number_2.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut b = Doc::open(&path).unwrap();
        b.exec(vec![Cmd::DeleteForeign(0)]);
        b.save_to(&path).unwrap();
        assert!(names_on_disk(&path).is_empty());
        // Another program adds a new annotation that happens to get the freed number.
        use lopdf::dictionary;
        let mut d = lopdf::Document::load(&path).unwrap();
        let old = b.foreign.iter().find_map(|f| f.obj).unwrap();
        d.objects.insert(
            old,
            lopdf::Object::Dictionary(dictionary! {
                "Type" => "Annot", "Subtype" => "Circle", "NM" => lopdf::Object::string_literal("newcomer"),
                "Rect" => vec![10.into(), 10.into(), 50.into(), 50.into()],
            }),
        );
        let (_, page) = d.get_pages().into_iter().next().unwrap();
        d.get_dictionary_mut(page).unwrap().set("Annots", vec![lopdf::Object::Reference(old)]);
        d.save(&path).unwrap();
        b.undo(); // would put the old square back
        b.save_to(&path).unwrap();
        assert_eq!(names_on_disk(&path), ["newcomer"], "not listed twice, nothing wrong restored");
    }

    #[test]
    fn comments_edited_elsewhere_are_kept_in_files_from_older_versions() {
        let path = tmp("older_format.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        let mut ink = all_kinds().remove(0);
        ink.note = "original".into();
        doc.exec(vec![Cmd::Add(ink.clone())]);
        doc.save_to(&path).unwrap();
        // Make it look like it was saved by Ochre 1.2.0 (no fingerprint), then edit
        // its comment in another app.
        rewrite(&path, |_, dict| {
            if dict.get(b"NM").and_then(|n| n.as_str()).ok() != Some(ink.id.as_bytes()) {
                return;
            }
            let raw = dict.get(crate::pdf::write::PRIVATE_KEY).unwrap().as_str().unwrap().to_vec();
            let mut json: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            json.as_object_mut().unwrap().remove("written");
            let raw = serde_json::to_vec(&json).unwrap();
            dict.set(crate::pdf::write::PRIVATE_KEY, lopdf::Object::String(raw, lopdf::StringFormat::Hexadecimal));
            dict.set("Contents", crate::pdf::write::text_string("edited elsewhere"));
        });
        let doc = Doc::open(&path).unwrap();
        assert!(doc.annots.is_empty(), "the edited one is now the other app's");
        assert!(doc.foreign.iter().any(|f| f.note.as_deref() == Some("edited elsewhere")));

        // Unedited ones from older versions are still Ochre's.
        let path = tmp("older_format_unedited.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        let mut kinds = all_kinds();
        kinds[0].note = "kept".into();
        doc.exec(kinds.iter().cloned().map(Cmd::Add).collect());
        doc.save_to(&path).unwrap();
        rewrite(&path, |_, dict| {
            let Ok(raw) = dict.get(crate::pdf::write::PRIVATE_KEY).and_then(|r| r.as_str()).map(<[u8]>::to_vec) else { return };
            let mut json: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            json.as_object_mut().unwrap().remove("written");
            dict.set(crate::pdf::write::PRIVATE_KEY, lopdf::Object::String(serde_json::to_vec(&json).unwrap(), lopdf::StringFormat::Hexadecimal));
        });
        assert_eq!(Doc::open(&path).unwrap().annots, kinds);
    }

    #[test]
    fn editing_the_same_annotation_here_and_elsewhere_keeps_both_without_clashing() {
        let path = tmp("both_edited.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        let ink = all_kinds().remove(0);
        doc.exec(vec![Cmd::Add(ink.clone())]);
        doc.save_to(&path).unwrap();
        // Here: recolor it (unsaved). Elsewhere: add a comment to it, and save.
        let mut mine = ink.clone();
        mine.style.color = [0.0, 0.6, 0.0];
        doc.exec(vec![Cmd::Modify { before: ink.clone(), after: mine.clone() }]);
        rewrite(&path, |_, dict| {
            if dict.get(b"NM").and_then(|n| n.as_str()).ok() == Some(ink.id.as_bytes()) {
                dict.set("Contents", crate::pdf::write::text_string("their comment"));
            }
        });
        let outcome = doc.save_to(&path).unwrap();
        assert_eq!(outcome.conflicts, 1);
        let names = names_on_disk(&path);
        let unique: BTreeSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "no two annotations share a name: {names:?}");
        // Theirs (with the comment) and mine (green, under a new name) are both there.
        let reopened = Doc::open(&path).unwrap();
        assert!(reopened.foreign.iter().any(|f| f.note.as_deref() == Some("their comment")));
        let kept = reopened.annots.iter().find(|a| a.style.color == [0.0, 0.6, 0.0]).expect("my version kept");
        assert_ne!(kept.id, ink.id);
        // And my edit is still undoable, now under the new name.
        assert_eq!(doc.get(&kept.id).map(|a| a.style.color), Some([0.0, 0.6, 0.0]));
        doc.undo();
        assert_eq!(doc.get(&kept.id).map(|a| a.style.color), Some(ink.style.color));
    }

    #[test]
    fn two_windows_editing_the_same_annotation_merge_their_edits() {
        let path = tmp("same_annotation.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut setup = Doc::open(&path).unwrap();
        let ink = all_kinds().remove(0);
        setup.exec(vec![Cmd::Add(ink.clone())]);
        setup.save_to(&path).unwrap();
        let open = || Doc::open(&path).unwrap();
        let edit = |doc: &mut Doc, f: &dyn Fn(&mut Annotation)| {
            let before = doc.get(&ink.id).unwrap().clone();
            let mut after = before.clone();
            f(&mut after);
            doc.exec(vec![Cmd::Modify { before, after }]);
        };
        let saved = || Doc::open(&path).unwrap();

        // Different parts: A adds a note, B recolors. Both survive, no conflict.
        let (mut a, mut b) = (open(), open());
        edit(&mut a, &|x| x.note = "from A".into());
        a.save_to(&path).unwrap();
        edit(&mut b, &|x| x.style.color = [0.0, 0.6, 0.0]);
        let outcome = b.save_to(&path).unwrap();
        assert_eq!(outcome, SaveOutcome { combined: true, conflicts: 0, history_cleared: false });
        let merged = saved().get(&ink.id).unwrap().clone();
        assert_eq!((merged.note.as_str(), merged.style.color), ("from A", [0.0, 0.6, 0.0]));
        assert_eq!(saved().annots.len(), 1);

        // The same part, differently: both versions kept, B's under a new name.
        let (mut a, mut b) = (open(), open());
        edit(&mut a, &|x| x.style.width = 9.0);
        a.save_to(&path).unwrap();
        edit(&mut b, &|x| x.style.width = 1.0);
        assert_eq!(b.save_to(&path).unwrap().conflicts, 1);
        let widths: BTreeSet<String> = saved().annots.iter().map(|x| x.style.width.to_string()).collect();
        assert_eq!(widths, BTreeSet::from(["9".to_string(), "1".to_string()]));

        // Deleted here but edited elsewhere: the edit is kept.
        let mut a = open();
        let mut b = open();
        edit(&mut a, &|x| x.note = "keep me".into());
        a.save_to(&path).unwrap();
        let index = b.index_of(&ink.id).unwrap();
        b.exec(vec![Cmd::Remove { annot: b.annots[index].clone(), index }]);
        assert_eq!(b.save_to(&path).unwrap().conflicts, 1);
        assert_eq!(saved().get(&ink.id).map(|x| x.note.clone()), Some("keep me".into()));
    }

    #[test]
    fn undo_after_a_merge_only_undoes_this_windows_change() {
        let path = tmp("undo_after_merge.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut setup = Doc::open(&path).unwrap();
        let ink = all_kinds().remove(0);
        setup.exec(vec![Cmd::Add(ink.clone())]);
        setup.save_to(&path).unwrap();
        let edit = |doc: &mut Doc, f: &dyn Fn(&mut Annotation)| {
            let before = doc.get(&ink.id).unwrap().clone();
            let mut after = before.clone();
            f(&mut after);
            doc.exec(vec![Cmd::Modify { before, after }]);
        };
        let ids_unique = |doc: &Doc| {
            let ids: BTreeSet<&str> = doc.annots.iter().map(|a| a.id.as_str()).collect();
            ids.len() == doc.annots.len()
        };

        // A adds a note, B recolors and saves (merged), then B undoes and saves:
        // only B's color goes back; A's note stays.
        let (mut a, mut b) = (Doc::open(&path).unwrap(), Doc::open(&path).unwrap());
        edit(&mut a, &|x| x.note = "from A".into());
        a.save_to(&path).unwrap();
        edit(&mut b, &|x| x.style.color = [0.0, 0.6, 0.0]);
        b.save_to(&path).unwrap();
        b.undo();
        assert_eq!(b.get(&ink.id).map(|x| (x.note.clone(), x.style.color)), Some(("from A".into(), ink.style.color)));
        b.save_to(&path).unwrap();
        let on_disk = Doc::open(&path).unwrap().get(&ink.id).unwrap().clone();
        assert_eq!((on_disk.note.as_str(), on_disk.style.color), ("from A", ink.style.color));
        // Redo brings B's color back, still with A's note.
        b.redo();
        assert_eq!(b.get(&ink.id).map(|x| (x.note.clone(), x.style.color)), Some(("from A".into(), [0.0, 0.6, 0.0])));

        // B deletes what A edited: the merge keeps A's version, and undo doesn't add
        // B's old copy next to it.
        let (mut a, mut b) = (Doc::open(&path).unwrap(), Doc::open(&path).unwrap());
        edit(&mut a, &|x| x.note = "A again".into());
        a.save_to(&path).unwrap();
        let index = b.index_of(&ink.id).unwrap();
        b.exec(vec![Cmd::Remove { annot: b.annots[index].clone(), index }]);
        b.save_to(&path).unwrap();
        while b.can_undo() {
            b.undo();
            assert!(ids_unique(&b), "{:?}", b.annots.iter().map(|x| &x.id).collect::<Vec<_>>());
        }
        assert_eq!(b.get(&ink.id).map(|x| x.note.as_str()), Some("A again"));
        b.save_to(&path).unwrap();
        assert!(ids_unique(&Doc::open(&path).unwrap()));
    }

    /// Five pages, each `500 + 10 * i` points wide so they can be told apart, in a
    /// nested page tree whose middle node holds what pages 2-5 inherit (resources,
    /// height). Another app's square sits on page 3.
    fn five_page_pdf() -> Vec<u8> {
        use lopdf::{Object, Stream, dictionary};
        let mut doc = lopdf::Document::with_version("1.7");
        let root = doc.new_object_id();
        let middle = doc.new_object_id();
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let resources = dictionary! { "Font" => dictionary! { "F1" => font } };
        let mut pages = Vec::new();
        for i in 0..5i64 {
            let text = format!("BT /F1 24 Tf 72 700 Td (Page {}) Tj ET", i + 1);
            let content = doc.add_object(Stream::new(dictionary! {}, text.into_bytes()));
            let mut page = dictionary! {
                "Type" => "Page", "Parent" => if i == 0 { root } else { middle }, "Contents" => content,
                "MediaBox" => vec![0.into(), 0.into(), (500 + 10 * i).into(), 792.into()],
            };
            if i == 0 {
                page.set("Resources", resources.clone());
            } else {
                // Width only: the height is inherited, as are the resources.
                page.set("MediaBox", vec![0.into(), 0.into(), (500 + 10 * i).into(), 792.into()]);
            }
            if i == 2 {
                let square = doc.add_object(dictionary! {
                    "Type" => "Annot", "Subtype" => "Square", "NM" => Object::string_literal("theirs-1"),
                    "Rect" => vec![100.into(), 100.into(), 200.into(), 200.into()],
                });
                page.set("Annots", vec![square.into()]);
            }
            pages.push(doc.add_object(page));
        }
        doc.objects.insert(middle, Object::Dictionary(dictionary! {
            "Type" => "Pages", "Parent" => root, "Count" => 4, "Resources" => resources,
            "Kids" => pages[1..].iter().map(|&p| p.into()).collect::<Vec<Object>>(),
        }));
        doc.objects.insert(root, Object::Dictionary(dictionary! {
            "Type" => "Pages", "Count" => 5, "Kids" => vec![pages[0].into(), middle.into()],
        }));
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    /// Widths of the file's pages in order (identifies them), and their rotations.
    fn page_widths(path: &Path) -> Vec<(i64, i64)> {
        let d = lopdf::Document::load(path).unwrap();
        d.get_pages()
            .values()
            .map(|&id| {
                let page = d.get_dictionary(id).unwrap();
                // Every page must have what it used to inherit, wherever it is now.
                let media = page.get(b"MediaBox").unwrap().as_array().unwrap();
                assert!(page.has(b"Resources") || d.get_dictionary(page.get(b"Parent").unwrap().as_reference().unwrap()).unwrap().has(b"Resources"));
                let rotate = page.get(b"Rotate").and_then(|r| r.as_i64()).unwrap_or(0);
                (media[2].as_float().unwrap() as i64, rotate)
            })
            .collect()
    }

    #[test]
    fn rotate_delete_and_reorder_pages() {
        let path = tmp("page_tools.pdf");
        std::fs::write(&path, five_page_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        let set = |v: &[usize]| v.iter().copied().collect::<BTreeSet<usize>>();
        // Our annotation on page 4; the other app's square is on page 3.
        let mut mark = all_kinds().remove(0);
        mark.page = 3;
        doc.exec(vec![Cmd::Add(mark.clone())]);
        doc.save_to(&path).unwrap();

        // Rotate page 1, move pages 4-5 to the front.
        doc.rotate_pages(&set(&[0]), true);
        doc.move_pages(&set(&[3, 4]), 0);
        assert!(doc.is_dirty());
        assert_eq!(doc.get(&mark.id).unwrap().page, 0, "the annotation moved with its page");
        assert_eq!(doc.foreign_page(0), Some(4), "and so did the other app's");
        doc.save_to(&path).unwrap();
        assert_eq!(page_widths(&path), [(530, 0), (540, 0), (500, 90), (510, 0), (520, 0)]);
        let reopened = Doc::open(&path).unwrap();
        assert_eq!(reopened.annots.iter().map(|a| (a.id.clone(), a.page)).collect::<Vec<_>>(), [(mark.id.clone(), 0)]);
        assert_eq!(reopened.foreign_page(0), Some(4));

        // Deleting the page with our annotation takes the annotation along; undo
        // (even after saving) brings both back, without doubling the annotation.
        assert!(doc.delete_pages(&set(&[0])));
        assert!(doc.get(&mark.id).is_none());
        doc.save_to(&path).unwrap();
        assert_eq!(page_widths(&path).len(), 4);
        doc.undo();
        assert_eq!(doc.get(&mark.id).map(|a| a.page), Some(0));
        assert_eq!(doc.hidden()[0].len(), 1, "the copy still in the file isn't drawn twice");
        doc.save_to(&path).unwrap();
        assert_eq!(page_widths(&path), [(530, 0), (540, 0), (500, 90), (510, 0), (520, 0)]);
        let reopened = Doc::open(&path).unwrap();
        assert_eq!(reopened.annots.len(), 1, "exactly one copy");
        let d = lopdf::Document::load(&path).unwrap();
        let first = *d.get_pages().values().next().unwrap();
        assert_eq!(d.get_dictionary(first).unwrap().get(b"Annots").unwrap().as_array().unwrap().len(), 1);

        // At least one page always stays.
        assert!(!doc.delete_pages(&(0..5).collect()));
        assert_eq!(doc.layout.len(), 5);

        // Undo all the way back gives the original order.
        while doc.can_undo() {
            doc.undo();
        }
        doc.save_to(&path).unwrap();
        assert_eq!(page_widths(&path), [(500, 0), (510, 0), (520, 0), (530, 0), (540, 0)]);
    }

    #[test]
    fn extracting_pages_makes_a_new_pdf_with_their_annotations() {
        let path = tmp("extract_from.pdf");
        let out = tmp("extracted.pdf");
        std::fs::write(&path, five_page_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        let mut mark = all_kinds().remove(0);
        mark.page = 1;
        doc.exec(vec![Cmd::Add(mark.clone())]); // unsaved: still goes into the extract
        doc.rotate_pages(&[2].into_iter().collect(), false);
        doc.extract_pages(&[1, 2].into_iter().collect(), &out).unwrap();
        assert_eq!(page_widths(&out), [(510, 0), (520, 270)]);
        let extracted = Doc::open(&out).unwrap();
        assert_eq!(extracted.annots.iter().map(|a| (a.id.as_str(), a.page)).collect::<Vec<_>>(), [(mark.id.as_str(), 0)]);
        assert_eq!(extracted.foreign.iter().map(|f| f.nm.clone()).collect::<Vec<_>>(), [Some("theirs-1".to_string())]);
        assert!(doc.is_dirty(), "the original is untouched");
        assert_eq!(page_widths(&path).len(), 5);
    }

    #[test]
    fn page_edits_and_changes_from_another_window_combine_or_refuse() {
        let path = tmp("pages_two_windows.pdf");
        std::fs::write(&path, five_page_pdf()).unwrap();
        // A reorders pages and saves; B (opened before) adds an annotation on what
        // was page 2 and saves: it lands on that page, wherever it is now.
        let (mut a, mut b) = (Doc::open(&path).unwrap(), Doc::open(&path).unwrap());
        a.move_pages(&[1].into_iter().collect(), 5);
        a.save_to(&path).unwrap();
        let mut mark = all_kinds().remove(0);
        mark.page = 1;
        b.exec(vec![Cmd::Add(mark.clone())]);
        assert!(b.save_to(&path).unwrap().combined);
        assert_eq!(page_widths(&path), [(500, 0), (520, 0), (530, 0), (540, 0), (510, 0)]);
        assert_eq!(Doc::open(&path).unwrap().get(&mark.id).map(|m| m.page), Some(4));

        // Both rearranging pages: B refuses to combine (and suggests Save As).
        let (mut a, mut b) = (Doc::open(&path).unwrap(), Doc::open(&path).unwrap());
        a.rotate_pages(&[0].into_iter().collect(), true);
        a.save_to(&path).unwrap();
        b.delete_pages(&[1].into_iter().collect());
        let err = b.save_to(&path).unwrap_err().to_string();
        assert!(err.contains("Save As"), "{err}");
        assert_eq!(page_widths(&path)[0], (500, 90), "A's save is intact");
    }

    #[test]
    fn undo_and_redo_after_combining_with_reordered_pages_keep_the_right_page() {
        let path = tmp("redo_after_reorder.pdf");
        std::fs::write(&path, five_page_pdf()).unwrap();
        let (mut a, mut b) = (Doc::open(&path).unwrap(), Doc::open(&path).unwrap());
        a.move_pages(&[1].into_iter().collect(), 5); // page 2 (510 wide) goes last
        a.save_to(&path).unwrap();
        let mut mark = all_kinds().remove(0);
        mark.page = 1; // on the 510-wide page, as B sees it
        b.exec(vec![Cmd::Add(mark.clone())]);
        b.save_to(&path).unwrap();
        assert_eq!(b.get(&mark.id).map(|m| m.page), Some(4));
        b.undo();
        assert!(b.get(&mark.id).is_none());
        b.redo();
        assert_eq!(b.get(&mark.id).map(|m| m.page), Some(4), "redo puts it back on the same page");
        b.save_to(&path).unwrap();
        assert_eq!(Doc::open(&path).unwrap().get(&mark.id).map(|m| m.page), Some(4));

        // If this window's history has page edits of its own, it can't be carried
        // over a reorder from elsewhere: it's cleared (and the save says so).
        let (mut a, mut b) = (Doc::open(&path).unwrap(), Doc::open(&path).unwrap());
        a.move_pages(&[0].into_iter().collect(), 5);
        a.save_to(&path).unwrap();
        // Page edits in the history, pages unchanged in the end.
        b.rotate_pages(&[0].into_iter().collect(), true);
        b.rotate_pages(&[0].into_iter().collect(), false);
        let mut other = all_kinds().remove(1);
        other.page = 0;
        b.exec(vec![Cmd::Add(other)]);
        let outcome = b.save_to(&path).unwrap();
        assert!(outcome.combined && outcome.history_cleared);
        assert!(!b.can_undo() && !b.can_redo());
    }

    #[test]
    fn undoing_a_saved_page_deletion_restores_other_apps_annotations_fully() {
        let path = tmp("restore_page_foreign.pdf");
        std::fs::write(&path, five_page_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        let theirs = |d: &Doc| d.foreign.iter().position(|f| f.nm.as_deref() == Some("theirs-1"));
        assert_eq!(theirs(&doc).and_then(|i| doc.foreign_page(i)), Some(2));
        doc.delete_pages(&[2].into_iter().collect());
        doc.save_to(&path).unwrap();
        assert!(theirs(&doc).and_then(|i| doc.foreign_page(i)).is_none());
        doc.undo();
        // Back on its page, and known (selectable, listed), not just drawn.
        let i = theirs(&doc).expect("still known after the save");
        assert_eq!(doc.foreign_page(i), Some(2));
        assert!(doc.foreign[i].selectable && !doc.deleted_foreign.contains(&i));
        doc.save_to(&path).unwrap();
        let reopened = Doc::open(&path).unwrap();
        assert_eq!(theirs(&reopened).and_then(|i| reopened.foreign_page(i)), Some(2));
    }

    #[test]
    fn annotations_saved_by_inkpdf_are_still_ours() {
        use crate::annot::model::{LEGACY_NM_PREFIX, NM_PREFIX};
        use crate::pdf::write::{LEGACY_PRIVATE_KEY, PRIVATE_KEY};
        let path = tmp("legacy.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        doc.exec(vec![Cmd::Add(all_kinds().remove(0))]);
        doc.save_to(&path).unwrap();

        // Rewrite our annotation the way the app wrote it before the rename.
        let mut d = lopdf::Document::load(&path).unwrap();
        for obj in d.objects.values_mut() {
            let Ok(dict) = obj.as_dict_mut() else { continue };
            let Ok(nm) = dict.get(b"NM").and_then(|n| n.as_str()).map(|n| n.to_vec()) else { continue };
            let Some(rest) = nm.strip_prefix(NM_PREFIX.as_bytes()) else { continue };
            let legacy_nm = [LEGACY_NM_PREFIX.as_bytes(), rest].concat();
            dict.set("NM", lopdf::Object::String(legacy_nm, lopdf::StringFormat::Literal));
            let data = dict.remove(PRIVATE_KEY).unwrap();
            dict.set(LEGACY_PRIVATE_KEY, data);
        }
        d.save(&path).unwrap();

        let doc = Doc::open(&path).unwrap();
        assert_eq!(doc.annots.len(), 1, "old annotations are still editable");
        assert!(doc.annots[0].id.starts_with(LEGACY_NM_PREFIX));
        assert_eq!(doc.foreign.len(), 1);
    }

    #[test]
    fn annotations_edited_by_other_apps_are_left_alone() {
        let path = tmp("edited.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        doc.exec(vec![Cmd::Add(all_kinds().remove(0))]);
        doc.save_to(&path).unwrap();

        // Simulate another app moving our annotation (changing /Rect).
        let mut d = lopdf::Document::load(&path).unwrap();
        let ids: Vec<_> = d
            .objects
            .iter()
            .filter(|(_, o)| o.as_dict().is_ok_and(|d| d.get(b"Subtype").and_then(|s| s.as_name()).ok() == Some(b"Ink")))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let dict = d.get_dictionary_mut(id).unwrap();
            dict.set("Rect", vec![0.into(), 0.into(), 99.into(), 99.into()]);
        }
        d.save(&path).unwrap();

        let doc = Doc::open(&path).unwrap();
        assert!(doc.annots.is_empty());
        assert_eq!(doc.foreign.len(), 2);
    }
}

/// Manual check of rotated annotations: `OCHRE_OUT=dir cargo test rotated_sample -- --ignored`
/// writes `dir/rotated.pdf` to view in other apps.
#[cfg(test)]
mod rotated_sample {
    use super::*;
    use crate::annot::model::{Kind, Pt, ShapeKind, Style};

    #[test]
    #[ignore]
    fn rotated_sample() {
        let Ok(out) = std::env::var("OCHRE_OUT") else { return };
        let path = PathBuf::from(out).join("rotated.pdf");
        std::fs::write(&path, tests_pdf()).unwrap();
        let mut doc = Doc::open(&path).unwrap();
        let st = |c| Style { fill: Some([1.0, 0.86, 0.0]), fill_opacity: 0.4, ..Style::new(c, 3.0, 1.0) };
        let mut cmds = Vec::new();
        for (i, shape) in [ShapeKind::Rect, ShapeKind::Ellipse, ShapeKind::Check, ShapeKind::Cross].into_iter().enumerate() {
            let x = 80.0 + i as f32 * 130.0;
            let (a, b) = if matches!(shape, ShapeKind::Check | ShapeKind::Cross) {
                (Pt::new(x, 740.0), Pt::new(x + 60.0, 680.0))
            } else {
                (Pt::new(x, 680.0), Pt::new(x + 100.0, 740.0))
            };
            let up = Annotation::new(0, st([0.86, 0.15, 0.15]), Kind::Shape { shape, a, b });
            let turned = up.rotated(up.center(), 0.5).clone();
            let mut turned = turned;
            turned.map_points(|p| Pt::new(p.x, p.y - 150.0));
            cmds.push(Cmd::Add(up));
            cmds.push(Cmd::Add(turned));
        }
        let text = Annotation::new(
            0,
            Style::new([0.1, 0.35, 0.9], 18.0, 1.0),
            Kind::Text { origin: Pt::new(100.0, 400.0), right: Pt::new(1.0, 0.0), down: Pt::new(0.0, -1.0), text: "Rotated text".into() },
        );
        let text = text.rotated(text.center(), 0.5);
        cmds.push(Cmd::Add(text));
        doc.exec(cmds);
        doc.save_to(&path).unwrap();
    }

    fn tests_pdf() -> Vec<u8> {
        use lopdf::{Object, dictionary};
        let mut d = lopdf::Document::with_version("1.7");
        let pages = d.new_object_id();
        let page = d.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        d.objects.insert(pages, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }));
        let cat = d.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        d.trailer.set("Root", cat);
        let mut v = Vec::new();
        d.save_to(&mut v).unwrap();
        v
    }
}

/// Manual check: `OCHRE_SAMPLE=in.pdf OCHRE_OUT=dir cargo test visual -- --ignored`
/// writes `dir/annotated.pdf` plus PNGs of page 1–2 as pdfium and as our overlay
/// renderer draw them, to compare with other viewers (e.g. `pdftoppm`).
#[cfg(test)]
mod visual {
    use super::*;
    use crate::annot::geometry::PageGeom;
    use crate::annot::model::{Kind, MarkupKind, Pt, ShapeKind, Style};
    use crate::annot::smoothing::finish_stroke;
    use crate::pdf::worker::{Req, Resp, Worker};

    fn wait(w: &Worker) -> Resp {
        w.rx.recv_timeout(std::time::Duration::from_secs(10)).expect("worker timeout")
    }

    #[test]
    #[ignore]
    fn visual() {
        let (Ok(input), Ok(out)) = (std::env::var("OCHRE_SAMPLE"), std::env::var("OCHRE_OUT")) else {
            return;
        };
        let out = PathBuf::from(out);
        let path = out.join("annotated.pdf");
        std::fs::copy(&input, &path).unwrap();
        let mut doc = Doc::open(&path).unwrap();

        let worker = Worker::spawn(eframe::egui::Context::default()).unwrap();
        worker.send(Req::Load { generation: doc.generation, bytes: doc.bytes.clone(), hide: doc.hidden() });
        let Resp::Loaded { pages, .. } = wait(&worker) else { panic!("load failed") };
        doc.set_pages(pages.clone());

        // A wobbly hand-drawn loop, sampled like a mouse (integer pixels at 1x zoom).
        let raw: Vec<Pt> = (0..300)
            .map(|i| {
                let t = i as f32 / 300.0 * std::f32::consts::TAU * 1.2;
                Pt::new((150.0 + 60.0 * t.cos() + 20.0 * (3.0 * t).sin()).round(), (450.0 + 40.0 * t.sin()).round())
            })
            .collect();
        let pen = Style::new([0.1, 0.35, 0.9], 3.0, 1.0);
        let mut cmds = vec![
            Cmd::Add(Annotation::new(0, pen, Kind::Ink { curve: finish_stroke(&raw, 1.0), highlighter: false })),
            Cmd::Add(Annotation::new(
                0,
                Style::new([1.0, 0.86, 0.0], 14.0, 0.4),
                Kind::Ink {
                    curve: finish_stroke(
                        &(0..200).map(|i| Pt::new(80.0 + i as f32 * 1.5, 360.0 + (i as f32 * 0.05).sin() * 8.0)).collect::<Vec<_>>(),
                        1.0,
                    ),
                    highlighter: true,
                },
            )),
            Cmd::Add(Annotation::new(0, Style { fill: Some([1.0, 0.86, 0.0]), fill_opacity: 0.5, ..Style::new([0.86, 0.15, 0.15], 2.0, 1.0) }, Kind::Shape { shape: ShapeKind::Ellipse, a: Pt::new(300.0, 420.0), b: Pt::new(420.0, 490.0) })),
            Cmd::Add(Annotation::new(0, Style::new([0.15, 0.65, 0.25], 2.5, 1.0), Kind::Shape { shape: ShapeKind::Arrow, a: Pt::new(450.0, 400.0), b: Pt::new(540.0, 470.0) })),
            Cmd::Add(Annotation::new(0, Style { fill: Some([0.55, 0.25, 0.85]), fill_opacity: 0.2, ..Style::new([0.55, 0.25, 0.85], 2.0, 0.8) }, Kind::Shape { shape: ShapeKind::Rect, a: Pt::new(60.0, 250.0), b: Pt::new(250.0, 320.0) })),
            Cmd::Add(Annotation::new(
                0,
                Style::new([1.0, 0.86, 0.0], 1.0, 0.4),
                Kind::Markup { markup: MarkupKind::Highlight, quads: vec![[Pt::new(72.0, 733.0), Pt::new(300.0, 733.0), Pt::new(72.0, 718.0), Pt::new(300.0, 718.0)]] },
            )),
            Cmd::Add(Annotation::new(
                0,
                Style::new([0.86, 0.15, 0.15], 1.0, 1.0),
                Kind::Markup { markup: MarkupKind::StrikeOut, quads: vec![[Pt::new(72.0, 718.0), Pt::new(300.0, 718.0), Pt::new(72.0, 703.0), Pt::new(300.0, 703.0)]] },
            )),
        ];
        for (page, g) in [(0usize, pages[0]), (1, pages[1])] {
            let inv: PageGeom = g;
            let inv = inv.to_user();
            let origin = inv.apply(Pt::new(80.0, 120.0));
            cmds.push(Cmd::Add(Annotation::new(
                page,
                Style::new([0.08, 0.08, 0.1], 16.0, 1.0),
                Kind::Text {
                    origin,
                    right: inv.apply_vec(Pt::new(1.0, 0.0)),
                    down: inv.apply_vec(Pt::new(0.0, 1.0)),
                    text: format!("Typed note on page {} (rotation {})\nSecond line: café — “quotes”", page + 1, g.rotation),
                },
            )));
            for (i, shape) in [ShapeKind::Check, ShapeKind::Cross].into_iter().enumerate() {
                let x = 330.0 + i as f32 * 40.0;
                let (a, b) = (inv.apply(Pt::new(x, 180.0)), inv.apply(Pt::new(x + 24.0, 204.0)));
                let style = Style::new(if i == 0 { [0.15, 0.62, 0.25] } else { [0.86, 0.15, 0.15] }, 3.0, 1.0);
                cmds.push(Cmd::Add(Annotation::new(page, style, Kind::Shape { shape, a, b })));
            }
            let start = inv.apply(Pt::new(80.0, 200.0));
            let end = inv.apply(Pt::new(300.0, 200.0));
            cmds.push(Cmd::Add(Annotation::new(page, pen, Kind::Shape { shape: ShapeKind::Arrow, a: start, b: end })));
        }
        doc.exec(cmds);
        doc.save_to(&path).unwrap();
        assert_eq!(doc.foreign.len(), 3, "foreign annotations must survive");

        // Render pages with pdfium after reload (our annotations are hidden there,
        // so also composite our overlay renderer on top, like the app does).
        worker.send(Req::Load { generation: doc.generation, bytes: doc.bytes.clone(), hide: vec![] });
        let _ = wait(&worker);
        for page in 0..2 {
            worker.send(Req::Render { generation: doc.generation, page, scale: 1.5, thumb: false });
            let Resp::Rendered { image, .. } = wait(&worker) else { panic!() };
            let [w, h] = image.size;
            let mut pm = tiny_skia::Pixmap::new(w as u32, h as u32).unwrap();
            for (dst, src) in pm.pixels_mut().iter_mut().zip(&image.pixels) {
                *dst = tiny_skia::ColorU8::from_rgba(src.r(), src.g(), src.b(), 255).premultiply();
            }
            pm.save_png(out.join(format!("pdfium-p{}.png", page + 1))).unwrap();

            let g = doc.pages[page];
            let (dw, dh) = g.display_size();
            let mut ov = tiny_skia::Pixmap::new((dw * 1.5) as u32, (dh * 1.5) as u32).unwrap();
            ov.fill(tiny_skia::Color::WHITE);
            let t = g.to_display().then_scale_translate(1.5, 0.0, 0.0).to_skia();
            for a in doc.annots.iter().filter(|a| a.page == page) {
                crate::annot::raster::draw(&mut ov.as_mut(), a, t);
            }
            ov.save_png(out.join(format!("ours-p{}.png", page + 1))).unwrap();
        }
    }
}
