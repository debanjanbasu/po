//! Dual-format local rows: a Cap'n Proto snapshot (col 0) beside the
//! automerge doc (col 1), plus the reserved metadata entries that keep the
//! two in step.

use std::borrow::Cow;

use anyhow::{Error, Result};
use automerge::AutoCommit;
use autosurgeon::hydrate;
use bytes::Bytes;
use tokio::try_join;

use crate::{
    helpers::{
        async_utils::spawn_blocking, encryption::crypto_system_sync, store_lock::lock_store,
    },
    models::app_state::AppState,
    storage::{
        automerge::{
            AutomergeSyncError, from_automerge_async, merge_and_hydrate_with_stale_check,
            reconcile_then_merge_remote, reconcile_to_automerge_async,
            reconcile_to_automerge_measured,
        },
        background_errors::{report_nonfatal, spawn_report},
        schema_version::{SchemaVersionError, unpack_versioned},
        snapshots::{self, DEFAULT_COMPACTION_THRESHOLD, RetentionPolicy},
    },
    veilid::table_store::Table,
};

use super::{CrdtModel, notify_model_changed};

/// The reserved `COL_AUTOMERGE` metadata entries a dual-format row can carry
/// beside its automerge doc.
///
/// The single enumeration every reserved-key builder and [`delete_row`] derive
/// from, so a wiped row can never leave a metadata entry behind — and a new
/// reserved entry is added by naming it here, nowhere else.
#[derive(Clone, Copy)]
pub enum ReservedMeta {
    /// Hash of the snapshot bytes the doc reflects; the pair [`live_doc`]
    /// trusts to skip reconciling.
    Currentness,
    /// Raw (uncompressed) doc length at the last successful compaction — the
    /// unit the compaction trigger measures (see
    /// [`load_compaction_watermark`]).
    CompactionWatermark,
}

impl ReservedMeta {
    /// Every reserved entry for a row. [`delete_row`] deletes all of them.
    pub const ALL: [Self; 2] = [Self::Currentness, Self::CompactionWatermark];

    const fn prefix(self) -> &'static [u8] {
        match self {
            Self::Currentness => b"\0oo.currentness\0",
            Self::CompactionWatermark => b"\0oo.compaction-watermark\0",
        }
    }

    /// This entry's key for a row. Passing a row-key *prefix* yields the key
    /// prefix covering every row in that range, which is how a range delete
    /// finds metadata for rows it cannot enumerate.
    #[must_use]
    pub fn key(self, key: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.prefix().len() + key.len());
        out.extend_from_slice(self.prefix());
        out.extend_from_slice(key);
        out
    }
}

/// The `COL_AUTOMERGE` currentness-marker key for a row — shorthand for
/// [`ReservedMeta::Currentness`], the entry the save/merge paths name directly.
#[must_use]
pub fn automerge_marker_key(key: &[u8]) -> Vec<u8> {
    ReservedMeta::Currentness.key(key)
}

/// Deletes a dual-format row: snapshot, automerge doc, and every reserved
/// metadata entry, in one `apply_batch`.
///
/// One batch, not three deletes: a half-wiped row (snapshot gone, doc left)
/// would reconcile the retained doc back and re-mark it current against a
/// re-created snapshot. Callers hold the row's store lock.
pub async fn delete_row(table: &Table, key: &[u8]) -> Result<()> {
    let meta_keys: Vec<Vec<u8>> = ReservedMeta::ALL.iter().map(|meta| meta.key(key)).collect();
    let mut deletes: Vec<(u32, &[u8])> = vec![(COL_SNAPSHOT, key), (COL_AUTOMERGE, key)];
    deletes.extend(
        meta_keys
            .iter()
            .map(|meta_key| (COL_AUTOMERGE, meta_key.as_slice())),
    );
    table.apply_batch(&deletes, &[]).await
}

/// The `(key, value)` pair recording a successful compaction's watermark: the
/// raw doc length the trigger measured, so the next attempt waits for the doc
/// to grow a further threshold instead of recompacting on every publish.
#[must_use]
pub fn compaction_watermark_entry(key: &[u8], doc_len: usize) -> (Vec<u8>, [u8; 8]) {
    (
        ReservedMeta::CompactionWatermark.key(key),
        u64::try_from(doc_len).unwrap_or(u64::MAX).to_le_bytes(),
    )
}

/// The raw doc length at a row's last successful compaction; `0` when never
/// compacted. A malformed entry is reported and treated as absent — the only
/// consequence is one redundant compaction attempt.
pub async fn load_compaction_watermark(table: &Table, key: &[u8]) -> Result<usize> {
    let Some(bytes) = table
        .load_opt(COL_AUTOMERGE, &ReservedMeta::CompactionWatermark.key(key))
        .await?
    else {
        return Ok(0);
    };
    let Some(word) = bytes.first_chunk::<8>().copied() else {
        report_nonfatal(
            "storage.store_trait",
            "Malformed compaction watermark",
            format!("stored {} bytes, expected 8", bytes.len()),
        );
        return Ok(0);
    };
    Ok(usize::try_from(u64::from_le_bytes(word)).unwrap_or(usize::MAX))
}

pub async fn load_automerge_current_marker(
    table: &'static str,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    let meta_key = automerge_marker_key(key);
    Table::open(table, DUAL_FORMAT_TABLE_COLS)
        .await?
        .load_opt(COL_AUTOMERGE, &meta_key)
        .await
        .map(|opt| opt.map(Into::into))
}

/// Column 0: Cap'n Proto snapshot (fast zero-copy reads).
pub const COL_SNAPSHOT: u32 = 0;

/// Column 1: Automerge document bytes (CRDT merge operations) plus reserved
/// currentness metadata entries.
pub const COL_AUTOMERGE: u32 = 1;

/// Number of columns in dual-format local tables (snapshot + automerge).
pub const DUAL_FORMAT_TABLE_COLS: u32 = 2;

/// A model's local automerge doc brought current with its snapshot: the
/// hydrated value, the doc bytes, and the currentness marker (snapshot hash)
/// the doc reflects. Publish paths feed [`Self::doc`] into a merged DHT write
/// and persist the written doc afterwards via [`persist_written_doc`], which
/// merges the published history into the latest local state before updating
/// both formats.
pub struct LiveDoc<T> {
    /// Hydrated model state corresponding to `doc`.
    pub value: T,
    /// Automerge document bytes reflecting `value`.
    pub doc: Vec<u8>,
    /// Hash of the snapshot bytes this doc was reconciled from.
    pub(crate) marker: Vec<u8>,
}

/// Load a model from local Cap'n Proto snapshot (fast path).
/// Returns `Default` if snapshot is missing.
///
/// A snapshot from an older schema version still decodes (see
/// [`decode_snapshot`]); one from a newer version is refused.
pub async fn load_local<T: CrdtModel>(key: &[u8]) -> Result<T> {
    let Some(bytes) = Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS)
        .await?
        .load_opt(COL_SNAPSHOT, key)
        .await?
    else {
        return Ok(T::default());
    };
    decode_snapshot::<T>(&bytes)
}

// ── Write operations ────────────────────────────────────────────────────

/// Save a model locally: Cap'n Proto snapshot only (col 0).
///
/// The automerge doc (col 1) is reconciled on-demand when needed for
/// DHT sync (see [`live_doc`] /
/// [`pull_and_merge`](crate::storage::store_trait::pull_and_merge)).
pub async fn save_local<T: CrdtModel>(key: &[u8], value: &T) -> Result<()> {
    let snapshot = value.pack()?;
    Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS)
        .await?
        .store(COL_SNAPSHOT, key, &snapshot)
        .await?;
    notify_model_changed::<T>();
    Ok(())
}

/// Build the `(doc, marker)` sidecar to commit alongside a snapshot write:
/// the value decoded from `snapshot` is reconciled *into* `existing_doc`, and
/// the marker is the hash of `snapshot` — the pair [`live_doc`] trusts.
///
/// Reconciling into the retained doc (rather than a fresh one) makes removals
/// real CRDT delete ops, causally after the inserts the doc already merged, so
/// the next publish drops them on every device. A missing doc reconciles from
/// empty, which stays correct.
pub async fn reconcile_snapshot_sidecar<T: CrdtModel>(
    existing_doc: Option<Vec<u8>>,
    snapshot: &[u8],
) -> Result<(Vec<u8>, Vec<u8>)> {
    let value = decode_snapshot::<T>(snapshot)?;
    let doc = reconcile_to_automerge_async(existing_doc, value).await?;
    Ok((doc, currentness_marker(snapshot)?))
}

/// Seed a model's local row from a doc the network already holds.
///
/// A model with no local row yet has no local automerge history, so building
/// its doc from the snapshot alone would give it an **independent root**.
/// Automerge replaces a whole map object when two roots are merged, so the
/// next merge against the network's doc keeps one side's map whole and drops
/// the other's keys — a contact written locally would vanish on the next
/// publish. Storing the network's own doc makes the local row descend from
/// the network's history, which is what keeps both sides' keys after a merge.
///
/// A row that already has local state is left alone: the caller raced
/// another writer, and that writer's state is the newer one.
pub async fn seed_local_doc<T: CrdtModel>(key: &[u8], written: &[u8]) -> Result<T> {
    let _guard = lock_store(key).await;
    let table = Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS).await?;
    if table.load_opt(COL_SNAPSHOT, key).await?.is_some() {
        // Local state appeared while the publish was in flight: keep it.
        return load_local::<T>(key).await;
    }
    let value = from_automerge_async::<T>(Bytes::copy_from_slice(written)).await?;
    let snapshot = value.pack()?;
    let marker = currentness_marker(&snapshot)?;
    table
        .apply_batch(
            &[],
            &[
                (COL_SNAPSHOT, key, &snapshot),
                (COL_AUTOMERGE, key, written),
                (COL_AUTOMERGE, &automerge_marker_key(key), &marker),
            ],
        )
        .await?;
    notify_model_changed::<T>();
    Ok(value)
}

/// Replace a model's local state wholesale (backup restore / time travel).
///
/// Unlike [`save_local`], this also brings the automerge doc current in the
/// **same transaction** as the snapshot write. The restored value is
/// reconciled *into* the existing doc, so entries absent from the backup
/// become real CRDT deletes that are causally after the inserts the doc
/// already merged — the next publish merges against the DHT record and the
/// deletion sticks on every device. Reconciling from a fresh doc instead
/// would leave two concurrent root maps, and the merge would keep whichever
/// root wins on `(lamport, actor)` — silently reverting or rewriting the
/// record.
///
/// Residual behaviour: entries created concurrently on another device that
/// this doc has never merged still survive the merge — normal CRDT semantics,
/// since the local doc never observed them as present. A missing doc is
/// reconciled from empty, which stays correct.
///
/// Takes the same per-account store lock as the dual-format merge and
/// `sync::mutate`, so an in-flight reconcile can't write a pre-restore doc or
/// marker back after the replace commits.
pub async fn replace_local<T: CrdtModel>(key: &[u8], value: &T) -> Result<()> {
    let _guard = lock_store(key).await;
    let snapshot = value.pack()?;
    let table = Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS).await?;
    let existing_doc = table.load_opt(COL_AUTOMERGE, key).await?.map(Into::into);
    let (doc, marker) = reconcile_snapshot_sidecar::<T>(existing_doc, &snapshot).await?;
    table
        .apply_batch(
            &[],
            &[
                (COL_SNAPSHOT, key, &snapshot),
                (COL_AUTOMERGE, key, &doc),
                (COL_AUTOMERGE, &automerge_marker_key(key), &marker),
            ],
        )
        .await?;
    notify_model_changed::<T>();
    Ok(())
}

/// Loads the captured account's retention policy. Only an absent `AppState`
/// uses the default; read/schema failures must never authorize pruning.
pub(in crate::storage) async fn load_user_retention_policy(
    account_key: &[u8],
) -> Result<RetentionPolicy> {
    load_local::<AppState>(account_key)
        .await
        .map(|state| state.retention_policy)
}

// ── Remote merge ────────────────────────────────────────────────────────

/// Outcome of merging a pulled remote doc into local state.
#[derive(Clone, Copy)]
pub struct PullOutcome {
    /// Local state changed as a result of the merge.
    pub changed: bool,
    /// The remote doc is missing local history (a publish failed or is
    /// still queued) — the caller should re-publish.
    pub remote_stale: bool,
}

/// Merge remote automerge bytes into the local dual-format doc (reconciling
/// the local doc current first), then persist both formats plus the fresh
/// marker when anything changed.
///
/// When the row's doc is stale (or absent) the remote merge folds into the
/// reconcile's own save — see [`merge_remote_into_stale_row`]. That fused path
/// replaces the pair this used to run: reconcile the doc and write it, then
/// re-parse those bytes, merge, serialize again and write the row a second
/// time. A pull over the largest row a nudge touches (see
/// `storage/automerge.rs`'s measured sizes) paid that twice per round.
pub async fn merge_remote<T: CrdtModel>(key: &[u8], remote_bytes: &[u8]) -> Result<PullOutcome> {
    let _guard = lock_store(key).await;

    let table = Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS).await?;
    match load_row_state::<T>(&table, key).await? {
        RowState::Stale {
            snapshot,
            doc,
            marker,
        } => {
            // Stale or missing doc: the snapshot is still the local state of
            // record, so reconcile it current and fold the remote merge into
            // the same save.
            merge_remote_into_stale_row::<T>(&table, key, snapshot, doc, marker, remote_bytes).await
        }
        RowState::Unsnapshotted => {
            // Nothing local to merge into: the remote doc is the merged doc.
            // (`live_doc` reports no local state here, so the old path took the
            // same shortcut.)
            let remote = remote_bytes.to_vec();
            let (merged_bytes, merged_value, remote_stale): (Vec<u8>, T, bool) =
                spawn_blocking(move || merge_and_hydrate_with_stale_check(None, &remote)).await??;
            store_merged_state::<T>(key, &merged_bytes, &merged_value).await?;
            Ok(PullOutcome {
                changed: true,
                remote_stale,
            })
        }
        RowState::Current { doc, .. } => {
            // Doc already current: one parse, one merge, one save, one write.
            let local_bytes = Some(doc);
            let remote = remote_bytes.to_vec();
            let (merged_bytes, merged_value, remote_stale, changed): (Vec<u8>, T, bool, bool) =
                spawn_blocking(move || {
                    let (merged, value, stale) =
                        merge_and_hydrate_with_stale_check(local_bytes.as_deref(), &remote)?;
                    let changed = local_bytes.as_deref() != Some(merged.as_slice());
                    Ok::<_, Error>((merged, value, stale, changed))
                })
                .await??;

            if changed {
                store_merged_state::<T>(key, &merged_bytes, &merged_value).await?;
            }
            Ok(PullOutcome {
                changed,
                remote_stale,
            })
        }
    }
}

/// Reconciles a stale row's snapshot current and folds the remote merge into
/// the same save — one parse, one merge, one serialization, one batch — then
/// returns what [`merge_remote`] reports.
///
/// End state matches the old reconcile-then-merge pair: the row holds the
/// merged doc, the merged value's snapshot, and that snapshot's marker, so a
/// following [`live_doc`] reads the row as `Current` instead of reconciling
/// again.
///
/// `changed` is what the old byte comparison measured: whether the merge added
/// anything. `merge` reports that directly, which spares a second
/// serialization purely to compare two copies of the doc.
async fn merge_remote_into_stale_row<T: CrdtModel>(
    table: &Table,
    key: &[u8],
    snapshot: Bytes,
    existing_doc: Option<Bytes>,
    marker: Vec<u8>,
    remote_bytes: &[u8],
) -> Result<PullOutcome> {
    let current_value = decode_snapshot::<T>(&snapshot)?;
    let remote = remote_bytes.to_vec();
    // `changed` is what the old path's byte comparison measured: whether the
    // merge added anything to the reconciled doc. The bytes that comparison
    // would have serialized twice are in hand here, so the comparison is free.
    let (doc_bytes, raw_len, merged_value, remote_stale, changed) = spawn_blocking(move || {
        let existing_doc = existing_doc.as_deref();
        let (doc_bytes, raw_len, merged_value, remote_stale) =
            reconcile_then_merge_remote(existing_doc, &current_value, &remote)?;
        let changed = Some(doc_bytes.as_slice()) != existing_doc;
        Ok::<_, Error>((doc_bytes, raw_len, merged_value, remote_stale, changed))
    })
    .await??;

    store_merged_row::<T>(table, key, &doc_bytes, &merged_value).await?;

    // Same compaction trigger `reconcile_row_doc` runs, now measured on the
    // merged doc — the row's actual content after this pull.
    maybe_spawn_compaction::<T>(table, key, &snapshot, &doc_bytes, raw_len, marker).await;

    Ok(PullOutcome {
        changed,
        remote_stale,
    })
}

/// Sync path: the model's local automerge doc brought current with its
/// snapshot — the hydrated value, doc bytes, and currentness marker the
/// doc reflects. `None` when no snapshot exists yet.
pub async fn live_doc<T: CrdtModel>(key: &[u8]) -> Result<Option<LiveDoc<T>>> {
    let table = Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS).await?;
    match load_row_state::<T>(&table, key).await? {
        RowState::Unsnapshotted => Ok(None),
        RowState::Current {
            snapshot,
            doc,
            marker,
        } => Ok(Some(LiveDoc {
            value: decode_snapshot::<T>(&snapshot)?,
            doc,
            marker,
        })),
        RowState::Stale {
            snapshot,
            doc,
            marker,
        } => Ok(Some(
            reconcile_row_doc::<T>(&table, key, snapshot, doc, marker).await?,
        )),
    }
}

/// A dual-format row as read for a doc load, classified by whether its
/// automerge doc is current with its snapshot.
enum RowState {
    /// No snapshot stored: no local state for a doc to be current with.
    Unsnapshotted,
    /// Stored doc current with the snapshot, whose hash is `marker`.
    Current {
        /// Snapshot bytes, decode them for the value.
        snapshot: Bytes,
        /// Doc bytes, as [`LiveDoc`] carries them.
        doc: Vec<u8>,
        marker: Vec<u8>,
    },
    /// Stored doc stale or absent: reconcile it from the snapshot before use.
    Stale {
        snapshot: Bytes,
        /// Retained doc to reconcile into; `None` if the row has none.
        doc: Option<Bytes>,
        marker: Vec<u8>,
    },
}

/// Load a row's dual-format state and classify its doc against the snapshot —
/// the one currentness rule every doc load reads.
///
/// Hashes the snapshot but never decodes it, so a caller that needs no value
/// (the publish fold's concurrency check) pays nothing for one.
async fn load_row_state<T: CrdtModel>(table: &Table, key: &[u8]) -> Result<RowState> {
    let marker_key = automerge_marker_key(key);
    let (snapshot, mut doc, stored_marker) = try_join!(
        table.load_opt(COL_SNAPSHOT, key),
        table.load_opt(COL_AUTOMERGE, key),
        table.load_opt(COL_AUTOMERGE, &marker_key),
    )?;
    let Some(snapshot) = snapshot else {
        return Ok(RowState::Unsnapshotted);
    };

    let marker = currentness_marker(&snapshot)?;
    if stored_marker.as_deref() == Some(marker.as_slice())
        && let Some(doc) = doc.take()
    {
        return Ok(RowState::Current {
            snapshot,
            doc: doc.into(),
            marker,
        });
    }
    Ok(RowState::Stale {
        snapshot,
        doc,
        marker,
    })
}

/// Reconcile a stale or absent doc current with its snapshot: hydrate the
/// snapshot's value, reconcile it into the retained doc, re-mark the row, and
/// compact past the watermark. `marker` is that snapshot's hash.
async fn reconcile_row_doc<T: CrdtModel>(
    table: &Table,
    key: &[u8],
    snapshot: Bytes,
    existing_doc: Option<Bytes>,
    marker: Vec<u8>,
) -> Result<LiveDoc<T>> {
    let current_value = decode_snapshot::<T>(&snapshot)?;
    let (doc_bytes, raw_len, current_value) = spawn_blocking(move || {
        let (doc_bytes, raw_len) =
            reconcile_to_automerge_measured(existing_doc.as_deref(), &current_value)?;
        Ok::<_, Error>((doc_bytes, raw_len, current_value))
    })
    .await??;

    table
        .apply_batch(
            &[],
            &[
                (COL_AUTOMERGE, key, &doc_bytes),
                (COL_AUTOMERGE, &automerge_marker_key(key), &marker),
            ],
        )
        .await?;
    maybe_spawn_compaction::<T>(table, key, &snapshot, &doc_bytes, raw_len, marker.clone()).await;

    Ok(LiveDoc {
        value: current_value,
        doc: doc_bytes,
        marker,
    })
}

/// Commits a merged dual-format row — snapshot, doc, and that snapshot's
/// currentness marker — as one batch, on the already-open table.
///
/// [`store_merged_state`] re-opens the table; this variant exists for callers
/// that already hold it (the remote-merge paths), so a pull does not pay a
/// second open per write.
async fn store_merged_row<T: CrdtModel>(
    table: &Table,
    key: &[u8],
    doc: &[u8],
    value: &T,
) -> Result<Vec<u8>> {
    let snapshot = value.pack()?;
    let marker = currentness_marker(&snapshot)?;
    table
        .apply_batch(
            &[],
            &[
                (COL_SNAPSHOT, key, &snapshot),
                (COL_AUTOMERGE, key, doc),
                (COL_AUTOMERGE, &automerge_marker_key(key), &marker),
            ],
        )
        .await?;
    notify_model_changed::<T>();
    Ok(snapshot)
}

/// Compaction trigger, shared by every path that rewrites a dual-format row:
/// over the threshold *and* grown past the last compacted length by at least a
/// threshold. Both conditions are measured on the doc's RAW (uncompressed)
/// serialization length — the unit `DEFAULT_COMPACTION_THRESHOLD` and the
/// watermark have always meant — even though the row stores the DEFLATE form,
/// so a completed compaction does not lower this measurement. Without the
/// watermark every mutation→publish would recompact the whole doc.
async fn maybe_spawn_compaction<T: CrdtModel>(
    table: &Table,
    key: &[u8],
    snapshot: &Bytes,
    doc_bytes: &[u8],
    raw_len: usize,
    marker: Vec<u8>,
) {
    if raw_len <= DEFAULT_COMPACTION_THRESHOLD
        || raw_len
            <= load_compaction_watermark(table, key)
                .await
                .unwrap_or(0)
                .saturating_add(DEFAULT_COMPACTION_THRESHOLD)
    {
        return;
    }
    let compaction_key = key.to_vec();
    let doc = doc_bytes.to_vec();
    let snapshot = snapshot.clone();
    spawn_report(
        "sync.store_trait",
        "Failed to compact CRDT snapshot/doc pair",
        async move {
            snapshots::maybe_compact::<T>(
                &compaction_key,
                &doc,
                raw_len,
                &snapshot,
                &marker,
                DEFAULT_COMPACTION_THRESHOLD,
            )
            .await
            .map(|_| ())
        },
    );
}

/// The model's local automerge doc brought current with its snapshot, under
/// the per-key store lock — for a merged DHT publish. `None` when no local
/// snapshot exists yet.
pub async fn locked_live_doc<T: CrdtModel>(key: &[u8]) -> Result<Option<LiveDoc<T>>> {
    let _guard = lock_store(key).await;
    live_doc::<T>(key).await
}

/// Incorporates a published document without losing remote additions or
/// concurrent local edits. Deleted or wholesale-replaced bases are skipped.
///
/// The optimistic-concurrency check, under the row's store lock: the row's doc
/// must still be the one `based_on` was loaded with, and current with the row's
/// snapshot — so a row replaced while the DHT write was in flight is never
/// overwritten. Only bytes are compared: the snapshot is hashed, not decoded,
/// on this unchanged-row path, where the value [`live_doc`] would decode is the
/// one comparison discards.
///
/// Returns the local state as of the fold: what [`load_local`] under the same
/// lock would report, so a publish needs no second load to hydrate from.
pub async fn persist_written_doc<'a, T: CrdtModel>(
    key: &[u8],
    written: &[u8],
    based_on: &'a LiveDoc<T>,
) -> Result<Cow<'a, T>> {
    let _guard = lock_store(key).await;
    let table = Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS).await?;
    let (current_doc_bytes, current_marker) = match load_row_state::<T>(&table, key).await? {
        RowState::Unsnapshotted => {
            // Snapshot gone (row deleted mid-publish): a load reports the
            // default.
            return Ok(Cow::Owned(T::default()));
        }
        RowState::Current { doc, marker, .. } => (doc, marker),
        RowState::Stale {
            snapshot,
            doc,
            marker,
        } => {
            // Stale or missing doc: reconcile and re-mark the row first,
            // exactly as a plain load would, before folding into it.
            let live = reconcile_row_doc::<T>(&table, key, snapshot, doc, marker).await?;
            (live.doc, live.marker)
        }
    };
    if current_marker == based_on.marker && current_doc_bytes == based_on.doc {
        if written == based_on.doc.as_slice() {
            // The write produced the row's own doc: nothing to fold, and
            // `based_on`'s value is still what a load reports.
            return Ok(Cow::Borrowed(&based_on.value));
        }
        let written_bytes = Bytes::copy_from_slice(written);
        let value = from_automerge_async::<T>(written_bytes.clone()).await?;
        let snapshot = store_merged_state::<T>(key, &written_bytes, &value).await?;
        return Ok(Cow::Owned(decode_snapshot::<T>(&snapshot)?));
    }

    let written_vec = written.to_vec();
    let base_doc_bytes = based_on.doc.clone();
    let Some((merged, value)) = spawn_blocking(move || -> Result<Option<(Vec<u8>, T)>> {
        let mut current_doc = AutoCommit::load(&current_doc_bytes)?;
        let mut base_doc = AutoCommit::load(&base_doc_bytes)?;
        if !current_doc
            .get_missing_deps(&base_doc.get_heads())
            .is_empty()
        {
            return Ok(None);
        }
        let mut written_doc = AutoCommit::load(&written_vec)?;
        current_doc.merge(&mut written_doc)?;
        let value: T = hydrate(&current_doc).map_err(AutomergeSyncError::SchemaValidation)?;
        Ok(Some((current_doc.save(), value)))
    })
    .await??
    else {
        // The row's doc does not hold `based_on`'s heads: it was replaced under
        // the publish. Fold nothing and report the row as it stands.
        return Ok(Cow::Owned(load_local::<T>(key).await?));
    };
    let snapshot = store_merged_state::<T>(key, &merged, &value).await?;
    Ok(Cow::Owned(decode_snapshot::<T>(&snapshot)?))
}

/// Commit a merged doc/snapshot pair, returning the snapshot bytes written.
/// Their decode — not `value` — is what a following [`load_local`] reports,
/// because the codec normalizes on unpack (an empty theme reads back as
/// `"system"`), so a caller that needs the local value decodes them.
async fn store_merged_state<T: CrdtModel>(key: &[u8], doc: &[u8], value: &T) -> Result<Vec<u8>> {
    let snapshot = value.pack()?;
    let marker = currentness_marker(&snapshot)?;
    Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS)
        .await?
        .apply_batch(
            &[],
            &[
                (COL_SNAPSHOT, key, &snapshot),
                (COL_AUTOMERGE, key, doc),
                (COL_AUTOMERGE, &automerge_marker_key(key), &marker),
            ],
        )
        .await?;
    notify_model_changed::<T>();
    Ok(snapshot)
}

/// Decode a model's local snapshot, accepting a schema version **older** than
/// the running one.
///
/// Older snapshots decode anyway, deliberately: Cap'n Proto fills fields added
/// since with schema defaults, so the value this build reads is the value that
/// was written, and every write re-stamps the row at `T::SCHEMA_VERSION`. The
/// alternative is what this looks like with exact matching — contacts, the
/// conversation index, TOTP, and app state become unreadable with no migration
/// and no way back, because reads and `sync::mutate` both start here and a
/// refused read is never rewritten, while the only remedy the UI can offer is
/// clearing app data (which destroys the password-encrypted keypair).
///
/// Newer snapshots are still refused: this build cannot see their fields, and
/// [`unpack_versioned`] reports that to the caller rather than substituting
/// defaults for data it does not understand.
pub fn decode_snapshot<T: CrdtModel>(snapshot_bytes: &[u8]) -> Result<T> {
    match unpack_versioned(
        T::DISPLAY_NAME,
        T::SCHEMA_VERSION,
        snapshot_bytes,
        T::unpack,
    ) {
        Ok((value, _version)) => Ok(value),
        // Guaranteed to succeed: `unpack_versioned` just decoded these same
        // bytes, and only the version check rejected them.
        Err(e)
            if e.downcast_ref::<SchemaVersionError>()
                .is_some_and(SchemaVersionError::is_older) =>
        {
            Ok(T::unpack(snapshot_bytes)?.0)
        }
        Err(e) => Err(e),
    }
}

fn currentness_marker(snapshot: &[u8]) -> Result<Vec<u8>> {
    Ok(Vec::from(crypto_system_sync()?.generate_hash(snapshot)))
}
