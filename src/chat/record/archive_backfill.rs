//! Session-scoped archive backfill: walks per-member archive chains to reconstruct
//! history with unbounded fan-out (`spawn_archive_backfill`/`load_archive_chain`).

use std::collections::HashSet;

use anyhow::{Context, Error, Result, anyhow, bail, ensure};
use veilid_core::{KeyPair, SharedSecret};

use crate::{
    chat::{
        feed::{
            ArchiveIndex, FeedMessage, ParticipantFeedMeta, normalize_remote_messages,
            sort_messages, union_messages_in_place,
        },
        manifest::{ConversationRoster, MemberStatus},
        storage,
    },
    config,
    helpers::{concurrency::map_concurrent, spawn_blocking},
    session::epoch as session,
    storage::{automerge::from_automerge_or_rescue, background_errors::report_nonfatal},
    user::ctx::UserCtx,
    veilid::{core::parse_record_key, store},
};

use super::decode_page_messages;

pub(super) async fn load_archive_messages(
    manifest_key_str: &str,
    roster: &ConversationRoster,
    feed_metas: &[Option<ParticipantFeedMeta>],
    secret: &SharedSecret,
    guard: &session::SessionGuard,
) -> Result<Vec<FeedMessage>> {
    if !guard.is_current() {
        bail!("session changed during archive hydration");
    }

    let archive_targets: Vec<_> = feed_metas
        .iter()
        .enumerate()
        .filter_map(|(member_index, feed_meta)| {
            if roster.members.get(member_index)?.status != MemberStatus::Active {
                return None;
            }
            feed_meta
                .as_ref()
                .filter(|feed_meta| !feed_meta.archive_root_key.is_empty())
                .map(|feed_meta| (member_index, feed_meta.archive_root_key.clone()))
        })
        .collect();
    let mut archive_messages = Vec::new();
    let mut loaded_archives = Vec::new();
    // Unbounded: every member's archive chain is walked at once. The chain
    // walk holds only one page's bytes at a time, so the in-flight set is a
    // handful of DHT reads, not a heap multiple.
    let results = map_concurrent(archive_targets, {
        let secret = secret.clone();
        async move |(member_index, archive_key)| {
            let result = load_archive_chain(&archive_key, &secret).await;
            (member_index, archive_key, result)
        }
    })
    .await;
    for (member_index, archive_key, result) in results {
        // A single member's chain can fail permanently (corrupt index, cycle)
        // while every other member's history loads. Failing the whole
        // conversation would also keep it in the failed-hydration retry row on
        // every reconnect, so skip the one chain and report it.
        let mut messages = match result {
            Ok(messages) => messages,
            Err(error) => {
                report_nonfatal(
                    "chat.record",
                    "Skipped a member's archive chain",
                    format!("{manifest_key_str}:{member_index}:{archive_key}: {error}"),
                );
                continue;
            }
        };
        let member = roster
            .members
            .get(member_index)
            .context("Missing archive member")?;
        normalize_remote_messages(&mut messages, &member.username);
        loaded_archives.push((member_index, messages));
    }
    loaded_archives.sort_unstable_by_key(|(member_index, _)| *member_index);
    // Streaming top-N across members: sorting by `cmp_messages` and draining
    // the front after each member keeps the resident set at
    // `MAX_LOCAL_MESSAGES` + one member's window. A per-member prefix cut would
    // keep whichever members happen to come first in roster order, and a
    // single drain after the loop would still peak at Σmembers × the per-chain
    // window (~1 GB of text at the extreme). Sorting by `cmp_messages` and
    // draining the front after each member is a streaming top-N: the resident
    // set is at most `MAX_LOCAL_MESSAGES` + one member's window, and the result
    // is the newest `MAX_LOCAL_MESSAGES` across all members — the same set
    // `crud.rs::trim_conv_history` keeps at persist, so the bound never
    // discards a message the merge would have retained. Each member's messages
    // arrive ascending (the chain walk drains the front of an ascending
    // accumulation), so this also leaves the union's `needs_sort` check cheap.
    for (_, messages) in loaded_archives {
        archive_messages.extend(messages);
        let excess = archive_messages
            .len()
            .saturating_sub(config::chat::MAX_LOCAL_MESSAGES);
        if excess > 0 {
            sort_messages(&mut archive_messages);
            archive_messages.drain(..excess);
        }
    }

    if !guard.is_current() {
        bail!("session changed during archive hydration");
    }

    Ok(archive_messages)
}

pub(super) async fn merge_loaded_archive_messages(
    ctx: &UserCtx,
    manifest_key_str: &str,
    archive_messages: Vec<FeedMessage>,
    guard: session::SessionGuard,
) -> Result<bool> {
    if !guard.is_current() {
        bail!("session changed during archive hydration");
    }
    let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
        bail!("session changed or account was deleted during archive hydration");
    };
    storage::mutate_for_account(&ctx.account_id, manifest_key_str, |conv| {
        if !guard.is_current() {
            return false;
        }
        union_messages_in_place(&mut conv.messages, archive_messages).is_some()
    })
    .await
}

async fn backfill_conversation_archives(
    ctx: &UserCtx,
    manifest_key_str: &str,
    feed_metas: Vec<Option<ParticipantFeedMeta>>,
    secret: SharedSecret,
    guard: session::SessionGuard,
) -> Result<bool> {
    let conv = storage::load_for_account(&ctx.account_id, manifest_key_str).await?;
    let archive_messages =
        load_archive_messages(manifest_key_str, &conv.roster, &feed_metas, &secret, &guard).await?;
    if archive_messages.is_empty() {
        return Ok(false);
    }

    merge_loaded_archive_messages(ctx, manifest_key_str, archive_messages, guard).await
}

pub fn spawn_archive_backfill(
    guard: session::SessionGuard,
    ctx: UserCtx,
    manifest_key_str: String,
    feed_metas: Vec<Option<ParticipantFeedMeta>>,
    secret: SharedSecret,
    on_complete: impl FnOnce(Result<bool>, &str) + 'static,
) {
    session::spawn_session_scoped(async move {
        let result =
            backfill_conversation_archives(&ctx, &manifest_key_str, feed_metas, secret, guard)
                .await;
        if guard.is_current() {
            on_complete(result, &manifest_key_str);
        }
    });
}

/// Loads all archive pages from a chain of DFLT records, following
/// `ArchiveIndex.next_archive_key` links. Callers sort once when merging.
pub async fn load_archive_chain(
    first_archive_key: &str,
    secret: &SharedSecret,
) -> Result<Vec<FeedMessage>> {
    let mut all_messages = Vec::new();
    let mut current_key_str = first_archive_key.to_owned();
    let mut visited = HashSet::new();

    for _ in 0..config::chat::MAX_ARCHIVE_CHAIN_DEPTH {
        if current_key_str.is_empty() {
            break;
        }
        ensure!(
            visited.insert(current_key_str.clone()),
            "Conversation archive chain contains a cycle"
        );
        let archive_key = parse_record_key(&current_key_str)
            .map_err(|error| anyhow!("invalid archive chain key: {error}"))?;

        let (index, pages) = store::with_record(&archive_key, None::<&KeyPair>, |key| {
            let secret = secret.clone();
            let current_key_str = current_key_str.clone();
            async move {
                let index_bytes = store::load_structured(&key, 0, &secret, true)
                    .await?
                    .context("Archive index subkey empty")?;
                let key_for_err = current_key_str.clone();
                let index: ArchiveIndex = spawn_blocking(move || {
                    let (index, _rescued_doc): (ArchiveIndex, _) =
                        from_automerge_or_rescue(&index_bytes).map_err(|error| {
                            anyhow!("corrupt ArchiveIndex for {key_for_err}: {error}")
                        })?;
                    index.validate()?;
                    Ok::<_, Error>(index)
                })
                .await??;

                // Non-forced: archive pages are immutable once the archive is
                // linked into the chain, so a cached page is always correct —
                // only uncached pages cost a network read.
                // Unbounded: a page is a few KB, so walking the whole chain at
                // once costs one DHT round trip per depth rather than a wave
                // of them per depth.
                let loaded_pages = map_concurrent(1..=index.page_count, {
                    let key = key.clone();
                    let secret = secret.clone();
                    let current_key_str = current_key_str.clone();
                    async move |subkey| {
                        let page = store::load_structured(&key, subkey, &secret, false)
                            .await?
                            .with_context(|| {
                                format!("Archive page {current_key_str}:{subkey} is missing")
                            })?;
                        Ok::<(u32, _), Error>((subkey, page))
                    }
                })
                .await;
                let mut ordered_pages = loaded_pages.into_iter().collect::<Result<Vec<_>>>()?;
                ordered_pages.sort_unstable_by_key(|(subkey, _)| *subkey);
                let pages: Vec<_> = ordered_pages.into_iter().map(|(_, page)| page).collect();
                Ok((index, pages))
            }
        })
        .await?;

        all_messages.extend(
            spawn_blocking(move || {
                let mut messages = Vec::new();
                for raw in pages {
                    messages.extend(decode_page_messages(&raw)?);
                }
                Ok::<_, Error>(messages)
            })
            .await??,
        );
        // Rolling **newest** window, not a prefix. The walk starts at the
        // chain's immutable root and follows `next_archive_key` forward, and
        // each generation's pages are decoded in ascending subkey (seq) order —
        // so `all_messages` accumulates oldest-first. Taking a prefix and
        // breaking would keep the oldest `MAX_LOCAL_MESSAGES` and never reach
        // the recent generations, i.e. exactly the messages
        // `crud.rs::trim_conv_history` discards at persist while dropping the
        // ones it keeps. Draining the front keeps the newest window, which is
        // the set that survives the trim, and lets the walk still reach the end
        // of the chain (bounded by `MAX_ARCHIVE_CHAIN_DEPTH`).
        let excess = all_messages
            .len()
            .saturating_sub(config::chat::MAX_LOCAL_MESSAGES);
        if excess > 0 {
            all_messages.drain(..excess);
        }
        current_key_str = index.next_archive_key;
    }

    if !current_key_str.is_empty() {
        report_nonfatal(
            "chat.record",
            "Archive chain exceeds the load cap; returning the first generations",
            format_args!(
                "conversation archive {first_archive_key}: remaining chain starts at {current_key_str}"
            ),
        );
    }
    Ok(all_messages)
}
