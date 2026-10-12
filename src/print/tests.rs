//! Printing end to end with pdfium: the preview against the printed PDF, and
//! annotations on paper. Skipped without pdfium (CI requires it).

use eframe::egui;

use crate::doc::next_generation;
use crate::pdf::worker::{Req, Resp, Worker};
use crate::print::geometry::SheetLayout;
use crate::print::pdfout::{self, Target};
use crate::print::pipeline::{JobOptions, Orientation, Scaling, first_page, flatten, plan};
use crate::print::render::SheetSpec;
use crate::print::testpdf::{A4, numbered, text_document};

fn wait(w: &Worker) -> Resp {
    w.rx.recv_timeout(std::time::Duration::from_secs(20)).expect("worker timeout")
}

/// Loads `bytes` into pdfium; returns its generation and page sizes as displayed.
fn load(w: &Worker, bytes: Vec<u8>) -> (u64, Vec<(f64, f64)>) {
    let generation = next_generation();
    w.send(Req::Load { generation, bytes: std::sync::Arc::new(bytes), hide: Vec::new() });
    let Resp::Loaded { pages, .. } = wait(w) else { panic!("load failed") };
    let sizes = pages.iter().map(|g| g.display_size()).map(|(w, h)| (w as f64, h as f64)).collect();
    (generation, sizes)
}

fn sheet(w: &Worker, generation: u64, spec: &SheetSpec, scale: f32) -> egui::ColorImage {
    w.send(Req::Sheet { generation, id: 0, spec: spec.clone(), scale });
    let Resp::Sheet { image, .. } = wait(w) else { panic!("sheet failed") };
    image
}

fn render(w: &Worker, generation: u64, page: usize) -> egui::ColorImage {
    w.send(Req::Render { generation, page, scale: 1.0, thumb: false, look: crate::look::PageLook::Normal });
    let Resp::Rendered { image, .. } = wait(w) else { panic!("render failed") };
    image
}

/// Average per-channel difference between two same-sized images (0..255).
fn diff(a: &egui::ColorImage, b: &egui::ColorImage) -> f64 {
    assert_eq!(a.size, b.size, "size mismatch");
    let total: f64 = a
        .pixels
        .iter()
        .zip(&b.pixels)
        .map(|(x, y)| x.to_array().iter().zip(y.to_array()).map(|(p, q)| (*p as f64 - q as f64).abs()).sum::<f64>())
        .sum();
    total / (a.pixels.len() * 4) as f64
}

fn bytes(doc: &mut lopdf::Document) -> Vec<u8> {
    pdfout::to_bytes(doc).unwrap()
}

/// The on-screen preview must match what the PDF writer produces, for n-up
/// layouts, rotated source pages, rotated sheets and booklets alike.
#[test]
fn preview_matches_printed_pdf() {
    let Ok(w) = Worker::spawn(egui::Context::default()) else { return };
    // Big text, so a misplaced or upside-down page shows.
    let pages: Vec<_> = (1..=7).map(|i| vec![(160, format!("P{i}")), (60, "TOP".to_string())]).collect();
    let mut src = text_document(&pages, A4);
    // Page 2 has a /Rotate, so rotated sources are covered too.
    let p2 = *src.get_pages().get(&2).unwrap();
    src.get_object_mut(p2).unwrap().as_dict_mut().unwrap().set("Rotate", 90);
    let src = pdfout::load_bytes(&bytes(&mut src)).unwrap();
    let (src_gen, sizes) = load(&w, bytes(&mut src.clone()));
    assert_eq!(sizes[1], (842.0, 595.0), "sizes are as displayed");

    let letter = Some((612.0, 792.0));
    let a4 = Some(A4);
    // (options, paper, margin)
    let cases = [
        (JobOptions { reverse: false, ..Default::default() }, None, 0.0),
        (JobOptions { nup: 2, ..Default::default() }, letter, 0.0),
        (JobOptions { nup: 4, rotate: 90, ..Default::default() }, letter, 9.0),
        (JobOptions { nup: 6, ..Default::default() }, letter, 9.0),
        (JobOptions { duplex: true, backs_rotate: true, nup: 2, ..Default::default() }, letter, 9.0),
        (JobOptions { rotate: 270, ..Default::default() }, None, 0.0),
        // 1-up on A4: page 2 is landscape, so automatic orientation turns its sheet.
        (JobOptions::default(), a4, 9.0),
        (JobOptions { orientation: Orientation::Portrait, ..Default::default() }, a4, 9.0),
        (JobOptions { orientation: Orientation::Landscape, scaling: Scaling::Shrink, ..Default::default() }, a4, 9.0),
        (JobOptions { scaling: Scaling::Actual, rotate: 90, ..Default::default() }, letter, 9.0),
        (JobOptions { duplex: true, backs_rotate: true, ..Default::default() }, a4, 9.0),
        // Booklets: blank slots, the gutter, and backs turned for a short-edge flip.
        (JobOptions { booklet: true, gutter_mm: 8.0, ..Default::default() }, a4, 9.0),
        (
            JobOptions {
                booklet: true,
                booklet_rtl: true,
                booklet_sheets: 1,
                backs_rotate: true,
                ..Default::default()
            },
            letter,
            0.0,
        ),
    ];
    for (n, (opts, paper, margin)) in cases.iter().enumerate() {
        let layout = SheetLayout::new(opts, *paper, *margin);
        let passes = plan(sizes.len(), opts).unwrap();
        let sides = flatten(&passes, opts);
        let mut out = pdfout::build(&src, &sides, &layout, Target::Printer).unwrap();
        let (out_gen, printed) = load(&w, bytes(&mut out));
        assert_eq!(printed.len(), sides.len());
        let fallback = sizes[first_page(sides.iter().map(|(s, _)| s)).unwrap_or(0)];
        for (i, (side, rot)) in sides.iter().enumerate() {
            let spec = SheetSpec { side: side.clone(), rotation: *rot, layout, fallback, tone: Default::default() };
            let (pw, ph) = spec.displayed_size(&sizes);
            let k = (400.0 / pw.max(ph)) as f32;
            let preview = sheet(&w, src_gen, &spec, k);
            // Landscape sheets are printed on portrait paper, turned; turn them back to compare.
            let (qw, qh) = printed[i];
            let turn_back = if (pw > ph) != (qw > qh) { 90 } else { 0 };
            let pdf_spec = SheetSpec { rotation: turn_back, ..SheetSpec::page(i) };
            let actual = sheet(&w, out_gen, &pdf_spec, k);
            let d = diff(&preview, &actual);
            // A deliberately wrong render (upside down) must score far worse,
            // otherwise this comparison couldn't catch a misplaced page.
            if !side.is_empty() {
                let wrong = sheet(&w, out_gen, &SheetSpec { rotation: (turn_back + 180) % 360, ..pdf_spec }, k);
                let dw = diff(&preview, &wrong);
                assert!(dw > 0.1, "case {n} sheet {i}: upside down only differs by {dw:.4}");
            }
            assert!(d < 0.01, "case {n} sheet {i} ({side:?} rot {rot}): preview differs from PDF by {d:.4}");
        }
        if let Some(paper) = paper {
            // Whatever the orientation, the printer always gets portrait paper-sized pages.
            assert!(printed.iter().all(|s| *s == *paper), "case {n}: {printed:?}");
        }
        w.send(Req::Close { generation: out_gen });
    }
}

/// Annotations print the way pdfium shows them, including on n-up sheets
/// (which only take the pages' own drawing).
#[test]
fn annotations_are_printed() {
    use crate::annot::model::{Annotation, Kind, Pt, Style};
    use crate::doc::{Cmd, Doc};

    let Ok(w) = Worker::spawn(egui::Context::default()) else { return };
    let dir = std::env::temp_dir().join(format!("ochre-print-annots-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("doc.pdf");
    std::fs::write(&path, bytes(&mut numbered(2, A4))).unwrap();
    let mut doc = Doc::open(&path).unwrap();
    let line = |y: f32| vec![Pt::new(100.0, y), Pt::new(250.0, y), Pt::new(350.0, y), Pt::new(500.0, y)];
    doc.exec(vec![
        Cmd::Add(Annotation::new(
            0,
            Style::new([1.0, 0.86, 0.0], 30.0, 0.4),
            Kind::Ink { curve: line(500.0), highlighter: true },
        )),
        Cmd::Add(Annotation::new(
            0,
            Style::new([0.1, 0.35, 0.9], 6.0, 1.0),
            Kind::Ink { curve: line(300.0), highlighter: false },
        )),
    ]);
    let saved = doc.current_bytes().unwrap();

    // pdfium drawing the annotations vs. the flattened page.
    let (orig_gen, _) = load(&w, saved.clone());
    let mut flat = pdfout::load_bytes(&saved).unwrap();
    let page = *flat.get_pages().get(&1).unwrap();
    assert!(flat.get_dictionary(page).unwrap().get(b"Annots").is_err(), "annotations are now page content");
    let (flat_gen, _) = load(&w, bytes(&mut flat));
    let (shown, printed) = (render(&w, orig_gen, 0), render(&w, flat_gen, 0));
    let d = diff(&shown, &printed);
    assert!(d < 0.05, "flattened page differs from the annotated one by {d:.4}");

    // On a 2-up sheet the pen stroke is there: blue pixels.
    let opts = JobOptions { nup: 2, reverse: false, ..Default::default() };
    let passes = plan(2, &opts).unwrap();
    let mut out =
        pdfout::build(&flat, &flatten(&passes, &opts), &SheetLayout::new(&opts, Some(A4), 0.0), Target::File).unwrap();
    let (out_gen, _) = load(&w, bytes(&mut out));
    let img = render(&w, out_gen, 0);
    let blue = img.pixels.iter().filter(|p| p.b() > 150 && p.r() < 80 && p.g() < 130).count();
    assert!(blue > 100, "pen stroke missing from the printed sheet ({blue} blue pixels)");
    std::fs::remove_dir_all(&dir).ok();
}

/// Sends a small print-ready job to the CUPS queue named in `OCHRE_TEST_PRINTER`
/// (use a virtual printer) and waits for CUPS to finish it.
#[test]
#[ignore]
fn sends_a_job_to_cups() {
    use crate::print::cups;
    let Ok(printer) = std::env::var("OCHRE_TEST_PRINTER") else { return };
    let dir = std::env::temp_dir().join(format!("ochre-print-cups-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = pdfout::load_bytes(&bytes(&mut numbered(3, A4))).unwrap();
    let opts = JobOptions { nup: 2, ..Default::default() };
    let passes = plan(3, &opts).unwrap();
    let layout = SheetLayout::new(&opts, Some(A4), 9.0);
    let files = pdfout::write_passes(&src, &passes, &opts, &layout, &dir, "job").unwrap();
    // Driver options travel with the job (a virtual printer just records them).
    let options = [("BRBrightness".to_string(), "10".to_string()), ("BRRed".to_string(), "-5".to_string())];
    let id = cups::print_file(&printer, &files[0], "Ochre test", &options, Some(cups::FORMAT_PRINT_READY_PDF)).unwrap();
    let mut state = cups::job_state(id);
    for _ in 0..120 {
        if state.is_final() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        state = cups::job_state(id);
    }
    assert_eq!(state, cups::JobState::Done, "job {id}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A grayscale sheet has no color left in it.
#[test]
fn grayscale_sheets_are_gray() {
    let Ok(w) = Worker::spawn(egui::Context::default()) else { return };
    // A page with a red square.
    let mut doc = numbered(1, A4);
    let page = *doc.get_pages().get(&1).unwrap();
    let red = doc.add_object(lopdf::Stream::new(lopdf::Dictionary::new(), b"1 0 0 rg 100 100 300 300 re f".to_vec()));
    let contents = doc.get_dictionary(page).unwrap().get(b"Contents").unwrap().clone();
    doc.get_dictionary_mut(page).unwrap().set("Contents", vec![contents, red.into()]);
    let (generation, _) = load(&w, bytes(&mut doc));
    let colored = |img: &egui::ColorImage| img.pixels.iter().filter(|p| p.r() != p.g() || p.g() != p.b()).count();
    assert!(colored(&sheet(&w, generation, &SheetSpec::page(0), 0.5)) > 1000);
    let gray = sheet(
        &w,
        generation,
        &SheetSpec { tone: crate::print::ppd::Tone { gray: true, ..Default::default() }, ..SheetSpec::page(0) },
        0.5,
    );
    assert_eq!(colored(&gray), 0);
    // Red prints as a mid gray, not white.
    // The square's middle is at (250, 250) pt from the bottom left: (125, 296) px at half size.
    let middle = gray.pixels[(842 - 250) / 2 * gray.size[0] + 125];
    assert!(middle.r() < 200, "{middle:?}");
}

/// A form like many made by other apps: filled-in text fields with no
/// appearance, which viewers draw themselves (`need_appearances` also says so
/// in the form). Three pages, each with its own field.
fn form_pdf(need_appearances: bool) -> Vec<u8> {
    use lopdf::{Object, StringFormat, dictionary};
    let mut doc = numbered(3, A4);
    let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let mut fields = Vec::new();
    for (n, page) in doc.get_pages().into_iter() {
        let value = format!("SECRET-{n} WWWWWWWWWWWW");
        let field = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "F" => 4,
            "T" => Object::string_literal(format!("field{n}")),
            "V" => Object::String(value.into_bytes(), StringFormat::Literal),
            "DA" => Object::string_literal("/Helv 24 Tf 0 g"),
            "Rect" => vec![100.into(), 400.into(), 500.into(), 440.into()],
            "P" => page,
        });
        doc.get_dictionary_mut(page).unwrap().set("Annots", vec![field.into()]);
        fields.push(Object::Reference(field));
    }
    let form = doc.add_object(dictionary! {
        "Fields" => fields, "NeedAppearances" => need_appearances,
        "DA" => Object::string_literal("/Helv 0 Tf 0 g"),
        "DR" => dictionary! { "Font" => dictionary! { "Helv" => font } },
    });
    doc.catalog_mut().unwrap().set("AcroForm", form);
    bytes(&mut doc)
}

#[test]
fn filled_form_fields_print() {
    let ctx = egui::Context::default();
    let Ok(w) = Worker::spawn(ctx.clone()) else { return };
    // Dark pixels where the field's text is, between y 400 and 440 (from the bottom).
    let ink = |img: &egui::ColorImage| {
        let band = &img.pixels[(842 - 440) * img.size[0]..(842 - 400) * img.size[0]];
        band.iter().filter(|p| p.r() < 100).count()
    };
    for need in [true, false] {
        let source = std::sync::Arc::new(form_pdf(need));
        // The reader shows the value.
        let (shown, _) = load(&w, source.to_vec());
        assert!(ink(&render(&w, shown, 0)) > 200, "NeedAppearances {need}: not shown");
        assert!(pdfout::needs_appearances(&pdfout::parse(&source).unwrap()), "NeedAppearances {need}");
        w.send(Req::Appearances { generation: next_generation(), bytes: source.clone() });
        let Resp::Appearances { result, .. } = wait(&w) else { panic!() };
        let mut doc = pdfout::parse(&result.unwrap()).unwrap();
        if !need {
            assert!(!pdfout::needs_appearances(&doc), "every field has an appearance now");
        }
        pdfout::flatten_annotations(&mut doc);
        let (printed, _) = load(&w, bytes(&mut doc));
        let n = ink(&render(&w, printed, 0));
        assert!(n > 200, "NeedAppearances {need}: the field's value isn't printed ({n} dark pixels)");
    }
    // Forms whose fields all have appearances, and documents without forms, don't need drawing.
    assert!(!pdfout::needs_appearances(&pdfout::parse(&bytes(&mut numbered(1, A4))).unwrap()));
}

#[test]
fn printing_some_pages_of_a_form_leaves_the_others_out() {
    let mut doc = pdfout::parse(&form_pdf(true)).unwrap();
    pdfout::flatten_annotations(&mut doc);
    let opts = JobOptions { page_ranges: "1".into(), reverse: false, ..Default::default() };
    let passes = plan(3, &opts).unwrap();
    let mut out =
        pdfout::build(&doc, &flatten(&passes, &opts), &SheetLayout::new(&opts, None, 0.0), Target::File).unwrap();
    let saved = bytes(&mut out);
    assert_eq!(lopdf::Document::load_mem(&saved).unwrap().get_pages().len(), 1);
    // Field values are kept as plain strings; the form must not keep the other pages' alive.
    let text = String::from_utf8_lossy(&saved);
    assert!(!text.contains("SECRET-2") && !text.contains("SECRET-3"), "other pages' fields are gone");
}

/// A crop box reaching past the media box: what shows is where they overlap.
#[test]
fn an_oversized_crop_box_is_clipped_to_the_paper() {
    let Ok(w) = Worker::spawn(egui::Context::default()) else { return };
    let pages: Vec<_> = vec![vec![(160, "P1".to_string()), (60, "TOP".to_string())]];
    let mut src = text_document(&pages, A4);
    let page = *src.get_pages().get(&1).unwrap();
    let crop: Vec<lopdf::Object> = vec![(-100).into(), (-50).into(), 700.into(), 900.into()];
    src.get_dictionary_mut(page).unwrap().set("CropBox", crop);
    let src = pdfout::load_bytes(&bytes(&mut src)).unwrap();
    let (src_gen, sizes) = load(&w, bytes(&mut src.clone()));
    assert_eq!(sizes[0], A4);
    let opts = JobOptions { reverse: false, ..Default::default() };
    let layout = SheetLayout::new(&opts, Some((612.0, 792.0)), 9.0);
    let sides = flatten(&plan(1, &opts).unwrap(), &opts);
    let mut out = pdfout::build(&src, &sides, &layout, Target::Printer).unwrap();
    let (out_gen, _) = load(&w, bytes(&mut out));
    let spec = SheetSpec { side: sides[0].0.clone(), rotation: 0, layout, fallback: A4, tone: Default::default() };
    let d = diff(&sheet(&w, src_gen, &spec, 0.5), &sheet(&w, out_gen, &SheetSpec::page(0), 0.5));
    assert!(d < 0.01, "preview differs from the PDF by {d:.4}");
}
