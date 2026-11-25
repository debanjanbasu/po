//! Public subkey read/write API: mode-driven convenience wrappers over the
//! encoding layer and record lifecycle.

use anyhow::{Context, Error as AnyhowError, Result};
use bytes::Bytes;
use dioxus::logger::tracing::debug;
use thiserror::Error;
use veilid_core::{
    DHTRecordReport, DHTReportScope, KeyPair, RecordKey, RoutingContext, SharedSecret, ValueData,
    ValueSubkeyRangeSet, VeilidAPIError,
};

use crate::{helpers::async_utils::sleep_ms, storage::background_errors::report_nonfatal};

use super::{
    lifecycle::{routing_context, with_record},
    mode::{StoreMode, WriteOutcome, load_opt_with_mode, store_with_mode},
};

// `load_opt_with_mode` (in `mode`) is the single read implementation; the raw
// fetch it shares with `read_subkey_raw` lives here as `fetch_subkey_raw`.

// ── Error taxonomy for DHT reads ─────────────────────────────────────────────

/// Marker context attached to `get_dht_value` failures so callers can
/// distinguish network reachability problems from decrypt/decompress errors
/// (via `anyhow::Error::downcast_ref::<DhtReadError>()`).
#[derive(Debug, Error)]
#[error("failed to read DHT subkey")]
pub struct DhtReadError;

/// Marker for a subkey that no replica holds — the record is reachable but
/// incomplete, so retrying cannot help and callers must not report it as a
/// network reachability failure.
#[derive(Debug, Error)]
#[error("no replica found for DHT subkey")]
pub struct DhtSubkeyMissing;

/// True when the error is a transient network condition: a [`DhtReadError`]
/// marker from the subkey read path, or a raw Veilid `TryAgain` / `Timeout` /
/// `NoConnection` (e.g. from `inspect_dht_record`). These say nothing about
/// the record itself — the network was just unreachable at that moment.
pub fn is_transient_network_error(e: &AnyhowError) -> bool {
    e.downcast_ref::<DhtReadError>().is_some()
        || matches!(
            e.downcast_ref::<VeilidAPIError>(),
            Some(
                VeilidAPIError::TryAgain { .. }
                    | VeilidAPIError::Timeout
                    | VeilidAPIError::NoConnection { .. }
            )
        )
}

/// How many times a `TryAgain` subkey read is retried before it becomes a
/// hard failure, and how long to wait between attempts.
///
/// Mirrors the record open's retry budget (`open_record`): the wait covers
/// the window where a freshly attached node's DHT fanout still reaches
/// nobody, so the network reaches "ready" within a few seconds.
const VALUE_RETRY_ATTEMPTS: u32 = 6;
const VALUE_RETRY_DELAY_MS: u64 = 750;

/// Read one subkey from the network, retrying Veilid's `TryAgain`.
///
/// `get_dht_value` hits the network on a cold routing table the same way the
/// record open does, and reports it as `TryAgain` ("offline, try again
/// later"). The record open already retries that (see `open_record`), but the
/// subkey read did not — so a fresh device that read a subkey seconds after
/// attaching died permanently, even though the network came up moments later.
/// That is what killed the first pairing attempt on a real iPhone: the
/// post-pairing identity pull hit a cold table, the read failed once, and the
/// pairing sync never resumed.
///
/// Every other error variant stays a hard error.
pub(super) async fn get_dht_value_with_retry(
    rc: &RoutingContext,
    key: &RecordKey,
    subkey: u32,
    force_refresh: bool,
) -> Result<Option<ValueData>, VeilidAPIError> {
    let mut attempt = 0_u32;
    loop {
        match rc.get_dht_value(key.clone(), subkey, force_refresh).await {
            Ok(value) => return Ok(value),
            Err(VeilidAPIError::TryAgain { .. }) => {
                attempt = attempt.saturating_add(1);
                if attempt > VALUE_RETRY_ATTEMPTS {
                    return Err(VeilidAPIError::TryAgain {
                        message: format!(
                            "subkey read still unanswered after {attempt} attempts: {key}#{subkey}"
                        ),
                    });
                }
                sleep_ms(VALUE_RETRY_DELAY_MS).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Reports a nonfatal error unless it is a transient network condition
/// ([`is_transient_network_error`]): callers retry on their own cadence
/// (watcher ticks, sync rounds), so a flapping or offline network logs a
/// warning instead of spamming the background-error surface. Returns `true`
/// when the error was surfaced (i.e. not a transient failure).
pub fn report_nonfatal_unless_network(
    domain: &'static str,
    context: &'static str,
    e: &AnyhowError,
) -> bool {
    if is_transient_network_error(e) {
        debug!("{context} (transient network): {e:#}");
        false
    } else {
        report_nonfatal(domain, context, e);
        true
    }
}
/// Reads data from a subkey with automatic record management.
///
/// Uses the requested mode (normally versioned dictionary-free zstd + AEAD).
/// Non-forced: serves cold reads (create-once pointers, roster on first
/// open) where the local copy is either absent or authoritative.
///
/// Thin convenience over [`load_opt`] inside a [`with_record`] scope; the
/// subkey read itself (fetch + decode per mode) lives in
/// `load_opt_with_mode`.
pub async fn read_subkey(
    key: &RecordKey,
    subkey: u32,
    secret: &SharedSecret,
    mode: StoreMode,
) -> Result<Bytes> {
    with_record(key, None::<&KeyPair>, |k| async move {
        load_opt(&k, subkey, secret, false, mode).await
    })
    .await?
    .context("Subkey not found on DHT")
}

/// Inspect a DHT record to compare local vs network sequence numbers.
///
/// Returns a [`DHTRecordReport`] whose [`newer_online_subkeys()`] method yields
/// the subkey set where the network has newer data than the local copy.
/// The record must already be open.
///
/// Use [`DHTReportScope::SyncGet`] for a lightweight read-only comparison
/// (does not accept newer values). Use [`UpdateGet`] to also pull newer
/// values into the local Veilid cache.
pub async fn inspect_record(
    key: &RecordKey,
    subkeys: Option<ValueSubkeyRangeSet>,
    scope: DHTReportScope,
) -> Result<DHTRecordReport> {
    let rc = routing_context().await?;
    rc.inspect_dht_record(key.clone(), subkeys, scope)
        .await
        .context("failed to inspect DHT record")
}

/// Reads raw stored bytes without decryption or decompression.
/// The record must already be open; force refresh for migration preflight.
///
/// Shares the record-open + retry + [`DhtReadError`] marking with the decoded
/// read path via [`fetch_subkey_raw`]; only the post-fetch step differs
/// (verbatim bytes here, `decode_with_mode` in `load_opt_with_mode`).
pub async fn read_subkey_raw(
    key: &RecordKey,
    subkey: u32,
    force_refresh: bool,
) -> Result<Option<Vec<u8>>> {
    Ok(fetch_subkey_raw(key, subkey, force_refresh)
        .await?
        .map(|value| value.data().to_vec()))
}

/// Fetch one subkey's stored value, retrying `TryAgain` and marking failures
/// with [`DhtReadError`]. The single record-open + bounds code shared by every
/// read path: [`read_subkey_raw`] returns these bytes verbatim, while
/// `mode::load_opt_with_mode` decodes them through [`decode_with_mode`].
pub(super) async fn fetch_subkey_raw(
    key: &RecordKey,
    subkey: u32,
    force_refresh: bool,
) -> Result<Option<ValueData>> {
    let rc = routing_context().await?;
    get_dht_value_with_retry(&rc, key, subkey, force_refresh)
        .await
        .context(DhtReadError)
}

/// Re-writes raw subkey bytes verbatim to re-publish them to the network.
///
/// Veilid keeps the sequence number when the data and writer are unchanged
/// but still runs the full `SetValue` fanout, so this pushes a locally stored
/// value back out to network nodes that are missing it. The record must be
/// open for writing.
///
/// A "newer value" return is success here: the goal is network coverage,
/// and the network already holding newer data satisfies it.
pub async fn write_subkey_raw(key: &RecordKey, subkey: u32, data: Vec<u8>) -> Result<()> {
    let rc = routing_context().await?;
    let _newer = rc
        .set_dht_value(key.clone(), subkey, data, None)
        .await
        .context("failed to re-publish DHT subkey")?;
    Ok(())
}

// ── Convenience wrappers (stable public API) ──────────────────────────

/// Writes `data` to the given subkey with versioned dictionary-free zstd and
/// encrypting with AEAD. The record must already be open for writing.
///
/// Returns [`WriteOutcome::Superseded`] (with the decoded newer value) when
/// the network rejected the write as stale — see [`WriteOutcome`].
pub async fn store(
    key: &RecordKey,
    subkey: u32,
    data: impl Into<Bytes>,
    secret: &SharedSecret,
) -> Result<WriteOutcome> {
    store_with_mode(
        key,
        subkey,
        data,
        Some(secret),
        StoreMode::StructuredEncrypted,
    )
    .await
}

/// Reads a subkey value, decrypts, and decompresses it.
/// Returns `None` if the subkey has never been written.
///
/// Thin alias for [`load_opt_with_mode`] with `Some(secret)`; see that
/// function for the mode contract.
///
/// `StoreMode::StructuredEncrypted` accepts both stable and legacy frames.
pub async fn load_opt(
    key: &RecordKey,
    subkey: u32,
    secret: &SharedSecret,
    force_refresh: bool,
    mode: StoreMode,
) -> Result<Option<Bytes>> {
    load_opt_with_mode(key, subkey, Some(secret), force_refresh, mode).await
}

/// Reads a subkey value, decrypts, and decompresses stable or legacy structured
/// framing. Convenience wrapper for the common case.
///
/// Equivalent to `load_opt(key, subkey, secret, force_refresh, StoreMode::StructuredEncrypted)`.
pub async fn load_structured(
    key: &RecordKey,
    subkey: u32,
    secret: &SharedSecret,
    force_refresh: bool,
) -> Result<Option<Bytes>> {
    load_opt(
        key,
        subkey,
        secret,
        force_refresh,
        StoreMode::StructuredEncrypted,
    )
    .await
}

/// Reads raw bytes from a subkey, decompressing with plain zstd (no AEAD decryption).
/// Returns `None` if the subkey has never been written.
///
/// Thin alias for [`load_opt_with_mode`] with no secret and
/// [`StoreMode::Unencrypted`]; see that function for the mode contract.
pub async fn load_unencrypted(
    key: &RecordKey,
    subkey: u32,
    force_refresh: bool,
) -> Result<Option<Bytes>> {
    load_opt_with_mode(key, subkey, None, force_refresh, StoreMode::Unencrypted).await
}
