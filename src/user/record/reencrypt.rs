//! Replayable AEAD-key migration without changing compressed payloads.
use std::mem;

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use tokio::try_join;
use veilid_core::{KeyPair, RecordKey, SharedSecret};
use zeroize::Zeroizing;

use crate::{
    compression::codec as compression,
    config::dht::IDENTITY_SYNC_TIMEOUT,
    helpers::encryption::{crypto_system, decrypt_with_nonce, encrypt_with_nonce},
    storage::automerge::from_automerge_async,
    user::{User, identity_layout as layout, sync_models::SyncPointers},
    veilid::store::{self, DhtTx},
};

use super::{child::child_owner_keypair, create::keypair_blob};

/// Migrate every password-encrypted identity subkey and every registered
/// child subkey. Conversation SMPL/archive records have independent keys.
/// The password-encrypted keypair is published only after all data succeeds.
pub async fn reencrypt_subkeys(
    dht_key: &RecordKey,
    old_secrets: &[SharedSecret],
    new_secret: &SharedSecret,
    writer_keypair: &KeyPair,
    user: &User,
) -> Result<()> {
    let pointers =
        migrate_identity(dht_key, old_secrets, new_secret, writer_keypair, user, None).await?;
    for (feature, pointer) in &pointers.pointers {
        let key = pointer.record_key()?;
        let writer = child_owner_keypair(writer_keypair, &feature.0).await?;
        store::with_record_timeout(
            &key,
            Some(&writer),
            IDENTITY_SYNC_TIMEOUT,
            |key| async move {
                if !child_has_values(&key, pointer.subkey_count).await? {
                    return Ok(());
                }
                store::with_transaction(vec![key.clone()], |tx| async move {
                    for subkey in 0..u32::from(pointer.subkey_count) {
                        reencrypt_one(&tx, &key, subkey, old_secrets, new_secret, false).await?;
                    }
                    Ok(())
                })
                .await
            },
        )
        .await
        .with_context(|| format!("Password rotation failed for child feature {feature}"))?;
    }

    // A registry changed during the child pass needs another pass; publishing
    // the new keypair first would strand a newly discovered old-key child.
    migrate_identity(
        dht_key,
        old_secrets,
        new_secret,
        writer_keypair,
        user,
        Some(&pointers),
    )
    .await?;
    Ok(())
}

async fn child_has_values(key: &RecordKey, count: u16) -> Result<bool> {
    // A freshly allocated child may have no published values, hence no
    // network transaction participants yet. Absence is not a migration.
    // Force-refresh: a cache-only read would miss remotely published values.
    for subkey in 0..u32::from(count) {
        if store::read_subkey_raw(key, subkey, true).await?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn migrate_identity(
    key: &RecordKey,
    old_secrets: &[SharedSecret],
    new_secret: &SharedSecret,
    writer: &KeyPair,
    user: &User,
    expected_children: Option<&SyncPointers>,
) -> Result<SyncPointers> {
    store::with_record_timeout(key, Some(writer), IDENTITY_SYNC_TIMEOUT, |key| async move {
        // Rewrap compressed dictionary bytes verbatim, before attempting
        // the only structured decode the migration needs: the registry.
        let dict_key = &key;
        store::with_transaction(vec![key.clone()], |tx| async move {
            try_join!(
                reencrypt_one(
                    &tx,
                    dict_key,
                    layout::SUBKEY_DICT_BLOB,
                    old_secrets,
                    new_secret,
                    false
                ),
                reencrypt_one(
                    &tx,
                    dict_key,
                    layout::SUBKEY_DICT_META,
                    old_secrets,
                    new_secret,
                    false
                ),
            )?;
            Ok(())
        })
        .await?;
        store::with_transaction(vec![key.clone()], |tx| async move {
            let pointers = Box::pin(migrate_core(&tx, &key, old_secrets, new_secret)).await?;
            if let Some(expected) = expected_children {
                ensure!(
                    &pointers == expected,
                    "Child registry changed during password rotation; retry required"
                );
                tx.set_unencrypted(&key, layout::SUBKEY_KEYPAIR, keypair_blob(user)?)
                    .await?;
            }
            Ok(pointers)
        })
        .await
    })
    .await
}

async fn migrate_core(
    tx: &DhtTx,
    key: &RecordKey,
    old_secrets: &[SharedSecret],
    new_secret: &SharedSecret,
) -> Result<SyncPointers> {
    let (_, _, _, pointers, _) = try_join!(
        reencrypt_one(
            tx,
            key,
            layout::SUBKEY_PROFILE,
            old_secrets,
            new_secret,
            true
        ),
        reencrypt_one(
            tx,
            key,
            layout::SUBKEY_SECURITY,
            old_secrets,
            new_secret,
            true
        ),
        reencrypt_one(
            tx,
            key,
            layout::SUBKEY_APP_STATE,
            old_secrets,
            new_secret,
            true
        ),
        reencrypt_one(
            tx,
            key,
            layout::SUBKEY_POINTERS,
            old_secrets,
            new_secret,
            true
        ),
        reencrypt_one(
            tx,
            key,
            layout::SUBKEY_PAIRING_BEACON,
            old_secrets,
            new_secret,
            false,
        ),
    )?;
    let mut pointers = pointers.context("Password-rotation registry is missing")?;
    let decompressed =
        compression::decompress_structured(Bytes::from(mem::take(&mut *pointers))).await?;
    Ok(from_automerge_async::<SyncPointers>(decompressed).await?)
}

async fn reencrypt_one(
    tx: &DhtTx,
    key: &RecordKey,
    subkey: u32,
    old_secrets: &[SharedSecret],
    new_secret: &SharedSecret,
    required: bool,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let Some(raw) = tx.get_raw(key, subkey).await? else {
        ensure!(!required, "Required identity subkey {subkey} is missing");
        return Ok(None);
    };
    crypto_system!(crypto);
    if let Ok(plaintext) = decrypt_with_nonce(&raw, new_secret, &crypto) {
        return Ok(Some(Zeroizing::new(plaintext)));
    }
    for secret in old_secrets {
        if let Ok(plaintext) = decrypt_with_nonce(&raw, secret, &crypto) {
            let plaintext = Zeroizing::new(plaintext);
            let ciphertext = encrypt_with_nonce(&plaintext, new_secret, &crypto)?;
            tx.set_raw(key, subkey, ciphertext)
                .await
                .with_context(|| format!("Password rotation write failed at subkey {subkey}"))?;
            return Ok(Some(plaintext));
        }
    }
    bail!("Subkey {subkey}: required password-key history is unavailable or ciphertext is corrupt")
}
