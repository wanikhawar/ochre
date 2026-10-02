//! The Ochre logo (app icon) and wordmark, drawn from the SVGs in `packaging/`.

use std::sync::OnceLock;

use eframe::egui::{self, ImageSource, SizeHint};

pub const ICON_SVG: &[u8] = include_bytes!("../../packaging/ochre.svg");
pub const WORDMARK_SVG: &[u8] = include_bytes!("../../packaging/ochre-wordmark.svg");

/// The wordmark's burnt umber, which is too dark on the dark theme...
const UMBER: &str = "#75442B";
/// ...where it's drawn in the icon's light parchment instead.
const PARCHMENT: &str = "#E8D7B3";

pub fn icon() -> ImageSource<'static> {
    ImageSource::Bytes { uri: "bytes://ochre.svg".into(), bytes: ICON_SVG.into() }
}

/// The wordmark in colors for the light or dark theme.
pub fn wordmark(dark: bool) -> ImageSource<'static> {
    if !dark {
        return ImageSource::Bytes { uri: "bytes://ochre-wordmark.svg".into(), bytes: WORDMARK_SVG.into() };
    }
    static DARK: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = DARK.get_or_init(|| String::from_utf8_lossy(WORDMARK_SVG).replace(UMBER, PARCHMENT).into_bytes());
    ImageSource::Bytes { uri: "bytes://ochre-wordmark-dark.svg".into(), bytes: bytes.as_slice().into() }
}

/// The app icon as pixels, for the window (taskbar, window switcher).
pub fn window_icon() -> Option<egui::IconData> {
    let size = SizeHint::Size { width: 256, height: 256, maintain_aspect_ratio: true };
    let image = egui_extras::image::load_svg_bytes_with_size(ICON_SVG, size, &resvg::usvg::Options::default()).ok()?;
    let [width, height] = image.size;
    let rgba = image.pixels.iter().flat_map(|p| p.to_srgba_unmultiplied()).collect();
    Some(egui::IconData { rgba, width: width as u32, height: height as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(svg: &[u8]) -> egui::ColorImage {
        let size = SizeHint::Size { width: 300, height: 300, maintain_aspect_ratio: true };
        egui_extras::image::load_svg_bytes_with_size(svg, size, &resvg::usvg::Options::default()).unwrap()
    }

    /// The README shows `packaging/ochre-wordmark-dark.svg` on GitHub's dark theme:
    /// it must stay the wordmark with the same color swap the app makes.
    #[test]
    fn the_readmes_dark_wordmark_matches_the_wordmark() {
        let dark = include_str!("../../packaging/ochre-wordmark-dark.svg");
        let expected = String::from_utf8_lossy(WORDMARK_SVG).replace(UMBER, PARCHMENT);
        assert!(dark == expected, "regenerate packaging/ochre-wordmark-dark.svg from ochre-wordmark.svg ({UMBER} -> {PARCHMENT})");
    }

    #[test]
    fn logos_render_and_the_dark_wordmark_is_light() {
        let icon = window_icon().expect("icon renders");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert!(icon.rgba.chunks(4).any(|p| p[3] > 0), "not blank");

        let light = render(WORDMARK_SVG);
        let dark_bytes = String::from_utf8_lossy(WORDMARK_SVG).replace(UMBER, PARCHMENT);
        let dark = render(dark_bytes.as_bytes());
        assert_eq!(light.size, dark.size);
        // The lettering (umber when light) is the darkest opaque color in the light
        // version; in the dark version nothing that dark is left.
        let luma = |p: &egui::Color32| {
            let [r, g, b, _] = p.to_srgba_unmultiplied();
            (r as u32 * 3 + g as u32 * 6 + b as u32) / 10
        };
        let darkest = |img: &egui::ColorImage| img.pixels.iter().filter(|p| p.a() == 255).map(luma).min().unwrap();
        assert!(darkest(&light) < 100, "umber lettering: {}", darkest(&light));
        assert!(darkest(&dark) > 100, "parchment lettering: {}", darkest(&dark));
    }
}
