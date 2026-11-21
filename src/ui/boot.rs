//! Boot pipeline shared by every target's entry: font registration,
//! deep-link intake, and the one main window's options.
//!
//! Extracted from the Dioxus launch edge (`src/main.rs` + `src/platform/window.rs`)
//! during the gpui-ce migration; the Dioxus entry keeps building until the
//! Phase 7 cutover deletes it.

use gpui::{
    App, SharedString, TitlebarOptions, WindowAppearance, WindowDecorations, WindowOptions, point,
    px, size,
};
use tracing::warn;

/// The 3270 family (rbanffy/3270font), loaded into the text system at boot.
/// TTF/OTF only — gpui does not rasterize WOFF2, so the old `FONT_WOFF2`
/// asset dies with the Dioxus entry (its `@font-face` lived in the CSS
/// pipeline). The whole UI renders in this family: the regular face for
/// prose, the condensed/semi-condensed faces for dense layouts.
pub static FONT_3270: &[u8] = include_bytes!("../../assets/fonts/3270-Regular.ttf");
pub static FONT_3270_CONDENSED: &[u8] =
    include_bytes!("../../assets/fonts/3270Condensed-Regular.ttf");
pub static FONT_3270_SEMI_CONDENSED: &[u8] =
    include_bytes!("../../assets/fonts/3270SemiCondensed-Regular.ttf");

/// Registers the app's fonts. Web starts with an empty font database, so
/// this must run before the first window opens on every target.
pub fn load_fonts(cx: &App) {
    let fonts = vec![
        std::borrow::Cow::Borrowed(FONT_3270),
        std::borrow::Cow::Borrowed(FONT_3270_CONDENSED),
        std::borrow::Cow::Borrowed(FONT_3270_SEMI_CONDENSED),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        warn!("failed to load application fonts: {error:#}");
    }
}

/// Routes URLs the OS hands the running app into the deep-link pump. Called
/// on the [`gpui::Application`] *before* `run` (the handler lives on the
/// platform, not on `App`). Apple cold URLs are captured before launch by
/// `platform::deep_link::bootstrap_apple_url_handler` and surface here too.
pub fn on_open_urls(application: &gpui::Application) {
    application.on_open_urls(|urls| {
        for url in urls {
            crate::platform::deep_link::ingest_url(&url);
        }
    });
}

/// Registers the `oo` scheme at runtime so the OS routes such links to the
/// app (macOS/Windows; the config module holds the per-action URL shapes).
pub fn register_scheme(cx: &App) {
    cx.register_url_scheme("po").detach();
}

/// Window options for the one main window: full options on desktop targets,
/// defaults on web (canvas follows the viewport; bounds/title are ignored).
pub fn window_options() -> gpui::WindowOptions {
    #[cfg(not(target_arch = "wasm32"))]
    return desktop_window_options();
    #[cfg(target_arch = "wasm32")]
    return gpui::WindowOptions::default();
}

/// The one desktop window: borderless, phone-shaped floor, page-drawn chrome.
///
/// Mapping from the old `platform::window::desktop_window()`:
/// - `WINDOW_TITLE "po"`      → `TitlebarOptions::title` (task bar / Alt-Tab)
/// - `MIN_INNER_SIZE 360×520` → `window_min_size` (same logical floor)
/// - macOS keep-style-mask family → `appears_transparent(true)` (bar hidden;
///   menu bar + ⌘Q/⌘W survive — gpui has no hide-traffic-lights knob, so the
///   lights stay and sit inside the app's chrome band)
/// - Windows/Linux `decorations(false)` → `WindowDecorations::Client`
/// - the page's drag strip → `app_owns_titlebar_drag(true)` + `Window::
///   start_window_move` in the ported window chrome widget
/// - dx's dev-window float (`always_on_top`) → deleted (dx-only concept)
#[cfg(not(target_arch = "wasm32"))]
pub fn desktop_window_options() -> WindowOptions {
    let titlebar = TitlebarOptions::default()
        .title(Some(SharedString::from("po")))
        .appears_transparent(true)
        // Traffic lights centered in the chrome band the page draws.
        .traffic_light_position(Some(point(px(16.0), px(14.0))));

    WindowOptions {
        titlebar: Some(titlebar),
        window_min_size: Some(size(px(360.0), px(520.0))),
        window_bounds: None, // the OS places it, as the old builder did
        is_resizable: true,
        is_minimizable: true,
        app_owns_titlebar_drag: true,
        window_decorations: Some(WindowDecorations::Client),
        ..WindowOptions::default()
    }
}

/// The theme override a future settings toggle will write: `Some(LIGHT)` /
/// `Some(DARK)` forces it, `None` follows the system (today's "system"
/// default of the `THEME` global).
pub fn set_theme_override(cx: &App, dark: Option<bool>) {
    cx.set_window_appearance(dark.map(|d| {
        if d {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        }
    }));
}
