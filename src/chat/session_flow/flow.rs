//! Contact-to-conversation bootstrap flow: pending-contact/invite-retry matching,
//! cancellation tracking, provisional-key cleanup, `finalize_conversation_bootstrap`.

use std::{
    collections::HashSet,
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::{Context, Result, anyhow, ensure};
use veilid_core::bytes::Bytes;

use crate::{
    chat::{
        contact::{ChatContact, ContactList, ContactStatus},
        conversation::{ConversationEntry, ConversationIndex},
        invite::matches_invite_retry,
        manager, record,
        signal::{PeerSignal, SignalPayload},
        storage,
    },
    config,
    helpers::{
        async_utils::now_epoch_millis,
        store_lock::{lock_store, recover_poison},
    },
    models::pending_operation::{OpType, PendingOperation},
    storage::store_trait::{self, CrdtModel},
    sync::{
        mutate::{MutationResult, mutate},
        pending_ops,
    },
    user::ctx::UserCtx,
    veilid::core::parse_record_key,
};

static CANCELED_CONTACT_WORK: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn canceled_contact_key(ctx: &UserCtx, dht_key_str: &str) -> String {
    format!("{}\0{dht_key_str}", ctx.account_id)
}

/// Drops every cancellation flag for `account_id`.
///
/// The flags guard in-flight bootstrap work, so they only need to outlive the
/// session that created them: callers advance the session epoch before their
/// async cleanup (see `session::phase::clear_session`), and an epoch-advanced
/// task can no longer run. A flag is also only ever consulted for a contact
/// that is still in the `ContactList` — `undo_add_contact` removes the contact
/// as it sets the flag, and `setup_conversation_local` requires the contact to
/// be present — so dropping them at teardown cannot resurrect an undone
/// contact's conversation. This is what bounds the set: without it the flags
/// live for the process, one ~150-byte entry per undo that is never re-added.
pub fn purge_canceled_contact_work_for_account(account_id: &str) {
    let prefix = format!("{account_id}\0");
    recover_poison(CANCELED_CONTACT_WORK.lock()).retain(|key| !key.starts_with(&prefix));
}

pub(super) fn clear_contact_work_cancellation(ctx: &UserCtx, dht_key_str: &str) {
    let key = canceled_contact_key(ctx, dht_key_str);
    recover_poison(CANCELED_CONTACT_WORK.lock()).remove(&key);
}

pub(super) fn mark_contact_work_canceled(ctx: &UserCtx, dht_key_str: &str) {
    let key = canceled_contact_key(ctx, dht_key_str);
    recover_poison(CANCELED_CONTACT_WORK.lock()).insert(key);
}

fn contact_work_canceled(ctx: &UserCtx, dht_key_str: &str) -> bool {
    let key = canceled_contact_key(ctx, dht_key_str);
    recover_poison(CANCELED_CONTACT_WORK.lock()).contains(&key)
}

fn provisional_conversation_key(ctx: &UserCtx, dht_key_str: &str) -> String {
    format!("provisional:{}:{dht_key_str}", ctx.account_id)
}

fn matches_contact_queue_work(
    op: &PendingOperation,
    ctx: &UserCtx,
    provisional_key: &str,
    dht_key_str: &str,
    manifest_key_str: Option<&str>,
) -> bool {
    if op.identity_key.as_ref() != ctx.account_id.as_ref() {
        return false;
    }
    match op.op_type {
        OpType::ConvRecordCreate => op.target_key.as_deref() == Some(provisional_key),
        OpType::ConvInviteSignal => {
            // The op's target is the recipient's record key, verbatim.
            let peer_matches = op.invite_peer_key() == Some(dht_key_str);
            if peer_matches {
                return true;
            }
            manifest_key_str.is_some_and(|manifest_key_str| {
                if op.payload == manifest_key_str.as_bytes() {
                    return true;
                }
                let (Ok(manifest_key), Ok(inviter_key)) = (
                    parse_record_key(manifest_key_str),
                    parse_record_key(ctx.dht_key.as_ref()),
                ) else {
                    return false;
                };
                matches_invite_retry(op, &manifest_key, &inviter_key, dht_key_str)
            })
        }
        OpType::ConvFeedSync => {
            manifest_key_str.is_some_and(|key| op.target_key.as_deref() == Some(key))
        }
        _ => false,
    }
}

pub(super) async fn cancel_contact_queue_work(
    ctx: &UserCtx,
    dht_key_str: &str,
    manifest_key_str: Option<&str>,
) -> Result<()> {
    let provisional_key = provisional_conversation_key(ctx, dht_key_str);
    pending_ops::complete_matching(|op| {
        matches_contact_queue_work(op, ctx, &provisional_key, dht_key_str, manifest_key_str)
    })
    .await?;
    Ok(())
}

async fn remove_conversation_entry(ctx: &UserCtx, manifest_key_str: &str) -> Result<()> {
    mutate::<ConversationIndex, ()>(ctx, |index| {
        if index.conversations.remove(manifest_key_str).is_some() {
            index.last_updated = now_epoch_millis();
            Ok(MutationResult::modified(()))
        } else {
            Ok(MutationResult::unchanged(()))
        }
    })
    .await?;

    Ok(())
}

pub(super) async fn cleanup_local_conversation_state(
    ctx: &UserCtx,
    dht_key_str: &str,
    manifest_key_str: Option<&str>,
) -> Result<()> {
    let provisional_key = provisional_conversation_key(ctx, dht_key_str);
    if UserCtx::try_current().is_some_and(|active| active.account_id == ctx.account_id) {
        manager::unwatch_conversation(&provisional_key).await;
    }
    storage::delete_for_account(ctx.account_id.as_ref(), &provisional_key).await?;

    if let Some(manifest_key_str) = manifest_key_str.filter(|key| !key.is_empty()) {
        remove_conversation_entry(ctx, manifest_key_str).await?;
        if manifest_key_str != provisional_key {
            if UserCtx::try_current().is_some_and(|active| active.account_id == ctx.account_id) {
                manager::unwatch_conversation(manifest_key_str).await;
            }
            storage::delete_for_account(ctx.account_id.as_ref(), manifest_key_str).await?;
        }
    }

    Ok(())
}

fn contact_ready_for_bootstrap(contact_list: &ContactList, dht_key_str: &str) -> bool {
    contact_list
        .find_by_dht_key(dht_key_str)
        .is_some_and(|contact| contact.conversation_record_key.is_empty())
}

pub(super) async fn setup_conversation_local(
    ctx: &UserCtx,
    username: &str,
    dht_key_str: &str,
) -> Result<()> {
    let provisional_key = provisional_conversation_key(ctx, dht_key_str);
    let _guard = lock_store(ctx.storage_key()).await;

    if contact_work_canceled(ctx, dht_key_str) {
        return Ok(());
    }

    let contact_list = store_trait::load_local::<ContactList>(ctx.storage_key()).await?;
    if !contact_ready_for_bootstrap(&contact_list, dht_key_str) {
        return Ok(());
    }

    let existing = storage::load_for_account(ctx.account_id.as_ref(), &provisional_key).await?;
    if !existing.has_conv_key() {
        let conv = record::build_local_conversation(
            ctx.username.as_ref(),
            ctx.dht_key.as_ref(),
            username,
            dht_key_str,
        )
        .await?;

        storage::save_for_account(ctx.account_id.as_ref(), &provisional_key, conv).await?;
    }

    ctx.enqueue_for_target(OpType::ConvRecordCreate, Bytes::new(), &provisional_key)
        .await?;

    Ok(())
}

/// Retries DHT conversation setup for a contact still in `Pending` state.
///
/// Re-enqueues a `ConvRecordCreate` op. If the provisional local conversation
/// still exists, the queue handler will bootstrap the DHT record. If a prior
/// attempt already completed, the handler reuses its committed contact pointer
/// or final conversation row instead of reinitializing the record.
pub async fn retry_pending_contact(contact: &ChatContact) -> Result<()> {
    ensure!(
        contact.conversation_record_key.is_empty(),
        "Contact already has a conversation record"
    );
    let ctx = UserCtx::try_current().context("Not logged in")?;
    ensure!(ctx.keypair.is_some(), "Account identity is not unlocked");
    clear_contact_work_cancellation(&ctx, &contact.dht_key);
    setup_conversation_local(&ctx, &contact.username, &contact.dht_key).await
}

/// Ensures `manifest_key_str` exists in `conv_index` with `(peer_username, title)`.
///
/// Returns `true` if `conv_index` was modified.
pub(in crate::chat) fn upsert_conversation_entry(
    conv_index: &mut ConversationIndex,
    manifest_key_str: &str,
    peer_username: &str,
    title: &str,
    update_existing: bool,
) -> bool {
    if let Some(entry) = conv_index.conversations.get_mut(manifest_key_str) {
        if !update_existing {
            return false;
        }
        let contact_changed = entry.contact_username != peer_username;
        let title_changed = entry.title != title;
        if contact_changed {
            peer_username.clone_into(&mut entry.contact_username);
        }
        if title_changed {
            title.clone_into(&mut entry.title);
        }
        if contact_changed || title_changed {
            conv_index.last_updated = now_epoch_millis();
        }
        return contact_changed || title_changed;
    }
    conv_index.add_entry(ConversationEntry::new(
        manifest_key_str,
        peer_username,
        title,
    ));
    true
}

/// Marks `contact` active and sets `conversation_record_key = manifest_key_str`.
///
/// Returns `true` if `contact` was modified.
pub(in crate::chat) fn activate_contact_for_conversation(
    contact: &mut ChatContact,
    manifest_key_str: &str,
) -> bool {
    let key_changed = &*contact.conversation_record_key != manifest_key_str;
    let status_changed = contact.status != ContactStatus::Active;
    if key_changed {
        contact.conversation_record_key = Arc::from(manifest_key_str);
    }
    if status_changed {
        contact.status = ContactStatus::Active;
        // The conversation just became usable for this peer.
        config::e2e::event!("conversation_active", conversation = %manifest_key_str, peer = %contact.username);
    }
    key_changed || status_changed
}

/// Called by the queue handler after [`record::bootstrap_conversation_dht`]
/// succeeds. Updates `ConversationIndex`, marks contact `Active`, and sends
/// the invite signal.
pub async fn finalize_conversation_bootstrap(
    ctx: &UserCtx,
    manifest_key_str: &str,
    other_username: &str,
    other_dht_key: &str,
) -> Result<()> {
    let finalized = {
        let _guard = lock_store(ctx.storage_key()).await;
        if contact_work_canceled(ctx, other_dht_key) {
            false
        } else {
            let mut contact_list =
                store_trait::load_local::<ContactList>(ctx.storage_key()).await?;
            if let Some(contact) = contact_list.contacts.get_mut(other_dht_key) {
                let mut conv_index =
                    store_trait::load_local::<ConversationIndex>(ctx.storage_key()).await?;
                if upsert_conversation_entry(
                    &mut conv_index,
                    manifest_key_str,
                    other_username,
                    other_username,
                    false,
                ) {
                    <ConversationIndex as CrdtModel>::persist_and_sync(&conv_index, ctx).await?;
                }
                if activate_contact_for_conversation(contact, manifest_key_str) {
                    <ContactList as CrdtModel>::persist_and_sync(&contact_list, ctx).await?;
                }
                true
            } else {
                false
            }
        }
    };

    if !finalized || contact_work_canceled(ctx, other_dht_key) {
        cleanup_local_conversation_state(ctx, other_dht_key, Some(manifest_key_str)).await?;
        return Ok(());
    }

    let manifest_key =
        parse_record_key(manifest_key_str).context("Invalid bootstrap manifest key")?;
    let my_identity_key =
        parse_record_key(ctx.dht_key.as_ref()).context("Invalid bootstrap inviter identity")?;
    if !contact_work_canceled(ctx, other_dht_key) {
        let signal = PeerSignal::unsigned(SignalPayload::ConversationInvite {
            manifest_key,
            inviter_key: my_identity_key,
        });
        enqueue_invite_retry(ctx, other_dht_key, manifest_key_str, &signal)
            .await
            .context("Failed to enqueue conversation invite")?;
        // The invite op is queued for this peer.
        config::e2e::event!("invite_sent", conversation = %manifest_key_str);
        if contact_work_canceled(ctx, other_dht_key) {
            cancel_contact_queue_work(ctx, other_dht_key, Some(manifest_key_str)).await?;
            cleanup_local_conversation_state(ctx, other_dht_key, Some(manifest_key_str)).await?;
            return Ok(());
        }
        if UserCtx::try_current().is_some_and(|c| c.account_id == ctx.account_id) {
            manager::ensure_watched(manifest_key_str).await;
        }
        storage::delete_for_account(
            &ctx.account_id,
            &provisional_conversation_key(ctx, other_dht_key),
        )
        .await?;
    }
    Ok(())
}

/// Enqueues a `ConvInviteSignal` op so the sync queue keeps retrying delivery
/// until the recipient acknowledges durable receipt.
///
/// The op carries two typed keys: the recipient's record key and the
/// conversation's manifest key. They used to be joined into one
/// `"{peer}:{manifest}"` string that callers recovered with
/// `split_once(':')` — but a Veilid record key contains colons of its own
/// (`"<key>:<encryption_key>"`), so that split truncated the peer key to
/// `"VLD0"` and the invite was addressed to nobody.
async fn enqueue_invite_retry(
    ctx: &UserCtx,
    other_dht_key: &str,
    manifest_key_str: &str,
    signal: &PeerSignal,
) -> Result<()> {
    let keypair = ctx
        .keypair
        .as_ref()
        .ok_or_else(|| anyhow!("invite retry requires local keypair"))?;
    ctx.enqueue_for_scoped_target(
        OpType::ConvInviteSignal,
        signal.to_signed_bytes(keypair).await?,
        other_dht_key,
        manifest_key_str,
    )
    .await
}
