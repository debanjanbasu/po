//! Pack side of the local conversation store: automerge submodel packing with a
//! reuse cache, plus message-page part encoding for `TableDB` persistence.

use anyhow::Result;
use bytes::Bytes;

use crate::{
    chat::{
        feed::{FeedMessage, ParticipantFeedMeta},
        manifest::{ConversationKeyRing, ConversationManifest, ConversationRoster},
    },
    helpers::capnp::{capnp_u32, pack},
    models::generated::{group_chat_capnp, local_conversation_capnp},
};

pub(super) mod unpack;

pub(super) use unpack::unpack_local_conversation;

use super::{
    FEED_META_SCHEMA_VERSION, FEED_META_TYPE_NAME, KEY_RING_SCHEMA_VERSION, KEY_RING_TYPE_NAME,
    LOCAL_CONV_SCHEMA_VERSION, LocalConversation, MANIFEST_SCHEMA_VERSION, MANIFEST_TYPE_NAME,
    MESSAGE_PAGE_SCHEMA_VERSION, MESSAGE_PAGE_TYPE_NAME, ROSTER_SCHEMA_VERSION, ROSTER_TYPE_NAME,
};

// ---------------------------------------------------------------------------
// Sub-model serialization cache
// ---------------------------------------------------------------------------

/// A snapshot of a sub-model value paired with its serialized bytes.
/// Used to skip re-serialization when the value hasn't changed.
#[derive(Debug, Clone)]
pub(super) struct CachedEntry<T> {
    pub snapshot: T,
    pub bytes: Bytes,
}

/// Caches serialized Cap'n Proto bytes for each sub-model alongside a snapshot
/// of the parsed value that produced them. `pack_local_conversation` compares
/// the current field against each snapshot and reuses cached bytes when equal,
/// avoiding redundant serialization on message-only updates.
#[derive(Debug, Clone, Default)]
pub(super) struct SubModelCache {
    pub manifest: Option<CachedEntry<ConversationManifest>>,
    pub roster: Option<CachedEntry<ConversationRoster>>,
    pub key_ring: Option<CachedEntry<ConversationKeyRing>>,
    pub feed_meta: Option<CachedEntry<ParticipantFeedMeta>>,
}

/// Returns cached bytes if the current value matches the snapshot, otherwise
/// serializes fresh, updates the cache slot, and returns the new bytes.
fn pack_with_cache<T: PartialEq + Clone>(
    value: &T,
    cache_slot: &mut Option<CachedEntry<T>>,
    pack_fn: impl FnOnce(&T) -> Result<Vec<u8>>,
) -> Result<Bytes> {
    if let Some(entry) = cache_slot.as_ref().filter(|e| e.snapshot == *value) {
        return Ok(entry.bytes.clone());
    }
    let bytes = Bytes::from(pack_fn(value)?);
    *cache_slot = Some(CachedEntry {
        snapshot: value.clone(),
        bytes: bytes.clone(),
    });
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Individual sub-model pack functions
// ---------------------------------------------------------------------------

pub(super) fn pack_manifest(manifest: &ConversationManifest) -> Result<Vec<u8>> {
    pack::<group_chat_capnp::ConversationManifest>(|mut root| {
        root.schema_version().set(MANIFEST_SCHEMA_VERSION);
        root.title().set_str(&manifest.title);
        root.owner_identity_key()
            .set_str(&manifest.owner_identity_key);
        root.created_at().set(manifest.created_at);
    })
}

pub(super) fn pack_roster(roster: &ConversationRoster) -> Result<Vec<u8>> {
    let member_count = capnp_u32(roster.members.len());
    pack::<group_chat_capnp::ConversationRoster>(|mut root| {
        root.schema_version().set(ROSTER_SCHEMA_VERSION);
        root.last_updated().set(roster.last_updated);
        let mut members = root.members().init(member_count);
        for (index, member) in (0..member_count).zip(roster.members.iter()) {
            let mut entry = members.at(index).get();
            entry.username().set_str(&member.username);
            entry.identity_key().set_str(&member.identity_key);
            entry.role().set(member.role.into());
            entry.status().set(member.status.into());
        }
    })
}

pub(super) fn pack_key_ring(key_ring: &ConversationKeyRing) -> Result<Vec<u8>> {
    let entry_count = capnp_u32(key_ring.entries.len());
    pack::<group_chat_capnp::ConversationKeyRing>(|mut root| {
        root.schema_version().set(KEY_RING_SCHEMA_VERSION);
        root.owner_identity_key()
            .set_str(&key_ring.owner_identity_key);
        let mut entries = root.entries().init(entry_count);
        for (index, entry) in (0..entry_count).zip(key_ring.entries.values()) {
            let mut capnp_entry = entries.at(index).get();
            capnp_entry.identity_key().set_str(&entry.identity_key);
            capnp_entry.wrapped_key().set_slice(&entry.wrapped_key);
        }
    })
}

pub(super) fn pack_feed_meta(meta: &ParticipantFeedMeta) -> Result<Vec<u8>> {
    pack::<group_chat_capnp::ParticipantFeedMeta>(|mut root| {
        root.schema_version().set(FEED_META_SCHEMA_VERSION);
        root.last_seq().set(meta.last_seq);
        root.last_message_at().set(meta.last_message_at);
        root.archive_key().set_str(&meta.archive_key);
        root.head_page_start_seq().set(meta.head_page_start_seq);
        root.archive_generation().set(meta.archive_generation);
        root.archive_root_key().set_str(&meta.archive_root_key);
        root.archive_epoch().set(meta.archive_epoch);
    })
}

fn pack_message_page_parts(
    messages: &[FeedMessage],
    page_seq_start: u64,
    page_seq_end: u64,
) -> Result<Vec<u8>> {
    let message_count = capnp_u32(messages.len());
    pack::<group_chat_capnp::MessagePage>(|mut root| {
        root.schema_version().set(MESSAGE_PAGE_SCHEMA_VERSION);
        root.page_seq_start().set(page_seq_start);
        root.page_seq_end().set(page_seq_end);
        let mut message_list = root.messages().init(message_count);
        for (index, message) in (0..message_count).zip(messages.iter()) {
            let mut capnp_message = message_list.at(index).get();
            capnp_message.seq().set(message.seq);
            capnp_message
                .sender_username()
                .set_str(&message.sender_username);
            capnp_message.content().set_str(&message.content);
            capnp_message.timestamp().set(message.timestamp);
            capnp_message.msg_id().set_str(&message.msg_id);
            capnp_message.status().set(message.status.into());
        }
    })
}

/// Packs a `LocalConversation` into a Cap'n Proto snapshot.
///
/// Each sub-model is individually serialized with `group_chat.capnp` and stored
/// as `Data` payloads in the `LocalConversation` envelope. When a `SubModelCache`
/// is provided, unchanged sub-models (manifest, roster, `key_ring`, `feed_meta`)
/// are detected via `PartialEq` against their cached snapshots and their
/// previously serialized bytes are reused — skipping redundant Cap'n Proto
/// serialization on the common message-only update path.
///
/// Messages are always re-serialized since they change on every update.
pub(super) fn pack_local_conversation(
    conv: &LocalConversation,
    cache: &mut SubModelCache,
) -> Result<Vec<u8>> {
    let manifest_bytes = pack_with_cache(&conv.manifest, &mut cache.manifest, pack_manifest)?;
    let roster_bytes = pack_with_cache(&conv.roster, &mut cache.roster, pack_roster)?;
    let key_ring_bytes = pack_with_cache(&conv.key_ring, &mut cache.key_ring, pack_key_ring)?;
    let feed_meta_bytes =
        pack_with_cache(&conv.my_feed_meta, &mut cache.feed_meta, pack_feed_meta)?;

    // Messages always re-serialized — they change on every send/receive.
    let message_page_bytes = pack_message_page_parts(&conv.messages, 0, 0)?;

    pack::<local_conversation_capnp::LocalConversation>(|mut root| {
        root.schema_version().set(LOCAL_CONV_SCHEMA_VERSION);
        root.manifest().set_slice(&manifest_bytes);
        root.roster().set_slice(&roster_bytes);
        root.key_ring().set_slice(&key_ring_bytes);
        root.conv_key_bytes().set_slice(&conv.conv_key_bytes);
        root.my_feed_meta().set_slice(&feed_meta_bytes);
        root.message_page().set_slice(&message_page_bytes);
    })
}
