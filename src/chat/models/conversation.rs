//! Conversation and group membership models.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::Result;
use autosurgeon::{Hydrate, Reconcile};
use dioxus::prelude::{GlobalSignal, ReadableExt, Signal};

use crate::{
    chat::manager,
    config,
    global_state::conversation_meta,
    helpers::{
        async_utils::now_epoch_millis,
        capnp::{capnp_u32, pack, unpack},
        store_lock::recover_poison,
    },
    models::{generated::chat_capnp, pending_operation::OpType},
    storage::{background_errors::report_nonfatal, store_trait::impl_crdt_model},
    sync::mutate::{MutationResult, mutate},
    user::ctx::UserCtx,
};

/// Lightweight reactive generation counter bumped only when typing state changes.
pub static TYPING_GENERATION: GlobalSignal<u64> = Signal::global(|| 0);

#[derive(Default)]
struct TypingEntry {
    version: u64,
    typing_at: u64,
    peers: HashSet<Arc<str>>,
}

impl TypingEntry {
    fn record_typing(&mut self, peer: Arc<str>, now: u64) {
        if self.typing_at > 0 && now.saturating_sub(self.typing_at) > config::typing::STALE_MS {
            self.peers.clear();
        }
        self.peers.insert(peer);
        self.typing_at = now;
        self.version = self.version.wrapping_add(1);
    }

    fn clear_peer(&mut self, peer: &str) -> bool {
        if self.peers.is_empty() || !self.peers.remove(peer) {
            return false;
        }
        if self.peers.is_empty() {
            self.typing_at = 0;
        }
        self.version = self.version.wrapping_add(1);
        true
    }
}
/// Per-conversation typing state and monotonic version counter. Non-reactive —
/// paired with [`TYPING_GENERATION`] so that a typing update neither wakes
/// `CONVERSATION_META` feed-update subscribers nor causes other conversations'
/// typing memos to re-evaluate.
static TYPING_VERSIONS: LazyLock<Mutex<HashMap<Arc<str>, TypingEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns the current typing version for a specific conversation key.
/// Used by `use_typing_state` to detect whether THIS conversation's typing
/// state actually changed, avoiding re-evaluation from other conversations.
pub fn typing_version_for(conv_key: &str) -> u64 {
    recover_poison(TYPING_VERSIONS.lock())
        .get(conv_key)
        .map_or(0, |entry| entry.version)
}

/// Timestamp (ms) of the most recent active typing signal in `conv_key`, or `0`.
pub fn typing_at_for(conv_key: &str) -> u64 {
    recover_poison(TYPING_VERSIONS.lock())
        .get(conv_key)
        .map_or(0, |entry| entry.typing_at)
}
/// Lightweight snapshot of conversation metadata for the `CONVERSATION_META`
/// global signal. Replaces anonymous `(u64, u64, u64)` tuples with named fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationMetaSnapshot {
    /// Bumped when new messages are merged from a remote feed, so the
    /// conversation-view `meta_version` memo triggers a message refresh.
    pub last_feed_update: u64,
}

pub fn ensure_meta_entry(conv_key: &str) {
    if conversation_meta().peek().contains_key(conv_key) {
        return;
    }
    conversation_meta().with_mut(|map| {
        map.entry(Arc::from(conv_key)).or_default();
    });
}

pub fn clear_meta_entry(conv_key: &str) {
    conversation_meta().write().remove(conv_key);
    recover_poison(TYPING_VERSIONS.lock()).remove(conv_key);
}

pub fn clear_all_meta_entries() {
    conversation_meta().write().clear();
    recover_poison(TYPING_VERSIONS.lock()).clear();
}

pub fn touch_last_feed_update(conv_key: &str) {
    if !conversation_meta().peek().contains_key(conv_key) {
        return;
    }
    conversation_meta().with_mut(|map| {
        if let Some(entry) = map.get_mut(conv_key) {
            entry.last_feed_update =
                now_epoch_millis().max(entry.last_feed_update.saturating_add(1));
        }
    });
}

pub(in crate::chat) fn touch_all_last_feed_updates() {
    let now = now_epoch_millis();
    conversation_meta().with_mut(|map| {
        for entry in map.values_mut() {
            entry.last_feed_update = now.max(entry.last_feed_update.saturating_add(1));
        }
    });
}

pub fn apply_typing_from_peer(conv_key: &Arc<str>, peer: Arc<str>, now: u64) {
    if !conversation_meta().peek().contains_key(conv_key.as_ref()) {
        return;
    }
    recover_poison(TYPING_VERSIONS.lock())
        .entry(Arc::clone(conv_key))
        .or_default()
        .record_typing(peer, now);
    TYPING_GENERATION.with_mut(|g| *g = g.wrapping_add(1));
}

pub fn clear_typing_from_peer(conv_key: &str, peer: &str) {
    let changed = recover_poison(TYPING_VERSIONS.lock())
        .get_mut(conv_key)
        .is_some_and(|entry| entry.clear_peer(peer));
    if changed {
        TYPING_GENERATION.with_mut(|g| *g = g.wrapping_add(1));
    }
}

/// An entry in the conversation index.
#[derive(Debug, Clone, PartialEq, Eq, Reconcile, Hydrate)]
pub struct ConversationEntry {
    pub conversation_record_key: String,
    pub contact_username: String,
    pub title: String,
    pub last_message_preview: String,
    pub last_message_at: u64,
    pub unread_count: u32,
}

impl ConversationEntry {
    pub fn new(conversation_key: &str, contact: &str, title: &str) -> Self {
        Self {
            conversation_record_key: conversation_key.to_string(),
            contact_username: contact.to_string(),
            title: title.to_string(),
            last_message_preview: String::new(),
            last_message_at: 0,
            unread_count: 0,
        }
    }
}

/// Index of all conversations (stored in conversations child record, subkey 0).
#[derive(Debug, Clone, Default, Reconcile, Hydrate)]
pub struct ConversationIndex {
    pub conversations: HashMap<String, ConversationEntry>,
    pub last_updated: u64,
}

impl ConversationIndex {
    pub fn add_entry(&mut self, entry: ConversationEntry) {
        if !self
            .conversations
            .contains_key(&entry.conversation_record_key)
        {
            self.last_updated = now_epoch_millis();
            self.conversations
                .insert(entry.conversation_record_key.clone(), entry);
        }
    }
}

const SCHEMA_VERSION: u32 = 1;

fn pack_conversation_index(index: &ConversationIndex) -> Result<Vec<u8>> {
    pack::<chat_capnp::ConversationIndex>(|mut root| {
        root.schema_version().set(SCHEMA_VERSION);
        root.last_updated().set(index.last_updated);
        let len = capnp_u32(index.conversations.len());
        let mut entries = root.conversations().init(len);
        for (i, entry) in index.conversations.values().enumerate() {
            let idx = capnp_u32(i);
            let mut e = entries.at(idx).get();
            e.conversation_record_key()
                .set_str(&entry.conversation_record_key);
            e.contact_username().set_str(&entry.contact_username);
            e.title().set_str(&entry.title);
            e.last_message_preview()
                .set_str(&entry.last_message_preview);
            e.last_message_at().set(entry.last_message_at);
            e.unread_count().set(entry.unread_count);
        }
    })
}

fn unpack_conversation_index(bytes: &[u8]) -> Result<(ConversationIndex, u32)> {
    unpack::<chat_capnp::ConversationIndex, _>(bytes, |root| {
        let entries = root.conversations().get();
        let mut conversations = HashMap::with_capacity(entries.len() as usize);
        for i in 0..entries.len() {
            let e = entries.at(i);
            let entry = ConversationEntry {
                conversation_record_key: e.conversation_record_key().get().as_str()?.to_string(),
                contact_username: e.contact_username().get().as_str()?.to_string(),
                title: e.title().get().as_str()?.to_string(),
                last_message_preview: e.last_message_preview().get().as_str()?.to_string(),
                last_message_at: e.last_message_at(),
                unread_count: e.unread_count(),
            };
            conversations.insert(entry.conversation_record_key.clone(), entry);
        }
        Ok((
            ConversationIndex {
                conversations,
                last_updated: root.last_updated(),
            },
            root.schema_version(),
        ))
    })
}

impl_crdt_model!(
    ConversationIndex,
    id = "conversations",
    display = "ConversationIndex",
    op = OpType::Chat,
    schema = SCHEMA_VERSION,
    pack = pack_conversation_index,
    unpack = unpack_conversation_index,
);

/// Loads the `ConversationIndex` for the current user, finds the entry matching
/// `conv_key`, applies `f`, saves, enqueues a sync op via the unified mutation
/// API. Local model commits invalidate the reactive list projection.
pub async fn mutate_entry<F>(conv_key: &str, f: F) -> bool
where
    F: FnOnce(&mut ConversationEntry) -> bool,
{
    let Some(ctx) = UserCtx::try_current() else {
        return false;
    };
    match mutate_entry_for_account(&ctx, conv_key, f).await {
        Ok(changed) => changed,
        Err(e) => {
            report_nonfatal("chat.conversation", "Failed to mutate ConversationIndex", e);
            false
        }
    }
}

async fn mutate_entry_for_account(
    ctx: &UserCtx,
    conv_key: &str,
    f: impl FnOnce(&mut ConversationEntry) -> bool,
) -> Result<bool> {
    mutate::<ConversationIndex, bool>(ctx, |index| {
        let Some(entry) = index.conversations.get_mut(conv_key) else {
            return Ok(MutationResult::unchanged(false));
        };
        if !f(entry) {
            return Ok(MutationResult::unchanged(false));
        }
        index.last_updated = now_epoch_millis();
        Ok(MutationResult::modified(true))
    })
    .await
}

/// Truncates `content` to ~50 chars and updates the conversation preview.
///
/// * **Own/replayed messages** (`unread_delta = None`): sets preview + timestamp.
/// * **Received merges** (`unread_delta = Some(n)`): adds the `n` messages this
///   merge inserted from other members, and replaces the preview text only when
///   `timestamp` is not older than what the preview already shows.
///
/// `n` comes from the merge itself, never from comparing timestamps: a peer
/// whose clock trails the newest message still gets counted (a timestamp gate
/// mutes that peer permanently, because `last_message_at` only rises).
pub async fn update_conversation_preview(
    conv_key: &str,
    content: &str,
    timestamp: u64,
    unread_delta: Option<u32>,
) {
    let Some(ctx) = UserCtx::try_current() else {
        return;
    };
    if let Err(error) =
        update_conversation_preview_for_account(&ctx, conv_key, content, timestamp, unread_delta)
            .await
    {
        report_nonfatal(
            "chat.conversation",
            "Failed to update conversation preview",
            error,
        );
    }
}

pub(in crate::chat) async fn update_conversation_preview_for_account(
    ctx: &UserCtx,
    conv_key: &str,
    content: &str,
    timestamp: u64,
    unread_delta: Option<u32>,
) -> Result<()> {
    let preview: String = content
        .chars()
        .take(config::chat::PREVIEW_MAX_CHARS)
        .collect();

    mutate_entry_for_account(ctx, conv_key, |entry| {
        let counted = if let Some(delta) = unread_delta
            && delta > 0
            && !manager::is_open_conversation(conv_key)
        {
            entry.unread_count = entry.unread_count.saturating_add(delta);
            true
        } else {
            false
        };
        // The timestamp gate only protects the preview text from an
        // out-of-order older message; it must not gate the unread count.
        if timestamp < entry.last_message_at
            || (timestamp == entry.last_message_at && preview == entry.last_message_preview)
        {
            return counted;
        }
        entry.last_message_at = timestamp;
        entry.last_message_preview = preview;
        true
    })
    .await?;
    Ok(())
}
