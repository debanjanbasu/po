//! Chat-specific DHT change handling for SMPL conversation records.
//!
//! Handles `ValueChange` events for conversation records. Owner subkeys
//! (0–2) trigger manifest/roster/key-ring updates. Member subkeys
//! trigger feed-meta + head-page refresh, with targeted archive backfill
//! handed off to background hydration when feed metadata indicates gaps.
//! See also: docs/watch-system.md.

use std::sync::Arc;

use veilid_core::ValueSubkeyRangeSet;

use crate::{
    chat::{manifest::ConversationRoster, record::full_conversation_subkeys, storage},
    session::epoch as session,
    user::ctx::UserCtx,
    veilid::{core::parse_record_key, store::PushedValue},
};

mod member;
mod updates;

use updates::{OwnerFlags, load_conversation_updates, member_changes_for_subkeys};

async fn handle_conversation_change_inner(
    ctx: UserCtx,
    manifest_key_str: &str,
    conv: &storage::LocalConversation,
    changed_subkeys: ValueSubkeyRangeSet,
    pushed: Option<PushedValue>,
    guard: session::SessionGuard,
) -> bool {
    if !guard.is_current() {
        return false;
    }

    let Ok(manifest_key) = parse_record_key(manifest_key_str) else {
        return false;
    };
    let secret = conv.conv_secret();
    let is_full_resync = changed_subkeys.is_empty();
    let effective_subkeys = if is_full_resync {
        full_conversation_subkeys(conv.roster.members.len())
    } else {
        changed_subkeys
    };

    let roster_changed = effective_subkeys.contains(ConversationRoster::SUBKEY);
    let owner = OwnerFlags::from_subkeys(&effective_subkeys);

    let member_changes = if is_full_resync || roster_changed {
        // The merged loader fetches owner+member docs in one scope, so the
        // remote roster isn't decoded yet here; use the local roster length
        // for member selection and let apply reconcile differences.
        (0..conv.roster.members.len())
            .map(|member_index| (member_index, true))
            .collect()
    } else {
        member_changes_for_subkeys(&effective_subkeys)
    };

    let combined = load_conversation_updates(
        &manifest_key,
        manifest_key_str,
        &secret,
        owner,
        &member_changes,
        pushed,
    )
    .await;
    if !guard.is_current() {
        return false;
    }

    let effective_roster = combined
        .owner
        .roster
        .clone()
        .unwrap_or_else(|| conv.roster.clone());

    member::apply_member_updates_batch(
        &ctx.account_id,
        manifest_key_str,
        &effective_roster,
        &secret,
        Some(ctx.dht_key.as_ref()),
        guard,
        (combined.owner, combined.member),
    )
    .await
}

/// Returns whether the merge changed local state for `changed_subkeys`.
///
/// `pushed` is the value the `ValueChange` delivered, when it delivered one:
/// the subkey it names is then decoded from the event instead of being read
/// back over the network.
pub async fn handle_conversation_change_loaded(
    ctx: UserCtx,
    manifest_key_str: &str,
    conv: &storage::LocalConversation,
    changed_subkeys: ValueSubkeyRangeSet,
    pushed: Option<PushedValue>,
    guard: session::SessionGuard,
) -> bool {
    if !conv.has_conv_key() {
        return false;
    }
    handle_conversation_change_inner(ctx, manifest_key_str, conv, changed_subkeys, pushed, guard)
        .await
}

/// Handle `ValueChange` for a conversation SMPL record.
///
/// Dispatches by subkey: owner subkeys (0–2) for manifest/roster/key-ring
/// changes, member subkeys for feed meta + head page changes. `pushed` is the
/// value the change delivered, if any.
pub async fn handle_conversation_change(
    manifest_key_str: Arc<str>,
    changed_subkeys: ValueSubkeyRangeSet,
    pushed: Option<PushedValue>,
    guard: session::SessionGuard,
) {
    let Some(ctx) = UserCtx::try_current() else {
        return;
    };
    if !guard.is_current() {
        return;
    }

    let conv = match storage::load_for_account(&ctx.account_id, &manifest_key_str).await {
        Ok(conv) if conv.has_conv_key() => conv,
        _ => return,
    };
    if !guard.is_current() {
        return;
    }

    handle_conversation_change_inner(
        ctx,
        &manifest_key_str,
        &conv,
        changed_subkeys,
        pushed,
        guard,
    )
    .await;
}
