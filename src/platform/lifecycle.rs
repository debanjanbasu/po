//! Cross-platform app lifecycle → sync integration.
//!
//! Foreground/background transitions come from per-platform sources —
//! Swift observers on iOS/macOS (`DeepLinkPlugin`), `MainActivity` on
//! Android, window focus on Windows/Linux, page visibility on web — and
//! funnel into the shared `foreground_catchup` entry point and, where available,
//! platform-specific background-flush hooks.
//!
//! Suspended iOS apps and frozen Android apps cannot receive inbound P2P
//! traffic, and a sleeping laptop drops its DHT watches. The catch-up
//! resyncs when the app has been away long enough that push delivery may
//! have been silently lost; the staleness gate keeps rapid focus changes
//! free.

use std::sync::atomic::{AtomicU64, Ordering};

use veilid_core::VeilidAppLifecycleState;

#[cfg(target_arch = "wasm32")]
use std::panic::UnwindSafe;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{JsCast, closure::Closure};
#[cfg(target_arch = "wasm32")]
use web_sys::{EventTarget, window};

#[cfg(target_arch = "wasm32")]
use crate::storage::background_errors::report_nonfatal;
use crate::{
    config,
    helpers::{async_utils::now_epoch_millis, spawn},
    sync::{APP_ACTIVITY_CHANGED, queue::notify_sync},
    user::{ctx::UserCtx, watcher::resync_identity},
    veilid::core::report_lifecycle_state,
};

/// Last time (epoch ms) a sync path made verified inbound contact with the
/// network: watch callbacks, sync nudges, clean inspect polls, resyncs.
static LAST_SYNC_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);

/// Records that a sync path just made verified contact with the network.
pub fn note_sync_activity() {
    LAST_SYNC_ACTIVITY_MS.store(now_epoch_millis(), Ordering::Relaxed);
}

/// Whether a heavy background pass should wait rather than run now.
///
/// On web: true while the page is visible. Both callers (the recompression
/// cycle and dictionary training) run their per-item CPU inline — on the only
/// thread there is — and a visible page is a page that paints, so a cycle
/// started now is measured in dropped frames while the same work in a hidden
/// tab costs nothing. A hidden page is also where a browser happily spends
/// background CPU, and `visibilitychange` arrives for backgrounding, tab
/// hiding, and screen lock alike.
///
/// Off web there is no page to paint and no equivalent signal: a background
/// app's thread is either scheduled or suspended by the OS, and neither case
/// is jank. Always false, so callers keep the code path and lose only the wait.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn defer_heavy_work() -> bool {
    !page_hidden()
}

#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub const fn defer_heavy_work() -> bool {
    false
}

/// Whether the web page is hidden. Reports "visible" when there is no document
/// to ask, so a caller that defers on a visible page can never stall on a host
/// that has no answer.
#[cfg(target_arch = "wasm32")]
fn page_hidden() -> bool {
    window()
        .and_then(|window| window.document())
        .is_some_and(|document| document.hidden())
}

fn is_stale() -> bool {
    let last = LAST_SYNC_ACTIVITY_MS.load(Ordering::Relaxed);
    now_epoch_millis().saturating_sub(last) >= config::lifecycle::STALE_AFTER_MS
}

/// App came to the foreground (or regained focus). Wakes the startup retry
/// loops and the sync queue; when the app has been away long enough that
/// watches or nudges may have been silently lost, pulls the identity and
/// child records from the network (which also re-arms the child watches).
pub fn foreground_catchup() {
    report_lifecycle_state(VeilidAppLifecycleState::Active);
    APP_ACTIVITY_CHANGED.notify_waiters();
    notify_sync();
    if !is_stale() {
        return;
    }
    spawn(async {
        if UserCtx::try_current().is_none() {
            return;
        }
        resync_identity().await;
    });
}

/// App is heading to the background. Kicks the sync queue so pending
/// writes get their best shot at dispatching during the platform's runway
/// (iOS `beginBackgroundTask`, Android post-`onStop` grace). Veilid's
/// offline-subkey-write flusher handles anything that does not make it.
#[cfg(any(
    target_arch = "wasm32",
    target_os = "ios",
    target_os = "macos",
    target_os = "android",
))]
pub fn background_flush() {
    report_lifecycle_state(VeilidAppLifecycleState::Background);
    notify_sync();
}

/// Installs web page-visibility listeners that drive the lifecycle hooks:
/// `visibilitychange` (tab shown/hidden), `resume`/`freeze` (Chromium tab
/// freezing), and `pagehide` (navigation away / tab close).
#[cfg(target_arch = "wasm32")]
pub fn install_web() {
    let Some(window) = window() else {
        report_nonfatal(
            "platform.lifecycle",
            "Cannot observe page lifecycle",
            "Browser window is unavailable",
        );
        return;
    };
    let Some(document) = window.document() else {
        report_nonfatal(
            "platform.lifecycle",
            "Cannot observe page lifecycle",
            "Browser document is unavailable",
        );
        return;
    };

    install_listener(&document, "visibilitychange", {
        let document = document.clone();
        move || {
            if document.hidden() {
                background_flush();
            } else {
                foreground_catchup();
            }
        }
    });
    install_listener(&document, "resume", foreground_catchup);
    install_listener(&document, "freeze", background_flush);
    install_listener(&window, "pagehide", background_flush);
}

#[cfg(target_arch = "wasm32")]
fn install_listener(
    target: &EventTarget,
    name: &'static str,
    handler: impl FnMut() + UnwindSafe + 'static,
) {
    let callback = Closure::<dyn FnMut()>::new(handler);
    if let Err(error) =
        target.add_event_listener_with_callback(name, callback.as_ref().unchecked_ref())
    {
        report_nonfatal(
            "platform.lifecycle",
            "Cannot observe page lifecycle",
            format!("Register {name}: {error:?}"),
        );
    } else {
        callback.forget();
    }
}
