//! Simple text-only PDFs: the two-sided calibration sheets, and test documents.

use lopdf::{Dictionary, Document, Object, Stream, dictionary};

pub const A4: (f64, f64) = (595.0, 842.0);

fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\\' | '(' | ')' => format!("\\{c}"),
            c if c.is_ascii() => c.to_string(),
            _ => "?".into(),
        })
        .collect()
}

/// Build a document where each page is a list of (font size, text) lines flowing down from the top.
pub fn text_document(pages: &[Vec<(u32, String)>], size: (f64, f64)) -> Document {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let mut kids = Vec::new();
    for lines in pages {
        let mut y = size.1 - 60.0;
        let mut ops = String::new();
        for (pt, text) in lines {
            y -= *pt as f64 * 1.4;
            ops += &format!("BT /F1 {pt} Tf 50 {y:.1} Td ({}) Tj ET\n", escape(text));
        }
        let content_id = doc.add_object(Stream::new(Dictionary::new(), ops.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), Object::Real(size.0 as f32), Object::Real(size.1 as f32)],
            "Resources" => resources_id,
            "Contents" => content_id,
        });
        kids.push(page_id.into());
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids, "Count" => count,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    doc
}

#[cfg(test)]
pub fn numbered(n: usize, size: (f64, f64)) -> Document {
    let pages: Vec<_> = (1..=n).map(|i| vec![(36, format!("P{i}"))]).collect();
    text_document(&pages, size)
}

/// Four sides (two sheets) that show whether back sides line up.
pub fn duplex_test(size: (f64, f64)) -> Document {
    let mut pages = Vec::new();
    for sheet in 1..=2 {
        pages.push(vec![
            (14, "^^^ TOP OF PAGE ^^^".into()),
            (40, format!("SHEET {sheet}")),
            (40, "FRONT".into()),
            (12, String::new()),
            (12, "Two-sided calibration test from Ochre.".into()),
            (12, format!("Turn this sheet over: the back must say SHEET {sheet} - BACK,")),
            (12, "and its TOP OF PAGE must be on the same edge as this one.".into()),
        ]);
        pages.push(vec![
            (14, "^^^ TOP OF PAGE ^^^".into()),
            (40, format!("SHEET {sheet}")),
            (40, "BACK".into()),
            (12, String::new()),
            (12, format!("If this side is not behind SHEET {sheet} - FRONT, switch")),
            (12, "    'Reverse order of back sides'".into()),
            (12, "If this side is upside down compared to the front, switch".into()),
            (12, "    'Rotate back sides 180 degrees'".into()),
            (12, "Then print the test again until both sheets are right.".into()),
        ]);
    }
    text_document(&pages, size)
}
