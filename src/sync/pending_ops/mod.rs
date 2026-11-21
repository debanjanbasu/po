//! Pending operations queue for offline-first sync.
//!
//! Operations are persisted to `TableDB` when offline and processed when network
//! becomes available. Failed operations after max retries move to dead-letter queue.
//! See also: docs/sync-pipeline.md.

mod dead_letter;
mod flow;
mod indexes;
mod rebuild;
mod storage;

use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, anyhow};
use tokio::sync::OwnedMutexGuard;

use crate::{
    helpers::{capnp::capnp_u32, framing, store_lock::lock_store},
    models::pending_operation::{DedupTarget, PendingOperation},
    storage::{
        account_table::{account_row_key, account_rows_in_column},
        background_errors::{clear_op_errors, report_nonfatal},
        sync_status::refresh_sync_status,
    },
    veilid::table_store,
};
use indexes::{DedupKey, Indexes, dedup_key_for, indexes, with_indexes_mut};
use storage::{
    delete_both_rows, delete_op, load_op_opt, load_ops_by_ids, store_op, store_op_replacing,
};

pub use dead_letter::retry_all_dead_letters;
pub use flow::{complete, complete_matching, enqueue, move_to_dead_letter, update_op};
pub use rebuild::init_indexes;
pub const PENDING_OPS_TABLE: &str = "oo_pending_ops";
pub const PENDING_OPS_COLS: u32 = 2;
pub const COL_PENDING: u32 = 0;
pub const COL_DEAD_LETTER: u32 = 1;

const COMPLETION_MARKER_PREFIX: &str = "[completed-op]";
const COMPLETION_MARKER_MESSAGE: &str =
    "[completed-op] Completed successfully before queue-row cleanup finished";
const ROW_CLEANUP_MARKER_PREFIX: &str = "[cleanup-row]";
const ROW_CLEANUP_MARKER_MESSAGE: &str =
    "[cleanup-row] Removing stale queue residue after partial cleanup failure";

// ---------------------------------------------------------------------------
// Key helpers
// ---------------------------------------------------------------------------

pub const fn id_to_key(id: u64) -> [u8; 8] {
    id.to_le_bytes()
}

pub fn key_to_id(key: &[u8]) -> Option<u64> {
    <[u8; 8]>::try_from(key).map_or_else(
        |_| {
            report_nonfatal(
                "sync.pending_ops",
                "Malformed op key (expected 8 bytes)",
                format!("len={}", key.len()),
            );
            None
        },
        |arr| Some(u64::from_le_bytes(arr)),
    )
}

fn dedup_lock_key(dedup_key: &DedupKey) -> Vec<u8> {
    let mut key = Vec::from(&b"sync.pending_ops.dedup\0"[..]);
    key.extend_from_slice(dedup_key.0.as_ref().as_bytes());
    key.push(0);
    // Small-integer OpType discriminant instead of `format!("{:?}")`: this
    // runs per squashable op on both enqueue and completion (hot path).
    key.push(dedup_key.1.discriminant());
    key.push(0);
    // Length-prefix each string so no pair of keys can serialize to the same
    // bytes (a peer key ending in the same bytes as a manifest key).
    match dedup_key.2.as_ref() {
        Some(DedupTarget::Target(target)) => {
            key.push(0);
            framing::push_str(&mut key, target);
        }
        Some(DedupTarget::Invite { peer, manifest }) => {
            key.push(1);
            framing::push_str(&mut key, peer);
            framing::push_str(&mut key, manifest);
        }
        None => {}
    }
    key
}

async fn lock_dedup_keys(keys: Vec<DedupKey>) -> Vec<OwnedMutexGuard<()>> {
    let mut serialized: Vec<Vec<u8>> = keys.iter().map(dedup_lock_key).collect();
    serialized.sort();
    serialized.dedup();

    let mut guards = Vec::with_capacity(serialized.len());
    for key in serialized {
        guards.push(lock_store(&key).await);
    }
    guards
}

async fn lock_related_dedup_keys(
    op: &PendingOperation,
    old_op: Option<&PendingOperation>,
) -> Vec<OwnedMutexGuard<()>> {
    let mut keys = vec![dedup_key_for(op)];
    if let Some(old) = old_op {
        keys.push(dedup_key_for(old));
    }
    let mut guards = lock_dedup_keys(keys).await;
    let mut row_key = Vec::from(&b"sync.pending_ops.op\0"[..]);
    row_key.extend_from_slice(&id_to_key(op.id));
    guards.push(lock_store(&row_key).await);
    guards
}

fn owns_queue_row_locked(idx: &Indexes, op: &PendingOperation) -> bool {
    idx.pending
        .by_id
        .get(&op.id)
        .is_some_and(|(_, seq, _)| *seq == op.sequence_number)
        && idx.pending_id_for_dedup(&dedup_key_for(op)) == Some(op.id)
}

const fn column_name(column: u32) -> &'static str {
    if column == COL_PENDING {
        "pending"
    } else {
        "dead-letter"
    }
}

fn completion_marker(op: &PendingOperation) -> PendingOperation {
    let mut marker = op.clone();
    marker.last_error = COMPLETION_MARKER_MESSAGE.to_string();
    marker.scheduled_at = u64::MAX;
    marker
}

fn row_cleanup_marker(op: &PendingOperation, column: u32) -> PendingOperation {
    let mut marker = op.clone();
    marker.last_error = format!("{ROW_CLEANUP_MARKER_MESSAGE} ({})", column_name(column));
    marker.scheduled_at = u64::MAX;
    marker
}

fn is_completion_marker(op: &PendingOperation) -> bool {
    op.scheduled_at == u64::MAX && op.last_error.starts_with(COMPLETION_MARKER_PREFIX)
}

fn is_row_cleanup_marker(op: &PendingOperation) -> bool {
    op.scheduled_at == u64::MAX && op.last_error.starts_with(ROW_CLEANUP_MARKER_PREFIX)
}

async fn persist_completion_marker(op: &PendingOperation) -> Result<()> {
    let marker = completion_marker(op);
    match store_op(COL_PENDING, &marker).await {
        Ok(()) => Ok(()),
        Err(pending_error) => store_op(COL_DEAD_LETTER, &marker).await.map_err(|dead_letter_error| {
            anyhow!(
                "failed to persist completion marker: pending={pending_error:#}; dead_letter={dead_letter_error:#}"
            )
        }),
    }
}

async fn persist_row_cleanup_marker(column: u32, op: &PendingOperation) -> Result<()> {
    store_op(column, &row_cleanup_marker(op, column)).await
}

async fn delete_row_or_mark_cleanup(
    column: u32,
    op: &PendingOperation,
    marked_context: &'static str,
    failure_context: &'static str,
) -> Result<()> {
    match delete_op(column, op.id).await {
        Ok(()) => Ok(()),
        Err(delete_error) => match persist_row_cleanup_marker(column, op).await {
            Ok(()) => {
                report_nonfatal(
                    "sync.pending_ops",
                    marked_context,
                    format!("id={}: {delete_error:#}", op.id),
                );
                Ok(())
            }
            Err(marker_error) => Err(anyhow!(
                "{failure_context}: id={}: delete={delete_error:#}; marker={marker_error:#}",
                op.id
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Safety gap added to the persisted max sequence number on restart.
/// Prevents duplicate sequence numbers if ops were enqueued but not
/// fully persisted before a crash.
///
/// Combined with the `+1` offset in `next_sequence_number`, the first
/// sequence number issued after restart is at least `max_persisted + 101`,
/// providing ≥101 slots of crash safety.
const SEQ_RESTART_GAP: u64 = 100;

/// Monotonic sequence counter — seeded from `init_indexes` scan, no `TableDB` I/O.
static SEQ_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn next_sequence_number() -> u64 {
    SEQ_COUNTER.fetch_add(1, Ordering::Acquire) + 1
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

pub(super) async fn related_dead_letters(op: &PendingOperation) -> Result<Vec<PendingOperation>> {
    let key = dedup_key_for(op);
    let ids = indexes().read().await.dead_letter_ids_for_dedup(&key);
    load_ops_by_ids(&ids, COL_DEAD_LETTER).await
}

/// Queue lengths are bounded by practical operation volume; overflow clamps.
pub async fn queue_counts() -> (u32, u32) {
    let indexes = indexes().read().await;
    (
        capnp_u32(indexes.pending.entries.len()),
        capnp_u32(indexes.dead_letter.entries.len()),
    )
}

pub async fn list_dead_letter() -> Result<Vec<PendingOperation>> {
    let ids: Vec<u64> = indexes()
        .read()
        .await
        .dead_letter
        .entries
        .iter()
        .copied()
        .collect();
    load_ops_by_ids(&ids, COL_DEAD_LETTER).await
}

pub async fn list_pending() -> Result<Vec<PendingOperation>> {
    let ids: Vec<u64> = indexes()
        .read()
        .await
        .pending
        .entries
        .iter()
        .map(|(_, _, id)| *id)
        .collect();
    load_ops_by_ids(&ids, COL_PENDING).await
}

/// Whether any pending op is ready to run at `now`.
///
/// Index-only check: the pending lane is sorted by `scheduled_at`, so the
/// first entry is the earliest-scheduled op.
pub async fn has_ready_pending(now: u64) -> bool {
    indexes()
        .read()
        .await
        .pending
        .entries
        .first()
        .is_some_and(|(scheduled_at, _, _)| *scheduled_at <= now)
}

/// Returns currently ready pending ops (`scheduled_at <= now`) plus the next
/// wake timestamp for the earliest future-scheduled op, if any.
pub async fn list_ready_pending(now: u64) -> Result<(Vec<PendingOperation>, Option<u64>)> {
    let (ids, next_wake_at) = {
        let idx = indexes().read().await;
        let mut ids = Vec::new();
        let mut next_wake_at = None;
        for (scheduled_at, _, id) in &idx.pending.entries {
            if *scheduled_at > now {
                next_wake_at = Some(*scheduled_at);
                break;
            }
            ids.push(*id);
        }
        drop(idx);
        (ids, next_wake_at)
    };
    Ok((load_ops_by_ids(&ids, COL_PENDING).await?, next_wake_at))
}

/// Returns `true` if this op is still the current owner of its queue row.
pub async fn owns_queue_row(op: &PendingOperation) -> bool {
    let idx = indexes().read().await;
    owns_queue_row_locked(&idx, op)
}

// ---------------------------------------------------------------------------
// Account cleanup
// ---------------------------------------------------------------------------

pub async fn delete_all_for_account(username: &str, account_id: &str) -> Result<()> {
    let table = table_store::open_table(PENDING_OPS_TABLE, PENDING_OPS_COLS).await?;
    // Shared account-key scan; pending-op keys are `{account_id}\0{id}`, so the
    // prefix `{account_id}\0` matches exactly this account's rows.
    let pending_keys =
        account_rows_in_column(&table, COL_PENDING, &account_row_key(account_id, &[])).await?;
    let dead_letter_keys =
        account_rows_in_column(&table, COL_DEAD_LETTER, &account_row_key(account_id, &[])).await?;
    let (pending_ids, dead_letter_ids) = {
        let idx = indexes().read().await;
        let mut pending_ids: BTreeSet<u64> =
            idx.pending_ids_for_user(username).into_iter().collect();
        let mut dead_letter_ids: BTreeSet<u64> =
            idx.dead_letter_ids_for_user(username).into_iter().collect();
        for key in pending_keys {
            if let Some(id) = key_to_id(&key)
                && !idx.pending.is_indexed_for_other_user(id, username)
            {
                pending_ids.insert(id);
            }
        }
        for key in dead_letter_keys {
            if let Some(id) = key_to_id(&key)
                && !idx.dead_letter.is_indexed_for_other_user(id, username)
            {
                dead_letter_ids.insert(id);
            }
        }
        (pending_ids, dead_letter_ids)
    };
    // The full-table key scan above is deliberate: it catches rows whose
    // index entries were lost to a crash, so rows for *other* users may be
    // loaded here and are filtered out below.
    let pending_rows = load_ops_by_ids(&pending_ids.into_iter().collect::<Vec<_>>(), COL_PENDING)
        .await?
        .into_iter()
        .filter(|op| op.identity_key.as_ref() == account_id)
        .collect::<Vec<_>>();
    let dead_letter_rows = load_ops_by_ids(
        &dead_letter_ids.into_iter().collect::<Vec<_>>(),
        COL_DEAD_LETTER,
    )
    .await?
    .into_iter()
    .filter(|op| op.identity_key.as_ref() == account_id)
    .collect::<Vec<_>>();

    // Pending lane first (matching prior ordering), then dead-letter.
    let pending_removed = purge_lane(
        COL_PENDING,
        pending_rows,
        Indexes::remove_pending_and_dedup,
        "Stored durable pending cleanup marker during account cleanup",
        "Failed to durably clean pending row during account cleanup",
    )
    .await?;
    // The account is gone — its dead-letter notices must not outlive it.
    let dead_letter_removed = purge_lane(
        COL_DEAD_LETTER,
        dead_letter_rows,
        Indexes::remove_dead_letter,
        "Stored durable dead-letter cleanup marker during account cleanup",
        "Failed to durably clean dead-letter row during account cleanup",
    )
    .await?;

    if pending_removed || dead_letter_removed {
        refresh_sync_status().await;
    }
    Ok(())
}

/// Durably removes one lane's rows for a deleted account, updating indexes
/// and clearing per-op error notices for every row actually removed.
async fn purge_lane(
    column: u32,
    rows: Vec<PendingOperation>,
    remove_indexed: fn(&mut Indexes, u64, Option<&PendingOperation>),
    marked_context: &'static str,
    failure_context: &'static str,
) -> Result<bool> {
    let mut removed_any = false;
    for op in rows {
        let _guards = lock_related_dedup_keys(&op, None).await;
        delete_row_or_mark_cleanup(column, &op, marked_context, failure_context).await?;
        with_indexes_mut(|idx| remove_indexed(idx, op.id, Some(&op))).await;
        clear_op_errors(op.id);
        removed_any = true;
    }
    Ok(removed_any)
}
