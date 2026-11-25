//! Send-message orchestrator: commit locally, acknowledge the saved draft,
//! then hand off to the durable delivery queue.

use std::sync::Arc;

use anyhow::Result;
use dioxus::{
    core::spawn_forever,
    logger::tracing::{debug, warn},
    prelude::*,
};
use tokio::sync::oneshot;

use crate::{
    chat::{
        contact::ContactList,
        conversation,
        feed::{FeedMessage, MessageStatus, generate_msg_id, insert_sorted_message},
        manifest::MemberStatus,
        mark_message_status, record,
        signal::{self, PeerSignal, SignalPayload, signal_all_peers},
        storage,
    },
    compression::{samples::collect_chat, training_queue::extract_chat_pattern},
    config,
    helpers::{async_utils::now_epoch_millis, spawn_blocking},
    models::pending_operation::{OpType, PendingOperation},
    session::epoch::SessionGuard,
    storage::{background_errors::report_nonfatal, store_trait},
    user::ctx::UserCtx,
};

use super::SendGuard;

/// Enqueue feed sync and signal peers if successful.
async fn enqueue_and_signal(
    ctx: &UserCtx,
    ck: &str,
    message_id: &str,
    my_member_subkey: u32,
    peers: &[String],
    guard: SessionGuard,
) -> Result<()> {
    ctx.enqueue_for_target(
        OpType::ConvFeedSync,
        PendingOperation::conv_feed_payload([message_id]),
        ck,
    )
    .await?;

    if guard.is_current()
        && let Some((conv_key, sender_identity)) = signal::conv_sender_keys(ck)
    {
        signal_all_peers(
            peers,
            &PeerSignal::unsigned(SignalPayload::NewData {
                conv_key,
                sender_identity,
                member_subkey: my_member_subkey,
            }),
        );
    }
    Ok(())
}

/// Records message content as a compression-training sample. Lives here
/// beside its only caller — this is telemetry, not a data-model concern.
async fn record_chat_sample(ctx: &UserCtx, content: Arc<str>) {
    let username = Arc::clone(&ctx.username);
    let account_id = Arc::clone(&ctx.account_id);
    if let Err(error) = spawn_blocking(move || {
        let pattern = extract_chat_pattern(&username, &content);
        collect_chat(&account_id, pattern);
    })
    .await
    {
        report_nonfatal("chat.hooks", "Failed to record chat sample", error);
    }
}

/// Reasons a send was blocked before any message was constructed.
enum SendBlock {
    NoKey,
    NotInRoster,
    TooLong,
    /// The conversation already holds `MAX_LOCAL_MESSAGES` undelivered
    /// messages — see `save_and_queue_message`'s pending check.
    TooManyPending,
}

impl SendBlock {
    /// Stable word for the `deep_link_send` marker.
    const fn reason(&self) -> &'static str {
        match self {
            Self::NoKey => "no_key",
            Self::NotInRoster => "not_in_roster",
            Self::TooLong => "too_long",
            Self::TooManyPending => "too_many_pending",
        }
    }
}

enum SendOutcome {
    Saved,
    NotQueued,
    Blocked(SendBlock),
    Canceled,
}

/// Report a blocked send (no conversation key / sender missing from roster) to the user.
fn report_send_blocked(
    blocked: &SendBlock,
    ck: &str,
    guard: SendGuard,
    send_error: &mut Signal<Option<String>>,
) {
    let user_msg = match blocked {
        SendBlock::NoKey => {
            warn!("Cannot send message: conversation key not initialized for {ck}");
            "Conversation key is not ready yet. Please wait and try again.".to_string()
        }
        SendBlock::NotInRoster => {
            warn!("Cannot send NewData nudge: sender not found in roster for {ck}");
            "Unable to send in this conversation right now. Please reopen it and try again."
                .to_string()
        }
        SendBlock::TooLong => {
            warn!("Cannot send message: exceeds config::chat::MAX_MESSAGE_BYTES for {ck}");
            format!(
                "Message is too long to send (limit {} bytes).",
                config::chat::MAX_MESSAGE_BYTES
            )
        }
        SendBlock::TooManyPending => {
            warn!(
                "Cannot send message: {ck} already holds {} unsent messages",
                config::chat::MAX_LOCAL_MESSAGES
            );
            format!(
                "This conversation has {} unsent messages and cannot hold more. Reconnect so they can be delivered, then send again.",
                config::chat::MAX_LOCAL_MESSAGES
            )
        }
    };
    if guard.live(ck) {
        send_error.set(Some(user_msg));
    }
}

async fn save_and_queue_message(
    ctx: UserCtx,
    ck: String,
    guard: SessionGuard,
    text: Arc<str>,
) -> Result<SendOutcome> {
    if text.len() > config::chat::MAX_MESSAGE_BYTES {
        return Ok(SendOutcome::Blocked(SendBlock::TooLong));
    }
    let mut blocked = None;
    let mut out: Option<(FeedMessage, u32, Vec<String>)> = None;
    let mutated = storage::mutate_for_account(&ctx.account_id, &ck, |conv| {
        if !guard.is_current() {
            return false;
        }
        if !conv.has_conv_key() {
            blocked = Some(SendBlock::NoKey);
            return false;
        }
        let peers = conv.roster.peer_identity_keys(&ctx.dht_key);
        let Some(my_member_subkey) = conv
            .roster
            .members
            .iter()
            .position(|member| {
                member.identity_key == ctx.dht_key.as_ref() && member.status == MemberStatus::Active
            })
            .map(record::member_feed_subkey)
        else {
            blocked = Some(SendBlock::NotInRoster);
            return false;
        };

        // The one send-path growth with no ceiling. `trim_conv_history` never
        // drops a `Pending` message — it is undelivered, so neither this
        // conversation's head page nor an archive holds a copy to re-merge it
        // from — so an offline queue grew without bound, in memory and on disk
        // (the audit's realistic figure was tens of MB; the worst case is
        // thousands of 16 KB messages). Bounding it at the same constant that
        // bounds everything else a conversation retains gives that constant one
        // meaning instead of two: a row holds at most `MAX_LOCAL_MESSAGES`
        // recent messages plus at most `MAX_LOCAL_MESSAGES` undelivered ones.
        //
        // Only the send path grows that set: the retry paths flip an existing
        // message's status (`delivery`, `dead_letter`, `queue::failure`), and
        // peer pages arrive `Sent`. Reaching the cap needs thousands of
        // messages typed into an offline chat, so refusing the next one is an
        // honest answer rather than something normal use can hit.
        let pending = conv
            .messages
            .iter()
            .filter(|message| message.status == MessageStatus::Pending)
            .count();
        if pending >= config::chat::MAX_LOCAL_MESSAGES {
            blocked = Some(SendBlock::TooManyPending);
            return false;
        }

        let next_seq = conv.my_feed_meta.last_seq + 1;
        let timestamp = now_epoch_millis();
        let msg = FeedMessage {
            seq: next_seq,
            sender_username: Arc::clone(&ctx.username),
            content: Arc::clone(&text),
            timestamp,
            msg_id: generate_msg_id(),
            status: MessageStatus::Pending,
        };
        insert_sorted_message(Arc::make_mut(&mut conv.messages), msg.clone());
        conv.my_feed_meta.last_seq = next_seq;
        conv.my_feed_meta.last_message_at = timestamp;

        out = Some((msg, my_member_subkey, peers));
        true
    })
    .await?;
    if !mutated {
        return Ok(blocked.map_or(SendOutcome::Canceled, SendOutcome::Blocked));
    }
    let Some((msg, my_member_subkey, peers)) = out else {
        return Ok(SendOutcome::Canceled);
    };
    if guard.is_current() {
        record_chat_sample(&ctx, Arc::clone(&text)).await;
    }
    let outcome = match enqueue_and_signal(&ctx, &ck, &msg.msg_id, my_member_subkey, &peers, guard)
        .await
    {
        Ok(()) => {
            // The feed op is queued: the message has left this device.
            config::e2e::event!("message_sent", conversation = %ck, msg_id = %msg.msg_id);
            SendOutcome::Saved
        }
        Err(error) => {
            report_nonfatal("chat.hooks", "Failed to enqueue conv feed sync", error);
            if let Err(error) =
                mark_message_status(&ctx.account_id, &ck, &msg.msg_id, MessageStatus::Failed).await
            {
                report_nonfatal(
                    "chat.hooks",
                    "Failed to persist undelivered message status",
                    error,
                );
            }
            SendOutcome::NotQueued
        }
    };
    if let Err(error) = conversation::update_conversation_preview_for_account(
        &ctx,
        &ck,
        text.as_ref(),
        msg.timestamp,
        None,
    )
    .await
    {
        report_nonfatal(
            "chat.hooks",
            "Failed to update saved message preview",
            error,
        );
    }
    Ok(outcome)
}

/// Emits the seam's own marker, always: an accepted=… line names the reason a
/// refusal happened instead of leaving a scenario to time out.
fn emit_deep_link_send(contact: &str, conversation: &str, accepted: bool, reason: &str) {
    config::e2e::event!(
        "deep_link_send",
        contact = %contact,
        conversation = %conversation,
        accepted = accepted,
        reason = %reason
    );
}

/// Deep-link send seam: resolves `contact` in the local contact list and runs
/// the ordinary send path with no conversation view open — the send channel a
/// surface with no typing channel drives from `po://send?…`
/// ([`crate::platform::deep_link`]).
///
/// Harness-only in effect: the deep link that reaches it is refused unless
/// `config::e2e::enabled()` is on.
pub async fn send_from_deep_link(contact: &str, text: Arc<str>) {
    let Some(ctx) = UserCtx::try_current() else {
        emit_deep_link_send(contact, "-", false, "no_session");
        return;
    };
    let conversation_key = match store_trait::load_local::<ContactList>(ctx.storage_key()).await {
        Ok(contact_list) => contact_list
            .contacts
            .values()
            .find(|entry| entry.username.as_ref() == contact)
            .map(|entry| entry.conversation_record_key.to_string())
            .filter(|key| !key.is_empty()),
        Err(error) => {
            report_nonfatal(
                "chat.hooks",
                "Failed to load contacts for a deep-link send",
                error,
            );
            None
        }
    };
    let Some(ck) = conversation_key else {
        emit_deep_link_send(contact, "-", false, "no_conversation");
        return;
    };
    match save_and_queue_message(ctx, ck.clone(), SessionGuard::capture(), text).await {
        Ok(SendOutcome::Saved) => emit_deep_link_send(contact, &ck, true, "saved"),
        Ok(SendOutcome::NotQueued) => emit_deep_link_send(contact, &ck, false, "not_queued"),
        Ok(SendOutcome::Blocked(blocked)) => {
            emit_deep_link_send(contact, &ck, false, blocked.reason());
        }
        Ok(SendOutcome::Canceled) => emit_deep_link_send(contact, &ck, false, "canceled"),
        Err(error) => {
            report_nonfatal("chat.hooks", "Failed to save a deep-link message", error);
            emit_deep_link_send(contact, &ck, false, "save_failed");
        }
    }
}

/// The owned local command survives navigation; only its result receiver
/// touches component state. Network signaling remains background work.
pub(super) async fn do_send_message(
    ck: &str,
    identity: Option<UserCtx>,
    guard: SendGuard,
    on_saved: Callback<Arc<str>>,
    send_error: &mut Signal<Option<String>>,
    text: Arc<str>,
) {
    let Some(ctx) = identity else {
        return;
    };
    let (sender, receiver) = oneshot::channel();
    let key = ck.to_owned();
    let command_text = Arc::clone(&text);
    let session_guard = guard.session;
    spawn_forever(async move {
        let result = save_and_queue_message(ctx, key, session_guard, command_text).await;
        if let Err(error) = &result {
            report_nonfatal("chat.hooks", "Failed to save message locally", error);
        }
        if sender.send(result).is_err() {
            debug!("Message command completed after its conversation view closed");
        }
    });

    let outcome = receiver.await;
    if !guard.live(ck) {
        return;
    }
    match outcome {
        Ok(Ok(SendOutcome::Saved)) => on_saved.call(text),
        Ok(Ok(SendOutcome::NotQueued)) => {
            on_saved.call(text);
            send_error.set(Some(
                "Message was saved, but delivery could not be queued. Check its status before retrying."
                    .to_string(),
            ));
        }
        Ok(Ok(SendOutcome::Blocked(blocked))) => {
            report_send_blocked(&blocked, ck, guard, send_error);
        }
        Ok(Ok(SendOutcome::Canceled)) => send_error.set(Some(
            "Message send canceled. Your draft is unchanged.".to_string(),
        )),
        Ok(Err(_)) => send_error.set(Some(
            "Message could not be saved. Your draft is unchanged.".to_string(),
        )),
        Err(error) => {
            report_nonfatal(
                "chat.hooks",
                "Message command stopped before reporting its result",
                error,
            );
            send_error.set(Some(
                "Save status could not be confirmed. Check the conversation before retrying."
                    .to_string(),
            ));
        }
    }
}
