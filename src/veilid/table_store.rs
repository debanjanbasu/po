//! Thin wrappers around Veilid's `TableDB` for key-value persistence.
//! All values are zstd-compressed transparently on write and decompressed on read.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, RwLock},
};

use anyhow::{Context, Error, Result};
use bytes::Bytes;
use futures_util::future::try_join_all;
use tokio::sync::OwnedMutexGuard;
use veilid_core::TableDB;

use crate::{
    compression::codec as compression,
    helpers::store_lock::{lock_store, recover_poison},
    veilid::core::ensure_local_api,
};

type TableCacheKey = (&'static str, u32);
type TableCacheMap = HashMap<TableCacheKey, Arc<TableDB>>;

static TABLE_CACHE: LazyLock<RwLock<TableCacheMap>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Table name for dictionary storage.
const DICTS_TABLE: &str = "po_dicts";

/// Column count for the dictionaries table.
const DICTS_COLS: u32 = 1;

const TABLE_ROW_LOCK_PREFIX: &[u8] = b"\0oo.table-row\0";

// ── Table ────────────────────────────────────────────────────────────────────

/// A typed handle to a `TableDB` table that captures table name and column count,
/// eliminating the need to repeat these parameters on every operation.
#[derive(Clone)]
pub struct Table {
    name: &'static str,
    db: Arc<TableDB>,
}

fn table_row_lock_key(table_name: &str, column: u32, key: &[u8]) -> Vec<u8> {
    let mut lock_key =
        Vec::with_capacity(TABLE_ROW_LOCK_PREFIX.len() + table_name.len() + 1 + 4 + key.len());
    lock_key.extend_from_slice(TABLE_ROW_LOCK_PREFIX);
    lock_key.extend_from_slice(table_name.as_bytes());
    lock_key.push(0);
    lock_key.extend_from_slice(&column.to_le_bytes());
    lock_key.extend_from_slice(key);
    lock_key
}

async fn lock_row(table_name: &str, column: u32, key: &[u8]) -> OwnedMutexGuard<()> {
    lock_store(&table_row_lock_key(table_name, column, key)).await
}

impl Table {
    pub async fn open(table_name: &'static str, column_count: u32) -> Result<Self> {
        let db = open_table(table_name, column_count).await?;
        Ok(Self {
            name: table_name,
            db,
        })
    }

    /// Write a compressed value under `key` in the specified column.
    pub async fn store(&self, column: u32, key: &[u8], value: &[u8]) -> Result<()> {
        let compressed = compression::compress_structured(Bytes::copy_from_slice(value)).await?;
        let _guard = lock_row(self.name, column, key).await;
        self.db
            .store(column, Bytes::copy_from_slice(key), compressed)
            .await
            .context("failed to store value")?;
        Ok(())
    }

    /// Store a compressed value only when the row does not exist.
    ///
    /// Compression happens before locking; the existence check and write are
    /// serialized with every other mutation of this row.
    pub async fn store_if_absent(&self, column: u32, key: &[u8], value: &[u8]) -> Result<bool> {
        let compressed = compression::compress_structured(Bytes::copy_from_slice(value)).await?;
        let _guard = lock_row(self.name, column, key).await;
        let key_bytes = Bytes::copy_from_slice(key);
        if self
            .db
            .load(column, key_bytes.clone())
            .await
            .context("failed to load value")?
            .is_some()
        {
            return Ok(false);
        }
        self.db
            .store(column, key_bytes, compressed)
            .await
            .context("failed to store value")?;
        Ok(true)
    }

    /// Load a compressed value by key. Returns `None` if the key does not exist.
    pub async fn load_opt(&self, column: u32, key: &[u8]) -> Result<Option<Bytes>> {
        let Some(compressed) = self
            .db
            .load(column, Bytes::copy_from_slice(key))
            .await
            .context("failed to load value")?
        else {
            return Ok(None);
        };
        let decompressed = compression::decompress_structured(compressed).await?;
        Ok(Some(decompressed))
    }

    /// Delete a value in the specified column.
    pub async fn delete(&self, column: u32, key: &[u8]) -> Result<()> {
        let _guard = lock_row(self.name, column, key).await;
        self.db
            .delete(column, Bytes::copy_from_slice(key))
            .await
            .context("failed to delete value")?;
        Ok(())
    }

    /// Delete only if the row still decodes to `expected`, serialized with mutations.
    pub(crate) async fn delete_if_matches(
        &self,
        column: u32,
        key: &[u8],
        expected: &[u8],
    ) -> Result<bool> {
        let _guard = lock_row(self.name, column, key).await;
        let key_bytes = Bytes::copy_from_slice(key);
        let Some(compressed) = self
            .db
            .load(column, key_bytes.clone())
            .await
            .context("failed to re-read value before conditional delete")?
        else {
            return Ok(false);
        };
        if compression::decompress_structured(compressed)
            .await?
            .as_ref()
            != expected
        {
            return Ok(false);
        }
        self.db
            .delete(column, key_bytes)
            .await
            .context("failed to delete matching value")?;
        Ok(true)
    }

    /// Replace an unchanged raw row without racing compressed stores, deletes,
    /// or batches. Compression must finish before acquiring the row lock.
    pub(crate) async fn compare_and_swap_raw(
        &self,
        column: u32,
        key: &[u8],
        expected: &[u8],
        replacement: impl Into<Bytes>,
    ) -> Result<bool> {
        let _guard = lock_row(self.name, column, key).await;
        let key_bytes = Bytes::copy_from_slice(key);
        if self
            .db
            .load(column, key_bytes.clone())
            .await
            .context("failed to re-read row before recompression")?
            .as_deref()
            != Some(expected)
        {
            return Ok(false);
        }
        self.db
            .store(column, key_bytes, replacement.into())
            .await
            .context("failed to replace recompressed row")?;
        Ok(true)
    }

    /// Atomically apply deletes then compressed writes in one transaction.
    ///
    /// `deletes` are `(column, key)`; `stores` are `(column, key, value)` and
    /// are compressed like [`Self::store`]. Ops buffer in order (a delete
    /// followed by a store of the same key ends up stored) and all commit
    /// together or not at all — used by backup restore so a replace can never
    /// leave the store half-wiped.
    pub async fn apply_batch(
        &self,
        deletes: &[(u32, &[u8])],
        stores: &[(u32, &[u8], &[u8])],
    ) -> Result<()> {
        if deletes.is_empty() && stores.is_empty() {
            return Ok(());
        }

        // Pre-compress all values before starting the transaction.
        let compressed_entries: Vec<(u32, &[u8], Bytes)> =
            try_join_all(stores.iter().map(|&(col, key, value)| async move {
                let compressed =
                    compression::compress_structured(Bytes::copy_from_slice(value)).await?;
                Ok::<_, Error>((col, key, compressed))
            }))
            .await?;

        // Acquire row locks in deterministic order to prevent deadlocks.
        let mut lock_keys: Vec<Vec<u8>> = deletes
            .iter()
            .map(|&(col, key)| table_row_lock_key(self.name, col, key))
            .chain(
                stores
                    .iter()
                    .map(|&(col, key, _)| table_row_lock_key(self.name, col, key)),
            )
            .collect();
        lock_keys.sort();
        lock_keys.dedup();
        let mut guards: Vec<OwnedMutexGuard<()>> = Vec::with_capacity(lock_keys.len());
        for lk in &lock_keys {
            guards.push(lock_store(lk).await);
        }

        // Execute as a single atomic transaction: deletes first, then writes.
        let txn = self.db.transact();
        for &(col, key) in deletes {
            txn.delete(col, Bytes::copy_from_slice(key))
                .await
                .context("batch delete op failed")?;
        }
        for (col, key, compressed) in compressed_entries {
            txn.store(col, Bytes::copy_from_slice(key), compressed)
                .await
                .context("batch store op failed")?;
        }
        txn.commit().await.context("batch commit failed")?;
        drop(guards); // Release row locks after atomic commit.
        Ok(())
    }

    /// Open a table and return the raw `TableDB` handle (for recompression).
    /// Callers handle compression/decompression themselves.
    #[must_use]
    pub fn raw(&self) -> Arc<TableDB> {
        self.db.clone()
    }
}

pub async fn open_table(table_name: &'static str, column_count: u32) -> Result<Arc<TableDB>> {
    let cache_key = (table_name, column_count);

    let cached_db = recover_poison(TABLE_CACHE.read()).get(&cache_key).cloned();
    if let Some(db) = cached_db {
        return Ok(db);
    }

    let api = ensure_local_api().await?;
    let db = api
        .open_table_db(table_name, column_count)
        .await
        .context("failed to open table")?;
    let db = Arc::new(db);
    Ok(recover_poison(TABLE_CACHE.write())
        .entry(cache_key)
        .or_insert(db)
        .clone())
}

// ── Dictionary blob storage (plain zstd, no dictionary) ──────────────────────

/// Open the dictionaries table.
async fn open_dicts() -> Result<Arc<TableDB>> {
    open_table(DICTS_TABLE, DICTS_COLS).await
}

/// Store a dictionary blob compressed with plain zstd (no trained dictionary).
/// This avoids circular dependency — dict blobs don't need themselves to decompress.
pub async fn store_dict(key: &[u8], value: impl Into<Bytes>) -> Result<()> {
    let compressed = compression::compress_plain(value.into()).await?;
    let db = open_dicts().await?;
    let _guard = lock_row(DICTS_TABLE, 0, key).await;
    db.store(0, Bytes::copy_from_slice(key), compressed)
        .await
        .context("failed to store dict")
}

/// Load a dictionary blob, decompressed with plain zstd.
pub async fn load_dict_opt(key: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(stored) = open_dicts()
        .await?
        .load(0, Bytes::copy_from_slice(key))
        .await
        .context("failed to load dict")?
    else {
        return Ok(None);
    };

    let decoded = compression::decompress_plain(stored).await?;
    Ok(Some(decoded.into()))
}

/// Prefix of the per-version dictionary blob keys (`structured.v{identity}`).
/// The table's other keys (`active_dictionary`, `scanned_dictionary`) are
/// single markers and are never candidates for deletion.
const DICT_VERSION_KEY_PREFIX: &[u8] = b"structured.v";

/// Deletes stored dictionary blobs whose version is not named in `keep`.
///
/// Every dictionary this device has ever trained or adopted otherwise keeps
/// its blob here forever — `store_dict` has no counterpart, and account
/// cleanup does not open this table — so an unreferenced version is a row no
/// reader can ever reach again. Versions are content-addressed, so only a
/// genuinely new dictionary adds a row (16 KiB trained, up to the 32 KiB DHT
/// subkey limit for an adopted one).
///
/// `keep` must name every version a structured row on this device can still be
/// framed with, plus the active one; [`crate::compression::codec::registry`]
/// derives that set from the recompression scan and holds it under the
/// dictionary-update lock, which is why this function takes plain version
/// strings and no lock of its own.
///
/// Returns the number of blobs removed.
pub async fn prune_dict_versions(keep: &[String]) -> Result<usize> {
    let table = Table::open(DICTS_TABLE, DICTS_COLS).await?;
    let mut deletes: Vec<(u32, Bytes)> = Vec::new();
    for key in table.raw().get_keys(0).await? {
        let Some(version) = key.strip_prefix(DICT_VERSION_KEY_PREFIX) else {
            continue;
        };
        if keep.iter().any(|kept| kept.as_bytes() == version) {
            continue;
        }
        deletes.push((0, key));
    }
    if deletes.is_empty() {
        return Ok(0);
    }
    let delete_refs: Vec<(u32, &[u8])> = deletes
        .iter()
        .map(|(column, key)| (*column, key.as_ref()))
        .collect();
    table.apply_batch(&delete_refs, &[]).await?;
    Ok(delete_refs.len())
}
