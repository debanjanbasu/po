//! Theme tokens ported from `shared/theme.css` (daisyUI "bumblebee" /
//! "bumblebee-dark") to gpui colors.
//!
//! The palette is ONE warm family — rust 35 / orange 55 / tan 62 / amber 78 /
//! gold 92 on the hue wheel, error the sole exception at red 25 — with chroma
//! held inside sRGB at each role's lightness so fills stay saturated instead
//! of mud. Lightness separates roles: text-bearing roles (primary, warning)
//! are pinned bright enough to read as text, fill-only roles (accent,
//! neutral) sit darker/deeper. Every `-content` clears 4.8:1 on its fill in
//! both themes; in dark every role also clears 6.3:1 as page text.
//!
//! Source values were `oklch(L% C H)` in CSS, converted to sRGB hex
//! (gamut-clamped) at port time — the hexes below are the verified results.
//! Hex → `Hsla` goes through a const RGB→HSL conversion because gpui's
//! `rgb_to_hsla` is not `const` and no `From<Rgba> for Hsla` exists; `hsla()`
//! is const and takes h/s/l in 0..1.

use gpui::{App, Hsla, WindowAppearance, hsla};

/// Shape tokens carried over from the theme blocks.
pub mod shape {
    use gpui::{Pixels, px};

    /// `--radius-selector` / `--radius-box` (cards, modals, avatars).
    pub const RADIUS_BOX: Pixels = px(16.0);
    /// `--radius-field` (inputs, buttons).
    pub const RADIUS_FIELD: Pixels = px(8.0);
    /// `--border` width.
    pub const BORDER: Pixels = px(1.0);
}

/// 0xRRGGBB → opaque `Hsla`, const-evaluable.
const fn hsl_of(hex: u32) -> Hsla {
    // `u8 as f32` is exact (8-bit values fit the f32 mantissa); the mask
    // guarantees the `u32` fits `u8`, so the double cast loses nothing.
    #[allow(clippy::cast_lossless)] // u32 -> u8 narrowing is the point
    let red = (((hex >> 16) & 0xFF) as u8) as f32 / 255.0;
    #[allow(clippy::cast_lossless)]
    let green = (((hex >> 8) & 0xFF) as u8) as f32 / 255.0;
    #[allow(clippy::cast_lossless)]
    let blue = ((hex & 0xFF) as u8) as f32 / 255.0;

    let max = if red > green {
        if red > blue { red } else { blue }
    } else {
        if green > blue { green } else { blue }
    };
    let min = if red < green {
        if red < blue { red } else { blue }
    } else {
        if green < blue { green } else { blue }
    };
    let delta = max - min;

    // Hue in 0..1 (hsla() scales by 360 internally). The equality
    // comparisons are exact by construction: `max` IS one of the three
    // channel values, never a computed float.
    #[allow(clippy::float_cmp)]
    let hue = if delta == 0.0 {
        0.0
    } else if max == red {
        let shift = (green - blue) / delta;
        if shift < 0.0 {
            shift / 6.0 + 1.0
        } else {
            shift / 6.0
        }
    } else if max == green {
        (blue - red) / delta / 6.0 + 1.0 / 3.0
    } else {
        (red - green) / delta / 6.0 + 2.0 / 3.0
    };

    // Lightness: mid of min/max. Saturation: delta over 1 - |2L - 1|.
    let lightness = f32::midpoint(max, min);
    let saturation = if delta == 0.0 {
        0.0
    } else {
        delta / (1.0 - 2.0f32.mul_add(lightness, -1.0).abs())
    };

    hsla(hue, saturation, lightness, 1.0)
}

/// One color role: a fill plus the readable color on top of it.
#[derive(Clone, Copy)]
pub struct Role {
    /// The role's fill color.
    pub fill: Hsla,
    /// The readable color on top of `fill`.
    pub content: Hsla,
}

const fn role(fill: u32, content: u32) -> Role {
    Role {
        fill: hsl_of(fill),
        content: hsl_of(content),
    }
}

/// The full token set for one appearance.
#[derive(Clone, Copy)]
pub struct Theme {
    /// Page background.
    pub base_100: Hsla,
    /// Slightly raised surface (cards, inputs).
    pub base_200: Hsla,
    /// Strongly raised surface (hover, wells).
    pub base_300: Hsla,
    /// Default text on `base_*`.
    pub base_content: Hsla,

    /// Brand action color — also readable as text (7 text sites : 1 fill).
    pub primary: Role,
    /// Secondary actions (warm rust-red).
    pub secondary: Role,
    /// Fill-only accent (avatar circles).
    pub accent: Role,
    /// Fill-only neutral (chips, scrollbars).
    pub neutral: Role,

    pub info: Role,
    pub success: Role,
    /// Warning is text-only (`text-warning`, 10 sites, no fills).
    pub warning: Role,
    /// Error — the only cool hue; must read as "failure".
    pub error: Role,
}

/// `bumblebee` — warm light theme (daisyUI default).
// Color tables read as RGB hex: `0xf18428` is the token value, not a number.
#[allow(clippy::unreadable_literal)]
pub const LIGHT: Theme = Theme {
    base_100: hsl_of(0xffffff),
    base_200: hsl_of(0xf9f2ed),
    base_300: hsl_of(0xe8dbd3),
    base_content: hsl_of(0x211813),
    primary: role(0xf18428, 0x1b0e06),
    secondary: role(0xc83e19, 0xfffaf8),
    accent: role(0xf9d6bf, 0x290c00),
    neutral: role(0x4a392e, 0xf2e9e4),
    info: role(0xce7c22, 0x1a0f06),
    success: role(0xdb9e2a, 0x171006),
    warning: role(0xb59525, 0x151106),
    error: role(0xd9232e, 0xfffaf9),
};

/// `bumblebee-dark` — the same family on near-black; polarity flipped so
/// text-bearing roles are LIGHT and take dark `-content`.
#[allow(clippy::unreadable_literal)]
pub const DARK: Theme = Theme {
    base_100: hsl_of(0x100c0a),
    base_200: hsl_of(0x1f1915),
    base_300: hsl_of(0x342c26),
    base_content: hsl_of(0xebe3dd),
    primary: role(0xf99447, 0x1b0e06),
    secondary: role(0xf86d4b, 0x200a06),
    accent: role(0xffddc7, 0x290c00),
    neutral: role(0x3f2f24, 0xefe6e0),
    info: role(0xf09129, 0x1a0f06),
    success: role(0xebaa2d, 0x171006),
    warning: role(0xe1ba2f, 0x151106),
    error: role(0xf84c4c, 0x220807),
};

impl Theme {
    /// The theme for the app's current appearance. Mirrors the old "system"
    /// default of the `THEME` global signal: follow the OS; a user override
    /// goes through `App::set_window_appearance` on top. Live re-skin: an
    /// entity observes `observe_window_appearance` and re-renders.
    // `App` here is the context type, not a structure-name repetition — the
    // lint misfires on the type path, so it is allowed explicitly.
    pub fn for_appearance(cx: &App) -> &'static Self {
        match cx.window_appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => &DARK,
            WindowAppearance::Light | WindowAppearance::VibrantLight => &LIGHT,
        }
    }
}

/// Semantic aliases used by widgets, so call sites never touch raw roles.
impl Theme {
    /// Input background.
    pub const fn input_bg(&self) -> Hsla {
        self.base_200
    }

    /// Input border (default state).
    pub const fn input_border(&self) -> Hsla {
        self.base_300
    }

    /// Muted secondary text (hints, timestamps).
    pub const fn text_dim(&self) -> Hsla {
        self.neutral.fill
    }

    /// `FeedbackKind::Error` text color.
    pub const fn error_text(&self) -> Hsla {
        self.error.fill
    }

    /// `FeedbackKind::Success` text color.
    pub const fn success_text(&self) -> Hsla {
        self.success.fill
    }

    /// `FeedbackKind::Warn` text color.
    pub const fn warn_text(&self) -> Hsla {
        self.warning.fill
    }
}
