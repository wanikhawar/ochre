//! Acrobat-like light and dark themes. The app follows the system theme.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme, Vec2,
    style::{ScrollStyle, Selection, WidgetVisuals},
};

/// Acrobat blue.
pub const ACCENT: Color32 = Color32::from_rgb(0x14, 0x73, 0xE6);
pub const WARN: Color32 = Color32::from_rgb(0xE6, 0x8A, 0x00);

#[derive(Clone, Copy)]
pub struct Palette {
    /// App bar, tool rail, floating pills.
    pub bar: Color32,
    /// Area behind the pages.
    pub canvas: Color32,
    pub border: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    /// Background of the active tool / selected item.
    pub accent_tint: Color32,
    /// Text fields and slider tracks.
    pub field: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bar: Color32::from_rgb(0x2B, 0x2B, 0x2B),
            canvas: Color32::from_rgb(0x1E, 0x1E, 0x1E),
            border: Color32::from_rgb(0x3A, 0x3A, 0x3A),
            text: Color32::from_rgb(0xE6, 0xE6, 0xE6),
            weak: Color32::from_rgb(0x9A, 0x9A, 0x9A),
            hover: Color32::from_rgb(0x3A, 0x3A, 0x3A),
            pressed: Color32::from_rgb(0x46, 0x46, 0x46),
            accent_tint: Color32::from_rgb(0x23, 0x4A, 0x7A),
            field: Color32::from_rgb(0x22, 0x22, 0x22),
        }
    } else {
        Palette {
            bar: Color32::WHITE,
            canvas: Color32::from_rgb(0xE9, 0xE9, 0xE9),
            border: Color32::from_rgb(0xDA, 0xDA, 0xDA),
            text: Color32::from_rgb(0x1F, 0x1F, 0x1F),
            weak: Color32::from_rgb(0x6E, 0x6E, 0x6E),
            hover: Color32::from_rgb(0xF0, 0xF0, 0xF0),
            pressed: Color32::from_rgb(0xE3, 0xE3, 0xE3),
            accent_tint: Color32::from_rgb(0xE1, 0xEE, 0xFC),
            field: Color32::from_rgb(0xF5, 0xF5, 0xF5),
        }
    }
}

/// Palette for the theme currently in use.
pub fn current(ctx: &egui::Context) -> Palette {
    palette(ctx.global_style().visuals.dark_mode)
}

pub fn install(ctx: &egui::Context) {
    ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::System);
    ctx.set_style_of(Theme::Light, style(false));
    ctx.set_style_of(Theme::Dark, style(true));
}

/// Soft shadow used for floating pills and cards.
pub fn soft_shadow(dark: bool) -> Shadow {
    Shadow { offset: [0, 2], blur: 14, spread: 0, color: Color32::from_black_alpha(if dark { 90 } else { 34 }) }
}

/// Frame of the floating pills over the canvas.
pub fn pill(ctx: &egui::Context) -> egui::Frame {
    let dark = ctx.global_style().visuals.dark_mode;
    let p = palette(dark);
    egui::Frame::new()
        .fill(p.bar)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(8)
        .shadow(soft_shadow(dark))
        .inner_margin(Margin::symmetric(6, 3))
}

fn widget(bg: Color32, fg: Color32, stroke: Stroke) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: stroke,
        corner_radius: CornerRadius::same(6),
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    }
}

fn style(dark: bool) -> egui::Style {
    let p = palette(dark);
    let mut s = egui::Style::default();
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };

    v.panel_fill = p.bar;
    v.window_fill = p.bar;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if dark { 110 } else { 45 }) };
    v.popup_shadow = soft_shadow(dark);
    v.extreme_bg_color = p.field;
    v.text_edit_bg_color = Some(p.field);
    v.faint_bg_color = p.hover;
    v.hyperlink_color = ACCENT;
    v.weak_text_color = Some(p.weak);
    v.selection = Selection { bg_fill: p.accent_tint, stroke: Stroke::new(1.5, ACCENT) };
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;

    let none = Stroke::NONE;
    v.widgets.noninteractive = widget(p.bar, p.text, Stroke::new(1.0, p.border));
    // Buttons are flat until hovered; slider tracks use `bg_fill`.
    // `bg_fill` of inactive widgets is the slider track and checkbox background.
    v.widgets.inactive = widget(p.border, p.text, none);
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.hovered = widget(p.hover, p.text, none);
    v.widgets.active = widget(p.pressed, p.text, Stroke::new(1.0, ACCENT));
    v.widgets.open = widget(p.hover, p.text, none);
    s.visuals = v;

    let sp = &mut s.spacing;
    sp.item_spacing = Vec2::new(5.0, 5.0);
    sp.button_padding = Vec2::new(8.0, 3.0);
    sp.interact_size = Vec2::new(24.0, 22.0);
    sp.slider_width = 72.0;
    sp.menu_margin = Margin::same(6);
    sp.window_margin = Margin::same(16);
    sp.scroll = ScrollStyle::floating();

    s.text_styles = [
        (TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(10.5, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
    ]
    .into();
    s
}
