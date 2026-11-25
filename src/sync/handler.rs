//! Sync operation dispatch.
//!
//! Executes a [`PendingOperation`] by matching exhaustively on its
//! [`OpType`]. The compiler guarantees every operation type has a handler.

use anyhow::{Result, anyhow, ensure};

use crate::{
    chat::{
        record as chat_record, session_flow,
        signal::{self, PeerSignal},
        storage,
    },
    compression::codec,
    models::pending_operation::{OpType, PendingOperation},
    storage::background_errors::{ErrorSeverity, report_nonfatal_with_severity},
    sync::pending_ops::owns_queue_row,
    user::{
        ctx::UserCtx, identity_context::IdentityContext, password::prepare_identity_sync, sync_ops,
    },
    veilid::dict_sync,
};

/// Execute a pending operation.
///
/// When `lane_identity` is `Some`, the pre-resolved context is reused for
/// handlers that require an identity, avoiding redundant table-store lookups
/// and KDF derivations within the same lane. Ordinary publication holds the
/// account's identity-sync read lease across resolution and dispatch; migration
/// takes its own exclusive lease and refreshes its context inside it.
pub async fn dispatch(
    op: &PendingOperation,
    lane_identity: Option<&IdentityContext>,
) -> Result<()> {
    // Only `CreateIdentity` runs without an identity context — it is the
    // bootstrap op that *creates* one (see [`OpType::requires_identity`]).
    if op.op_type == OpType::CreateIdentity {
        return sync_ops::create_identity(op).await;
    }

    let resolved;
    let id = if let Some(ctx) = lane_identity {
        ctx
    } else {
        resolved = IdentityContext::for_user(&op.username).await?;
        &resolved
    };
    ensure!(
        id.user.account_id() == op.identity_key.as_ref()
            && id.user.username() == op.username.as_ref(),
        "Sync operation belongs to a deleted or replaced account"
    );
    if op.op_type != OpType::ReencryptSubkeys {
        prepare_identity_sync(id).await?;
    }

    match op.op_type {
        OpType::ProfileAndSecurity => sync_ops::sync_profile_and_security(id).await,
        OpType::ReencryptSubkeys => sync_ops::reencrypt_subkeys(op, id).await,
        OpType::DictPublish => {
            let result = dict_sync::publish_to_dht(&id.key, &id.keypair, &id.secret).await;
            if result.is_ok() {
                // Harness seam: this device's dictionary reached the network,
                // so the announcing side and any receiver converge on this
                // content digest.
                codec::report_dict_converged(codec::current_version());
            }
            result
        }
        OpType::ConvFeedSync => chat_record::sync_conv_feed(op, id).await,
        OpType::ConvInviteSignal => conv_invite_signal(op, id).await,
        OpType::ConvRecordCreate => conv_record_create(op, id).await,
        op_type => {
            let Some(result) = sync_ops::publish_registered_op(op_type, id).await else {
                return Err(anyhow!("no registered sync surface for {op_type:?}"));
            };
            result
        }
    }
}

// ── Operation handlers ─────────────────────────────────────────────────

/// Sentinel error: an invite signal was delivered but its ack is still pending.
///
/// The failure path matches on this type (not the message text) to retry a
/// delivered-but-unacked invite on its own wall-clock budget instead of the
/// invite delivery budget.
#[derive(Debug, thiserror::Error)]
#[error("invite awaiting ack")]
pub struct InviteAwaitingAck;

async fn conv_invite_signal(op: &PendingOperation, id: &IdentityContext) -> Result<()> {
    // The op's target is the recipient's record key and nothing else. It used
    // to be a `"{peer}:{manifest}"` composite that this handler split with
    // `split_once(':')` — but a Veilid record key contains colons itself
    // (`"<key>:<encryption_key>"`), so the split yielded the peer `"VLD0"`
    // and every invite went to a nonexistent identity.
    let target = op
        .invite_peer_key()
        .ok_or_else(|| anyhow!("invite op missing target_key"))?;
    let signal = match PeerSignal::from_bytes(&op.payload) {
        Ok(signal) => signal,
        Err(e) => {
            report_nonfatal_with_severity(
                "sync.handler",
                ErrorSeverity::Visible,
                "Dropping malformed conversation invite retry payload",
                format!("op_id={}: {e}. Clear local app data to recover.", op.id),
            );
            return Ok(());
        }
    };
    let delivered = signal::send_signal_as(target, &signal, &id.key, &id.keypair).await;
    if !owns_queue_row(op).await {
        return Ok(());
    }
    if delivered {
        Err(InviteAwaitingAck.into())
    } else {
        Err(anyhow!("peer offline or no route"))
    }
}

async fn conv_record_create(op: &PendingOperation, id: &IdentityContext) -> Result<()> {
    let manifest_key_str = chat_record::bootstrap_conversation_dht(op, id).await?;

    let conv = storage::load_for_account(id.user.account_id(), &manifest_key_str).await?;

    let my_dht_key = id.user.dht_key();
    let other = conv
        .roster
        .members
        .iter()
        .find(|m| m.identity_key != my_dht_key);
    if let Some(member) = other {
        // The stored user carries no runtime crypto, but the invite signature
        // needs the keypair the lane resolved.
        let mut user_ctx = UserCtx::from_user(&id.user);
        user_ctx.keypair = Some(id.keypair.clone());
        session_flow::finalize_conversation_bootstrap(
            &user_ctx,
            &manifest_key_str,
            &member.username,
            &member.identity_key,
        )
        .await?;
    }
    Ok(())
}
