//! Session-owned watches on identity child records.
//!
//! The identity record itself is watched by [`super::identity_watch`], so
//! profile/security/app-state/pointer changes arrive push-style. Child
//! records (TOTP tokens, contacts, conversation index) are separate DHT
//! records: without their own watches a change made on another device is
//! only discovered on a full identity resync. This module keeps one watch
//! per existing child record so those changes also arrive live.
//!
//! Lifecycle:
//! - [`sync`] after admission resync and at the end of every identity
//!   resync (pointers may have appeared or migrated), and after reconnect
//!   to retry child records that never pinned; Veilid itself reconciles watches
//!   that were successfully armed;
//! - [`stop_all`] on logout / account deletion, alongside the identity
//!   watch teardown;
//! - watch death notices (`count == 0`) re-arm through [`sync`] from the
//!   change dispatcher.
//!
//! Only records with an existing pointer are watched — watching never
//! creates records; creation stays with the normal sync paths.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use tokio::sync::{Mutex as AsyncMutex, MutexGuard};

use veilid_core::RecordKey;

use crate::{
    helpers::store_lock::recover_poison,
    session::epoch as session_epoch,
    session::phase::SESSION,
    storage::background_errors::report_nonfatal,
    user::{get_session_crypto, record::child_owner_keypair},
    veilid::store,
};

static LIFECYCLE: AsyncMutex<()> = AsyncMutex::const_new(());

pub(super) struct TeardownGuard {
    _guard: MutexGuard<'static, ()>,
}

pub(super) async fn begin_teardown() -> TeardownGuard {
    TeardownGuard {
        _guard: LIFECYCLE.lock().await,
    }
}

/// Child features watched per session. Matches the models pulled by
/// `watcher::pull_identity_children`.
pub const WATCHED_FEATURES: [&str; 3] = ["totp", "contacts", "conversations"];

/// Tracked child record keys → feature id, for change dispatch and fallback inspection.
/// Keys remain tracked while an inactive watch is waiting for the next re-arm.
static WATCHED: LazyLock<Mutex<HashMap<String, (RecordKey, &'static str)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns the feature id when `key` is tracked for the authenticated session.
pub fn feature_for_key(key: &str) -> Option<&'static str> {
    recover_poison(WATCHED.lock())
        .get(key)
        .map(|(_, feature)| *feature)
}

/// Snapshot tracked child records for the inspect-poll sync backstop.
pub fn watched_records() -> Vec<(RecordKey, &'static str)> {
    recover_poison(WATCHED.lock()).values().cloned().collect()
}

/// Pins and watches every child record that has a pointer on the
/// authenticated user. Idempotent: re-issuing a watch updates it in place,
/// and pointer migrations or removals drop stale watches before re-arming.
/// No-op without an authenticated session or session crypto.
pub async fn sync() {
    let _lifecycle = LIFECYCLE.lock().await;
    let guard = session_epoch::SessionGuard::capture();
    let Some((username, pointers)) = ({
        let session = SESSION.peek();
        session.authenticated_user().map(|user| {
            (
                user.username_arc(),
                user.runtime.sync_pointers.clone().unwrap_or_default(),
            )
        })
    }) else {
        return;
    };
    let Some(crypto) = get_session_crypto(&username) else {
        return;
    };

    for feature in WATCHED_FEATURES {
        let key = pointers
            .pointers
            .get(feature)
            .and_then(|pointer| match pointer.record_key() {
                Ok(key) => Some(key),
                Err(error) => {
                    report_nonfatal("user.child_watch", "Invalid child pointer key", error);
                    None
                }
            });
        let key_str = key.as_ref().map(ToString::to_string);

        // Drop stale watches when a pointer moves or disappears.
        let stale: Vec<String> = {
            let map = recover_poison(WATCHED.lock());
            map.iter()
                .filter(|(watched_key, (_, watched_feature))| {
                    *watched_feature == feature && Some(watched_key.as_str()) != key_str.as_deref()
                })
                .map(|(watched_key, _)| watched_key.clone())
                .collect()
        };
        for old in stale {
            remove_watch(&old).await;
        }

        let Some(key) = key else {
            continue;
        };
        let Some(key_str) = key_str else {
            continue;
        };

        if !guard.is_current() {
            return;
        }
        let owner = match child_owner_keypair(&crypto.keypair, feature).await {
            Ok(kp) => kp,
            Err(error) => {
                report_nonfatal(
                    "user.child_watch",
                    "Failed to derive child owner keypair",
                    error,
                );
                continue;
            }
        };
        if let Err(error) = store::pin_record(&key, Some(owner)).await {
            report_nonfatal("user.child_watch", "Failed to pin child record", error);
            continue;
        }
        if !guard.is_current() {
            remove_watch(&key_str).await;
            return;
        }
        // `inspect_record` requires an open record. Track the key only after
        // pin succeeds, but retain it if watch setup fails so inspect-poll can
        // recover remote changes while a later reconciliation retries push.
        recover_poison(WATCHED.lock()).insert(key_str.clone(), (key.clone(), feature));
        if let Err(error) = store::watch_record(&key, None).await {
            if !guard.is_current() {
                remove_watch(&key_str).await;
                return;
            }
            report_nonfatal("user.child_watch", "Failed to watch child record", error);
            continue;
        }
        if !guard.is_current() {
            // Lost a race against logout — undo and stop.
            remove_watch(&key_str).await;
            return;
        }
    }
}

/// Cancels and unpins every child watch. Call during session teardown while
/// holding the guard from [`begin_teardown`], across epoch advance and session
/// clear, so no concurrent [`sync`] can re-arm a record after this sweep.
pub(super) async fn stop_all(_teardown: &TeardownGuard) {
    let entries: Vec<(String, RecordKey)> = recover_poison(WATCHED.lock())
        .iter()
        .map(|(key_str, (key, _))| (key_str.clone(), key.clone()))
        .collect();
    for (key_str, key) in entries {
        match store::unpin_record(&key).await {
            Ok(_) => {
                recover_poison(WATCHED.lock()).remove(&key_str);
            }
            Err(error) => report_nonfatal(
                "user.child_watch",
                "Failed to unpin child record during teardown",
                error,
            ),
        }
    }
}

/// Unpins and forgets a stale key only after Veilid confirms the close.
async fn remove_watch(key_str: &str) {
    let Some(key) = recover_poison(WATCHED.lock())
        .get(key_str)
        .map(|(key, _)| key.clone())
    else {
        return;
    };
    match store::unpin_record(&key).await {
        Ok(_) => {
            recover_poison(WATCHED.lock()).remove(key_str);
        }
        Err(error) => report_nonfatal(
            "user.child_watch",
            "Failed to unpin stale child record",
            error,
        ),
    }
}
