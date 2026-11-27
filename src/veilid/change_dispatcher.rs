//! Coalescing dispatcher for DHT `ValueChange` events.
//!
//! Prevents unbounded spawn fan-out by:
//! - limiting concurrent handlers
//! - coalescing duplicate keys while one is already in flight
//! - collapsing overflow into a bounded forced-resync sweep instead of
//!   dropping changes when the dirty-key set is saturated

use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    mem,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    chat::{
        conversation::ConversationIndex,
        manager::{is_watched_conversation, renew_watch, should_renew},
        record, storage, watcher,
    },
    config,
    helpers::{
        async_utils::sleep_ms, concurrency::map_concurrent, spawn, store_lock::recover_poison,
    },
    session::{epoch as session, phase::SESSION},
    storage::{background_errors::report_nonfatal, store_trait::load_local},
    user::{self, ctx::UserCtx, watcher as user_watcher},
    veilid::store,
};
use dioxus::{logger::tracing::debug, prelude::ReadableExt};
use tokio::sync::Notify;
use veilid_core::{ValueSubkeyRangeSet, VeilidValueChange};

static CHANGE_DISPATCHER: LazyLock<Arc<DirtyChangeDispatcher>> =
    LazyLock::new(DirtyChangeDispatcher::new);

struct CoalescedEntry<T> {
    pending: Option<T>,
    queued: bool,
    in_flight: bool,
    force_resync: bool,
}

struct CoalescedState<K, T> {
    queue: VecDeque<K>,
    entries: HashMap<K, CoalescedEntry<T>>,
    overflow_resync_queued: bool,
    overflow_resync_in_flight: bool,
}

impl<K, T> Default for CoalescedState<K, T> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            entries: HashMap::new(),
            overflow_resync_queued: false,
            overflow_resync_in_flight: false,
        }
    }
}

enum CoalescedJob<K, T> {
    Item(K, T, bool),
    OverflowResync,
}

struct EnqueueResult {
    notify: bool,
    overflow_started: bool,
    /// `true` when the key already existed in the entries map before this
    /// enqueue (in-flight or had pending state). Callers use this to decide
    /// whether to debounce the dispatch notification.
    had_prior_entry: bool,
}

impl<K, T> CoalescedState<K, T>
where
    K: Clone + Eq + Hash,
{
    fn enqueue(
        &mut self,
        key: K,
        value: T,
        force_resync: bool,
        capacity: usize,
        merge_pending: impl FnOnce(&mut T, &T),
    ) -> EnqueueResult {
        let mut notify = false;
        let mut overflow_started = false;

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.force_resync |= force_resync;
            if let Some(pending) = entry.pending.as_mut() {
                merge_pending(pending, &value);
            } else {
                entry.pending = Some(value);
            }
            if !entry.queued && !entry.in_flight {
                entry.queued = true;
                self.queue.push_back(key);
                notify = true;
            }
            return EnqueueResult {
                notify,
                overflow_started,
                had_prior_entry: true,
            };
        }

        if self.entries.len() < capacity {
            self.entries.insert(
                key.clone(),
                CoalescedEntry {
                    pending: Some(value),
                    queued: true,
                    in_flight: false,
                    force_resync,
                },
            );
            self.queue.push_back(key);
            return EnqueueResult {
                notify: true,
                overflow_started,
                had_prior_entry: false,
            };
        }

        if self.overflow_resync_in_flight {
            self.overflow_resync_queued = true;
        } else if !self.overflow_resync_queued {
            self.overflow_resync_queued = true;
            notify = true;
            overflow_started = true;
        }

        EnqueueResult {
            notify,
            overflow_started,
            had_prior_entry: false,
        }
    }

    fn take_next(&mut self) -> Option<CoalescedJob<K, T>> {
        if self.overflow_resync_queued {
            self.overflow_resync_queued = false;
            self.overflow_resync_in_flight = true;
            return Some(CoalescedJob::OverflowResync);
        }

        while let Some(key) = self.queue.pop_front() {
            let mut remove = false;
            let mut job = None;
            {
                let Some(entry) = self.entries.get_mut(&key) else {
                    continue;
                };
                entry.queued = false;
                if let Some(change) = entry.pending.take() {
                    entry.in_flight = true;
                    job = Some(CoalescedJob::Item(
                        key.clone(),
                        change,
                        mem::take(&mut entry.force_resync),
                    ));
                } else if !entry.in_flight {
                    remove = true;
                }
            }
            if remove {
                self.entries.remove(&key);
            }
            if let Some(job) = job {
                return Some(job);
            }
        }

        None
    }

    fn finish_item(&mut self, key: &K) -> bool {
        let mut requeue = false;
        let mut remove = false;
        {
            let Some(entry) = self.entries.get_mut(key) else {
                return false;
            };
            entry.in_flight = false;
            if entry.pending.is_some() {
                if !entry.queued {
                    entry.queued = true;
                    requeue = true;
                }
            } else if !entry.queued {
                remove = true;
            }
        }
        if requeue {
            self.queue.push_back(key.clone());
        }
        if remove {
            self.entries.remove(key);
        }
        requeue
    }

    const fn finish_overflow(&mut self) -> bool {
        self.overflow_resync_in_flight = false;
        self.overflow_resync_queued
    }
}

struct DirtyChangeDispatcher {
    state: Mutex<CoalescedState<Arc<str>, VeilidValueChange>>,
    notify: Notify,
    /// Set while a coalesce debounce timer is in-flight. Prevents spawning
    /// duplicate timers; cleared when the timer fires and calls `notify_one`.
    debounce_active: AtomicBool,
}

type DispatchJob = CoalescedJob<Arc<str>, VeilidValueChange>;

impl DirtyChangeDispatcher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CoalescedState::default()),
            notify: Notify::new(),
            debounce_active: AtomicBool::new(false),
        })
    }

    /// A change with no value payload (coalesced multi-subkey hint or an
    /// inspect-found change) carries nothing to reconcile, so it must force
    /// a full local resync instead of a value merge.
    fn needs_forced_resync(change: &VeilidValueChange) -> bool {
        change.count == 0 || change.subkeys.is_empty()
    }

    fn enqueue(&self, change: VeilidValueChange) -> EnqueueResult {
        let key = Arc::from(change.key.to_string());
        let force_resync = Self::needs_forced_resync(&change);
        let result = {
            let mut guard = recover_poison(self.state.lock());
            guard.enqueue(
                key,
                change,
                force_resync,
                config::dispatcher::CHANNEL_CAPACITY,
                merge_pending_change,
            )
        };

        if result.overflow_started {
            report_nonfatal(
                "veilid.change_dispatcher",
                "Dirty-key capacity exceeded; scheduling forced conversation resync",
                format!("capacity={}", config::dispatcher::CHANNEL_CAPACITY),
            );
        }

        result
    }

    fn take_next(&self) -> Option<DispatchJob> {
        recover_poison(self.state.lock()).take_next()
    }
}

/// Schedules a coalesced dispatch notification after
/// [`config::dispatcher::COALESCE_WINDOW_MS`].
///
/// At most one debounce timer runs at a time. If a timer is already active,
/// subsequent calls are no-ops — the pending events keep merging in the
/// [`CoalescedState`] and will be dispatched when the existing timer fires.
fn debounce_notify(dispatcher: &Arc<DirtyChangeDispatcher>) {
    if dispatcher
        .debounce_active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let d = Arc::clone(dispatcher);
        spawn(async move {
            sleep_ms(config::dispatcher::COALESCE_WINDOW_MS).await;
            d.debounce_active.store(false, Ordering::Release);
            d.notify.notify_one();
            debug!("coalesce debounce timer fired");
        });
    }
}

fn merge_pending_change(existing: &mut VeilidValueChange, change: &VeilidValueChange) {
    existing.subkeys = existing.subkeys.union(&change.subkeys);
    existing.count = existing.count.min(change.count);
    // A merge unions several subkeys' ranges, so the single delivered value no
    // longer names one: the watcher's load reads each changed subkey back
    // itself (see `store::PushedValue` for the unmerged case).
    existing.value = None;
}

async fn handle_change(key: Arc<str>, change: Box<VeilidValueChange>, force_resync: bool) {
    let guard = session::SessionGuard::capture();

    // Core committed this value locally before reporting it, so hand it to the
    // watcher's load instead of making that load fetch the bytes back over the
    // network. `None` when the change carries none (see `store::pushed_value`).
    let pushed = store::pushed_value(&change);

    // A watch-expiry signal (count == 0) on a watched conversation warrants
    // renewal regardless of local-store state.
    if change.count == 0 && is_watched_conversation(&key) && should_renew(&key) {
        let renew_key = Arc::clone(&key);
        spawn(async move {
            renew_watch(&renew_key).await;
        });
    }

    // The user's own identity record is never a conversation; skip the
    // conversation-store lookup and go straight to the identity watcher.
    let is_own_identity = SESSION
        .peek()
        .authenticated_user()
        .is_some_and(|user| user.dht_key() == key.as_ref());
    if !is_own_identity
        && user::child_watch::feature_for_key(&key).is_none()
        && let Some(ctx) = UserCtx::try_current()
        && dispatch_conversation(
            &ctx,
            guard,
            Arc::clone(&key),
            force_resync || change.count == 0,
            Some(change.subkeys.clone()),
            pushed,
            "Failed to load local conversation while dispatching DHT change",
        )
        .await
    {
        return;
    }

    if guard.is_current() {
        user_watcher::handle_dht_change(key, change).await;
    }
}

/// Loads the local conversation for `key` and wakes its watcher.
///
/// Shared by normal change handling and overflow forced-resync: with
/// `force_resync` (or no announced set) the watcher receives the full
/// conversation subkey range, otherwise `change_subkeys`. `pushed` carries the
/// value the change delivered, so the watcher's load of that one subkey costs
/// no network read. Returns `true` when the key was handled as a conversation —
/// dispatched, or abandoned because the session ended — so the caller stops
/// handling this key; `false` means fall through (e.g. to the identity
/// watcher).
async fn dispatch_conversation(
    ctx: &UserCtx,
    guard: session::SessionGuard,
    key: Arc<str>,
    force_resync: bool,
    change_subkeys: Option<ValueSubkeyRangeSet>,
    pushed: Option<store::PushedValue>,
    err_context: &'static str,
) -> bool {
    match storage::load_for_account(&ctx.account_id, &key).await {
        Ok(conv) if conv.has_conv_key() => {
            if !guard.is_current() {
                return true;
            }
            let subkeys = if force_resync {
                record::full_conversation_subkeys(conv.roster.members.len())
            } else {
                change_subkeys.unwrap_or_default()
            };
            watcher::handle_conversation_change_loaded(
                ctx.clone(),
                &key,
                &conv,
                subkeys,
                pushed,
                guard,
            )
            .await;
            true
        }
        Ok(_) => false,
        Err(e) => {
            report_nonfatal(
                "veilid.change_dispatcher",
                err_context,
                format!("{key}: {e}"),
            );
            false
        }
    }
}

/// Overflow recovery: the dirty-key set saturated, so individual changes may
/// have been dropped. Resync the identity record and every local conversation.
async fn resync_local_records() {
    user_watcher::resync_identity().await;

    let guard = session::SessionGuard::capture();
    let Some(ctx) = UserCtx::try_current() else {
        return;
    };

    let index: ConversationIndex = match load_local(ctx.storage_key()).await {
        Ok(index) => index,
        Err(e) => {
            report_nonfatal(
                "veilid.change_dispatcher",
                "Failed to load ConversationIndex for forced resync",
                e,
            );
            return;
        }
    };

    let keys: Vec<Arc<str>> = index
        .conversations
        .keys()
        .map(|key| Arc::from(key.as_str()))
        .collect();
    // Unbounded: each key's dispatch serialises on that conversation's own
    // lifecycle lock, so the sweep fan-out adds no shared contention.
    map_concurrent(keys, async move |key| {
        if !guard.is_current() {
            return;
        }
        dispatch_conversation(
            &ctx,
            guard,
            key,
            true,
            None,
            None,
            "Failed to load conversation during forced resync",
        )
        .await;
    })
    .await;
}

pub fn init() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if !STARTED.swap(true, Ordering::AcqRel) {
        spawn_supervisor(Arc::clone(&CHANGE_DISPATCHER));
    }
}

fn spawn_supervisor(dispatcher: Arc<DirtyChangeDispatcher>) {
    spawn(async move {
        loop {
            let Some(job) = dispatcher.take_next() else {
                dispatcher.notify.notified().await;
                continue;
            };

            let dispatcher = Arc::clone(&dispatcher);
            spawn(async move {
                match job {
                    CoalescedJob::Item(key, change, force_resync) => {
                        handle_change(Arc::clone(&key), Box::new(change), force_resync).await;
                        if recover_poison(dispatcher.state.lock()).finish_item(&key) {
                            debounce_notify(&dispatcher);
                        }
                    }
                    CoalescedJob::OverflowResync => {
                        resync_local_records().await;
                        if recover_poison(dispatcher.state.lock()).finish_overflow() {
                            dispatcher.notify.notify_one();
                        }
                    }
                }
            });
        }
    });
}

pub fn enqueue(change: VeilidValueChange) {
    let dispatcher = &*CHANGE_DISPATCHER;
    let result = dispatcher.enqueue(change);
    if !result.notify {
        return;
    }
    if result.had_prior_entry {
        debounce_notify(dispatcher);
    } else {
        dispatcher.notify.notify_one();
    }
}
