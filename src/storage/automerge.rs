//! Automerge document management — the single source of truth for CRDT data.
//!
//! This module provides load, save, reconcile, merge, and hydrate operations.
//!
//! # One stored form: automerge DEFLATE
//!
//! Every producer here emits [`AutoCommit::save`]: automerge's own DEFLATE
//! form, which the table layer then compresses with structured zstd at
//! [`crate::config::compression::ZSTD_LEVEL`] like any other row. That single
//! form — locally and on the DHT — is the smallest and cheapest one.
//!
//! The tables below are **native level-22** measurements (automerge 0.12 +
//! autosurgeon 0.14, accumulated CRDT history, trained dictionary
//! `assets/dict/structured.zdict`), taken before `ZSTD_LEVEL` moved 22 → 12;
//! the level-12 byte sizes are ~1-3% larger, and the CPU numbers are ceilings
//! roughly 9× above the shipped ones. The *comparison* they justify — DEFLATE
//! plus zstd beats raw plus zstd — is unaffected:
//!
//! | doc | `save_nocompress()` | `save()` (DEFLATE) | zstd L22(dict, `save_nocompress()`) | zstd L22(dict, `save()`) |
//! |---|---|---|---|---|
//! | 20 messages | 3 733 B | 1 727 B | 1 639 B | **1 587 B** |
//! | 200 messages | 34 656 B | 10 280 B | 11 071 B | **10 294 B** |
//! | 1 200 messages | 203 109 B | 57 374 B | 60 306 B | **57 388 B** |
//!
//! Skipping DEFLATE only moves the work: zstd then has to run over a
//! 2.2×–3.5× larger input on the write path, and it stores a *larger* row.
//! Full write-path totals (serialize + zstd, level 22) per publish — 200
//! messages 3 463 µs / 11 071 B (raw) vs 614 µs / 10 294 B; 1 200 messages
//! 18 758 µs / 60 306 B vs 3 233 µs / 57 388 B — roughly 5.7× cheaper and
//! 5–7% smaller. The trained dictionary earns nothing here (5% at most,
//! 0.95–1.00× the size of the same zstd output without it); it earns its keep
//! on other structured rows.
//!
//! [`AutoCommit::load`] reads either form — automerge's storage format is
//! self-describing — so rows written before this rule keep loading. The
//! single-sided merge passthroughs return the one side's bytes verbatim, so a
//! row still holding the pre-change raw form keeps it until the next reconcile
//! rewrites it.

use crate::helpers::async_utils::spawn_blocking;
use automerge::{
    AutoCommit, Automerge, AutomergeError, ROOT, error::UpdateObjectError,
    transaction::Transactable,
};
use autosurgeon::{
    Hydrate, HydrateError, Reconcile, hydrate, reconcile, reconcile::ReconcileError,
};
use dioxus::logger::tracing::warn;
use thiserror::Error;

/// Typed errors for CRDT automerge operations.
#[derive(Debug, Error)]
pub enum AutomergeSyncError {
    #[error(transparent)]
    Automerge(#[from] AutomergeError),
    #[error(transparent)]
    Reconcile(#[from] ReconcileError),
    #[error("failed to rebuild rescued automerge document")]
    RebuildRescued(#[source] UpdateObjectError),
    #[error("schema validation failed")]
    SchemaValidation(#[source] HydrateError),
    #[error("automerge worker task failed")]
    WorkerPanicked,
}

/// Load `existing` (or a fresh document, valid only for first-time writes)
/// and reconcile `value` into it.
fn reconciled_doc<T: Reconcile>(
    existing: Option<&[u8]>,
    value: &T,
) -> Result<AutoCommit, AutomergeSyncError> {
    let mut doc = match existing {
        Some(bytes) => AutoCommit::load(bytes)?,
        None => AutoCommit::new(),
    };
    reconcile(&mut doc, value)?;
    Ok(doc)
}

/// Reconcile a Rust value into an automerge document, preserving CRDT history.
///
/// If `existing` is `Some`, loads the document and reconciles into it.
/// If `None`, creates a fresh document (only valid for first-time writes).
pub fn reconcile_to_automerge<T: Reconcile>(
    existing: Option<&[u8]>,
    value: &T,
) -> Result<Vec<u8>, AutomergeSyncError> {
    Ok(reconciled_doc(existing, value)?.save())
}

/// [`reconcile_to_automerge`] plus the *raw* (uncompressed) serialization
/// length, which the compaction trigger measures
/// (`store_trait::reconcile_row_doc`): the trigger's threshold and watermark
/// are in raw doc units, and the stored form is DEFLATE, so measuring the
/// stored bytes would silently redefine them.
///
/// Two serializations, not one — `save_nocompress()` is an order of magnitude
/// cheaper than `save()`, and this variant runs only on that compaction path.
pub fn reconcile_to_automerge_measured<T: Reconcile>(
    existing: Option<&[u8]>,
    value: &T,
) -> Result<(Vec<u8>, usize), AutomergeSyncError> {
    let mut doc = reconciled_doc(existing, value)?;
    let raw_len = doc.save_nocompress().len();
    Ok((doc.save(), raw_len))
}

/// Hydrate a Rust value from automerge document bytes.
pub fn from_automerge<T: Hydrate>(bytes: &[u8]) -> Result<T, AutomergeSyncError> {
    let doc = AutoCommit::load(bytes)?;
    hydrate(&doc).map_err(AutomergeSyncError::SchemaValidation)
}

/// Hydrate a Rust value from document bytes, attempting best-effort rescue
/// when strict loading fails.
///
/// `Automerge::rescue` extracts the current value from documents that fail
/// strict validation, dropping CRDT history. On rescue, the value is rebuilt
/// into a fresh document and returned alongside its bytes — callers that
/// persist doc bytes must replace the corrupt original with these, since the
/// original bytes will keep failing strict loads (e.g. in `reconcile`).
///
/// Returns `(value, None)` when the strict load succeeded, and
/// `(value, Some(rebuilt_bytes))` when rescue was needed. Errors with the
/// original strict-load error when rescue also fails.
pub fn from_automerge_or_rescue<T: Hydrate>(
    bytes: &[u8],
) -> Result<(T, Option<Vec<u8>>), AutomergeSyncError> {
    let strict_err = match from_automerge(bytes) {
        Ok(value) => return Ok((value, None)),
        Err(e) => e,
    };

    let Ok(rescued) = Automerge::rescue(bytes) else {
        return Err(strict_err);
    };
    let mut doc = AutoCommit::new();
    doc.update_object(&ROOT, &rescued)
        .map_err(AutomergeSyncError::RebuildRescued)?;
    let value = hydrate(&doc).map_err(AutomergeSyncError::SchemaValidation)?;
    warn!(error = %strict_err, "rescued corrupt automerge document (CRDT history dropped)");
    Ok((value, Some(doc.save())))
}

/// Reconcile `value` into `existing`, then merge `remote` into the result —
/// one in-memory doc for both steps, serialized once.
///
/// [`reconcile_to_automerge_measured`] + [`merge_and_hydrate_with_stale_check`]
/// collapsed: the two-function pair parses the reconciled doc back off its own
/// serialization, merges, and serializes again, which for a large doc is a
/// full extra parse+save (and an extra row write) on a path that only ever
/// feeds one store. Returns the merged bytes, the raw length of the merged doc
/// (the compaction watermark's unit — see [`reconcile_to_automerge_measured`]),
/// the hydrated merged value, and the same `remote_stale` verdict
/// [`merge_and_hydrate_with_stale_check`] reports.
pub fn reconcile_then_merge_remote<T: Reconcile + Hydrate>(
    existing: Option<&[u8]>,
    value: &T,
    remote: &[u8],
) -> Result<(Vec<u8>, usize, T, bool), AutomergeSyncError> {
    let mut doc = reconciled_doc(existing, value)?;
    let mut remote_doc = AutoCommit::load(remote)?;
    doc.merge(&mut remote_doc)?;

    let mut merged_heads = doc.get_heads();
    let mut remote_heads = remote_doc.get_heads();
    merged_heads.sort_unstable();
    remote_heads.sort_unstable();
    let remote_stale = merged_heads != remote_heads;

    let raw_len = doc.save_nocompress().len();
    let merged = doc.save();
    let value: T = hydrate(&doc).map_err(AutomergeSyncError::SchemaValidation)?;
    Ok((merged, raw_len, value, remote_stale))
}

/// Merge a remote document into a local one, returning the merged bytes,
/// the hydrated value, and whether the *remote* doc is missing changes
/// present in the merged doc — i.e. the network copy is stale (a publish
/// failed or is still queued) and should be re-published.
///
/// If local is empty/missing, the remote doc is used as-is. Hydrates
/// directly from the in-memory merged doc — avoids a redundant
/// serialize→deserialize round-trip.
pub fn merge_and_hydrate_with_stale_check<T: Hydrate>(
    local: Option<&[u8]>,
    remote: &[u8],
) -> Result<(Vec<u8>, T, bool), AutomergeSyncError> {
    let mut remote_doc = AutoCommit::load(remote)?;
    let (mut doc, remote_stale) = match local {
        Some(local_bytes) if !local_bytes.is_empty() => {
            let mut local_doc = AutoCommit::load(local_bytes)?;
            local_doc.merge(&mut remote_doc)?;
            let mut merged_heads = local_doc.get_heads();
            let mut remote_heads = remote_doc.get_heads();
            merged_heads.sort_unstable();
            remote_heads.sort_unstable();
            let stale = merged_heads != remote_heads;
            (local_doc, stale)
        }
        _ => (remote_doc, false),
    };
    let value: T = hydrate(&doc).map_err(AutomergeSyncError::SchemaValidation)?;
    Ok((doc.save(), value, remote_stale))
}

/// Merge two automerge docs, preserving both histories. Either side may be
/// missing or empty; returns `None` only when both are.
fn merge_docs(a: Option<&[u8]>, b: Option<&[u8]>) -> Result<Option<Vec<u8>>, AutomergeSyncError> {
    let a = a.filter(|bytes| !bytes.is_empty());
    let b = b.filter(|bytes| !bytes.is_empty());
    match (a, b) {
        (Some(a), Some(b)) => {
            let mut doc = AutoCommit::load(a)?;
            let mut other = AutoCommit::load(b)?;
            doc.merge(&mut other)?;
            Ok(Some(doc.save()))
        }
        (Some(one), None) | (None, Some(one)) => Ok(Some(one.to_vec())),
        (None, None) => Ok(None),
    }
}

async fn run_automerge_blocking<T, F>(f: F) -> Result<T, AutomergeSyncError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AutomergeSyncError> + Send + 'static,
{
    spawn_blocking(f)
        .await
        .map_err(|_| AutomergeSyncError::WorkerPanicked)?
}

/// Async [`spawn_blocking`] counterpart to [`reconcile_to_automerge`].
pub async fn reconcile_to_automerge_async<T: Reconcile + Clone + Send + 'static>(
    existing: Option<Vec<u8>>,
    value: T,
) -> Result<Vec<u8>, AutomergeSyncError> {
    run_automerge_blocking(move || reconcile_to_automerge(existing.as_deref(), &value)).await
}

/// Async [`spawn_blocking`] counterpart to [`from_automerge`].
pub async fn from_automerge_async<T: Hydrate + Send + 'static>(
    bytes: impl AsRef<[u8]> + Send + 'static,
) -> Result<T, AutomergeSyncError> {
    run_automerge_blocking(move || from_automerge(bytes.as_ref())).await
}

/// Async [`spawn_blocking`] counterpart to [`merge_docs`].
pub async fn merge_docs_async(
    a: Option<Vec<u8>>,
    b: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>, AutomergeSyncError> {
    run_automerge_blocking(move || merge_docs(a.as_deref(), b.as_deref())).await
}
