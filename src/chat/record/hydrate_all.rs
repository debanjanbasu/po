//! Bulk hydration of every conversation in the `ConversationIndex` (pairing path):
//! locally-polled concurrent futures with progress callbacks and a global timeout.

use std::{collections::HashSet, future::Future, time::Duration};

use futures_util::{StreamExt, stream::FuturesUnordered};
use tracing::warn;

use crate::{
    chat::conversation::ConversationIndex,
    config,
    helpers::async_utils::timeout,
    session::epoch::SessionGuard,
    storage::{
        background_errors::{push_background_error, report_nonfatal},
        store_trait::load_local,
    },
    user::ctx::UserCtx,
    veilid::core::parse_record_key,
};

use super::hydrate_conversation;

/// Returned by [`hydrate_all_conversations`] in place of an empty list when
/// the `ConversationIndex` itself could not be read: the caller must treat
/// the run as failed (nothing was attempted), not as "all conversations
/// hydrated". Never a real conversation key, so the failed-hydration retry
/// path rejects and drops it.
pub const HYDRATION_LOAD_FAILED_KEY: &str = "\0hydration-load-failed";

/// Returns the conversation keys that failed to hydrate during a bulk run.
/// Hydrates ALL conversations from the `ConversationIndex` — pulls every
/// conversation's key ring, manifest, roster, and message history from DHT.
///
/// Called during new-device pairing to ensure all data is cached locally
/// before the old device potentially goes offline. Individual conversation
/// failures are logged and skipped (non-fatal).
///
/// `on_start` is called once with the total conversation count before work starts.
/// `on_progress` is called after each conversation completes (success or failure)
/// from this caller-owned future, so global signal writes stay on the Dioxus runtime.
///
/// The overall operation is bounded by `config::chat::HYDRATION_TIMEOUT_SECS`.
pub async fn hydrate_all_conversations(
    ctx: &UserCtx,
    guard: SessionGuard,
    on_start: impl FnOnce(usize),
    on_progress: impl Fn(bool),
) -> Vec<String> {
    if !guard.is_current() {
        return Vec::new();
    }
    // The index is the only record of what to hydrate, so a read failure here
    // means this run hydrated nothing — an empty `Vec` reads as "all done" to
    // the caller (pairing then reaches `Complete`), so the cause must reach the
    // log intact and the user must be told, not left with a silent success.
    let index = match load_local::<ConversationIndex>(ctx.storage_key()).await {
        Ok(index) => index,
        Err(error) => {
            if guard.is_current() {
                report_nonfatal(
                    "chat.record",
                    "Failed to load ConversationIndex for hydration",
                    error,
                );
                push_background_error(
                    "chat.record",
                    "History could not be loaded. Keep your other device online and reopen the app.",
                );
            }
            // Not a conversation key (rejected by `parse_record_key`, so the
            // retry path drops it): the one value that tells the pairing
            // caller "nothing was attempted" apart from "nothing failed".
            return vec![HYDRATION_LOAD_FAILED_KEY.to_owned()];
        }
    };

    if !guard.is_current() {
        return Vec::new();
    }
    let total = index.conversations.len();
    guard.commit(|| on_start(total));
    let tasks = FuturesUnordered::new();
    let mut invalid_keys = Vec::new();
    let mut pending_keys = HashSet::with_capacity(total);

    for entry in index.conversations.values() {
        if !guard.is_current() {
            return Vec::new();
        }
        let conversation_record_key = entry.conversation_record_key.clone();
        let contact_username = entry.contact_username.clone();
        let Ok(manifest_key) = parse_record_key(&conversation_record_key) else {
            warn!("Invalid conversation key during hydration: {conversation_record_key}");
            guard.commit(|| on_progress(false));
            invalid_keys.push(conversation_record_key);
            continue;
        };
        pending_keys.insert(conversation_record_key.clone());
        let task_ctx = ctx.clone();
        tasks.push(async move {
            if let Err(error) = hydrate_conversation(
                manifest_key,
                &conversation_record_key,
                &task_ctx,
                guard,
            )
            .await
            {
                warn!(
                    "Failed to hydrate conversation after pairing: {contact_username} ({conversation_record_key}): {error}"
                );
                return (conversation_record_key, false);
            }
            (conversation_record_key, true)
        });
    }

    let mut failed = await_hydration_results(tasks, pending_keys, |success| {
        guard.commit(|| on_progress(success));
    })
    .await;
    failed.extend(invalid_keys);

    if !failed.is_empty() {
        guard.commit(|| {
            report_nonfatal(
                "chat.record",
                "Some conversations failed to hydrate after pairing",
                format!("{} conversation(s): {}", failed.len(), failed.join(", ")),
            );
        });
    }

    failed
}

/// Polls hydration futures to completion or timeout, returning `failed_keys`.
/// Dropping the collection on timeout cancels every remaining future before
/// their keys are marked for retry.
async fn await_hydration_results<F>(
    mut tasks: FuturesUnordered<F>,
    mut pending_keys: HashSet<String>,
    on_progress: impl Fn(bool),
) -> Vec<String>
where
    F: Future<Output = (String, bool)>,
{
    let deadline = Duration::from_secs(config::chat::HYDRATION_TIMEOUT_SECS);
    let mut failed = Vec::new();

    let timed_out = timeout(deadline, async {
        while let Some((key, hydrated)) = tasks.next().await {
            on_progress(hydrated);
            pending_keys.remove(&key);
            if !hydrated {
                failed.push(key);
            }
        }
    })
    .await
    .is_err();

    if timed_out {
        warn!("Conversation hydration timed out after {deadline:?}");
        drop(tasks);
        for key in pending_keys {
            on_progress(false);
            failed.push(key);
        }
    }

    failed
}
