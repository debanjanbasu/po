// Veilid's generic async transaction pipeline exceeds rustc's default
// trait-solver recursion budget; 256 is sufficient once app fan-out stays local.
#![recursion_limit = "256"]
#![feature(try_blocks)]

//! Application entry point. Boots the Dioxus runtime and global state.

mod app;
mod chat;
mod components;
mod compression;
pub(crate) mod config;
mod global_state;
mod helpers;
mod models;
pub(crate) mod platform;
mod router;
mod session;
mod storage;
mod sync;
mod user;
mod veilid;

use app::shell::App;
#[cfg(not(any(target_arch = "wasm32", all(target_os = "ios", feature = "mobile"))))]
use dioxus::logger::tracing::warn;
#[cfg(target_arch = "wasm32")]
use dioxus::logger::{init, tracing::Level};
#[cfg(all(target_os = "ios", feature = "mobile"))]
use dioxus::mobile::{Config, tao::event_loop::EventLoopBuilder};
use dioxus::prelude::*;
#[cfg(any(target_os = "ios", target_os = "macos"))]
use platform::deep_link::bootstrap_apple_url_handler;
#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
use platform::window::desktop_config;
#[cfg(all(target_os = "ios", feature = "mobile"))]
use platform::window::ios_window;
#[cfg(not(target_arch = "wasm32"))]
use std::env;
#[cfg(not(target_arch = "wasm32"))]
use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt};
#[cfg(not(any(target_arch = "wasm32", all(target_os = "ios", feature = "mobile"))))]
use veilid_core::VeilidAPIError::NotInitialized;
#[cfg(feature = "veilid-stats")]
use veilid_core::{StatsManager, StatsSettings};

pub(crate) const FAVICON: Asset = asset!("/assets/favicon.ico");
pub(crate) const LOGO: Asset = asset!("/assets/logo.svg");
pub(crate) const APPLE_TOUCH_ICON: Asset = asset!("/assets/icons/apple-touch-icon.png");
pub(crate) const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");
pub(crate) const FONT_WOFF2: Asset = asset!("/assets/fonts/3270-Regular.woff2");

/// Native: default INFO with `veilid_core` capped at WARN — its network
/// state-machine chatter (degraded/recovered flapping, relay changes, dial-info
/// discovery) drowns app logs. The cap is only applied when `RUST_LOG` doesn't
/// already target `veilid_core`: `add_directive` replaces a same-target
/// directive, so applying it unconditionally would silently discard
/// `RUST_LOG=veilid_core=trace`. Other `RUST_LOG` directives still compose.
/// Wasm: `tracing-wasm` has no per-target filtering, so the web console keeps
/// plain INFO init.
fn init_logging() {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut filter = EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .from_env_lossy();
        if !env::var("RUST_LOG").is_ok_and(|v| v.contains("veilid_core"))
            && let Ok(directive) = "veilid_core=warn".parse()
        {
            filter = filter.add_directive(directive);
        }
        // `try_init`: logger may already be initialized by host/runtime.
        let _ = fmt().with_env_filter(filter).try_init();
    }
    #[cfg(target_arch = "wasm32")]
    if init(Level::INFO).is_err() {
        // Logger may already be initialized by host/runtime.
    }
}
#[cfg(feature = "veilid-stats")]
fn init_veilid_stats() {
    StatsManager::init(
        None,
        StatsSettings {
            enabled: true,
            ..StatsSettings::default()
        },
    );
}

fn main() {
    init_logging();
    #[cfg(feature = "veilid-stats")]
    init_veilid_stats();

    // Tao registers its iOS app delegate while constructing the event loop.
    // Hook scene configuration before UIApplicationMain can deliver cold URLs.
    #[cfg(all(target_os = "ios", feature = "mobile"))]
    let event_loop = EventLoopBuilder::with_user_event().build();

    #[cfg(any(target_os = "ios", target_os = "macos"))]
    bootstrap_apple_url_handler();

    #[cfg(all(target_os = "ios", feature = "mobile"))]
    LaunchBuilder::new()
        .with_cfg(
            Config::new()
                .with_event_loop(event_loop)
                .with_window(ios_window()),
        )
        .launch(App);
    // The desktop window is the app's own surface — borderless, with the page
    // drawing its chrome (`platform::window` owns what each OS must stop
    // drawing for that).
    #[cfg(all(
        feature = "desktop",
        any(target_os = "macos", target_os = "windows", target_os = "linux")
    ))]
    LaunchBuilder::new().with_cfg(desktop_config()).launch(App);
    #[cfg(not(any(
        all(target_os = "ios", feature = "mobile"),
        all(
            feature = "desktop",
            any(target_os = "macos", target_os = "windows", target_os = "linux")
        )
    )))]
    launch(App);

    // Shut Veilid down at the launch edge. `App`'s `use_drop` cannot: Dioxus
    // drains a scope's spawned tasks before it runs its drop hooks, so the
    // shutdown task was never polled. Web has no shutdown edge (the page just
    // unloads), so this is native-only.
    #[cfg(not(any(target_arch = "wasm32", all(target_os = "ios", feature = "mobile"))))]
    shutdown_veilid();
}

/// Shuts the Veilid API down once the UI event loop has returned.
///
/// A dedicated runtime: the app's own is gone by then (and `main` was never
/// guaranteed one — on desktop and mobile `launch` drives its own).
#[cfg(not(any(target_arch = "wasm32", all(target_os = "ios", feature = "mobile"))))]
fn shutdown_veilid() {
    let Ok(runtime) = tokio::runtime::Runtime::new() else {
        warn!("Veilid shutdown skipped: could not start a runtime");
        return;
    };
    match runtime.block_on(veilid::core::veilid_shutdown()) {
        Ok(()) | Err(NotInitialized) => {}
        Err(error) => warn!("Veilid shutdown failed: {error}"),
    }
}
