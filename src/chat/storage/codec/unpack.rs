//! Unpack side of the local conversation store: strict decode of manifest, roster,
//! key-ring, feed-meta, and page blobs with schema-version checks and cache seeding.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Result, anyhow};
use bytes::Bytes;

use crate::{
    chat::{
        feed::{FeedMessage, ParticipantFeedMeta, cmp_messages, sort_messages},
        manifest::{
            ConversationKeyRing, ConversationManifest, ConversationRoster, KeyRingEntry,
            MemberEntry,
        },
        storage::DecodedLocalConversation,
    },
    helpers::capnp::unpack,
    models::generated::{group_chat_capnp, local_conversation_capnp},
    storage::schema_version::unpack_versioned,
};

use super::{
    CachedEntry, FEED_META_SCHEMA_VERSION, FEED_META_TYPE_NAME, KEY_RING_SCHEMA_VERSION,
    KEY_RING_TYPE_NAME, LOCAL_CONV_SCHEMA_VERSION, LocalConversation, MANIFEST_SCHEMA_VERSION,
    MANIFEST_TYPE_NAME, MESSAGE_PAGE_SCHEMA_VERSION, MESSAGE_PAGE_TYPE_NAME, ROSTER_SCHEMA_VERSION,
    ROSTER_TYPE_NAME, SubModelCache,
};

fn unpack_manifest(bytes: &[u8]) -> Result<(ConversationManifest, u32)> {
    unpack::<group_chat_capnp::ConversationManifest, _>(bytes, |root| {
        Ok((
            ConversationManifest {
                title: root.title().get().as_str()?.to_string(),
                owner_identity_key: root.owner_identity_key().get().as_str()?.to_string(),
                created_at: root.created_at(),
            },
            root.schema_version(),
        ))
    })
}

fn unpack_roster(bytes: &[u8]) -> Result<(ConversationRoster, u32)> {
    unpack::<group_chat_capnp::ConversationRoster, _>(bytes, |root| {
        let members_capnp = root.members().get();
        let mut members = Vec::with_capacity(members_capnp.len() as usize);
        for i in 0..members_capnp.len() {
            let entry = members_capnp.at(i);
            let role = entry
                .role()
                .map_err(|_| anyhow!("invalid member role"))?
                .into();
            let status = entry
                .status()
                .map_err(|_| anyhow!("invalid member status"))?
                .into();
            members.push(MemberEntry {
                username: entry.username().get().as_str()?.to_string(),
                identity_key: entry.identity_key().get().as_str()?.to_string(),
                role,
                status,
            });
        }
        Ok((
            ConversationRoster {
                members,
                last_updated: root.last_updated(),
            },
            root.schema_version(),
        ))
    })
}

fn unpack_key_ring(bytes: &[u8]) -> Result<(ConversationKeyRing, u32)> {
    unpack::<group_chat_capnp::ConversationKeyRing, _>(bytes, |root| {
        let entries_capnp = root.entries().get();
        let mut entries = BTreeMap::new();
        for i in 0..entries_capnp.len() {
            let entry = entries_capnp.at(i);
            let identity_key = entry.identity_key().get().as_str()?.to_string();
            entries.insert(
                identity_key.clone(),
                KeyRingEntry {
                    identity_key,
                    wrapped_key: entry.wrapped_key().get().as_slice().to_vec(),
                },
            );
        }
        Ok((
            ConversationKeyRing {
                entries,
                owner_identity_key: root.owner_identity_key().get().as_str()?.to_string(),
            },
            root.schema_version(),
        ))
    })
}

fn unpack_feed_meta(bytes: &[u8]) -> Result<(ParticipantFeedMeta, u32)> {
    unpack::<group_chat_capnp::ParticipantFeedMeta, _>(bytes, |root| {
        Ok((
            ParticipantFeedMeta {
                last_seq: root.last_seq(),
                last_message_at: root.last_message_at(),
                archive_key: root.archive_key().get().as_str()?.to_string(),
                head_page_start_seq: root.head_page_start_seq(),
                archive_generation: root.archive_generation(),
                archive_root_key: root.archive_root_key().get().as_str()?.to_string(),
                archive_epoch: root.archive_epoch(),
            },
            root.schema_version(),
        ))
    })
}

fn unpack_message_page(bytes: &[u8]) -> Result<(Vec<FeedMessage>, u32)> {
    unpack::<group_chat_capnp::MessagePage, _>(bytes, |root| {
        let messages_capnp = root.messages().get();
        let mut messages = Vec::with_capacity(messages_capnp.len() as usize);
        for i in 0..messages_capnp.len() {
            let message = messages_capnp.at(i);
            let status = message
                .status()
                .map_err(|_| anyhow!("invalid message status"))?
                .into();
            messages.push(FeedMessage {
                seq: message.seq(),
                sender_username: Arc::from(message.sender_username().get().as_str()?),
                content: Arc::from(message.content().get().as_str()?),
                timestamp: message.timestamp(),
                msg_id: Arc::from(message.msg_id().get().as_str()?),
                status,
            });
        }
        Ok((messages, root.schema_version()))
    })
}

fn normalize_message_order(messages: &mut [FeedMessage]) {
    if !messages.is_sorted_by(|a, b| cmp_messages(a, b).is_le()) {
        sort_messages(messages);
    }
}

/// Stores a cache entry for nonempty serialized bytes.
fn seed_cache_entry<T: Clone>(value: &T, raw_bytes: &[u8]) -> Option<CachedEntry<T>> {
    if raw_bytes.is_empty() {
        None
    } else {
        Some(CachedEntry {
            snapshot: value.clone(),
            bytes: Bytes::copy_from_slice(raw_bytes),
        })
    }
}

/// Decodes a stored sub-model, defaulting empty data to the current schema.
fn decode_submodel<T>(
    data: &[u8],
    type_name: &'static str,
    schema_version: u32,
    default_fn: impl FnOnce() -> T,
    unpack_fn: impl Fn(&[u8]) -> Result<(T, u32)>,
) -> Result<T> {
    if data.is_empty() {
        Ok(default_fn())
    } else {
        unpack_versioned(type_name, schema_version, data, unpack_fn).map(|(value, _)| value)
    }
}

pub(in crate::chat::storage) fn unpack_local_conversation(
    bytes: &[u8],
) -> Result<(DecodedLocalConversation, u32)> {
    unpack::<local_conversation_capnp::LocalConversation, _>(bytes, |root| {
        let version = root.schema_version();
        if version != LOCAL_CONV_SCHEMA_VERSION {
            return Ok((
                DecodedLocalConversation {
                    conversation: LocalConversation::default(),
                    pack_cache: SubModelCache::default(),
                },
                version,
            ));
        }

        let manifest_data = root.manifest().get().as_slice();
        let manifest = decode_submodel(
            manifest_data,
            MANIFEST_TYPE_NAME,
            MANIFEST_SCHEMA_VERSION,
            || ConversationManifest::new_direct("", ""),
            unpack_manifest,
        )?;

        let roster_data = root.roster().get().as_slice();
        let roster = decode_submodel(
            roster_data,
            ROSTER_TYPE_NAME,
            ROSTER_SCHEMA_VERSION,
            ConversationRoster::default,
            unpack_roster,
        )?;

        let key_ring_data = root.key_ring().get().as_slice();
        let key_ring = decode_submodel(
            key_ring_data,
            KEY_RING_TYPE_NAME,
            KEY_RING_SCHEMA_VERSION,
            ConversationKeyRing::default,
            unpack_key_ring,
        )?;

        let conv_key_bytes = root.conv_key_bytes().get().as_slice().to_vec();

        let feed_meta_data = root.my_feed_meta().get().as_slice();
        let my_feed_meta = decode_submodel(
            feed_meta_data,
            FEED_META_TYPE_NAME,
            FEED_META_SCHEMA_VERSION,
            ParticipantFeedMeta::default,
            unpack_feed_meta,
        )?;

        let message_page_data = root.message_page().get().as_slice();
        let mut messages = decode_submodel(
            message_page_data,
            MESSAGE_PAGE_TYPE_NAME,
            MESSAGE_PAGE_SCHEMA_VERSION,
            Vec::new,
            unpack_message_page,
        )?;
        normalize_message_order(&mut messages);

        // Reuse verified current-schema bytes for unchanged sub-models.
        let pack_cache = SubModelCache {
            manifest: seed_cache_entry(&manifest, manifest_data),
            roster: seed_cache_entry(&roster, roster_data),
            key_ring: seed_cache_entry(&key_ring, key_ring_data),
            feed_meta: seed_cache_entry(&my_feed_meta, feed_meta_data),
        };

        Ok((
            DecodedLocalConversation {
                conversation: LocalConversation {
                    manifest,
                    roster,
                    key_ring,
                    conv_key_bytes,
                    my_feed_meta,
                    messages: Arc::new(messages),
                },
                pack_cache,
            },
            version,
        ))
    })
}
