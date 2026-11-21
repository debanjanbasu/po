//! Core invite processing: resolve inviter key, hydrate local conversation state,
//! signal peers; stale sessions keep the durable pending invite for later retry.

use anyhow::{Context, Result, ensure};
use veilid_core::{RecordKey, bytes::Bytes};

use crate::{
    chat::{
        contact::{ChatContact, ContactList},
        conversation::ConversationIndex,
        manager,
        record::{self, hydrate_conversation_heads, spawn_archive_backfill},
        session_flow::{activate_contact_for_conversation, upsert_conversation_entry},
        signal::{self, PeerSignal, SignalPayload, signal_all_peers},
    },
    config,
    helpers::async_utils::now_epoch_millis,
    models::pending_operation::OpType,
    session::epoch as session,
    storage::{background_errors::report_nonfatal, store_trait::load_local},
    sync::mutate::{MutationResult, mutate},
    user::ctx::UserCtx,
    veilid::core::parse_record_key,
};

/// Result of processing an invite. Stale-session attempts keep the durable
/// pending invite so the next authenticated session can retry safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InviteProcessOutcome {
    Accepted,
    Preserved,
}

/// Saves local conversation/contact state after accepting an invite.
async fn save_invite_local_state(
    ctx: &UserCtx,
    manifest_key_str: &str,
    owner_username: &str,
    owner_identity_key: &str,
    title: &str,
) -> Result<()> {
    mutate::<ConversationIndex, ()>(ctx, |index| {
        if upsert_conversation_entry(index, manifest_key_str, owner_username, title, true) {
            Ok(MutationResult::modified(()))
        } else {
            Ok(MutationResult::unchanged(()))
        }
    })
    .await?;

    mutate::<ContactList, ()>(ctx, |list| {
        if let Some(contact) = list.contacts.get_mut(owner_identity_key) {
            if activate_contact_for_conversation(contact, manifest_key_str) {
                list.last_updated = now_epoch_millis();
                return Ok(MutationResult::modified(()));
            }
            return Ok(MutationResult::unchanged(()));
        }

        let mut contact = ChatContact::new(owner_username, owner_identity_key, owner_username);
        activate_contact_for_conversation(&mut contact, manifest_key_str);
        list.add_contact(contact);
        Ok(MutationResult::modified(()))
    })
    .await?;
    Ok(())
}

/// Checks whether `inviter_dht_key` is a known (active or pending) contact.
pub(super) async fn is_known_contact(ctx: &UserCtx, inviter_dht_key: &str) -> bool {
    let contacts: ContactList = match load_local(ctx.storage_key()).await {
        Ok(c) => c,
        Err(error) => {
            report_nonfatal(
                "chat.invite",
                "Failed to look up invite sender; keeping invite for manual review",
                error,
            );
            return false;
        }
    };
    contacts.find_by_dht_key(inviter_dht_key).is_some()
}

pub(super) async fn send_invite_ack_for_ctx(
    ctx: &UserCtx,
    manifest_key: &RecordKey,
    target_key: &RecordKey,
) {
    let Some(keypair) = ctx.keypair.as_ref() else {
        return;
    };
    let Ok(sender_key) = parse_record_key(ctx.dht_key.as_ref()) else {
        return;
    };
    let signal = PeerSignal::unsigned(SignalPayload::ConversationInviteAck {
        manifest_key: manifest_key.clone(),
        inviter_key: target_key.clone(),
        invitee_key: sender_key.clone(),
    });
    let target_key_str = target_key.to_string();
    signal::send_signal_as(&target_key_str, &signal, &sender_key, keypair).await;
}

/// Core invite processing — resolves key, hydrates conversation, signals peers.
///
/// `inviter_key` is the invite's authenticated sender (the signal proof pins
/// it); the conversation is only admitted when the hydrated manifest names that
/// same identity as its owner.
pub(super) async fn process_invite(
    ctx: &UserCtx,
    guard: session::SessionGuard,
    manifest_key: RecordKey,
    manifest_key_str: &str,
    inviter_key: &RecordKey,
) -> Result<InviteProcessOutcome> {
    let sender_identity =
        parse_record_key(&ctx.dht_key).context("Invalid local identity for invite processing")?;
    let hydrated = hydrate_conversation_heads(&manifest_key, manifest_key_str, ctx, guard).await?;

    // Hydration proves the record's writer slots back its roster; this proves
    // the roster's owner is who invited us, since a record's owner can write a
    // manifest naming anyone as its owner.
    let manifest_owner = parse_record_key(&hydrated.conv.manifest.owner_identity_key)
        .context("Conversation manifest owner is not a record key")?;
    ensure!(
        manifest_owner == *inviter_key,
        "Conversation owner is not the invite sender"
    );

    {
        let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
            return Ok(InviteProcessOutcome::Preserved);
        };
        ctx.enqueue_for_target(OpType::ConvFeedSync, Bytes::new(), manifest_key_str)
            .await
            .context("Failed to enqueue ConvFeedSync after invite")?;
    }

    if !guard.is_current() {
        return Ok(InviteProcessOutcome::Preserved);
    }

    let owner_username = hydrated
        .conv
        .roster
        .members
        .iter()
        .find(|member| member.identity_key == hydrated.conv.manifest.owner_identity_key)
        .map_or("Unknown", |member| member.username.as_str());

    // The sender is only nameable once the roster is hydrated, so the marker
    // lands here rather than at raw signal arrival.
    config::e2e::event!("invite_received", from = %owner_username, conversation = %manifest_key_str);

    save_invite_local_state(
        ctx,
        manifest_key_str,
        owner_username,
        &hydrated.conv.manifest.owner_identity_key,
        &hydrated.conv.manifest.title,
    )
    .await?;

    if !guard.is_current() {
        return Ok(InviteProcessOutcome::Preserved);
    }
    manager::ensure_watched(manifest_key_str).await;

    spawn_archive_backfill(
        guard,
        ctx.clone(),
        manifest_key_str.to_string(),
        hydrated.feed_metas,
        hydrated.conv.conv_secret(),
        move |result, manifest_key_str| {
            if let Err(error) = result {
                report_nonfatal(
                    "chat.record",
                    "Failed to backfill accepted conversation archives",
                    format!("{manifest_key_str}: {error}"),
                );
            }
        },
    );

    let my_feed_subkey = record::member_feed_subkey(hydrated.member_index);
    signal_all_peers(
        &hydrated.conv.roster.peer_identity_keys(&ctx.dht_key),
        &PeerSignal::unsigned(SignalPayload::NewData {
            conv_key: manifest_key,
            sender_identity,
            member_subkey: my_feed_subkey,
        }),
    );
    Ok(InviteProcessOutcome::Accepted)
}
