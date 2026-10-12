//! Page colors for reading comfort: normal, sepia, or dark. Display only: the
//! saved PDF and the annotations' colors don't change.
//!
//! Each mode runs a pixel's lightness through a three-stop gradient (black, mid
//! gray, white) and then adds back part of its color, so colored text and
//! highlights stay recognizable. Sepia's gradient goes from dark brown through
//! warm browns to aged paper, like an old photo. Dark first inverts lightness but
//! keeps hue (`c + 1 - max - min` per channel), so black text turns light while
//! saturated colors such as a yellow highlight stay what they are.

use eframe::egui::Color32;
use serde::{Deserialize, Serialize};

use crate::annot::model::{Annotation, Style};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PageLook {
    #[default]
    Normal,
    Sepia,
    Dark,
}

/// How a page look maps colors.
struct Tone {
    /// Invert lightness (keeping hue) first.
    invert: bool,
    /// What lightness 0, 0.5 and 1 become.
    stops: [[u8; 3]; 3],
    /// How much of a color's difference from its gray is kept.
    chroma: f32,
}

/// Rec. 601 luma, the weights summing to 1.
fn luma(c: [f32; 3]) -> f32 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

impl Tone {
    /// The gradient at lightness `l` (0..=1), channel `i`.
    fn ramp(&self, l: f32, i: usize) -> f32 {
        let [a, m, b] = self.stops.map(|s| s[i] as f32 / 255.0);
        if l < 0.5 { a + (m - a) * l * 2.0 } else { m + (b - m) * (l - 0.5) * 2.0 }
    }
}

impl PageLook {
    pub const ALL: [PageLook; 3] = [PageLook::Normal, PageLook::Sepia, PageLook::Dark];

    pub fn label(self) -> &'static str {
        match self {
            PageLook::Normal => "Normal",
            PageLook::Sepia => "Sepia",
            PageLook::Dark => "Dark",
        }
    }

    pub fn next(self) -> PageLook {
        match self {
            PageLook::Normal => PageLook::Sepia,
            PageLook::Sepia => PageLook::Dark,
            PageLook::Dark => PageLook::Normal,
        }
    }

    fn tone(self) -> Option<Tone> {
        match self {
            PageLook::Normal => None,
            PageLook::Sepia => Some(Tone {
                invert: false,
                stops: [[0x2E, 0x1E, 0x12], [0x8C, 0x66, 0x42], [0xEB, 0xD7, 0xB2]],
                chroma: 0.5,
            }),
            PageLook::Dark => Some(Tone {
                invert: true,
                stops: [[0x26, 0x26, 0x2A], [0x80, 0x80, 0x82], [0xDA, 0xDA, 0xDA]],
                chroma: 0.7,
            }),
        }
    }

    /// The color of a blank page.
    pub fn paper(self) -> Color32 {
        let [r, g, b] = self.map_rgb([1.0; 3]).map(|v| (v * 255.0).round() as u8);
        Color32::from_rgb(r, g, b)
    }

    /// What black text becomes.
    pub fn ink(self) -> Color32 {
        let [r, g, b] = self.map_rgb([0.0; 3]).map(|v| (v * 255.0).round() as u8);
        Color32::from_rgb(r, g, b)
    }

    /// Maps a color (RGB in 0..=1).
    pub fn map_rgb(self, c: [f32; 3]) -> [f32; 3] {
        let Some(tone) = self.tone() else { return c };
        let c = c.map(|v| v.clamp(0.0, 1.0));
        let c = if tone.invert {
            let (max, min) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
            c.map(|v| v + 1.0 - max - min)
        } else {
            c
        };
        let l = luma(c);
        std::array::from_fn(|i| (tone.ramp(l, i) + tone.chroma * (c[i] - l)).clamp(0.0, 1.0))
    }

    /// Maps unpremultiplied RGBA pixels in place (alpha is kept).
    pub fn map_rgba(self, px: &mut [u8]) {
        let Some(tone) = self.tone() else { return };
        // Per channel, luma -> gradient color.
        let ramp: [[i32; 256]; 3] =
            std::array::from_fn(|i| std::array::from_fn(|l| (tone.ramp(l as f32 / 255.0, i) * 255.0).round() as i32));
        let chroma = (tone.chroma * 256.0).round() as i32;
        for p in px.as_chunks_mut::<4>().0 {
            let (mut r, mut g, mut b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if tone.invert {
                let s = 255 - r.max(g).max(b) - r.min(g).min(b);
                (r, g, b) = (r + s, g + s, b + s);
            }
            let l = (77 * r + 150 * g + 29 * b + 128) >> 8;
            for (i, c) in [r, g, b].into_iter().enumerate() {
                p[i] = (ramp[i][l as usize] + ((chroma * (c - l) + 128) >> 8)).clamp(0, 255) as u8;
            }
        }
    }

    pub fn map_style(self, s: &Style) -> Style {
        Style { color: self.map_rgb(s.color), fill: s.fill.map(|f| self.map_rgb(f)), ..*s }
    }

    /// `a` with its colors mapped.
    pub fn map_annot(self, a: &Annotation) -> Annotation {
        Annotation { style: self.map_style(&a.style), ..a.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    fn rgb(c: [u8; 3]) -> [f32; 3] {
        c.map(|v| v as f32 / 255.0)
    }

    #[test]
    fn black_and_white_become_ink_and_paper() {
        for look in [PageLook::Sepia, PageLook::Dark] {
            let stops = look.tone().unwrap().stops;
            let (ink, paper) = if look == PageLook::Dark { (stops[2], stops[0]) } else { (stops[0], stops[2]) };
            assert!(close(look.map_rgb([0.0; 3]), rgb(ink)), "{look:?}");
            assert!(close(look.map_rgb([1.0; 3]), rgb(paper)), "{look:?}");
            let paper32 = look.paper();
            assert_eq!([paper32.r(), paper32.g(), paper32.b()], paper);
        }
        assert_eq!(PageLook::Normal.map_rgb([0.2, 0.4, 0.6]), [0.2, 0.4, 0.6]);
    }

    #[test]
    fn dark_keeps_hue() {
        // A saturated color only gets squeezed into the ink..paper range.
        let yellow = PageLook::Dark.map_rgb([1.0, 1.0, 0.0]);
        assert!(yellow[0] > 0.8 && yellow[1] > 0.8 && yellow[2] < 0.25, "{yellow:?}");
        // A light tint becomes a dark tint of the same hue, not its complement.
        let pink = PageLook::Dark.map_rgb([1.0, 0.8, 0.8]);
        assert!(pink[0] > pink[1] + 0.1 && pink[0] > pink[2] + 0.1 && pink[0] < 0.4, "{pink:?}");
        // The inversion alone is its own inverse.
        let c = [0.3, 0.55, 0.9];
        let inv = |c: [f32; 3]| {
            let (max, min) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
            c.map(|v| v + 1.0 - max - min)
        };
        assert!(close(inv(inv(c)), c));
    }

    #[test]
    fn pixels_match_colors() {
        let colors = [[0u8, 0, 0], [255, 255, 255], [128, 128, 128], [255, 220, 0], [20, 90, 230], [250, 200, 200]];
        for look in PageLook::ALL {
            let mut px: Vec<u8> = colors.iter().flat_map(|c| [c[0], c[1], c[2], 200]).collect();
            look.map_rgba(&mut px);
            for (c, p) in colors.iter().zip(px.as_chunks::<4>().0) {
                let want = look.map_rgb(rgb(*c)).map(|v| v * 255.0);
                for i in 0..3 {
                    assert!((p[i] as f32 - want[i]).abs() <= 2.0, "{look:?} {c:?}: {p:?} vs {want:?}");
                }
                assert_eq!(p[3], 200, "alpha is kept");
            }
        }
    }

    #[test]
    fn cycles_through_all() {
        let mut l = PageLook::Normal;
        for want in [PageLook::Sepia, PageLook::Dark, PageLook::Normal] {
            l = l.next();
            assert_eq!(l, want);
        }
    }
}
