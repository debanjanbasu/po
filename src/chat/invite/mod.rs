//! Conversation invite/join handlers.
//! See also: docs/peer-signaling.md.

mod processing;
mod receiver;
mod types;

use anyhow::Result;
use veilid_core::RecordKey;

use crate::{
    chat::signal::{PeerSignal, SignalPayload},
    models::pending_operation::{OpType, PendingOperation},
    sync::pending_ops,
};

/// The `(manifest_key, inviter_key)` carried by an invite op's payload.
///
/// Both are typed record keys decoded from the signed payload, so no
/// delimiter parsing is involved. Returns `None` when the payload is not a
/// well-formed invite.
pub(in crate::chat) fn invite_payload_keys(
    op: &PendingOperation,
) -> Option<(RecordKey, RecordKey)> {
    match PeerSignal::from_bytes(&op.payload).ok()?.payload {
        SignalPayload::ConversationInvite {
            manifest_key,
            inviter_key,
        } => Some((manifest_key, inviter_key)),
        _ => None,
    }
}

/// Matches a queued outbound-invite retry against its destination and payload.
pub(in crate::chat) fn matches_invite_retry(
    op: &PendingOperation,
    manifest_key: &RecordKey,
    inviter_key: &RecordKey,
    target_key: &str,
) -> bool {
    if op.op_type != OpType::ConvInviteSignal {
        return false;
    }
    // The op carries two typed keys: the recipient's record key and the
    // conversation's manifest key. They used to be one `"{peer}:{manifest}"`
    // composite parsed with `split_once(':')`, which truncated the peer key
    // at the colon inside the record key itself (`"<key>:<encryption_key>"`),
    // so the invite went to `"VLD0"`.
    if op.invite_peer_key() != Some(target_key) {
        return false;
    }
    // The signed payload carries the manifest and the inviter keys, so it is
    // the authoritative conversation binding — no second copy to compare and
    // no key string to allocate per scan.
    matches!(
        invite_payload_keys(op),
        Some((candidate_manifest, candidate_inviter))
            if &candidate_manifest == manifest_key && &candidate_inviter == inviter_key
    )
}

pub async fn handle_conversation_invite_ack(
    manifest_key: RecordKey,
    owner_key: RecordKey,
    ack_sender_key: RecordKey,
) -> Result<()> {
    let invitee_key_str = ack_sender_key.to_string();
    pending_ops::complete_matching(|op| {
        matches_invite_retry(op, &manifest_key, &owner_key, &invitee_key_str)
    })
    .await?;
    Ok(())
}

pub use receiver::{
    accept_pending_invite_by_key, clear_pending_invites, handle_conversation_invite,
    reject_pending_invite_by_key, restore_pending_invites_for_current_session,
};
pub use types::PENDING_INVITES;
