//! The window the OS hands us, per platform.
//!
//! The app is distraction-free by default, draws edge to edge, and looks the
//! same everywhere — it does not adopt a platform's window furniture. On
//! mobile that means the system bars are hidden (`native/android/MainActivity.kt`,
//! `native/apple/Info.ios.plist`, and [`ios_window`] below); on desktop the
//! window is **borderless** and the page draws the band, the caption buttons
//! and the drag strip itself (`components::shared::window_chrome`, geometry in
//! `overrides.css`). This module owns the native half: what each platform's
//! window must stop drawing for that to be true.
//!
//! Every desktop gets the same result, but not through the same flag, because
//! what a window can give up differs:
//!
//! - **Windows / Linux**: there is no menu bar to lose, so the decorations go
//!   entirely. Windows keeps `WS_SIZEBOX` and tao answers `WM_NCHITTEST`
//!   itself, so the window still resizes from its (invisible) frame; a Linux
//!   window has no such frame left, which is why the page draws resize edges
//!   there. Windows could also keep a DWM shadow
//!   (`WindowBuilderExtWindows::with_undecorated_shadow`), but that variant
//!   insets the client area by the shadow margins and leaves only the top edge
//!   hit-testable, so the app would no longer fill the window it draws.
//! - **macOS**: the style mask stays and only what it *draws* is switched off —
//!   title hidden, title bar transparent, content view full size, traffic
//!   lights hidden. A borderless `NSWindow` instead would also lose the app
//!   menu (Dioxus drops the menu bar when the decorations are off) and with it
//!   ⌘Q, ⌘W and the Edit menu's cut/copy/paste, which is not a trade a
//!   text-heavy messenger should make. Keeping the mask also keeps the
//!   window's shadow and its native resize from the edges.

#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
// `with_title_hidden`, `with_titlebar_transparent`, `with_fullsize_content_view`
// and `with_titlebar_buttons_hidden` are macOS-only extension methods.
#[cfg(all(feature = "desktop", target_os = "macos"))]
use dioxus::desktop::tao::platform::macos::WindowBuilderExtMacOS;

/// Native window title.
///
/// With the title bar gone this is what the task bar, Alt-Tab and Mission
/// Control show. It matches the bundle name (`CFBundleDisplayName` /
/// `CFBundleName` in the Apple plists), the Android label, and the web
/// bundle's `<title>` (`[web.app].title` in `Dioxus.toml`).
#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
const WINDOW_TITLE: &str = "po";

/// The smallest window the shell still lays out in. The layout is
/// mobile-first, so the floor is a phone-shaped window rather than a desktop
/// one.
#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
const MIN_INNER_SIZE: LogicalSize<f64> = LogicalSize::new(360.0, 520.0);

/// The desktop app's window: borderless, with a phone-shaped floor.
///
/// Passed to `LaunchBuilder::with_cfg` in `main`. The app has exactly one
/// window, so this is the only place that builds one.
#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
#[must_use]
pub fn desktop_config() -> Config {
    Config::new().with_window(desktop_window())
}

#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
fn desktop_window() -> WindowBuilder {
    let window = WindowBuilder::new()
        .with_title(WINDOW_TITLE)
        .with_min_inner_size(MIN_INNER_SIZE);

    // `dx` floats a dev build's window so it stays visible while you work;
    // replacing the builder would drop that, so it is repeated here from the
    // same source (absent from a release bundle, where the default applies).
    #[cfg(debug_assertions)]
    let window = window.with_always_on_top(dioxus::cli_config::always_on_top().unwrap_or(true));

    // Keep the style mask, drop everything it draws — including the traffic
    // lights, so the page's caption cluster is the window's only one. The
    // menu bar survives because the decorations do; see the module docs.
    #[cfg(target_os = "macos")]
    let window = window
        .with_title_hidden(true)
        .with_titlebar_transparent(true)
        .with_fullsize_content_view(true)
        .with_titlebar_buttons_hidden(true);

    // No menu bar to lose here, so the frame goes entirely.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    let window = window.with_decorations(false);

    window
}

#[cfg(all(target_os = "ios", feature = "mobile"))]
use dioxus::mobile::{WindowBuilder, tao::platform::ios::WindowBuilderExtIOS};

/// The iOS window: nothing of the system's is left showing.
///
/// The status bar is the bundle's decision (`UIStatusBarHidden` plus
/// `UIViewControllerBasedStatusBarAppearance=false` in
/// `native/apple/Info.ios.plist`), which is the app-level pair iOS honours
/// under the scene lifecycle. The home indicator has no `Info.plist` key at
/// all — it is a view-controller preference — so it is set on the window tao
/// builds instead. Both are preferences rather than states: the system hides
/// the indicator a moment after the last touch and brings it back on a swipe,
/// and a foldable's inner or cover display follows the same rule.
#[cfg(all(target_os = "ios", feature = "mobile"))]
#[must_use]
pub fn ios_window() -> WindowBuilder {
    WindowBuilder::new().with_prefers_home_indicator_hidden(true)
}
