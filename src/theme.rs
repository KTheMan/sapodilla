use egui::{
    Color32, Context, CornerRadius, FontId, Frame, Margin, RichText, Stroke, TextStyle, Ui,
};

pub const ACCENT: Color32 = Color32::from_rgb(255, 45, 170);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(255, 90, 190);
pub const SECONDARY_CYAN: Color32 = Color32::from_rgb(39, 221, 235);
pub const SIGNAL_LIME: Color32 = Color32::from_rgb(216, 255, 57);
pub const INK: Color32 = Color32::from_rgb(11, 11, 16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub app: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent_soft: Color32,
    /// Accent suitable for small text on the current panel surface.
    pub accent_text: Color32,
    /// Non-text focus/active outline with at least 3:1 surface contrast.
    pub focus: Color32,
}

impl Palette {
    pub fn for_dark_mode(dark_mode: bool) -> Self {
        if dark_mode {
            Self {
                app: INK,
                panel: Color32::from_rgb(18, 19, 26),
                surface: Color32::from_rgb(25, 27, 36),
                surface_hover: Color32::from_rgb(34, 37, 49),
                border: Color32::from_rgb(52, 56, 71),
                text: Color32::from_rgb(245, 247, 251),
                muted: Color32::from_rgb(169, 176, 195),
                accent_soft: Color32::from_rgb(65, 23, 52),
                accent_text: ACCENT,
                focus: SECONDARY_CYAN,
            }
        } else {
            Self {
                app: Color32::from_rgb(244, 243, 248),
                panel: Color32::from_rgb(252, 251, 253),
                surface: Color32::WHITE,
                surface_hover: Color32::from_rgb(247, 245, 250),
                border: Color32::from_rgb(222, 218, 228),
                text: Color32::from_rgb(31, 27, 36),
                muted: Color32::from_rgb(101, 95, 110),
                accent_soft: Color32::from_rgb(255, 225, 243),
                accent_text: Color32::from_rgb(166, 0, 96),
                focus: Color32::from_rgb(0, 103, 122),
            }
        }
    }
}

/// Apply Sapodilla's product styling while preserving the user's light/dark preference.
pub fn apply(ctx: &Context) {
    let mut style = (*ctx.style()).clone();
    let dark_mode = style.visuals.dark_mode;
    let palette = Palette::for_dark_mode(dark_mode);

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
    visuals.window_fill = palette.panel;
    visuals.extreme_bg_color = palette.app;
    visuals.text_edit_bg_color = Some(palette.surface);
    visuals.faint_bg_color = palette.surface_hover;
    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.selection.bg_fill = palette.accent_soft;
    visuals.selection.stroke = Stroke::new(1.5_f32, palette.text);
    visuals.hyperlink_color = if dark_mode {
        Color32::from_rgb(196, 181, 253)
    } else {
        Color32::from_rgb(91, 33, 182)
    };
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
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.widgets.inactive.bg_fill = palette.surface;
    visuals.widgets.inactive.weak_bg_fill = palette.surface;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.widgets.hovered.bg_fill = palette.surface_hover;
    visuals.widgets.hovered.weak_bg_fill = palette.surface_hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.widgets.active.bg_fill = palette.accent_soft;
    visuals.widgets.active.weak_bg_fill = palette.accent_soft;
    visuals.widgets.active.bg_stroke = Stroke::new(2.0_f32, palette.focus);
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
        .stroke(Stroke::new(1.0_f32, palette.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(12))
}

pub fn primary_button(ui: &mut Ui, text: impl Into<String>) -> egui::Response {
    primary_button_enabled(ui, true, text)
}

pub fn primary_button_enabled(
    ui: &mut Ui,
    enabled: bool,
    text: impl Into<String>,
) -> egui::Response {
    let palette = Palette::for_dark_mode(ui.visuals().dark_mode);
    let (fill, stroke, text_color) = if enabled {
        (ACCENT, ACCENT_HOVER, INK)
    } else {
        (palette.surface, palette.border, palette.muted)
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text.into()).color(text_color).strong())
            .fill(fill)
            .stroke(Stroke::new(1.0_f32, stroke))
            .corner_radius(CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 36.0)),
    )
}

pub fn panel_title(ui: &mut Ui, eyebrow: &str, title: &str) {
    let accent_text = Palette::for_dark_mode(ui.visuals().dark_mode).accent_text;
    ui.label(
        RichText::new(eyebrow.to_uppercase())
            .size(10.0)
            .strong()
            .color(accent_text),
    );
    ui.label(RichText::new(title).size(21.0).strong());
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
        .corner_radius(CornerRadius::same(99))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).strong().color(color));
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn contrast(left: Color32, right: Color32) -> f32 {
        let (bright, dark) = if luminance(left) > luminance(right) {
            (luminance(left), luminance(right))
        } else {
            (luminance(right), luminance(left))
        };
        (bright + 0.05) / (dark + 0.05)
    }

    #[test]
    fn body_and_primary_action_colors_meet_wcag_aa_contrast() {
        for dark_mode in [false, true] {
            let palette = Palette::for_dark_mode(dark_mode);
            assert!(contrast(palette.text, palette.panel) >= 4.5);
            assert!(contrast(palette.muted, palette.panel) >= 4.5);
            assert!(contrast(palette.accent_text, palette.panel) >= 4.5);
            assert!(contrast(palette.focus, palette.surface) >= 3.0);
            assert!(contrast(palette.focus, palette.accent_soft) >= 3.0);
        }
        assert!(contrast(INK, ACCENT) >= 4.5);
        assert!(contrast(INK, SIGNAL_LIME) >= 4.5);
        assert!(contrast(SECONDARY_CYAN, Palette::for_dark_mode(true).app) >= 3.0);
    }
}
