//! Core queue-row flow: squash-aware enqueue under dedup locks, completion
//! markers, and pending-to-dead-letter transitions.

use anyhow::{Result, anyhow};

use crate::{
    config,
    models::pending_operation::{OpType, PendingOperation},
    storage::{
        background_errors::{clear_op_errors, report_nonfatal},
        sync_status::refresh_sync_status,
    },
    sync::queue as sync_queue,
};

use super::{
    COL_DEAD_LETTER, COL_PENDING, dedup_key_for, delete_both_rows, indexes, list_dead_letter,
    list_pending, load_op_opt, lock_related_dedup_keys, owns_queue_row_locked,
    persist_completion_marker, related_dead_letters, store_op, store_op_replacing,
    with_indexes_mut,
};

pub async fn enqueue(mut op: PendingOperation) -> Result<()> {
    let _guards = lock_related_dedup_keys(&op, None).await;
    let existing = indexes()
        .read()
        .await
        .pending_id_for_dedup(&dedup_key_for(&op));
    let dead_letters = related_dead_letters(&op).await?;
    if op.op_type == OpType::ConvFeedSync {
        // Only a feed squash needs the incumbent row's message ids.
        let existing_op = if let Some(id) = existing {
            load_op_opt(COL_PENDING, id).await?
        } else {
            None
        };
        let mut message_ids: Vec<_> = op.conv_feed_message_ids()?.collect();
        for old_op in existing_op.iter().chain(&dead_letters) {
            message_ids.extend(old_op.conv_feed_message_ids()?);
        }
        op.payload = PendingOperation::conv_feed_payload(message_ids);
    }

    let mut removed = Vec::new();
    if let Some(old_id) = existing.filter(|old_id| *old_id != op.id) {
        removed.push((COL_PENDING, old_id));
        removed.push((COL_DEAD_LETTER, old_id));
    }
    for old_op in &dead_letters {
        removed.push((COL_PENDING, old_op.id));
        removed.push((COL_DEAD_LETTER, old_op.id));
    }
    removed.sort_unstable();
    removed.dedup();
    store_op_replacing(COL_PENDING, &op, &removed).await?;

    with_indexes_mut(|idx| idx.replace_pending(&op, &removed)).await;
    refresh_sync_status().await;

    for &(column, id) in &removed {
        if column == COL_DEAD_LETTER {
            clear_op_errors(id);
        }
    }
    sync_queue::get_sync_queue().notify_enqueue();
    Ok(())
}

/// Persists an updated queue row and reindexes it. `prior` is the row copy
/// the caller already holds (from its own load or an earlier mutation); when
/// `None`, the stored row is loaded here as a fallback.
pub async fn update_op(op: PendingOperation, prior: Option<&PendingOperation>) -> Result<()> {
    let owned_prior;
    let prior = if let Some(prior) = prior {
        Some(prior)
    } else {
        owned_prior = load_op_opt(COL_PENDING, op.id).await?;
        owned_prior.as_ref()
    };
    let _guards = lock_related_dedup_keys(&op, prior).await;

    {
        let idx = indexes().read().await;
        if !owns_queue_row_locked(&idx, &op) {
            return Ok(());
        }
    }

    store_op(COL_PENDING, &op).await?;

    with_indexes_mut(|idx| {
        if !owns_queue_row_locked(idx, &op) {
            return;
        }
        idx.upsert_pending(&op, prior);
        idx.remove_dedup(op.id);
        idx.set_dedup(&op);
    })
    .await;

    Ok(())
}

pub async fn complete(op: &PendingOperation) -> Result<()> {
    let _guards = lock_related_dedup_keys(op, None).await;
    complete_locked(op).await
}

pub(super) async fn complete_locked(op: &PendingOperation) -> Result<()> {
    complete_locked_inner(op).await?;
    refresh_sync_status().await;
    Ok(())
}

async fn complete_locked_inner(op: &PendingOperation) -> Result<()> {
    if let Err(delete_error) = delete_both_rows(op.id).await {
        persist_completion_marker(op).await.map_err(|marker_error| {
            anyhow!(
                "failed to persist completion marker for id={}: delete={delete_error:#}; marker={marker_error:#}",
                op.id
            )
        })?;
        report_nonfatal(
            "sync.pending_ops",
            "Stored durable completion marker after queue-row cleanup failed",
            format!("id={}: {delete_error:#}", op.id),
        );
    }

    with_indexes_mut(|idx| {
        idx.remove_pending_and_dedup(op.id, Some(op));
        idx.remove_dead_letter(op.id, Some(op));
    })
    .await;

    Ok(())
}

/// Completes every queued operation matching `pred`, scanning both the
/// pending queue and the dead-letter queue. Returns the number completed.
pub async fn complete_matching(pred: impl Fn(&PendingOperation) -> bool) -> Result<usize> {
    let mut matching: Vec<PendingOperation> = list_pending()
        .await?
        .into_iter()
        .filter(|op| pred(op))
        .collect();
    matching.extend(list_dead_letter().await?.into_iter().filter(|op| pred(op)));
    matching.sort_unstable_by_key(|op| op.id);
    matching.dedup_by_key(|op| op.id);

    let mut completed = 0;
    for op in &matching {
        let result = {
            let _guards = lock_related_dedup_keys(op, None).await;
            complete_locked_inner(op).await
        };
        if let Err(error) = result {
            // A failed op keeps its queue row and is completed by a later
            // matching pass; don't let one transient cleanup failure block
            // the rest of the batch.
            report_nonfatal(
                "sync.pending_ops",
                "Failed to complete matching queue op; continuing with remaining",
                error,
            );
            continue;
        }
        completed += 1;
    }
    refresh_sync_status().await;
    Ok(completed)
}

/// Moves an op to the dead-letter queue. Returns `true` when the op was
/// actually dead-lettered, `false` when it no longer owns its queue row (a
/// newer op superseded it) — callers must not apply dead-letter side effects
/// (e.g. marking messages failed) in that case.
pub async fn move_to_dead_letter(op: &PendingOperation) -> Result<bool> {
    let guards = lock_related_dedup_keys(op, None).await;

    {
        let idx = indexes().read().await;
        if !owns_queue_row_locked(&idx, op) {
            return Ok(false);
        }
    }

    store_op_replacing(COL_DEAD_LETTER, op, &[(COL_PENDING, op.id)]).await?;

    with_indexes_mut(|idx| {
        idx.remove_pending_and_dedup(op.id, Some(op));
        idx.insert_dead_letter(op);
    })
    .await;
    refresh_sync_status().await;

    // Release this op's dedup locks before evicting: a victim usually has a
    // different dedup key, and acquiring it under these would order two lock
    // sets against each other (see `evict_overflow_dead_letters`).
    drop(guards);
    super::dead_letter::evict_overflow_dead_letters().await;

    // Harness seam: the op really moved to the dead-letter column, so the
    // harness can watch the DLQ grow. The op type and attempt count are
    // not secrets; the payload and error text are never logged here.
    config::e2e::event!(
        "dead_lettered",
        op = op.op_type.display_name(),
        attempts = op.attempts
    );

    Ok(true)
}
