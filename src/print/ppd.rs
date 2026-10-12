//! Just enough of a PPD parser to list a printer's options, their choices and
//! defaults, and its paper sizes.

use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct PrinterOption {
    pub keyword: String,
    pub label: String,
    pub group: String,
    pub default: String,
    /// (value, label)
    pub choices: Vec<(String, String)>,
    /// Shown under "Printer settings" rather than tucked away in "Advanced".
    pub common: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Ppd {
    pub options: Vec<PrinterOption>,
    pub paper_sizes: HashMap<String, (f64, f64)>,
    /// Printable area per paper size: left, bottom, right, top, in points.
    pub imageable: HashMap<String, [f64; 4]>,
}

/// Duplicate of PageSize that every PPD carries.
const HIDDEN: [&str; 1] = ["PageRegion"];
const COMMON_KEYWORDS: [&str; 7] =
    ["PageSize", "MediaType", "InputSlot", "ColorModel", "Resolution", "cupsPrintQuality", "OutputMode"];
const COMMON_WORDS: [&str; 11] = [
    "quality",
    "resolution",
    "color model",
    "colour model",
    "colormodel",
    "grayscale",
    "greyscale",
    "monocolor",
    "mono color",
    "media type",
    "mediatype",
];

fn is_common(keyword: &str, label: &str) -> bool {
    let text = format!("{keyword} {label}").to_lowercase();
    COMMON_KEYWORDS.contains(&keyword) || COMMON_WORDS.iter().any(|w| text.contains(w))
}

/// "Name/Label" -> ("Name", "Label"); the label defaults to the name.
fn name_and_label(spec: &str) -> (String, String) {
    match spec.split_once('/') {
        Some((name, label)) if !label.trim().is_empty() => (name.trim().into(), label.trim().into()),
        Some((name, _)) => (name.trim().into(), name.trim().into()),
        None => (spec.trim().into(), spec.trim().into()),
    }
}

/// PPDs are ISO-8859-1 by default.
pub fn decode(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

pub fn parse(text: &str) -> Ppd {
    let mut ppd = Ppd::default();
    let mut defaults: HashMap<String, String> = HashMap::new();
    let mut group: Option<String> = None;
    let mut current: Option<PrinterOption> = None;
    let mut in_quote = false;

    for raw in text.lines() {
        if in_quote {
            if raw.contains('"') {
                in_quote = false;
            }
            continue;
        }
        let line = raw.trim_end();
        if !line.starts_with('*') || line.starts_with("*%") {
            continue;
        }
        if line.matches('"').count() % 2 == 1 {
            in_quote = true; // value continues on the next lines
        }
        let (head, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.trim();
        let (keyword, spec) = head[1..].split_once(char::is_whitespace).unwrap_or((&head[1..], ""));
        let spec = spec.trim();

        match keyword {
            "OpenGroup" => group = Some(name_and_label(value).1),
            "CloseGroup" => group = None,
            "OpenUI" | "JCLOpenUI" => {
                let (kw, label) = name_and_label(spec.trim_start_matches('*'));
                current = Some(PrinterOption {
                    common: is_common(&kw, &label),
                    keyword: kw,
                    label,
                    group: group.clone().unwrap_or_else(|| "General".into()),
                    default: String::new(),
                    choices: Vec::new(),
                });
            }
            "CloseUI" | "JCLCloseUI" => {
                if let Some(opt) = current.take()
                    && !opt.choices.is_empty()
                    && !HIDDEN.contains(&opt.keyword.as_str())
                {
                    ppd.options.push(opt);
                }
            }
            "ImageableArea" => {
                let (name, _) = name_and_label(spec);
                let nums: Vec<f64> =
                    value.trim_matches('"').split_whitespace().filter_map(|n| n.parse().ok()).collect();
                if let [l, b, r, t, ..] = nums.as_slice() {
                    ppd.imageable.insert(name, [*l, *b, *r, *t]);
                }
            }
            "PaperDimension" => {
                let (name, _) = name_and_label(spec);
                let nums: Vec<f64> =
                    value.trim_matches('"').split_whitespace().filter_map(|n| n.parse().ok()).collect();
                if let [w, h, ..] = nums.as_slice() {
                    ppd.paper_sizes.insert(name, (*w, *h));
                }
            }
            kw if kw.starts_with("Default") && spec.is_empty() => {
                if let Some(v) = value.split_whitespace().next() {
                    defaults.insert(kw["Default".len()..].to_string(), v.to_string());
                }
            }
            kw => {
                if let Some(opt) = current.as_mut().filter(|o| o.keyword == kw && !spec.is_empty()) {
                    opt.choices.push(name_and_label(spec));
                }
            }
        }
    }

    for opt in &mut ppd.options {
        opt.default = defaults
            .get(&opt.keyword)
            .filter(|d| opt.choices.iter().any(|(v, _)| v == *d))
            .cloned()
            .unwrap_or_else(|| opt.choices[0].0.clone());
    }
    ppd
}

/// How the printer driver will change the colors, as far as the preview can show it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tone {
    pub gray: bool,
    /// Each from -1 (the lowest setting) through 0 (none) to 1 (the highest).
    pub brightness: f32,
    pub contrast: f32,
    /// Color balance: red, green, blue.
    pub balance: [f32; 3],
}

impl Tone {
    pub fn is_plain(&self) -> bool {
        *self == Tone::default()
    }
}

/// The tone the chosen `values` (keyword -> choice) give: grayscale, and the
/// brightness, contrast and color balance sliders many drivers have (Brother's
/// `BRBrightness`, `BRRed`… from -20 to 20, Gutenprint's `Brightness`…).
pub fn tone(options: &[PrinterOption], values: &HashMap<String, String>) -> Tone {
    let mut tone = Tone { gray: grayscale(options, values), ..Tone::default() };
    for opt in options {
        let Some(level) = values.get(&opt.keyword).and_then(|v| level(opt, v)) else { continue };
        let name = opt.label.to_lowercase();
        let keyword = opt.keyword.to_lowercase();
        let is = |word: &str| name == word || keyword.ends_with(word);
        if is("brightness") {
            tone.brightness = level;
        } else if is("contrast") {
            tone.contrast = level;
        } else if let Some(i) = ["red", "green", "blue"].iter().position(|c| is(c)) {
            tone.balance[i] = level;
        }
    }
    tone
}

/// Sets every option that can make the printer print in grayscale to a color
/// choice: its default if that's in color, else its first choice that is.
pub fn in_color(options: &[PrinterOption], values: &mut HashMap<String, String>) {
    for opt in options {
        let gray =
            |v: &str| grayscale(std::slice::from_ref(opt), &HashMap::from([(opt.keyword.clone(), v.to_string())]));
        if !values.get(&opt.keyword).is_some_and(|v| gray(v)) {
            continue;
        }
        let color = std::iter::once(&opt.default).chain(opt.choices.iter().map(|(v, _)| v)).find(|v| !gray(v));
        if let Some(v) = color {
            values.insert(opt.keyword.clone(), v.clone());
        }
    }
}

/// Turns off the printer's own two-sided printing: every option about duplex
/// (`Duplex`, `EFDuplex`, `JCLDuplex`…) is set to its one-sided choice, if it has one.
pub fn one_sided(options: &[PrinterOption], values: &mut HashMap<String, String>) {
    const DUPLEX: [&str; 4] = ["duplex", "2-sided", "two-sided", "double-sided"];
    const SIMPLEX: [&str; 7] = ["none", "off", "simplex", "one-sided", "1-sided", "single-sided", "onesided"];
    for opt in options {
        let about = format!("{} {}", opt.keyword, opt.label).to_lowercase();
        if !DUPLEX.iter().any(|w| about.contains(w)) {
            continue;
        }
        let simplex = opt.choices.iter().find(|(v, l)| {
            let (v, l) = (v.to_lowercase(), l.to_lowercase());
            SIMPLEX.iter().any(|w| v == *w || l == *w || v.contains("simplex") || l.contains("simplex"))
        });
        if let Some((v, _)) = simplex {
            values.insert(opt.keyword.clone(), v.clone());
        }
    }
}

/// Where `value` sits on a numeric option's scale: -1 at the lowest choice, 0
/// in the middle (or at 0 when the scale goes from negative to positive), 1 at
/// the highest. None for options that aren't numbers.
fn level(opt: &PrinterOption, value: &str) -> Option<f32> {
    let numbers: Vec<f32> = opt.choices.iter().map(|(v, _)| v.parse::<f32>().ok()).collect::<Option<_>>()?;
    let (lo, hi) = numbers.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &n| (lo.min(n), hi.max(n)));
    if numbers.len() < 3 || hi <= lo {
        return None;
    }
    let neutral = if lo < 0.0 && hi > 0.0 { 0.0 } else { (lo + hi) / 2.0 };
    let v: f32 = value.parse().ok()?;
    Some(if v >= neutral { (v - neutral) / (hi - neutral) } else { (v - neutral) / (neutral - lo) }.clamp(-1.0, 1.0))
}

/// Whether the chosen `values` (keyword -> choice) make the printer print in
/// grayscale. Drivers say it many ways: `ColorModel=Gray`, Brother's
/// `BRMonoColor=Mono`, `print-color-mode=monochrome`, HP's `KGray`, or a
/// "Grayscale" option switched on (Canon's `CNIJGrayScale`).
fn grayscale(options: &[PrinterOption], values: &HashMap<String, String>) -> bool {
    const GRAY: [&str; 6] = ["gray", "grey", "mono", "black and white", "black & white", "blackwhite"];
    const COLOR: [&str; 4] = ["color", "colour", "outputmode", "output mode"];
    const ON: [&str; 4] = ["on", "yes", "true", "1"];
    let has = |text: &str, words: &[&str]| {
        let text = text.to_lowercase();
        words.iter().any(|w| text.contains(w))
    };
    options.iter().any(|opt| {
        let Some(value) = values.get(&opt.keyword) else { return false };
        let label = opt.choices.iter().find(|(v, _)| v == value).map_or("", |(_, l)| l.as_str());
        let about = format!("{} {}", opt.keyword, opt.label);
        if has(&about, &GRAY) && !has(&about, &COLOR) {
            // An option for grayscale itself: "Grayscale: On".
            ON.iter().any(|on| value.eq_ignore_ascii_case(on) || label.eq_ignore_ascii_case(on))
        } else {
            // A color option set to gray: "Color / Grayscale: Mono", "ColorModel: Gray".
            has(&about, &COLOR) && has(&format!("{value} {label}"), &GRAY)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"*PPD-Adobe: "4.3"
*% comment
*OpenGroup: General/General
*OpenUI *PageSize/Media Size: PickOne
*DefaultPageSize: Letter
*PageSize A4/A4: "<</PageSize[595 842]>>setpagedevice"
*PageSize Letter/US Letter: "<</PageSize[612 792]
>>setpagedevice"
*CloseUI: *PageSize
*OpenUI *PageRegion: PickOne
*PageRegion A4/A4: "x"
*CloseUI: *PageRegion
*CloseGroup: General
*OpenGroup: Print Settings (Advanced)
*OpenUI *BRResolution/Print Quality: PickOne
*DefaultBRResolution: PlainNormal
*BRResolution PlainFast/Plain Fast: ""
*BRResolution PlainNormal/Plain Normal: ""
*CloseUI: *BRResolution
*OpenUI *BRBiDir/Bi-Directional Printing: PickOne
*DefaultBRBiDir: ON
*BRBiDir OFF/Off: ""
*BRBiDir ON/On: ""
*CloseUI: *BRBiDir
*CloseGroup: Print Settings (Advanced)
*PaperDimension A4/A4:										"595 842"
*PaperDimension Letter/US Letter: "612 792"
*ImageableArea A4/A4:								"9 9 586 833"
"#;

    #[test]
    fn parses_options_groups_defaults_and_paper() {
        let ppd = parse(SAMPLE);
        let kws: Vec<_> = ppd.options.iter().map(|o| o.keyword.as_str()).collect();
        assert_eq!(kws, ["PageSize", "BRResolution", "BRBiDir"]);
        let size = &ppd.options[0];
        assert_eq!((size.default.as_str(), size.choices.len(), size.common), ("Letter", 2, true));
        assert_eq!(size.choices[1], ("Letter".to_string(), "US Letter".to_string()));
        let quality = &ppd.options[1];
        assert_eq!(
            (quality.group.as_str(), quality.label.as_str(), quality.common),
            ("Print Settings (Advanced)", "Print Quality", true)
        );
        assert!(!ppd.options[2].common);
        assert_eq!(ppd.paper_sizes["A4"], (595.0, 842.0));
        assert_eq!(ppd.paper_sizes["Letter"], (612.0, 792.0));
        assert_eq!(ppd.imageable["A4"], [9.0, 9.0, 586.0, 833.0]);
    }

    fn option(keyword: &str, label: &str, choices: &[(&str, &str)]) -> PrinterOption {
        PrinterOption {
            keyword: keyword.into(),
            label: label.into(),
            group: "General".into(),
            default: choices[0].0.into(),
            choices: choices.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
            common: true,
        }
    }

    #[test]
    fn knows_the_ways_drivers_say_grayscale() {
        let options = [
            option("BRMonoColor", "Color / Grayscale", &[("Color", "Color"), ("Mono", "Grayscale")]),
            option("BRColorMatching", "Color Mode", &[("Natural", "Natural"), ("None", "None")]),
            option("BRColorEnhancement", "Color Enhancement", &[("OFF", "OFF"), ("ON", "ON")]),
            option("ColorModel", "Output Mode", &[("RGB", "Color"), ("KGray", "Black Only")]),
            option("print-color-mode", "Color Mode", &[("color", "Color"), ("monochrome", "Monochrome")]),
            option("CNIJGrayScale", "Grayscale Printing", &[("0", "OFF"), ("1", "ON")]),
        ];
        let gray = |pairs: &[(&str, &str)]| {
            let values = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            tone(&options, &values).gray
        };
        assert!(!gray(&[("BRMonoColor", "Color"), ("BRColorMatching", "None"), ("BRColorEnhancement", "ON")]));
        assert!(gray(&[("BRMonoColor", "Mono")]));
        assert!(gray(&[("ColorModel", "KGray")]));
        assert!(!gray(&[("ColorModel", "RGB")]));
        assert!(gray(&[("print-color-mode", "monochrome")]));
        assert!(gray(&[("CNIJGrayScale", "1")]));
        assert!(!gray(&[("CNIJGrayScale", "0")]));
    }

    #[test]
    fn reads_brightness_contrast_and_color_balance() {
        let scale: Vec<(String, String)> = (-20..=20).map(|n| (n.to_string(), n.to_string())).collect();
        let slider = |keyword: &str, label: &str| PrinterOption {
            choices: scale.clone(),
            ..option(keyword, label, &[("0", "0")])
        };
        let options = [
            slider("BRBrightness", "Brightness"),
            slider("BRContrast", "Contrast"),
            slider("BRRed", "Red"),
            slider("BRBlue", "Blue"),
            option("BRHalfTonePattern", "Halftone Pattern", &[("Diffusion", "Diffusion"), ("Dither", "Dither")]),
        ];
        let values: HashMap<String, String> = [
            ("BRBrightness", "10"),
            ("BRContrast", "-20"),
            ("BRRed", "5"),
            ("BRBlue", "0"),
            ("BRHalfTonePattern", "Dither"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let t = tone(&options, &values);
        assert_eq!((t.brightness, t.contrast, t.balance, t.gray), (0.5, -1.0, [0.25, 0.0, 0.0], false));
        assert!(tone(&options, &HashMap::new()).is_plain());
        // A 0 to 2 scale is neutral in the middle.
        let gutenprint = PrinterOption {
            choices: ["0", "0.5", "1", "1.5", "2"].iter().map(|v| (v.to_string(), v.to_string())).collect(),
            ..option("Brightness", "Brightness", &[("1", "1")])
        };
        let t = tone(&[gutenprint], &HashMap::from([("Brightness".to_string(), "1.5".to_string())]));
        assert_eq!(t.brightness, 0.5);
    }

    #[test]
    fn back_to_color() {
        let options = [
            option("BRMonoColor", "Color / Grayscale", &[("Color", "Color"), ("Mono", "Grayscale")]),
            // A printer whose own default is grayscale.
            PrinterOption {
                default: "Gray".into(),
                ..option("ColorModel", "Output Mode", &[("Gray", "Grayscale"), ("RGB", "Color")])
            },
            option("BRResolution", "Print Quality", &[("Normal", "Normal"), ("Best", "Best")]),
        ];
        let mut values: HashMap<String, String> =
            [("BRMonoColor", "Mono"), ("ColorModel", "Gray"), ("BRResolution", "Best")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        in_color(&options, &mut values);
        assert_eq!((values["BRMonoColor"].as_str(), values["ColorModel"].as_str()), ("Color", "RGB"));
        assert_eq!(values["BRResolution"], "Best", "other settings stay");
    }

    #[test]
    fn manual_two_sided_turns_off_the_printers_duplexer() {
        let options = [
            option(
                "Duplex",
                "2-Sided Printing",
                &[("None", "Off"), ("DuplexNoTumble", "Long Edge"), ("DuplexTumble", "Short Edge")],
            ),
            option("EFDuplex", "Duplex", &[("False", "Simplex"), ("TopTop", "Top-Top")]),
            // Brother's "advanced" settings for its manual two-sided mode: no one-sided choice.
            option("BRDuplexMode", "Advanced 2-sided Settings", &[("Normal", "Normal"), ("Duplex1", "Duplex1")]),
        ];
        let mut values: HashMap<String, String> =
            [("Duplex", "DuplexNoTumble"), ("EFDuplex", "TopTop"), ("BRDuplexMode", "Duplex1")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        one_sided(&options, &mut values);
        assert_eq!(values["Duplex"], "None");
        assert_eq!(values["EFDuplex"], "False");
        assert_eq!(values["BRDuplexMode"], "Duplex1", "left alone");
    }
}
