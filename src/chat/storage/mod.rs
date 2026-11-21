//! Single-table local storage for conversations.
//!
//! Replaces 6 separate `TableDB` tables (`oo_chat_manifests`, `oo_chat_rosters`,
//! `oo_chat_key_rings`, `oo_chat_conv_keys`, `oo_chat_feed_meta`,
//! `oo_chat_feed_pages`) with one `oo_conversations` table storing a
//! `LocalConversation` per manifest record key.
//!
//! Rows are not app-level encrypted: Veilid's `TableDB` AEAD-encrypts every key
//! and value at rest under a device key held in the OS protected store, so an
//! in-row AEAD would add a second lock on the same door (and would break the
//! cross-device backup path, which re-encrypts exported rows under the
//! account-bound password-derived key).
//!
//! Uses schema-backed snapshots:
//!   - Column 0: `LocalConversation` Cap'n Proto envelope
//!   - Embedded `Data` payloads are `group_chat.capnp` structs
//!   - Column 1: automerge docs + currentness markers for CRDT rows
//!     (e.g. the per-account `ConversationIndex`)

mod codec;
mod crud;
mod merge;
mod pending_invites;

/// Raw-value save — conversation creation/import paths only. All other
/// writers go through `mutate_for_account` so the per-row lock serializes
/// the read-modify-write.
pub(in crate::chat) use crud::save_for_account;
pub use crud::{
    clear_cache, delete_all_for_account, delete_for_account, load_for_account, mutate_for_account,
};
pub(in crate::chat) use pending_invites::{
    MAX_PENDING_INVITES, pending_invite_is_recent, sort_and_cap,
};
pub use pending_invites::{
    StoredPendingInvite, delete_all_pending_invites_for_account, delete_pending_invite_for_account,
    load_pending_invites_for_account, store_pending_invite_for_account,
};

use std::sync::Arc;

use crate::{
    chat::{
        feed::{FeedMessage, MessageStatus, ParticipantFeedMeta},
        manifest::{
            ConversationKeyRing, ConversationManifest, ConversationRoster, MemberRole, MemberStatus,
        },
    },
    config,
    helpers::capnp::capnp_enum_convert,
    models::generated::group_chat_capnp,
};

use veilid_core::{BareSharedSecret, SharedSecret, VALID_CRYPTO_KINDS};

/// Single-table name for all conversation data.
const CONV_TABLE: &str = "oo_conversations";

/// Current schema version for the `LocalConversation` Cap'n Proto struct.
// Older local snapshots are replaced by the storage read policy.
const LOCAL_CONV_SCHEMA_VERSION: u32 = 3;
const LOCAL_CONV_TYPE_NAME: &str = "LocalConversation";
const MANIFEST_SCHEMA_VERSION: u32 = 3;
const MANIFEST_TYPE_NAME: &str = "GroupChat.ConversationManifest";
const ROSTER_SCHEMA_VERSION: u32 = 3;
const ROSTER_TYPE_NAME: &str = "GroupChat.ConversationRoster";
const KEY_RING_SCHEMA_VERSION: u32 = 1;
const KEY_RING_TYPE_NAME: &str = "GroupChat.ConversationKeyRing";
const FEED_META_SCHEMA_VERSION: u32 = 2;
const FEED_META_TYPE_NAME: &str = "GroupChat.ParticipantFeedMeta";
const MESSAGE_PAGE_SCHEMA_VERSION: u32 = 3;
const MESSAGE_PAGE_TYPE_NAME: &str = "GroupChat.MessagePage";

capnp_enum_convert!(MemberRole => group_chat_capnp::MemberRole {
    Owner,
    Member,
});
capnp_enum_convert!(MemberStatus => group_chat_capnp::MemberStatus {
    Invited,
    Active,
});
capnp_enum_convert!(MessageStatus => group_chat_capnp::MessageStatus {
    Sent,
    Pending,
    Failed,
});

/// All conversation state cached locally, keyed by manifest record key string.
#[derive(Debug, Clone)]
pub struct LocalConversation {
    pub manifest: ConversationManifest,
    pub roster: ConversationRoster,
    pub key_ring: ConversationKeyRing,
    pub conv_key_bytes: Vec<u8>,
    pub my_feed_meta: ParticipantFeedMeta,
    pub messages: Arc<Vec<FeedMessage>>,
}

impl Default for LocalConversation {
    fn default() -> Self {
        Self {
            manifest: ConversationManifest::default(),
            roster: ConversationRoster::default(),
            key_ring: ConversationKeyRing::default(),
            conv_key_bytes: vec![0u8; config::crypto::KEY_LEN],
            my_feed_meta: ParticipantFeedMeta::default(),
            messages: Arc::new(Vec::new()),
        }
    }
}

impl LocalConversation {
    /// Creates a `LocalConversation` with the given conversation key.
    pub fn with_conv_key(secret: &SharedSecret) -> Self {
        Self {
            conv_key_bytes: Self::secret_to_bytes(secret),
            ..Self::default()
        }
    }

    /// Sets the conversation key from a `SharedSecret`.
    pub fn set_conv_key(&mut self, secret: &SharedSecret) {
        self.conv_key_bytes = Self::secret_to_bytes(secret);
    }

    fn secret_to_bytes(secret: &SharedSecret) -> Vec<u8> {
        let b = secret.value().bytes();
        let len = b.len().min(config::crypto::KEY_LEN);
        b[..len].to_vec()
    }

    /// Returns the conversation key bytes as a `SharedSecret`.
    pub fn conv_secret(&self) -> SharedSecret {
        let mut arr = [0u8; config::crypto::KEY_LEN];
        let len = self.conv_key_bytes.len().min(config::crypto::KEY_LEN);
        arr[..len].copy_from_slice(&self.conv_key_bytes[..len]);
        SharedSecret::new(VALID_CRYPTO_KINDS[0], BareSharedSecret::new(&arr))
    }

    pub fn has_conv_key(&self) -> bool {
        self.conv_key_bytes.len() == config::crypto::KEY_LEN
            && self.conv_key_bytes.iter().any(|&b| b != 0)
    }
}

#[derive(Debug, Clone)]
struct DecodedLocalConversation {
    conversation: LocalConversation,
    /// Pre-populated serialization cache from unpack, keyed by snapshot values.
    /// Enables `pack_local_conversation` to skip re-serializing unchanged sub-models.
    pack_cache: codec::SubModelCache,
}
