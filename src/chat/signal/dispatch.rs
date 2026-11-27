//! `app_message` ingress: sender-proof identity derivation and dispatch to
//! authenticated conversation-signal and sync-nudge handlers.

use std::{
    num::NonZeroUsize,
    sync::{Arc, LazyLock, Mutex},
};

use dioxus::logger::tracing::{debug, warn};
use lru::LruCache;
use veilid_core::{DHTSchema, PublicKey, RecordKey, ValueSubkeyRangeSet};

use crate::{
    chat::{
        conversation::{apply_typing_from_peer, clear_typing_from_peer},
        invite::{handle_conversation_invite, handle_conversation_invite_ack},
        manager::{ensure_watched, is_open_conversation},
        manifest::MemberStatus,
        record, storage,
        watcher::handle_conversation_change_loaded,
    },
    helpers::{
        async_utils::{now_epoch_millis, retry_after_session_stable_delay},
        spawn,
        store_lock::recover_poison,
    },
    // Aliased: the sync-nudge handler also has a local `device_id` binding
    // (the sender's device id from the wire payload) that would shadow the fn.
    platform::device_id::device_id as current_device_id,
    session::epoch as session,
    storage::background_errors::{push_background_error, report_nonfatal},
    user::{ctx::UserCtx, identity_layout::IDENTITY_SUBKEY_COUNT, watcher::handle_sync_nudge},
    veilid::core::{get_veilid_api, parse_record_key},
};

use super::wire::{PeerSignal, SenderProof, SignalPayload};

struct AuthenticatedConversationSignal {
    ctx: UserCtx,
    conv: Arc<storage::LocalConversation>,
    conv_key_str: Arc<str>,
    sender_identity: Arc<str>,
}

const SENDER_IDENTITY_CACHE_CAP: NonZeroUsize = match NonZeroUsize::new(256) {
    Some(v) => v,
    None => unreachable!(),
};

static SENDER_IDENTITY_CACHE: LazyLock<Mutex<LruCache<PublicKey, RecordKey>>> =
    LazyLock::new(|| Mutex::new(LruCache::new(SENDER_IDENTITY_CACHE_CAP)));

async fn derive_sender_identity(proof: &SenderProof) -> Option<RecordKey> {
    let cached = recover_poison(SENDER_IDENTITY_CACHE.lock())
        .get(&proof.sender_public_key)
        .cloned();
    if let Some(record_key) = cached {
        return Some(record_key);
    }
    let Ok(schema) = DHTSchema::dflt(IDENTITY_SUBKEY_COUNT) else {
        warn!("Failed to build identity schema while verifying peer signal sender");
        return None;
    };
    let Ok(api) = get_veilid_api().await else {
        warn!("Failed to access Veilid API while verifying peer signal sender");
        return None;
    };
    match api
        .get_dht_record_key(schema, proof.sender_public_key.clone(), None)
        .await
    {
        Ok(record_key) => {
            recover_poison(SENDER_IDENTITY_CACHE.lock())
                .put(proof.sender_public_key.clone(), record_key.clone());
            Some(record_key)
        }
        Err(e) => {
            warn!("Failed to derive sender identity from proof: {e}");
            None
        }
    }
}

async fn verify_claimed_signal_identity(
    signal: &PeerSignal,
    claimed_identity: &RecordKey,
    subject_kind: &str,
    subject_id: &str,
) -> Option<RecordKey> {
    let Some(proof) = signal.sender_proof() else {
        warn!(
            "Rejected {} signal without sender proof for {subject_kind} {subject_id}",
            signal.kind_name(),
        );
        return None;
    };
    let Some(derived_sender_identity) = derive_sender_identity(proof).await else {
        warn!(
            "Rejected {} signal because sender identity could not be derived for {subject_kind} {subject_id}",
            signal.kind_name(),
        );
        return None;
    };
    if derived_sender_identity != *claimed_identity {
        warn!(
            "Rejected {} signal with mismatched sender identity {} for {subject_kind} {subject_id} (proof derived {})",
            signal.kind_name(),
            claimed_identity,
            derived_sender_identity,
        );
        return None;
    }
    match proof.verify(&signal.unsigned_bytes()).await {
        Ok(true) => Some(derived_sender_identity),
        Ok(false) => {
            warn!(
                "Rejected {} signal with invalid sender proof for {subject_kind} {subject_id}",
                signal.kind_name(),
            );
            None
        }
        Err(e) => {
            warn!(
                "Failed to verify {} sender proof for {subject_kind} {subject_id}: {e}",
                signal.kind_name(),
            );
            None
        }
    }
}

async fn authenticate_conversation_signal(
    conv_key_str: Arc<str>,
    sender_identity: RecordKey,
    member_subkey: Option<u32>,
) -> Option<AuthenticatedConversationSignal> {
    let guard = session::SessionGuard::capture();
    let ctx = UserCtx::try_current()?;

    let sender_identity_str: Arc<str> = Arc::from(sender_identity.to_string());
    let conv = match storage::load_for_account(&ctx.account_id, &conv_key_str).await {
        Ok(conv) if conv.has_conv_key() => conv,
        _ => return None,
    };
    if !guard.is_current() {
        return None;
    }

    let Some((member_index, member)) = conv
        .roster
        .members
        .iter()
        .enumerate()
        .find(|(_, member)| member.identity_key == *sender_identity_str)
    else {
        warn!(
            "Rejected signal from non-member {} for conversation {}",
            sender_identity_str, conv_key_str,
        );
        return None;
    };

    if !matches!(member.status, MemberStatus::Active) {
        warn!(
            "Rejected signal from inactive member {} for conversation {}",
            sender_identity_str, conv_key_str,
        );
        return None;
    }

    if let Some(member_subkey) = member_subkey {
        let feed_subkey = record::member_feed_subkey(member_index);
        let page_subkey = record::member_page_subkey(member_index);
        if member_subkey != feed_subkey && member_subkey != page_subkey {
            warn!(
                "Rejected signal with mismatched member_subkey {} from {} for conversation {}",
                member_subkey, sender_identity_str, conv_key_str,
            );
            return None;
        }
    }

    Some(AuthenticatedConversationSignal {
        ctx,
        conv,
        conv_key_str,
        sender_identity: sender_identity_str,
    })
}

fn spawn_authenticated_conversation_signal<F, Fut>(
    signal: PeerSignal,
    member_subkey: Option<u32>,
    require_open_conversation: bool,
    on_authenticated: F,
) where
    F: FnOnce(AuthenticatedConversationSignal, session::SessionGuard) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let conv_key_str: Arc<str> = Arc::from(signal.conversation_key().to_string());
    // Presence-style signals (typing) only matter for the conversation on
    // screen; data signals are handled for any known conversation since the
    // session watches them all.
    if require_open_conversation && !is_open_conversation(&conv_key_str) {
        return;
    }

    let guard = session::SessionGuard::capture();
    spawn(async move {
        let Some(sender_identity) = verify_claimed_signal_identity(
            &signal,
            signal.sender_identity(),
            "conversation",
            &conv_key_str,
        )
        .await
        else {
            return;
        };
        let Some(authenticated) =
            authenticate_conversation_signal(conv_key_str, sender_identity, member_subkey).await
        else {
            return;
        };
        if !guard.is_current()
            || (require_open_conversation && !is_open_conversation(&authenticated.conv_key_str))
        {
            return;
        }
        on_authenticated(authenticated, guard).await;
    });
}

fn handle_typing_signal(signal: PeerSignal) {
    spawn_authenticated_conversation_signal(signal, None, true, |signal, _| async move {
        apply_typing_from_peer(
            &signal.conv_key_str,
            signal.sender_identity,
            now_epoch_millis(),
        );
    });
}

fn handle_stop_typing_signal(signal: PeerSignal) {
    spawn_authenticated_conversation_signal(signal, None, true, |signal, _| async move {
        clear_typing_from_peer(&signal.conv_key_str, &signal.sender_identity);
    });
}

fn handle_new_data_signal(signal: PeerSignal) {
    let SignalPayload::NewData { member_subkey, .. } = &signal.payload else {
        return;
    };
    let member_subkey = *member_subkey;
    spawn_authenticated_conversation_signal(
        signal,
        Some(member_subkey),
        false,
        move |signal, guard| async move {
            clear_typing_from_peer(&signal.conv_key_str, &signal.sender_identity);
            ensure_watched(&signal.conv_key_str).await;
            if !guard.is_current() {
                return;
            }
            let AuthenticatedConversationSignal {
                ctx,
                conv,
                conv_key_str,
                ..
            } = signal;
            let changed = handle_conversation_change_loaded(
                ctx.clone(),
                &conv_key_str,
                &conv,
                ValueSubkeyRangeSet::single(member_subkey),
                None,
                guard,
            )
            .await;
            // Same read-before-write race the sync-nudge path covers: the
            // signal can outrun the sender's DHT write reaching the nodes we
            // read from. One delayed re-pull, only while the session holds.
            if !changed && guard.is_current() {
                retry_after_session_stable_delay(guard, || async {
                    handle_conversation_change_loaded(
                        ctx,
                        &conv_key_str,
                        &conv,
                        ValueSubkeyRangeSet::single(member_subkey),
                        None,
                        guard,
                    )
                    .await
                })
                .await;
            }
        },
    );
}

fn handle_conversation_invite_signal(signal: PeerSignal) {
    let (manifest_key, inviter_key) = match &signal.payload {
        SignalPayload::ConversationInvite {
            manifest_key,
            inviter_key,
            ..
        } => (manifest_key.clone(), inviter_key.clone()),
        _ => return,
    };
    let guard = session::SessionGuard::capture();
    let manifest_key_str = manifest_key.to_string();
    spawn(async move {
        if !guard.is_current() {
            return;
        }
        if verify_claimed_signal_identity(
            &signal,
            &inviter_key,
            "conversation invite",
            &manifest_key_str,
        )
        .await
        .is_none()
        {
            return;
        }
        if !guard.is_current() {
            return;
        }
        if let Err(e) =
            handle_conversation_invite(manifest_key, &manifest_key_str, inviter_key).await
        {
            report_nonfatal("chat.signal", "Failed to handle conversation invite", e);
            push_background_error(
                "chat.signal",
                "Failed to process an incoming conversation invite.",
            );
        }
    });
}

fn handle_conversation_invite_ack_signal(signal: PeerSignal) {
    let (manifest_key, owner_key, ack_sender_key) = match &signal.payload {
        SignalPayload::ConversationInviteAck {
            manifest_key,
            inviter_key,
            invitee_key,
            ..
        } => (
            manifest_key.clone(),
            inviter_key.clone(),
            invitee_key.clone(),
        ),
        _ => return,
    };
    let guard = session::SessionGuard::capture();
    let manifest_key_str = manifest_key.to_string();
    spawn(async move {
        if !guard.is_current() {
            return;
        }
        let Some(ctx) = UserCtx::try_current() else {
            return;
        };
        let Ok(local_identity) = parse_record_key(ctx.dht_key.as_ref()) else {
            warn!(
                "Rejected conversation invite ack for {} because the local inviter identity is invalid",
                manifest_key_str,
            );
            return;
        };
        if local_identity != owner_key {
            warn!(
                "Rejected conversation invite ack for {} because it targeted inviter {} instead of {}",
                manifest_key_str, owner_key, local_identity,
            );
            return;
        }
        if verify_claimed_signal_identity(
            &signal,
            &ack_sender_key,
            "conversation invite ack",
            &manifest_key_str,
        )
        .await
        .is_none()
        {
            return;
        }
        if !guard.is_current() {
            return;
        }
        if let Err(e) =
            handle_conversation_invite_ack(manifest_key, owner_key, ack_sender_key).await
        {
            report_nonfatal("chat.signal", "Failed to handle conversation invite ack", e);
            push_background_error(
                "chat.signal",
                "Failed to acknowledge a delivered conversation invite.",
            );
        }
    });
}

/// Handles a same-account sync nudge from a sibling device: verifies the
/// proof was signed by this account's own identity keypair (only sibling
/// devices hold it), then pulls the nudged scopes from the DHT.
fn handle_sync_nudge_signal(signal: PeerSignal) {
    let SignalPayload::SyncNudge {
        identity_key,
        scope,
        device_id,
        ..
    } = &signal.payload
    else {
        return;
    };
    let identity_key = identity_key.clone();
    let scope = *scope;
    let sender_device = Arc::clone(device_id);
    let guard = session::SessionGuard::capture();
    spawn(async move {
        // Only meaningful when the nudge names our own identity record.
        let Some(ctx) = UserCtx::try_current() else {
            return;
        };
        let Ok(own_identity) = parse_record_key(ctx.dht_key.as_ref()) else {
            return;
        };
        if own_identity != identity_key {
            return;
        }
        // Ignore our own nudges (defense in depth — the sender already
        // excludes this device's route).
        match current_device_id().await {
            Ok(own_device) if own_device == sender_device => return,
            Err(_) => return,
            Ok(_) => {}
        }
        if verify_claimed_signal_identity(
            &signal,
            &identity_key,
            "sync nudge from device",
            &sender_device,
        )
        .await
        .is_none()
        {
            return;
        }
        if !guard.is_current() {
            return;
        }
        // Tier-2 (nudge) ground truth: a line here proves the sibling's
        // private-route signal reached this device.
        debug!("Sync nudge received from sibling device (scope={scope:#04x}); pulling");
        handle_sync_nudge(scope).await;
    });
}

/// Handles an incoming `AppMessage` by parsing it as a `PeerSignal` and
/// dispatching to the appropriate handler.
///
/// sender proof. Private-route `app_message`s do not expose trusted sender
/// metadata when Veilid sender privacy is enabled, so route provenance is not
/// used for typing or new-data state changes.
pub fn handle_app_message(message: &[u8]) {
    let signal = match PeerSignal::from_bytes(message) {
        Ok(signal) => signal,
        Err(error) => {
            debug!("Dropping malformed app_message: {error:#}");
            return;
        }
    };

    match signal.payload {
        SignalPayload::Typing { .. } => handle_typing_signal(signal),
        SignalPayload::StopTyping { .. } => {
            handle_stop_typing_signal(signal);
        }
        SignalPayload::NewData { .. } => handle_new_data_signal(signal),
        SignalPayload::ConversationInvite { .. } => {
            handle_conversation_invite_signal(signal);
        }
        SignalPayload::ConversationInviteAck { .. } => {
            handle_conversation_invite_ack_signal(signal);
        }
        SignalPayload::SyncNudge { .. } => {
            handle_sync_nudge_signal(signal);
        }
    }
}
