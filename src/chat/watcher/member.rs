//! Member-feed watch handler: resolves remote member updates, applies owner and
//! member updates in place locally, and spawns archive backfill when head pages
//! lag feed metas.

use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

use dioxus::logger::tracing::warn;
use veilid_core::SharedSecret;

use crate::{
    chat::{
        conversation,
        feed::{
            FeedMessage, MessagePage, ParticipantFeedMeta, message_dedup_key,
            normalize_remote_messages, union_messages_in_place_with_seen,
        },
        manifest::{ConversationRoster, MemberStatus},
        record, storage,
    },
    config,
    session::epoch as session,
    storage::background_errors::report_nonfatal,
};

use super::updates::{MemberRemoteUpdate, OwnerUpdates};

pub(super) fn archive_backfill_needed(
    messages: &[FeedMessage],
    sender_username: &str,
    feed_meta: &ParticipantFeedMeta,
) -> bool {
    if feed_meta.archive_root_key.is_empty() || feed_meta.head_page_start_seq <= 1 {
        return false;
    }

    let sequences: BTreeSet<u64> = messages
        .iter()
        .filter(|message| {
            *message.sender_username == *sender_username
                && message.seq > 0
                && message.seq < feed_meta.head_page_start_seq
        })
        .map(|message| message.seq)
        .collect();
    let mut next_seq = 1;
    for seq in sequences {
        if seq != next_seq {
            return true;
        }
        next_seq = seq.saturating_add(1);
    }
    next_seq < feed_meta.head_page_start_seq
}

fn normalize_remote_page(
    manifest_key_str: &str,
    expected_username: &str,
    remote_page: &mut MessagePage,
) {
    for msg in &mut remote_page.messages {
        if *msg.sender_username != *expected_username {
            warn!(
                "Normalized remote sender username {} to roster member {} for conversation {}",
                msg.sender_username, expected_username, manifest_key_str,
            );
        }
    }
    normalize_remote_messages(&mut remote_page.messages, expected_username);
}

#[derive(Clone, Copy)]
struct MemberRuntimeContext<'a> {
    expected_identity: &'a str,
    expected_username: &'a str,
    can_merge_page: bool,
}

#[derive(Default)]
struct MemberUpdateOutcome {
    changed: bool,
    last_msg: Option<FeedMessage>,
    /// Messages this merge newly inserted from *other* members, so the unread
    /// count follows what the merge added instead of the senders' clocks (and
    /// so a multi-member batch counts every member's messages, not one page's).
    new_foreign: u32,
    backfill_feed_meta: Option<ParticipantFeedMeta>,
}

fn resolve_member_update<'a>(
    roster: &'a ConversationRoster,
    manifest_key_str: &str,
    member_index: usize,
    remote_page: &mut Option<MessagePage>,
) -> Option<MemberRuntimeContext<'a>> {
    let Some(member) = roster.members.get(member_index) else {
        report_nonfatal(
            "chat.watcher",
            "Received member update for unknown roster slot",
            format!("{manifest_key_str}:{member_index}"),
        );
        return None;
    };

    let can_merge_page = matches!(member.status, MemberStatus::Active);
    if let Some(page) = remote_page.as_mut() {
        if can_merge_page {
            normalize_remote_page(manifest_key_str, &member.username, page);
        } else {
            warn!(
                "Dropped page from inactive member {} in conversation {}",
                member.identity_key, manifest_key_str,
            );
            *remote_page = None;
        }
    }

    Some(MemberRuntimeContext {
        expected_identity: member.identity_key.as_str(),
        expected_username: member.username.as_str(),
        can_merge_page,
    })
}

fn apply_member_update_in_place(
    conv: &mut storage::LocalConversation,
    manifest_key_str: &str,
    member: MemberRuntimeContext<'_>,
    remote_page: Option<&MessagePage>,
    feed_meta: Option<&ParticipantFeedMeta>,
    my_identity_key: Option<&str>,
    seen: &mut HashSet<u128>,
) -> MemberUpdateOutcome {
    if config::e2e::enabled()
        && my_identity_key != Some(member.expected_identity)
        && let Some(page) = remote_page
    {
        // Emit per genuinely new remote message before the union folds it in.
        for message in &page.messages {
            if !seen.contains(&message_dedup_key(message)) {
                config::e2e::event!("message_received", conversation = %manifest_key_str, msg_id = %message.msg_id, from = %member.expected_username);
            }
        }
    }
    let known_before = seen.len();
    let last_msg = remote_page.and_then(|page| {
        union_messages_in_place_with_seen(&mut conv.messages, page.messages.iter().cloned(), seen)
    });
    // `seen` holds every id the batch knows, so the delta is exactly what this
    // page inserted. History backfill (`my_identity_key = None`) must not raise
    // the badge for archived messages, hence the `Some(mine)` guard.
    let new_foreign = match my_identity_key {
        Some(mine) if mine != member.expected_identity => {
            u32::try_from(seen.len().saturating_sub(known_before)).unwrap_or(u32::MAX)
        }
        _ => 0,
    };
    let self_feed_meta_changed = if let Some(feed_meta) = feed_meta
        && let Some(my_identity_key) = my_identity_key
        && member.expected_identity == my_identity_key
    {
        conv.my_feed_meta.merge_from(feed_meta.clone())
    } else {
        false
    };
    MemberUpdateOutcome {
        changed: last_msg.is_some() || self_feed_meta_changed,
        new_foreign,
        backfill_feed_meta: feed_meta.and_then(|feed_meta| {
            (member.can_merge_page
                && archive_backfill_needed(&conv.messages, member.expected_username, feed_meta))
            .then(|| feed_meta.clone())
        }),
        last_msg,
    }
}

/// Assigns `incoming` only when the stored value differs, reporting whether the
/// slot moved. `changed` from an owner-doc merge gates a save that
/// re-serializes every message, so a re-fetched-but-identical doc must not
/// report one.
fn set_if_different<T: PartialEq>(slot: &mut T, incoming: Option<T>) -> bool {
    match incoming {
        Some(value) if *slot != value => {
            *slot = value;
            true
        }
        _ => false,
    }
}

/// Apply owner and member updates in ONE store mutation — a single
/// load→merge-all→save cycle instead of separate cycles for owner and member
/// subkeys. Post-effects (conversation preview, archive backfill spawns) run
/// after the save; the preview uses the newest merged message across the batch
/// and the unread count the batch's merges actually inserted, instead of
/// whichever member happened to be applied last.
pub(super) async fn apply_member_updates_batch(
    account_id: &str,
    manifest_key_str: &str,
    roster: &ConversationRoster,
    secret: &SharedSecret,
    my_identity_key: Option<&str>,
    guard: session::SessionGuard,
    (owner_updates, updates): (OwnerUpdates, Vec<MemberRemoteUpdate>),
) -> bool {
    let mut work = Vec::with_capacity(updates.len());
    for update in updates {
        if update.page.is_none() && update.feed_meta.is_none() {
            continue;
        }
        let mut remote_page = update.page;
        let Some(member) = resolve_member_update(
            roster,
            manifest_key_str,
            update.member_index,
            &mut remote_page,
        ) else {
            continue;
        };
        work.push((update.member_index, member, remote_page, update.feed_meta));
    }
    let has_owner_updates = owner_updates.manifest.is_some()
        || owner_updates.roster.is_some()
        || owner_updates.key_ring.is_some();
    if (!has_owner_updates && work.is_empty()) || !guard.is_current() {
        return false;
    }

    let OwnerUpdates {
        manifest,
        roster: owner_roster,
        key_ring,
    } = owner_updates;
    let mut outcomes: Vec<(usize, MemberUpdateOutcome)> = Vec::with_capacity(work.len());
    let mutate_result = storage::mutate_for_account(account_id, manifest_key_str, |conv| {
        // Re-check under the row lock so a torn-down session's stale
        // work never writes under the new one.
        if !guard.is_current() {
            return false;
        }
        let mut changed = set_if_different(&mut conv.manifest, manifest);
        changed |= set_if_different(&mut conv.roster, owner_roster);
        changed |= set_if_different(&mut conv.key_ring, key_ring);
        let mut seen = if work
            .iter()
            .any(|(_, _, remote_page, _)| remote_page.is_some())
        {
            conv.messages.iter().map(message_dedup_key).collect()
        } else {
            HashSet::new()
        };
        for (member_index, member, remote_page, feed_meta) in work {
            let outcome = apply_member_update_in_place(
                conv,
                manifest_key_str,
                member,
                remote_page.as_ref(),
                feed_meta.as_ref(),
                my_identity_key,
                &mut seen,
            );
            changed |= outcome.changed;
            outcomes.push((member_index, outcome));
        }
        changed
    })
    .await;

    let changed = match mutate_result {
        Ok(changed) => changed,
        Err(e) => {
            report_nonfatal(
                "chat.watcher",
                "Failed to save conversation updates locally",
                format!("{manifest_key_str}: {e}"),
            );
            return false;
        }
    };
    if !guard.is_current() {
        return false;
    }

    apply_batch_side_effects(
        account_id,
        manifest_key_str,
        roster,
        secret,
        guard,
        outcomes,
    )
    .await;
    changed
}

async fn apply_batch_side_effects(
    account_id: &str,
    manifest_key_str: &str,
    roster: &ConversationRoster,
    secret: &SharedSecret,
    guard: session::SessionGuard,
    outcomes: Vec<(usize, MemberUpdateOutcome)>,
) {
    // Every member's additions count, not just the page holding the newest
    // message: a batch (forced resync, watch death, overflow recovery) merges
    // all members at once, and `last_message_at` advancing past the others
    // means they could never be counted later.
    let new_foreign = outcomes
        .iter()
        .map(|(_, outcome)| outcome.new_foreign)
        .fold(0u32, u32::saturating_add);
    if let Some(last_msg) = outcomes
        .iter()
        .filter_map(|(_, outcome)| outcome.last_msg.as_ref())
        .max_by_key(|msg| msg.timestamp)
    {
        conversation::update_conversation_preview(
            manifest_key_str,
            &last_msg.content,
            last_msg.timestamp,
            (new_foreign > 0).then_some(new_foreign),
        )
        .await;
    }

    if outcomes
        .iter()
        .any(|(_, outcome)| outcome.backfill_feed_meta.is_some())
    {
        let account_id: Arc<str> = Arc::from(account_id);
        let manifest_key_str: Arc<str> = Arc::from(manifest_key_str);
        for (member_index, outcome) in outcomes {
            if let Some(feed_meta) = outcome.backfill_feed_meta {
                session::spawn_session_scoped(maybe_backfill_member_archives(
                    Arc::clone(&account_id),
                    Arc::clone(&manifest_key_str),
                    roster.clone(),
                    member_index,
                    guard,
                    feed_meta,
                    secret.clone(),
                ));
            }
        }
    }
}

async fn maybe_backfill_member_archives(
    account_id: Arc<str>,
    manifest_key_str: Arc<str>,
    roster: ConversationRoster,
    member_index: usize,
    guard: session::SessionGuard,
    feed_meta: ParticipantFeedMeta,
    secret: SharedSecret,
) {
    if !guard.is_current() || feed_meta.archive_root_key.is_empty() {
        return;
    }
    let Some(member) = roster.members.get(member_index) else {
        return;
    };
    if !matches!(member.status, MemberStatus::Active) {
        return;
    }

    match record::load_archive_chain(&feed_meta.archive_root_key, &secret).await {
        Ok(messages) => {
            if !guard.is_current() || messages.is_empty() {
                return;
            }
            apply_member_updates_batch(
                &account_id,
                &manifest_key_str,
                &roster,
                &secret,
                None,
                guard,
                (
                    OwnerUpdates::default(),
                    vec![MemberRemoteUpdate {
                        member_index,
                        feed_meta: None,
                        page: Some(MessagePage {
                            messages,
                            ..Default::default()
                        }),
                    }],
                ),
            )
            .await;
        }
        Err(e) => report_nonfatal(
            "chat.watcher",
            "Failed to backfill member archive",
            format!("{manifest_key_str}:{member_index}: {e}"),
        ),
    }
}
