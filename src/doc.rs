//! An open document: original bytes, our editable annotations, foreign
//! annotation bookkeeping and undo/redo.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::annot::geometry::PageGeom;
use crate::annot::model::{Annotation, Foreign};
use crate::pdf::annots::{Scan, save, scan};

#[derive(Clone, Debug)]
pub enum Cmd {
    Add(Annotation),
    Remove { annot: Annotation, index: usize },
    Modify { before: Annotation, after: Annotation },
    /// Hide an annotation made by other software (removed from the file on save).
    DeleteForeign(usize),
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
        let bytes = std::fs::read(path)?;
        let (scan, read_only) = match lopdf::Document::load_mem(&bytes) {
            Ok(d) if d.is_encrypted() => {
                (Scan::default(), Some("Encrypted PDFs can be viewed but not annotated yet.".into()))
            }
            Ok(d) => (scan(&d), None),
            Err(e) => (Scan::default(), Some(format!("Annotations can't be saved in this file: {e}"))),
        };
        Ok(Doc {
            path: path.to_path_buf(),
            bytes: Arc::new(bytes),
            generation: next_generation(),
            pages: Vec::new(),
            annots: scan.managed.clone(),
            foreign: scan.foreign.clone(),
            saved: scan.managed.clone(),
            deleted_foreign: BTreeSet::new(),
            read_only,
            page_rev: Vec::new(),
            scan,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// `/Annots` indices pdfium must not draw, per page.
    pub fn hidden(&self) -> Vec<Vec<usize>> {
        self.scan.hidden(&self.deleted_foreign)
    }

    pub fn set_pages(&mut self, pages: Vec<PageGeom>) {
        self.page_rev = vec![0; pages.len()];
        self.pages = pages;
    }

    fn on_page(list: &[Annotation], page: usize) -> Vec<&Annotation> {
        list.iter().filter(|a| a.page == page).collect()
    }

    pub fn dirty_pages(&self) -> BTreeSet<usize> {
        let mut pages: BTreeSet<usize> = self.deleted_foreign.iter().map(|&i| self.foreign[i].page).collect();
        let touched: BTreeSet<usize> = self.annots.iter().chain(&self.saved).map(|a| a.page).collect();
        for p in touched {
            if Self::on_page(&self.annots, p) != Self::on_page(&self.saved, p) {
                pages.insert(p);
            }
        }
        pages
    }

    pub fn is_dirty(&self) -> bool {
        !self.dirty_pages().is_empty()
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
                let (from, to) = if fwd { (before, after) } else { (after, before) };
                if let Some(i) = self.index_of(&from.id) {
                    self.annots[i] = to.clone();
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

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Saves to `out` (which may be the current path) and reopens from it.
    pub fn save_to(&mut self, out: &Path) -> Result<()> {
        if let Some(why) = &self.read_only {
            anyhow::bail!("{why}");
        }
        save(&self.bytes, &self.scan, &self.annots, &self.dirty_pages(), &self.deleted_foreign, out)?;
        let reopened = Doc::open(out)?;
        let pages = std::mem::take(&mut self.pages);
        *self = reopened;
        self.set_pages(pages);
        Ok(())
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
        assert!(doc.foreign.is_empty());
        assert_eq!(doc.annots.len(), created.len() - 1);
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
            worker.send(Req::Render { generation: doc.generation, page, scale: 1.5 });
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
