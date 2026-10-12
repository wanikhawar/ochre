//! Output sheets on screen: pdfium draws each page straight onto the sheet,
//! placed with the same geometry the PDF writer uses, so the preview shows
//! exactly what prints without writing a PDF.

use crate::print::geometry::{Matrix, SheetLayout, rotated_size};
use crate::print::pipeline::{BLANK, Orientation, Scaling, Side};
use crate::print::ppd::Tone;

/// How one output sheet is made up.
#[derive(Clone, Debug, PartialEq)]
pub struct SheetSpec {
    pub side: Side,
    /// Extra clockwise rotation of the sheet on paper.
    pub rotation: u32,
    pub layout: SheetLayout,
    /// Size for blank sides when no paper size is known.
    pub fallback: (f64, f64),
    /// The colors as the printer driver will print them.
    pub tone: Tone,
}

/// Shows pages exactly as they are in the document (no paper layout).
pub const AS_IS: SheetLayout = SheetLayout {
    nup: 1,
    paper: None,
    margin: 0.0,
    orientation: Orientation::Auto,
    scaling: Scaling::Fit,
    gutter: 0.0,
};

/// One page on a sheet drawn at some scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub page: usize,
    /// The page as displayed (points, y down, origin top-left) -> sheet pixels.
    pub matrix: Matrix,
    /// Where it may draw, in sheet pixels: `[left, top, right, bottom]`.
    pub clip: [f64; 4],
}

impl SheetSpec {
    /// One document page on its own, as it is in the file.
    pub fn page(page: usize) -> SheetSpec {
        SheetSpec { side: vec![page], rotation: 0, layout: AS_IS, fallback: (612.0, 792.0), tone: Tone::default() }
    }

    /// Sheet size before the sheet's own rotation. `sizes`: pages as displayed, in points.
    fn sheet_size(&self, sizes: &[(f64, f64)]) -> (f64, f64) {
        let first = self.side.iter().find(|&&i| i != BLANK).and_then(|&i| sizes.get(i)).copied();
        self.layout.sheet_size(first, self.fallback)
    }

    /// Size of the sheet as it will look on paper, in points.
    pub fn displayed_size(&self, sizes: &[(f64, f64)]) -> (f64, f64) {
        let (w, h) = self.sheet_size(sizes);
        rotated_size(w, h, self.rotation)
    }

    /// Pixel size of the sheet drawn at `k` pixels per point.
    pub fn pixel_size(&self, sizes: &[(f64, f64)], k: f64) -> (u32, u32) {
        let (dw, dh) = self.displayed_size(sizes);
        ((dw * k).ceil().max(1.0) as u32, (dh * k).ceil().max(1.0) as u32)
    }

    /// Where each page goes when the sheet is drawn at `k` pixels per point.
    pub fn placements(&self, sizes: &[(f64, f64)], k: f64) -> Vec<Placement> {
        let (sw, sh) = self.sheet_size(sizes);
        let (pixels_w, pixels_h) = self.pixel_size(sizes, k);
        let to_screen = screen_rotation(sw, sh, self.rotation).then(Matrix::scale(k));
        self.side
            .iter()
            .enumerate()
            .filter(|&(_, &i)| i != BLANK)
            .filter_map(|(slot, &page)| {
                let (pw, ph) = *sizes.get(page)?;
                // The page as displayed (y down) -> the sheet (y up) -> the screen (y down).
                let matrix = Matrix::flip_y(ph)
                    .then(self.layout.place(slot, (sw, sh), (pw, ph)))
                    .then(Matrix::flip_y(sh))
                    .then(to_screen);
                // The page's own area, and nothing beyond the paper.
                let corners = [(0.0, 0.0), (pw, 0.0), (0.0, ph), (pw, ph)].map(|(x, y)| matrix.apply(x, y));
                let (x0, x1) = corners.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.0), hi.max(p.0)));
                let (y0, y1) = corners.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.1), hi.max(p.1)));
                let clip = [x0.max(0.0), y0.max(0.0), x1.min(pixels_w as f64), y1.min(pixels_h as f64)];
                (clip[0] < clip[2] && clip[1] < clip[3]).then_some(Placement { page, matrix, clip })
            })
            .collect()
    }
}

/// Changes RGBA pixels the way the printer driver will: brightness, contrast
/// and color balance, then grayscale. Paper stays white and black stays black,
/// as no driver puts ink on blank paper; the mid-tones move.
pub fn apply_tone(tone: &Tone, rgba: &mut [u8]) {
    if tone.is_plain() {
        return;
    }
    let curve = |balance: f32| -> [u8; 256] {
        std::array::from_fn(|i| {
            let mut c = i as f32 / 255.0;
            // Contrast: an S-curve (more) or its inverse (less).
            let s =
                if tone.contrast >= 0.0 { c * c * (3.0 - 2.0 * c) } else { 0.5 - ((1.0 - 2.0 * c).asin() / 3.0).sin() };
            c += (s - c) * tone.contrast.abs();
            // Brightness and color balance lift or lower the mid-tones.
            c = c.clamp(0.0, 1.0).powf(2f32.powf(-0.8 * tone.brightness - 0.6 * balance));
            (c * 255.0).round() as u8
        })
    };
    let curves = tone.balance.map(curve);
    for p in rgba.as_chunks_mut::<4>().0 {
        for (c, curve) in p[..3].iter_mut().zip(&curves) {
            *c = curve[*c as usize];
        }
        if tone.gray {
            let y = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32).round() as u8;
            p[..3].fill(y);
        }
    }
}

/// Clockwise rotation of a `w`×`h` sheet in y-down (screen) coordinates.
fn screen_rotation(w: f64, h: f64, rotation: u32) -> Matrix {
    match rotation % 360 {
        90 => Matrix([0.0, 1.0, -1.0, 0.0, h, 0.0]),
        180 => Matrix([-1.0, 0.0, 0.0, -1.0, w, h]),
        270 => Matrix([0.0, -1.0, 1.0, 0.0, 0.0, w]),
        _ => Matrix::IDENTITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::pipeline::JobOptions;

    const A4: (f64, f64) = (595.0, 842.0);

    #[test]
    fn a_page_as_is_fills_its_sheet() {
        let p = SheetSpec::page(0).placements(&[A4], 0.5);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].matrix, Matrix::scale(0.5));
        assert_eq!(p[0].clip, [0.0, 0.0, 297.5, 421.0]);
    }

    #[test]
    fn two_up_pages_lie_sideways_on_portrait_paper() {
        let job = JobOptions { nup: 2, ..Default::default() };
        let layout = SheetLayout::new(&job, Some(A4), 0.0);
        let spec = SheetSpec { side: vec![0, 1], layout, fallback: A4, ..SheetSpec::page(0) };
        let p = spec.placements(&[A4, A4], 1.0);
        // Page 1 on the bottom half, page 2 on the top; both with their top edge to the left.
        assert!(p[0].clip[1] >= 420.0 && p[1].clip[3] <= 422.0, "{p:?}");
        for q in &p {
            let (x_top, _) = q.matrix.apply(297.0, 0.0);
            let (x_bottom, _) = q.matrix.apply(297.0, 842.0);
            assert!(x_top < x_bottom, "top edge on the left: {q:?}");
        }
        assert_eq!(spec.pixel_size(&[A4, A4], 1.0), (595, 842));
    }

    #[test]
    fn sheet_rotation_turns_everything() {
        let spec = SheetSpec { rotation: 90, ..SheetSpec::page(0) };
        assert_eq!(spec.displayed_size(&[A4]), (842.0, 595.0));
        // The page's top-left corner ends up at the sheet's top-right.
        let p = spec.placements(&[A4], 1.0)[0];
        let (x, y) = p.matrix.apply(0.0, 0.0);
        assert!((x - 842.0).abs() < 1e-9 && y.abs() < 1e-9, "{x},{y}");
    }

    #[test]
    fn actual_size_is_cut_off_at_the_paper() {
        let job = JobOptions { scaling: Scaling::Actual, ..Default::default() };
        let spec = SheetSpec { layout: SheetLayout::new(&job, Some(A4), 0.0), fallback: A4, ..SheetSpec::page(0) };
        let p = spec.placements(&[(1000.0, 1400.0)], 1.0)[0];
        assert_eq!(p.clip, [0.0, 0.0, 595.0, 842.0]);
    }

    #[test]
    fn tone_keeps_paper_white_and_moves_the_mid_tones() {
        let px = |tone: Tone| {
            let mut rgba = vec![255, 255, 255, 255, 0, 0, 0, 255, 128, 128, 128, 255];
            apply_tone(&tone, &mut rgba);
            rgba
        };
        for tone in [
            Tone { brightness: -1.0, ..Tone::default() },
            Tone { contrast: 1.0, ..Tone::default() },
            Tone { contrast: -1.0, ..Tone::default() },
        ] {
            let p = px(tone);
            assert_eq!((&p[..3], &p[4..7]), (&[255, 255, 255][..], &[0, 0, 0][..]), "{tone:?}");
        }
        assert!(px(Tone { brightness: 1.0, ..Tone::default() })[8] > 160);
        assert!(px(Tone { brightness: -1.0, ..Tone::default() })[8] < 100);
        let redder = px(Tone { balance: [1.0, 0.0, 0.0], ..Tone::default() });
        assert!(redder[8] > 150 && redder[9] == 128);
        let gray = px(Tone { gray: true, balance: [1.0, 0.0, 0.0], ..Tone::default() });
        assert!(gray[8] == gray[9] && gray[9] == gray[10]);
    }
}
