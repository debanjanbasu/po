//! Identity-record creation: the initial DFLT(16) record with all subkeys seeded,
//! and the encrypted keypair-blob subkey writer.

use anyhow::{Context, Result};
use autosurgeon::Reconcile;
use tokio::try_join;
use veilid_core::{DHTSchema, RecordKey, SharedSecret};

use crate::{
    models::{app_state::AppState, totp::TotpTokenList},
    nonfatal,
    storage::{automerge::reconcile_to_automerge_async, store_trait::SyncSurface},
    user::{
        User, identity_layout,
        sync_models::{ChildPointer, FeatureId, SyncPointers, SyncProfile, SyncSecurity},
    },
    veilid::{
        dict_sync,
        store::{self, StoreMode},
    },
};

use super::child::ensure_child_record;

/// Creates the initial DFLT(16) identity record for a new user.
pub async fn create_identity_record(
    user: &User,
    app_state: &AppState,
    password_secret: &SharedSecret,
) -> Result<RecordKey> {
    let keypair = user
        .runtime
        .keypair
        .as_ref()
        .context("No keypair available for identity creation")?;
    // Deterministic key (hash of owner + schema): ensure opens an existing
    // record (same credentials re-registered, paired device) instead of
    // re-creating it, which fails inside veilid-core.
    let key = store::ensure_record_with_keypair(
        DHTSchema::dflt(identity_layout::IDENTITY_SUBKEY_COUNT)?,
        keypair,
    )
    .await?;

    let profile = SyncProfile::from(user);
    let security = SyncSecurity::from(user);

    let (totp_key, _totp_cp, _totp_mig, _totp_owner) = ensure_child_record(
        None,
        TotpTokenList::CHILD_SUBKEYS,
        keypair,
        TotpTokenList::ID,
    )
    .await?;

    let mut pointers = SyncPointers::default();
    pointers.pointers.insert(
        FeatureId::new(TotpTokenList::ID),
        ChildPointer {
            key: totp_key.to_string(),
            subkey_count: TotpTokenList::CHILD_SUBKEYS,
        },
    );

    // The dictionary has to be announced with the identity record itself.
    // Waiting for the first training run leaves the record without a
    // dictionary announcement for up to an hour, and a device that pairs
    // during that window finds nothing to converge on.
    store::with_record(&key, Some(keypair), |k| async move {
        try_join!(
            write_initial(&k, SyncProfile::SUBKEY, &profile, password_secret),
            write_initial(&k, SyncSecurity::SUBKEY, &security, password_secret),
            write_initial(
                &k,
                identity_layout::SUBKEY_APP_STATE,
                app_state,
                password_secret
            ),
            write_keypair_blob(&k, user),
            write_initial(
                &k,
                identity_layout::SUBKEY_POINTERS,
                &pointers,
                password_secret
            ),
            dict_sync::write_initial_dictionary(&k, password_secret),
        )?;
        Ok(())
    })
    .await?;

    // Best-effort durability flush; the record is already written locally
    // and syncs opportunistically, so a flush failure is not fatal.
    nonfatal!(
        "user.record",
        "Failed to flush newly created identity record",
        store::flush_dht_record(&key).await
    );

    Ok(key)
}

pub async fn write_keypair_blob(key: &RecordKey, user: &User) -> Result<()> {
    store::store_with_mode(
        key,
        identity_layout::SUBKEY_KEYPAIR,
        keypair_blob(user)?,
        None,
        StoreMode::Unencrypted,
    )
    .await?
    .require_written()
}

pub(super) fn keypair_blob(user: &User) -> Result<Vec<u8>> {
    let nonce_len = u32::try_from(user.nonce().len()).context("nonce length overflow")?;
    let mut blob = Vec::with_capacity(4 + user.nonce().len() + user.keypair_encrypted().len());
    blob.extend_from_slice(&nonce_len.to_le_bytes());
    blob.extend_from_slice(user.nonce());
    blob.extend_from_slice(user.keypair_encrypted());
    Ok(blob)
}

/// Write an automerge document to a subkey for the first time (no DHT read).
async fn write_initial<T: Reconcile + Clone + Send + 'static>(
    key: &RecordKey,
    subkey: u32,
    data: &T,
    secret: &SharedSecret,
) -> Result<()> {
    let bytes = reconcile_to_automerge_async(None, data.clone()).await?;
    // Freshly created record: a superseded write would break the creation
    // invariant, so surface it as an error.
    store::store(key, subkey, bytes, secret)
        .await?
        .require_written()
}
