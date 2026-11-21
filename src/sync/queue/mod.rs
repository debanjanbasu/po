//! Sync queue for offline-first synchronization operations.
//!
//! Maintains a drain-loop that batches pending operations from `TableDB`,
//! executes them against the DHT, and handles retry/failed-state transitions.
//! A background processor wakes on `Notify` to drain the queue whenever
//! new operations are enqueued or the network comes back online.
//! Explicit account deletion installs a per-account tombstone so already-loaded
//! operations cannot keep dispatching DHT side effects after local wipe.
//! See also: docs/sync-pipeline.md.

mod failure;

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::Result;
use tokio::{select, sync::Notify};
use tracing::info;

use crate::{
    chat::signal::nudge_sibling_devices,
    config,
    helpers::{
        async_utils::{now_epoch_millis, sleep_ms},
        concurrency::map_concurrent,
        spawn,
        store_lock::recover_poison,
    },
    models::pending_operation::{OpType, PendingOperation},
    session::phase::SESSION,
    storage::{
        background_errors::report_nonfatal,
        sync_status::{
            mark_sync_processing_finished, mark_sync_processing_started, refresh_sync_status,
        },
    },
    sync::{handler, pending_ops, queue_policy},
    user::{User, identity_context::IdentityContext, password::lock_identity_sync},
};

// ── Deleted-account tombstones ───────────────────────────────────────────────

static DELETED_ACCOUNT_TOMBSTONES: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn is_account_deleted(account_id: &str) -> bool {
    !account_id.is_empty() && recover_poison(DELETED_ACCOUNT_TOMBSTONES.lock()).contains(account_id)
}

/// Mark a local account as deleted so already-loaded queue ops are dropped
/// before they can dispatch any further DHT side effects.
pub fn mark_account_deleted(account_id: &str) {
    if account_id.is_empty() {
        return;
    }
    recover_poison(DELETED_ACCOUNT_TOMBSTONES.lock()).insert(account_id.to_string());
}

/// Clear a deleted-account tombstone when that exact account snapshot is
/// intentionally reintroduced to the local device.
pub fn clear_account_deleted(account_id: &str) {
    if account_id.is_empty() {
        return;
    }
    recover_poison(DELETED_ACCOUNT_TOMBSTONES.lock()).remove(account_id);
}

// ── Lane execution & nudging ─────────────────────────────────────────────────

/// Scope bits for the same-account sync nudge sent after this op type
/// publishes. Conversation record ops are excluded: peers learn via
/// `NewData` signals and sibling devices watch those records directly.
const fn nudge_scope_for(op_type: OpType) -> u8 {
    match op_type {
        OpType::Profile | OpType::Security | OpType::ProfileAndSecurity | OpType::AppState => {
            config::nudge::SCOPE_CORE
        }
        OpType::ReencryptSubkeys => config::nudge::SCOPE_FULL,
        OpType::Totp => config::nudge::SCOPE_TOTP,
        OpType::ChatContacts => config::nudge::SCOPE_CONTACTS,
        OpType::Chat => config::nudge::SCOPE_CONVERSATIONS,
        OpType::DictPublish => config::nudge::SCOPE_DICT,
        OpType::CreateIdentity
        | OpType::ConvFeedSync
        | OpType::ConvRecordCreate
        | OpType::ConvInviteSignal => 0,
    }
}

// Lane-level lease: intentionally held across the lane's DHT operations to
// amortize acquisition; dropping per-op would serialize the lane.
async fn execute_lane(ops: Vec<PendingOperation>) {
    let queue = get_sync_queue();
    let mut crypto_lease = None;
    let mut lane_identity = None;
    let mut nudge_scope = 0u8;
    for op in ops {
        let op_type = op.op_type;
        if op_type == OpType::ReencryptSubkeys || crypto_lease.is_none() {
            // Migration takes the write lease itself. Resolve ordinary
            // publication contexts again after reacquiring the read lease.
            crypto_lease = if op_type == OpType::ReencryptSubkeys {
                None
            } else {
                Some(lock_identity_sync(&op.identity_key).await)
            };
            lane_identity = IdentityContext::for_user(&op.username)
                .await
                .ok()
                .filter(|identity| identity.user.account_id() == op.identity_key.as_ref());
        }
        match queue.execute_operation(&op, lane_identity.as_ref()).await {
            Ok(ExecuteOutcome::Stale) => {}
            Ok(ExecuteOutcome::DeletedAccount) => queue.handle_deleted_account_drop(op).await,
            Ok(ExecuteOutcome::Dispatched) => {
                nudge_scope |= nudge_scope_for(op_type);
                queue.handle_success(op).await;
            }
            Err(error) => queue.handle_failure(op, error).await,
        }
    }
    drop(crypto_lease);
    // One nudge per lane: after the DHT writes land, tell sibling devices
    // to pull the published scopes now instead of waiting for a DHT watch
    // callback or the inspect poll.
    if nudge_scope != 0
        && let Some(identity) = lane_identity.as_ref()
    {
        nudge_sibling_devices(identity.key.clone(), identity.keypair.clone(), nudge_scope);
    }
}

// ── Drain loop & worker ──────────────────────────────────────────────────────

/// Harness seam: a drain cycle finished and no ready op is left, so the
/// queue is genuinely drained. Counts are the authoritative in-memory index
/// counts — every pending row (including future-scheduled retries) and every
/// dead-letter row.
async fn report_drain_completed() {
    if !config::e2e::enabled() {
        return;
    }
    if pending_ops::has_ready_pending(now_epoch_millis()).await {
        return;
    }
    let (pending, dead) = pending_ops::queue_counts().await;
    info!(
        target: "e2e",
        event = "queue_drained",
        pending = pending,
        dead = dead
    );
}

/// Guard that resets `IS_PROCESSING` flag on drop (panic-safe).
struct ProcessingGuard;

impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        IS_PROCESSING.store(false, Ordering::Release);
        mark_sync_processing_finished();
    }
}

enum ExecuteOutcome {
    Dispatched,
    DeletedAccount,
    Stale,
}

static IS_PROCESSING: AtomicBool = AtomicBool::new(false);
static SYNC_QUEUE: LazyLock<SyncQueue> = LazyLock::new(SyncQueue::default);

pub fn get_sync_queue() -> &'static SyncQueue {
    &SYNC_QUEUE
}

/// Notifies the background sync processor that new work is available.
pub fn notify_sync() {
    SYNC_QUEUE.notify_enqueue();
}

#[derive(Default)]
pub struct SyncQueue {
    recheck: AtomicBool,
    notify: Notify,
    wake_notify: Notify,
    wake_worker_started: AtomicBool,
    next_wake_ms: AtomicU64,
}

impl SyncQueue {
    async fn load_prioritized_ready_ops(&self) -> Vec<PendingOperation> {
        let now = now_epoch_millis();
        let (mut ready, next_wake_at) = match pending_ops::list_ready_pending(now).await {
            Ok(result) => result,
            Err(e) => {
                report_nonfatal("sync.queue", "Failed to load ready ops from DB", e);
                return Vec::new();
            }
        };
        if let Some(next_wake_at) = next_wake_at {
            self.schedule_wake_if_earlier_inner(next_wake_at);
        }

        let active_account_id = SESSION
            .peek()
            .authenticated_user()
            .map(User::account_id_arc);
        queue_policy::prioritize_ready_ops(&mut ready, active_account_id.as_deref());
        ready
    }

    async fn process_ready_ops(&self, ready: Vec<PendingOperation>) {
        if ready.is_empty() {
            return;
        }

        // Preserve lane-dispatch order from prioritized `ready` list so active-user
        // lanes are not randomized by `HashMap` iteration.
        let mut lanes: Vec<Vec<PendingOperation>> = Vec::new();
        let mut lane_positions: HashMap<queue_policy::LaneKey, usize> = HashMap::new();
        for op in ready {
            let lane = queue_policy::lane_key(&op);
            match lane_positions.entry(lane) {
                Entry::Occupied(occ) => lanes[*occ.get()].push(op),
                Entry::Vacant(vac) => {
                    let position = lanes.len();
                    vac.insert(position);
                    lanes.push(vec![op]);
                }
            }
        }

        // Unbounded across lanes: `execute_lane` already serialises the ops
        // inside one lane, and each lane key is distinct, so running every
        // lane at once adds no shared lock — the fan-out just stops paying
        // a permit-pool wave of latency.
        map_concurrent(lanes, async move |ops| {
            execute_lane(ops).await;
        })
        .await;
    }

    pub async fn process_queue(&self) {
        // No network gate: veilid is the network authority. Ops are always
        // dispatched; core absorbs offline writes (`offline_subkey_writes`)
        // and failures while unreachable don't consume retry budget (see
        // `handle_failure`). Draining resumes event-driven on the attach edge.
        if IS_PROCESSING.swap(true, Ordering::AcqRel) {
            return;
        }

        let _guard = ProcessingGuard;
        mark_sync_processing_started();
        refresh_sync_status().await;

        let ready = self.load_prioritized_ready_ops().await;
        self.process_ready_ops(ready).await;

        // Re-read authoritative queue counts: completed ops are removed from the
        // indexes without refreshing SYNC_STATUS, so without this the guard's
        // `mark_sync_processing_finished()` would recompute from a stale
        // in-memory pending count and leave the phase stuck on `Processing`
        // (spinner + badge showing ghost items) until the next queue event.
        refresh_sync_status().await;
        // Guard drops here, auto-resets is_processing
        report_drain_completed().await;
    }

    pub fn notify_enqueue(&self) {
        self.recheck.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    async fn execute_operation(
        &self,
        op: &PendingOperation,
        lane_identity: Option<&IdentityContext>,
    ) -> Result<ExecuteOutcome> {
        if !pending_ops::owns_queue_row(op).await {
            return Ok(ExecuteOutcome::Stale);
        }
        if is_account_deleted(op.identity_key.as_ref()) {
            return Ok(ExecuteOutcome::DeletedAccount);
        }
        handler::dispatch(op, lane_identity).await?;
        Ok(ExecuteOutcome::Dispatched)
    }

    fn ensure_wake_worker(&self) {
        if self.wake_worker_started.swap(true, Ordering::AcqRel) {
            return;
        }
        spawn(async move {
            loop {
                let scheduled_at = SYNC_QUEUE.next_wake_ms.load(Ordering::Acquire);
                if scheduled_at == 0 {
                    SYNC_QUEUE.wake_notify.notified().await;
                    continue;
                }

                let delay = scheduled_at.saturating_sub(now_epoch_millis());
                select! {
                    () = sleep_ms(delay) => {
                        if SYNC_QUEUE
                            .next_wake_ms
                            .compare_exchange(scheduled_at, 0, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            SYNC_QUEUE.notify.notify_one();
                        }
                    }
                    () = SYNC_QUEUE.wake_notify.notified() => {}
                }
            }
        });
    }

    pub(super) fn schedule_wake_if_earlier_inner(&self, scheduled_at: u64) {
        let mut current = self.next_wake_ms.load(Ordering::Acquire);
        loop {
            if current != 0 && current <= scheduled_at {
                return;
            }
            match self.next_wake_ms.compare_exchange(
                current,
                scheduled_at,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.ensure_wake_worker();
                    self.wake_notify.notify_one();
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }
}

pub fn start_background_processor() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    spawn(async move {
        let queue = get_sync_queue();
        queue.ensure_wake_worker();
        loop {
            queue.notify.notified().await;
            // Drain loop: keep processing while new work arrived during the last run.
            // The `recheck` flag is set by `notify_enqueue` when we're mid-drain.
            loop {
                // Relaxed: only this thread reads `recheck`; the reset does not
                // publish data to other threads. Correctness relies on the
                // Acquire load below pairing with the producer's Release store.
                queue.recheck.store(false, Ordering::Relaxed);
                queue.process_queue().await;
                if !queue.recheck.load(Ordering::Acquire) {
                    break;
                }
            }
        }
    });
}
