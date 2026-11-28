//! Storage-mode plumbing: compression + AEAD encoding/decoding per
//! [`StoreMode`].

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use veilid_core::{RecordKey, SharedSecret};

use crate::{
    compression::codec as compression,
    helpers::{
        encryption::{crypto_system_sync, decrypt_with_nonce, encrypt_with_nonce},
        spawn_blocking,
    },
};

use super::{api::fetch_subkey_raw, lifecycle::routing_context};
/// Compress and encrypt in a single blocking task.
///
/// Pipelines both operations to reduce context switch overhead while sharing
/// the stable dictionary-free transport frame on every DHT write path.
async fn compress_and_encrypt(data: Bytes, secret: &SharedSecret) -> Result<Vec<u8>> {
    let secret = secret.clone();

    spawn_blocking(move || {
        let compressed = compression::compress_transport_blocking(&data)?;
        let crypto = crypto_system_sync().context("failed to get crypto system")?;
        encrypt_with_nonce(&compressed, &secret, &crypto)
    })
    .await?
}

/// Storage mode for DHT operations.
///
/// All modes use [`crate::config::compression::ZSTD_LEVEL`] (22), zstd's
/// maximum level. The mode determines dictionary usage and encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreMode {
    /// Versioned dictionary-free zstd + AEAD for all application DHT data.
    /// Readers also accept legacy dictionary frames for migration.
    StructuredEncrypted,
    /// Plain zstd compression + AEAD encryption (no dictionary).
    /// Used for dictionary blobs to avoid circular dependency.
    PlainEncrypted,
    /// Plain zstd compression only (no encryption).
    /// Used for already-encrypted data like keypair blobs.
    Unencrypted,
}

impl StoreMode {
    const fn needs_encryption(self) -> bool {
        matches!(self, Self::StructuredEncrypted | Self::PlainEncrypted)
    }
}

/// Outcome of a DHT subkey write.
///
/// Veilid's `set_dht_value` returns the network's value when it is *newer*
/// than the one being written — meaning the write was **not applied**.
/// Callers must not treat a superseded write as durable.
#[derive(Debug)]
#[must_use = "a superseded write was not applied; handle or reject it"]
pub enum WriteOutcome {
    /// Accepted locally. Network propagation is Veilid's job: offline
    /// writes are queued in Veilid's own table store and replayed on
    /// reattach/restart.
    Written,
    /// The network already holds a newer value for this subkey; the write
    /// was rejected. Payload is that newer value, decoded through the same
    /// mode (decrypt + decompress) as the attempted write.
    Superseded(Bytes),
}

impl WriteOutcome {
    /// Errors on [`WriteOutcome::Superseded`] — for absolute-value writes
    /// where losing to a concurrent writer must surface as a retryable
    /// failure (the sync queue re-runs the op against fresh state).
    pub fn require_written(self) -> Result<()> {
        match self {
            Self::Written => Ok(()),
            Self::Superseded(_) => Err(anyhow!(
                "write superseded by a newer network value; retry against fresh state"
            )),
        }
    }
}

/// Internal write: compress then optionally encrypt based on mode.
///
/// Returns [`WriteOutcome::Superseded`] with the decoded newer value when
/// the network rejected the write as older than what it already holds.
pub async fn store_with_mode(
    key: &RecordKey,
    subkey: u32,
    data: impl Into<Bytes>,
    secret: Option<&SharedSecret>,
    mode: StoreMode,
) -> Result<WriteOutcome> {
    let payload = encode_with_mode(data.into(), secret, mode).await?;

    let rc = routing_context().await?;
    let newer = rc
        .set_dht_value(key.clone(), subkey, payload, None)
        .await
        .context("failed to write DHT subkey")?;

    match newer {
        None => Ok(WriteOutcome::Written),
        Some(value) => {
            let decoded = decode_with_mode(value.data(), secret, mode).await?;
            Ok(WriteOutcome::Superseded(decoded))
        }
    }
}

/// Encode subkey bytes for storage: compress, then optionally encrypt,
/// according to `mode`. Shared by direct writes and transactional writes.
pub(super) async fn encode_with_mode(
    data: Bytes,
    secret: Option<&SharedSecret>,
    mode: StoreMode,
) -> Result<Vec<u8>> {
    if mode.needs_encryption() {
        let Some(s) = secret else {
            bail!("secret required for encrypted store mode");
        };
        if mode == StoreMode::StructuredEncrypted {
            // Pipeline both in one blocking task for structured+encrypted (hot path).
            compress_and_encrypt(data, s).await
        } else {
            let compressed = compression::compress_plain(data).await?;
            let crypto = crypto_system_sync().context("crypto system not available")?;
            encrypt_with_nonce(&compressed, s, &crypto)
        }
    } else {
        // Unencrypted mode is always plain-compressed.
        Ok(compression::compress_plain(data).await?.into())
    }
}

/// Decode raw stored subkey bytes: optionally decrypt, then decompress with
/// the mode-appropriate domain. Shared by reads and write-conflict returns.
pub async fn decode_with_mode(
    raw: &[u8],
    secret: Option<&SharedSecret>,
    mode: StoreMode,
) -> Result<Bytes> {
    let after_decrypt = if mode.needs_encryption() {
        let crypto = crypto_system_sync().context("crypto system not available")?;
        Bytes::from(decrypt_with_nonce(
            raw,
            secret.context("secret required for encrypted load mode")?,
            &crypto,
        )?)
    } else {
        Bytes::copy_from_slice(raw)
    };

    match mode {
        StoreMode::StructuredEncrypted => compression::decompress_structured(after_decrypt).await,
        StoreMode::PlainEncrypted | StoreMode::Unencrypted => {
            compression::decompress_plain(after_decrypt).await
        }
    }
}

/// The one real read implementation: fetch one subkey, then decode it through
/// the requested mode. Every public read path funnels here.
///
/// Mode contract:
/// - `StructuredEncrypted` / `PlainEncrypted` require `secret = Some(..)`
///   (`decode_with_mode` errors otherwise) and decrypt before decompressing;
///   `StructuredEncrypted` additionally accepts legacy dictionary frames.
/// - `Unencrypted` takes `secret = None` (a `Some` is ignored) and only
///   plain-zstd decompresses — for already-encrypted payloads like keyrings.
///
/// Returns `Ok(None)` when the subkey has never been written. Fetch failures
/// carry the [`super::api::DhtReadError`] marker (see
/// [`super::api::is_transient_network_error`]); the fetch itself — record
/// open, `TryAgain` retry budget, bounds — is shared with the raw read path
/// via [`super::api::fetch_subkey_raw`].
pub(super) async fn load_opt_with_mode(
    key: &RecordKey,
    subkey: u32,
    secret: Option<&SharedSecret>,
    force_refresh: bool,
    mode: StoreMode,
) -> Result<Option<Bytes>> {
    // `get_dht_value` hits the network on a cold routing table the same way
    // the record open does, and reports it as `TryAgain` — but unlike the
    // open (see `open_record`) it had no retry, so a fresh device read a
    // subkey seconds after attaching and the read died permanently. The
    // network came up moments later; the pairing sync did not.
    let Some(value) = fetch_subkey_raw(key, subkey, force_refresh).await? else {
        return Ok(None);
    };

    let decoded = decode_with_mode(value.data(), secret, mode).await?;
    Ok(Some(decoded))
}
