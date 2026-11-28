//! Login-time conversation watch arming and catch-up sweep.
//!
//! The session — not the UI — owns conversation watches: every conversation
//! in the `ConversationIndex` is watched from login to logout, so remote
//! writes (other people's messages, and this account's writes from other
//! devices) arrive as native `ValueChanged` events no matter which screen is
//! open. Veilid keeps the watches honest — auto-renewal, reattach
//! reconciliation, and missed-change delivery are native.
//!
//! Ordering is subscribe-then-snapshot: watches are armed first, then a
//! one-shot sweep inspects each conversation (`DHTReportScope::SyncGet`,
//! one RPC per conversation) for subkeys that are newer on the network than
//! the local cache and routes only those through the normal change pipeline.
//! Anything written during the sweep is caught by the just-armed watches.

use std::{
    cmp::Reverse,
    collections::HashSet,
    sync::{Arc, Mutex},
};

use veilid_core::DHTReportScope;

use crate::{
    chat::{
        contact::{ContactList, ContactStatus},
        conversation::ConversationIndex,
        feed::MessageStatus,
        manager,
        session_flow::retry_pending_contact,
        storage,
        watcher::handle_conversation_change,
    },
    config,
    helpers::concurrency,
    helpers::{async_utils::retry_after_session_stable_delay, store_lock::recover_poison},
    models::pending_operation::{OpType, PendingOperation},
    session::epoch as session,
    storage::{background_errors::report_nonfatal, store_trait::load_local},
    user::ctx::UserCtx,
    veilid::{core::parse_record_key, store},
};

use super::{full_conversation_subkeys, hydrate_conversation, store_failed_hydrations};

/// Arms watches for every indexed conversation, hydrates any the local store
/// doesn't know yet, then sweeps known ones for missed writes.
///
/// `sweep_all` selects the login pass (hydrate + sweep everything, once per
/// login after identity resync) from the incremental reconcile pass (arm new
/// keys, unwatch removed ones, sweep only the newly armed ones). Both are the
/// same pass — the bool only feeds `sync_watch_set` — so they live in one
/// function instead of two byte-identical wrappers.
pub async fn arm_and_catch_up(sweep_all: bool) {
    if sweep_all {
        reconcile_pending_local_work().await;
    }
    let Some((ctx, guard, sweep_keys)) = sync_watch_set(sweep_all).await else {
        return;
    };
    sweep_known(&ctx, guard, sweep_keys).await;
}

/// Re-queues durable local work whose process stopped between its local commit
/// and queue enqueue. Queue squashing folds these IDs into any existing feed op.
async fn reconcile_pending_local_work() {
    let Some(ctx) = UserCtx::try_current() else {
        return;
    };
    let guard = session::SessionGuard::capture();

    match storage::list_manifest_keys(&ctx.account_id).await {
        Ok(keys) => {
            for key in keys {
                if !guard.is_current() {
                    return;
                }
                if key.is_empty() {
                    continue;
                }
                let conv = match storage::load_for_account(&ctx.account_id, &key).await {
                    Ok(conv) => conv,
                    Err(error) => {
                        report_nonfatal(
                            "chat.catchup",
                            "Failed to load conversation for pending reconciliation",
                            error,
                        );
                        continue;
                    }
                };
                let message_ids: Vec<_> = conv
                    .messages
                    .iter()
                    .filter(|message| message.status == MessageStatus::Pending)
                    .map(|message| message.msg_id.as_ref())
                    .collect();
                if message_ids.is_empty() {
                    continue;
                }
                if let Err(error) = ctx
                    .enqueue_for_target(
                        OpType::ConvFeedSync,
                        PendingOperation::conv_feed_payload(message_ids),
                        &key,
                    )
                    .await
                {
                    report_nonfatal(
                        "chat.catchup",
                        "Failed to reconcile pending conversation feed",
                        error,
                    );
                }
            }
        }
        Err(error) => report_nonfatal(
            "chat.catchup",
            "Failed to enumerate conversations for pending reconciliation",
            error,
        ),
    }

    let contacts = match load_local::<ContactList>(ctx.storage_key()).await {
        Ok(contacts) => contacts,
        Err(error) => {
            report_nonfatal(
                "chat.catchup",
                "Failed to load contacts for pending reconciliation",
                error,
            );
            return;
        }
    };
    for contact in contacts.contacts.values().filter(|contact| {
        contact.status == ContactStatus::Pending && contact.conversation_record_key.is_empty()
    }) {
        if !guard.is_current() {
            return;
        }
        if let Err(error) = retry_pending_contact(contact).await {
            report_nonfatal(
                "chat.catchup",
                "Failed to reconcile pending contact setup",
                error,
            );
        }
    }
}

/// Sweeps one already-local conversation after its DHT watch is re-armed.
pub async fn sweep_rearmed_conversation(key: Arc<str>, guard: session::SessionGuard) {
    let Some(ctx) = UserCtx::try_current() else {
        return;
    };
    if !guard.is_current() {
        return;
    }
    sweep_conversation(&ctx, &key, guard).await;
}

/// Sweeps the conversations whose index `last_message_at` advanced in a pull
/// that just landed. A nudge/poll reports on the *index*, but a message lives
/// in the conversation record: if that record's `ValueChange` was lost, only
/// this sweep sees the write. Scoping to advanced entries keeps the cost at
/// one `SyncGet` per message instead of one per watched conversation.
pub async fn sweep_advanced_conversations(
    ctx: &UserCtx,
    index_before: Option<&ConversationIndex>,
    index_after: &ConversationIndex,
    guard: session::SessionGuard,
) {
    let advanced: Vec<String> = index_after
        .conversations
        .iter()
        .filter(|(key, entry)| {
            index_before
                .and_then(|before| before.conversations.get(key.as_str()))
                .is_none_or(|before| entry.last_message_at > before.last_message_at)
        })
        .map(|(key, _)| key.clone())
        .collect();
    sweep_known(ctx, guard, advanced).await;
}

async fn sweep_known(ctx: &UserCtx, guard: session::SessionGuard, known: Vec<String>) {
    // Unbounded: each sweep serialises on its own conversation lifecycle lock,
    // so the fan-out adds no shared contention — it just stops being waves.
    concurrency::map_concurrent(known, async |key: String| {
        if guard.is_current() {
            sweep_conversation(ctx, &key, guard).await;
        }
    })
    .await;
}

/// Shared arm/unwatch/hydrate pass. Returns the user and session guard that
/// owned the pass, plus keys requiring a catch-up sweep.
async fn sync_watch_set(sweep_all: bool) -> Option<(UserCtx, session::SessionGuard, Vec<String>)> {
    let ctx = UserCtx::try_current()?;
    let guard = session::SessionGuard::capture();

    let Ok(index) = load_local::<ConversationIndex>(ctx.account_id.as_bytes()).await else {
        report_nonfatal(
            "chat.catchup",
            "Failed to load ConversationIndex for watch arming",
            "load_local error",
        );
        return None;
    };

    let desired = desired_watch_keys(&index);
    let watched_before = manager::watched_conversation_keys();

    // Unwatch conversations that vanished from the index (deleted here or on
    // another device) or fell out of the watch cap.
    {
        let desired_set: HashSet<&str> = desired.iter().map(String::as_str).collect();
        for key in &watched_before {
            if !desired_set.contains(key.as_ref()) {
                if !guard.is_current() {
                    return None;
                }
                manager::unwatch_conversation(key).await;
            }
        }
    }

    // Arm first (subscribe-then-snapshot), partitioning known from unknown.
    // Concurrent: each arm is a pin + watch round trip and each first-time key
    // costs a store read, so a serial pass scales login latency with the
    // conversation count. Per-key safety is unchanged — `ensure_watched`
    // serializes on the key's own lifecycle lock and the store read holds only
    // that row's lock.
    let sweep_keys = Mutex::new(Vec::new());
    let unknown = Mutex::new(Vec::new());
    // Unbounded arm pass (helpers::concurrency::map_concurrent): each arm is
    // a pin + watch round trip and each first-time key costs a store read, so
    // a serial pass would scale login latency with the conversation count.
    concurrency::map_concurrent(desired, {
        let sweep_keys = &sweep_keys;
        let unknown = &unknown;
        let watched_before = &watched_before;
        let ctx = &ctx;
        async |key: String| {
            if !guard.is_current() {
                return;
            }
            manager::ensure_watched(&key).await;
            match storage::load_for_account(&ctx.account_id, &key).await {
                Ok(conv) if conv.has_conv_key() => {
                    if sweep_all
                        || !watched_before
                            .iter()
                            .any(|watched| watched.as_ref() == key.as_str())
                    {
                        recover_poison(sweep_keys.lock()).push(key);
                    }
                }
                _ => recover_poison(unknown.lock()).push(key),
            }
        }
    })
    .await;
    if !guard.is_current() {
        return None;
    }
    let sweep_keys = recover_poison(sweep_keys.into_inner());
    let unknown = recover_poison(unknown.into_inner());

    hydrate_unknown(&ctx, &guard, unknown).await;
    Some((ctx, guard, sweep_keys))
}

/// Index keys to watch, capped at the most recently active
/// `MAX_CONVERSATION_WATCHES` conversations.
fn desired_watch_keys(index: &ConversationIndex) -> Vec<String> {
    let mut entries: Vec<(&str, u64)> = index
        .conversations
        .values()
        .map(|e| (e.conversation_record_key.as_str(), e.last_message_at))
        .collect();
    if entries.len() > config::chat::MAX_CONVERSATION_WATCHES {
        entries.sort_by_key(|&(_, last_message_at)| Reverse(last_message_at));
        entries.truncate(config::chat::MAX_CONVERSATION_WATCHES);
    }
    entries.into_iter().map(|(k, _)| k.to_owned()).collect()
}

/// Cold-hydrates conversations present in the index but absent (or key-less)
/// in the local store — e.g. created on another device while this one was
/// offline. Runs with unbounded concurrency; failures are non-fatal and
/// retried by the failed-hydrations path.
async fn hydrate_unknown(ctx: &UserCtx, guard: &session::SessionGuard, keys: Vec<String>) {
    if keys.is_empty() {
        return;
    }
    let failed = Mutex::new(Vec::new());
    let session_guard = *guard;
    // Unbounded: every unknown key is cold-hydrated at once.
    concurrency::map_concurrent(keys, {
        let failed = &failed;
        let ctx = &ctx;
        async |key: String| {
            if !session_guard.is_current() {
                return;
            }
            let Ok(manifest_key) = parse_record_key(&key) else {
                return;
            };
            if let Err(e) = hydrate_conversation(manifest_key, &key, ctx, session_guard).await {
                report_nonfatal(
                    "chat.catchup",
                    "Failed to hydrate conversation discovered in index",
                    format!("{key}: {e:#}"),
                );
                recover_poison(failed.lock()).push(key);
            }
        }
    })
    .await;
    if !session_guard.is_current() {
        return;
    }
    let failed = recover_poison(failed.into_inner());
    if !failed.is_empty()
        && let Err(e) = store_failed_hydrations(ctx, session_guard, &failed).await
    {
        report_nonfatal(
            "chat.catchup",
            "Failed to persist index-discovered hydration failures",
            e,
        );
    }
}

/// Inspects one conversation for subkeys newer on the network than the local
/// cache (one `SyncGet` RPC) and routes them through the normal conversation
/// change pipeline, which force-reads exactly those subkeys.
async fn sweep_conversation(ctx: &UserCtx, key: &str, guard: session::SessionGuard) {
    let Ok(record_key) = parse_record_key(key) else {
        return;
    };
    let Ok(conv) = storage::load_for_account(&ctx.account_id, key).await else {
        return;
    };
    let subkeys = full_conversation_subkeys(conv.roster.members.len());
    // The retry re-runs the inspect after `RETRY_DELAY_MS`; nothing is written
    // from a stale session either way — `handle_conversation_change` re-checks
    // the epoch before it merges anything.
    let report = retry_after_session_stable_delay(guard, || {
        store::inspect_record(&record_key, Some(subkeys.clone()), DHTReportScope::SyncGet)
    })
    .await;
    let Some(report) = report else {
        return; // Session changed during the retry wait.
    };
    match report {
        Ok(report) => {
            let newer = report.newer_online_subkeys();
            if !newer.is_empty() {
                // The inspect reports subkey ranges only — no delivered value
                // to decode — so the load reads the newer subkeys back.
                handle_conversation_change(Arc::from(key), newer, None, guard).await;
            }
        }
        Err(error) => {
            store::report_nonfatal_unless_network(
                "chat.catchup",
                "Catch-up inspect failed",
                &error,
            );
        }
    }
}
