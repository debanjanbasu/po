//! Receiver-side invite flow: incoming invite signals, session restore of
//! queued invites, and user accept/reject actions.

use std::mem;

use anyhow::{Context, Result};
use tokio::sync::OwnedMutexGuard;
use tracing::{info, warn};
use veilid_core::RecordKey;

use crate::{
    chat::{
        contact::{ContactList, ContactStatus},
        conversation::ConversationIndex,
        storage::{
            delete_pending_invite_for_account, load_pending_invites_for_account,
            store_pending_invite_for_account,
        },
    },
    helpers::{async_utils::now_epoch_millis, store_lock::lock_store},
    session::{epoch::SessionGuard, phase::SESSION},
    storage::{background_errors::report_nonfatal, store_trait::load_local},
    user::ctx::UserCtx,
};

use super::{
    processing::{InviteProcessOutcome, is_known_contact, process_invite, send_invite_ack_for_ctx},
    types::{
        PENDING_INVITES, PendingInvite, deferred_pending_invites, insert_pending_invite_signal,
        pending_invite_snapshot, push_deferred_pending_invite, remove_pending_invite_signal,
        restore_deferred_pending_invites,
    },
};

async fn pending_invite_lock(ctx: &UserCtx) -> OwnedMutexGuard<()> {
    lock_store(format!("chat-pending-invites\0{}", ctx.account_id).as_bytes()).await
}

async fn clear_stored_pending_invite(
    ctx: &UserCtx,
    guard: SessionGuard,
    manifest_key_str: &str,
) -> Result<()> {
    let _lock = pending_invite_lock(ctx).await;
    delete_pending_invite_for_account(&ctx.account_id, manifest_key_str).await?;
    guard.commit(|| remove_pending_invite_signal(manifest_key_str));
    Ok(())
}

pub fn clear_pending_invites() {
    PENDING_INVITES.write().clear();
    // Deferred entries may belong to another locally pending account. Keep
    // them account-bound across logout/account switches for its next admission.
}

pub async fn restore_pending_invites_for_current_session() -> Result<()> {
    let Some((ctx, guard)) = UserCtx::try_current_with_guard() else {
        return Ok(());
    };
    let projection_lock = pending_invite_lock(&ctx).await;

    let deferred = {
        let now = now_epoch_millis();
        let mut invites = deferred_pending_invites();
        invites.retain(|pending| super::types::invite_is_recent(pending.invite.received_at, now));
        mem::take(&mut *invites)
    };
    let (matching, mut retained): (Vec<_>, Vec<_>) = deferred.into_iter().partition(|pending| {
        pending
            .account_id
            .as_deref()
            .is_none_or(|id| id == ctx.account_id.as_ref())
    });
    for (index, pending) in matching.iter().enumerate() {
        let invite = &pending.invite;
        if let Err(e) = store_pending_invite_for_account(
            &ctx.account_id,
            &invite.manifest_key_str,
            &invite.inviter_key,
            invite.received_at,
        )
        .await
        {
            retained.extend_from_slice(&matching[index..]);
            restore_deferred_pending_invites(retained);
            return Err(e);
        }
    }
    restore_deferred_pending_invites(retained);

    let existing_index: ConversationIndex = load_local(ctx.storage_key())
        .await
        .context("Failed to load ConversationIndex while restoring pending invites")?;
    let contacts: ContactList = load_local(ctx.storage_key())
        .await
        .context("Failed to load ContactList while restoring pending invites")?;

    let mut restored = Vec::new();
    for invite in load_pending_invites_for_account(&ctx.account_id).await? {
        if existing_index
            .conversations
            .contains_key(&invite.manifest_key_str)
            && contacts.contacts.values().any(|contact| {
                contact.status == ContactStatus::Active
                    && contact.conversation_record_key.as_ref() == invite.manifest_key_str
            })
        {
            if let Err(e) =
                delete_pending_invite_for_account(&ctx.account_id, &invite.manifest_key_str).await
            {
                report_nonfatal(
                    "chat.invite",
                    "Failed to clear stale accepted invite during session restore",
                    format!("{}: {e}", invite.manifest_key_str),
                );
            }
            continue;
        }
        restored.push(invite);
    }
    restored.sort_by_key(|invite| invite.received_at);
    guard.commit(|| PENDING_INVITES.write().clone_from(&restored));
    drop(projection_lock);
    for invite in &restored {
        if !guard.is_current() {
            break;
        }
        send_invite_ack_for_ctx(&ctx, &invite.manifest_key, &invite.inviter_key).await;
    }

    Ok(())
}

pub async fn handle_conversation_invite(
    manifest_key: RecordKey,
    manifest_key_str: &str,
    inviter_key: RecordKey,
) -> Result<()> {
    let invite = PendingInvite {
        manifest_key: manifest_key.clone(),
        manifest_key_str: manifest_key_str.to_string(),
        inviter_key: inviter_key.clone(),
        received_at: now_epoch_millis(),
    };
    let Some((session_ctx, guard)) = UserCtx::try_current_with_guard() else {
        info!("Invite queued until a session is ready: {manifest_key_str}");
        // Signals received while a second-factor gate is active still belong to
        // that pending identity. Before any identity is loaded the signal route
        // does not expose an owning account, so those rare entries remain unbound.
        let account_id =
            SESSION.with(|phase| phase.user().map(|user| user.account_id().to_owned()));
        push_deferred_pending_invite(account_id, invite);
        return Ok(());
    };

    let known_contact = session_ctx.has_identity()
        && session_ctx.keypair.is_some()
        && is_known_contact(&session_ctx, &inviter_key.to_string()).await;
    {
        let _lock = pending_invite_lock(&session_ctx).await;
        store_pending_invite_for_account(
            &session_ctx.account_id,
            manifest_key_str,
            &inviter_key,
            invite.received_at,
        )
        .await?;
        if !known_contact {
            guard.commit(|| insert_pending_invite_signal(invite.clone()));
        }
    }
    send_invite_ack_for_ctx(&session_ctx, &manifest_key, &inviter_key).await;

    if !guard.is_current() {
        info!("Invite stored until the authenticated session is ready: {manifest_key_str}");
        return Ok(());
    }
    if !known_contact {
        info!("Invite awaiting consent or identity readiness: {manifest_key_str}");
        return Ok(());
    }

    let ctx = session_ctx;
    match process_invite(&ctx, guard, manifest_key, manifest_key_str, &inviter_key).await {
        Ok(InviteProcessOutcome::Accepted) => {
            if let Err(e) = clear_stored_pending_invite(&ctx, guard, manifest_key_str).await {
                report_nonfatal(
                    "chat.invite",
                    "Failed to clear processed invite after auto-accept",
                    format!("{manifest_key_str}: {e}"),
                );
            }
            Ok(())
        }
        Ok(InviteProcessOutcome::Preserved) => {
            info!("Invite acceptance preserved for retry after session change: {manifest_key_str}");
            Ok(())
        }
        Err(e) => {
            warn!("Invite processing failed for {manifest_key_str}: {e}");
            let _lock = pending_invite_lock(&ctx).await;
            if load_pending_invites_for_account(&ctx.account_id)
                .await?
                .iter()
                .any(|pending| pending.manifest_key_str == manifest_key_str)
            {
                guard.commit(|| insert_pending_invite_signal(invite));
            }
            Err(e)
        }
    }
}

/// Accepts a pending invite by manifest key. Safer than index-based access
/// when the invite list can mutate between renders.
pub async fn accept_pending_invite_by_key(manifest_key_str: &str) -> Result<()> {
    let invite = pending_invite_snapshot(manifest_key_str)
        .with_context(|| format!("Pending invite not found for key {manifest_key_str}"))?;
    let (ctx, guard) = UserCtx::try_current_with_guard().context("Not logged in")?;
    let admitted = process_invite(
        &ctx,
        guard,
        invite.manifest_key,
        manifest_key_str,
        &invite.inviter_key,
    )
    .await?;
    if admitted == InviteProcessOutcome::Accepted {
        clear_stored_pending_invite(&ctx, guard, manifest_key_str)
            .await
            .context("Failed to clear pending invite after acceptance")?;
    }
    Ok(())
}

/// Rejects (dismisses) a pending invite by manifest key.
pub async fn reject_pending_invite_by_key(manifest_key_str: &str) -> Result<()> {
    let (ctx, guard) = UserCtx::try_current_with_guard().context("Not logged in")?;
    clear_stored_pending_invite(&ctx, guard, manifest_key_str)
        .await
        .context("Failed to dismiss pending invite")
}
