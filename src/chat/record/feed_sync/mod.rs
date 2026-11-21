//! Feed sync (head-page publish + archive rotation) for conversation records.

use std::sync::Arc;

use anyhow::{Context, Error, Result, anyhow, bail, ensure};
use bytes::Bytes;
use tokio::{sync::OwnedMutexGuard, try_join};
use veilid_core::{KeyPair, RecordKey, SharedSecret};

use crate::{
    chat::{
        conv_keys::unwrap_conversation_key,
        feed::{
            MessagePage, MessageStatus, ParticipantFeedMeta, message_seq_bounds,
            normalize_remote_messages, union_messages_in_place,
        },
        manifest::{ConversationRoster, MemberStatus},
        storage::{LocalConversation, load_for_account, mutate_for_account},
    },
    helpers::spawn_blocking,
    models::pending_operation::PendingOperation,
    nonfatal,
    storage::automerge::from_automerge,
    user::{ctx::UserCtx, identity_context::IdentityContext},
    veilid::{
        core::parse_record_key,
        store::{self, StoreMode},
    },
};

use super::{member_feed_subkey, member_page_subkey};

mod archive;

use archive::{RotationOutcome, split_for_rotation, write_archive_page};

async fn lock_for_local_commit(user: &UserCtx) -> Result<OwnedMutexGuard<()>> {
    user.lock_for_persisted_account()
        .await?
        .context("Account was deleted before local conversation sync commit")
}
async fn mutate_active_conv(
    user: &UserCtx,
    account_id: &str,
    target_key: &str,
    missing_error: &'static str,
    apply: impl FnOnce(&mut LocalConversation) -> bool,
) -> Result<bool> {
    let _user_lock = lock_for_local_commit(user).await?;
    let mut still_present = false;
    let changed = mutate_for_account(account_id, target_key, |conv| {
        if !conv.has_conv_key() {
            return false;
        }
        still_present = true;
        apply(conv)
    })
    .await?;
    ensure!(still_present, "{missing_error}");
    Ok(changed)
}

/// Resolved conversation sync context for `sync_conv_feed`.
struct ConvSyncCtx {
    user: UserCtx,
    record_key: RecordKey,
    conv: Arc<LocalConversation>,
    secret: SharedSecret,
}

async fn conv_sync_preamble(
    op: &PendingOperation,
    label: &'static str,
    ctx: &IdentityContext,
) -> Result<ConvSyncCtx> {
    let user = UserCtx::from_user(&ctx.user);
    let target = op.target_key.as_deref().context(label)?;
    let record_key = parse_record_key(target).map_err(|e| anyhow!("Invalid {label}: {e}"))?;
    let conv = load_for_account(&user.account_id, target).await?;
    let secret = if conv.has_conv_key() {
        conv.conv_secret()
    } else if let Some(entry) = conv.key_ring.find_entry(ctx.user.dht_key())
        && let Ok(conv_key) = unwrap_conversation_key(&entry.wrapped_key, &ctx.keypair).await
    {
        let _user_lock = lock_for_local_commit(&user).await?;
        nonfatal!(
            "chat.record",
            "Failed to re-cache conversation key",
            mutate_for_account(&user.account_id, target, |conv| {
                if conv.has_conv_key() {
                    false
                } else {
                    conv.set_conv_key(&conv_key);
                    true
                }
            })
            .await
        );
        conv_key
    } else {
        bail!(
            "Cannot resolve conversation key for {target}: no cached key and key ring unwrap failed"
        );
    };
    Ok(ConvSyncCtx {
        user,
        record_key,
        conv,
        secret,
    })
}

/// Sync feed meta + head page to the conversation SMPL record.
///
/// When the head page exceeds `PAGE_SIZE_THRESHOLD`, performs archive rotation:
/// freezes the current head into an archive DFLT record, updates feed meta
/// with the archive key, and writes a fresh head page to the SMPL subkey.
pub async fn sync_conv_feed(op: &PendingOperation, ctx: &IdentityContext) -> Result<()> {
    let _ = op.conv_feed_message_ids()?;
    let my_identity_key = ctx.user.dht_key();
    let my_username = ctx.user.username();
    let target_key_str = op
        .target_key
        .as_deref()
        .context("ConvFeedSync op missing target_key")?
        .to_owned();
    let account_id = ctx.user.account_id().to_owned();

    let c = conv_sync_preamble(op, "ConvFeedSync op missing target_key", ctx).await?;
    let keypair = ctx.keypair.clone();
    let user = c.user;
    let member_index = c
        .conv
        .roster
        .members
        .iter()
        .position(|m| m.identity_key == my_identity_key && m.status == MemberStatus::Active)
        .context("Not an active conversation member")?;
    let feed_subkey = member_feed_subkey(member_index);
    let record_key = c.record_key;
    let secret = c.secret;
    let conv = c.conv;
    let local_head_start = conv.my_feed_meta.head_page_start_seq;
    let (meta_doc, page_doc, store_changed) = merge_current_own_feed(
        &conv,
        &record_key,
        member_index,
        &secret,
        &user,
        &account_id,
        &target_key_str,
    )
    .await?;
    // When the merge changed the store, `persist_locked` re-put the merged
    // value into the cache; re-read it as one Arc bump instead of deep-cloning
    // the whole conversation. Unchanged stores keep the pre-merge Arc.
    let conv = if store_changed {
        load_for_account(&account_id, &target_key_str).await?
    } else {
        conv
    };

    // `head_page_start_seq` is the root-generation signal: another device
    // rotating the archive advances it. When it advanced during the merge
    // above, the observed remote docs still carry the pre-rotation automerge
    // history — using them as bases would keep growing the head/meta docs by
    // one rotation cycle forever until they exceed Veilid's subkey ceiling.
    // Rebuild from fresh roots instead (base None); union-by-msg_id
    // reconstructs full content everywhere, and a Superseded retry inside
    // reconcile_write_open re-reconciles from the newer base, so retries are
    // double-root-free. Without rotation the local bases are kept so
    // stale-device edits still merge.
    let remote_rotated = conv.my_feed_meta.head_page_start_seq > local_head_start;

    let full_page = build_head_page(&conv, my_username);
    let (page, feed_meta, rotated_here) = resolve_head_and_meta(
        full_page,
        &conv.my_feed_meta,
        &user,
        &account_id,
        &target_key_str,
        &secret,
        &keypair,
    )
    .await?;

    // This publish performs the rotation itself: the fresh head (only the
    // overflow message) and the updated feed meta are written from fresh
    // roots — the old head history stays behind in the archive pages.
    let fresh_root = remote_rotated || rotated_here;
    let meta_doc = if fresh_root { None } else { meta_doc };
    let page_doc = if fresh_root { None } else { page_doc };

    let (written_meta, written_page) =
        store::with_record(&record_key, Some(&keypair), |key| async move {
            try_join!(
                store::reconcile_write_open(
                    &key,
                    feed_subkey,
                    &secret,
                    false,
                    meta_doc.as_deref(),
                    &feed_meta
                ),
                store::reconcile_write_open(
                    &key,
                    member_page_subkey(member_index),
                    &secret,
                    false,
                    page_doc.as_deref(),
                    &page,
                ),
            )
            .map_err(Error::from)
        })
        .await
        .context("Failed to publish conversation feed")?;

    let my_username_owned = my_username.to_owned();
    let (merged_meta, merged_page) = spawn_blocking(move || {
        let merged_meta: ParticipantFeedMeta = from_automerge(&written_meta)?;
        let mut merged_page: MessagePage = from_automerge(&written_page)?;
        normalize_remote_messages(&mut merged_page.messages, &my_username_owned);
        Ok::<_, Error>((merged_meta, merged_page))
    })
    .await??;
    // Persist stays outside the write scope only for error-containment (a
    // failed local persist must not fail the DHT publish); the audit comment
    // in merge_current_own_feed shows the locks allow nesting either way.
    persist_published_feed(
        &user,
        &account_id,
        &target_key_str,
        merged_meta,
        merged_page,
    )
    .await?;
    Ok(())
}

async fn persist_published_feed(
    user: &UserCtx,
    account_id: &str,
    target_key: &str,
    merged_meta: ParticipantFeedMeta,
    merged_page: MessagePage,
) -> Result<()> {
    mutate_active_conv(
        user,
        account_id,
        target_key,
        "Conversation was removed while publishing its feed",
        |conv| {
            let meta_changed = conv.my_feed_meta.merge_from(merged_meta);
            // Deliberately uncapped. `merged_page` is one head page (bounded on
            // write by `PAGE_SIZE_THRESHOLD`, and by the DHT subkey ceiling on
            // the wire), so capping it here cannot address a large transient —
            // the memory this path can add is bounded by that one page, while
            // the historical growth comes from the messages already in
            // `conv.messages` (Pending backlog) and from the archive merge,
            // bounded in `archive_backfill::load_archive_messages`.
            //
            // A cap here would also be unsafe in the direction it looks cheap:
            // `merged_page` is hydrated from a CRDT list, so it is in insertion
            // order, not sorted order — `take` would keep an arbitrary prefix,
            // and once `conv.messages.len()` reaches `MAX_LOCAL_MESSAGES` the
            // budget would be zero, merging nothing and dropping incoming
            // messages that `trim_conv_history` would have kept.
            let messages_changed =
                union_messages_in_place(&mut conv.messages, merged_page.messages).is_some();
            meta_changed || messages_changed
        },
    )
    .await
    .map(|_| ())
    .context("Failed to persist published conversation feed")
}

/// Builds the head page from unsent messages and messages at or after
/// `head_page_start_seq`, without re-archiving sent messages.
fn build_head_page(conv: &LocalConversation, my_username: &str) -> MessagePage {
    let head_start = conv.my_feed_meta.head_page_start_seq;
    let my_msgs: Vec<_> = conv
        .messages
        .iter()
        .filter(|m| {
            *m.sender_username == *my_username
                && (m.seq >= head_start || m.status == MessageStatus::Pending)
        })
        .cloned()
        .map(|mut message| {
            message.status = MessageStatus::Sent;
            message
        })
        .collect();
    let (first_seq, page_seq_end) = message_seq_bounds(&my_msgs);
    MessagePage {
        messages: my_msgs,
        page_seq_start: if first_seq == 0 {
            head_start
        } else {
            head_start.min(first_seq)
        },
        page_seq_end,
    }
}

/// Preserve the observed histories as reconcile bases, so a racing device's
/// additions are merged rather than deleted by a detached snapshot.
async fn merge_current_own_feed(
    conv: &LocalConversation,
    record_key: &RecordKey,
    member_index: usize,
    secret: &SharedSecret,
    user: &UserCtx,
    account_id: &str,
    target_key_str: &str,
) -> Result<(Option<Bytes>, Option<Bytes>, bool)> {
    // Deadlock audit (single-scope restructure): a with_record scope may span
    // the mutate_active_conv call only if no task holds the conversation's
    // per-row STORE_LOCK / persisted-account lock while waiting on this
    // record's RECORD_LOCK. All with_record call sites run before their
    // account/row locks (sync/pending_ops, sync/queue/failure,
    // chat/storage/crud, store_trait, cleanup.rs) or acquire record locks
    // with none held (chat record bootstrap/hydration/watcher, archive,
    // pairing_beacon, dict_sync, user record paths). Only
    // chat/hooks/cold_open.rs refresh_local_roster calls load_roster
    // beforehand for the same reason. The reverse order never happens, so
    // holding RECORD_LOCKS across mutate_active_conv cannot deadlock.
    let (meta_doc, page_doc, store_changed) = store::with_record(record_key, None, |key| {
        let secret = secret.clone();
        async move {
            let (meta_doc, page_doc) = try_join!(
                store::load_structured(&key, member_feed_subkey(member_index), &secret, true),
                store::load_structured(&key, member_page_subkey(member_index), &secret, true),
            )?;
            let meta_raw = meta_doc.clone();
            let page_raw = page_doc.clone();
            let (remote_meta, mut remote_page) = spawn_blocking(move || {
                let remote_meta = meta_raw
                    .as_deref()
                    .map(from_automerge::<ParticipantFeedMeta>)
                    .transpose()?;
                let remote_page = page_raw
                    .as_deref()
                    .map(from_automerge::<MessagePage>)
                    .transpose()?;
                Ok::<_, Error>((remote_meta, remote_page))
            })
            .await??;
            if let Some(page) = &mut remote_page {
                let member = conv
                    .roster
                    .members
                    .get(member_index)
                    .context("Missing feed member")?;
                normalize_remote_messages(&mut page.messages, &member.username);
            }
            let store_changed = mutate_active_conv(
                user,
                account_id,
                target_key_str,
                "Conversation was removed while refreshing its feed",
                |stored| {
                    let meta_changed =
                        remote_meta.is_some_and(|meta| stored.my_feed_meta.merge_from(meta));
                    let added = remote_page.is_some_and(|page| {
                        union_messages_in_place(&mut stored.messages, page.messages).is_some()
                    });
                    added || meta_changed
                },
            )
            .await?;
            Ok((meta_doc, page_doc, store_changed))
        }
    })
    .await?;
    Ok((meta_doc, page_doc, store_changed))
}

/// Resolves the head page and feed meta to write. When the head page overflows,
/// performs archive rotation: freezes the overflow into an archive DFLT record,
/// updates feed meta with the archive key, and persists the updated feed meta
/// locally so a retried op still sees the archive key.
async fn resolve_head_and_meta(
    full_page: MessagePage,
    my_feed_meta: &ParticipantFeedMeta,
    user: &UserCtx,
    account_id: &str,
    target_key_str: &str,
    secret: &SharedSecret,
    keypair: &KeyPair,
) -> Result<(MessagePage, ParticipantFeedMeta, bool)> {
    let mut meta = my_feed_meta.clone();
    let (head, rotated) = match split_for_rotation(full_page) {
        RotationOutcome::Unchanged(head) => (head, false),
        RotationOutcome::Rotated(rotation) => {
            for archive_page in rotation.archive_pages {
                write_archive_page(archive_page, &mut meta, target_key_str, secret, keypair)
                    .await?;
            }
            meta.head_page_start_seq = rotation.new_head.page_seq_start;
            (rotation.new_head, true)
        }
    };

    if meta != *my_feed_meta {
        mutate_active_conv(
            user,
            account_id,
            target_key_str,
            "Conversation was removed during archive rotation",
            |conv| conv.my_feed_meta.merge_from(meta.clone()),
        )
        .await
        .context("Failed to persist archive rotation progress")?;
    }

    Ok((head, meta, rotated))
}

/// Loads the roster from a conversation DHT record.
pub async fn load_roster(
    manifest_key: RecordKey,
    secret: &SharedSecret,
) -> Result<ConversationRoster> {
    let bytes = store::read_subkey(
        &manifest_key,
        ConversationRoster::SUBKEY,
        secret,
        StoreMode::StructuredEncrypted,
    )
    .await?;
    Ok(spawn_blocking(move || from_automerge(&bytes)).await??)
}
