//! Account-scoped single-column `TableDB` helper.
//!
//! Standardizes the row-key convention for account-scoped tables —
//! `{account_id}\0{suffix}` — and provides whole-account row deletion via
//! prefix scan, so every table built on this helper has a uniform cleanup
//! story for account deletion.

use anyhow::{Error, Result};
use bytes::Bytes;
use futures_util::future::try_join_all;

use crate::veilid::table_store::Table;

const COLS: u32 = 1;
const COL: u32 = 0;

/// Row key: `{account_id}\0{suffix}`. An empty suffix is the account
/// prefix itself, used for prefix scans.
#[must_use]
pub fn account_row_key(account_id: &str, suffix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(account_id.len() + 1 + suffix.len());
    key.extend_from_slice(account_id.as_bytes());
    key.push(0);
    key.extend_from_slice(suffix);
    key
}

/// A table whose rows belong to one account each (key `{account_id}\0{suffix}`).
pub struct AccountTable {
    name: &'static str,
}

/// All keys in `column` that start with `prefix`, as owned `Bytes`.
///
/// The shared prefix scan behind every account-scoped enumeration and
/// deletion in the app.
pub async fn account_rows_in_column(
    raw: &std::sync::Arc<veilid_core::TableDB>,
    column: u32,
    prefix: &[u8],
) -> Result<Vec<Bytes>> {
    Ok(raw
        .get_keys(column)
        .await?
        .into_iter()
        .filter(|key| key.starts_with(prefix))
        .collect())
}

/// Delete every row whose key starts with `{account_id}\0` in **one** column
/// of **any** table, in a single atomic batch.
///
/// For multi-column account tables, call this once per column. It is the
/// shared implementation behind every `delete_all_for_account` in the app, so
/// the prefix-scan + batch-delete logic exists exactly once.
pub async fn delete_account_column(table: &Table, column: u32, account_id: &str) -> Result<()> {
    let prefix = account_row_key(account_id, &[]);
    let keys: Vec<Bytes> = table
        .raw()
        .get_keys(column)
        .await?
        .into_iter()
        .filter(|key| key.starts_with(&prefix))
        .collect();
    let delete_refs: Vec<(u32, &[u8])> = keys.iter().map(|key| (column, key.as_ref())).collect();
    table.apply_batch(&delete_refs, &[]).await
}

impl AccountTable {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }

    pub async fn load(&self, account_id: &str, suffix: &[u8]) -> Result<Option<Bytes>> {
        Table::open(self.name, COLS)
            .await?
            .load_opt(COL, &account_row_key(account_id, suffix))
            .await
    }

    pub async fn store(&self, account_id: &str, suffix: &[u8], value: &[u8]) -> Result<()> {
        Table::open(self.name, COLS)
            .await?
            .store(COL, &account_row_key(account_id, suffix), value)
            .await
    }

    /// Store only if the row does not exist yet. Returns whether it stored.
    pub async fn store_if_absent(
        &self,
        account_id: &str,
        suffix: &[u8],
        value: &[u8],
    ) -> Result<bool> {
        let key = account_row_key(account_id, suffix);
        let table = Table::open(self.name, COLS).await?;
        table.store_if_absent(COL, &key, value).await
    }

    pub async fn delete(&self, account_id: &str, suffix: &[u8]) -> Result<()> {
        Table::open(self.name, COLS)
            .await?
            .delete(COL, &account_row_key(account_id, suffix))
            .await
    }

    /// All `(suffix, value)` rows belonging to the account.
    pub async fn entries(&self, account_id: &str) -> Result<Vec<(Vec<u8>, Bytes)>> {
        let table = Table::open(self.name, COLS).await?;
        let prefix = account_row_key(account_id, &[]);
        let keys: Vec<Bytes> = table
            .raw()
            .get_keys(COL)
            .await?
            .into_iter()
            .filter(|key| key.starts_with(&prefix))
            .collect();
        let loaded = try_join_all(keys.iter().map(|key| {
            let table = table.clone();
            let prefix_len = prefix.len();
            async move {
                Ok::<_, Error>(
                    table
                        .load_opt(COL, key)
                        .await?
                        .map(|value| (key[prefix_len..].to_vec(), value)),
                )
            }
        }))
        .await?;
        Ok(loaded.into_iter().flatten().collect())
    }

    /// Delete every row belonging to the account (prefix scan) in one atomic batch.
    pub async fn delete_all_for_account(&self, account_id: &str) -> Result<()> {
        let table = Table::open(self.name, COLS).await?;
        delete_account_column(&table, COL, account_id).await
    }
}
