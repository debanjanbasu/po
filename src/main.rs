// Veilid's generic async transaction pipeline exceeds rustc's default
// trait-solver recursion budget; 256 is sufficient once app fan-out stays local.
#![recursion_limit = "256"]
#![feature(try_blocks)]

//! Application entry point. Boots the Dioxus runtime and global state.

mod chat;
mod compression;
pub(crate) mod config;
mod global_state;
mod helpers;
mod models;
pub(crate) mod platform;
mod session;
mod storage;
mod sync;
pub(crate) mod ui; // gpui port (migration in progress)
pub(crate) mod user;
mod veilid;

#[cfg(not(any(
    feature = "gpui-desktop",
    feature = "gpui-web",
    feature = "gpui-mobile"
)))]
use crate::helpers::signals::{GlobalSignal, Signal};
#[cfg(not(any(
    feature = "gpui-desktop",
    feature = "gpui-web",
    feature = "gpui-mobile"
)))]
#[cfg(all(
    not(any(
        feature = "gpui-desktop",
        feature = "gpui-web",
        feature = "gpui-mobile"
    )),
    any(target_os = "ios", target_os = "macos")
))]
use platform::deep_link::bootstrap_apple_url_handler;
#[cfg(not(target_arch = "wasm32"))]
use std::env;
#[cfg(not(target_arch = "wasm32"))]
use tracing::warn;
#[cfg(not(target_arch = "wasm32"))]
use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt};
#[cfg(all(
    not(target_arch = "wasm32"),
    not(all(target_os = "ios", any(feature = "mobile", feature = "gpui-mobile")))
))]
use veilid_core::VeilidAPIError::NotInitialized;
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
    #[cfg(target_arch = "wasm32")]
    if tracing_wasm::try_set_as_global_default().is_err() {
        // Logger may already be initialized by host/runtime.
    }
}
#[cfg(any(
    feature = "gpui-desktop",
    feature = "gpui-web",
    feature = "gpui-mobile"
))]
fn main() {
    #[cfg(not(target_os = "android"))]
    use gpui::AppContext as _;
    init_logging();

    #[cfg(all(feature = "gpui-desktop", target_os = "macos"))]
    platform::deep_link::bootstrap_apple_url_handler();

    #[cfg(target_family = "wasm")]
    gpui_platform::web_init();

    let application = gpui_platform::application();
    // The URL handler lives on the Application (pre-run); the scheme
    // registration happens inside the boot closure on App.
    ui::boot::on_open_urls(&application);
    // The web event loop does not block: `run` returns as soon as boot
    // finishes, and dropping the handle releases the app (killing the window
    // before the first frame). On wasm the handle must outlive `main` —
    // park it in a leaked static, the same contract hello_web documents.
    #[cfg(target_arch = "wasm32")]
    {
        let handle = application.run(|cx: &mut gpui::App| {
            boot(cx);
        });
        std::mem::forget(handle);
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _handle = application.run(|cx: &mut gpui::App| {
        boot(cx);
    });

    // Same reasoning as the Dioxus edge: GPUI drains spawned tasks before
    // release hooks, so Veilid shuts down here after the loop returns
    // (desktop only; web has no shutdown edge, mobile never returns).
    #[cfg(feature = "gpui-desktop")]
    shutdown_veilid();
}

/// The shared boot closure: runs `on_finish_launching` for every target.
fn boot(cx: &mut gpui::App) {
    use gpui::AppContext as _;

    // Mobile has no ambient tokio runtime: veilid's `spawn_blocking` (and any
    // other tokio call) panics without one. Enter a process-wide runtime so
    // `Handle::current()` resolves from the dispatcher thread.
    #[cfg(all(any(target_os = "ios", target_os = "android"), feature = "gpui-mobile"))]
    {
        use std::sync::LazyLock;
        static MOBILE_TOKIO: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("mobile tokio runtime")
        });
        // Leak the guard: the runtime context must stay entered for the
        // process lifetime (the startup task's spawn_blocking runs long
        // after boot returns).
        std::mem::forget(MOBILE_TOKIO.enter());
    }
    ui::boot::load_fonts(cx);
    ui::boot::register_scheme(cx);
    ui::state::init_session(cx);
    ui::widgets::toast::init_toasts(cx);
    ui::router::init_router(cx);
    ui::router::init_auth_router(cx);
    // The startup pipeline: local-first init → network attach, driving
    // SessionPhase exactly as the frozen shell's `run_app_init` did.
    ui::startup::start(cx);
    if let Err(error) = cx.open_window(ui::boot::window_options(), |_, cx| {
        cx.new(|_| crate::ui::app::RootView::new())
    }) {
        eprintln!("failed to open the main window: {error}");
    }
    cx.activate(true);
}

/// Shuts the Veilid API down once the UI event loop has returned.
///
/// A dedicated runtime: the app's own is gone by then (and `main` was never
/// guaranteed one — on desktop and mobile `launch` drives its own).
#[cfg(all(
    not(target_arch = "wasm32"),
    not(all(target_os = "ios", any(feature = "mobile", feature = "gpui-mobile")))
))]
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
