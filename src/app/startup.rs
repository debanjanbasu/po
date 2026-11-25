use std::sync::OnceLock;

use anyhow::{Context, Result};
use dioxus::{logger::tracing::info, prelude::ReadableExt};
use tokio::try_join;

use crate::{
    compression::{codec, samples},
    global_state::{last_active_route, nav_bar_items, theme},
    helpers::spawn,
    models::app_state::AppState,
    platform::{deep_link, device_id},
    router::nav_item::NAV_ITEMS,
    session::{
        epoch::SessionGuard,
        phase::{SESSION, SessionPhase, resolve_restore_to_anonymous},
    },
    storage::background_errors::{ErrorSeverity, clear_errors_for_domain, report_nonfatal},
    sync::pending_ops,
    user::{self, admit_user},
    veilid::core::{attach_network, ensure_local_api},
};

use super::retry::{is_permanent_local_error, never_permanent, retry_until_ok};

/// Activity-panel domains for the two startup phases. Persistent failures are
/// surfaced under these so the `Restoring` splash is never silently stuck;
/// the matching domain clears when its phase succeeds.
const STARTUP_LOCAL_DOMAIN: &str = "app.startup.local";
const STARTUP_NETWORK_DOMAIN: &str = "app.startup.network";

static APP_BOOTSTRAP: OnceLock<()> = OnceLock::new();

/// Phase 1: Local-first initialization. Requires only `api_startup()` (no
/// network). Provides `TableDB`, `Crypto`, session hydration, theme, etc.
async fn run_local_init() -> Result<()> {
    // Ask the browser directly, before Veilid reads its config and writes the
    // device key. Veilid's own error for a refused write arrives as
    // `failed to write local storage: ` with the `DOMException` name stripped
    // by its `JsValue::as_string()` mapping, and the config that would let us
    // see the backend is only built inside `ensure_local_api` below — so this is
    // the last point where the cause is still readable. Runs on every start:
    // one namespaced write/read/delete.
    #[cfg(target_arch = "wasm32")]
    if let Some(problem) = crate::platform::browser_storage::probe() {
        return Err(anyhow::Error::new(problem));
    }
    drop(
        ensure_local_api()
            .await
            .context("Initialize Veilid local API")?,
    );
    deep_link::consume_pending_action();
    // Harness seam: proves the app reached its local-first startup under the
    // harness, before any network work — the one marker every surface can
    // observe (Android included, where the seam arrives as an intent extra).
    crate::config::e2e::event!("app_started", platform = std::env::consts::OS);
    device_id::device_id().await.context("Load device id")?;
    device_id::device_label()
        .await
        .context("Ensure device label")?;
    let ((), (), guest_theme) = try_join!(
        async {
            codec::reload_from_tablestore()
                .await
                .context("Reload compression dictionaries from table store")
        },
        async {
            pending_ops::init_indexes()
                .await
                .context("Initialize pending-op indexes")
        },
        async {
            user::load_guest_theme_opt()
                .await
                .context("Load guest theme")
        },
    )?;
    if let Some(guest_theme_val) = guest_theme {
        theme().write().clone_from(&guest_theme_val);
    }
    // Restore a remembered account, but only into an untouched session:
    // this init loop retries on failure, so by the time a retry succeeds the
    // user may have already logged in interactively. Guard on the epoch
    // captured before the async hydration plus an untouched phase so
    // restoration can never clobber a live session.
    //
    // The session boots in `Restoring` (router shows a splash, not the login
    // form). Any path that decides no remembered session will be admitted
    // must resolve it to `Anonymous`; hydration errors propagate to the retry
    // loop and intentionally stay in `Restoring`.
    let restore_guard = SessionGuard::capture();
    if let Some(user) = user::hydrate_remembered_user()
        .await
        .context("Hydrate remembered user")?
        && restore_guard.is_current()
    {
        let untouched = matches!(
            &*SESSION.peek(),
            SessionPhase::Restoring | SessionPhase::Anonymous
        );
        if untouched && let Err(error) = admit_user(user).await {
            report_nonfatal("app.startup", "Failed to restore remembered session", error);
        }
    }
    // Any restore path that will not admit (no remembered user, stale epoch,
    // session already owned by another actor, or complete_admission bailing
    // without setting a phase) must give way to the login form — the helper
    // no-ops when another actor moved the session on.
    resolve_restore_to_anonymous();
    Ok(())
}
/// Phase 2: Network attachment. Runs in the background — can fail and retry
/// without blocking local-first operations or session restoration.
async fn run_network_attach() {
    // Attachment failures are connectivity conditions: every one of them can
    // clear on a later attempt, so this loop never gives up.
    if retry_until_ok(
        STARTUP_NETWORK_DOMAIN,
        ErrorSeverity::Visible,
        "Network attach",
        never_permanent,
        attach_network,
    )
    .await
    .is_some()
    {
        info!("Veilid network attached successfully");
        clear_errors_for_domain(STARTUP_NETWORK_DOMAIN);
    }
}

pub fn hydrate_app_state_signals(state: &AppState) {
    let new_nav = state.reconcile_nav(&NAV_ITEMS);
    // Compare via a condition temporary: the read guard must be dropped
    // before the `write()` below, or the signal borrow check panics.
    if *nav_bar_items().peek() != new_nav {
        nav_bar_items().write().clone_from(&new_nav);
    }
    if !state.theme.is_empty() && theme().peek().as_str() != state.theme {
        theme().write().clone_from(&state.theme);
    }
    // Only hydrate once: once LAST_ACTIVE_ROUTE is non-empty (set by router or
    // prior hydration), never overwrite it with persisted state.
    if !state.last_active_route.is_empty() && last_active_route().peek().is_empty() {
        last_active_route()
            .write()
            .clone_from(&state.last_active_route);
    }
}

pub async fn run_app_init() {
    // Phase 1: Local-first initialization (retries until local API is available)
    let local_init_ok = retry_until_ok(
        STARTUP_LOCAL_DOMAIN,
        ErrorSeverity::Visible,
        "Local init",
        is_permanent_local_error,
        || async {
            APP_BOOTSTRAP.get_or_init(samples::init);
            run_local_init().await.context("Local init")
        },
    )
    .await
    .is_some();
    if local_init_ok {
        clear_errors_for_domain(STARTUP_LOCAL_DOMAIN);
    } else {
        // A stored row that will not decode will not decode on the next
        // attempt: hand the app back its entry point. Without this the session
        // stays in `Restoring`, the router keeps rendering the splash, and the
        // login form — the only route to Settings, account removal or a
        // re-pair — is unreachable forever. No-op when the phase already moved
        // on (another actor admitted, or admission failed on its own).
        resolve_restore_to_anonymous();
    }

    // Phase 2: Network attachment (background, non-blocking)
    spawn(async {
        run_network_attach().await;
    });
}
