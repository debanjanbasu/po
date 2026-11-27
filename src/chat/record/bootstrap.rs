//! Local conversation construction and DHT bootstrap.

use std::{collections::BTreeMap, str::from_utf8, sync::Arc};

use anyhow::{Context, Error, Result, anyhow, ensure};
use veilid_core::{DHTSchema, DHTSchemaSMPLMember, RecordKey, bytes::Bytes};

use crate::{
    chat::{
        contact::ContactList,
        conv_keys::{
            generate_conversation_key, identity_public_key, unwrap_conversation_key,
            wrap_conversation_key,
        },
        feed::ParticipantFeedMeta,
        manifest::{
            ConversationKeyRing, ConversationManifest, ConversationRoster, KeyRingEntry,
            MemberEntry, MemberRole, MemberStatus,
        },
        storage::{LocalConversation, load_for_account, save_for_account},
    },
    helpers::spawn_blocking,
    models::pending_operation::PendingOperation,
    nonfatal,
    storage::{
        automerge::{from_automerge, reconcile_to_automerge},
        store_trait::load_local,
    },
    sync::pending_ops,
    user::identity_context::IdentityContext,
    veilid::{
        core::{get_veilid_api, parse_record_key},
        store,
    },
};

use super::{MEMBER_SUBKEY_COUNT, OWNER_SUBKEY_COUNT, conv_owner_keypair, member_feed_subkey};

/// Builds local conversation state without DHT calls. The queued bootstrap
/// resolves authenticated member signing keys and wraps this saved secret.
pub async fn build_local_conversation(
    my_username: &str,
    my_identity_key: &str,
    other_username: &str,
    other_identity_key: &str,
) -> Result<LocalConversation> {
    let conv_key = generate_conversation_key().await?;

    let key_ring = ConversationKeyRing {
        entries: BTreeMap::new(),
        owner_identity_key: my_identity_key.to_owned(),
    };

    let manifest = ConversationManifest::new_direct(my_identity_key, other_username);

    let mut roster = ConversationRoster::default();
    roster.add_member(MemberEntry {
        username: my_username.to_owned(),
        identity_key: my_identity_key.to_owned(),
        role: MemberRole::Owner,
        status: MemberStatus::Active,
    });
    roster.add_member(MemberEntry {
        username: other_username.to_owned(),
        identity_key: other_identity_key.to_owned(),
        role: MemberRole::Member,
        status: MemberStatus::Active,
    });

    let mut conv = LocalConversation::with_conv_key(&conv_key);
    conv.manifest = manifest;
    conv.roster = roster;
    conv.key_ring = key_ring;
    conv.my_feed_meta = ParticipantFeedMeta::default();

    Ok(conv)
}

/// Creates the conversation SMPL record (owned by the derived conversation
/// owner keypair) and persists its key into the pending op so retries reuse
/// it instead of creating another record.
async fn create_conversation_record(
    op: &PendingOperation,
    ctx: &IdentityContext,
    conv: &mut LocalConversation,
) -> Result<RecordKey> {
    let api = get_veilid_api().await?;
    let secret = conv.conv_secret();
    let mut members = Vec::with_capacity(conv.roster.members.len());
    let mut entries = BTreeMap::new();
    for member in &conv.roster.members {
        let identity_key = parse_record_key(&member.identity_key)
            .context("Invalid conversation member identity record")?;
        let public_key = identity_public_key(&identity_key).await?;
        members.push(DHTSchemaSMPLMember {
            m_key: api.generate_member_id(&public_key)?.into_value(),
            m_cnt: MEMBER_SUBKEY_COUNT,
        });
        entries.insert(
            member.identity_key.clone(),
            KeyRingEntry {
                identity_key: member.identity_key.clone(),
                wrapped_key: wrap_conversation_key(&secret, &public_key).await?,
            },
        );
    }
    conv.key_ring = ConversationKeyRing {
        entries,
        owner_identity_key: ctx.user.dht_key().to_owned(),
    };

    let schema = DHTSchema::smpl(OWNER_SUBKEY_COUNT, members)
        .map_err(|e| anyhow!("failed to create SMPL schema: {e}"))?;

    let owner = conv_owner_keypair(&ctx.keypair, conv.manifest.created_at).await?;
    // Deterministic key (hash of owner + schema): ensure is idempotent
    // across retries and leaves the record closed — the caller's
    // with_record opens it for the initial writes.
    let key = store::ensure_record_with_keypair(schema, &owner).await?;

    // Re-derivation also repairs queued pre-release records whose schema
    // incorrectly used identity record addresses as member IDs.
    let mut updated_op = op.clone();
    updated_op.payload = Bytes::from(key.to_string());
    pending_ops::update_op(updated_op, Some(op))
        .await
        .context("Failed to persist DHT key into ConvRecordCreate op payload")?;

    nonfatal!(
        "chat.record",
        "Failed to flush newly created SMPL record",
        store::flush_dht_record(&key).await
    );

    Ok(key)
}

/// Creates the SMPL DHT record and writes initial subkeys for a conversation
/// that was previously built locally via [`build_local_conversation`].
///
/// Idempotent: if `op.payload` already contains a record key from a prior
/// attempt, the existing DHT record is reused instead of creating a new one.
///
/// Returns the final manifest record key string so the caller (queue handler
/// in `session_flow`) can update `ConversationIndex` and `ContactList`.
pub async fn bootstrap_conversation_dht(
    op: &PendingOperation,
    ctx: &IdentityContext,
) -> Result<String> {
    let provisional_key = op
        .target_key
        .as_deref()
        .context("ConvRecordCreate op missing target_key (provisional key)")?;

    let account_id = ctx.user.account_id();
    if !op.payload.is_empty() {
        let key_str =
            from_utf8(&op.payload).context("ConvRecordCreate op payload is not valid UTF-8")?;
        let _ =
            parse_record_key(key_str).context("ConvRecordCreate op payload is not a valid key")?;
        let saved = load_for_account(account_id, key_str).await?;
        if saved.has_conv_key() {
            validate_bootstrap_state(&saved, ctx)?;
            return Ok(key_str.to_owned());
        }
    }

    let mut conv = Arc::unwrap_or_clone(load_for_account(account_id, provisional_key).await?);
    if !conv.has_conv_key()
        && let Some(final_key) = finalized_contact_key(ctx, provisional_key).await?
    {
        return Ok(final_key);
    }
    validate_bootstrap_state(&conv, ctx)?;
    let manifest_key = create_conversation_record(op, ctx, &mut conv).await?;
    if let Some(key_ring) = publish_initial_state(&manifest_key, ctx, &conv).await? {
        conv.key_ring = key_ring;
    }
    let final_key = manifest_key.to_string();
    save_for_account(account_id, &final_key, conv).await?;
    // Finalization removes the provisional row only after the index/contact
    // and invite work are durable. Replays can use the final row even then.
    Ok(final_key)
}

/// A replacement/manual retry may not carry the earlier attempt's payload.
/// Finalization publishes the contact pointer before deleting provisional state.
async fn finalized_contact_key(
    ctx: &IdentityContext,
    provisional_key: &str,
) -> Result<Option<String>> {
    let prefix = format!("provisional:{}:", ctx.user.account_id());
    let other_identity = provisional_key
        .strip_prefix(&prefix)
        .context("Conversation provisional key does not match its account")?;
    let contacts = load_local::<ContactList>(ctx.user.account_id().as_bytes()).await?;
    let Some(contact) = contacts.find_by_dht_key(other_identity) else {
        return Ok(None);
    };
    let final_key = contact.conversation_record_key.as_ref();
    if final_key.is_empty() {
        return Ok(None);
    }
    let _ = parse_record_key(final_key).context("Invalid finalized conversation key")?;
    let saved = load_for_account(ctx.user.account_id(), final_key).await?;
    validate_bootstrap_state(&saved, ctx)?;
    ensure!(
        saved
            .roster
            .members
            .iter()
            .any(|member| member.identity_key == other_identity),
        "Finalized conversation does not contain its contact"
    );
    Ok(Some(final_key.to_owned()))
}

fn validate_bootstrap_state(conv: &LocalConversation, ctx: &IdentityContext) -> Result<()> {
    ensure!(
        conv.has_conv_key() && conv.manifest.created_at != 0,
        "Conversation bootstrap state is missing; refusing to publish defaults"
    );
    ensure!(
        conv.manifest.owner_identity_key == ctx.user.dht_key(),
        "Conversation bootstrap owner does not match its account"
    );
    ensure!(
        conv.roster.members.iter().any(|member| {
            member.identity_key == ctx.user.dht_key() && member.role == MemberRole::Owner
        }),
        "Conversation bootstrap owner is missing from its roster"
    );
    Ok(())
}

async fn publish_initial_state(
    manifest_key: &RecordKey,
    ctx: &IdentityContext,
    conv: &LocalConversation,
) -> Result<Option<ConversationKeyRing>> {
    let owner = conv_owner_keypair(&ctx.keypair, conv.manifest.created_at).await?;
    let conv_key = conv.conv_secret();
    store::with_record(manifest_key, Some(&owner), |key| async move {
        // The public key-ring envelope is part of the atomic initial commit.
        // Its presence means replay must not reset the roster or member feed.
        if let Some(bytes) =
            store::load_unencrypted(&key, ConversationKeyRing::SUBKEY, true).await?
        {
            let existing: ConversationKeyRing =
                spawn_blocking(move || from_automerge(&bytes)).await??;
            ensure!(
                existing.owner_identity_key == ctx.user.dht_key(),
                "Published conversation key ring has a different owner"
            );
            let entry = existing
                .find_entry(ctx.user.dht_key())
                .context("Published conversation key ring is missing its owner")?;
            ensure!(
                unwrap_conversation_key(&entry.wrapped_key, &ctx.keypair).await? == conv_key,
                "Published conversation key differs from saved bootstrap state"
            );
            return Ok(Some(existing));
        }
        let manifest = conv.manifest.clone();
        let roster = conv.roster.clone();
        let key_ring = conv.key_ring.clone();
        let my_feed_meta = conv.my_feed_meta.clone();
        let (manifest_bytes, roster_bytes, key_ring_bytes, feed_meta_bytes) =
            spawn_blocking(move || {
                let manifest_bytes = reconcile_to_automerge(None, &manifest)?;
                let roster_bytes = reconcile_to_automerge(None, &roster)?;
                let key_ring_bytes = reconcile_to_automerge(None, &key_ring)?;
                let feed_meta_bytes = reconcile_to_automerge(None, &my_feed_meta)?;
                Ok::<_, Error>((
                    manifest_bytes,
                    roster_bytes,
                    key_ring_bytes,
                    feed_meta_bytes,
                ))
            })
            .await??;
        let member_index = conv
            .roster
            .my_member_index(ctx.user.dht_key(), false)
            .context("Conversation owner is missing from its member slots")?;
        store::with_transaction(vec![key.clone()], |tx| async move {
            ensure!(
                tx.get_raw(&key, ConversationKeyRing::SUBKEY)
                    .await?
                    .is_none(),
                "Conversation was initialized concurrently; retry without resetting it"
            );
            tx.set(
                &key,
                ConversationManifest::SUBKEY,
                manifest_bytes,
                &conv_key,
                None,
            )
            .await?;
            tx.set(
                &key,
                ConversationRoster::SUBKEY,
                roster_bytes,
                &conv_key,
                None,
            )
            .await?;
            tx.set_unencrypted(&key, ConversationKeyRing::SUBKEY, key_ring_bytes)
                .await?;
            tx.set(
                &key,
                member_feed_subkey(member_index),
                feed_meta_bytes,
                &conv_key,
                Some(&ctx.keypair),
            )
            .await?;
            Ok(())
        })
        .await?;
        Ok(None)
    })
    .await
}
