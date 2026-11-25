//! DHT transactions: staged multi-record writes committed or rolled back
//! as one unit.

use anyhow::{Context, Error, Result, bail};
use bytes::Bytes;
use veilid_core::{
    DHTTransaction, DHTTransactionSetValueOptions, KeyPair, RecordKey, SharedSecret,
};

use crate::{nonfatal, veilid::core::get_veilid_api};

use super::{
    api::DhtReadError,
    mode::{StoreMode, encode_with_mode},
};

/// Scoped handle for staging writes into an open DHT transaction.
///
/// Writes go through the same compression/encryption encoding as the
/// non-transactional `store*` functions. Obtain one via [`with_transaction`].
pub struct DhtTx {
    tx: DHTTransaction,
}

impl DhtTx {
    /// Read the transaction-begin ciphertext without decryption or codec work.
    /// Password rotation can preserve the original compressed framing exactly.
    pub async fn get_raw(&self, key: &RecordKey, subkey: u32) -> Result<Option<Bytes>> {
        Ok(self
            .tx
            .get(key.clone(), subkey)
            .await
            .context(DhtReadError)?
            .map(|value| value.data_bytes()))
    }

    /// Stages a versioned dictionary-free zstd + AEAD write — the
    /// transactional counterpart of [`super::api::store`].
    ///
    /// `writer` overrides the record's open writer for this subkey; `None`
    /// signs with the writer the record was opened with.
    pub async fn set(
        &self,
        key: &RecordKey,
        subkey: u32,
        data: impl Into<Bytes>,
        secret: &SharedSecret,
        writer: Option<&KeyPair>,
    ) -> Result<()> {
        let payload =
            encode_with_mode(data.into(), Some(secret), StoreMode::StructuredEncrypted).await?;
        self.stage_payload(key, subkey, payload, writer).await
    }

    /// Stages an unencrypted (plain-compressed) write signed by the record's
    /// open writer.
    pub async fn set_unencrypted(
        &self,
        key: &RecordKey,
        subkey: u32,
        data: impl Into<Bytes>,
    ) -> Result<()> {
        let payload = encode_with_mode(data.into(), None, StoreMode::Unencrypted).await?;
        self.stage_payload(key, subkey, payload, None).await
    }

    /// Stage a plain-zstd + AEAD write without a learned dictionary.
    /// Dictionary blobs use this mode.
    pub async fn set_encrypted_no_dict(
        &self,
        key: &RecordKey,
        subkey: u32,
        data: impl Into<Bytes>,
        secret: &SharedSecret,
    ) -> Result<()> {
        let payload =
            encode_with_mode(data.into(), Some(secret), StoreMode::PlainEncrypted).await?;
        self.stage_payload(key, subkey, payload, None).await
    }

    /// Stage already-encoded ciphertext verbatim. The record must be open with
    /// its writer; supersession fails the enclosing transaction for retry.
    pub async fn set_raw(&self, key: &RecordKey, subkey: u32, payload: Vec<u8>) -> Result<()> {
        self.stage_payload(key, subkey, payload, None).await
    }

    async fn stage_payload(
        &self,
        key: &RecordKey,
        subkey: u32,
        payload: Vec<u8>,
        writer: Option<&KeyPair>,
    ) -> Result<()> {
        let options = writer.map(|w| DHTTransactionSetValueOptions {
            writer: Some(w.clone()),
            min_seqnum: None,
        });
        let newer = self
            .tx
            .set(key.clone(), subkey, payload, options)
            .await
            .context("failed to stage transactional DHT write")?;
        if newer.is_some() {
            bail!(
                "transactional write superseded by a newer network value; \
                 retry against fresh state"
            );
        }
        Ok(())
    }
}

/// Runs `f` inside a DHT transaction over `record_keys`, committing on
/// success and rolling back on error.
///
/// All records must already be **open** (create them / open them first —
/// typically by running inside a [`super::lifecycle::with_record`] scope).
/// Transactions are online-only: offline nodes, contention, and failed
/// consensus surface as retryable errors, which pending-op queues already
/// handle by re-running the op.
///
/// The rollback on error is best-effort (nonfatal-logged) — Veilid tears
/// down uncommitted transactions in the background if rollback fails.
pub async fn with_transaction<F, Fut, T>(record_keys: Vec<RecordKey>, f: F) -> Result<T>
where
    F: FnOnce(DhtTx) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let api = get_veilid_api().await?;
    let tx = api
        .transact_dht_records(record_keys, None)
        .await
        .context("failed to begin DHT transaction")?;

    match f(DhtTx { tx: tx.clone() }).await {
        Ok(value) => {
            tx.commit()
                .await
                .context("failed to commit DHT transaction")?;
            Ok(value)
        }
        Err(e) => {
            nonfatal!(
                "veilid.store",
                "Failed to roll back DHT transaction",
                tx.rollback().await.map_err(Error::from)
            );
            Err(e)
        }
    }
}
