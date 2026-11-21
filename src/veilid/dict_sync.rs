//! Dictionary synchronization across devices via DHT.
//!
//! The current dictionary blob is stored in [`SUBKEY_DICT_BLOB`] of the
//! identity record. [`SUBKEY_DICT_META`] carries its content identity in an
//! automerge CRDT. Assignments are frozen in
//! [`crate::user::identity_layout`].
//!
//! Both `publish_to_dht` and `check_and_sync` batch their DHT operations into a
//! single `with_record` call to avoid redundant open/close cycles.

use anyhow::{Context, Result, bail};
use autosurgeon::{Hydrate, Reconcile};
use bytes::Bytes;
use veilid_core::{KeyPair, RecordKey, SharedSecret};

use crate::{
    compression::codec::{self as compression, DictionaryId},
    storage::{
        automerge::{from_automerge_async, reconcile_to_automerge_async},
        upgrade_status::{increment_upgrade_progress, set_upgrade_status},
    },
};

use super::store::{self, StoreMode};

pub use crate::user::identity_layout::{SUBKEY_DICT_BLOB, SUBKEY_DICT_META};

/// Dictionary version metadata stored in [`SUBKEY_DICT_META`] (automerge CRDT).
#[derive(Debug, Clone, Default, Reconcile, Hydrate)]
struct DictMeta {
    #[autosurgeon(missing = "String::new")]
    dict_id: String,
}

impl DictMeta {
    fn dictionary(&self) -> Result<Option<DictionaryId>> {
        if self.dict_id.is_empty() {
            return Ok(None);
        }
        let id = self.dict_id.parse()?;
        if !matches!(id, DictionaryId::Content(_)) {
            bail!("dictionary metadata requires a content identity");
        }
        Ok(Some(id))
    }
}

/// Write the active dictionary's metadata and blob into a **freshly created**
/// identity record.
///
/// The dictionary has to be announced with the identity record itself: waiting
/// for the first training run leaves the record without a dictionary
/// announcement for up to an hour, and a device that pairs during that window
/// finds nothing to converge on.
///
/// Only safe on a record with no dictionary subkeys yet — a later publication
/// goes through [`publish_to_dht`], which reconciles the existing metadata.
/// These are first-write-only subkeys, so they use plain writes (like the
/// other initial subkeys) rather than a transaction.
pub async fn write_initial_dictionary(key: &RecordKey, secret: &SharedSecret) -> Result<()> {
    let version = compression::current_version();
    if !matches!(version, DictionaryId::Content(_)) {
        bail!("active dictionary must have a content identity");
    }
    let dict = compression::get_dict_arc(version).context("active dictionary bytes are missing")?;
    let meta = DictMeta {
        dict_id: version.to_string(),
    };
    let meta_bytes = reconcile_to_automerge_async(None, meta).await?;
    let dict = Bytes::from_owner(dict);
    store::store_with_mode(
        key,
        SUBKEY_DICT_BLOB,
        dict,
        Some(secret),
        StoreMode::PlainEncrypted,
    )
    .await?
    .require_written()?;
    store::store_with_mode(
        key,
        SUBKEY_DICT_META,
        meta_bytes,
        Some(secret),
        StoreMode::StructuredEncrypted,
    )
    .await?
    .require_written()
}

/// Publish the active dictionary metadata and blob to DHT.
///
/// A superseded or oversized publication leaves the old announcement and blob intact.
pub async fn publish_to_dht(
    identity_key: &RecordKey,
    keypair: &KeyPair,
    secret: &SharedSecret,
) -> Result<()> {
    store::with_record(identity_key, Some(keypair), |key| async move {
        store::with_transaction(vec![key.clone()], |tx| async move {
            compression::reload_from_tablestore().await?;
            let version = compression::current_version();
            if !matches!(version, DictionaryId::Content(_)) {
                bail!("active dictionary must have a content identity");
            }
            let dict = compression::get_dict_arc(version)
                .context("active dictionary bytes are missing")?;
            let dict = Bytes::from_owner(dict);
            let existing_raw = tx.get_raw(&key, SUBKEY_DICT_META).await?;
            let existing = match existing_raw {
                Some(raw) => Some(
                    store::decode_with_mode(&raw, Some(secret), StoreMode::StructuredEncrypted)
                        .await?,
                ),
                None => None,
            };
            let mut meta: DictMeta = match existing.clone() {
                Some(bytes) => from_automerge_async(bytes).await?,
                None => DictMeta::default(),
            };
            meta.dict_id = version.to_string();
            let meta_bytes =
                reconcile_to_automerge_async(existing.as_deref().map(<[u8]>::to_vec), meta).await?;
            tx.set_encrypted_no_dict(&key, SUBKEY_DICT_BLOB, dict, secret)
                .await?;
            tx.set(&key, SUBKEY_DICT_META, meta_bytes, secret, None)
                .await
        })
        .await
    })
    .await
}

/// Check if remote dictionary data is newer or locally incomplete and sync it.
///
/// Batches the metadata read and dictionary blob reads into a single
/// `with_record` call. Called by the DHT watcher when the identity record changes.
pub async fn check_and_sync(identity_key: RecordKey, secret: &SharedSecret) -> Result<()> {
    store::with_record(&identity_key, None::<&KeyPair>, |key| async move {
        let Some(meta_bytes) = store::load_structured(&key, SUBKEY_DICT_META, secret, true).await?
        else {
            return Ok(());
        };

        let remote_meta: DictMeta = from_automerge_async(meta_bytes).await?;
        let Some(remote_version) = remote_meta.dictionary()? else {
            return Ok(());
        };
        let status_run = set_upgrade_status(1);

        if compression::load_dict_version(remote_version)
            .await?
            .is_some()
        {
            if compression::current_version() != remote_version {
                compression::activate_dict_version(remote_version).await?;
            }
        } else {
            let dict = store::load_opt(
                &key,
                SUBKEY_DICT_BLOB,
                secret,
                true,
                StoreMode::PlainEncrypted,
            )
            .await?
            .context("dictionary not found on DHT")?;
            compression::install_and_activate_dict_version(remote_version, dict.into()).await?;
        }
        increment_upgrade_progress(status_run.token(), 1);
        // Harness seam: this device now runs the dictionary the network
        // announces, so both sides of an account converge on one content
        // digest. Reported whether the version was already active or just
        // adopted — convergence, not the act of switching, is the contract.
        compression::report_dict_converged(remote_version);
        Ok(())
    })
    .await
}
