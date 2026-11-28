//! Local doc engine for identity-record subkeys (profile / security /
//! app state): one automerge doc per subkey, kept in a shared table.

use anyhow::Result;
use bytes::Bytes;

use crate::{
    helpers::async_utils::spawn_blocking,
    storage::{
        account_table::AccountTable,
        automerge::{merge_and_hydrate_with_stale_check, reconcile_to_automerge_async},
    },
};

use super::SyncSurface;

/// Shared local table holding one automerge doc per identity-record subkey
/// (rows keyed `{account_id}\0{subkey_le}`).
pub const IDENTITY_DOC_TABLE: &str = "po_identity_docs";

/// The surface's local automerge-doc bytes from the shared identity-doc table.
pub async fn load_doc_bytes<T: SyncSurface>(account_id: &str) -> Result<Option<Bytes>> {
    AccountTable::new(IDENTITY_DOC_TABLE)
        .load(account_id, &T::SUBKEY.to_le_bytes())
        .await
}

/// Persist the surface's local automerge-doc bytes (see [`load_doc_bytes`]).
pub async fn store_doc_bytes<T: SyncSurface>(account_id: &str, doc: &[u8]) -> Result<()> {
    AccountTable::new(IDENTITY_DOC_TABLE)
        .store(account_id, &T::SUBKEY.to_le_bytes(), doc)
        .await
}

/// Reconcile `value` into the surface's local doc (created if missing),
/// persisting when changed, and return the doc bytes. A no-op reconcile
/// writes nothing.
///
/// Identity-subkey publish path: the projected value is reconciled into the
/// persisted history so a merged write can never drop local ops.
pub async fn reconcile_local_doc<T: SyncSurface>(account_id: &str, value: &T) -> Result<Vec<u8>> {
    let existing = load_doc_bytes::<T>(account_id).await?;
    let doc = reconcile_to_automerge_async(existing.as_deref().map(<[u8]>::to_vec), value.clone())
        .await?;
    if existing.as_deref() != Some(doc.as_slice()) {
        store_doc_bytes::<T>(account_id, &doc).await?;
    }
    Ok(doc)
}

/// Merge remote automerge bytes into the surface's local doc and persist the
/// union. Returns the hydrated merged value plus `remote_stale` — true when
/// the remote doc is missing local history and should be re-published.
///
/// The identity-placement pull path — no snapshot bookkeeping. Child records
/// use [`merge_remote`](crate::storage::store_trait::merge_remote), which
/// keeps the Cap'n Proto snapshot and currentness marker in step.
///
/// Locking: none taken, mirroring the historical identity-doc functions;
/// callers needing mutual exclusion hold
/// [`crate::helpers::store_lock::lock_store`] across the sequence.
pub async fn merge_remote_doc<T: SyncSurface>(
    account_id: &str,
    remote_bytes: &[u8],
) -> Result<(T, bool)> {
    let local = load_doc_bytes::<T>(account_id).await?;
    let remote = remote_bytes.to_vec();
    let local_vec = local.as_deref().map(<[u8]>::to_vec);
    let (merged, merged_value, remote_stale): (Vec<u8>, T, bool) =
        spawn_blocking(move || merge_and_hydrate_with_stale_check(local_vec.as_deref(), &remote))
            .await??;
    if Some(merged.as_slice()) != local.as_deref() {
        store_doc_bytes::<T>(account_id, &merged).await?;
    }
    Ok((merged_value, remote_stale))
}
