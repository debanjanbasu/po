//! Head-overflow archive rotation: size-bounded page chunking, deterministic
//! per-generation archive records, `ArchiveIndex` upkeep, idempotent retry skipping.

use std::{collections::HashSet, mem::take};

use anyhow::{Context, Error, Result, anyhow, ensure};
use veilid_core::{DHTSchema, KeyPair, RecordKey, SharedSecret};

use crate::{
    chat::{
        feed::{
            ArchiveIndex, FeedMessage, MessagePage, ParticipantFeedMeta, SeqRange,
            message_dedup_key, message_seq_bounds,
        },
        record::archive_owner_keypair,
    },
    config,
    helpers::async_utils::spawn_blocking,
    nonfatal,
    storage::{
        automerge::{from_automerge_or_rescue, reconcile_to_automerge},
        background_errors::report_nonfatal,
    },
    veilid::{core::ensure_local_api, store},
};

/// Estimated serialized automerge size of one message: ~160 bytes of fixed
/// overhead (actor ID, ops, change metadata, seq counters, field keys) plus
/// the variable-length content and sender username.
fn estimated_message_size(message: &FeedMessage) -> usize {
    160 + message.content.len() + message.sender_username.len()
}

/// Estimates the serialized automerge document size for a `MessagePage` without
/// performing a full reconcile+save cycle.
fn estimated_automerge_size(page: &MessagePage) -> usize {
    page.messages.iter().map(estimated_message_size).sum()
}

/// Builds a `MessagePage` from messages, deriving its seq bounds.
fn page_from_messages(messages: Vec<FeedMessage>) -> MessagePage {
    let (page_seq_start, page_seq_end) = message_seq_bounds(&messages);
    MessagePage {
        messages,
        page_seq_start,
        page_seq_end,
    }
}

/// Splits archived messages into pages whose estimated automerge size stays
/// within `PAGE_SIZE_THRESHOLD`, so one oversized rotation (e.g. after a long
/// offline backlog) can never produce an archive page too large for its
/// single DHT subkey.
fn chunk_archive_messages(messages: Vec<FeedMessage>) -> Vec<MessagePage> {
    let mut pages = Vec::new();
    let mut current: Vec<FeedMessage> = Vec::new();
    let mut current_size = 0usize;
    for message in messages {
        let message_size = estimated_message_size(&message);
        if !current.is_empty() && current_size + message_size > config::chat::PAGE_SIZE_THRESHOLD {
            pages.push(page_from_messages(take(&mut current)));
            current_size = 0;
        }
        current_size += message_size;
        current.push(message);
    }
    if !current.is_empty() {
        pages.push(page_from_messages(current));
    }
    pages
}

/// Result of a page rotation: the frozen archive pages and the new head page.
pub(super) struct RotationResult {
    /// Frozen pages to write to the archive DFLT record, in seq order, each
    /// sized within `PAGE_SIZE_THRESHOLD`.
    pub(super) archive_pages: Vec<MessagePage>,
    /// Fresh head page containing only messages that overflowed.
    pub(super) new_head: MessagePage,
}

pub(super) enum RotationOutcome {
    Unchanged(MessagePage),
    Rotated(RotationResult),
}

/// Splits the head page into size-bounded archive pages (all but the last
/// message) and a fresh head page (only the last message), or returns the
/// unchanged page when it fits within the threshold or has ≤1 message.
pub(super) fn split_for_rotation(mut page: MessagePage) -> RotationOutcome {
    if page.messages.len() <= 1
        || estimated_automerge_size(&page) <= config::chat::PAGE_SIZE_THRESHOLD
    {
        return RotationOutcome::Unchanged(page);
    }

    // Archive = all messages except the last one. The last message starts the
    // new head page so it is included in the next DHT write.
    let split_point = page.messages.len() - 1;
    let head_msgs = page.messages.split_off(split_point);

    RotationOutcome::Rotated(RotationResult {
        archive_pages: chunk_archive_messages(page.messages),
        new_head: page_from_messages(head_msgs),
    })
}

/// Load an `ArchiveIndex` from subkey 0 of an archive record, together with
/// the automerge doc bytes it was hydrated from (so updates can reconcile
/// into the existing history instead of starting a fresh doc).
///
/// A doc that fails strict loading goes through best-effort rescue first.
/// When rescue also fails the error propagates — defaulting the index on the
/// write path would reset `page_count` to 0 and overwrite existing pages, so
/// the op must fail and retry instead.
async fn load_archive_index(
    archive_key: &RecordKey,
    keypair: &KeyPair,
    conv_secret: &SharedSecret,
    error_context: &'static str,
) -> Result<(ArchiveIndex, Option<Vec<u8>>)> {
    store::with_record(archive_key, Some(keypair), |key| {
        let secret = conv_secret.clone();
        async move {
            let index_bytes = store::load_structured(&key, 0, &secret, true).await?;
            let Some(b) = index_bytes else {
                return Ok((ArchiveIndex::default(), None));
            };
            spawn_blocking(move || match from_automerge_or_rescue::<ArchiveIndex>(&b) {
                Ok((index, rescued_doc)) => {
                    index.validate()?;
                    // On rescue the original bytes are unusable as a
                    // reconcile base — use the rebuilt doc instead.
                    Ok((index, Some(rescued_doc.unwrap_or_else(|| Vec::from(b)))))
                }
                Err(e) => Err(anyhow!("{error_context}: {e:#}")),
            })
            .await?
        }
    })
    .await
}

/// Subkey count of an archive record: index at subkey 0 + `ARCHIVE_MAX_PAGES`
/// pages. The configured page count (15 + 1 = 16) fits comfortably in `u16`.
const ARCHIVE_SUBKEYS: u16 = config::chat::ARCHIVE_MAX_PAGES + 1;

/// Derives the owner keypair for an archive chain generation together with
/// the record key that a record owned by it must have. Record keys are
/// `hash(kind ‖ owner ‖ schema)`, so the key is fully determined by the derivation —
/// creation is idempotent and any of the user's devices can re-derive the
/// writer.
async fn archive_owner_and_key(
    identity_keypair: &KeyPair,
    conv_key_str: &str,
    archive_epoch: u64,
    generation: u32,
) -> Result<(KeyPair, RecordKey)> {
    let owner =
        archive_owner_keypair(identity_keypair, conv_key_str, archive_epoch, generation).await?;
    let schema = DHTSchema::dflt(ARCHIVE_SUBKEYS)
        .map_err(|e| anyhow!("failed to create archive DFLT schema: {e}"))?;
    let key = ensure_local_api()
        .await?
        .get_dht_record_key(schema, owner.key(), None)
        .await
        .map_err(|e| anyhow!("failed to derive archive record key: {e}"))?;
    Ok((owner, key))
}

/// Ensures the archive record for `owner` exists (idempotent — the key is
/// deterministic), leaving it closed for callers' scoped opens.
async fn ensure_archive_record(owner: &KeyPair) -> Result<RecordKey> {
    let schema = DHTSchema::dflt(ARCHIVE_SUBKEYS)
        .map_err(|e| anyhow!("failed to create archive DFLT schema: {e}"))?;
    let key = store::ensure_record_with_keypair(schema, owner).await?;
    nonfatal!(
        "chat.record",
        "Failed to flush newly created archive record",
        store::flush_dht_record(&key).await
    );
    Ok(key)
}

/// Sequence ranges only narrow the lookup: two devices can allocate the
/// same sequence to different message IDs, so coverage must be ID-based.
async fn archive_page_already_written(
    index: &ArchiveIndex,
    archive_key: &RecordKey,
    secret: &SharedSecret,
    archive_page: &MessagePage,
) -> Result<bool> {
    if index.seq_ranges.is_empty() {
        return Ok(false);
    }
    let (start, end) = (archive_page.page_seq_start, archive_page.page_seq_end);
    let mut overlapping_subkeys = Vec::new();
    for (page_index, range) in index.seq_ranges.iter().enumerate() {
        if range.end >= start && range.start <= end {
            overlapping_subkeys.push(u32::try_from(page_index)?.saturating_add(1));
        }
    }
    if overlapping_subkeys.is_empty() {
        return Ok(false);
    }
    let mut missing: HashSet<u128> = archive_page
        .messages
        .iter()
        .map(message_dedup_key)
        .collect();
    store::with_record(archive_key, None, |key| {
        let secret = secret.clone();
        async move {
            for subkey in overlapping_subkeys {
                let Some(bytes) = store::load_structured(&key, subkey, &secret, true).await? else {
                    continue;
                };
                let (page, _): (MessagePage, _) =
                    spawn_blocking(move || from_automerge_or_rescue(&bytes)).await??;
                for message in &page.messages {
                    missing.remove(&message_dedup_key(message));
                }
                if missing.is_empty() {
                    return Ok(true);
                }
            }
            Ok(missing.is_empty())
        }
    })
    .await
}

/// Creates or reuses the archive record for the member's current chain
/// generation, writes the frozen page to the next available subkey, updates
/// the `ArchiveIndex` in subkey 0, and returns the updated feed meta.
///
/// Archive records are owned by a keypair derived from
/// `archive:{conv_key}:{generation}`, so their keys are deterministic:
/// creation is idempotent and no pending-key bookkeeping is needed. When the
/// current archive is full (`ARCHIVE_MAX_PAGES`), the next generation's
/// record is created and chained via `ArchiveIndex.next_archive_key`.
pub(super) async fn write_archive_page(
    archive_page: MessagePage,
    feed_meta: &mut ParticipantFeedMeta,
    conv_key_str: &str,
    conv_secret: &SharedSecret,
    identity_keypair: &KeyPair,
) -> Result<()> {
    'chain: loop {
        let (owner, expected_key) = archive_owner_and_key(
            identity_keypair,
            conv_key_str,
            feed_meta.archive_epoch,
            feed_meta.archive_generation,
        )
        .await?;

        ensure!(
            feed_meta.archive_key.is_empty() || feed_meta.archive_key == expected_key.to_string(),
            "Archive tail does not match its derived owner; refusing to discard history"
        );

        let had_published_archive = !feed_meta.archive_key.is_empty();
        let current_archive_key = if had_published_archive {
            expected_key
        } else {
            // Deterministic key — safe to "create" again on retry.
            ensure_archive_record(&owner).await?
        };
        if feed_meta.archive_root_key.is_empty() {
            // First write of a chain: either the very first archive ever
            // (epoch 0, nothing published) or the first write of a new epoch
            // after rollover (epoch stamped, tail not yet published). Both
            // stamp the traversal root here.
            ensure!(
                !had_published_archive,
                "Published archive is missing its root"
            );
            feed_meta.archive_root_key = current_archive_key.to_string();
        }

        let (index, index_doc) = load_archive_index(
            &current_archive_key,
            &owner,
            conv_secret,
            "Corrupt ArchiveIndex in write_archive_page",
        )
        .await?;
        ensure!(
            !had_published_archive || index_doc.is_some(),
            "Published archive is missing its index"
        );
        let mut generation = ChainedGeneration {
            owner,
            archive_key: current_archive_key,
            index,
            index_doc,
        };

        loop {
            if archive_page_already_written(
                &generation.index,
                &generation.archive_key,
                conv_secret,
                &archive_page,
            )
            .await?
            {
                feed_meta.archive_key = generation.archive_key.to_string();
                return Ok(());
            }
            if generation.index.page_count < u32::from(config::chat::ARCHIVE_MAX_PAGES) {
                break;
            }
            match link_next_generation(
                identity_keypair,
                conv_key_str,
                conv_secret,
                feed_meta,
                generation,
            )
            .await?
            {
                Some(next) => generation = next,
                // Depth-cap rollover: the meta was reset to the new epoch
                // (generation 0, no tail, no root); re-derive from the top so
                // this page lands in the fresh epoch's first record.
                None => continue 'chain,
            }
        }
        let current_archive_key_str = generation.archive_key.to_string();
        write_page_and_index(archive_page, generation, conv_secret).await?;
        feed_meta.archive_key = current_archive_key_str;
        return Ok(());
    }
}

/// Appends `page` to the next free subkey of the chain's current archive
/// record together with the updated `ArchiveIndex`, in one DHT transaction.
/// Archive pages are append-once — a superseded write means another device
/// claimed this page slot, which fails (and rolls back) the transaction so
/// the op retries against a re-read index.
async fn write_page_and_index(
    page: MessagePage,
    generation: ChainedGeneration,
    conv_secret: &SharedSecret,
) -> Result<()> {
    let ChainedGeneration {
        owner,
        archive_key: current_archive_key,
        mut index,
        index_doc,
    } = generation;
    let page_subkey = index.page_count + 1;
    index.page_count += 1;
    index.seq_ranges.push(SeqRange {
        start: page.page_seq_start,
        end: page.page_seq_end,
    });
    let (page_bytes, index_bytes) = spawn_blocking(move || {
        let page_bytes = reconcile_to_automerge(None, &page)?;
        let index_bytes = reconcile_to_automerge(index_doc.as_deref(), &index)?;
        Ok::<_, Error>((page_bytes, index_bytes))
    })
    .await??;

    store::with_record(&current_archive_key, Some(&owner), |key| async move {
        store::with_transaction(vec![key.clone()], |tx| async move {
            ensure!(
                tx.get_raw(&key, page_subkey).await?.is_none(),
                "Archive page slot was claimed concurrently; retry against its current index"
            );
            tx.set(&key, page_subkey, page_bytes, conv_secret, None)
                .await?;
            tx.set(&key, 0, index_bytes, conv_secret, None).await?;
            Ok(())
        })
        .await
    })
    .await?;
    Ok(())
}

/// Next-generation chain state returned by [`link_next_generation`] when the
/// current archive is full.
struct ChainedGeneration {
    owner: KeyPair,
    archive_key: RecordKey,
    index: ArchiveIndex,
    index_doc: Option<Vec<u8>>,
}

/// When the current archive generation is full (`ARCHIVE_MAX_PAGES`),
/// creates (idempotently) and links the next generation's record — signed by
/// the current generation's owner, advances `feed_meta.archive_generation`,
/// and loads the next generation's `ArchiveIndex`. Conflicting links are
/// reported instead of severing an existing history chain.
///
/// At `MAX_ARCHIVE_CHAIN_DEPTH` a rollover starts a NEW EPOCH instead:
/// `feed_meta.archive_epoch` is incremented from the previous epoch, so all
/// devices derive the identical next epoch from identical chain state,
/// generation resets to 0, and the tail/root keys are cleared so the next
/// chain derives fresh owners. Returns `None` to signal the caller to
/// re-derive from the reset meta. The prior chain's root is left on the DHT;
/// pre-rollover history stays retrievable from every device's local message
/// store (archives only serve cold hydration).
async fn link_next_generation(
    identity_keypair: &KeyPair,
    conv_key_str: &str,
    conv_secret: &SharedSecret,
    feed_meta: &mut ParticipantFeedMeta,
    current: ChainedGeneration,
) -> Result<Option<ChainedGeneration>> {
    let ChainedGeneration {
        owner,
        archive_key: current_archive_key,
        index,
        index_doc,
    } = current;
    let next_generation = feed_meta
        .archive_generation
        .checked_add(1)
        .context("Archive generation overflow")?;
    if config::chat::MAX_ARCHIVE_CHAIN_DEPTH <= next_generation as usize {
        let old_root = std::mem::take(&mut feed_meta.archive_root_key);
        feed_meta.archive_epoch = feed_meta
            .archive_epoch
            .checked_add(1)
            .context("Archive epoch overflow")?;
        feed_meta.archive_generation = 0;
        feed_meta.archive_key.clear();
        report_nonfatal(
            "chat.record",
            "Archive chain rolled over to a new epoch",
            format_args!(
                "conversation {conv_key_str}: prior chain root {old_root} remains on the DHT; \
                 pre-rollover history stays in local message stores"
            ),
        );
        return Ok(None);
    }
    let index_doc = index_doc.context("Full archive is missing its index document")?;
    let (next_owner, next_archive_key) = archive_owner_and_key(
        identity_keypair,
        conv_key_str,
        feed_meta.archive_epoch,
        next_generation,
    )
    .await?;
    let next_key_str = next_archive_key.to_string();

    ensure!(
        index.next_archive_key.is_empty() || index.next_archive_key == next_key_str,
        "Archive link does not match its derived owner; refusing to discard history"
    );
    if index.next_archive_key.is_empty() {
        let created = ensure_archive_record(&next_owner).await?;
        let mut linked = index;
        linked.next_archive_key = created.to_string();
        store::with_record(&current_archive_key, Some(&owner), |key| {
            let idx = linked;
            let secret = conv_secret.clone();
            async move {
                store::reconcile_write_open(&key, 0, &secret, true, Some(&index_doc), &idx).await?;
                Ok(())
            }
        })
        .await?;
    }
    feed_meta.archive_generation = next_generation;
    let (index, index_doc) = load_archive_index(
        &next_archive_key,
        &next_owner,
        conv_secret,
        "Corrupt chained ArchiveIndex in write_archive_page",
    )
    .await?;
    Ok(Some(ChainedGeneration {
        owner: next_owner,
        archive_key: next_archive_key,
        index,
        index_doc,
    }))
}
