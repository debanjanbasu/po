//! Bundled IBM 3270 typography.
//!
//! The font ships inside the binary, so po needs no system font installation.
//! We register the family with GPUI's text system *before* the first window
//! opens; afterwards `font_family("IBM 3270")` resolves everywhere.

use std::borrow::Cow;

use gpui::App;

/// The bundled IBM 3270 typeface (TTF build of rbanffy/3270font v3.0.1).
pub const IBM3270_TTF: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/fonts/3270-Regular.ttf"));

/// The font family name as recorded in the font's name table (nameID 1).
pub const IBM3270_FAMILY: &str = "IBM 3270";

/// Register the bundled font with GPUI.
///
/// Must be called before the first window is opened.
///
/// # Errors
/// Returns an error when the platform text system rejects the font data.
pub fn load_fonts(cx: &mut App) -> gpui::Result<()> {
    cx.text_system()
        .add_fonts(vec![Cow::Borrowed(IBM3270_TTF)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_font_is_a_real_ttf() {
        // TrueType files start with the 0x00010000 sfnt version.
        assert_eq!(&IBM3270_TTF[..4], &[0x00, 0x01, 0x00, 0x00]);
        assert!(IBM3270_TTF.len() > 100_000);
    }
}
