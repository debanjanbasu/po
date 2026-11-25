//! Pure scheduling policy: retry/backoff parameters, active-user prioritization
//! of ready ops, and conflict-free lane-key grouping.

use std::sync::Arc;

use crate::{
    config::sync,
    models::pending_operation::{OpType, PendingOperation},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    RetryAt(u64),
    MoveToDeadLetter,
}

pub fn prioritize_ready_ops(ready: &mut [PendingOperation], active_account_id: Option<&str>) {
    let is_active = |op: &PendingOperation| active_account_id == Some(op.identity_key.as_ref());
    ready.sort_by(|a, b| {
        is_active(b)
            .cmp(&is_active(a))
            .then(a.scheduled_at.cmp(&b.scheduled_at))
            .then(a.sequence_number.cmp(&b.sequence_number))
    });
}

pub fn retry_decision(
    attempts: u32,
    now: u64,
    max_retries: u8,
    backoff_base_ms: u64,
    max_backoff_ms: u64,
) -> RetryDecision {
    if max_retries > 0 && u32::from(max_retries) <= attempts {
        return RetryDecision::MoveToDeadLetter;
    }

    RetryDecision::RetryAt(now.saturating_add(backoff_delay(
        attempts,
        backoff_base_ms,
        max_backoff_ms,
    )))
}

/// Retry decision for a non-delivery wait: an invite the peer already holds
/// (`INVITE_ACK_WAIT_MS`) or an op deferred by a pending password rotation
/// (`ROTATION_WAIT_MS`).
///
/// Both lanes are bounded by *wall time* rather than the attempt-counted
/// delivery budget (`MAX_RETRIES`): a delivered-but-unacked invite must not
/// dead-letter while the peer is online but slow, and a mid-rotation op must
/// not dead-letter while the key migration is still converging. The caller
/// still advances the attempt counter, so successive retries back off
/// exponentially rather than repeating at `backoff_base_ms`.
pub fn wait_until_deadline_decision(
    created_at_ms: i64,
    now: u64,
    attempts: u32,
    backoff_base_ms: u64,
    max_backoff_ms: u64,
    deadline_ms: u64,
) -> RetryDecision {
    if now.saturating_sub(u64::try_from(created_at_ms).unwrap_or(0)) >= deadline_ms {
        return RetryDecision::MoveToDeadLetter;
    }
    RetryDecision::RetryAt(now.saturating_add(backoff_delay(
        attempts,
        backoff_base_ms,
        max_backoff_ms,
    )))
}

/// [`wait_until_deadline_decision`] for a delivered-but-unacked invite.
pub fn ack_wait_decision(
    created_at_ms: i64,
    now: u64,
    attempts: u32,
    backoff_base_ms: u64,
    max_backoff_ms: u64,
) -> RetryDecision {
    wait_until_deadline_decision(
        created_at_ms,
        now,
        attempts,
        backoff_base_ms,
        max_backoff_ms,
        sync::INVITE_ACK_WAIT_MS,
    )
}

/// [`wait_until_deadline_decision`] for an op deferred by a pending password
/// rotation.
pub fn rotation_wait_decision(
    created_at_ms: i64,
    now: u64,
    attempts: u32,
    backoff_base_ms: u64,
    max_backoff_ms: u64,
) -> RetryDecision {
    wait_until_deadline_decision(
        created_at_ms,
        now,
        attempts,
        backoff_base_ms,
        max_backoff_ms,
        sync::ROTATION_WAIT_MS,
    )
}

fn backoff_delay(attempts: u32, backoff_base_ms: u64, max_backoff_ms: u64) -> u64 {
    (2u64.pow(attempts.min(10)).saturating_mul(backoff_base_ms)).min(max_backoff_ms)
}

/// Lane key for conflict-free parallel execution.
/// Ops with the same lane key execute serially; different keys execute concurrently.
pub type LaneKey = (Arc<str>, OpFamily, Option<Arc<str>>);

/// Groups op types that share the same DHT record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpFamily {
    Identity,
    Conversation,
    System,
}

const fn op_family(op_type: OpType) -> OpFamily {
    match op_type {
        OpType::Profile
        | OpType::Security
        | OpType::ProfileAndSecurity
        | OpType::ReencryptSubkeys
        | OpType::CreateIdentity
        | OpType::Totp
        | OpType::ChatContacts => OpFamily::Identity,
        OpType::Chat
        | OpType::ConvFeedSync
        | OpType::ConvRecordCreate
        | OpType::ConvInviteSignal => OpFamily::Conversation,
        OpType::AppState | OpType::DictPublish => OpFamily::System,
    }
}

pub fn lane_key(op: &PendingOperation) -> LaneKey {
    (
        op.identity_key.clone(),
        op_family(op.op_type),
        op.target_key.clone(),
    )
}

/// Returns `(max_retries, backoff_base_ms, max_backoff_ms)` for the given op type.
/// `max_retries == 0` means retry indefinitely.
pub const fn retry_params(op_type: OpType) -> (u8, u64, u64) {
    // `ReencryptSubkeys` retries indefinitely so password-change DHT
    // convergence happens on the user's next login even if the queue tried to
    // run while they were logged out. It uses the default backoff schedule;
    // `ConvInviteSignal` keeps its longer invite-specific schedule.
    if matches!(op_type, OpType::ConvInviteSignal) {
        // Invite delivery only lands when the peer is online, so the budget
        // is attempts, not wall time: the 2h-capped backoff means this window
        // spans ~2 days at max backoff, covering long offline stretches.
        // The early attempts stay close together (10s, 20s, 40s) so a peer
        // that is momentarily unreachable — a network flap, a route that has
        // not settled yet — gets the invite within the same interaction.
        // Beyond that a dead contact would squat a retry lane forever; the
        // DLQ retry path gives manual recovery once the contact resurfaces.
        return (
            sync::INVITE_MAX_RETRIES,
            sync::INVITE_BACKOFF_BASE_MS,
            sync::INVITE_MAX_BACKOFF_MS,
        );
    }
    if matches!(op_type, OpType::ReencryptSubkeys) {
        return (0, sync::BACKOFF_BASE_MS, sync::MAX_BACKOFF_MS);
    }
    (
        sync::MAX_RETRIES,
        sync::BACKOFF_BASE_MS,
        sync::MAX_BACKOFF_MS,
    )
}
