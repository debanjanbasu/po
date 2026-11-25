//! In-memory key indexes for pending operations and dead-letter queue.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, LazyLock},
};

use tokio::sync::RwLock;

use crate::models::pending_operation::{DedupTarget, OpType, PendingOperation};

/// Dedup key: `(account_id, op_type, target)`. The target prevents
/// conversation ops for different records from squashing each other.
pub(super) type DedupKey = (Arc<str>, OpType, Option<DedupTarget>);

pub(super) fn dedup_key_for(op: &PendingOperation) -> DedupKey {
    (op.identity_key.clone(), op.op_type, op.dedup_target())
}

pub(super) const fn pending_entry(op: &PendingOperation) -> (u64, u64, u64) {
    (op.scheduled_at, op.sequence_number, op.id)
}

fn insert_username_id(index: &mut HashMap<Arc<str>, BTreeSet<u64>>, username: &Arc<str>, id: u64) {
    index.entry(username.clone()).or_default().insert(id);
}

fn remove_username_id(index: &mut HashMap<Arc<str>, BTreeSet<u64>>, username: &str, id: u64) {
    let empty = index.get_mut(username).is_some_and(|ids| {
        ids.remove(&id);
        ids.is_empty()
    });
    if empty {
        index.remove(username);
    }
}

// ── Shared username lookup ─────────────────────────────────────────────

/// Tracks the bidirectional mapping between operation ids and usernames.
/// Shared by both `PendingLane` and `DeadLetterLane`.
#[derive(Default)]
struct UsernameLookup {
    by_username: HashMap<Arc<str>, BTreeSet<u64>>,
    username_by_id: HashMap<u64, Arc<str>>,
}

impl UsernameLookup {
    fn ids_for_user(&self, username: &str) -> Vec<u64> {
        self.by_username
            .get(username)
            .map_or_else(Vec::new, |ids| ids.iter().copied().collect())
    }

    fn is_indexed_for_other_user(&self, id: u64, username: &str) -> bool {
        self.username_by_id
            .get(&id)
            .is_some_and(|u| u.as_ref() != username)
    }

    fn insert(&mut self, id: u64, username: &Arc<str>) {
        self.username_by_id.insert(id, username.clone());
        insert_username_id(&mut self.by_username, username, id);
    }

    fn remove(&mut self, id: u64, fallback_username: Option<&Arc<str>>) {
        if let Some(username) = self.username_by_id.remove(&id) {
            remove_username_id(&mut self.by_username, username.as_ref(), id);
        } else if let Some(username) = fallback_username {
            remove_username_id(&mut self.by_username, username.as_ref(), id);
        }
    }
}

// ── Pending lane ────────────────────────────────────────────────────

/// Indexes for the pending operations lane.
#[derive(Default)]
pub(super) struct PendingLane {
    /// Pending operations sorted by (`scheduled_at`, `sequence_number`, id).
    /// Ready ops (`scheduled_at=0`) always sort before future-scheduled ones.
    pub(super) entries: BTreeSet<(u64, u64, u64)>,
    /// O(1) lookup from op id -> pending index entry.
    pub(super) by_id: HashMap<u64, (u64, u64, u64)>,
    users: UsernameLookup,
}

impl PendingLane {
    pub(super) fn ids_for_user(&self, username: &str) -> Vec<u64> {
        self.users.ids_for_user(username)
    }

    pub(super) fn is_indexed_for_other_user(&self, id: u64, username: &str) -> bool {
        self.users.is_indexed_for_other_user(id, username)
    }

    pub(super) fn remove(&mut self, id: u64, fallback_old: Option<&PendingOperation>) {
        if let Some(entry) = self.by_id.remove(&id) {
            self.entries.remove(&entry);
        } else if let Some(old) = fallback_old {
            self.entries.remove(&pending_entry(old));
        }
        self.users.remove(id, fallback_old.map(|o| &o.username));
    }

    pub(super) fn upsert(
        &mut self,
        op: &PendingOperation,
        fallback_old: Option<&PendingOperation>,
    ) {
        self.remove(op.id, fallback_old);
        let entry = pending_entry(op);
        self.entries.insert(entry);
        self.by_id.insert(op.id, entry);
        self.users.insert(op.id, &op.username);
    }
}

// ── Dead-letter lane ────────────────────────────────────────────────

/// Indexes for the dead-letter queue lane.
#[derive(Default)]
pub(super) struct DeadLetterLane {
    /// Dead-letter op ids.
    pub(super) entries: BTreeSet<u64>,
    users: UsernameLookup,
}

impl DeadLetterLane {
    pub(super) fn ids_for_user(&self, username: &str) -> Vec<u64> {
        self.users.ids_for_user(username)
    }

    pub(super) fn is_indexed_for_other_user(&self, id: u64, username: &str) -> bool {
        self.users.is_indexed_for_other_user(id, username)
    }

    pub(super) fn insert(&mut self, op: &PendingOperation) {
        self.entries.insert(op.id);
        self.users.insert(op.id, &op.username);
    }

    pub(super) fn remove(&mut self, id: u64, fallback_old: Option<&PendingOperation>) {
        self.entries.remove(&id);
        self.users.remove(id, fallback_old.map(|o| &o.username));
    }
}

// ── Indexes coordinator ─────────────────────────────────────────────────

/// Which lane a dedup-keyed op currently sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum DedupLane {
    Pending,
    DeadLetter,
}

#[derive(Default)]
pub(super) struct Indexes {
    pub(super) pending: PendingLane,
    pub(super) dead_letter: DeadLetterLane,
    /// `(account_id, OpType, target_key)` → the ids holding that key, by lane.
    ///
    /// One map instead of the former pair (`Indexes::dedup` for the pending
    /// lane, `DeadLetterLane::by_dedup` for the dead-letter lane): the lane is
    /// now part of the value, so a key can never be tracked in two places and
    /// `enqueue` no longer has to reconcile the two by hand. The pending lane
    /// holds at most one id per key (enqueue squashes), the dead-letter lane
    /// may hold several.
    pub(super) dedup: HashMap<DedupKey, HashMap<DedupLane, BTreeSet<u64>>>,
    /// Reverse index for O(1) dedup cleanup on remove/retry/dead-letter transitions.
    pub(super) dedup_by_id: HashMap<u64, (DedupKey, DedupLane)>,
}

impl Indexes {
    pub(super) fn pending_ids_for_user(&self, username: &str) -> Vec<u64> {
        self.pending.ids_for_user(username)
    }

    pub(super) fn dead_letter_ids_for_user(&self, username: &str) -> Vec<u64> {
        self.dead_letter.ids_for_user(username)
    }
    /// Ids currently holding `key` in the dead-letter lane.
    pub(super) fn dead_letter_ids_for_dedup(&self, key: &DedupKey) -> Vec<u64> {
        self.dedup
            .get(key)
            .and_then(|lanes| lanes.get(&DedupLane::DeadLetter))
            .map_or_else(Vec::new, |ids| ids.iter().copied().collect())
    }

    /// The pending-lane id holding `key`, if any (enqueue squashes to one).
    pub(super) fn pending_id_for_dedup(&self, key: &DedupKey) -> Option<u64> {
        self.dedup
            .get(key)
            .and_then(|lanes| lanes.get(&DedupLane::Pending))
            .and_then(|ids| ids.iter().next().copied())
    }

    pub(super) fn replace_pending(&mut self, op: &PendingOperation, removed: &[(u32, u64)]) {
        for &(column, id) in removed {
            if column == super::COL_PENDING {
                self.remove_pending_and_dedup(id, None);
            } else {
                self.remove_dead_letter(id, None);
            }
        }
        self.upsert_pending(op, None);
        self.remove_dedup(op.id);
        self.set_dedup(op);
    }

    /// Removes `id`'s dedup registration from whatever lane holds it.
    pub(super) fn remove_dedup(&mut self, id: u64) {
        let Some((dk, lane)) = self.dedup_by_id.remove(&id) else {
            return;
        };
        if let Some(lanes) = self.dedup.get_mut(&dk) {
            if let Some(ids) = lanes.get_mut(&lane) {
                ids.remove(&id);
                if ids.is_empty() {
                    lanes.remove(&lane);
                }
            }
            if lanes.is_empty() {
                self.dedup.remove(&dk);
            }
        }
    }

    pub(super) fn upsert_pending(
        &mut self,
        op: &PendingOperation,
        fallback_old: Option<&PendingOperation>,
    ) {
        self.pending.upsert(op, fallback_old);
    }

    pub(super) fn insert_dead_letter(&mut self, op: &PendingOperation) {
        self.dead_letter.insert(op);
        self.set_dead_letter_dedup(op);
    }

    pub(super) fn remove_dead_letter(&mut self, id: u64, fallback_old: Option<&PendingOperation>) {
        self.dead_letter.remove(id, fallback_old);
    }

    /// Registers `op` under its dedup key in the pending lane, evicting any
    /// *different* pending id that previously held the key (enqueue squash).
    pub(super) fn set_dedup(&mut self, op: &PendingOperation) {
        let dk = dedup_key_for(op);
        if let Some(ids) = self
            .dedup
            .get_mut(&dk)
            .and_then(|lanes| lanes.get_mut(&DedupLane::Pending))
        {
            let old_ids: Vec<u64> = ids.iter().copied().filter(|id| *id != op.id).collect();
            for old_id in old_ids {
                ids.remove(&old_id);
                self.dedup_by_id.remove(&old_id);
            }
        }
        self.dedup
            .entry(dk.clone())
            .or_default()
            .entry(DedupLane::Pending)
            .or_default()
            .insert(op.id);
        self.dedup_by_id.insert(op.id, (dk, DedupLane::Pending));
    }

    /// Registers `op` under its dedup key in the dead-letter lane (several ids
    /// may share a key there).
    pub(super) fn set_dead_letter_dedup(&mut self, op: &PendingOperation) {
        let dk = dedup_key_for(op);
        self.dedup
            .entry(dk.clone())
            .or_default()
            .entry(DedupLane::DeadLetter)
            .or_default()
            .insert(op.id);
        self.dedup_by_id.insert(op.id, (dk, DedupLane::DeadLetter));
    }

    pub(super) fn remove_pending_and_dedup(
        &mut self,
        id: u64,
        fallback_old: Option<&PendingOperation>,
    ) {
        self.pending.remove(id, fallback_old);
        self.remove_dedup(id);
    }

    pub(super) fn dismiss_dead_letter(&mut self, id: u64) {
        self.remove_dead_letter(id, None);
        self.remove_dedup(id);
    }
}

pub(super) fn indexes() -> &'static RwLock<Indexes> {
    static INSTANCE: LazyLock<RwLock<Indexes>> = LazyLock::new(|| RwLock::new(Indexes::default()));
    &INSTANCE
}

pub(super) async fn with_indexes_mut<R>(f: impl FnOnce(&mut Indexes) -> R) -> R {
    let mut idx = indexes().write().await;
    f(&mut idx)
}
