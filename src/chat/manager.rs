//! Conversation manager — session-owned DHT watch registry.
//!
//! Every conversation in the `ConversationIndex` is watched from login to
//! logout (`ensure_watched` / `cancel_all_watches`); opening a conversation
//! in the UI does not change watch lifetime. The UI marks which conversation
//! is on screen (`set_open_conversation`) so signal handlers can gate
//! presence-style signals (typing) to the open conversation only.
//!
//! Watches cover the full subkey range (`None`), including our own member
//! subkeys — a write from another device on this account arrives as an
//! ordinary `ValueChanged` and merges into the local store.
//!
//! Reattach needs no handling here: veilid-core retains desired watch
//! state across detach, reconciles per-node watches itself, and delivers
//! changes missed while detached as ordinary `ValueChange` updates.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use anyhow::Result;
use dioxus::logger::tracing::debug;
use veilid_core::RecordKey;

use crate::{
    chat::record::sweep_rearmed_conversation,
    config,
    helpers::{
        async_utils::{now_epoch_millis, sleep_ms},
        store_lock::{KeyedMutex, recover_poison},
    },
    nonfatal,
    session::epoch as session,
    storage::background_errors::report_nonfatal,
    veilid::core::parse_record_key,
    veilid::store,
};

/// Per-key watch lifecycle; `NeedsRearm` remains tracked for bounded recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchState {
    Opening,
    Open,
    NeedsRearm,
    Closing,
}

/// Serialize pin/watch/unpin for each key across session teardown and reopen.
static WATCH_LOCKS: LazyLock<KeyedMutex<Arc<str>>> = LazyLock::new(KeyedMutex::new);

/// Global conversation watch manager state.
static CONV_MANAGER: LazyLock<RwLock<ConversationManager>> =
    LazyLock::new(|| RwLock::new(ConversationManager::default()));

/// Minimum interval (ms) between renewal attempts for the same key.
const RENEWAL_DEDUP_INTERVAL_MS: u64 = 5000;

/// Acquires a read guard on `CONV_MANAGER`, recovering from poison.
fn read_manager() -> RwLockReadGuard<'static, ConversationManager> {
    recover_poison(CONV_MANAGER.read())
}

/// Acquires a write guard on `CONV_MANAGER`, recovering from poison.
fn write_manager() -> RwLockWriteGuard<'static, ConversationManager> {
    recover_poison(CONV_MANAGER.write())
}

/// Moves `key` from state `from` to `to` under the write lock, returning
/// whether the transition happened. The lock is released on return, so callers
/// may await (re-arming, sweeping) afterwards.
fn claim_state(key: &str, from: WatchState, to: WatchState) -> bool {
    let mut manager = write_manager();
    if manager.state(key) == Some(from) {
        manager.set_state(key, to);
        true
    } else {
        false
    }
}

/// Tracks the set of watched conversation keys and the UI-open conversation.
///
/// All mutation goes through `&mut self`; callers acquire the lock from the
/// global `CONV_MANAGER` static.
#[derive(Debug, Default)]
pub struct ConversationManager {
    active: HashMap<Arc<str>, WatchState>,
    renewals: HashMap<Arc<str>, u64>,
    open_conversation: Option<Arc<str>>,
}

impl ConversationManager {
    /// Returns the current state for a key, if tracked.
    fn state(&self, key: &str) -> Option<WatchState> {
        self.active.get(key).copied()
    }

    /// Sets the state for a key.
    fn set_state(&mut self, key: &str, state: WatchState) {
        self.active
            .entry(Arc::from(key))
            .and_modify(|s| *s = state)
            .or_insert(state);
    }

    /// Removes a key from tracking.
    fn remove(&mut self, key: &str) {
        self.active.remove(key);
        self.renewals.remove(key);
    }

    /// Check if a conversation watch is active or waiting for an explicit re-arm.
    pub fn is_active(&self, key: &str) -> bool {
        matches!(
            self.state(key),
            Some(WatchState::Opening | WatchState::Open | WatchState::NeedsRearm)
        )
    }
}

/// Pins the conversation record open (read-only — scoped writes swap their
/// own writer in; see `store::with_record`) and installs a watch over the
/// full subkey range. Pinning is idempotent, so re-watching an already-pinned
/// record just refreshes the watch params. Watching `None` covers subkeys of
/// members added to the roster later, and our own member subkeys — veilid
/// does not echo a node's own writes back to it, so watching them costs
/// nothing locally while letting other devices' writes through.
async fn open_and_watch(record_key: &RecordKey) -> Result<()> {
    store::pin_record(record_key, None).await?;
    if let Err(e) = store::watch_record(record_key, None).await {
        nonfatal!(
            "chat.manager",
            "Failed to unpin conversation record after watch failure",
            store::unpin_record(record_key).await.map(|_| ())
        );
        return Err(e);
    }
    Ok(())
}

/// Unpins the conversation record. Closing it also cancels its outbound
/// watch, so no explicit cancel is needed.
async fn unpin_conversation(record_key: &RecordKey) {
    nonfatal!(
        "chat.manager",
        "Failed to unpin conversation record",
        store::unpin_record(record_key).await.map(|_| ())
    );
}

fn schedule_watch_rearm_once(key: Arc<str>, guard: session::SessionGuard) {
    let now = now_epoch_millis();
    let mut manager = write_manager();
    if manager
        .renewals
        .get(key.as_ref())
        .is_some_and(|last| now.saturating_sub(*last) < RENEWAL_DEDUP_INTERVAL_MS)
    {
        return;
    }
    manager.renewals.insert(Arc::clone(&key), now);
    drop(manager);
    session::spawn_session_scoped(async move {
        sleep_ms(RENEWAL_DEDUP_INTERVAL_MS).await;
        if guard.is_current() {
            let _ = watch_conversation_inner(&key, &guard, false).await;
        }
    });
}

fn schedule_conversation_sweep(key: Arc<str>, guard: session::SessionGuard) {
    session::spawn_session_scoped(async move {
        if guard.is_current() {
            sweep_rearmed_conversation(key, guard).await;
        }
    });
}

fn fail_opening_watch(
    key: &str,
    guard: session::SessionGuard,
    retain_for_rearm: bool,
    retry_once: bool,
) {
    let mut manager = write_manager();
    if !guard.is_current() {
        if manager.state(key) == Some(WatchState::Opening) {
            manager.set_state(key, WatchState::Closing);
        }
        return;
    }
    if manager.state(key) != Some(WatchState::Opening) {
        return;
    }
    if !retain_for_rearm {
        manager.remove(key);
        return;
    }
    manager.set_state(key, WatchState::NeedsRearm);
    if retry_once {
        manager.renewals.remove(key);
    }
    drop(manager);
    if retry_once {
        schedule_watch_rearm_once(Arc::from(key), guard);
    }
}

// ── Watch lifecycle ─────────────────────────────────────────────────

async fn watch_conversation_inner(
    key: &str,
    guard: &session::SessionGuard,
    retry_once: bool,
) -> bool {
    let _lifecycle = WATCH_LOCKS.lock(Arc::from(key)).await;
    if !guard.is_current() {
        return false;
    }

    let needs_rearm = {
        let mut manager = write_manager();
        match manager.state(key) {
            Some(WatchState::NeedsRearm) => {
                manager.set_state(key, WatchState::Opening);
                true
            }
            Some(_) => return false,
            None if !retry_once => return false,
            None if manager.active.len() >= config::chat::MAX_CONVERSATION_WATCHES => {
                debug!("Conversation watch limit reached; not arming {key}");
                return false;
            }
            None => {
                manager.set_state(key, WatchState::Opening);
                false
            }
        }
    };

    let record_key = match parse_record_key(key) {
        Ok(record_key) => record_key,
        Err(error) => {
            let mut manager = write_manager();
            if manager.state(key) == Some(WatchState::Opening) {
                manager.remove(key);
            }
            drop(manager);
            report_nonfatal("chat.manager", "Invalid conversation watch key", error);
            return false;
        }
    };

    if let Err(error) = open_and_watch(&record_key).await {
        store::report_nonfatal_unless_network(
            "chat.manager",
            "Failed to open/watch DHT values",
            &error,
        );
        // Retain `NeedsRearm` even for a first arm: dropping the key would also
        // drop its `ValueChange` routing (`is_watched_conversation`) and nothing
        // would re-arm it until an unrelated index pull. The recovery is
        // bounded — the scheduled re-arm runs with `retry_once = false`, so a
        // failure there re-arms nothing further: at most one extra pin+watch
        // per key, never a storm.
        fail_opening_watch(key, *guard, true, retry_once);
        return false;
    }

    if !guard.is_current() {
        // Teardown owns the tracked Closing entry and will retry unpinning.
        write_manager().set_state(key, WatchState::Closing);
        return false;
    }

    let mut manager = write_manager();
    if manager.state(key) == Some(WatchState::Opening) {
        manager.set_state(key, WatchState::Open);
    }
    drop(manager);
    if needs_rearm {
        schedule_conversation_sweep(Arc::from(key), *guard);
    }
    !needs_rearm
}

/// Idempotently arms the session watch for a conversation. Safe to call from
/// any path; an open key returns immediately, while `NeedsRearm` retries setup.
pub async fn ensure_watched(key: &str) {
    let _ = ensure_watched_if_new(key).await;
}

/// Returns true when this call changed an untracked conversation into an
/// active watch; callers that opened the conversation can then catch up.
pub async fn ensure_watched_if_new(key: &str) -> bool {
    let guard = session::SessionGuard::capture();
    watch_conversation_inner(key, &guard, true).await
}

/// Check if a record key string is a session-watched conversation.
pub fn is_watched_conversation(key: &str) -> bool {
    read_manager().is_active(key)
}

/// Snapshot of watched or re-arm-pending conversation keys.
pub fn watched_conversation_keys() -> Vec<Arc<str>> {
    read_manager()
        .active
        .iter()
        .filter(|(_, s)| {
            matches!(
                s,
                WatchState::Opening | WatchState::Open | WatchState::NeedsRearm
            )
        })
        .map(|(k, _)| Arc::clone(k))
        .collect()
}

/// Stops watching a conversation and unpins its record. Used when a
/// conversation disappears from the `ConversationIndex` (deleted on this or
/// another device) — not by the UI, which only marks open/closed.
pub async fn unwatch_conversation(key: &str) {
    let guard = session::SessionGuard::capture();
    let _lifecycle = WATCH_LOCKS.lock(Arc::from(key)).await;
    if !guard.is_current() {
        return;
    }

    let should_cancel = {
        let mut mgr = write_manager();
        let tracked = mgr.state(key).is_some();
        if tracked {
            mgr.set_state(key, WatchState::Closing);
        }
        tracked
    };

    if should_cancel && let Ok(record_key) = parse_record_key(key) {
        unpin_conversation(&record_key).await;
    }
    if should_cancel {
        write_manager().remove(key);
    }
}

// ── UI open-conversation tracking ───────────────────────────────────

/// Marks a conversation as on-screen. Presence-style signal handlers
/// (typing) gate on this rather than on watch state.
pub fn set_open_conversation(key: &str) {
    write_manager().open_conversation = Some(Arc::from(key));
}

/// Clears the open-conversation mark if it still points at `key`.
pub fn clear_open_conversation(key: &str) {
    let mut mgr = write_manager();
    if mgr.open_conversation.as_deref() == Some(key) {
        mgr.open_conversation = None;
    }
}

/// Check if a conversation is currently on screen.
pub fn is_open_conversation(key: &str) -> bool {
    read_manager().open_conversation.as_deref() == Some(key)
}

// ── Renewal ─────────────────────────────────────────────────────────

/// Returns `true` if a renewal should proceed for the given key,
/// updating the last-renewal timestamp. Returns `false` if a renewal
/// was already initiated within `RENEWAL_DEDUP_INTERVAL_MS`.
pub fn should_renew(key: &Arc<str>) -> bool {
    let now = now_epoch_millis();
    let mut manager = write_manager();
    if !manager.is_active(key) {
        return false;
    }
    let ConversationManager {
        active, renewals, ..
    } = &mut *manager;
    renewals.retain(|renewal_key, _| {
        matches!(
            active.get(renewal_key.as_ref()),
            Some(WatchState::Opening | WatchState::Open | WatchState::NeedsRearm)
        )
    });
    if let Some(&last) = manager.renewals.get(key.as_ref())
        && now.saturating_sub(last) < RENEWAL_DEDUP_INTERVAL_MS
    {
        debug!("Skipping duplicate watch renewal for {key} (last {last}ms ago)");
        return false;
    }
    manager.renewals.insert(Arc::clone(key), now);
    true
}

/// Renews an expired conversation watch. An inactive result triggers one
/// immediate re-open and one bounded delayed retry if re-arming fails.
pub async fn renew_watch(key: &str) {
    renew_watch_inner(key, session::SessionGuard::capture(), true).await;
}

async fn renew_watch_inner(key: &str, guard: session::SessionGuard, retry_once: bool) {
    let _lifecycle = WATCH_LOCKS.lock(Arc::from(key)).await;
    if !guard.is_current() || !is_watched_conversation(key) {
        return;
    }
    let Ok(record_key) = parse_record_key(key) else {
        return;
    };

    if read_manager().state(key) == Some(WatchState::NeedsRearm) {
        let should_rearm = claim_state(key, WatchState::NeedsRearm, WatchState::Opening);
        if should_rearm {
            rearm_conversation_watch(key, &record_key, guard, retry_once).await;
        }
        return;
    }

    if let Err(error) = store::watch_record(&record_key, None).await {
        let inactive = matches!(
            error.downcast_ref::<store::WatchError>(),
            Some(store::WatchError::Inactive)
        );
        // `Inactive` must reach the error surface (push delivery for this
        // conversation is dead until the re-arm below); a transient network
        // failure during the renewal does not — the re-arm machinery here
        // retries it, and the watcher's own ticks retry on their cadence.
        store::report_nonfatal_unless_network("chat.manager", "Failed to renew DHT watch", &error);
        if inactive && guard.is_current() {
            let should_rearm = claim_state(key, WatchState::Open, WatchState::Opening);
            if should_rearm {
                rearm_conversation_watch(key, &record_key, guard, retry_once).await;
            }
        }
    }
}

async fn rearm_conversation_watch(
    key: &str,
    record_key: &RecordKey,
    guard: session::SessionGuard,
    retry_once: bool,
) {
    if let Err(error) = store::unpin_record(record_key).await {
        report_nonfatal(
            "chat.manager",
            "Failed to close inactive DHT watch before re-arming",
            format!("{key}: {error:#}"),
        );
        fail_opening_watch(key, guard, true, retry_once);
        return;
    }

    if !guard.is_current() {
        claim_state(key, WatchState::Opening, WatchState::Closing);
        return;
    }
    if read_manager().state(key) != Some(WatchState::Opening) {
        return;
    }

    if let Err(error) = open_and_watch(record_key).await {
        report_nonfatal(
            "chat.manager",
            "Failed to re-open DHT watch after inactive renewal",
            format!("{key}: {error:#}"),
        );
        fail_opening_watch(key, guard, true, retry_once);
        return;
    }

    if !guard.is_current() {
        claim_state(key, WatchState::Opening, WatchState::Closing);
        return;
    }

    if claim_state(key, WatchState::Opening, WatchState::Open) {
        schedule_conversation_sweep(Arc::from(key), guard);
    }
}

/// Cancel all active DHT watches and clear the tracked set.
pub async fn cancel_all_watches() {
    let keys = {
        let mut manager = write_manager();
        manager.open_conversation = None;
        let keys = manager.active.keys().cloned().collect::<Vec<_>>();
        for state in manager.active.values_mut() {
            *state = WatchState::Closing;
        }
        drop(manager);
        keys
    };

    // Renewal timestamps are session-scoped; drop them so keys from this
    // session don't linger into the next login.
    write_manager().renewals.clear();

    if keys.is_empty() {
        return;
    }

    for key_str in &keys {
        let _lifecycle = WATCH_LOCKS.lock(Arc::clone(key_str)).await;
        if read_manager().state(key_str) != Some(WatchState::Closing) {
            continue;
        }
        match parse_record_key(key_str) {
            Ok(record_key) => unpin_conversation(&record_key).await,
            Err(e) => report_nonfatal(
                "chat.manager",
                "Failed to parse own record key during cancel",
                e,
            ),
        }
        write_manager().remove(key_str);
    }
}
