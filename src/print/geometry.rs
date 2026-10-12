//! Where each page goes on a sheet. Shared by the PDF writer (lopdf) and the
//! on-screen preview (poppler/cairo) so the two can never disagree.
//!
//! Matrices use the PDF convention `[a b c d e f]`:
//! `x' = a·x + c·y + e`, `y' = b·x + d·y + f`, with y pointing up.

use crate::print::pipeline::{Orientation, Scaling, layout};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix(pub [f64; 6]);

impl Matrix {
    pub const IDENTITY: Matrix = Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    pub fn translate(x: f64, y: f64) -> Matrix {
        Matrix([1.0, 0.0, 0.0, 1.0, x, y])
    }

    pub fn scale(s: f64) -> Matrix {
        Matrix([s, 0.0, 0.0, s, 0.0, 0.0])
    }

    /// Apply `self` first, then `next`.
    pub fn then(self, next: Matrix) -> Matrix {
        let [a, b, c, d, e, f] = self.0;
        let [na, nb, nc, nd, ne, nf] = next.0;
        Matrix([
            na * a + nc * b,
            nb * a + nd * b,
            na * c + nc * d,
            nb * c + nd * d,
            na * e + nc * f + ne,
            nb * e + nd * f + nf,
        ])
    }

    pub fn apply(self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }

    /// Flip between y-up and y-down for a box of the given height.
    pub fn flip_y(height: f64) -> Matrix {
        Matrix([1.0, 0.0, 0.0, -1.0, 0.0, height])
    }
}

/// Size of a `w`×`h` box once shown with a clockwise rotation.
pub fn rotated_size(w: f64, h: f64, rotation: u32) -> (f64, f64) {
    if rotation % 180 == 90 { (h, w) } else { (w, h) }
}

/// Unrotated page space (0..w, 0..h, y up) -> the page as displayed with a
/// clockwise `/Rotate` of `rotation` degrees (still y up, origin bottom-left).
pub fn display_rotation(w: f64, h: f64, rotation: u32) -> Matrix {
    match rotation % 360 {
        90 => Matrix([0.0, -1.0, 1.0, 0.0, 0.0, w]),
        180 => Matrix([-1.0, 0.0, 0.0, -1.0, w, h]),
        270 => Matrix([0.0, 1.0, -1.0, 0.0, h, 0.0]),
        _ => Matrix::IDENTITY,
    }
}

/// Points per millimetre.
pub const PT_PER_MM: f64 = 72.0 / 25.4;

/// A displayed page of size `page` (y up) -> its slot on an n-up sheet of size `paper` (y up),
/// keeping `margin` points clear around the edge and `gap` points between columns.
pub fn place(slot: usize, nup: usize, paper: (f64, f64), margin: f64, gap: f64, page: (f64, f64)) -> Matrix {
    let l = layout(nup).expect("valid n-up");
    let (pw, ph) = paper;
    // The usable area of the portrait sheet.
    let (aw, ah) = ((pw - 2.0 * margin).max(1.0), (ph - 2.0 * margin).max(1.0));
    let (cw, ch) = if l.landscape { (ah, aw) } else { (aw, ah) };
    let gap = gap.clamp(0.0, cw / 2.0);
    let (cell_w, cell_h) = ((cw - gap * (l.cols - 1) as f64) / l.cols as f64, ch / l.rows as f64);
    let (w, h) = (page.0.max(1.0), page.1.max(1.0));
    let s = (cell_w / w).min(cell_h / h);
    let (col, row) = ((slot % l.cols) as f64, (slot / l.cols) as f64);
    let x = col * (cell_w + gap) + (cell_w - w * s) / 2.0;
    let y = ch - (row + 1.0) * cell_h + (cell_h - h * s) / 2.0;
    let m = Matrix::scale(s).then(Matrix::translate(x, y));
    let to_sheet = if l.landscape {
        // (x, y) on the landscape canvas -> (aw - y, x) in the usable area,
        // so the canvas's top edge runs along the sheet's left edge.
        Matrix([0.0, 1.0, -1.0, 0.0, aw, 0.0])
    } else {
        Matrix::IDENTITY
    };
    m.then(to_sheet).then(Matrix::translate(margin, margin))
}

/// How pages are laid out on paper. Shared by the preview and the PDF writer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SheetLayout {
    pub nup: usize,
    /// Portrait paper size in points; None when the printer doesn't say
    /// (then pages go through at their own size, untouched).
    pub paper: Option<(f64, f64)>,
    /// Unprintable border around the paper, in points.
    pub margin: f64,
    pub orientation: Orientation,
    pub scaling: Scaling,
    /// Space between the two halves of a booklet sheet, in points.
    pub gutter: f64,
}

impl SheetLayout {
    pub fn new(job: &crate::print::pipeline::JobOptions, paper: Option<(f64, f64)>, margin: f64) -> SheetLayout {
        SheetLayout {
            nup: job.pages_per_side(),
            paper,
            margin,
            orientation: job.orientation,
            scaling: job.scaling,
            gutter: if job.booklet { job.gutter_mm.max(0.0) * PT_PER_MM } else { 0.0 },
        }
    }

    /// No paper to lay out on: 1-up pages print exactly as they are.
    pub fn passthrough(&self) -> bool {
        self.nup == 1 && self.paper.is_none()
    }

    /// Sheet size (before any extra rotation) for a side whose first page
    /// is displayed at `page`; `fallback` is used when nothing else is known.
    pub fn sheet_size(&self, page: Option<(f64, f64)>, fallback: (f64, f64)) -> (f64, f64) {
        let Some((pw, ph)) = self.paper else {
            return page.unwrap_or(fallback);
        };
        if self.nup > 1 {
            return (pw, ph); // n-up layouts turn themselves onto portrait paper
        }
        let landscape = match self.orientation {
            Orientation::Portrait => false,
            Orientation::Landscape => true,
            Orientation::Auto => page.is_some_and(|(w, h)| w > h),
        };
        if landscape { (ph, pw) } else { (pw, ph) }
    }

    /// Where a displayed page (y up) goes on `sheet` when it fills slot `slot`.
    pub fn place(&self, slot: usize, sheet: (f64, f64), page: (f64, f64)) -> Matrix {
        if self.passthrough() {
            return Matrix::IDENTITY;
        }
        if self.nup > 1 {
            return place(slot, self.nup, sheet, self.margin, self.gutter, page);
        }
        let (sw, sh) = sheet;
        let (w, h) = (page.0.max(1.0), page.1.max(1.0));
        let fit = ((sw - 2.0 * self.margin) / w).min((sh - 2.0 * self.margin) / h).max(0.01);
        let s = match self.scaling {
            Scaling::Fit => fit,
            Scaling::Shrink => fit.min(1.0),
            Scaling::Actual => 1.0,
        };
        Matrix::scale(s).then(Matrix::translate((sw - w * s) / 2.0, (sh - h * s) / 2.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }

    #[test]
    fn rotation_maps_corners() {
        // top-left of an unrotated 100x200 page
        let tl = (0.0, 200.0);
        assert!(close(display_rotation(100.0, 200.0, 90).apply(tl.0, tl.1), (200.0, 100.0))); // -> top-right
        assert!(close(display_rotation(100.0, 200.0, 180).apply(tl.0, tl.1), (100.0, 0.0))); // -> bottom-right
        assert!(close(display_rotation(100.0, 200.0, 270).apply(tl.0, tl.1), (0.0, 0.0))); // -> bottom-left
    }

    #[test]
    fn two_up_turns_pages_onto_portrait_paper() {
        let paper = (612.0, 792.0);
        let page = (612.0, 792.0);
        // Page 1 fills the left half of the landscape canvas; on the portrait
        // sheet that's the bottom half, with its top edge pointing left.
        let m = place(0, 2, paper, 0.0, 0.0, page);
        let (x, y) = m.apply(page.0 / 2.0, page.1 / 2.0);
        assert!(x > 0.0 && x < 612.0 && y > 0.0 && y < 396.0, "{x},{y}");
        let (_, top_y) = m.apply(0.0, page.1);
        let (_, bottom_y) = m.apply(0.0, 0.0);
        assert!((top_y - bottom_y).abs() < 1e-6, "page top should run along the sheet's y axis");
    }

    #[test]
    fn four_up_grid() {
        let m = place(3, 4, (595.0, 842.0), 0.0, 0.0, (595.0, 842.0));
        let (x, y) = m.apply(10.0, 10.0);
        assert!(x > 297.0 && y < 421.0, "slot 4 is bottom-right: {x},{y}");
    }

    fn a4(orientation: Orientation, scaling: Scaling) -> SheetLayout {
        SheetLayout { nup: 1, paper: Some((595.0, 842.0)), margin: 9.0, orientation, scaling, gutter: 0.0 }
    }

    #[test]
    fn automatic_orientation_follows_the_page() {
        let l = a4(Orientation::Auto, Scaling::Fit);
        assert_eq!(l.sheet_size(Some((842.0, 595.0)), (0.0, 0.0)), (842.0, 595.0));
        assert_eq!(l.sheet_size(Some((595.0, 842.0)), (0.0, 0.0)), (595.0, 842.0));
        let l = a4(Orientation::Portrait, Scaling::Fit);
        assert_eq!(l.sheet_size(Some((842.0, 595.0)), (0.0, 0.0)), (595.0, 842.0));
        let l = a4(Orientation::Landscape, Scaling::Fit);
        assert_eq!(l.sheet_size(Some((595.0, 842.0)), (0.0, 0.0)), (842.0, 595.0));
    }

    #[test]
    fn fit_keeps_inside_the_printable_area_and_centres() {
        // A big landscape scan (1000x700) on landscape A4 with 9pt margins.
        let l = a4(Orientation::Auto, Scaling::Fit);
        let sheet = l.sheet_size(Some((1000.0, 700.0)), (0.0, 0.0));
        let m = l.place(0, sheet, (1000.0, 700.0));
        let (x0, y0) = m.apply(0.0, 0.0);
        let (x1, y1) = m.apply(1000.0, 700.0);
        assert!(
            x0 >= 9.0 - 1e-6 && y0 >= 9.0 - 1e-6 && x1 <= 833.0 + 1e-6 && y1 <= 586.0 + 1e-6,
            "{x0},{y0} {x1},{y1}"
        );
        assert!(((x0 + x1) / 2.0 - 421.0).abs() < 1e-6 && ((y0 + y1) / 2.0 - 297.5).abs() < 1e-6, "centred");
        // Shrink leaves small pages alone; actual size never scales.
        let small = a4(Orientation::Auto, Scaling::Shrink).place(0, (595.0, 842.0), (300.0, 400.0));
        assert_eq!(small.apply(300.0, 400.0).0 - small.apply(0.0, 0.0).0, 300.0);
        let actual = a4(Orientation::Auto, Scaling::Actual).place(0, (842.0, 595.0), (1000.0, 700.0));
        assert_eq!(actual.apply(1000.0, 0.0).0 - actual.apply(0.0, 0.0).0, 1000.0);
    }

    #[test]
    fn booklet_gutter_keeps_pages_off_the_fold() {
        // Landscape canvas turned onto portrait A4: the fold runs across the paper at y = 421.
        let gap = 20.0;
        let left = place(0, 2, (595.0, 842.0), 0.0, gap, (595.0, 842.0));
        let right = place(1, 2, (595.0, 842.0), 0.0, gap, (595.0, 842.0));
        let ys = |m: Matrix| {
            let (_, a) = m.apply(0.0, 0.0);
            let (_, b) = m.apply(595.0, 842.0);
            (a.min(b), a.max(b))
        };
        let (l0, l1) = ys(left);
        let (r0, r1) = ys(right);
        assert!(
            l0 >= 421.0 + gap / 2.0 - 1e-6 || l1 <= 421.0 - gap / 2.0 + 1e-6,
            "left half clear of the fold: {l0}..{l1}"
        );
        assert!(
            r0 >= 421.0 + gap / 2.0 - 1e-6 || r1 <= 421.0 - gap / 2.0 + 1e-6,
            "right half clear of the fold: {r0}..{r1}"
        );
        let job = crate::print::pipeline::JobOptions { booklet: true, gutter_mm: 10.0, nup: 4, ..Default::default() };
        let l = SheetLayout::new(&job, Some((595.0, 842.0)), 0.0);
        assert_eq!(l.nup, 2);
        assert!((l.gutter - 28.3465).abs() < 1e-3);
    }

    #[test]
    fn passthrough_without_paper() {
        let l = SheetLayout { paper: None, ..a4(Orientation::Auto, Scaling::Fit) };
        assert!(l.passthrough());
        assert_eq!(l.sheet_size(Some((100.0, 50.0)), (1.0, 1.0)), (100.0, 50.0));
        assert_eq!(l.place(0, (100.0, 50.0), (100.0, 50.0)), Matrix::IDENTITY);
    }
}
