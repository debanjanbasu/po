//! Child-record lifecycle: deterministic per-feature owner keypairs, the
//! ensure/migrate path behind child pointers, and pointer-cache-aware
//! resolution of a feature's child record key.

use anyhow::{Context, Error, Result};
use dioxus::prelude::ReadableExt;
use veilid_core::{DHTSchema, KeyPair, RecordKey, SharedSecret};

use crate::{
    helpers::encryption::derive_scoped_keypair,
    nonfatal,
    session::phase::{SESSION, mutate_authenticated_user},
    storage::automerge::from_automerge_async,
    user::{
        identity_layout,
        sync_models::{ChildPointer, FeatureId, SyncPointers},
    },
    veilid::store::{self, StoreMode},
};

/// Derives the owner keypair for a feature's child DHT record.
///
/// Child records must not share the identity keypair as owner: Veilid record
/// keys are `hash(owner ‖ schema)`, so same-schema children (they are all
/// `DFLT(1)`) would collide onto a single record. Every device re-derives the
/// same writer from the synced identity keypair.
pub async fn child_owner_keypair(identity_keypair: &KeyPair, feature_id: &str) -> Result<KeyPair> {
    derive_scoped_keypair(identity_keypair, &format!("child-record:{feature_id}")).await
}

pub(super) async fn ensure_child_record(
    pointer: Option<&ChildPointer>,
    expected_count: u16,
    identity_keypair: &KeyPair,
    feature_id: &str,
) -> Result<(RecordKey, ChildPointer, bool, KeyPair)> {
    let owner = child_owner_keypair(identity_keypair, feature_id).await?;
    let schema = DHTSchema::dflt(expected_count)?;
    // The key a correctly-owned child record must have. Pointers to anything
    // else (legacy records owned by the identity keypair collided across
    // features) are migrated to a fresh, correctly-owned record.
    let expected_key = store::derive_record_key(schema.clone(), &owner.key())
        .await
        .context("failed to derive expected child record key")?;

    if let Some(cp) = pointer {
        let key = cp
            .record_key()
            .context("invalid child record key in pointer")?;
        if cp.subkey_count == expected_count && key == expected_key {
            return Ok((key, cp.clone(), false, owner));
        }
    }

    // Pin-aware ensure: never a raw create/open/close, which would globally
    // close a record the child watcher holds pinned with an active watch.
    let key = store::ensure_record_with_keypair(schema, &owner).await?;

    let cp = ChildPointer {
        key: key.to_string(),
        subkey_count: expected_count,
    };

    nonfatal!(
        "user.record",
        "Failed to flush newly created child record",
        store::flush_dht_record(&key).await
    );

    Ok((key, cp, true, owner))
}

pub async fn resolve_child_key_for_id(
    dht_key: RecordKey,
    secret: &SharedSecret,
    keypair: &KeyPair,
    feature_id: &str,
    child_subkeys: u16,
) -> Result<(RecordKey, KeyPair)> {
    let fid = FeatureId::new(feature_id);

    let dht_key_str = dht_key.to_string();
    let (cached_pointer, has_cached_pointers, account_id) = {
        let guard = SESSION.read();
        guard
            .authenticated_user()
            .filter(|user| user.dht_key() == dht_key_str)
            .map_or((None, false, None), |user| {
                let pointer = user
                    .runtime
                    .sync_pointers
                    .as_ref()
                    .and_then(|p| p.pointers.get(&fid).cloned());
                (
                    pointer,
                    user.runtime.sync_pointers.is_some(),
                    Some(user.account_id_arc()),
                )
            })
    };

    let (pointer_for_ensure, loaded_pointers) = if has_cached_pointers {
        (cached_pointer, None)
    } else {
        // SyncSurface::read_from was removed with the trait's dead surface;
        // this is its only remaining semantics: raw subkey read + hydrate.
        let bytes = store::read_subkey(
            &dht_key,
            identity_layout::SUBKEY_POINTERS,
            secret,
            StoreMode::StructuredEncrypted,
        )
        .await?;
        let p: SyncPointers = from_automerge_async(bytes).await?;
        let cp = p.pointers.get(&fid).cloned();
        if let Some(account_id) = account_id.as_ref() {
            mutate_authenticated_user(|user| {
                if user.account_id() == account_id.as_ref() && user.runtime.sync_pointers.is_none()
                {
                    user.set_sync_pointers(p.clone());
                }
            });
        }
        (cp, Some(p))
    };

    // No early return on an existing pointer: `ensure_child_record` also
    // validates that the pointed-at key matches the derived per-feature
    // owner, migrating legacy (collided) records when it does not. The owner
    // keypair is returned so writers need not derive it a second time.
    let (key, new_cp, migrated, owner) = ensure_child_record(
        pointer_for_ensure.as_ref(),
        child_subkeys,
        keypair,
        feature_id,
    )
    .await?;

    if migrated {
        let mut updated = loaded_pointers
            .or_else(|| {
                SESSION
                    .read()
                    .authenticated_user()
                    .filter(|user| user.dht_key() == dht_key_str)
                    .and_then(|user| user.runtime.sync_pointers.clone())
            })
            .unwrap_or_default();
        let cp_clone = new_cp.clone();
        updated.pointers.insert(fid.clone(), new_cp);

        store::reconcile_write(
            &dht_key,
            Some(keypair),
            identity_layout::SUBKEY_POINTERS,
            secret,
            true,
            None,
            &updated,
        )
        .await
        .map_err(Error::from)?;

        if let Some(account_id) = account_id {
            mutate_authenticated_user(|user| {
                if user.account_id() == account_id.as_ref() {
                    let current = user.runtime.sync_pointers.get_or_insert(updated);
                    current.pointers.insert(fid, cp_clone);
                }
            });
        }
    }

    Ok((key, owner))
}
