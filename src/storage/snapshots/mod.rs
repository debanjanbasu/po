//! Snapshot history metadata and CRDT compaction for [`CrdtModel`] types.
//!
//! When an Automerge doc's *raw* (uncompressed) serialization exceeds a size
//! threshold *and* has grown a further threshold past the row's compaction
//! watermark, the current snapshot's timestamp and size are added to a
//! retained history index. Both thresholds are in raw doc bytes — the unit
//! they have always had, and the one that does not move when the doc's stored
//! form changes. The watermark is what keeps a doc that stays over the
//! threshold from appending a restore point on every publish (see
//! [`maybe_compact`]).
//!
//! The settings UI lists snapshot history and can time-travel: each snapshot
//! retains its payload so the user can restore backward to any entry and
//! forward again (restoring captures the pre-restore state as a new entry, so
//! "undo" is just restoring to a later entry).

use anyhow::{Context, Result, ensure};
use autosurgeon::{Hydrate, Reconcile};
use dioxus::prelude::*;

use crate::{
    chat::{contact::ContactList, conversation::ConversationIndex},
    config,
    helpers::{async_utils::now_epoch_millis, capnp::capnp_u32, store_lock::lock_store},
    models::{app_state::AppState, totp::TotpTokenList},
    veilid::table_store::Table,
};

use super::store_trait::{
    COL_AUTOMERGE, COL_SNAPSHOT, CrdtModel, DUAL_FORMAT_TABLE_COLS, compaction_watermark_entry,
    decode_snapshot, load_automerge_current_marker, load_local, load_user_retention_policy,
    replace_local,
};

/// Table storing snapshot history indexes for all `CrdtModel` types.
pub(super) const SNAPSHOTS_TABLE: &str = "po_snapshots";

/// Column 0 stores each model's snapshot history index; column 1 stores the
/// restorable Cap'n Proto payload for each snapshot entry.
pub(super) const SNAPSHOTS_COLS: u32 = 2;
const COL_INDEX: u32 = 0;
const COL_PAYLOAD: u32 = 1;

/// Index binary format version. v1 rows (bare count+entries) load via the
/// migration path and are rewritten as v2 on the next append/prune.
const INDEX_FORMAT_VERSION: u8 = 2;

static SNAPSHOT_REVISION: GlobalSignal<u64> = Signal::global(|| 0);

/// Subscribe snapshot-history resources to committed index changes.
#[must_use]
pub fn revision() -> u64 {
    SNAPSHOT_REVISION()
}

fn notify_snapshots_changed() {
    SNAPSHOT_REVISION.with_mut(|revision| *revision = revision.wrapping_add(1));
}

/// Threshold in bytes of the doc's *raw* (uncompressed) serialization for
/// Automerge document compaction. Snapshot history retains a restorable
/// payload for every entry, subject to retention.
pub use crate::config::storage::COMPACTION_THRESHOLD as DEFAULT_COMPACTION_THRESHOLD;
/// Hard cap on retained snapshot entries per model per user (UI `aria-valuemax`).
pub use crate::config::storage::MAX_SNAPSHOTS;

/// User-configurable retention policy for CRDT snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reconcile, Hydrate)]
pub enum RetentionPolicy {
    /// Keep snapshots for 30 days.
    #[default]
    Days30,
    /// Keep snapshots for 90 days.
    Days90,
    /// Keep all snapshots indefinitely.
    Forever,
}

impl RetentionPolicy {
    pub(super) const fn retention_ms(self) -> Option<u64> {
        match self {
            Self::Days30 => Some(30 * 24 * 60 * 60 * 1000),
            Self::Days90 => Some(90 * 24 * 60 * 60 * 1000),
            Self::Forever => None,
        }
    }

    /// Encode as a `u8` for Cap'n Proto serialization.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Days30 => 0,
            Self::Days90 => 1,
            Self::Forever => 2,
        }
    }

    /// Decode from a `u8` produced by [`to_u8`], defaulting to `Days30`.
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Days90,
            2 => Self::Forever,
            _ => Self::Days30,
        }
    }
}

/// A snapshot history entry.
#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    /// Milliseconds since epoch when the snapshot was taken.
    pub timestamp: u64,
    /// Size of the Cap'n Proto snapshot in bytes.
    pub size: u32,
}

/// Builds the index key for a model's snapshot list.
fn index_key(model_id: &str, account_key: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(model_id.len() + 1 + account_key.len());
    key.extend_from_slice(model_id.as_bytes());
    key.push(0);
    key.extend_from_slice(account_key);
    key
}

/// Builds the `COL_PAYLOAD` key for one snapshot entry. Big-endian timestamp so
/// key order matches time order. Same layout as [`index_key`] plus the stamp.
fn payload_key(model_id: &str, account_key: &[u8], timestamp: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(model_id.len() + 1 + account_key.len() + 1 + 8);
    key.extend_from_slice(model_id.as_bytes());
    key.push(0);
    key.extend_from_slice(account_key);
    key.push(0);
    key.extend_from_slice(&timestamp.to_be_bytes());
    key
}

const SNAPSHOT_INDEX_LOCK_PREFIX: &[u8] = b"\0oo.snapshot-index\0";

fn snapshot_index_lock_key(model_id: &str, account_key: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(
        SNAPSHOT_INDEX_LOCK_PREFIX.len() + model_id.len() + 1 + account_key.len(),
    );
    key.extend_from_slice(SNAPSHOT_INDEX_LOCK_PREFIX);
    key.extend_from_slice(model_id.as_bytes());
    key.push(0);
    key.extend_from_slice(account_key);
    key
}

fn next_snapshot_timestamp(entries: &[SnapshotEntry], requested: u64) -> u64 {
    entries.iter().fold(requested, |next, entry| {
        next.max(entry.timestamp.saturating_add(1))
    })
}

// v2: [u8 version=2][u32 count LE][{timestamp u64 LE, size u32 LE}...]
fn serialize_index(entries: &[SnapshotEntry]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + entries.len() * 12);
    buf.push(INDEX_FORMAT_VERSION);
    let count = capnp_u32(entries.len());
    buf.extend_from_slice(&count.to_le_bytes());
    for entry in entries {
        buf.extend_from_slice(&entry.timestamp.to_le_bytes());
        buf.extend_from_slice(&entry.size.to_le_bytes());
    }
    buf
}

fn deserialize_index(bytes: &[u8]) -> Result<Vec<SnapshotEntry>> {
    // v2: [u8 version=2][u32 LE count][rows...]. A v1 row starts with the
    // 4-byte LE count, which collides with the v2 marker byte whenever the
    // count's low byte is 2 (i.e. exactly two entries) — so disambiguate by
    // total length: valid v2 framing is 1 + 4 + 12*count bytes; anything else
    // (including the empty index, either format) parses as v1.
    if bytes.len() >= 5 && bytes[0] == INDEX_FORMAT_VERSION {
        let count = u32::from_le_bytes(bytes[1..5].try_into()?);
        if bytes.len() == 5 + 12 * usize::try_from(count)? {
            return parse_index_rows(count, &bytes[5..]);
        }
    }
    let header: [u8; 4] = *bytes
        .first_chunk::<4>()
        .context("Snapshot index is missing its count")?;
    parse_index_rows(u32::from_le_bytes(header), &bytes[4..])
}

fn parse_index_rows(count: u32, data: &[u8]) -> Result<Vec<SnapshotEntry>> {
    let count = usize::try_from(count)?;
    let (rows, trailing) = data.as_chunks::<12>();
    ensure!(
        trailing.is_empty() && rows.len() == count,
        "Snapshot index length does not match its declared count"
    );
    let mut entries = rows
        .iter()
        .map(|row| {
            Ok(SnapshotEntry {
                timestamp: u64::from_le_bytes(row[..8].try_into()?),
                size: u32::from_le_bytes(row[8..].try_into()?),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_unstable_by_key(|entry| entry.timestamp);
    ensure!(
        entries
            .windows(2)
            .all(|pair| pair[0].timestamp != pair[1].timestamp),
        "Snapshot index contains duplicate timestamps"
    );
    Ok(entries)
}

async fn load_index(ik: &[u8]) -> Result<Vec<SnapshotEntry>> {
    Table::open(SNAPSHOTS_TABLE, SNAPSHOTS_COLS)
        .await?
        .load_opt(COL_INDEX, ik)
        .await?
        .map_or_else(|| Ok(Vec::new()), |bytes| deserialize_index(&bytes))
}

fn snapshot_index_key_matches_account(key: &[u8], account_key: &[u8]) -> bool {
    key.iter()
        .position(|byte| *byte == 0)
        .is_some_and(|sep| key.get(sep + 1..) == Some(account_key))
}

/// True when a payload key belongs to the account: `model_id\0account_key\0ts`.
///
/// The 8-byte big-endian timestamp routinely contains NUL bytes, so matching
/// from the end (`rposition`) is unsound; instead anchor on the first NUL after
/// the model id, exactly like [`snapshot_index_key_matches_account`].
fn payload_key_matches_account(key: &[u8], account_key: &[u8]) -> bool {
    let Some(sep) = key.iter().position(|byte| *byte == 0) else {
        return false;
    };
    let account_end = sep + 1 + account_key.len();
    key.get(sep + 1..account_end) == Some(account_key)
        && key.get(account_end) == Some(&0)
        && key.len() == account_end + 1 + 8
}

/// Deletes `COL_PAYLOAD` rows for every entry dropped from the index.
async fn delete_orphaned_payloads<T: CrdtModel>(
    account_key: &[u8],
    dropped: &[SnapshotEntry],
) -> Result<()> {
    if dropped.is_empty() {
        return Ok(());
    }
    let table = Table::open(SNAPSHOTS_TABLE, SNAPSHOTS_COLS).await?;
    let payload_keys: Vec<Vec<u8>> = dropped
        .iter()
        .map(|entry| payload_key(T::ID, account_key, entry.timestamp))
        .collect();
    let delete_refs: Vec<(u32, &[u8])> = payload_keys
        .iter()
        .map(|key| (COL_PAYLOAD, key.as_slice()))
        .collect();
    table.apply_batch(&delete_refs, &[]).await
}

/// Prunes and returns the dropped entries so callers can remove their payloads.
fn prune_entries_with_dropped(
    entries: &mut Vec<SnapshotEntry>,
    policy: Option<RetentionPolicy>,
) -> Vec<SnapshotEntry> {
    if let Some(window) = policy.and_then(RetentionPolicy::retention_ms) {
        let cutoff = now_epoch_millis().saturating_sub(window);
        entries.retain(|entry| entry.timestamp >= cutoff);
    }
    let excess = entries.len().saturating_sub(config::storage::MAX_SNAPSHOTS);
    entries.drain(..excess).collect()
}

async fn append_snapshot_entry<T: CrdtModel>(
    account_key: &[u8],
    snapshot_bytes: &[u8],
    retention_policy: Option<RetentionPolicy>,
) -> Result<()> {
    let ik = index_key(T::ID, account_key);
    let _guard = lock_store(&snapshot_index_lock_key(T::ID, account_key)).await;
    let mut entries = load_index(&ik).await?;

    // The newest entry already holds these exact bytes: a second entry would
    // add no restorable state and, under `MAX_SNAPSHOTS`, evict a distinct one
    // to make room for the duplicate. Retention is deferred, not changed —
    // the next distinct append and `apply_retention` prune by the same policy.
    if let Some(newest) = entries.last()
        && Table::open(SNAPSHOTS_TABLE, SNAPSHOTS_COLS)
            .await?
            .load_opt(
                COL_PAYLOAD,
                &payload_key(T::ID, account_key, newest.timestamp),
            )
            .await?
            .as_deref()
            == Some(snapshot_bytes)
    {
        return Ok(());
    }

    let timestamp = next_snapshot_timestamp(&entries, now_epoch_millis());

    // Payload before index: an entry without its payload cannot be restored, so
    // a failed payload write leaves the index untouched rather than recording a
    // dead entry. The failure propagates: callers like [`restore_to_snapshot`]
    // must not proceed (and [`maybe_compact`] must not advance its watermark)
    // over a snapshot that was never recorded.
    Table::open(SNAPSHOTS_TABLE, SNAPSHOTS_COLS)
        .await?
        .store(
            COL_PAYLOAD,
            &payload_key(T::ID, account_key, timestamp),
            snapshot_bytes,
        )
        .await
        .context("Failed to store snapshot payload")?;

    entries.push(SnapshotEntry {
        timestamp,
        size: capnp_u32(snapshot_bytes.len()),
    });
    let dropped = prune_entries_with_dropped(&mut entries, retention_policy);
    persist_snapshot_entries::<T>(account_key, &entries).await?;
    delete_orphaned_payloads::<T>(account_key, &dropped).await?;
    Ok(())
}

async fn persist_snapshot_entries<T: CrdtModel>(
    account_key: &[u8],
    entries: &[SnapshotEntry],
) -> Result<()> {
    let index_key = index_key(T::ID, account_key);
    let index_bytes = serialize_index(entries);
    Table::open(SNAPSHOTS_TABLE, SNAPSHOTS_COLS)
        .await?
        .store(COL_INDEX, &index_key, &index_bytes)
        .await?;
    notify_snapshots_changed();
    Ok(())
}

/// Check if the automerge doc needs compaction and perform it if so.
///
/// Runs when the doc exceeds `threshold` *and* has grown a further `threshold`
/// past the row's compaction watermark (the trigger in
/// [`crate::storage::store_trait::live_doc`]). `doc_bytes` is the doc as
/// stored — automerge's DEFLATE form — and is both what the compare below
/// reads and what the batch writes back; `raw_len` is that doc's *uncompressed*
/// serialization length, the unit `threshold` and the watermark are in. The
/// doc is already in the single stored form, so compaction adds only the
/// restore point: it records the current Cap'n Proto snapshot, then advances
/// the watermark in the same batch as the doc.
///
/// Returns `true` if the doc was compacted.
pub async fn maybe_compact<T: CrdtModel>(
    account_key: &[u8],
    doc_bytes: &[u8],
    raw_len: usize,
    snapshot_bytes: &[u8],
    expected_marker: &[u8],
    threshold: usize,
) -> Result<bool> {
    if raw_len <= threshold {
        return Ok(false);
    }

    let model_table = T::TABLE_NAME;
    let _account_guard = lock_store(account_key).await;

    let Some(current_snapshot) = Table::open(model_table, DUAL_FORMAT_TABLE_COLS)
        .await?
        .load_opt(COL_SNAPSHOT, account_key)
        .await?
    else {
        return Ok(false);
    };
    if current_snapshot != snapshot_bytes {
        return Ok(false);
    }

    let Some(current_doc) = Table::open(model_table, DUAL_FORMAT_TABLE_COLS)
        .await?
        .load_opt(COL_AUTOMERGE, account_key)
        .await?
    else {
        return Ok(false);
    };
    if current_doc != doc_bytes {
        return Ok(false);
    }

    if load_automerge_current_marker(model_table, account_key)
        .await?
        .as_deref()
        != Some(expected_marker)
    {
        return Ok(false);
    }

    let policy = load_user_retention_policy(account_key).await?;
    append_snapshot_entry::<T>(account_key, &current_snapshot, Some(policy)).await?;

    // The watermark is the raw length the *trigger* measured, not the stored
    // DEFLATE length: a watermark in the stored form would sit 2–3.5× below
    // every future measurement and gate nothing. Doc and watermark commit
    // together so a failed write cannot leave a watermark behind a doc it never
    // described.
    let (watermark_key, watermark) = compaction_watermark_entry(account_key, raw_len);
    Table::open(model_table, DUAL_FORMAT_TABLE_COLS)
        .await?
        .apply_batch(
            &[],
            &[
                (COL_AUTOMERGE, account_key, doc_bytes),
                (COL_AUTOMERGE, &watermark_key, watermark.as_slice()),
            ],
        )
        .await?;

    Ok(true)
}

/// Lists the retained snapshot history for a model, oldest first.
///
/// Every listed entry has a retained payload: the payload row is written
/// before its index entry, so the two always pair up.
pub async fn list_snapshots<T: CrdtModel>(account_key: &[u8]) -> Result<Vec<SnapshotEntry>> {
    let ik = index_key(T::ID, account_key);
    load_index(&ik).await
}

/// Records a manual snapshot and retains its restorable payload.
pub async fn create_manual_snapshot<T: CrdtModel>(account_key: &[u8], value: &T) -> Result<()> {
    let snapshot_bytes = value.pack()?;
    let policy = load_user_retention_policy(account_key).await?;
    append_snapshot_entry::<T>(account_key, &snapshot_bytes, Some(policy)).await
}

/// Restores a model's local state to a retained snapshot.
///
/// The pre-restore state is captured as a new snapshot entry first (subject to
/// the current retention policy), so moving forward again is just restoring to
/// a later entry in the same list. Returns `Ok(false)` when the target payload
/// equals the current state (no-op). On `Ok(true)` the caller must enqueue the
/// model's `OP_TYPE` so the restored state re-publishes to the DHT and other
/// devices converge.
///
/// Fails with "Snapshot has no restorable payload" for entries without a
/// retained payload, and with a downcastable
/// [`SchemaVersionError`](crate::storage::schema_version::SchemaVersionError)
/// when the retained payload's schema version differs from the running one
/// (the cause is preserved through the `anyhow` chain).
pub async fn restore_to_snapshot<T: CrdtModel>(account_key: &[u8], timestamp: u64) -> Result<bool> {
    let payload = Table::open(SNAPSHOTS_TABLE, SNAPSHOTS_COLS)
        .await?
        .load_opt(COL_PAYLOAD, &payload_key(T::ID, account_key, timestamp))
        .await?
        .context("Snapshot has no restorable payload")?;
    let value = decode_snapshot::<T>(&payload)?;

    // Capture the pre-restore state as a new entry (skipped when restoring is
    // a no-op, so undo chains don't accumulate identical snapshots).
    let current_bytes = load_local::<T>(account_key).await?.pack()?;
    if current_bytes == payload {
        return Ok(false);
    }
    let policy = load_user_retention_policy(account_key).await?;
    append_snapshot_entry::<T>(account_key, &current_bytes, Some(policy)).await?;

    replace_local::<T>(account_key, &value).await?;
    Ok(true)
}

/// Immediately applies a retention policy to a model's snapshots.
///
/// Called when the user changes the retention setting so old snapshots
/// are pruned right away — not deferred until the next compaction event.
pub async fn apply_retention<T: CrdtModel>(account_key: &[u8]) -> Result<()> {
    let _account_guard = lock_store(account_key).await;
    let policy = load_user_retention_policy(account_key).await?;
    let ik = index_key(T::ID, account_key);
    let _guard = lock_store(&snapshot_index_lock_key(T::ID, account_key)).await;
    let mut entries = load_index(&ik).await?;
    let dropped = prune_entries_with_dropped(&mut entries, Some(policy));
    if dropped.is_empty() {
        return Ok(());
    }
    // Index before payload: a dropped entry must leave the index first. The
    // reverse order would delete a payload whose entry is still listed, making
    // it unrestorable if the index write then failed.
    persist_snapshot_entries::<T>(account_key, &entries).await?;
    delete_orphaned_payloads::<T>(account_key, &dropped).await
}

/// [`apply_retention`] for every `CrdtModel` surface in one call.
///
/// Per-surface order is unchanged (`apply_retention` still takes the account
/// lock, then the surface's index lock), and each call still commits its own
/// surface, so a failure part-way leaves the surfaces already pruned correct.
pub async fn apply_retention_all(account_key: &[u8]) -> Result<()> {
    apply_retention::<TotpTokenList>(account_key).await?;
    apply_retention::<ContactList>(account_key).await?;
    apply_retention::<ConversationIndex>(account_key).await?;
    apply_retention::<AppState>(account_key).await?;
    Ok(())
}

pub async fn delete_all_for_account(account_key: &[u8]) -> Result<()> {
    let table = Table::open(SNAPSHOTS_TABLE, SNAPSHOTS_COLS).await?;
    // Both columns share the shared account-match scan via a byte predicate:
    // index keys are `model_id\0account_key` and payload keys are
    // `model_id\0account_key\0ts`, so the matchers differ in shape only.
    let mut delete_keys: Vec<(u32, Vec<u8>)> = Vec::new();
    for (col, matches) in [
        (
            COL_INDEX,
            snapshot_index_key_matches_account as fn(&[u8], &[u8]) -> bool,
        ),
        (
            COL_PAYLOAD,
            payload_key_matches_account as fn(&[u8], &[u8]) -> bool,
        ),
    ] {
        let keys: Vec<bytes::Bytes> = table
            .raw()
            .get_keys(col)
            .await?
            .into_iter()
            .filter(|key| matches(key, account_key))
            .collect();
        delete_keys.extend(keys.into_iter().map(|key| (col, key.to_vec())));
    }
    let delete_refs: Vec<(u32, &[u8])> = delete_keys
        .iter()
        .map(|(col, key)| (*col, key.as_slice()))
        .collect();
    table.apply_batch(&delete_refs, &[]).await?;
    notify_snapshots_changed();
    Ok(())
}
