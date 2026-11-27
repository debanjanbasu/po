use super::retry::{is_permanent_stored_row_error, retry_until_ok};
#[cfg(all(feature = "desktop", any(target_os = "windows", target_os = "linux")))]
use crate::platform::lifecycle::foreground_catchup;
#[cfg(target_arch = "wasm32")]
use crate::platform::lifecycle::install_web;
#[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
use crate::platform::{
    deep_link,
    lifecycle::{background_flush, foreground_catchup},
    native_bridge,
};
use crate::{
    APPLE_TOUCH_ICON, FAVICON, FONT_WOFF2, LOGO, TAILWIND_CSS,
    app::{
        router::ActiveRouter,
        startup::{hydrate_app_state_signals, run_app_init},
    },
    chat::{invite::restore_pending_invites_for_current_session, record::retry_failed_hydrations},
    components::{ActivityBar, ActivityModal, ErrorToast, WindowChrome},
    compression::{recompression::spawn_recompressor, trainer::spawn_trainer},
    global_state::theme,
    helpers::{spawn, store_lock::lock_store},
    models::app_state::AppState,
    nonfatal,
    router::helpers::mark_app_state_hydrated,
    session::{epoch::SessionGuard, phase::SESSION},
    storage::{
        background_errors::{ErrorSeverity, clear_errors_for_domain},
        store_trait::{load_local, use_model_revision},
    },
    sync::queue::get_sync_queue,
    user::store_guest_theme,
};
use anyhow::Error;
#[cfg(all(feature = "desktop", any(target_os = "windows", target_os = "linux")))]
use dioxus::desktop::{
    tao::event::{Event, WindowEvent},
    use_wry_event_handler,
};
use dioxus::prelude::*;

/// Marks the builds whose window is borderless, so `overrides.css` can reserve
/// the band the page's own chrome occupies. The value comes from
/// `platform::window::BORDERLESS` (the single source of truth): `"desktop"`
/// there, empty everywhere else.
#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
const WINDOW_CHROME: &str = crate::platform::window::BORDERLESS;
#[cfg(not(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
)))]
const WINDOW_CHROME: &str = "";

/// Isolated component so `document::Style` is only created once and never
/// receives "changed props" warnings when the parent `App` re-renders.
#[component]
fn FontFace() -> Element {
    rsx! {
        document::Link {
            rel: "preload",
            href: FONT_WOFF2,
            r#as: "font",
            r#type: "font/woff2",
            crossorigin: "anonymous",
        }
        document::Style {
            "@font-face {{ font-family: 'IBM3270'; font-display: swap; src: url('{FONT_WOFF2}') format('woff2'); }}"
        }
    }
}

#[component]
pub fn App() -> Element {
    #[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
    {
        use_future(move || async move {
            loop {
                native_bridge::wait_activity_change().await;
                foreground_catchup();
                deep_link::refresh_pending_action_if_empty();
            }
        });
        use_future(move || async move {
            loop {
                native_bridge::wait_background().await;
                background_flush();
            }
        });
    }

    // Web lifecycle: page visibility / freeze / pagehide drive the same
    // foreground-catchup and background-flush hooks as the native apps.
    #[cfg(target_arch = "wasm32")]
    use_hook(install_web);

    // Windows/Linux desktop: window focus is the foreground edge (macOS
    // uses the native didBecomeActive observer instead — avoid double-fire).
    #[cfg(all(feature = "desktop", any(target_os = "windows", target_os = "linux")))]
    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent {
            event: WindowEvent::Focused(true),
            ..
        } = event
        {
            foreground_catchup();
        }
    });

    // Respawn compression workers whenever the user authenticates (including
    // re-login after logout). `use_effect` re-runs when SESSION changes.
    use_effect(move || {
        if SESSION.read().authenticated_user().is_some() {
            spawn_trainer();
            spawn_recompressor();
        }
    });

    // Derive the DaisyUI data-theme value from the THEME signal.
    // "system" → no attribute (CSS prefers-color-scheme handles it).
    let data_theme = use_memo(move || match theme().read().as_str() {
        "light" => Some("bumblebee"),
        "dark" => Some("bumblebee-dark"),
        _ => None,
    });

    // Startup initialization runs in the background while routing remains live.
    use_future(move || async move {
        Box::pin(run_app_init()).await;
    });

    // Persist theme globally so it survives refresh before login/registration.
    // Skip the first run to avoid clobbering a persisted theme with the
    // default "system" before startup hydration can load saved settings.
    let mut theme_persistence_armed = use_signal(|| false);
    use_effect(move || {
        let theme_val = theme().read().clone();
        if !*theme_persistence_armed.peek() {
            theme_persistence_armed.set(true);
            return;
        }
        spawn(async move {
            nonfatal!(
                "main",
                "Failed to persist guest theme",
                store_guest_theme(&theme_val).await
            );
        });
    });

    // Root-scoped admission follow-up. Unlike auth component tasks, this scope
    // survives the navigation caused by the SESSION transition. Including the
    // epoch reruns it for re-admission of the same account, including startup.
    let admitted_identity = use_memo(move || {
        SESSION
            .read()
            .authenticated_user()
            .map(|user| (user.account_id_arc(), SessionGuard::capture()))
    });
    let app_state_revision = use_model_revision::<AppState>();
    use_resource(move || async move {
        let _ = app_state_revision();
        let Some((account_id, guard)) = admitted_identity() else {
            return;
        };
        let preferences_loaded = retry_until_ok(
            "app.account_state",
            ErrorSeverity::Visible,
            "Load account preferences",
            is_permanent_stored_row_error,
            || async {
                let _lock = lock_store(account_id.as_bytes()).await;
                let state = load_local::<AppState>(account_id.as_bytes()).await?;
                guard.commit(|| {
                    hydrate_app_state_signals(&state);
                    mark_app_state_hydrated(guard, &state.last_active_route);
                });
                Ok::<(), Error>(())
            },
        )
        .await
        .is_some();
        if preferences_loaded {
            guard.commit(|| clear_errors_for_domain("app.account_state"));
        } else {
            // The saved preferences row will not decode on a later attempt, and
            // the router gates the whole authenticated app on
            // `app_state_is_hydrated`: running on defaults (including the
            // default nav bar, which `reconcile_nav` rebuilds from an empty
            // order) is the difference between a usable app and a splash that
            // never ends. The unstored route stays empty, so the layout
            // restores nothing rather than restoring something wrong.
            guard.commit(|| {
                hydrate_app_state_signals(&AppState::default());
                mark_app_state_hydrated(guard, "");
            });
        }
    });
    use_resource(move || async move {
        let Some((_account_id, guard)) = admitted_identity() else {
            return;
        };
        nonfatal!(
            "main",
            "Failed to restore pending invites for the admitted account",
            restore_pending_invites_for_current_session().await
        );
        if !guard.is_current() {
            return;
        }
        retry_failed_hydrations().await;
        // Unconditional: the queue has no network gate. If veilid can't reach
        // anyone yet, ops stay pending at zero cost and the attach edge
        // re-notifies when connectivity returns.
        get_sync_queue().notify_enqueue();
    });

    rsx! {
        meta {
            name: "viewport",
            content: "width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no, shrink-to-fit=no, viewport-fit=cover",
        }
        document::Link { rel: "icon", r#type: "image/svg+xml", href: LOGO }
        document::Link { rel: "icon", href: FAVICON }
        document::Link { rel: "apple-touch-icon", href: APPLE_TOUCH_ICON }
        document::Stylesheet { href: TAILWIND_CSS }
        FontFace {}
        div {
            class: "text-xs h-full bg-base-100 text-base-content",
            "data-theme": data_theme,
            // Marks the borderless desktop window (`platform::window`): its
            // chrome band is reserved by `overrides.css`, which is the only
            // place that knows how tall it is. Empty off desktop.
            "data-window-chrome": WINDOW_CHROME,
            ErrorBoundary {
                handle_error: move |error: ErrorContext| rsx! {
                    ErrorToast { error }
                },
                ActiveRouter {}
            }
            ActivityBar {}
            ActivityModal {}
            WindowChrome {}
        }
    }
}
