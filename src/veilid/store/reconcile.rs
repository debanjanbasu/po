//! Automerge reconcile-write helpers: CRDT merge + bounded conflict retry
//! on top of the subkey store API.

use anyhow::{Error as AnyhowError, Result, anyhow};
use autosurgeon::Reconcile;
use bytes::Bytes;
use thiserror::Error;
use tracing::debug;
use veilid_core::{KeyPair, RecordKey, SharedSecret};

use crate::storage::automerge::{
    AutomergeSyncError, merge_docs_async, reconcile_to_automerge_async,
};

use super::{
    api::{load_opt, store},
    lifecycle::with_record,
    mode::{StoreMode, WriteOutcome},
};

/// Errors from CRDT reconcile-write operations.
#[derive(Debug, Error)]
pub enum ReconcileWriteError {
    #[error(transparent)]
    Automerge(Box<AutomergeSyncError>),
    #[error(transparent)]
    Storage(#[from] AnyhowError),
}

impl From<AutomergeSyncError> for ReconcileWriteError {
    fn from(err: AutomergeSyncError) -> Self {
        Self::Automerge(Box::new(err))
    }
}

/// Write attempts per reconcile before giving up: initial write plus
/// re-reconciles against newer network values returned by write conflicts.
const RECONCILE_CONFLICT_ATTEMPTS: usize = 3;

/// Reconcile-write an Automerge value to an already-open DHT record.
///
/// Loads the existing subkey, merges via CRDT reconciliation, then stores.
/// When the network rejects the write because it holds a newer value, that
/// value becomes the new base and the write is retried (bounded) — competing
/// writes converge instead of being silently dropped.
///
/// The first attempt reconciles `value` against its original `local_doc` and
/// merges any network history on top. Reapplying the snapshot after that merge
/// would turn remote-only map entries into explicit deletions, so the merge
/// happens once, before the loop. On a `Superseded` retry the attempt payload
/// is instead rebuilt as `reconcile(newer_base, value)`: a fresh history rooted
/// in the newest observed network doc plus `value`. This mirrors the documented
/// no-local-doc semantics (where `value` is already an authoritative update
/// against the first observed base), so it introduces no new deletion class;
/// the single merge keeps concurrent local edits from the original history.
/// Derived struct impls never delete map entries absent from `value`
/// (autosurgeon-derive only puts), and callers union-persist the returned doc,
/// so retries cannot double-root or clobber remote-only state.
///
/// Returns the written doc bytes, whose hydrated value may differ from
/// `value`. Callers must persist that merged value with the returned history.
/// History is not reset to an unrelated document to reduce its size: doing
/// so creates competing collection objects on the next multi-device merge.
///
/// The record must already be open (caller manages open/close lifecycle).
/// Always uses `StoreMode::StructuredEncrypted` — all Automerge data is structured.
pub async fn reconcile_write_open<T: Reconcile + Clone + Send + 'static>(
    key: &RecordKey,
    subkey: u32,
    secret: &SharedSecret,
    force_refresh: bool,
    local_doc: Option<&[u8]>,
    value: &T,
) -> Result<Vec<u8>, ReconcileWriteError> {
    let mut base = load_opt(
        key,
        subkey,
        secret,
        force_refresh,
        StoreMode::StructuredEncrypted,
    )
    .await?;

    for _ in 0..RECONCILE_CONFLICT_ATTEMPTS {
        // Byte-equality against the network base is the only safe merge skip:
        // equal bytes mean the base carries no history `local_doc` lacks, so
        // `intended` already contains everything a merge could add. Any partial
        // history comparison risks dropping remote-only ops.
        let local: Vec<u8> = match local_doc {
            Some(doc) if base.as_deref() == Some(doc) => {
                reconcile_to_automerge_async(Some(doc.to_vec()), value.clone()).await?
            }
            Some(doc) => {
                let intended =
                    reconcile_to_automerge_async(Some(doc.to_vec()), value.clone()).await?;
                merge_docs_async(Some(intended), base.as_ref().map(|b| b.to_vec()))
                    .await?
                    .ok_or_else(|| anyhow!("reconcile merge lost its nonempty local document"))?
            }
            None => {
                reconcile_to_automerge_async(base.as_ref().map(|b| b.to_vec()), value.clone())
                    .await?
            }
        };
        let bytes = Bytes::from(local);
        match store(key, subkey, bytes.clone(), secret).await? {
            WriteOutcome::Written => return Ok(bytes.into()),
            WriteOutcome::Superseded(newer) => {
                debug!(
                    key = %key,
                    subkey,
                    "reconcile write superseded; re-reconciling against newer network value"
                );
                base = Some(newer);
            }
        }
    }

    Err(ReconcileWriteError::Storage(anyhow!(
        "reconcile write kept losing to newer network values after \
         {RECONCILE_CONFLICT_ATTEMPTS} attempts"
    )))
}

/// [`reconcile_write_open`] with automatic record lifecycle
/// (open → write → close). Returns the written doc bytes.
pub async fn reconcile_write<T: Reconcile + Clone + Send + 'static>(
    key: &RecordKey,
    keypair: Option<&KeyPair>,
    subkey: u32,
    secret: &SharedSecret,
    force_refresh: bool,
    local_doc: Option<&[u8]>,
    value: &T,
) -> Result<Vec<u8>, ReconcileWriteError> {
    let secret = secret.clone();
    with_record(key, keypair, |k| async move {
        reconcile_write_open(&k, subkey, &secret, force_refresh, local_doc, value)
            .await
            .map_err(AnyhowError::from)
    })
    .await
    .map_err(ReconcileWriteError::from)
}
