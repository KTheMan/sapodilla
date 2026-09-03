use egui::{
    Color32, Context, CornerRadius, FontId, Frame, Margin, RichText, Sense, Stroke, TextStyle, Ui,
};
use serde::{Deserialize, Serialize};

pub const DEFAULT_ACCENT_RGB: [u8; 3] = [255, 45, 170];
pub const DEFAULT_ACCENT: Color32 = Color32::from_rgb(
    DEFAULT_ACCENT_RGB[0],
    DEFAULT_ACCENT_RGB[1],
    DEFAULT_ACCENT_RGB[2],
);
pub const SECONDARY_CYAN: Color32 = Color32::from_rgb(39, 221, 235);
pub const SIGNAL_LIME: Color32 = Color32::from_rgb(216, 255, 57);
pub const DANGER: Color32 = Color32::from_rgb(255, 93, 115);
pub const INK: Color32 = Color32::from_rgb(11, 11, 16);
pub const KISS_GREEN: Color32 = Color32::from_rgb(67, 170, 139);
pub const PERFORATION_RED: Color32 = Color32::from_rgb(249, 65, 68);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccentPreset {
    SapodillaPink,
    ElectricCyan,
    SignalLime,
    Tangerine,
    StudioViolet,
    CobaltBlue,
}

impl AccentPreset {
    pub const ALL: [Self; 6] = [
        Self::SapodillaPink,
        Self::ElectricCyan,
        Self::SignalLime,
        Self::Tangerine,
        Self::StudioViolet,
        Self::CobaltBlue,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::SapodillaPink => "Sapodilla Pink",
            Self::ElectricCyan => "Electric Cyan",
            Self::SignalLime => "Signal Lime",
            Self::Tangerine => "Tangerine",
            Self::StudioViolet => "Studio Violet",
            Self::CobaltBlue => "Cobalt Blue",
        }
    }

    pub const fn rgb(self) -> [u8; 3] {
        match self {
            Self::SapodillaPink => DEFAULT_ACCENT_RGB,
            Self::ElectricCyan => [39, 221, 235],
            Self::SignalLime => [216, 255, 57],
            Self::Tangerine => [255, 138, 42],
            Self::StudioViolet => [155, 123, 255],
            Self::CobaltBlue => [60, 140, 255],
        }
    }

    pub const fn color(self) -> Color32 {
        let [red, green, blue] = self.rgb();
        Color32::from_rgb(red, green, blue)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccentChoice {
    Preset(AccentPreset),
    Custom([u8; 3]),
}

impl Default for AccentChoice {
    fn default() -> Self {
        Self::Preset(AccentPreset::SapodillaPink)
    }
}

impl AccentChoice {
    pub const fn rgb(self) -> [u8; 3] {
        match self {
            Self::Preset(preset) => preset.rgb(),
            Self::Custom(rgb) => rgb,
        }
    }

    pub const fn color(self) -> Color32 {
        let [red, green, blue] = self.rgb();
        Color32::from_rgb(red, green, blue)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Preset(preset) => preset.name(),
            Self::Custom(_) => "Custom",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub app: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub overlay: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent_fill: Color32,
    pub accent_hover: Color32,
    pub accent_pressed: Color32,
    pub on_accent: Color32,
    pub accent_soft: Color32,
    pub accent_text: Color32,
    pub accent_border: Color32,
    /// Focus stays independent from the personalized accent.
    pub focus: Color32,
    /// Fixed cut-operation semantics, tone-adjusted only for readable text.
    pub kiss_text: Color32,
    pub perforation_text: Color32,
}

impl Palette {
    pub fn for_accent(dark_mode: bool, accent: Color32) -> Self {
        let (app, panel, surface, surface_hover, overlay, border, text, muted, focus) = if dark_mode
        {
            (
                INK,
                Color32::from_rgb(18, 19, 26),
                Color32::from_rgb(25, 27, 36),
                Color32::from_rgb(34, 37, 49),
                Color32::from_rgb(28, 32, 43),
                Color32::from_rgb(52, 56, 71),
                Color32::from_rgb(245, 247, 251),
                Color32::from_rgb(169, 176, 195),
                SECONDARY_CYAN,
            )
        } else {
            (
                Color32::from_rgb(244, 243, 248),
                Color32::from_rgb(252, 251, 253),
                Color32::WHITE,
                Color32::from_rgb(247, 245, 250),
                Color32::from_rgb(241, 239, 246),
                Color32::from_rgb(222, 218, 228),
                Color32::from_rgb(31, 27, 36),
                Color32::from_rgb(101, 95, 110),
                Color32::from_rgb(0, 103, 122),
            )
        };

        let on_accent = readable_foreground(accent);
        let brighten = if on_accent == INK {
            Color32::WHITE
        } else {
            INK
        };
        let deepen = if on_accent == INK {
            INK
        } else {
            Color32::WHITE
        };
        let accent_hover = mix(accent, brighten, 0.10);
        let pressed_candidate = mix(accent, deepen, 0.08);
        let accent_pressed = if contrast_ratio(on_accent, pressed_candidate) >= 4.5 {
            pressed_candidate
        } else {
            accent
        };
        let contrast_endpoint = if dark_mode { Color32::WHITE } else { INK };
        let accent_text = ensure_contrast(accent, panel, 4.5, contrast_endpoint);
        let accent_border = ensure_contrast(accent, panel, 3.0, contrast_endpoint);
        let accent_soft = mix(panel, accent, if dark_mode { 0.22 } else { 0.14 });
        let semantic_endpoint = if dark_mode { Color32::WHITE } else { INK };
        let kiss_text = ensure_contrast(KISS_GREEN, panel, 4.5, semantic_endpoint);
        let perforation_text = ensure_contrast(PERFORATION_RED, panel, 4.5, semantic_endpoint);

        Self {
            app,
            panel,
            surface,
            surface_hover,
            overlay,
            border,
            text,
            muted,
            accent_fill: accent,
            accent_hover,
            accent_pressed,
            on_accent,
            accent_soft,
            accent_text,
            accent_border,
            focus,
            kiss_text,
            perforation_text,
        }
    }

    pub fn for_dark_mode(dark_mode: bool) -> Self {
        Self::for_accent(dark_mode, DEFAULT_ACCENT)
    }
}

/// Apply Sapodilla's studio styling while preserving the user's light/dark preference.
pub fn apply(ctx: &Context, accent: Color32) {
    let mut style = (*ctx.style()).clone();
    let dark_mode = style.visuals.dark_mode;
    let palette = Palette::for_accent(dark_mode, accent);

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 32.0;
    style.spacing.icon_width = 18.0;
    style.spacing.icon_width_inner = 12.0;
    style.spacing.indent = 18.0;
    style.spacing.window_margin = Margin::same(16);
    style.spacing.menu_margin = Margin::same(8);
    style.animation_time = 0.12;

    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(20.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(14.0));

    let visuals = &mut style.visuals;
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.muted);
    visuals.panel_fill = palette.panel;
    visuals.window_fill = palette.overlay;
    visuals.extreme_bg_color = palette.app;
    visuals.text_edit_bg_color = Some(palette.surface);
    visuals.faint_bg_color = palette.surface_hover;
    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = stroke(1.0, palette.border);
    visuals.selection.bg_fill = palette.accent_soft;
    visuals.selection.stroke = stroke(1.5, palette.accent_border);
    visuals.hyperlink_color = palette.focus;
    visuals.slider_trailing_fill = true;
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    visuals.collapsing_header_frame = true;

    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(8);
    }
    visuals.widgets.noninteractive.bg_fill = palette.panel;
    visuals.widgets.noninteractive.weak_bg_fill = palette.panel;
    visuals.widgets.noninteractive.bg_stroke = stroke(1.0, palette.border);
    visuals.widgets.inactive.bg_fill = palette.surface;
    visuals.widgets.inactive.weak_bg_fill = palette.surface;
    visuals.widgets.inactive.bg_stroke = stroke(1.0, palette.border);
    visuals.widgets.hovered.bg_fill = palette.surface_hover;
    visuals.widgets.hovered.weak_bg_fill = palette.surface_hover;
    visuals.widgets.hovered.bg_stroke = stroke(1.5, palette.focus);
    visuals.widgets.active.bg_fill = palette.accent_soft;
    visuals.widgets.active.weak_bg_fill = palette.accent_soft;
    visuals.widgets.active.bg_stroke = stroke(2.0, palette.focus);
    visuals.widgets.open = visuals.widgets.active;

    ctx.set_style(style);
}

pub fn panel_frame(dark_mode: bool) -> Frame {
    let palette = Palette::for_dark_mode(dark_mode);
    Frame::new()
        .fill(palette.panel)
        .inner_margin(Margin::same(14))
}

pub fn card(dark_mode: bool) -> Frame {
    let palette = Palette::for_dark_mode(dark_mode);
    Frame::new()
        .fill(palette.surface)
        .stroke(stroke(1.0, palette.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(12))
}

pub fn spectrum_rule(ui: &mut Ui, accent: Color32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 3.0), Sense::hover());
    let accent_end = rect.left() + width * 0.62;
    let cyan_end = rect.left() + width * 0.82;
    ui.painter().rect_filled(
        egui::Rect::from_min_max(rect.min, egui::pos2(accent_end, rect.bottom())),
        2.0,
        accent,
    );
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(accent_end + 2.0, rect.top()),
            egui::pos2(cyan_end, rect.bottom()),
        ),
        2.0,
        SECONDARY_CYAN,
    );
    ui.painter().rect_filled(
        egui::Rect::from_min_max(egui::pos2(cyan_end + 2.0, rect.top()), rect.max),
        2.0,
        SIGNAL_LIME,
    );
}

pub fn primary_button(ui: &mut Ui, accent: Color32, text: impl Into<String>) -> egui::Response {
    primary_button_enabled(ui, accent, true, text)
}

pub fn primary_button_enabled(
    ui: &mut Ui,
    accent: Color32,
    enabled: bool,
    text: impl Into<String>,
) -> egui::Response {
    let palette = Palette::for_accent(ui.visuals().dark_mode, accent);
    let text = text.into();
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        if enabled {
            visuals.widgets.inactive.bg_fill = palette.accent_fill;
            visuals.widgets.inactive.weak_bg_fill = palette.accent_fill;
            visuals.widgets.inactive.bg_stroke = stroke(1.5, palette.accent_border);
            visuals.widgets.hovered.bg_fill = palette.accent_hover;
            visuals.widgets.hovered.weak_bg_fill = palette.accent_hover;
            visuals.widgets.hovered.bg_stroke = stroke(2.0, palette.focus);
            visuals.widgets.active.bg_fill = palette.accent_pressed;
            visuals.widgets.active.weak_bg_fill = palette.accent_pressed;
            visuals.widgets.active.bg_stroke = stroke(2.0, palette.focus);
        } else {
            visuals.widgets.inactive.bg_fill = palette.surface;
            visuals.widgets.inactive.weak_bg_fill = palette.surface;
            visuals.widgets.inactive.bg_stroke = stroke(1.0, palette.border);
        }
        ui.add_enabled(
            enabled,
            egui::Button::new(
                RichText::new(text)
                    .color(if enabled {
                        palette.on_accent
                    } else {
                        palette.muted
                    })
                    .strong(),
            )
            .corner_radius(CornerRadius::same(9))
            .min_size(egui::vec2(0.0, 36.0)),
        )
    })
    .inner
}

pub fn secondary_button(ui: &mut Ui, accent: Color32, text: impl Into<String>) -> egui::Response {
    secondary_button_enabled(ui, accent, true, text)
}

pub fn secondary_button_enabled(
    ui: &mut Ui,
    accent: Color32,
    enabled: bool,
    text: impl Into<String>,
) -> egui::Response {
    let palette = Palette::for_accent(ui.visuals().dark_mode, accent);
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        visuals.widgets.inactive.bg_fill = palette.surface;
        visuals.widgets.inactive.bg_stroke = stroke(1.0, palette.border);
        visuals.widgets.hovered.bg_fill = palette.accent_soft;
        visuals.widgets.hovered.bg_stroke = stroke(1.5, palette.focus);
        visuals.widgets.active.bg_fill = palette.accent_soft;
        visuals.widgets.active.bg_stroke = stroke(2.0, palette.focus);
        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(text.into()).color(if enabled {
                palette.text
            } else {
                palette.muted
            }))
            .corner_radius(CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 34.0)),
        )
    })
    .inner
}

pub fn danger_button(ui: &mut Ui, text: impl Into<String>) -> egui::Response {
    let palette = Palette::for_dark_mode(ui.visuals().dark_mode);
    let on_danger = readable_foreground(DANGER);
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        visuals.widgets.inactive.bg_fill = DANGER;
        visuals.widgets.inactive.bg_stroke = stroke(1.5, DANGER);
        visuals.widgets.hovered.bg_fill = mix(DANGER, Color32::WHITE, 0.10);
        visuals.widgets.hovered.bg_stroke = stroke(2.0, palette.focus);
        visuals.widgets.active.bg_fill = mix(DANGER, INK, 0.08);
        visuals.widgets.active.bg_stroke = stroke(2.0, palette.focus);
        ui.add(
            egui::Button::new(RichText::new(text.into()).color(on_danger).strong())
                .corner_radius(CornerRadius::same(8))
                .min_size(egui::vec2(0.0, 34.0)),
        )
    })
    .inner
}

pub fn toolbar_toggle(
    ui: &mut Ui,
    accent: Color32,
    selected: &mut bool,
    text: &str,
) -> egui::Response {
    let palette = Palette::for_accent(ui.visuals().dark_mode, accent);
    let is_selected = *selected;
    let response = ui
        .scope(|ui| {
            let visuals = &mut ui.style_mut().visuals;
            visuals.widgets.inactive.bg_fill = if is_selected {
                palette.accent_soft
            } else {
                palette.surface
            };
            visuals.widgets.inactive.bg_stroke = if is_selected {
                stroke(1.0, palette.border)
            } else {
                stroke(1.0, palette.border)
            };
            visuals.widgets.hovered.bg_fill = if is_selected {
                mix(palette.accent_soft, palette.surface_hover, 0.35)
            } else {
                palette.surface_hover
            };
            visuals.widgets.hovered.bg_stroke = stroke(1.5, palette.focus);
            visuals.widgets.active.bg_fill = palette.accent_soft;
            visuals.widgets.active.bg_stroke = stroke(2.0, palette.focus);
            ui.add(
                egui::Button::new(RichText::new(text).color(palette.text))
                    .selected(is_selected)
                    .corner_radius(CornerRadius::same(8))
                    .min_size(egui::vec2(0.0, 32.0)),
            )
        })
        .inner;
    if is_selected {
        let indicator = egui::Rect::from_min_max(
            egui::pos2(response.rect.left() + 7.0, response.rect.bottom() - 3.0),
            egui::pos2(response.rect.right() - 7.0, response.rect.bottom() - 1.0),
        );
        ui.painter()
            .rect_filled(indicator, 1.0, palette.accent_border);
    }
    if response.clicked() {
        *selected = !*selected;
    }
    response
}

pub fn accent_choice_button(
    ui: &mut Ui,
    active: AccentChoice,
    preset: AccentPreset,
) -> egui::Response {
    let selected = active == AccentChoice::Preset(preset);
    let candidate = preset.color();
    let palette = Palette::for_accent(ui.visuals().dark_mode, candidate);
    let marker = if selected { "[x]" } else { "[ ]" };
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        visuals.widgets.inactive.bg_fill = if selected {
            palette.accent_soft
        } else {
            palette.surface
        };
        visuals.widgets.inactive.bg_stroke = if selected {
            stroke(2.0, palette.accent_border)
        } else {
            stroke(1.5, palette.accent_border)
        };
        visuals.widgets.hovered.bg_fill = palette.accent_soft;
        visuals.widgets.hovered.bg_stroke = stroke(2.0, palette.focus);
        ui.add(
            egui::Button::new(format!("{marker} {}", preset.name()))
                .selected(selected)
                .corner_radius(CornerRadius::same(9))
                .min_size(egui::vec2(166.0, 40.0)),
        )
    })
    .inner
}

pub fn panel_title(ui: &mut Ui, accent: Color32, eyebrow: &str, title: &str) {
    let palette = Palette::for_accent(ui.visuals().dark_mode, accent);
    let (rail, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), Sense::hover());
    ui.painter().rect_filled(
        egui::Rect::from_min_max(rail.min, egui::pos2(rail.left() + 38.0, rail.bottom())),
        2.0,
        palette.accent_fill,
    );
    ui.painter().line_segment(
        [
            egui::pos2(rail.left() + 45.0, rail.center().y),
            egui::pos2(rail.right() - 10.0, rail.center().y),
        ],
        stroke(1.0, palette.border),
    );
    ui.painter().circle_filled(
        egui::pos2(rail.right() - 4.0, rail.center().y),
        3.0,
        palette.focus,
    );
    ui.label(
        RichText::new(eyebrow.to_uppercase())
            .size(10.0)
            .strong()
            .color(palette.accent_text),
    );
    ui.label(RichText::new(title).size(22.0).strong());
}

pub fn muted(ui: &mut Ui, text: impl Into<String>) -> egui::Response {
    let color = Palette::for_dark_mode(ui.visuals().dark_mode).muted;
    ui.label(RichText::new(text.into()).color(color))
}

pub fn status_badge(ui: &mut Ui, ready: bool, text: &str) -> egui::InnerResponse<()> {
    let dark_mode = ui.visuals().dark_mode;
    let (fill, color) = if ready {
        (SIGNAL_LIME, INK)
    } else {
        let palette = Palette::for_dark_mode(dark_mode);
        (palette.surface, palette.muted)
    };
    Frame::new()
        .fill(fill)
        .stroke(stroke(
            1.0,
            if ready {
                SIGNAL_LIME
            } else {
                palette_border(dark_mode)
            },
        ))
        .corner_radius(CornerRadius::same(99))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).strong().color(color));
        })
}

fn palette_border(dark_mode: bool) -> Color32 {
    Palette::for_dark_mode(dark_mode).border
}

fn stroke(width: f32, color: Color32) -> Stroke {
    Stroke::new(width, color)
}

fn mix(left: Color32, right: Color32, amount: f32) -> Color32 {
    let channel = |left: u8, right: u8| {
        (f32::from(left) + (f32::from(right) - f32::from(left)) * amount)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(
        channel(left.r(), right.r()),
        channel(left.g(), right.g()),
        channel(left.b(), right.b()),
    )
}

fn readable_foreground(background: Color32) -> Color32 {
    if contrast_ratio(INK, background) >= contrast_ratio(Color32::WHITE, background) {
        INK
    } else {
        Color32::WHITE
    }
}

fn ensure_contrast(
    color: Color32,
    background: Color32,
    minimum: f32,
    endpoint: Color32,
) -> Color32 {
    if contrast_ratio(color, background) >= minimum {
        return color;
    }
    for step in 1..=100 {
        let candidate = mix(color, endpoint, step as f32 / 100.0);
        if contrast_ratio(candidate, background) >= minimum {
            return candidate;
        }
    }
    endpoint
}

fn linear(channel: u8) -> f32 {
    let value = f32::from(channel) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(color: Color32) -> f32 {
    0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
}

pub(crate) fn contrast_ratio(left: Color32, right: Color32) -> f32 {
    let (bright, dark) = if luminance(left) > luminance(right) {
        (luminance(left), luminance(right))
    } else {
        (luminance(right), luminance(left))
    };
    (bright + 0.05) / (dark + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_derives_contrast_safe_roles_in_both_themes() {
        for preset in AccentPreset::ALL {
            assert_palette_contrast(preset.color());
        }
    }

    #[test]
    fn custom_boundary_colors_derive_contrast_safe_roles() {
        for rgb in [
            [0, 0, 0],
            [255, 255, 255],
            [127, 127, 127],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [250, 248, 252],
            [18, 19, 26],
        ] {
            assert_palette_contrast(Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        }
    }

    fn assert_palette_contrast(accent: Color32) {
        for dark_mode in [false, true] {
            let palette = Palette::for_accent(dark_mode, accent);
            assert!(contrast_ratio(palette.text, palette.panel) >= 4.5);
            assert!(contrast_ratio(palette.muted, palette.panel) >= 4.5);
            assert!(contrast_ratio(palette.accent_text, palette.panel) >= 4.5);
            assert!(contrast_ratio(palette.accent_border, palette.panel) >= 3.0);
            assert!(contrast_ratio(palette.focus, palette.surface) >= 3.0);
            assert!(contrast_ratio(palette.focus, palette.accent_soft) >= 3.0);
            assert!(contrast_ratio(palette.on_accent, palette.accent_fill) >= 4.5);
            assert!(contrast_ratio(palette.on_accent, palette.accent_hover) >= 4.5);
            assert!(contrast_ratio(palette.on_accent, palette.accent_pressed) >= 4.5);
            assert!(contrast_ratio(palette.kiss_text, palette.panel) >= 4.5);
            assert!(contrast_ratio(palette.perforation_text, palette.panel) >= 4.5);
        }
        assert!(contrast_ratio(INK, SIGNAL_LIME) >= 4.5);
    }
}
