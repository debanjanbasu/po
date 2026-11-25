//! Dead-letter queue actions: retry/dismiss individual or all ops, with
//! arbitration against newer dedup owners on stale retries.

use std::{collections::HashMap, future::Future, sync::Arc};

use anyhow::{Result, anyhow};
use tokio::sync::oneshot;

use crate::{
    chat::{feed::MessageStatus, mark_delivery_status},
    config,
    helpers::concurrency::map_concurrent,
    models::pending_operation::{OpType, PendingOperation},
    nonfatal,
    session::epoch::spawn_session_detached,
    storage::{
        background_errors::{clear_op_errors, report_nonfatal},
        sync_status::refresh_sync_status,
    },
    sync::queue::notify_sync,
    user::{ctx::UserCtx, store::load_matching_user},
};

use super::{
    COL_DEAD_LETTER, COL_PENDING, dedup_key_for, delete_row_or_mark_cleanup, flow::complete_locked,
    indexes, list_dead_letter, load_op_opt, load_ops_by_ids, lock_related_dedup_keys,
    related_dead_letters, store_op_replacing, with_indexes_mut,
};

/// Arbitration happens only after every owner read succeeds. DLQ rows win
/// sequence ties, matching startup rebuild.
async fn resolve_ownership_conflict(
    op: &PendingOperation,
    pending_owner: Option<&PendingOperation>,
    dead_letters: &[PendingOperation],
) -> Result<bool> {
    if pending_owner.is_some_and(|owner| owner.id == op.id) {
        delete_row_or_mark_cleanup(
            COL_DEAD_LETTER,
            op,
            "Stored durable dead-letter cleanup marker for an already retried op",
            "Failed to durably clean dead-letter residue for an already retried op",
        )
        .await?;
        with_indexes_mut(|idx| idx.remove_dead_letter(op.id, Some(op))).await;
        clear_op_errors(op.id);
        return Ok(true);
    }

    if pending_owner.is_some_and(|owner| owner.sequence_number > op.sequence_number)
        || dead_letters
            .iter()
            .any(|owner| (owner.sequence_number, owner.id) > (op.sequence_number, op.id))
    {
        complete_locked(op).await?;
        clear_op_errors(op.id);
        return Ok(true);
    }
    Ok(false)
}

async fn mark_retried_feed_messages(op: &PendingOperation) -> Result<()> {
    if op.op_type == OpType::ConvFeedSync
        && let Some(target) = op.target_key.as_deref()
    {
        let message_ids: Vec<_> = op.conv_feed_message_ids()?.collect();
        if !message_ids.is_empty()
            && let Some(user) = load_matching_user(&op.username, &op.identity_key).await?
        {
            let user_ctx = UserCtx::from_user(&user);
            let user_lock = user_ctx.lock_for_persisted_account().await?;
            if user_lock.is_some() {
                nonfatal!(
                    "sync.pending_ops",
                    "Failed to reset dead-lettered messages to pending on retry",
                    mark_delivery_status(
                        &op.identity_key,
                        target,
                        &message_ids,
                        MessageStatus::Failed,
                        MessageStatus::Pending,
                    )
                    .await
                );
            }
        }
    }
    Ok(())
}
async fn related_dead_letters_from_snapshot(
    op: &PendingOperation,
    snapshot: &HashMap<u64, PendingOperation>,
) -> Result<Vec<PendingOperation>> {
    let ids = indexes()
        .read()
        .await
        .dead_letter_ids_for_dedup(&dedup_key_for(op));
    let mut rows: HashMap<_, _> = ids
        .iter()
        .filter_map(|id| snapshot.get(id).cloned().map(|row| (*id, row)))
        .collect();
    let missing: Vec<_> = ids
        .iter()
        .copied()
        .filter(|id| !rows.contains_key(id))
        .collect();
    rows.extend(
        load_ops_by_ids(&missing, COL_DEAD_LETTER)
            .await?
            .into_iter()
            .map(|row| (row.id, row)),
    );
    Ok(ids.into_iter().filter_map(|id| rows.remove(&id)).collect())
}

async fn retry_from_dead_letter_inner(
    id: u64,
    refresh_status: bool,
    notify_queue: bool,
    dead_letter_snapshot: Option<Arc<HashMap<u64, PendingOperation>>>,
) -> Result<bool> {
    let mut retried = false;
    let initial_opt = match dead_letter_snapshot
        .as_ref()
        .and_then(|s| s.get(&id).cloned())
    {
        Some(op) => Some(op),
        None => load_op_opt(COL_DEAD_LETTER, id).await?,
    };
    if let Some(initial) = initial_opt {
        let guards = lock_related_dedup_keys(&initial, None).await;
        let Some(mut op) = load_op_opt(COL_DEAD_LETTER, id).await? else {
            return Ok(false);
        };
        let mut removed = vec![(COL_PENDING, id), (COL_DEAD_LETTER, id)];
        let dedup_key = dedup_key_for(&op);
        let owner_id = indexes().read().await.pending_id_for_dedup(&dedup_key);
        let pending_owner = if let Some(owner_id) = owner_id {
            load_op_opt(COL_PENDING, owner_id).await?
        } else {
            None
        };
        let dead_letters = if let Some(snapshot) = &dead_letter_snapshot {
            related_dead_letters_from_snapshot(&op, snapshot).await?
        } else {
            related_dead_letters(&op).await?
        };
        let merged_feed_payload = if op.op_type == OpType::ConvFeedSync {
            let mut message_ids: Vec<_> = op.conv_feed_message_ids()?.collect();
            for old in pending_owner.iter().chain(&dead_letters) {
                message_ids.extend(old.conv_feed_message_ids()?);
            }
            Some(PendingOperation::conv_feed_payload(message_ids))
        } else {
            None
        };
        if resolve_ownership_conflict(&op, pending_owner.as_ref(), &dead_letters).await? {
            drop(guards);
            if refresh_status {
                refresh_sync_status().await;
            }
            return Ok(false);
        }
        if let Some(payload) = merged_feed_payload {
            op.payload = payload;
        }
        if let Some(owner_id) = owner_id {
            removed.push((COL_PENDING, owner_id));
            removed.push((COL_DEAD_LETTER, owner_id));
        }
        for old in &dead_letters {
            removed.push((COL_PENDING, old.id));
            removed.push((COL_DEAD_LETTER, old.id));
        }

        op.attempts = 0;
        op.last_error.clear();
        op.scheduled_at = 0;
        removed.sort_unstable();
        removed.dedup();
        store_op_replacing(COL_PENDING, &op, &removed).await?;

        with_indexes_mut(|idx| idx.replace_pending(&op, &removed)).await;
        retried = true;
        drop(guards);
        for &(column, old_id) in &removed {
            if column == COL_DEAD_LETTER {
                clear_op_errors(old_id);
            }
        }

        mark_retried_feed_messages(&op).await?;
    }

    if refresh_status {
        refresh_sync_status().await;
    }
    if notify_queue && retried {
        notify_sync();
    }
    Ok(retried)
}

pub async fn retry_from_dead_letter(id: u64) -> Result<()> {
    retry_from_dead_letter_inner(id, true, true, None)
        .await
        .map(|_| ())
}

/// Moves ALL dead-lettered ops back to the pending queue for retry.
/// Called on network reconnect so stale failures get a fresh chance.
///
/// Eligibility is deliberately not derived from `last_error`: identical text
/// can hide a resolved cause (the peer came back online), so every row is
/// re-enqueued with `attempts = 0`. What bounds the cost of a lane full of
/// permanently failing ops is [`config::sync::MAX_DEAD_LETTERS`] — one
/// reconnect re-reads, re-enqueues and re-budgets at most that many rows, not
/// one per row ever dead-lettered.
pub async fn retry_all_dead_letters() -> Result<u32> {
    run_bulk_session(retry_all_dead_letters_inner()).await
}

async fn retry_all_dead_letters_inner() -> Result<u32> {
    let snapshot_rows = list_dead_letter().await?;
    let ids: Vec<_> = snapshot_rows.iter().map(|op| op.id).collect();
    let snapshot = Arc::new(
        snapshot_rows
            .into_iter()
            .map(|op| (op.id, op))
            .collect::<HashMap<_, _>>(),
    );
    let had_ids = !ids.is_empty();
    let retried = if had_ids {
        bulk_dead_letter_op(ids, "Failed to retry dead-lettered op", move |id| {
            retry_from_dead_letter_inner(id, false, false, Some(Arc::clone(&snapshot)))
        })
        .await
    } else {
        0
    };
    if had_ids {
        refresh_sync_status().await;
        if retried > 0 {
            notify_sync();
        }
    }
    Ok(retried)
}

/// Survives component unmount but is canceled when the active session changes.
async fn run_bulk_session(future: impl Future<Output = Result<u32>> + 'static) -> Result<u32> {
    let (sender, receiver) = oneshot::channel();
    spawn_session_detached(async move {
        let _ = sender.send(future.await);
    });
    receiver
        .await
        .map_err(|_| anyhow!("bulk dead-letter operation canceled with its session"))?
}

async fn dismiss_from_dead_letter_inner(id: u64, refresh_status: bool) -> Result<bool> {
    let mut dismissed = false;
    if let Some(initial) = load_op_opt(COL_DEAD_LETTER, id).await? {
        let guards = lock_related_dedup_keys(&initial, None).await;
        if let Some(op) = load_op_opt(COL_DEAD_LETTER, id).await? {
            if indexes().read().await.pending.by_id.contains_key(&id) {
                delete_row_or_mark_cleanup(
                    COL_DEAD_LETTER,
                    &op,
                    "Stored durable cleanup marker while dismissing stale dead-letter residue",
                    "Failed to dismiss stale dead-letter residue",
                )
                .await?;
                with_indexes_mut(|idx| idx.remove_dead_letter(id, Some(&op))).await;
            } else {
                complete_locked(&op).await?;
            }
            clear_op_errors(id);
            dismissed = true;
        }
        drop(guards);
    }
    if refresh_status {
        refresh_sync_status().await;
    }
    Ok(dismissed)
}

pub async fn dismiss_from_dead_letter(id: u64) -> Result<()> {
    dismiss_from_dead_letter_inner(id, true).await.map(|_| ())
}

/// Evicts the oldest dead-letter rows past [`config::sync::MAX_DEAD_LETTERS`].
///
/// Called after a row is dead-lettered, so the lane cannot grow without bound:
/// ids come from a monotonic counter, so the dead-letter index's id order is
/// creation order and the oldest rows go first. Removal goes through the
/// regular [dismiss] path — the same per-row arbitration, locking, row
/// deletion and index update as a user pressing Dismiss — so there is only one
/// way a dead-letter row leaves the queue. What that costs the user is spelled
/// out on [`config::sync::MAX_DEAD_LETTERS`].
///
/// Runs outside the dead-lettering caller's dedup locks: a victim usually has
/// a different dedup key, and taking its locks while holding another op's
/// would order two lock sets against each other.
///
/// [dismiss]: fn@dismiss_from_dead_letter
pub(super) async fn evict_overflow_dead_letters() {
    let victims: Vec<u64> = {
        let idx = indexes().read().await;
        let excess = idx
            .dead_letter
            .entries
            .len()
            .saturating_sub(config::sync::MAX_DEAD_LETTERS);
        idx.dead_letter
            .entries
            .iter()
            .copied()
            .take(excess)
            .collect()
    };
    if victims.is_empty() {
        return;
    }
    let evicted = bulk_dead_letter_op(
        victims,
        "Failed to evict an over-cap dead-letter row",
        |id| dismiss_from_dead_letter_inner(id, false),
    )
    .await;
    if evicted > 0 {
        refresh_sync_status().await;
    }
}

pub async fn dismiss_all_dead_letters() -> Result<u32> {
    run_bulk_session(dismiss_all_dead_letters_inner()).await
}

async fn dismiss_all_dead_letters_inner() -> Result<u32> {
    let ids: Vec<u64> = indexes()
        .read()
        .await
        .dead_letter
        .entries
        .iter()
        .copied()
        .collect();
    let had_ids = !ids.is_empty();
    let dismissed = if had_ids {
        bulk_dead_letter_op(ids, "Failed to dismiss dead-lettered op", |id| {
            dismiss_from_dead_letter_inner(id, false)
        })
        .await
    } else {
        0
    };
    if had_ids {
        refresh_sync_status().await;
    }
    Ok(dismissed)
}

/// Fans out a per-id async operation over all dead-letter ids, unbounded.
/// Returns the count of successful operations.
async fn bulk_dead_letter_op<F>(ids: Vec<u64>, failure_context: &'static str, per_id_fn: F) -> u32
where
    F: AsyncFn(u64) -> Result<bool> + Clone + 'static,
{
    if ids.is_empty() {
        return 0;
    }
    let results = map_concurrent(ids, per_id_fn).await;
    let mut succeeded = 0u32;
    for result in results {
        match result {
            Ok(true) => succeeded = succeeded.saturating_add(1),
            Ok(false) => {}
            Err(error) => {
                report_nonfatal("sync.pending_ops", failure_context, error);
            }
        }
    }

    succeeded
}
