//! Session epoch guard for stale async task cancellation.
//!
//! Every login/logout/admit increments the global epoch. Async tasks capture
//! the epoch at spawn time and check it before writing to global signals,
//! preventing old futures from corrupting state after a session change.
//!
//! Two complementary mechanisms, with different roles:
//! - [`spawn_session_scoped`] promptly cancels a task when the epoch
//!   advances — an optimization that stops doomed work (backfills, training)
//!   from wasting bandwidth and CPU after logout. It dies with the calling
//!   scope; [`spawn_session_detached`] is the root-scoped variant for work
//!   that must survive component unmount (e.g. post-pairing hydration).
//! - [`SessionGuard`] fences synchronous signal commits. Durable async
//!   commits additionally need an account-ID check under the same local
//!   lock as account cleanup; task cancellation is not a transaction.
//!
//! **WASM compatibility note**: `SessionGuard::is_current()` compares against
//! the global atomic rather than a watch channel because `subscribe()` on the
//! single-threaded WASM runtime may return a receiver with a stale value.

use std::{
    future::Future,
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};

use dioxus::core::spawn_forever;
use tokio::{select, sync::watch};

use crate::helpers;

static SESSION_EPOCH: AtomicU64 = AtomicU64::new(0);

static SESSION_TX: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// Returns the current session epoch.
pub fn current_epoch() -> u64 {
    SESSION_EPOCH.load(Ordering::SeqCst)
}

/// Increments the session epoch, invalidating all prior epoch captures.
/// Also notifies all [`SessionGuard`] holders via the watch channel.
/// Returns the new epoch value.
pub fn advance_epoch() -> u64 {
    let next = SESSION_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    if SESSION_TX.send(next).is_err() {
        // No active receivers; this is fine during early startup or teardown.
    }
    next
}

/// Cooperative session guard — captures epoch at creation.
///
/// Copies share the same epoch snapshot, so all copies track the same
/// session lifetime. Uses atomic epoch comparison for WASM compatibility.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionGuard {
    epoch: u64,
}

impl SessionGuard {
    /// Capture the current epoch. The guard is "current" until [`advance_epoch`]
    /// is called.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            epoch: current_epoch(),
        }
    }

    /// The epoch captured by this guard. For value comparisons (e.g. against a
    /// startup snapshot); staleness checks should use [`Self::is_current`].
    #[must_use]
    pub const fn epoch(self) -> u64 {
        self.epoch
    }

    /// Returns `true` if the session has not changed since capture.
    ///
    /// Compares against the global atomic rather than a watch receiver
    /// because `subscribe()` on WASM may return a receiver with a stale value.
    #[must_use]
    pub fn is_current(self) -> bool {
        self.epoch == SESSION_EPOCH.load(Ordering::SeqCst)
    }

    /// Execute `f` only if the guard is still current. Returns whether `f` ran.
    pub fn commit(self, f: impl FnOnce()) -> bool {
        if self.is_current() {
            f();
            true
        } else {
            false
        }
    }
}

/// Spawns an async task that is canceled cooperatively when the session changes.
///
/// The task is owned by the **calling scope**, so it is *also* canceled when
/// that component unmounts — which makes it safe to capture component-owned
/// signals. For session-lifetime work that must survive component unmount
/// (e.g. post-pairing hydration), use [`spawn_session_detached`].
///
/// The receiver is marked as seen at subscribe time, so `changed()` only fires
/// on *future* epoch advances, not on the stale initial value that WASM
/// `subscribe()` may return.
pub fn spawn_session_scoped<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    helpers::spawn(until_epoch_change(future));
}

/// Like [`spawn_session_scoped`], but rooted at the runtime's **root scope**
/// (`spawn_forever`): the task survives the spawning component unmounting and
/// is canceled *only* by a session-epoch advance (logout/admit).
///
/// Because the task outlives the caller's scope, the future must **not**
/// capture component-owned reactive values (`use_signal`/`use_memo`/…) —
/// using them from the root scope after the component drops is a
/// use-after-drop hazard. Global statics and owned data are fine.
pub fn spawn_session_detached<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    spawn_forever(until_epoch_change(future));
}

/// Wraps `future` so it resolves early when the session epoch advances.
/// The receiver is marked as seen at subscribe time (see the WASM note above).
fn until_epoch_change<F>(future: F) -> impl Future<Output = ()>
where
    F: Future<Output = ()> + 'static,
{
    let guard = SessionGuard::capture();
    let mut rx = SESSION_TX.subscribe();
    rx.borrow_and_update();
    async move {
        if !guard.is_current() {
            return;
        }
        select! {
            biased;
            () = async {
                loop {
                    if rx.changed().await.is_err() || !guard.is_current() {
                        break;
                    }
                }
            } => {}
            () = future => {}
        }
    }
}
