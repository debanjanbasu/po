//! Domain merge for `LocalConversation` persistence.
//!
//! Every save funnels through [`merge_disk_into_incoming`]: the incoming
//! value is unioned with the row already on disk before the write. Message
//! loss is therefore impossible even for a writer that raced or bypassed the
//! per-row lock — the lock (via `mutate_for_account`) only guarantees
//! atomicity of the scalar last-writer-wins fields.

use std::sync::Arc;

use crate::chat::feed::union_messages_in_place;

use super::LocalConversation;

/// Merges the on-disk conversation into `incoming` (the writer's intent):
///
/// - `messages`: union by dedup key — never loses a message from either side;
///   incoming wins on a dedup match (status transitions must stick).
/// - `my_feed_meta`: field-wise monotonic max — every field only advances.
/// - `roster`: last-writer-wins by `last_updated`.
/// - `manifest`: incoming wins unless it is unset (`created_at == 0`).
/// - `key_ring`: entry union; incoming wins for matching identity keys.
/// - `conv_key_bytes`: a known key is never replaced by an unset one.
pub(super) fn merge_disk_into_incoming(incoming: &mut LocalConversation, disk: LocalConversation) {
    let disk_has_conv_key = disk.has_conv_key();
    let LocalConversation {
        manifest,
        roster,
        key_ring,
        conv_key_bytes,
        my_feed_meta,
        messages,
    } = disk;

    union_messages_in_place(&mut incoming.messages, Arc::unwrap_or_clone(messages));
    incoming.my_feed_meta.merge_from(my_feed_meta);

    if roster.last_updated > incoming.roster.last_updated {
        incoming.roster = roster;
    }
    if incoming.manifest.created_at == 0 && manifest.created_at != 0 {
        incoming.manifest = manifest;
    }
    for (identity_key, entry) in key_ring.entries {
        incoming
            .key_ring
            .entries
            .entry(identity_key)
            .or_insert(entry);
    }
    if incoming.key_ring.owner_identity_key.is_empty() {
        incoming.key_ring.owner_identity_key = key_ring.owner_identity_key;
    }
    if !incoming.has_conv_key() && disk_has_conv_key {
        incoming.conv_key_bytes = conv_key_bytes;
    }
}
