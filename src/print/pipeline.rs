//! Which source pages land on which side of which sheet, in what order and in
//! how many passes. Pure index arithmetic, no PDF code, so every ordering rule
//! is easy to test.
//!
//! Order of operations: page ranges -> n-up or booklet imposition (pages
//! onto sides) -> odd/even (counted on output sides, like CUPS `page-set`)
//! -> duplex padding -> copies/collate -> reverse.

use serde::{Deserialize, Serialize};

/// Source page indices printed on one side of a sheet, one per slot. Empty means blank.
pub type Side = Vec<usize>;

/// An empty slot on a side (booklets pad with these so every page keeps its place).
pub const BLANK: usize = usize::MAX;

pub const NUP_CHOICES: [usize; 6] = [1, 2, 4, 6, 9, 16];

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub cols: usize,
    pub rows: usize,
    /// Composed on a landscape canvas, then turned onto the portrait paper
    /// (what CUPS number-up does too).
    pub landscape: bool,
}

pub fn layout(nup: usize) -> Option<Layout> {
    let (cols, rows, landscape) = match nup {
        1 => (1, 1, false),
        2 => (2, 1, true),
        4 => (2, 2, false),
        6 => (3, 2, true),
        9 => (3, 3, false),
        16 => (4, 4, false),
        _ => return None,
    };
    Some(Layout { cols, rows, landscape })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageSet {
    #[default]
    All,
    Odd,
    Even,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    /// Each page goes on paper turned the same way as the page.
    #[default]
    Auto,
    Portrait,
    Landscape,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scaling {
    /// Grow or shrink each page to fill the printable area.
    #[default]
    Fit,
    /// Only shrink pages that don't fit; smaller ones print at actual size.
    Shrink,
    /// Actual size, centred; anything beyond the paper is cut off.
    Actual,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobOptions {
    /// "1-3,7,10-"; empty means all pages.
    pub page_ranges: String,
    pub page_set: PageSet,
    pub copies: u32,
    pub collate: bool,
    /// Face-up output trays need the last page printed first.
    pub reverse: bool,
    pub nup: usize,
    /// 0 / 90 / 180 / 270, applied to every output side.
    pub rotate: u32,
    pub orientation: Orientation,
    pub scaling: Scaling,
    /// Guided manual two-sided printing.
    pub duplex: bool,
    pub backs_reverse: bool,
    pub backs_rotate: bool,
    /// Two pages side by side on each side of the sheet, ordered so the
    /// folded stack reads as a booklet. Always printed two-sided.
    pub booklet: bool,
    /// Sheets folded together into one booklet; 0 puts every page in one.
    pub booklet_sheets: u32,
    /// Bound on the right, for right-to-left documents.
    pub booklet_rtl: bool,
    /// Extra space at the fold, in millimetres.
    pub gutter_mm: f64,
    /// With several documents in one job, start each on a fresh sheet.
    pub separate_docs: bool,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            page_ranges: String::new(),
            page_set: PageSet::All,
            copies: 1,
            collate: true,
            reverse: true,
            nup: 1,
            rotate: 0,
            orientation: Orientation::Auto,
            scaling: Scaling::Fit,
            duplex: false,
            backs_reverse: false,
            backs_rotate: false,
            booklet: false,
            booklet_sheets: 0,
            booklet_rtl: false,
            gutter_mm: 0.0,
            separate_docs: true,
        }
    }
}

impl JobOptions {
    /// Printed on both sides (booklets always are).
    pub fn two_sided(&self) -> bool {
        self.duplex || self.booklet
    }

    /// Source pages on each side of a sheet.
    pub fn pages_per_side(&self) -> usize {
        if self.booklet { 2 } else { self.nup }
    }
}

/// One trip of paper through the printer.
#[derive(Clone, Debug, PartialEq)]
pub struct Pass {
    pub sides: Vec<Side>,
    pub rotate180: bool,
    pub label: &'static str,
}

/// Parse "1-3, 7, 10-" into 0-based page indices. Pages past the end are dropped.
pub fn parse_ranges(spec: &str, n_pages: usize) -> Result<Vec<usize>, String> {
    let spec: String = spec.chars().filter(|c| !c.is_whitespace()).collect();
    if spec.is_empty() {
        return Ok((0..n_pages).collect());
    }
    let mut pages = Vec::new();
    for part in spec.split(',').filter(|p| !p.is_empty()) {
        let bad = || format!("Invalid page range: “{part}”");
        let (start, end) = match part.split_once('-') {
            Some((a, b)) => (
                if a.is_empty() { 1 } else { a.parse::<usize>().map_err(|_| bad())? },
                if b.is_empty() { n_pages } else { b.parse::<usize>().map_err(|_| bad())? },
            ),
            None => {
                let p = part.parse::<usize>().map_err(|_| bad())?;
                (p, p)
            }
        };
        if start < 1 || end < start {
            return Err(bad());
        }
        pages.extend((start - 1)..end.min(n_pages));
    }
    Ok(pages)
}

/// 0-based indices -> "1-3, 7". The inverse of `parse_ranges` for sorted input.
pub fn format_ranges(pages: &[usize]) -> String {
    let mut sorted = pages.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        parts.push(if start == end { format!("{}", start + 1) } else { format!("{}-{}", start + 1, end + 1) });
        i += 1;
    }
    parts.join(", ")
}

pub fn plan(n_pages: usize, opts: &JobOptions) -> Result<Vec<Pass>, String> {
    plan_docs(&[n_pages], opts)
}

/// Split `pages` (global indices) into runs that each stay within one document.
fn split_by_doc(pages: &[usize], doc_pages: &[usize]) -> Vec<Vec<usize>> {
    let mut starts = Vec::with_capacity(doc_pages.len());
    let mut total = 0;
    for n in doc_pages {
        starts.push(total);
        total += n;
    }
    let doc_of = |p: usize| starts.iter().rposition(|&s| s <= p).unwrap_or(0);
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current = None;
    for &p in pages {
        let d = doc_of(p);
        if current != Some(d) {
            groups.push(Vec::new());
            current = Some(d);
        }
        groups.last_mut().unwrap().push(p);
    }
    groups
}

/// Booklet imposition: pages in folding order, two per side, front then back
/// of each sheet. Every `sheets_per` sheets (0: all of them) make one booklet.
pub fn booklet_sides(pages: &[usize], sheets_per: usize, rtl: bool) -> Vec<Side> {
    let per = if sheets_per == 0 { pages.len().max(1) } else { sheets_per * 4 };
    let mut sides = Vec::new();
    for chunk in pages.chunks(per) {
        let mut p = chunk.to_vec();
        while p.len() % 4 != 0 {
            p.push(BLANK);
        }
        let n = p.len();
        for i in 0..n / 4 {
            // Outside of the sheet: the last page on the left, the first on the right.
            let front = [p[n - 1 - 2 * i], p[2 * i]];
            let back = [p[2 * i + 1], p[n - 2 - 2 * i]];
            for [left, right] in [front, back] {
                let side = if rtl { vec![right, left] } else { vec![left, right] };
                sides.push(if side.iter().all(|&s| s == BLANK) { Vec::new() } else { side });
            }
        }
    }
    sides
}

/// Like `plan`, for several documents printed as one job. `doc_pages` holds
/// each document's page count; page numbers run on across documents.
pub fn plan_docs(doc_pages: &[usize], opts: &JobOptions) -> Result<Vec<Pass>, String> {
    if !opts.booklet && layout(opts.nup).is_none() {
        return Err(format!("Unsupported pages per sheet: {}", opts.nup));
    }
    let n_pages: usize = doc_pages.iter().sum();
    let pages = parse_ranges(&opts.page_ranges, n_pages)?;
    let duplex = opts.two_sided();
    let mut sides: Vec<Side> = if opts.booklet {
        booklet_sides(&pages, opts.booklet_sheets as usize, opts.booklet_rtl)
    } else {
        let groups =
            if opts.separate_docs && doc_pages.len() > 1 { split_by_doc(&pages, doc_pages) } else { vec![pages] };
        let last = groups.len().saturating_sub(1);
        let mut sides = Vec::new();
        for (g, group) in groups.iter().enumerate() {
            sides.extend(group.chunks(opts.nup).map(|c| c.to_vec()));
            // The next document starts on the front of a new sheet.
            if duplex && g < last && sides.len() % 2 == 1 {
                sides.push(Vec::new());
            }
        }
        match opts.page_set {
            PageSet::All => sides,
            PageSet::Odd => sides.into_iter().step_by(2).collect(),
            PageSet::Even => sides.into_iter().skip(1).step_by(2).collect(),
        }
    };
    if sides.iter().all(|s| s.is_empty()) {
        return Err("No pages selected".into());
    }

    // A unit is what must stay together when making copies: one side, or a
    // front/back pair when printing two-sided.
    let per_unit = if duplex { 2 } else { 1 };
    if duplex && sides.len() % 2 == 1 {
        sides.push(Vec::new());
    }
    let units: Vec<Vec<Side>> = sides.chunks(per_unit).map(|c| c.to_vec()).collect();
    let copies = opts.copies.max(1) as usize;
    let units: Vec<Vec<Side>> = if opts.collate {
        (0..copies).flat_map(|_| units.iter().cloned()).collect()
    } else {
        units.iter().flat_map(|u| std::iter::repeat_n(u.clone(), copies)).collect()
    };

    if !duplex {
        let mut seq: Vec<Side> = units.into_iter().flatten().collect();
        if opts.reverse {
            seq.reverse();
        }
        return Ok(vec![Pass { sides: seq, rotate180: false, label: "Pages" }]);
    }
    let mut fronts: Vec<Side> = units.iter().map(|u| u[0].clone()).collect();
    let mut backs: Vec<Side> = units.iter().map(|u| u[1].clone()).collect();
    if opts.reverse {
        fronts.reverse();
    }
    if opts.backs_reverse {
        backs.reverse();
    }
    // The calibration test sets up flipping portrait paper on its long edge.
    // Booklet sheets are landscape on that paper and flip on their short
    // edge, which is the same turn plus half a turn.
    let rotate180 = opts.backs_rotate != opts.booklet;
    Ok(vec![Pass { sides: fronts, rotate180: false, label: "Front" }, Pass { sides: backs, rotate180, label: "Back" }])
}

/// The first real page anywhere in `sides` (for sizing blank sheets).
pub fn first_page<'a>(sides: impl IntoIterator<Item = &'a Side>) -> Option<usize> {
    sides.into_iter().flat_map(|s| s.iter()).copied().find(|&p| p != BLANK)
}

/// Every side of every pass, with the rotation it gets on paper.
pub fn flatten(passes: &[Pass], opts: &JobOptions) -> Vec<(Side, u32)> {
    passes
        .iter()
        .flat_map(|p| {
            let rot = (opts.rotate + if p.rotate180 { 180 } else { 0 }) % 360;
            p.sides.iter().map(move |s| (s.clone(), rot))
        })
        .collect()
}

pub fn describe_side(side: &Side) -> String {
    if side.contains(&BLANK) {
        let parts: Vec<String> =
            side.iter().map(|&p| if p == BLANK { "blank".into() } else { format!("p. {}", p + 1) }).collect();
        return parts.join(" + ");
    }
    let nums: Vec<usize> = side.iter().map(|i| i + 1).collect();
    match nums.as_slice() {
        [] => "blank".into(),
        [one] => format!("p. {one}"),
        [first, .., last] if nums.windows(2).all(|w| w[1] == w[0] + 1) => format!("pp. {first}–{last}"),
        _ => format!("pp. {}", nums.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ")),
    }
}

/// Thumbnail captions for the print preview, in print order:
/// "<sheet>\t<pages>", e.g. "Front 2\tpp. 3–4".
pub fn side_labels(passes: &[Pass], duplex: bool) -> Vec<String> {
    passes
        .iter()
        .flat_map(|p| {
            p.sides.iter().enumerate().map(move |(n, side)| {
                let prefix = if duplex { format!("{} {}", p.label, n + 1) } else { format!("{}", n + 1) };
                format!("{prefix}\t{}", describe_side(side))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> JobOptions {
        JobOptions { reverse: false, ..Default::default() }
    }

    fn order(passes: &[Pass], i: usize) -> Vec<Vec<usize>> {
        passes[i].sides.iter().map(|s| s.iter().map(|p| p + 1).collect()).collect()
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_ranges("", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_ranges("1-2, 5, 8-", 9).unwrap(), vec![0, 1, 4, 7, 8]);
        assert_eq!(parse_ranges("-2,4-99", 5).unwrap(), vec![0, 1, 3, 4]);
        for bad in ["x", "3-1", "0", "1-a"] {
            assert!(parse_ranges(bad, 5).is_err(), "{bad}");
        }
    }

    #[test]
    fn format_round_trip() {
        assert_eq!(format_ranges(&[0, 1, 2, 6, 8, 9]), "1-3, 7, 9-10");
        assert_eq!(format_ranges(&[]), "");
        assert_eq!(parse_ranges(&format_ranges(&[4, 0, 1]), 9).unwrap(), vec![0, 1, 4]);
    }

    #[test]
    fn plain_reverse_odd_even() {
        assert_eq!(order(&plan(3, &opts()).unwrap(), 0), vec![vec![1], vec![2], vec![3]]);
        let o = JobOptions { reverse: true, ..opts() };
        assert_eq!(order(&plan(3, &o).unwrap(), 0), vec![vec![3], vec![2], vec![1]]);
        let o = JobOptions { page_set: PageSet::Odd, ..opts() };
        assert_eq!(order(&plan(5, &o).unwrap(), 0), vec![vec![1], vec![3], vec![5]]);
        let o = JobOptions { page_set: PageSet::Even, ..opts() };
        assert_eq!(order(&plan(5, &o).unwrap(), 0), vec![vec![2], vec![4]]);
    }

    #[test]
    fn collate() {
        let o = JobOptions { copies: 2, ..opts() };
        assert_eq!(order(&plan(2, &o).unwrap(), 0), vec![vec![1], vec![2], vec![1], vec![2]]);
        let o = JobOptions { copies: 2, collate: false, ..opts() };
        assert_eq!(order(&plan(2, &o).unwrap(), 0), vec![vec![1], vec![1], vec![2], vec![2]]);
        let o = JobOptions { copies: 2, reverse: true, ..opts() };
        assert_eq!(order(&plan(2, &o).unwrap(), 0), vec![vec![2], vec![1], vec![2], vec![1]]);
    }

    #[test]
    fn nup_and_errors() {
        let o = JobOptions { nup: 2, ..opts() };
        assert_eq!(order(&plan(5, &o).unwrap(), 0), vec![vec![1, 2], vec![3, 4], vec![5]]);
        let o = JobOptions { page_ranges: "7-9".into(), ..opts() };
        assert!(plan(3, &o).is_err());
    }

    #[test]
    fn duplex() {
        let o = JobOptions { duplex: true, ..opts() };
        let p = plan(5, &o).unwrap();
        assert_eq!(order(&p, 0), vec![vec![1], vec![3], vec![5]]);
        assert_eq!(order(&p, 1), vec![vec![2], vec![4], vec![]]);

        let o = JobOptions { duplex: true, reverse: true, ..opts() };
        let p = plan(4, &o).unwrap();
        assert_eq!(order(&p, 0), vec![vec![3], vec![1]]);
        assert_eq!(order(&p, 1), vec![vec![2], vec![4]]);

        let o = JobOptions { duplex: true, backs_reverse: true, backs_rotate: true, ..opts() };
        let p = plan(4, &o).unwrap();
        assert_eq!(order(&p, 1), vec![vec![4], vec![2]]);
        assert!(p[1].rotate180);

        let o = JobOptions { duplex: true, copies: 2, ..opts() };
        let p = plan(3, &o).unwrap();
        assert_eq!(order(&p, 0), vec![vec![1], vec![3], vec![1], vec![3]]);
        assert_eq!(order(&p, 1), vec![vec![2], vec![], vec![2], vec![]]);

        let o = JobOptions { duplex: true, copies: 2, collate: false, ..opts() };
        let p = plan(4, &o).unwrap();
        assert_eq!(order(&p, 0), vec![vec![1], vec![1], vec![3], vec![3]]);
        assert_eq!(order(&p, 1), vec![vec![2], vec![2], vec![4], vec![4]]);
    }

    fn b(v: &[usize]) -> Vec<usize> {
        // 1-based for readability, 0 for a blank slot
        v.iter().map(|&p| if p == 0 { BLANK } else { p - 1 }).collect()
    }

    #[test]
    fn booklet_imposition() {
        let pages: Vec<usize> = (0..8).collect();
        assert_eq!(booklet_sides(&pages, 0, false), vec![b(&[8, 1]), b(&[2, 7]), b(&[6, 3]), b(&[4, 5])]);
        assert_eq!(booklet_sides(&pages, 0, true), vec![b(&[1, 8]), b(&[7, 2]), b(&[3, 6]), b(&[5, 4])]);
        // 5 pages pad to 8 with blanks at the end of the booklet.
        let five: Vec<usize> = (0..5).collect();
        assert_eq!(booklet_sides(&five, 0, false), vec![b(&[0, 1]), b(&[2, 0]), b(&[0, 3]), b(&[4, 5])]);
        // A single page: the back is completely blank.
        assert_eq!(booklet_sides(&[0], 0, false), vec![b(&[0, 1]), vec![]]);
        // Two booklets of one sheet each.
        assert_eq!(booklet_sides(&pages, 1, false), vec![b(&[4, 1]), b(&[2, 3]), b(&[8, 5]), b(&[6, 7])]);
    }

    #[test]
    fn booklet_passes() {
        let o = JobOptions { booklet: true, ..opts() };
        let p = plan(8, &o).unwrap();
        assert_eq!(p.len(), 2, "always two-sided");
        assert_eq!(order(&p, 0), vec![vec![8, 1], vec![6, 3]]);
        assert_eq!(order(&p, 1), vec![vec![2, 7], vec![4, 5]]);
        assert!(p[1].rotate180, "short-edge flip");
        let o = JobOptions { booklet: true, backs_rotate: true, reverse: true, ..opts() };
        let p = plan(8, &o).unwrap();
        assert!(!p[1].rotate180);
        assert_eq!(order(&p, 0), vec![vec![6, 3], vec![8, 1]]);
        // n-up and odd/even don't apply to booklets.
        let o = JobOptions { booklet: true, nup: 4, page_set: PageSet::Odd, ..opts() };
        assert_eq!(plan(4, &o).unwrap()[0].sides.len(), 1);
        // Copies keep each sheet's front and back together.
        let o = JobOptions { booklet: true, copies: 2, ..opts() };
        let p = plan(4, &o).unwrap();
        assert_eq!(order(&p, 0), vec![vec![4, 1], vec![4, 1]]);
        assert_eq!(order(&p, 1), vec![vec![2, 3], vec![2, 3]]);
    }

    #[test]
    fn several_documents() {
        // 3 + 2 pages, two-sided: the second document starts on a new sheet.
        let o = JobOptions { duplex: true, ..opts() };
        let p = plan_docs(&[3, 2], &o).unwrap();
        assert_eq!(order(&p, 0), vec![vec![1], vec![3], vec![4]]);
        assert_eq!(order(&p, 1), vec![vec![2], vec![], vec![5]]);
        // Without separation they run on.
        let o = JobOptions { duplex: true, separate_docs: false, ..opts() };
        let p = plan_docs(&[3, 2], &o).unwrap();
        assert_eq!(order(&p, 0), vec![vec![1], vec![3], vec![5]]);
        // 2-up: documents don't share a sheet.
        let o = JobOptions { nup: 2, ..opts() };
        assert_eq!(order(&plan_docs(&[3, 2], &o).unwrap(), 0), vec![vec![1, 2], vec![3], vec![4, 5]]);
        // Ranges count pages across documents.
        let o = JobOptions { nup: 2, page_ranges: "3-4".into(), ..opts() };
        assert_eq!(order(&plan_docs(&[3, 2], &o).unwrap(), 0), vec![vec![3], vec![4]]);
        assert_eq!(split_by_doc(&[4, 0, 1], &[3, 2]), vec![vec![4], vec![0, 1]]);
    }

    #[test]
    fn describe() {
        assert_eq!(describe_side(&b(&[0, 3])), "blank + p. 3");
        assert_eq!(describe_side(&vec![0, 1, 2, 3]), "pp. 1–4");
        assert_eq!(describe_side(&vec![]), "blank");
        assert_eq!(describe_side(&vec![0, 4]), "pp. 1, 5");
    }

    #[test]
    fn reads_settings_saved_by_earlier_versions() {
        let json = r#"{"backs_reverse":false,"backs_rotate":true,"collate":true,"duplex":false,
            "fit_to_page":false,"nup":2,"page_set":"odd","reverse":true,"rotate":0}"#;
        let o: JobOptions = serde_json::from_str(json).unwrap();
        assert_eq!((o.nup, o.page_set, o.backs_rotate, o.copies), (2, PageSet::Odd, true, 1));
    }
}
