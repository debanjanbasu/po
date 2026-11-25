//! Recompression manager for migrating old data to current dictionary versions.
//!
//! When dictionaries are updated, old data compressed with previous versions
//! needs to be migrated. This module:
//! - Recounts remaining old-version rows from local `TableDB`
//! - Recompresses structured rows reactively when dictionaries are swapped
//! - Retains historical dictionary blobs locally until end-to-end cleanup is safe
//!
//! A dictionary activation forces a scan; the hourly fallback skips the scan
//! while the active identity is still the one the last completed scan ran under
//! ([`SCANNED_DICTIONARY_KEY`]).

use anyhow::{Error, Result};
use bytes::Bytes;
use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::{select, sync::Notify};

use crate::{
    chat::{contact::ContactList, conversation::ConversationIndex},
    compression::training_queue,
    config::{self, storage::DEVICE_TABLE},
    helpers::{async_utils::sleep_ms, concurrency, spawn_blocking},
    models::{app_state::AppState, totp::TotpTokenList},
    nonfatal,
    session::epoch::{current_epoch, spawn_session_scoped},
    storage::{
        background_errors::report_nonfatal,
        store_trait::{DUAL_FORMAT_TABLE_COLS, IDENTITY_DOC_TABLE, SyncSurface},
        upgrade_status::{UpgradeStatusToken, increment_upgrade_progress, set_upgrade_status},
    },
    sync::pending_ops::{PENDING_OPS_COLS, PENDING_OPS_TABLE},
    user::store::{CLEANUP_TABLE, USER_TABLE, USER_TABLE_COLS},
    veilid::table_store::{self, Table},
};

use super::codec::{self, DictionaryId, frame_version, load_frame_dictionary};

/// Notify to wake recompression when dictionaries are swapped.
static RECOMPRESS_NOTIFY: Notify = Notify::const_new();
static RECOMPRESSOR_EPOCH_STARTED: AtomicU64 = AtomicU64::new(u64::MAX);

/// Dictionary activations — the primary trigger. `notify_recompress` bumps this,
/// and a cycle may only skip its scan while the generation still matches the one
/// the last completed scan observed. In-memory only; across a restart the
/// answer comes from [`SCANNED_DICTIONARY_KEY`] alone.
static RECOMPRESS_GENERATION: AtomicU64 = AtomicU64::new(0);
static SCANNED_GENERATION: AtomicU64 = AtomicU64::new(0);

/// `po_dicts` key naming the dictionary identity the last completed full scan ran
/// under: beside the blobs it refers to, and outside the scanned tables.
const SCANNED_DICTIONARY_KEY: &[u8] = b"scanned_dictionary";

const SESSION_TABLE: &str = "po_session";
const SNAPSHOTS_TABLE: &str = "po_snapshots";
const PENDING_INVITE_TABLE: &str = "po_pending_invites";
const FAILED_HYDRATION_TABLE: &str = "po_failed_hydrations";

#[derive(Clone, Copy)]
struct TableSpec {
    name: &'static str,
    columns: u32,
}

const STRUCTURED_TABLES: [TableSpec; 14] = [
    TableSpec {
        name: IDENTITY_DOC_TABLE,
        columns: 1,
    },
    TableSpec {
        name: training_queue::SAMPLES_TABLE,
        columns: training_queue::SAMPLES_TABLE_COLS,
    },
    TableSpec {
        name: USER_TABLE,
        columns: USER_TABLE_COLS,
    },
    TableSpec {
        name: SESSION_TABLE,
        columns: 1,
    },
    TableSpec {
        name: DEVICE_TABLE,
        columns: 1,
    },
    TableSpec {
        name: PENDING_OPS_TABLE,
        columns: PENDING_OPS_COLS,
    },
    TableSpec {
        name: SNAPSHOTS_TABLE,
        columns: 2,
    },
    TableSpec {
        name: PENDING_INVITE_TABLE,
        columns: 1,
    },
    TableSpec {
        name: FAILED_HYDRATION_TABLE,
        columns: 1,
    },
    TableSpec {
        name: AppState::TABLE_NAME,
        columns: DUAL_FORMAT_TABLE_COLS,
    },
    TableSpec {
        name: TotpTokenList::TABLE_NAME,
        columns: DUAL_FORMAT_TABLE_COLS,
    },
    TableSpec {
        name: ContactList::TABLE_NAME,
        columns: DUAL_FORMAT_TABLE_COLS,
    },
    TableSpec {
        name: ConversationIndex::TABLE_NAME,
        columns: DUAL_FORMAT_TABLE_COLS,
    },
    // Owned by `user::store::cleanup`, which stores through the compressed
    // `Table` API like every other structured row, so its rows carry dictionary
    // frames too. Named by constant, not literal, so a rename cannot silently
    // drop it from this scan and leave rows outliving their dictionary blob.
    TableSpec {
        name: CLEANUP_TABLE,
        columns: 1,
    },
];

/// Notify the recompression manager that a dictionary swap occurred.
///
/// Always forces the next cycle to scan, even if the active identity happens to
/// match the last scanned one again (a swap chain that returns to it).
pub fn notify_recompress() {
    RECOMPRESS_GENERATION.fetch_add(1, Ordering::SeqCst);
    RECOMPRESS_NOTIFY.notify_one();
}

// ── Background recompression task ─────────────────────────────────────────────

/// Spawn the background recompression task, scoped to the current session.
/// Wakes reactively when dictionaries are swapped, with a 1-hour fallback timer
/// that stays a no-op while the active dictionary is the one already scanned.
pub fn spawn_recompressor() {
    let epoch = current_epoch();
    if RECOMPRESSOR_EPOCH_STARTED.swap(epoch, Ordering::SeqCst) == epoch {
        return;
    }
    // Swaps from before this session (the startup dictionary load, or one while
    // logged out) already show up as an identity change; only later ones force.
    SCANNED_GENERATION.store(
        RECOMPRESS_GENERATION.load(Ordering::SeqCst),
        Ordering::SeqCst,
    );
    spawn_session_scoped(async move {
        // The idle cadence is the fallback, not the only one: a cycle that
        // deferred itself (see `defer_heavy_work`) returns its own delay, and
        // without that a deferred cycle would wait another hour — which on a tab
        // the user keeps open and visible means never.
        let mut delay_ms = config::compression::RECOMPRESSION_INTERVAL_MS;
        loop {
            select! {
                () = RECOMPRESS_NOTIFY.notified() => {}
                () = sleep_ms(delay_ms) => {}
            }
            delay_ms = match run_recompression_cycle().await {
                Ok(outcome) => outcome.next_delay_ms(),
                Err(error) => {
                    nonfatal!(
                        "compression.recompression",
                        "Recompression cycle failed",
                        Err::<(), Error>(error)
                    );
                    config::compression::RECOMPRESSION_INTERVAL_MS
                }
            };
        }
    });
}

/// A stale row identified by `(column, key)` — deliberately **without** its
/// raw bytes. The scan reads every structured row to decide staleness, so
/// buffering the bytes here would hold the whole table's stale payload in
/// memory for the entire migration (a rotation typically touches thousands of
/// rows); `migrate_table` reloads the one row it works on, and
/// `compare_and_swap_raw` re-reads it under the row lock anyway.
type StaleRow = (u32, Bytes);

/// Why a cycle stopped — which is also how soon the next one should run.
///
/// A row that cannot be recompressed does *not* produce [`CycleOutcome::Deferred`]:
/// its retry stays on the idle cadence, as it always has, because nothing about
/// it changes by being tried a minute later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleOutcome {
    /// Nothing to do, or everything found was migrated.
    Done,
    /// Work is pending and was not done now (the page is visible). The scan
    /// marker is untouched, so the next cycle starts where this one stopped.
    Deferred,
}

impl CycleOutcome {
    /// The delay until the next cycle. Idle for [`Self::Done`]; the short resume
    /// timer for [`Self::Deferred`], so the pass lands on the first hidden
    /// moment rather than an hour of them.
    const fn next_delay_ms(self) -> u64 {
        match self {
            Self::Done => config::compression::RECOMPRESSION_INTERVAL_MS,
            Self::Deferred => config::compression::DEFERRED_RESUME_MS,
        }
    }
}

/// Run one cycle of recompression.
///
/// The scan — one key listing per column of every structured table, plus a row
/// read per key — is skipped when no dictionary activation happened since the
/// last one it completed and the active identity is still the scanned one.
async fn run_recompression_cycle() -> Result<CycleOutcome> {
    let generation = RECOMPRESS_GENERATION.load(Ordering::SeqCst);
    let current_version = codec::current_version();
    if generation == SCANNED_GENERATION.load(Ordering::SeqCst)
        && scan_marker_matches(current_version).await
    {
        // The no-op path stays cheap and stays on the idle cadence: this is the
        // hourly wake of a device whose dictionary has not moved.
        return Ok(CycleOutcome::Done);
    }

    // Everything below is the expensive half — a key listing and a row read for
    // every column of 14 tables, then a decompress and recompress per stale row,
    // all inline on the single thread web has. Do it while the page is hidden.
    // Nothing is recorded on the way out, so the work stays pending and
    // `next_delay_ms` brings the cycle back within the minute.
    if crate::platform::lifecycle::defer_heavy_work() {
        return Ok(CycleOutcome::Deferred);
    }

    let mut table_stale_rows: Vec<(Table, Vec<StaleRow>)> =
        Vec::with_capacity(STRUCTURED_TABLES.len());
    let mut old_count = 0u64;
    // Every frame version the scan sees, referenced or not: this is what later
    // proves a dictionary blob unreferenced (see the prune below).
    let mut observed_versions = HashSet::new();

    for table in STRUCTURED_TABLES {
        let (handle, stale_rows) =
            collect_old_rows_in_table(table, current_version, &mut observed_versions).await?;
        old_count = old_count.saturating_add(stale_rows.len() as u64);
        if !stale_rows.is_empty() {
            table_stale_rows.push((handle, stale_rows));
        }
    }

    let mut complete = true;
    if old_count > 0 {
        let status_run = set_upgrade_status(old_count);
        let status_token = status_run.token();
        for (handle, stale_rows) in table_stale_rows {
            if !migrate_table(handle, stale_rows, status_token).await {
                complete = false;
            }
        }
    }

    // Only a scan that left nothing behind records the marker: a row whose old
    // dictionary is unrecoverable must keep the fallback retrying it.
    if complete {
        table_store::store_dict(SCANNED_DICTIONARY_KEY, current_version.to_string()).await?;
        SCANNED_GENERATION.store(generation, Ordering::SeqCst);
        // Nothing local references the other versions any more, so their blobs
        // are dead weight this device never reclaims otherwise. The trade this
        // makes: a *remote* copy still framed with a pruned version — a subkey
        // an older build wrote with a dictionary and nobody has republished
        // since — becomes unreadable here until its owner republishes with the
        // dictionary-free frame every current build writes. The registry's
        // live-version set and the embedded dictionary stay kept, so this only
        // reaches a version no local row uses and no recent codec path needs.
        if let Err(error) = codec::prune_unreferenced_dicts(&observed_versions).await {
            nonfatal!(
                "compression.recompression",
                "Failed to prune unreferenced dictionary blobs",
                Err::<(), Error>(error)
            );
        }
    }
    // `complete == false` (a row whose old dictionary is unrecoverable) keeps the
    // idle cadence, not the resume timer: retrying it a minute later is one more
    // scan for the same answer, which is what the marker is for.
    Ok(CycleOutcome::Done)
}

/// Whether the last completed scan already covered `current_version`.
///
/// An unreadable marker forces a scan rather than skipping one.
async fn scan_marker_matches(current_version: DictionaryId) -> bool {
    match completed_scan_identity().await {
        Ok(marked) => marked == Some(current_version),
        Err(error) => {
            nonfatal!(
                "compression.recompression",
                "Cannot read the last-scan marker; scanning",
                Err::<(), Error>(error)
            );
            false
        }
    }
}

/// Dictionary identity recorded by the last completed scan, if any.
async fn completed_scan_identity() -> Result<Option<DictionaryId>> {
    let Some(stored) = table_store::load_dict_opt(SCANNED_DICTIONARY_KEY).await? else {
        return Ok(None);
    };
    Ok(Some(String::from_utf8(stored)?.trim().parse()?))
}

// ── TableStore recompression ─────────────────────────────────────────────────

async fn collect_old_rows_in_table(
    table: TableSpec,
    current_version: DictionaryId,
    observed: &mut HashSet<DictionaryId>,
) -> Result<(Table, Vec<StaleRow>)> {
    let handle = Table::open(table.name, table.columns).await?;
    let db = handle.raw();
    let mut all_keys = Vec::new();

    for col in 0..table.columns {
        for key in db.get_keys(col).await? {
            all_keys.push((col, key));
        }
    }

    // Unbounded scan: each row is read, classified and dropped in its own
    // task, so the resident cost stays one row per in-flight task instead of
    // a permit-pool wave. The scan already yields per row on wasm.
    let results = concurrency::map_concurrent(all_keys, |(col, key): (u32, bytes::Bytes)| {
        let db = Arc::clone(&db);
        async move {
            // The raw bytes are read, classified, and dropped here — only
            // the key survives the scan — so the scan's resident cost is
            // one row at a time per concurrent task rather than the whole
            // table's stale payload.
            let Some(raw) = db.load(col, key.clone()).await? else {
                return Ok::<Option<(Option<DictionaryId>, Option<StaleRow>)>, Error>(None);
            };
            let version = match frame_version(&raw) {
                Ok(version) => version,
                Err(error) => {
                    nonfatal!(
                        "compression.recompression",
                        "Cannot count a row with malformed compression framing",
                        Err::<(), Error>(error)
                    );
                    return Ok(None);
                }
            };
            drop(raw);
            let stale = (version != current_version).then_some((col, key));
            Ok(Some((Some(version), stale)))
        }
    })
    .await;

    let mut stale_rows = Vec::new();
    for result in results {
        if let Some((version, entry)) = result? {
            if let Some(version) = version {
                observed.insert(version);
            }
            if let Some(entry) = entry {
                stale_rows.push(entry);
            }
        }
    }

    Ok((handle, stale_rows))
}

/// Migrate a single table's stale rows to the current dictionary version.
///
/// Uses unbounded parallel recompression to overlap I/O and
/// CPU work across rows. Each row still acquires its own lock for safe CAS.
/// Returns `true` when every stale row was migrated.
async fn migrate_table(
    handle: Table,
    stale_rows: Vec<StaleRow>,
    status_token: UpgradeStatusToken,
) -> bool {
    // Unbounded over rows: each row re-reads under its own lock and CASes, so
    // there is no shared contention to throttle — the fan-out just stops
    // paying a permit wave per batch of rows.
    let results = concurrency::map_concurrent(stale_rows, |(col, key): (u32, bytes::Bytes)| {
        let handle = handle.clone();
        async move {
            // Web decompresses and recompresses this row inline on the one
            // thread there is, so hand the browser a macrotask between rows:
            // the cycle is gated to a hidden page, but a page can become
            // visible mid-cycle, and a burst with no await in it would hold
            // the first paint of that page off for the length of the burst.
            #[cfg(target_arch = "wasm32")]
            sleep_ms(0).await;

            // Reloaded here rather than carried from the scan: the row may
            // have changed (or gone) between the two passes, and this read
            // is what `compare_and_swap_raw` re-checks under the row lock.
            let Some(raw) = handle.load_opt(col, &key).await? else {
                return Ok::<(), Error>(());
            };

            let Some(recompressed) = maybe_recompress_raw(raw.clone()).await? else {
                return Ok(());
            };

            if handle
                .compare_and_swap_raw(col, &key, &raw, recompressed)
                .await?
            {
                increment_upgrade_progress(status_token, 1);
            }
            Ok(())
        }
    })
    .await;

    // A row that can't be recompressed (e.g. its old dictionary is
    // unrecoverable) stays on its old version — skipping it keeps one
    // corrupt row from aborting the whole migration every cycle.
    let mut failed = false;
    for result in results {
        if let Err(error) = result {
            failed = true;
            report_nonfatal(
                "compression.recompression",
                "Failed to recompress row; leaving it on the old dictionary",
                error,
            );
        }
    }
    !failed
}

/// Recompress a single raw data blob if it uses an old dictionary.
///
/// Takes `data` by value so the row bytes move straight into the blocking
/// closure — no defensive copy. A single `spawn_blocking` call decompresses
/// with the old dict and recompresses with the current one.
async fn maybe_recompress_raw(data: Bytes) -> Result<Option<Vec<u8>>> {
    let version = frame_version(&data)?;
    let current = codec::current_version();
    if version == current {
        return Ok(None);
    }

    let dict = load_frame_dictionary(&data).await?;

    let recompressed = spawn_blocking(move || {
        let decompressed = codec::decompress_structured_with_dictionary(
            &data,
            dict.as_ref().map(|(id, prepared)| (*id, prepared.as_ref())),
        )?;
        codec::compress_structured_blocking(&decompressed)
    })
    .await??;

    Ok(Some(recompressed))
}
