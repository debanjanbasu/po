//! Session-owned identity record watch.
//!
//! Keeps the authenticated user's identity record open ("pinned") for the
//! whole session and installs one whole-record DHT watch on it. Remote
//! changes — profile edits from another device, pairing beacons, dictionary
//! updates — then arrive push-style as `ValueChange` events instead of
//! being discovered by polling. Scoped `with_record` operations borrow the
//! pinned open (see [`store::pin_record`]), so sync-queue writes and the
//! watch coexist on the same record without open/close races.
//!
//! Lifecycle:
//! - [`start`] after admission ([`complete_admission`]), when the identity
//!   record is first created, and on watch death ([`reestablish`]);
//!   network reattach needs nothing — veilid-core reconciles watches and
//!   delivers missed changes itself;
//! - [`stop`] on logout / account deletion, under a [`begin_teardown`]
//!   guard held across epoch-advance → stop → session-clear;
//! - Veilid shutdown drops pin state via [`store::clear_pins`] — the API
//!   teardown closes every record anyway.
//!
//! All transitions serialize on one lifecycle lock so a concurrent
//! [`start`] (watch expiry, reattach) can never observe a half-torn-down
//! session and re-pin the identity of an account that is logging out.

use anyhow::Result;

use tokio::sync::{Mutex, MutexGuard};
use veilid_core::{KeyPair, RecordKey};

use crate::{
    session::{epoch as session_epoch, phase::SESSION},
    veilid::{core::parse_record_key, store},
};

/// Serializes lifecycle transitions. [`start`] validates the session
/// *inside* this lock; teardown holds it across the whole logout sequence.
static LIFECYCLE: Mutex<()> = Mutex::const_new(());

/// Proof that the identity-watch lifecycle is locked for session teardown.
/// Obtain via [`begin_teardown`] **before** advancing the session epoch and
/// hold until the session phase is cleared.
pub struct TeardownGuard {
    _guard: MutexGuard<'static, ()>,
}

/// Locks the lifecycle for session teardown. While held, no [`start`] /
/// [`reestablish`] can run — so none can re-pin the identity between the
/// epoch advance and the session clear.
pub async fn begin_teardown() -> TeardownGuard {
    TeardownGuard {
        _guard: LIFECYCLE.lock().await,
    }
}

/// Pins the identity record open with the session writer keypair and
/// installs a whole-record watch. Idempotent — re-pinning refreshes the
/// writer and re-issuing the watch updates it in place. No-op when there is
/// no authenticated session, no published identity key, or no keypair (a
/// read-only pin would strip write credentials from scoped operations).
pub async fn start() -> Result<()> {
    let _lifecycle = LIFECYCLE.lock().await;
    let epoch = session_epoch::SessionGuard::capture();
    let Some((key, keypair)) = session_identity() else {
        return Ok(());
    };
    store::pin_record(&key, Some(keypair)).await?;
    let watch = store::watch_record(&key, None).await;
    // Teardown advances the epoch only under `begin_teardown`, which waits
    // on this lock — but verify anyway so a future teardown path that skips
    // the guard can never leave a logged-out identity pinned.
    if !epoch.is_current() {
        rollback(&key).await;
        return Ok(());
    }
    // A pin without a live watch would hold the record open while missing
    // change notifications — undo the pin so the retry path (reestablish /
    // first-connectivity resync) re-establishes both together.
    if watch.is_err() {
        rollback(&key).await;
    }
    watch
}

/// Cancels the identity watch and closes the pinned record. Call with the
/// key captured **before** the session clears, under the same
/// [`TeardownGuard`] that covers the epoch advance. Best-effort: failures
/// are reported, never propagated. If the record was never pinned (e.g.
/// admission without a keypair) there is nothing to close — this lifecycle
/// does not own any other opens.
pub async fn stop(_teardown: &TeardownGuard, key: &RecordKey) {
    // Unpinning closes the record, which also cancels its outbound watch.
    crate::nonfatal!(
        "user.identity_watch",
        "Failed to unpin identity record",
        store::unpin_record(key).await
    );
}

/// Re-pins and re-watches after watch death, Veilid restart, or first
/// identity creation. Best-effort wrapper around [`start`].
pub async fn reestablish() {
    crate::nonfatal!(
        "user.identity_watch",
        "Failed to reestablish identity watch",
        start().await
    );
}

/// Undoes a pin/watch that lost a race against session teardown.
async fn rollback(key: &RecordKey) {
    crate::nonfatal!(
        "user.identity_watch",
        "Failed to unpin stale identity record",
        store::unpin_record(key).await
    );
}

/// The authenticated session's identity key + writer keypair, or `None`
/// when either is unavailable.
#[allow(clippy::significant_drop_tightening)] // the guard owns the borrow; cloning out first would copy the whole user
fn session_identity() -> Option<(RecordKey, KeyPair)> {
    let session = &*SESSION;
    let guard = session.peek();
    let user = guard.authenticated_user()?;
    if user.dht_key().is_empty() {
        return None;
    }
    let key = parse_record_key(user.dht_key()).ok()?;
    let keypair = user.runtime.keypair.clone()?;
    Some((key, keypair))
}
