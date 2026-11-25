//! Outgoing delivery-status updates and retry of the feed holding a failed message.

use std::{collections::HashSet, sync::Arc};

use anyhow::{Context, Result, ensure};
use dioxus::logger::tracing::info;

use crate::{
    chat::{feed::MessageStatus, storage},
    config::e2e,
    models::pending_operation::{OpType, PendingOperation},
    user::ctx::UserCtx,
};

/// Updates delivery state for outgoing messages covered by a feed operation.
pub async fn mark_delivery_status(
    account_id: &str,
    conversation_key: &str,
    message_ids: &[&str],
    from: MessageStatus,
    to: MessageStatus,
) -> Result<()> {
    let message_ids: HashSet<_> = message_ids.iter().copied().collect();
    // A transition to `Sent` is the sync queue confirming the feed op landed on
    // the network, i.e. delivery to peers.
    let report_delivered = e2e::enabled() && to == MessageStatus::Sent;
    storage::mutate_for_account(account_id, conversation_key, |conv| {
        let mut changed = false;
        for message in Arc::make_mut(&mut conv.messages) {
            if message_ids.contains(&message.msg_id.as_ref()) && message.status == from {
                message.status = to;
                changed = true;
                if report_delivered {
                    info!(target: "e2e", event = "message_delivered", conversation = %conversation_key, msg_id = %message.msg_id);
                }
            }
        }
        changed
    })
    .await?;
    Ok(())
}

pub async fn mark_message_status(
    account_id: &str,
    conversation_key: &str,
    message_id: &str,
    status: MessageStatus,
) -> Result<()> {
    let mut found = false;
    storage::mutate_for_account(account_id, conversation_key, |conv| {
        let Some(message) = Arc::make_mut(&mut conv.messages)
            .iter_mut()
            .find(|message| message.msg_id.as_ref() == message_id)
        else {
            return false;
        };
        found = true;
        message.status = status;
        true
    })
    .await?;
    ensure!(found, "Message not found");
    Ok(())
}

/// Retries the conversation feed containing a failed message.
pub async fn retry_message_delivery(conversation_key: &str, message_id: &str) -> Result<()> {
    let ctx = UserCtx::try_current().context("Not signed in")?;
    let mut found = false;
    let mut wrong_sender = false;
    storage::mutate_for_account(&ctx.account_id, conversation_key, |conv| {
        let Some(message) = Arc::make_mut(&mut conv.messages)
            .iter_mut()
            .find(|message| message.msg_id.as_ref() == message_id)
        else {
            return false;
        };
        found = true;
        if message.sender_username != ctx.username {
            wrong_sender = true;
            return false;
        }
        message.status = MessageStatus::Pending;
        true
    })
    .await?;
    ensure!(found, "Message not found");
    ensure!(!wrong_sender, "Cannot retry another sender's message");

    if let Err(error) = ctx
        .enqueue_for_target(
            OpType::ConvFeedSync,
            PendingOperation::conv_feed_payload([message_id]),
            conversation_key,
        )
        .await
    {
        // Only undo the message selected for this retry. Other pending rows may
        // already belong to a live operation for the same conversation.
        mark_message_status(
            &ctx.account_id,
            conversation_key,
            message_id,
            MessageStatus::Failed,
        )
        .await?;
        return Err(error);
    }
    Ok(())
}
