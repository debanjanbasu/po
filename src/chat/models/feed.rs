//! Participant feed and message page models for group chat.

use std::{
    cmp::Ordering as CmpOrdering,
    collections::HashSet,
    hash::{BuildHasher, Hasher},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{config, helpers::async_utils::now_epoch_millis};
use anyhow::{Context, Result, ensure};
use autosurgeon::{Hydrate, Reconcile};
use dioxus::logger::tracing::warn;

// ── Enums ──────────────────────────────────────────────────────────────────

/// Local delivery state for an outgoing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reconcile, Hydrate)]
pub enum MessageStatus {
    #[default]
    Sent,
    Pending,
    Failed,
}

// ── Models ─────────────────────────────────────────────────────────────────

/// A single message within a participant feed.
#[derive(Debug, Clone, PartialEq, Eq, Reconcile, Hydrate)]
pub struct FeedMessage {
    pub seq: u64,
    #[autosurgeon(with = "crate::chat::models::arc_str")]
    pub sender_username: Arc<str>,
    #[autosurgeon(with = "crate::chat::models::arc_str")]
    pub content: Arc<str>,
    pub timestamp: u64,
    /// Unique message ID: exactly [`config::chat::MSG_ID_HEX_LEN`] lowercase hex
    /// characters ([`generate_msg_id`]). Generated on send, used for
    /// cross-device dedup via [`message_dedup_key`].
    #[autosurgeon(with = "crate::chat::models::arc_str")]
    pub msg_id: Arc<str>,
    pub status: MessageStatus,
}

/// Fixed per-hasher seeds for [`msg_id_dedup_key`]'s two hash lanes.
///
/// Distinct lanes widen the 64-bit `foldhash` output to a 128-bit key, so two
/// distinct ids collide only on a ~2^-128 double hash hit, and neither lane is
/// derived from attacker-visible data.
const DEDUP_KEY_SEED_A: u64 = 0x706f_6d73_675f_6964; // "pomsgid"
/// Second lane's seed; any constant distinct from [`DEDUP_KEY_SEED_A`] works.
const DEDUP_KEY_SEED_B: u64 = 0x706f_6d73_675f_6965;

/// One 64-bit `foldhash` lane over `bytes` with a fixed seed.
///
/// Fixed seeds, not a `RandomState`: the key only ever lives inside one
/// process' dedup sets, so a per-process seed would buy nothing and this keeps
/// the key stable enough to reason about (and to assert on) across runs.
fn dedup_hash_lane(seed: u64, bytes: &[u8]) -> u64 {
    let mut hasher = foldhash::fast::FixedState::with_seed(seed).build_hasher();
    hasher.write(bytes);
    hasher.finish()
}

/// The dedup key of a `msg_id`, computed straight from the string.
///
/// Two fixed-seed `foldhash` lanes give a 128-bit key, so distinct ids collide
/// only on a ~2^-128 double hash hit — versus the previous `u128::from_str_radix`
/// parse, where `"10"`, `"+10"` and `"0010"` all keyed identically (and hex was
/// case-folded), letting two distinct remote messages alias one key and one of
/// them be dropped as a duplicate. No memo: hashing an id is cheaper than the
/// sharded lock the memo took on every merge, and the merge path runs
/// unbounded-concurrent, where any shared lock is the bottleneck.
pub fn message_dedup_key(message: &FeedMessage) -> u128 {
    msg_id_dedup_key(&message.msg_id)
}

/// [`message_dedup_key`] over a raw id.
#[must_use]
pub fn msg_id_dedup_key(msg_id: &str) -> u128 {
    let low = dedup_hash_lane(DEDUP_KEY_SEED_A, msg_id.as_bytes());
    let high = dedup_hash_lane(DEDUP_KEY_SEED_B, msg_id.as_bytes());
    (u128::from(high) << 64) | u128::from(low)
}

#[must_use]
pub fn cmp_messages(a: &FeedMessage, b: &FeedMessage) -> CmpOrdering {
    (a.timestamp, a.seq)
        .cmp(&(b.timestamp, b.seq))
        .then_with(|| a.sender_username.cmp(&b.sender_username))
        .then_with(|| a.msg_id.cmp(&b.msg_id))
        .then_with(|| a.content.cmp(&b.content))
}

pub fn sort_messages(messages: &mut [FeedMessage]) {
    messages.sort_by(cmp_messages);
}

pub fn insert_sorted_message(messages: &mut Vec<FeedMessage>, message: FeedMessage) {
    if messages
        .last()
        .is_none_or(|last| cmp_messages(last, &message) != CmpOrdering::Greater)
    {
        messages.push(message);
        return;
    }
    let insert_at =
        messages.partition_point(|current| cmp_messages(current, &message) != CmpOrdering::Greater);
    messages.insert(insert_at, message);
}

/// Normalizes a page that came off the wire: every message takes the roster
/// entry's username and `Sent`, and any `msg_id` that is not a canonical id is
/// **dropped** here rather than carried, keyed and persisted.
///
/// A signed member slot, not message text, determines the remote sender: the
/// caller passes the username of the roster entry whose subkey this page was
/// read from, and that entry's writer slot is bound to its identity key when
/// the roster is loaded (`chat::conv_keys::verify_roster_binding`).
///
/// This is the single choke point every remote page passes through — the
/// watcher, hydration, feed sync and archive backfill all call it — so it is
/// where an id is still cheap to refuse. A valid id is exactly
/// [`MSG_ID_HEX_LEN`] lowercase hex characters ([`generate_msg_id`]). Refusing
/// anything else at the wire keeps the merge's dedup key injective for every id
/// a page can carry: a peer cannot alias one message onto another with a
/// `"+10"`-style variant of its id, and a page can no longer pin megabytes of
/// string in the merge path.
pub fn normalize_remote_messages(messages: &mut Vec<FeedMessage>, sender_username: &str) {
    messages.retain(|message| {
        let accepted = is_canonical_msg_id(&message.msg_id);
        if !accepted {
            warn!(
                "Dropped a remote message with a non-canonical {}-byte msg_id from {} (want {} lowercase hex chars)",
                message.msg_id.len(),
                message.sender_username,
                config::chat::MSG_ID_HEX_LEN,
            );
        }
        accepted
    });
    let sender: Arc<str> = Arc::from(sender_username);
    for message in messages {
        message.sender_username = Arc::clone(&sender);
        message.status = MessageStatus::Sent;
    }
}

/// Whether `msg_id` is a canonical [`generate_msg_id`] id: exactly
/// [`config::chat::MSG_ID_HEX_LEN`] lowercase hex characters.
#[must_use]
pub fn is_canonical_msg_id(msg_id: &str) -> bool {
    msg_id.len() == config::chat::MSG_ID_HEX_LEN
        && msg_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Unions `incoming` into `messages` by [`message_dedup_key`], keeping the
/// existing copy on a dedup match (local status transitions like
/// Pending→Sent must win over a stale remote copy) and re-sorting only when
/// an insertion violates the existing order.
///
/// This is the single canonical message merge: the watcher (remote pages),
/// hydration (head/archive pages), and the local store's merge-on-persist
/// all share these semantics. It never removes a message from either side.
///
/// Returns the newest message after the union when anything was added,
/// `None` when the union was a no-op (which includes an empty `incoming`).
///
/// `incoming` is a `Vec`, not `impl IntoIterator`, so the empty case can be
/// answered before the seen set is built: every caller already holds one, and
/// keying the local conversation per call (`message_dedup_key` takes the
/// process-global memo's lock) to union nothing into it is pure loss. A caller
/// folding several incoming sets into one conversation should build the set
/// once and use [`union_messages_in_place_with_seen`].
pub fn union_messages_in_place(
    messages: &mut Arc<Vec<FeedMessage>>,
    incoming: Vec<FeedMessage>,
) -> Option<FeedMessage> {
    if incoming.is_empty() {
        return None;
    }
    let mut seen: HashSet<u128> = messages.iter().map(message_dedup_key).collect();
    union_messages_in_place_with_seen(messages, incoming, &mut seen)
}

pub(in crate::chat) fn union_messages_in_place_with_seen(
    messages: &mut Arc<Vec<FeedMessage>>,
    incoming: impl IntoIterator<Item = FeedMessage>,
    seen: &mut HashSet<u128>,
) -> Option<FeedMessage> {
    let mut changed = false;
    let mut needs_sort = false;
    for message in incoming {
        if !seen.insert(message_dedup_key(&message)) {
            continue;
        }
        let messages = Arc::make_mut(messages);
        if messages
            .last()
            .is_some_and(|last| cmp_messages(&message, last) == CmpOrdering::Less)
        {
            needs_sort = true;
        }
        messages.push(message);
        changed = true;
    }
    if !changed {
        return None;
    }
    if needs_sort {
        sort_messages(Arc::make_mut(messages).as_mut_slice());
    }
    messages.last().cloned()
}

pub fn message_seq_bounds(messages: &[FeedMessage]) -> (u64, u64) {
    let mut seq_start = 0;
    let mut seq_end = 0;
    for message in messages {
        if seq_start == 0 || message.seq < seq_start {
            seq_start = message.seq;
        }
        seq_end = seq_end.max(message.seq);
    }
    (seq_start, seq_end)
}

/// Generates a unique 32-char hex message ID: timestamp (64 bits) +
/// monotonic counter (32 bits) + random suffix (32 bits).
///
/// The random suffix keeps IDs unique across devices on the same account:
/// two devices sending at the same millisecond with equal counter values
/// (both restart from 0 per process) would otherwise collide and be dropped
/// as duplicates by `message_dedup_key`. Randomness failure is returned to the
/// caller; a predictable fallback could collide with another device's ID.
pub fn generate_msg_id() -> Result<Arc<str>> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = now_epoch_millis();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed) & u64::from(u32::MAX);
    let mut rand_bytes = [0u8; 4];
    getrandom::fill(&mut rand_bytes).context("Failed to generate message ID randomness")?;
    let rand = u32::from_le_bytes(rand_bytes);
    Ok(Arc::from(
        format!("{ts:016x}{counter:08x}{rand:08x}").as_str(),
    ))
}

/// Metadata for a participant's feed record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct ParticipantFeedMeta {
    pub last_seq: u64,
    pub last_message_at: u64,
    /// Writable archive tail; the immutable root is stored separately.
    pub archive_key: String,
    pub head_page_start_seq: u64,
    /// Chain generation of the current archive record (0 = first archive).
    /// Scopes the derived archive owner keypair.
    pub archive_generation: u32,
    #[autosurgeon(missing = "String::new")]
    pub archive_root_key: String,
    /// Monotonic rollover counter, incremented from the previous epoch when a
    /// NEW rollover begins (generation resets to 0). 0 = the pre-epoch steady
    /// state. Derived from chain state, so skew-proof by construction.
    #[autosurgeon(missing = "zero_u64")]
    pub archive_epoch: u64,
}

const fn zero_u64() -> u64 {
    0
}

impl ParticipantFeedMeta {
    /// Preserve rotation progress and sequence counters from either device.
    pub fn merge_from(&mut self, other: Self) -> bool {
        let mut changed = false;
        // A rollover resets the generation counter, so generation numbers only
        // compare meaningfully within one epoch. Unequal epochs still
        // arbitrate on epoch alone (higher epoch = newer chain), but epochs
        // are derived counters: two racing devices derive the SAME next epoch
        // and converge on the equal-epoch path, where the append-once guard
        // arbitrates the slot. Clock skew cannot diverge the values.
        match other.archive_epoch.cmp(&self.archive_epoch) {
            CmpOrdering::Greater => {
                self.archive_epoch = other.archive_epoch;
                self.archive_generation = other.archive_generation;
                self.archive_key = other.archive_key;
                self.archive_root_key = other.archive_root_key;
                changed = true;
            }
            CmpOrdering::Equal => {
                match other.archive_generation.cmp(&self.archive_generation) {
                    CmpOrdering::Greater => {
                        self.archive_generation = other.archive_generation;
                        self.archive_key = other.archive_key;
                        changed = true;
                    }
                    CmpOrdering::Equal
                        if self.archive_key.is_empty() && !other.archive_key.is_empty() =>
                    {
                        self.archive_key = other.archive_key;
                        changed = true;
                    }
                    CmpOrdering::Equal | CmpOrdering::Less => {}
                }
                if self.archive_root_key.is_empty() && !other.archive_root_key.is_empty() {
                    self.archive_root_key = other.archive_root_key;
                    changed = true;
                }
            }
            CmpOrdering::Less => {}
        }
        if other.head_page_start_seq > self.head_page_start_seq {
            self.head_page_start_seq = other.head_page_start_seq;
            changed = true;
        }
        if other.last_seq > self.last_seq {
            self.last_seq = other.last_seq;
            changed = true;
        }
        if other.last_message_at > self.last_message_at {
            self.last_message_at = other.last_message_at;
            changed = true;
        }
        changed
    }
}

/// A page of messages stored in a DHT subkey.
#[derive(Debug, Clone, Default, Reconcile, Hydrate)]
pub struct MessagePage {
    pub messages: Vec<FeedMessage>,
    pub page_seq_start: u64,
    pub page_seq_end: u64,
}

/// Index stored in archive DFLT subkey 0 — tracks page count and chain pointer.
#[derive(Debug, Clone, Default, Reconcile, Hydrate)]
pub struct ArchiveIndex {
    pub page_count: u32,
    pub next_archive_key: String,
    pub seq_ranges: Vec<SeqRange>,
}

impl ArchiveIndex {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.page_count <= u32::from(config::chat::ARCHIVE_MAX_PAGES)
                && self.seq_ranges.len() == self.page_count as usize
                && self.seq_ranges.iter().all(|range| range.start <= range.end),
            "Invalid archive index page layout"
        );
        Ok(())
    }
}

/// Sequence range for a single archive page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct SeqRange {
    pub start: u64,
    pub end: u64,
}
