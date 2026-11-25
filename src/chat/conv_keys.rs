//! Conversation-key cryptography: generation plus HPKE wrapping per member.
//!
//! The conversation key encrypts all conversation data (head pages, archive
//! records); every roster member receives it HPKE-sealed to their identity
//! key inside the key ring. Only these primitives are chat-specific — the
//! underlying cached crypto system stays in [`crate::helpers::encryption`].

use anyhow::{Context, Result, bail, ensure};

use veilid_core::{
    BareSharedSecret, DHTSchema, KeyPair, PublicKey, RecordKey, SharedSecret, VALID_CRYPTO_KINDS,
};

use crate::{
    chat::{
        manifest::ConversationRoster,
        record::{MEMBER_SUBKEY_COUNT, OWNER_SUBKEY_COUNT},
    },
    config,
    helpers::encryption::get_cached_crypto_system,
    user::identity_layout::IDENTITY_SUBKEY_COUNT,
    veilid::{
        core::{get_veilid_api, parse_record_key},
        store,
    },
};

/// Generates a random conversation key for encrypting all conversation data.
pub async fn generate_conversation_key() -> Result<SharedSecret> {
    let cs = get_cached_crypto_system().await?;
    Ok(cs.random_shared_secret())
}

/// AAD domain separator for HPKE-wrapped conversation keys. Never change —
/// existing wrapped key ring entries authenticate against it.
const CONV_KEY_WRAP_AAD: &[u8] = b"po.conversation-key.hpke.v1";

/// Wraps a conversation key for a specific member using HPKE (RFC 9180,
/// base mode) sealed to the member's identity key.
///
/// The sealed blob is self-describing (version, crypto kind, encapsulated
/// KEM key, ciphertext+tag) and can only be opened by the member's identity
/// secret. Authenticity of the key ring comes from the DHT record owner's
/// signature, not from the seal itself (HPKE base mode is sender-anonymous).
pub async fn wrap_conversation_key(
    conv_key: &SharedSecret,
    their_public_key: &PublicKey,
) -> Result<Vec<u8>> {
    let cs = get_cached_crypto_system().await?;
    let encapsulation_key = cs
        .encapsulation_key_from_signing_key(their_public_key)
        .context("failed to derive member encapsulation key")?;
    cs.hpke_seal(
        &encapsulation_key,
        CONV_KEY_WRAP_AAD,
        &conv_key.value().bytes(),
    )
    .context("failed to wrap conversation key")
}

/// Unwraps an HPKE-sealed conversation key wrapped for this member.
///
/// Only the member's identity secret can open the blob; no other party's
/// key material is needed.
pub async fn unwrap_conversation_key(wrapped: &[u8], my_keypair: &KeyPair) -> Result<SharedSecret> {
    let cs = get_cached_crypto_system().await?;
    let decapsulation_key = cs
        .decapsulation_key_from_signing_secret(&my_keypair.secret())
        .context("failed to derive decapsulation key")?;
    let decrypted = cs
        .hpke_open(&decapsulation_key, CONV_KEY_WRAP_AAD, wrapped)
        .context("failed to unwrap conversation key")?;
    if decrypted.len() != config::crypto::KEY_LEN {
        bail!("unwrapped key has invalid length");
    }
    Ok(SharedSecret::new(
        VALID_CRYPTO_KINDS[0],
        BareSharedSecret::new(&decrypted),
    ))
}

/// Resolves the signed record descriptor, then verifies the identity layout.
/// An opaque record address is never a signing or encapsulation key.
pub(in crate::chat) async fn identity_public_key(identity_key: &RecordKey) -> Result<PublicKey> {
    let public_key = store::record_owner(identity_key).await?;
    let expected_key = get_veilid_api()
        .await?
        .get_dht_record_key(
            DHTSchema::dflt(IDENTITY_SUBKEY_COUNT)?,
            public_key.clone(),
            None,
        )
        .await?;
    ensure!(
        expected_key == *identity_key,
        "Record is not a DFLT identity owned by the resolved signing key"
    );
    Ok(public_key)
}

/// Binds a roster read out of a conversation record to the writer slots that
/// record's SMPL schema actually grants: slot `i` must hold the member ID
/// derived from the identity record roster entry `i` names, and the subkey
/// layout must be the one every reader assumes.
///
/// The write path derives each slot from the member's identity record, and SMPL
/// only lets that member write its own subkeys. A roster naming a real user on
/// a slot owned by someone else is therefore an impersonation: without this
/// check the reader stores and displays the slot writer's messages under the
/// named user.
///
/// A schema is fixed when its record is created, so counts must match exactly;
/// adding a member means a new record, not a longer roster.
pub(in crate::chat) async fn verify_roster_binding(
    manifest_key: &RecordKey,
    roster: &ConversationRoster,
) -> Result<()> {
    let schema = store::record_schema(manifest_key).await?;
    let DHTSchema::SMPL(smpl) = schema else {
        bail!("Conversation record is not a SMPL record");
    };
    ensure!(
        smpl.o_cnt() == OWNER_SUBKEY_COUNT,
        "Conversation record has an unexpected owner subkey count"
    );
    ensure!(
        smpl.members().len() == roster.members.len(),
        "Conversation roster does not match its record's writer slots"
    );
    let api = get_veilid_api().await?;
    for (slot_index, (slot, member)) in smpl.members().iter().zip(&roster.members).enumerate() {
        ensure!(
            slot.m_cnt == MEMBER_SUBKEY_COUNT,
            "Conversation record member slot has an unexpected subkey count"
        );
        let identity_key = parse_record_key(&member.identity_key)
            .context("Invalid conversation member identity record")?;
        let public_key = identity_public_key(&identity_key).await?;
        ensure!(
            slot.m_key == api.generate_member_id(&public_key)?.into_value(),
            "Conversation roster entry {slot_index} does not own its record writer slot"
        );
    }
    Ok(())
}
