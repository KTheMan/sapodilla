//! Semantic icon aliases for the Sapodilla interface.
//!
//! Keeping the visual vocabulary here prevents individual views from mixing
//! families or weights and makes future icon changes deliberate.

use egui_phosphor::regular as phosphor;

pub const ALIGN_BOTTOM: &str = phosphor::ALIGN_BOTTOM;
pub const ALIGN_CENTER_HORIZONTAL: &str = phosphor::ALIGN_CENTER_HORIZONTAL;
pub const ALIGN_CENTER_VERTICAL: &str = phosphor::ALIGN_CENTER_VERTICAL;
pub const ALIGN_LEFT: &str = phosphor::ALIGN_LEFT;
pub const ALIGN_RIGHT: &str = phosphor::ALIGN_RIGHT;
pub const ALIGN_TOP: &str = phosphor::ALIGN_TOP;
#[cfg(not(target_arch = "wasm32"))]
pub const ARROWS_CLOCKWISE: &str = phosphor::ARROWS_CLOCKWISE;
#[cfg(not(target_arch = "wasm32"))]
pub const CARET_LEFT: &str = phosphor::CARET_LEFT;
#[cfg(not(target_arch = "wasm32"))]
pub const CARET_RIGHT: &str = phosphor::CARET_RIGHT;
pub const DOTS_SIX_VERTICAL: &str = phosphor::DOTS_SIX_VERTICAL;
pub const DOTS_THREE: &str = phosphor::DOTS_THREE;
pub const EYE: &str = phosphor::EYE;
pub const EYE_SLASH: &str = phosphor::EYE_SLASH;
#[cfg(not(target_arch = "wasm32"))]
pub const FOLDER_PLUS: &str = phosphor::FOLDER_PLUS;
pub const GRID_NINE: &str = phosphor::GRID_NINE;
pub const LINK: &str = phosphor::LINK;
pub const LINK_BREAK: &str = phosphor::LINK_BREAK;
pub const LOCK: &str = phosphor::LOCK;
pub const LOCK_OPEN: &str = phosphor::LOCK_OPEN;
pub const SHUFFLE: &str = phosphor::SHUFFLE;
pub const TRASH: &str = phosphor::TRASH_SIMPLE;
pub const UPLOAD: &str = phosphor::UPLOAD_SIMPLE;

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_icons_are_single_private_use_glyphs() {
        for icon in [
            ALIGN_LEFT,
            ALIGN_CENTER_HORIZONTAL,
            ALIGN_RIGHT,
            EYE,
            LOCK,
            DOTS_THREE,
            UPLOAD,
        ] {
            let mut chars = icon.chars();
            let glyph = chars.next().expect("icon has a glyph");
            assert!(('\u{e000}'..='\u{f8ff}').contains(&glyph));
            assert!(chars.next().is_none());
        }
    }

    #[test]
    fn installed_font_contains_sidebar_glyphs() {
        let context = egui::Context::default();
        install(&context);
        let _ = context.run(egui::RawInput::default(), |_| {});
        context.fonts_mut(|fonts| {
            let font = egui::FontId::proportional(17.0);
            for icon in [EYE, LOCK, DOTS_THREE, ALIGN_BOTTOM] {
                assert!(fonts.has_glyph(&font, icon.chars().next().unwrap()));
            }
        });
    }
}
