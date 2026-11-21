//! Startup pipeline — the gpui-side port of `app/startup.rs`.
//!
//! Same two phases with identical semantics:
//! - **Phase 1 (local-first, retrying):** browser-storage probe → Veilid
//!   local API → deep-link consumption → `app_started` seam → device
//!   id/label → codec/indexes/guest-theme in `try_join!` → remembered-user
//!   hydration into an untouched session → `resolve_restore_to_anonymous`
//!   fallback. A stored row that will not decode will not decode on the
//!   next attempt: any not-admitted path hands the app back its entry point
//!   (the session must never stay in `Restoring` past a completed phase).
//! - **Phase 2 (network, background):** `attach_network` retries forever —
//!   attachment failures are connectivity conditions.
//!
//! Dioxus signal writes become ui-state calls (`ui::state`); the dioxus
//! `spawn` roots become `App::spawn` tasks started from the boot closure.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use gpui::App;
use tokio::try_join;
use tracing::info;

use crate::{
    compression::{codec, samples},
    models::app_state::AppState,
    platform::{deep_link, device_id},
    session::{
        epoch::SessionGuard,
        phase::{SessionPhase, resolve_restore_to_anonymous},
    },
    storage::background_errors::{ErrorSeverity, clear_errors_for_domain, report_nonfatal},
    sync::pending_ops,
    ui::state::set_app_state_hydrated,
    user,
    veilid::core::{attach_network, ensure_local_api},
};

// The frozen `retry_until_ok` machinery is dioxus-light (only the tracing
// facade, a direct dep now) — reuse it as-is until the app module deletes.
use crate::helpers::retry::{is_permanent_local_error, never_permanent, retry_until_ok};

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
    // device key (the old probe comment, verbatim): this is the last point
    // where the cause is still readable.
    #[cfg(target_arch = "wasm32")]
    if let Some(problem) = crate::platform::browser_storage::probe() {
        return Err(anyhow::Error::new(problem));
    }
    tracing::info!("DBG run_local_init: before ensure_local_api");
    drop(
        ensure_local_api()
            .await
            .context("Initialize Veilid local API")?,
    );
    tracing::info!("DBG run_local_init: after ensure_local_api");
    tracing::info!("DBG run_local_init: before consume_pending_action");
    deep_link::consume_pending_action();
    tracing::info!("DBG run_local_init: before app_started event");
    // Harness seam: proves the app reached its local-first startup under the
    // harness, before any network work.
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
        crate::global_state::theme()
            .write()
            .clone_from(&guest_theme_val);
    }
    // Restore a remembered account, but only into an untouched session: this
    // init loop retries on failure, so by the time a retry succeeds the user
    // may have already logged in interactively. Guard on the epoch captured
    // before the async hydration plus an untouched phase so restoration can
    // never clobber a live session.
    let restore_guard = SessionGuard::capture();
    if let Some(user) = user::hydrate_remembered_user()
        .await
        .context("Hydrate remembered user")?
        && restore_guard.is_current()
    {
        let untouched = matches!(
            session_phase_current(),
            SessionPhase::Restoring | SessionPhase::Anonymous
        );
        if untouched && let Err(error) = user::admit_user(user).await {
            report_nonfatal("app.startup", "Failed to restore remembered session", error);
        }
    }
    // Any restore path that will not admit must give way to the login form —
    // the helper no-ops when another actor moved the session on.
    resolve_restore_to_anonymous();
    Ok(())
}

/// Reads the session phase without a gpui context: the phase lives in the
/// `SessionPhaseGlobal` entity, so this snapshot goes through the same
/// process-global the Dioxus `SESSION.peek()` used (the session module owns
/// it until the phase cutover completes).
fn session_phase_current() -> SessionPhase {
    crate::session::phase::SESSION.peek().clone()
}

/// Phase 2: Network attachment. Runs in the background — can fail and retry
/// without blocking local-first operations or session restoration.
async fn run_network_attach() {
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

/// The ui-state port of `hydrate_app_state_signals`: reconciles nav items,
/// theme, and the last active route into the ui/session state.
pub fn hydrate_app_state(state: &AppState) {
    crate::ui::state::hydrate_nav_theme_route(state);
}

/// The full pipeline: phase 1 retrying in the foreground task, phase 2 as a
/// detached background task. Started from the gpui boot closure.
pub fn start(cx: &App) {
    tracing::info!("DBG startup::start called");
    cx.spawn(async move |_cx| {
        tracing::info!("DBG startup task polled");
        // Phase 1: Local-first initialization (retries until local API is
        // available).
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
            resolve_restore_to_anonymous();
        }

        // Phase 2: Network attachment (background, non-blocking). Detached:
        // it retries forever by design, on the domain executor (veilid
        // futures are tokio-native).
        crate::helpers::spawn(async {
            run_network_attach().await;
        });

        // The pipeline completed: hydration gate opens. Persisted AppState
        // hydration itself lands with the settings cutover; mark hydrated so
        // the shell shows the real UI.
        set_app_state_hydrated(true);
    })
    .detach();
}
