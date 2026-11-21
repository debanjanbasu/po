//! Failure handling: retry/backoff, dead-letter transitions, success cleanup.

use std::sync::Arc;

use anyhow::{Error, Result};
use tracing::warn;

use crate::{
    chat::{feed::MessageStatus, mark_delivery_status},
    global_state::network_status,
    helpers::async_utils::now_epoch_millis,
    models::pending_operation::{OpType, PendingOperation},
    nonfatal,
    storage::background_errors::{
        ErrorSeverity, clear_op_errors, push_op_error, report_nonfatal_with_severity,
    },
    sync::{
        handler::InviteAwaitingAck,
        pending_ops,
        queue::SyncQueue,
        queue_policy::{self, RetryDecision},
    },
    user::ctx::UserCtx,
    user::password::PasswordRotationPending,
};

use super::is_account_deleted;

enum FeedTransitionOutcome {
    Done,
    DeletedAccount,
}

async fn transition_conv_feed_messages(
    op: &PendingOperation,
    from_statuses: &[MessageStatus],
    to_status: MessageStatus,
) -> Result<FeedTransitionOutcome> {
    if op.op_type != OpType::ConvFeedSync {
        return Ok(FeedTransitionOutcome::Done);
    }
    let message_ids: Vec<&str> = op
        .conv_feed_message_ids()
        .map_err(|error| error.context("ConvFeedSync has a corrupt message-ID payload"))?
        .collect();
    let Some(target) = op.target_key.as_deref().filter(|_| !message_ids.is_empty()) else {
        return Ok(FeedTransitionOutcome::Done);
    };
    let Some(user_lock) =
        UserCtx::for_queue_op(Arc::clone(&op.username), Arc::clone(&op.identity_key))
            .lock_for_persisted_account()
            .await
            .map_err(|error| {
                error.context("Failed to verify account before updating message status")
            })?
    else {
        return Ok(FeedTransitionOutcome::DeletedAccount);
    };
    for &from in from_statuses {
        if is_account_deleted(op.identity_key.as_ref()) {
            return Ok(FeedTransitionOutcome::DeletedAccount);
        }
        mark_delivery_status(&op.identity_key, target, &message_ids, from, to_status)
            .await
            .map_err(|error| {
                error.context("Failed to persist message delivery status transition")
            })?;
    }
    drop(user_lock);
    Ok(FeedTransitionOutcome::Done)
}

async fn mark_dead_lettered_feed_failed(op: &PendingOperation) {
    if let Err(error) =
        transition_conv_feed_messages(op, &[MessageStatus::Pending], MessageStatus::Failed).await
    {
        report_nonfatal_with_severity(
            "sync.queue",
            ErrorSeverity::Visible,
            "Failed to mark dead-lettered feed messages failed",
            error,
        );
    }
}
impl SyncQueue {
    pub(super) async fn handle_failure(&self, mut op: PendingOperation, error: Error) {
        if is_account_deleted(op.identity_key.as_ref()) {
            self.handle_deleted_account_drop(op).await;
            return;
        }

        // If this op was squashed while executing, don't persist stale error state.
        if !pending_ops::owns_queue_row(&op).await {
            return;
        }

        // An attempt made while Veilid will not serve an open is not evidence
        // against the op: don't consume retry budget, don't dead-letter, don't
        // report. The op stays ready as-is; the readiness edge in
        // `handle_attachment` re-notifies the queue the moment the DHT
        // becomes usable.
        if !network_status().read().dht_ready() {
            return;
        }

        // A DELIVERED-but-unacked invite (and a mid-rotation op) is not a
        // delivery failure, so it is not reported as one. Only real "peer
        // offline / no route" failures are.
        let rotation_pending = error.is::<PasswordRotationPending>();
        let invite_awaiting_ack =
            op.op_type == OpType::ConvInviteSignal && error.is::<InviteAwaitingAck>();
        if !rotation_pending && !invite_awaiting_ack {
            // Attempts stay queue-internal until the op dead-letters.
            warn!(
                "Sync op attempt failed: {} ({}) attempt={}: {error:#}",
                op.username,
                op.op_type.display_name(),
                op.attempts.saturating_add(1),
            );
        }
        // The stored row still equals `op` (ownership verified above): hand a
        // copy to update_op so it can skip its own load round-trip.
        let stored_prior = op.clone();
        // Every path advances the counter: a fixed delay that never counts
        // stays immortal (`queue_policy`'s finite-retry rationale) and
        // re-reads the pending lane forever at `backoff_base`. The two
        // non-delivery cases are bounded by wall time instead of by attempts
        // — `ack_wait_decision` for an acked-pending invite,
        // `rotation_wait_decision` for a mid-rotation op — so their retries
        // still back off exponentially rather than spinning at 1 Hz.
        op.attempts = op.attempts.saturating_add(1);
        op.last_error = error.to_string();

        let (max_retries, backoff_base, max_backoff) = queue_policy::retry_params(op.op_type);
        let decision = if rotation_pending {
            // A mid-rotation op waits for the pending key migration, so it
            // runs on a wall-clock budget (`ROTATION_WAIT_MS`) instead of the
            // attempt-counted delivery budget; the counter still advances
            // (`attempts += 1` above) so the retries back off exponentially
            // and the lane stays finite.
            queue_policy::rotation_wait_decision(
                op.created_at,
                now_epoch_millis(),
                op.attempts,
                backoff_base,
                max_backoff,
            )
        } else if invite_awaiting_ack {
            // Re-sends of an invite the peer already holds are not delivery
            // attempts, so they run on their own wall-clock budget instead of
            // the attempt-counted delivery budget. The attempt counter still
            // advances (`attempts += 1` above) so the lane stays finite and
            // the log/error text stays truthful.
            queue_policy::ack_wait_decision(
                op.created_at,
                now_epoch_millis(),
                op.attempts,
                backoff_base,
                max_backoff,
            )
        } else {
            queue_policy::retry_decision(
                op.attempts,
                now_epoch_millis(),
                max_retries,
                backoff_base,
                max_backoff,
            )
        };
        match decision {
            RetryDecision::MoveToDeadLetter => {
                match pending_ops::move_to_dead_letter(&op).await {
                    Err(error) => {
                        report_nonfatal_with_severity(
                            "sync.queue",
                            ErrorSeverity::Visible,
                            "Failed to move op to dead letter",
                            error,
                        );
                    }
                    // Only report + mark messages failed when the op actually
                    // dead-lettered — a superseded op's newer owner is still
                    // trying to deliver them. Field extraction is deferred to
                    // this arm so stale/superseded transitions stay
                    // allocation-free.
                    Ok(true) => {
                        push_op_error(
                            "sync.queue",
                            ErrorSeverity::Visible,
                            format!(
                                "Sync gave up after {} attempts: {} ({}): {}",
                                op.attempts,
                                op.username,
                                op.op_type.display_name(),
                                op.last_error,
                            ),
                            op.id,
                        );
                        mark_dead_lettered_feed_failed(&op).await;
                    }
                    Ok(false) => {}
                }
            }
            RetryDecision::RetryAt(scheduled_at) => {
                op.scheduled_at = scheduled_at;
                nonfatal!(
                    "sync.queue",
                    "Failed to update op backoff",
                    pending_ops::update_op(op, Some(&stored_prior)).await
                );
                self.schedule_wake_if_earlier_inner(scheduled_at);
            }
        }
    }

    pub(super) async fn handle_success(&self, op: PendingOperation) {
        if is_account_deleted(op.identity_key.as_ref()) {
            self.handle_deleted_account_drop(op).await;
            return;
        }
        match transition_conv_feed_messages(
            &op,
            &[MessageStatus::Pending, MessageStatus::Failed],
            MessageStatus::Sent,
        )
        .await
        {
            Ok(FeedTransitionOutcome::Done) => {}
            Ok(FeedTransitionOutcome::DeletedAccount) => {
                self.handle_deleted_account_drop(op).await;
                return;
            }
            Err(error) => {
                self.handle_failure(op, error).await;
                return;
            }
        }
        if is_account_deleted(op.identity_key.as_ref()) {
            self.handle_deleted_account_drop(op).await;
            return;
        }
        if let Err(error) = pending_ops::complete(&op).await {
            // `Visible`, matching the other queue bookkeeping failures above: a
            // completion that cannot be persisted leaves the row queued, so the
            // op is executed and published again on the next drain while the UI
            // reports success. The user cannot act on a cause they cannot see.
            report_nonfatal_with_severity(
                "sync.queue",
                ErrorSeverity::Visible,
                "Failed to finalize completed op (it stays queued and runs again)",
                error,
            );
        } else {
            clear_op_errors(op.id);
        }
    }

    pub(super) async fn handle_deleted_account_drop(&self, op: PendingOperation) {
        if let Err(error) = pending_ops::complete(&op).await {
            // Same reason as the success path: the row survives its account, so
            // the op is retried by a queue whose owner is gone. Visible so the
            // repeat is attributable rather than mysterious.
            report_nonfatal_with_severity(
                "sync.queue",
                ErrorSeverity::Visible,
                "Failed to clean deleted-account op (its row outlives the account)",
                error,
            );
        }
    }
}
