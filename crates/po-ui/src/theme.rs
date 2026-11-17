//! po's colour theme.
//!
//! The palette is a direct port of the orange "bumblebee" themes from the
//! sibling `oo` project: the same OKLCH values, converted to sRGB at runtime
//! with a small, dependency-free implementation. The result is a warm amber
//! interface on near-black surfaces — retro, noir, and legible.

use gpui::{Hsla, SharedString, rgba};

/// Lightness of an OKLCH colour, 0..=1.
pub const fn oklch(lightness: f32, chroma: f32, hue: f32) -> Oklch {
    Oklch {
        lightness,
        chroma,
        hue,
    }
}

/// A colour in the OKLCH space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    /// Perceptual lightness, 0..=1.
    pub lightness: f32,
    /// Chroma (colourfulness); 0 is grey.
    pub chroma: f32,
    /// Hue angle in degrees.
    pub hue: f32,
}

impl Oklch {
    /// Convert to a packed `0xRRGGBB` sRGB value.
    ///
    /// # Panics
    /// Panics if the value is out of range (a programming error, not user input).
    #[must_use]
    pub fn to_rgb(self) -> u32 {
        let (r, g, b) = self.to_srgb();
        let to_byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        (to_byte(r) << 16) | (to_byte(g) << 8) | to_byte(b)
    }

    /// Convert to sRGB components in the 0..=1 range.
    #[must_use]
    pub fn to_srgb(self) -> (f32, f32, f32) {
        let hue = self.hue.to_radians();
        let a = self.chroma * hue.cos();
        let b = self.chroma * hue.sin();

        let l_ = self.lightness + 0.396_337_8 * a + 0.215_803_8 * b;
        let m_ = self.lightness - 0.105_561_4 * a - 0.063_854_2 * b;
        let s_ = self.lightness - 0.089_484_2 * a - 1.291_485_5 * b;

        let l = l_ * l_ * l_;
        let m = m_ * m_ * m_;
        let s = s_ * s_ * s_;

        let r = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_9 * s;
        let g = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s;
        let bl = -0.004_196_1 * l - 0.703_418_6 * m + 1.707_614_7 * s;

        (encode_srgb(r), encode_srgb(g), encode_srgb(bl))
    }

    /// Convert to GPUI's colour type.
    #[must_use]
    pub fn to_hsla(self) -> Hsla {
        gpui::rgb_to_hsla(rgba(self.to_rgb()))
    }
}

/// Apply the sRGB transfer function to a linear-light component.
fn encode_srgb(value: f32) -> f32 {
    let v = value.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Which palette is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    /// Light palette.
    Light,
    /// Dark palette (the default: noir).
    Dark,
}

impl ThemeMode {
    /// The opposite mode.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }

    /// Parse a mode name, accepting `toggle`.
    #[must_use]
    pub fn parse(value: &str, current: Self) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "toggle" => Some(current.toggled()),
            _ => None,
        }
    }
}

/// po's colour palette.
#[derive(Debug, Clone)]
pub struct PoTheme {
    /// Which palette this is.
    pub mode: ThemeMode,
    /// The font family used across the interface.
    pub font_family: SharedString,
    /// Page background.
    pub base_100: Hsla,
    /// Slightly raised background.
    pub base_200: Hsla,
    /// Borders and separators.
    pub base_300: Hsla,
    /// Default text colour.
    pub content: Hsla,
    /// Primary accent — amber.
    pub primary: Hsla,
    /// Text drawn on the primary accent.
    pub primary_content: Hsla,
    /// Secondary accent — burnt orange.
    pub secondary: Hsla,
    /// Text drawn on the secondary accent.
    pub secondary_content: Hsla,
    /// Soft highlight.
    pub accent: Hsla,
    /// Text drawn on the soft highlight.
    pub accent_content: Hsla,
    /// Neutral surfaces.
    pub neutral: Hsla,
    /// Text drawn on neutral surfaces.
    pub neutral_content: Hsla,
    /// Informational colour.
    pub info: Hsla,
    /// Success colour.
    pub success: Hsla,
    /// Warning colour.
    pub warning: Hsla,
    /// Error colour.
    pub error: Hsla,
}

impl PoTheme {
    /// The dark ("noir") palette.
    #[must_use]
    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            font_family: SharedString::from(FONT_FAMILY),
            base_100: oklch(0.16, 0.008, 55.0).to_hsla(),
            base_200: oklch(0.22, 0.012, 55.0).to_hsla(),
            base_300: oklch(0.30, 0.016, 55.0).to_hsla(),
            content: oklch(0.92, 0.012, 55.0).to_hsla(),
            primary: oklch(0.76, 0.152, 55.0).to_hsla(),
            primary_content: oklch(0.18, 0.027, 55.0).to_hsla(),
            secondary: oklch(0.70, 0.178, 35.0).to_hsla(),
            secondary_content: oklch(0.18, 0.039, 35.0).to_hsla(),
            accent: oklch(0.92, 0.048, 55.0).to_hsla(),
            accent_content: oklch(0.20, 0.06, 55.0).to_hsla(),
            neutral: oklch(0.32, 0.030, 55.0).to_hsla(),
            neutral_content: oklch(0.93, 0.012, 55.0).to_hsla(),
            info: oklch(0.74, 0.157, 62.0).to_hsla(),
            success: oklch(0.78, 0.150, 78.0).to_hsla(),
            warning: oklch(0.80, 0.151, 92.0).to_hsla(),
            error: oklch(0.66, 0.208, 25.0).to_hsla(),
        }
    }

    /// The light palette.
    #[must_use]
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            font_family: SharedString::from(FONT_FAMILY),
            base_100: oklch(1.0, 0.0, 0.0).to_hsla(),
            base_200: oklch(0.965, 0.010, 55.0).to_hsla(),
            base_300: oklch(0.90, 0.018, 55.0).to_hsla(),
            content: oklch(0.22, 0.018, 55.0).to_hsla(),
            primary: oklch(0.72, 0.164, 55.0).to_hsla(),
            primary_content: oklch(0.18, 0.027, 55.0).to_hsla(),
            secondary: oklch(0.56, 0.181, 35.0).to_hsla(),
            secondary_content: oklch(0.99, 0.008, 35.0).to_hsla(),
            accent: oklch(0.90, 0.050, 55.0).to_hsla(),
            accent_content: oklch(0.20, 0.06, 55.0).to_hsla(),
            neutral: oklch(0.36, 0.030, 55.0).to_hsla(),
            neutral_content: oklch(0.94, 0.012, 55.0).to_hsla(),
            info: oklch(0.66, 0.140, 62.0).to_hsla(),
            success: oklch(0.74, 0.142, 78.0).to_hsla(),
            warning: oklch(0.68, 0.128, 92.0).to_hsla(),
            error: oklch(0.57, 0.213, 25.0).to_hsla(),
        }
    }

    /// The palette for a given mode.
    #[must_use]
    pub fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Light => Self::light(),
            ThemeMode::Dark => Self::dark(),
        }
    }

}

/// The font family used across the interface.
pub const FONT_FAMILY: &str = crate::font::IBM3270_FAMILY;


impl PoTheme {
    /// Install the theme as a GPUI global on `app`.
    pub fn install(app: &mut gpui::App, mode: ThemeMode) {
        app.set_global(Self::for_mode(mode));
    }

    /// Read the current theme from any context.
    pub fn current(cx: &gpui::App) -> &Self {
        cx.global::<Self>()
    }

    /// Flip to the other palette and return the new theme.
    pub fn toggled(&self) -> Self {
        Self::for_mode(self.mode.toggled())
    }
}

impl gpui::Global for PoTheme {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oklch_greys_are_neutral() {
        let (r, g, b) = oklch(0.5, 0.0, 0.0).to_srgb();
        assert!((r - g).abs() < 0.01);
        assert!((g - b).abs() < 0.01);
    }

    #[test]
    fn oklch_white_is_white_and_black_is_black() {
        assert_eq!(oklch(1.0, 0.0, 0.0).to_rgb(), 0x00FF_FFFF);
        assert_eq!(oklch(0.0, 0.0, 0.0).to_rgb(), 0x0000_0000);
    }

    #[test]
    fn primary_is_amber_in_both_palettes() {
        let dark = PoTheme::dark();
        let light = PoTheme::light();
        // Amber sits around #f7a325 / #eb8a1a; assert it is warm: red > blue.
        let (dr, _, db) = oklch(0.76, 0.152, 55.0).to_srgb();
        assert!(dr > db, "dark primary should be warm");
        let (lr, _, lb) = oklch(0.72, 0.164, 55.0).to_srgb();
        assert!(lr > lb, "light primary should be warm");
        assert_ne!(dark.primary, light.primary);
    }

    #[test]
    fn dark_base_is_near_black() {
        let (r, g, b) = oklch(0.16, 0.008, 55.0).to_srgb();
        assert!(r < 0.1 && g < 0.1 && b < 0.1, "expected near black: {r} {g} {b}");
    }

    #[test]
    fn mode_parsing_supports_toggle() {
        assert_eq!(ThemeMode::parse("light", ThemeMode::Dark), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::parse("toggle", ThemeMode::Light), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::parse("nope", ThemeMode::Dark), None);
    }
}
