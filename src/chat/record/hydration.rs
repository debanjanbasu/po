//! Cold-start conversation hydration and per-conversation head hydration.

use std::{collections::HashSet, sync::Arc};

use anyhow::{Context, Error, Result, bail};
use autosurgeon::Hydrate;
use bytes::Bytes;
use dioxus::logger::tracing::warn;
use tokio::try_join;
use veilid_core::{KeyPair, RecordKey, SharedSecret};

use crate::{
    chat::{
        conv_keys::{unwrap_conversation_key, verify_roster_binding},
        conversation,
        feed::{
            FeedMessage, MessagePage, ParticipantFeedMeta, message_dedup_key,
            normalize_remote_messages, union_messages_in_place,
        },
        manifest::{ConversationKeyRing, ConversationManifest, ConversationRoster, MemberStatus},
        storage::{self, LocalConversation},
    },
    config,
    helpers::{async_utils::retry_once_on_transient, concurrency::map_concurrent, spawn_blocking},
    session::epoch::{self as session, SessionGuard},
    storage::{
        automerge::{from_automerge, from_automerge_or_rescue},
        background_errors::report_nonfatal,
    },
    user::ctx::UserCtx,
    veilid::store,
};

use super::{
    archive_backfill::{load_archive_messages, merge_loaded_archive_messages},
    member_feed_subkey, member_page_subkey,
};

/// Decodes one automerge-encoded value from `raw`, rescuing corrupt docs and
/// reporting the failure as non-fatal (the caller then skips that slot).
///
/// The single decode wrapper for every watcher/hydration/backfill path; the
/// error policy is fixed here — `report_nonfatal` with `component`/`label` —
/// so callers can never drift into silently swallowing or double-reporting a
/// corrupt doc.
pub(in crate::chat) fn decode_automerge<T: Hydrate>(
    raw: Option<&[u8]>,
    component: &'static str,
    label: &'static str,
    context: &str,
) -> Option<T> {
    raw.and_then(|raw| {
        from_automerge_or_rescue::<T>(raw)
            .map(|(value, _rescued_doc)| value)
            .inspect_err(|e| report_nonfatal(component, label, format!("{context}: {e}")))
            .ok()
    })
}

/// Decodes messages from a single raw page without sorting; callers sort once at the end.
pub(in crate::chat::record) fn decode_page_messages(raw: &[u8]) -> Result<Vec<FeedMessage>> {
    let (page, _rescued_doc) =
        from_automerge_or_rescue::<MessagePage>(raw).context("Corrupt message page")?;
    Ok(page.messages)
}

struct LoadedConversationHeadState {
    key_ring: ConversationKeyRing,
    secret: SharedSecret,
    manifest: ConversationManifest,
    roster: ConversationRoster,
    head_messages: Vec<FeedMessage>,
    feed_metas: Vec<Option<ParticipantFeedMeta>>,
    member_index: usize,
}

pub struct HeadHydration {
    pub conv: Arc<LocalConversation>,
    pub member_index: usize,
    pub feed_metas: Vec<Option<ParticipantFeedMeta>>,
}

async fn load_member_head_state(
    key: &RecordKey,
    secret: &SharedSecret,
    roster: &ConversationRoster,
    manifest_key_str: &str,
) -> Result<(Vec<FeedMessage>, Vec<Option<ParticipantFeedMeta>>)> {
    // Unbounded: every member's head subkeys are fetched at once, so the state
    // is limited by the slowest member's DHT round trip rather than by a
    // permit-pool serialising the loads into waves.
    let results = map_concurrent(0..roster.members.len(), {
        let key = key.clone();
        let secret = secret.clone();
        async move |member_index| {
            let key = key.clone();
            let secret = secret.clone();
            let load = move || {
                let key = key.clone();
                let secret = secret.clone();
                async move {
                    try_join!(
                        store::load_structured(
                            &key,
                            member_page_subkey(member_index),
                            &secret,
                            true
                        ),
                        store::load_structured(
                            &key,
                            member_feed_subkey(member_index),
                            &secret,
                            true
                        ),
                    )
                }
            };
            (
                member_index,
                retry_once_on_transient(store::is_transient_network_error, load).await,
            )
        }
    })
    .await;
    // Slot-indexed, not order-indexed: a member whose read failed leaves `None`
    // in its own slot, so it can never shift another member's page onto the
    // wrong username or feed-meta slot.
    let mut loaded: Vec<Option<(Option<Bytes>, Option<Bytes>)>> = vec![None; roster.members.len()];
    for (member_index, result) in results {
        match result {
            Ok(docs) => {
                if let Some(slot) = loaded.get_mut(member_index) {
                    *slot = Some(docs);
                }
            }
            Err(error) => report_nonfatal(
                "chat.record",
                "Failed to load a member's head subkeys",
                format!("{manifest_key_str}:{member_index}: {error}"),
            ),
        }
    }

    let members = roster.members.clone();
    let manifest_key_str = manifest_key_str.to_owned();
    spawn_blocking(move || {
        let mut messages = Vec::new();
        let mut feed_metas = Vec::with_capacity(members.len());
        for (member_index, (member, docs)) in members.iter().zip(loaded).enumerate() {
            // One member's head unreadable: skip it rather than failing the
            // whole conversation (the watcher path skips the same way); the
            // next watch, sweep or reopen retries it.
            let Some((page, feed_meta_raw)) = docs else {
                feed_metas.push(None);
                continue;
            };
            let feed_meta = decode_automerge(
                feed_meta_raw.as_deref(),
                "chat.record",
                "Corrupt participant feed meta (skipped)",
                &format!("{manifest_key_str}:{member_index}"),
            );
            if member.status == MemberStatus::Active
                && let Some(raw) = page.as_deref()
            {
                let mut member_messages = decode_page_messages(raw)?;
                normalize_remote_messages(&mut member_messages, &member.username);
                messages.extend(member_messages);
            }
            feed_metas.push(feed_meta);
        }
        Ok((messages, feed_metas))
    })
    .await?
}

async fn load_conversation_head_state(
    manifest_key: &RecordKey,
    manifest_key_str: &str,
    my_identity_key: &str,
    keypair: &KeyPair,
    guard: &session::SessionGuard,
) -> Result<LoadedConversationHeadState> {
    let (key_ring, secret, manifest, roster, head_messages, feed_metas) =
        store::with_record(manifest_key, None::<&KeyPair>, |key| async move {
            let key_ring_bytes = store::load_unencrypted(&key, ConversationKeyRing::SUBKEY, true)
                .await?
                .context("Key ring subkey empty")?;
            let key_ring: ConversationKeyRing =
                spawn_blocking(move || from_automerge(&key_ring_bytes)).await??;
            let entry = key_ring
                .find_entry(my_identity_key)
                .context("Not in key ring")?;
            let secret = unwrap_conversation_key(&entry.wrapped_key, keypair).await?;
            let (manifest_bytes, roster_bytes) = try_join!(
                store::load_structured(&key, ConversationManifest::SUBKEY, &secret, true),
                store::load_structured(&key, ConversationRoster::SUBKEY, &secret, true),
            )?;
            let manifest_bytes = manifest_bytes.context("Manifest subkey empty")?;
            let roster_bytes = roster_bytes.context("Roster subkey empty")?;
            let (manifest, roster) = spawn_blocking(move || {
                let manifest: ConversationManifest = from_automerge(&manifest_bytes)?;
                let roster: ConversationRoster = from_automerge(&roster_bytes)?;
                Ok::<_, Error>((manifest, roster))
            })
            .await??;
            let (head_messages, feed_metas) =
                load_member_head_state(&key, &secret, &roster, manifest_key_str).await?;
            Ok((
                key_ring,
                secret,
                manifest,
                roster,
                head_messages,
                feed_metas,
            ))
        })
        .await?;

    // Must run outside the record scope above: it opens the record again to
    // read the schema, and scopes on one key must not nest.
    verify_roster_binding(manifest_key, &roster)
        .await
        .with_context(|| format!("Unverified conversation roster for {manifest_key_str}"))?;

    if !guard.is_current() {
        bail!("session changed during conversation hydration");
    }

    let member_index = roster
        .members
        .iter()
        .position(|member| member.identity_key == my_identity_key)
        .context("Current user not present in conversation roster")?;

    Ok(LoadedConversationHeadState {
        key_ring,
        secret,
        manifest,
        roster,
        head_messages,
        feed_metas,
        member_index,
    })
}

fn apply_head_hydration(
    conv: &mut LocalConversation,
    loaded: LoadedConversationHeadState,
    manifest_key_str: &str,
) -> (usize, Vec<Option<ParticipantFeedMeta>>) {
    let LoadedConversationHeadState {
        key_ring,
        secret,
        manifest,
        roster,
        head_messages,
        feed_metas,
        member_index,
    } = loaded;

    if config::e2e::enabled() {
        // Emit per message this hydration newly inserts from a peer's head page.
        let my_username = roster
            .members
            .get(member_index)
            .map(|member| member.username.as_ref());
        let seen: HashSet<u128> = conv.messages.iter().map(message_dedup_key).collect();
        for message in &head_messages {
            if Some(message.sender_username.as_ref()) != my_username
                && !seen.contains(&message_dedup_key(message))
            {
                config::e2e::event!("message_received", conversation = %manifest_key_str, msg_id = %message.msg_id, from = %message.sender_username);
            }
        }
    }

    conv.manifest = manifest;
    conv.roster = roster;
    conv.key_ring = key_ring;
    conv.set_conv_key(&secret);
    union_messages_in_place(&mut conv.messages, head_messages);
    if let Some(feed_meta) = feed_metas.get(member_index).cloned().flatten() {
        conv.my_feed_meta.merge_from(feed_meta);
    } else {
        warn!(
            "Missing self feed meta during hydration for conversation {manifest_key_str}; keeping local/default state"
        );
    }

    (member_index, feed_metas)
}

pub async fn hydrate_conversation_heads(
    manifest_key: &RecordKey,
    manifest_key_str: &str,
    ctx: &UserCtx,
    guard: session::SessionGuard,
) -> Result<HeadHydration> {
    let keypair = ctx
        .keypair
        .as_ref()
        .context("No keypair for conversation hydration")?;
    let loaded = load_conversation_head_state(
        manifest_key,
        manifest_key_str,
        &ctx.dht_key,
        keypair,
        &guard,
    )
    .await?;

    if !guard.is_current() {
        bail!("session changed during conversation hydration");
    }
    let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
        bail!("session changed or account was deleted during conversation hydration");
    };

    let mut hydrated_meta: Option<(usize, Vec<Option<ParticipantFeedMeta>>)> = None;
    storage::mutate_for_account(&ctx.account_id, manifest_key_str, |conv| {
        if !guard.is_current() {
            return false;
        }
        hydrated_meta = Some(apply_head_hydration(conv, loaded, manifest_key_str));
        true
    })
    .await?;

    let (member_index, feed_metas) =
        hydrated_meta.context("session changed during conversation hydration")?;

    if !guard.is_current() {
        bail!("session changed during conversation hydration");
    }

    let conv = storage::load_for_account(&ctx.account_id, manifest_key_str).await?;

    Ok(HeadHydration {
        conv,
        member_index,
        feed_metas,
    })
}

/// Pulls a single conversation's full state from DHT (key ring, manifest,
/// roster, all member head pages) and saves it locally.
///
/// Persists hydrated head state before archive backfill so watcher/signal paths
/// can authenticate the conversation while archive loads are still in flight.
///
/// Used during new-device pairing and lazy on-open recovery. Failures on
/// individual conversations are non-fatal and logged.
pub async fn hydrate_conversation(
    manifest_key: RecordKey,
    manifest_key_str: &str,
    ctx: &UserCtx,
    guard: SessionGuard,
) -> Result<Arc<storage::LocalConversation>> {
    let hydrated = hydrate_conversation_heads(&manifest_key, manifest_key_str, ctx, guard).await?;

    if !guard.is_current() {
        bail!("session changed during conversation hydration");
    }

    let archive_messages = load_archive_messages(
        manifest_key_str,
        &hydrated.conv.roster,
        &hydrated.feed_metas,
        &hydrated.conv.conv_secret(),
        &guard,
    )
    .await?;

    if !guard.is_current() {
        bail!("session changed during conversation hydration");
    }

    let conv = if archive_messages.is_empty() {
        hydrated.conv
    } else {
        merge_loaded_archive_messages(ctx, manifest_key_str, archive_messages, guard).await?;
        // Cache hit: the merge just wrote through the store choke-point.
        storage::load_for_account(&ctx.account_id, manifest_key_str).await?
    };

    if let Some(last_msg) = conv.messages.last()
        && let Err(error) = conversation::update_conversation_preview_for_account(
            ctx,
            manifest_key_str,
            &last_msg.content,
            last_msg.timestamp,
            None,
        )
        .await
    {
        report_nonfatal(
            "chat.record",
            "Failed to update conversation preview after hydration",
            error,
        );
    }

    Ok(conv)
}
