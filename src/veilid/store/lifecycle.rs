//! Record lifecycle: routing context, the per-record scope lock, and the
//! open→use→close scope helpers that serialize against pins.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{LazyLock, Mutex as StdMutex},
    time::Duration,
};

use anyhow::{Context, Error, Result};
use thiserror::Error as ThisError;
use tracing::{debug, warn};
use veilid_core::{
    DHTRecordDescriptor, DHTSchema, KeyPair, PublicKey, RecordKey, RoutingContext,
    VALID_CRYPTO_KINDS, ValueSubkeyRangeSet, VeilidAPIError,
};

use crate::{
    config,
    helpers::{
        async_utils::{sleep_ms, timeout},
        store_lock::{KeyedMutex, recover_poison},
    },
    nonfatal,
    veilid::core::get_veilid_api,
};

/// How many times a `TryAgain` record open is retried before it becomes a
/// hard failure, and how long to wait between attempts.
///
/// The wait covers the window where a freshly attached node's DHT fanout
/// still reaches nobody; the network reaches "ready" within a few seconds.
const OPEN_RETRY_ATTEMPTS: u32 = 6;
const OPEN_RETRY_DELAY_MS: u64 = 750;
// ── Routing context helper ───────────────────────────────────────────

/// Get a routing context from the live Veilid API. Cheap in Veilid 0.5.x —
/// a small struct wrapping the API handle — so no caching is needed, and no
/// shutdown invalidation either: each call sees the current API instance.
pub(super) async fn routing_context() -> Result<RoutingContext> {
    get_veilid_api()
        .await?
        .routing_context()
        .context("failed to get routing context")
}

/// Per-record locks serializing `with_record*` scopes.
///
/// Veilid's open/close are global per record: `close_dht_record` closes the
/// record for every task, and re-opening an already-open record *replaces*
/// its writer — including replacing it with `None`, which downgrades a
/// writable open to read-only. Concurrent scopes on the same key (sync
/// queue, pairing, watcher loads) therefore corrupted each other: one
/// scope's close made a sibling's write fail with "record not open", and a
/// read-only open stripped a writer scope's credentials, failing writes with
/// "value is not writable". Serializing whole open→use→close scopes per
/// record removes both races. Idle entries are pruned by [`KeyedMutex`].
pub(super) static RECORD_LOCKS: LazyLock<KeyedMutex<RecordKey>> = LazyLock::new(KeyedMutex::new);

/// Derives the deterministic opaque key for a schema and record owner.
pub async fn derive_record_key(schema: DHTSchema, owner: &PublicKey) -> Result<RecordKey> {
    get_veilid_api()
        .await?
        .get_dht_record_key(schema, owner.clone(), None)
        .await
        .context("failed to derive record key")
}

/// Ensures the deterministic record for `schema` + `keypair` exists, leaving
/// it *closed* — callers scope ([`with_record`]) or pin ([`pin_record`]) it
/// when they need it open. Returns the opaque record key.
///
/// Open-first: record keys derive deterministically from owner + schema, so
/// an existing record (previous run, recreated account with the same
/// credentials, paired device) is opened straight from the local record
/// store — never re-created, which would fail and ERROR-log inside
/// veilid-core. Only when the record is unknown both locally and (when
/// online) on the network is it created.
///
/// Runs under the per-record scope lock and respects pins: Veilid closes are
/// global per record, so closing here while a watcher holds the record
/// pinned (with an active watch) would yank it out from under the watcher.
/// A pinned record is already known to exist, so this is a no-op for it.
pub async fn ensure_record_with_keypair(schema: DHTSchema, keypair: &KeyPair) -> Result<RecordKey> {
    let key = derive_record_key(schema.clone(), &keypair.key()).await?;

    let _guard = RECORD_LOCKS.lock(key.clone()).await;

    if is_pinned(&key) {
        return Ok(key);
    }

    let rc = routing_context().await?;
    match rc
        .open_dht_record(key.clone(), Some(keypair.clone()), None)
        .await
    {
        Ok(_descriptor) => {}
        // KeyNotFound: unknown locally and on the network. TryAgain: unknown
        // locally and offline — create locally either way; Veilid rehydrates
        // from the network copy (if one exists) on later opens.
        Err(VeilidAPIError::KeyNotFound { .. } | VeilidAPIError::TryAgain { .. }) => {
            let _descriptor = rc
                .create_dht_record(VALID_CRYPTO_KINDS[0], schema, Some(keypair.clone()))
                .await
                .context("failed to create DHT record")?;
            // Veilid bakes a random record-encryption secret into the key it
            // returns (and into this open). Our values are app-layer
            // encrypted, and every device must derive the *same* key, so
            // re-open under the opaque-only key to drop the secret.
            let _descriptor = rc
                .open_dht_record(key.clone(), Some(keypair.clone()), None)
                .await
                .context("failed to re-open created DHT record without encryption key")?;
        }
        Err(e) => return Err(e).context("failed to open DHT record"),
    }
    rc.close_dht_record(key.clone())
        .await
        .context("failed to close ensured DHT record")?;
    Ok(key)
}

/// Opens an existing DHT record for reading (or writing if `writer` is provided).
///
/// Private on purpose: all record lifecycle goes through [`with_record`] /
/// [`pin_record`] / [`ensure_record_with_keypair`], which serialize against
/// each other per record.
///
/// Veilid reports a cold routing table as `TryAgain` — "offline, try again
/// later" or "no node answered the open" (veilid-core
/// `storage_manager/open_record.rs`). That is explicitly retryable, but a
/// single attempt turned it into a hard failure: a fresh device opens a
/// record seconds after attaching, while the fanout still reaches nobody,
/// so pairing died on the first attempt even though the network came up
/// moments later. Retry `TryAgain` with a short backoff before giving up;
/// every other variant stays a hard error.
pub(super) async fn open_record(
    key: &RecordKey,
    writer: Option<KeyPair>,
) -> Result<DHTRecordDescriptor> {
    let rc = routing_context().await?;
    let mut attempt = 0_u32;
    loop {
        match rc.open_dht_record(key.clone(), writer.clone(), None).await {
            Ok(descriptor) => return Ok(descriptor),
            Err(VeilidAPIError::TryAgain { .. }) => {
                attempt = attempt.saturating_add(1);
                if attempt > OPEN_RETRY_ATTEMPTS {
                    return Err(VeilidAPIError::TryAgain {
                        message: format!(
                            "record open still unanswered after {attempt} attempts: {key}"
                        ),
                    })
                    .context("failed to open DHT record");
                }
                warn!(
                    "record open unanswered (attempt {attempt}); retrying in {}ms",
                    OPEN_RETRY_DELAY_MS
                );
                sleep_ms(OPEN_RETRY_DELAY_MS).await;
            }
            Err(e) => return Err(e).context("failed to open DHT record"),
        }
    }
}

/// Closes a previously opened DHT record.
///
/// Veilid closes are global per record — this closes it for *every* task
/// (and cancels the record's outbound watch), not just the caller. Private
/// on purpose; see [`open_record`].
pub(super) async fn close_record(key: &RecordKey) -> Result<()> {
    let rc = routing_context().await?;
    rc.close_dht_record(key.clone())
        .await
        .context("failed to close DHT record")
}

/// Runs `op` once and, when it fails with a transient network error, waits
/// [`config::nudge::RETRY_DELAY_MS`] and runs it once more — **outside** any
/// record scope.
///
/// This is [`crate::helpers::async_utils::retry_once_on_transient`] with the
/// classifier and the budget fixed to the store's own, kept here rather than
/// parameterised so the sleep it makes can never be re-introduced inside a
/// scope: `with_record*` hold the per-record lock across the whole
/// open→use→close, and a 2 s sleep there serialises the record for 2 s
/// (loading, then closing, then the single `TryAgain` retry the read paths
/// already do internally) — worse than re-acquiring the lock. Callers that
/// need a custom classifier or budget still use the helper directly, on
/// work that holds no record scope.
///
/// Returns the second attempt's result when it ran, `Err` either way.
///
/// Thin alias over the canonical [`crate::helpers::async_utils::retry_once_on_transient`]
/// with Veilid's transient-network classifier, so the two former copies of the
/// one-retry pattern cannot drift.
pub(super) async fn retry_transient_on_error<T, F, Fut>(op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    crate::helpers::async_utils::retry_once_on_transient(super::api::is_transient_network_error, op)
        .await
}

/// Opens a record and returns Veilid's validated descriptor for it (owner and
/// schema as the network nodes agree on them, not as stored data claims).
///
/// Re-opening preserves a pin's writer and watches; unpinned opens are closed
/// before returning. Must not be called inside another scope for the same key.
async fn record_descriptor(key: &RecordKey) -> Result<DHTRecordDescriptor> {
    let _guard = RECORD_LOCKS.lock(key.clone()).await;
    let (writer, close_after) = match pin_state(key) {
        PinState::Unpinned => (None, true),
        PinState::Pinned(writer) => (writer, false),
    };
    let descriptor = open_record(key, writer).await?;
    if close_after {
        close_record(key).await?;
    }
    Ok(descriptor)
}

/// Resolve the owner's public key from Veilid's validated record descriptor.
pub async fn record_owner(key: &RecordKey) -> Result<PublicKey> {
    Ok(record_descriptor(key).await?.owner())
}

/// Resolve a record's schema from Veilid's validated record descriptor.
///
/// A schema is the record's writer-slot list — which key may write which
/// subkey is fixed when the record is created, so callers binding stored data
/// back to a writer read it here instead of trusting the data's own claims.
pub async fn record_schema(key: &RecordKey) -> Result<DHTSchema> {
    Ok(record_descriptor(key).await?.schema())
}

/// Opens a DHT record, runs `f`, then always closes it (default timeout).
///
/// Scopes on the same key are serialized (see [`RECORD_LOCKS`]), so this is
/// safe to run concurrently with other scopes — but must not be nested on
/// the same key, which would deadlock.
///
/// `f` runs once: the lock is held across it, so it must not sleep waiting
/// out a transient network flap — a retry sleeps outside the scope. Failures
/// of a transient shape surface to the caller to retry with
/// [`retry_transient_on_error`]; callers that will not retry are better off
/// wrapping this call in it too, so the retry re-opens instead of re-running
/// `f` against a scope that may have lost its record.
pub async fn with_record<F, Fut, T>(key: &RecordKey, keypair: Option<&KeyPair>, f: F) -> Result<T>
where
    F: FnOnce(RecordKey) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    with_record_timeout(key, keypair, config::dht::RECORD_TIMEOUT, f).await
}

/// Opens a DHT record with custom timeout, runs `f`, then always closes it.
///
/// The record is guaranteed to be closed even if `f` returns an error or
/// times out. The whole open→use→close scope holds the per-record lock, so
/// concurrent scopes on the same key run one after another instead of
/// closing the record (or replacing its writer) out from under each other.
/// The lock therefore covers every await inside `f`: see [`with_record`] for
/// what that rules out — sleeping out a transient error in particular.
pub async fn with_record_timeout<F, Fut, T>(
    key: &RecordKey,
    keypair: Option<&KeyPair>,
    duration: Duration,
    f: F,
) -> Result<T>
where
    F: FnOnce(RecordKey) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let _guard = RECORD_LOCKS.lock(key.clone()).await;

    // A pinned record (session-owned open with an active watch) is already
    // open: closing it here would cancel the pin's watch, and opening
    // read-only would strip the pin's writer. Scopes therefore borrow the
    // pinned open. Re-opening with a different writer is safe — it replaces
    // only the writer context and leaves the watch running — so a scope
    // needing credentials the pin lacks swaps its writer in and restores
    // the pin's writer on exit. Pin state can't change mid-scope: pin and
    // unpin take this same per-record lock.
    let pin = pin_state(key);
    let requested = keypair.cloned();
    let swapped = match &pin {
        PinState::Unpinned => {
            let _descriptor = open_record(key, requested).await?;
            false
        }
        PinState::Pinned(pin_writer) => {
            let needs_swap = requested.is_some() && requested != *pin_writer;
            if needs_swap {
                let _descriptor = open_record(key, requested).await?;
            }
            needs_swap
        }
    };

    // Ensure close (or writer restore) happens regardless of outcome. A
    // transient error retries out here — after the scope's close, so the
    // retry re-opens — instead of sleeping inside `f`, which would hold the
    // record's lock for the wait. `f` is `FnOnce`, so the retry re-calls it
    // through the one-shot cell that holds it; the first call replaces it with
    // `Err`, so a third attempt (which [`retry_transient_on_error`] never
    // makes) would refuse to re-run a closure that must not run twice.
    let mut f = Some(f);
    let result = timeout(
        duration,
        retry_transient_on_error(move || {
            let fut: Pin<Box<dyn Future<Output = Result<T>>>> = match f.take() {
                Some(f) => Box::pin(f(key.clone())),
                None => Box::pin(futures_util::future::err(Error::msg(
                    "record scope retried more than once; refusing to re-run it",
                ))),
            };
            fut
        }),
    )
    .await;

    match pin {
        PinState::Unpinned => {
            nonfatal!(
                "veilid.store",
                "Failed to close DHT record",
                close_record(key).await
            );
        }
        PinState::Pinned(pin_writer) if swapped => {
            nonfatal!(
                "veilid.store",
                "Failed to restore pinned record writer",
                open_record(key, pin_writer).await.map(|_| ())
            );
        }
        PinState::Pinned(_) => {}
    }

    result.map_err(Error::from)?
}

// ── Pinned records ───────────────────────────────────────────────────────────

/// Records held open for the whole time something cares about them — the
/// identity record, child records, and actively watched conversation
/// records — mapped to the writer they were pinned with. The pin owns the
/// record's open lifecycle: `with_record*` scopes borrow the pinned open
/// (temporarily swapping the writer when they need different credentials)
/// instead of open/closing, which would kill the pin's DHT watch. Guarded
/// by the same per-record lock as scopes, so pin state cannot change
/// mid-scope.
static PINNED_RECORDS: LazyLock<StdMutex<HashMap<RecordKey, Option<KeyPair>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn is_pinned(key: &RecordKey) -> bool {
    recover_poison(PINNED_RECORDS.lock()).contains_key(key)
}

/// Pin state of a record: unpinned, or pinned with the writer it was opened
/// with (absent for a read-only pin).
enum PinState {
    Unpinned,
    Pinned(Option<KeyPair>),
}

fn pin_state(key: &RecordKey) -> PinState {
    recover_poison(PINNED_RECORDS.lock())
        .get(key)
        .map_or(PinState::Unpinned, |writer| {
            PinState::Pinned(writer.clone())
        })
}

/// Opens `key` and keeps it open until [`unpin_record`] / [`clear_pins`].
///
/// Idempotent — re-pinning an already-pinned key re-opens it (refreshing
/// the writer) and stays pinned. Scoped writes with a different keypair
/// still work on a read-only pin: the scope swaps the writer in and
/// restores the pin's writer on exit.
pub async fn pin_record(key: &RecordKey, writer: Option<KeyPair>) -> Result<()> {
    let _guard = RECORD_LOCKS.lock(key.clone()).await;
    let _descriptor = open_record(key, writer.clone()).await?;
    recover_poison(PINNED_RECORDS.lock()).insert(key.clone(), writer);
    Ok(())
}

/// Closes the record and removes the pin. Returns `true` if `key` was
/// pinned (and has now been closed). The pin is dropped only after a
/// successful close — on failure the record stays tracked so the caller
/// can retry instead of leaking an untracked open.
pub async fn unpin_record(key: &RecordKey) -> Result<bool> {
    let _guard = RECORD_LOCKS.lock(key.clone()).await;
    if !is_pinned(key) {
        return Ok(false);
    }
    close_record(key).await?;
    recover_poison(PINNED_RECORDS.lock()).remove(key);
    Ok(true)
}

/// Drops all pin state without network calls — for Veilid shutdown, where
/// the API teardown closes every record anyway. Native only: web has no
/// shutdown edge, so nothing calls it there.
#[cfg(not(target_arch = "wasm32"))]
pub fn clear_pins() {
    recover_poison(PINNED_RECORDS.lock()).clear();
}

// ── DHT watches and record flushes ───────────────────────────────────────────

/// DHT watch setup outcomes that require caller recovery.
#[derive(Debug, ThisError)]
pub enum WatchError {
    #[error("Veilid did not activate the DHT watch")]
    Inactive,
}

/// Installs (or replaces) a DHT watch on an **open** record. `None` subkeys
/// watches the whole record. Re-issuing updates the desired watch params.
///
/// `watch_dht_values` returning `Ok(true)` means the *desired* watch is
/// registered with veilid-core's outbound watch manager — per-node
/// placement happens asynchronously and is not guaranteed. `Ok(false)`
/// means the watch is not active (cancelled): push delivery for this
/// record is dead until re-armed, so surface it loudly.
pub async fn watch_record(key: &RecordKey, subkeys: Option<ValueSubkeyRangeSet>) -> Result<()> {
    let rc = routing_context().await?;
    let active = rc
        .watch_dht_values(key.clone(), subkeys, None, None)
        .await
        .context("failed to watch DHT record")?;
    if active {
        debug!("DHT watch registered for {key}");
        Ok(())
    } else {
        warn!("DHT watch on {key} reported inactive — no push delivery until re-armed");
        Err(WatchError::Inactive.into())
    }
}

/// Flushes pending writes for a DHT record to ensure durability.
/// Returns `true` if there were pending writes that were flushed.
pub async fn flush_dht_record(key: &RecordKey) -> Result<bool> {
    let rc = routing_context().await?;
    rc.flush_dht_record(key.clone(), None)
        .await
        .context("failed to flush DHT record")
}
