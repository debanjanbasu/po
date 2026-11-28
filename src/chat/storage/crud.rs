//! Per-conversation local CRUD choke-point: LRU conversation/pack caches and the
//! lock-load-mutate-persist loop with CRDT merge-on-persist (`mutate_for_account`).

use std::{
    collections::{BTreeSet, HashSet},
    num::NonZeroUsize,
    str,
    sync::{Arc, LazyLock, Mutex},
};

use crate::{
    chat::conversation::{self, ConversationIndex},
    chat::models::feed::{FeedMessage, MessageStatus},
    config,
    helpers::{
        spawn_blocking,
        store_lock::{lock_store, recover_poison},
    },
    nonfatal,
    storage::{
        account_table::{account_row_key, account_rows_in_column},
        schema_version::{SchemaVersionError, unpack_versioned},
        store_trait::{
            COL_AUTOMERGE, COL_SNAPSHOT, DUAL_FORMAT_TABLE_COLS, ReservedMeta,
            automerge_marker_key, delete_row, notify_model_changed, reconcile_snapshot_sidecar,
        },
    },
    user::ctx::UserCtx,
    veilid::table_store::Table,
};
use anyhow::{Context, Error, Result, ensure};
use futures_util::future::try_join_all;
use lru::LruCache;
use veilid_core::bytes::Bytes;

use super::{
    CONV_TABLE, LOCAL_CONV_SCHEMA_VERSION, LOCAL_CONV_TYPE_NAME, LocalConversation,
    codec::{SubModelCache, pack_local_conversation, unpack_local_conversation},
};

// ---------------------------------------------------------------------------
// Session-scoped LRU read cache
// ---------------------------------------------------------------------------

/// Maximum cached conversations. Sized to
/// [`config::chat::MAX_CONVERSATION_WATCHES`] so every watched conversation can
/// stay cached without thrashing.
///
/// The entry count is **not** a memory bound on its own: one conversation at
/// [`config::chat::MAX_LOCAL_MESSAGES`] × [`config::chat::MAX_MESSAGE_BYTES`] is
/// ~32 MB of message text, so 64 entries could reach ~2 GB. The byte ceilings
/// below are what actually bound it.
const CACHE_CAP: NonZeroUsize = match NonZeroUsize::new(config::chat::MAX_CONVERSATION_WATCHES) {
    Some(v) => v,
    None => unreachable!(),
};

/// Payload ceiling for one cached conversation.
///
/// 4 MB admits a full `MAX_LOCAL_MESSAGES` conversation of typical messages
/// (~2 KB each) while refusing the 16 KB-message worst case, which is read from
/// the table instead.
const CACHE_MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;

/// Resident payload ceiling across all cached conversations.
///
/// 8× the per-entry ceiling: a full set of ordinary conversations (64 × ~0.5 MB
/// ≈ 32 MB) still stays warm, and total resident payload can never exceed this
/// — down from the ~2 GB the entry count alone permitted.
const CACHE_MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

type CacheKey = Vec<u8>;

/// Payload bytes a cached conversation holds: message text plus roster and
/// key-ring strings, plus a fixed head per message for the struct itself.
///
/// Only used to decide admission and eviction, so allocator rounding is
/// ignored — but it deliberately sums the variable-length fields rather than
/// assuming the ~50-byte average the previous version of this comment claimed,
/// which under-counted the worst case by three orders of magnitude.
fn estimated_conversation_bytes(conv: &LocalConversation) -> usize {
    const MESSAGE_HEAD_BYTES: usize = std::mem::size_of::<FeedMessage>();
    let messages: usize = conv
        .messages
        .iter()
        .map(|message| {
            MESSAGE_HEAD_BYTES
                + message.content.len()
                + message.sender_username.len()
                + message.msg_id.len()
        })
        .sum();
    let members: usize = conv
        .roster
        .members
        .iter()
        .map(|member| member.username.len() + member.identity_key.len())
        .sum();
    let key_ring: usize = conv
        .key_ring
        .entries
        .values()
        .map(|entry| entry.identity_key.len() + entry.wrapped_key.len())
        .sum();
    messages + members + key_ring
}

/// Byte-accounted LRU of hydrated conversations.
///
/// An entry over [`CACHE_MAX_ENTRY_BYTES`] is **not cached at all** rather than
/// cached truncated. `mutate_for_account` persists the value it loaded —
/// `persist_locked` writes it straight through, without `persist_merged`'s
/// disk union — so a truncated cache hit would write the truncated history back
/// to the row. Refusing an oversized entry costs one table read per load;
/// truncating one costs messages.
struct CachedConversations {
    lru: LruCache<CacheKey, Arc<LocalConversation>>,
    /// Sum of [`estimated_conversation_bytes`] over `lru`, kept in step under
    /// the same lock so eviction and accounting cannot diverge.
    bytes: usize,
}

impl CachedConversations {
    fn new() -> Self {
        Self {
            lru: LruCache::new(CACHE_CAP),
            bytes: 0,
        }
    }

    fn get(&mut self, key: &[u8]) -> Option<Arc<LocalConversation>> {
        self.lru.get(key).cloned()
    }

    fn put(&mut self, key: CacheKey, conv: Arc<LocalConversation>) {
        let size = estimated_conversation_bytes(&conv);
        if size > CACHE_MAX_ENTRY_BYTES {
            // Oversized: evict any stale entry for this key and leave it
            // uncached, so readers and writers see the full row from the table.
            if let Some(old) = self.lru.pop(&key) {
                self.bytes = self
                    .bytes
                    .saturating_sub(estimated_conversation_bytes(&old));
            }
            return;
        }
        if let Some(old) = self.lru.put(key, conv) {
            self.bytes = self
                .bytes
                .saturating_sub(estimated_conversation_bytes(&old));
        }
        self.bytes = self.bytes.saturating_add(size);
        while self.bytes > CACHE_MAX_TOTAL_BYTES {
            let Some((_, evicted)) = self.lru.pop_lru() else {
                self.bytes = 0;
                break;
            };
            self.bytes = self
                .bytes
                .saturating_sub(estimated_conversation_bytes(&evicted));
        }
    }

    fn remove(&mut self, key: &[u8]) {
        if let Some(old) = self.lru.pop(key) {
            self.bytes = self
                .bytes
                .saturating_sub(estimated_conversation_bytes(&old));
        }
    }

    fn remove_all_for_account(&mut self, prefix: &[u8]) {
        self.lru.retain(|key, _| !key.starts_with(prefix));
        self.recount();
    }

    fn clear_all(&mut self) {
        self.lru.clear();
        self.bytes = 0;
    }

    /// Re-derives the byte total from the cache contents — used after a
    /// `retain`, where the evicted values are not returned.
    fn recount(&mut self) {
        self.bytes = self
            .lru
            .iter()
            .map(|(_, conv)| estimated_conversation_bytes(conv))
            .sum();
    }
}

/// Twin in-memory caches shared by the whole conversation store: hydrated
/// `LocalConversation` values (fast reloads) and their serialized sub-model
/// bytes (skip re-serialization of unchanged sub-models on the common
/// message-only update). Both maps live under one static so eviction and
/// invalidation stay in step and there is a single cache surface to reason
/// about. Locks are per-map: a value cache hit never blocks pack-cache work.
struct ConversationCaches {
    conversations: Mutex<CachedConversations>,
    packs: Mutex<LruCache<CacheKey, SubModelCache>>,
}

impl ConversationCaches {
    fn new() -> Self {
        Self {
            conversations: Mutex::new(CachedConversations::new()),
            packs: Mutex::new(LruCache::new(CACHE_CAP)),
        }
    }

    fn get(&self, key: &[u8]) -> Option<Arc<LocalConversation>> {
        recover_poison(self.conversations.lock()).get(key)
    }

    fn put(&self, key: Vec<u8>, conv: Arc<LocalConversation>) {
        recover_poison(self.conversations.lock()).put(key, conv);
    }

    fn remove(&self, key: &[u8]) {
        recover_poison(self.conversations.lock()).remove(key);
        recover_poison(self.packs.lock()).pop(key);
    }

    fn remove_all_for_account(&self, account_id: &str) {
        let prefix = account_row_key(account_id, &[]);
        recover_poison(self.conversations.lock()).remove_all_for_account(&prefix);
        recover_poison(self.packs.lock()).retain(|key, _| !key.starts_with(&prefix));
    }

    /// Clear all cached conversations. Called on session teardown so stale
    /// entries from a previous session never survive into the next login.
    fn clear_all(&self) {
        recover_poison(self.conversations.lock()).clear_all();
        recover_poison(self.packs.lock()).clear();
    }

    /// Takes (removes) a pack cache entry for the given key, returning a default
    /// if none exists. The caller is expected to pass it to
    /// `pack_local_conversation` and then store the updated cache back via
    /// [`Self::put_pack`].
    fn take_pack(&self, key: &[u8]) -> SubModelCache {
        recover_poison(self.packs.lock())
            .pop(key)
            .unwrap_or_default()
    }

    fn put_pack(&self, key: Vec<u8>, cache: SubModelCache) {
        recover_poison(self.packs.lock()).put(key, cache);
    }
}

static CACHES: LazyLock<ConversationCaches> = LazyLock::new(ConversationCaches::new);

/// Clear all cached conversations. Called on session teardown so stale entries
/// from a previous session never survive into the next login.
pub fn clear_cache() {
    CACHES.clear_all();
}

/// Composes a storage key scoped to the user's identity.
/// Uses `\0` separator since neither identity keys nor manifest keys contain null bytes.
fn storage_key(identity_key: &str, manifest_key: &str) -> Vec<u8> {
    account_row_key(identity_key, manifest_key.as_bytes())
}

async fn load_with_key(key: &[u8]) -> Result<(LocalConversation, SubModelCache)> {
    let table = Table::open(CONV_TABLE, DUAL_FORMAT_TABLE_COLS).await?;
    if let Some(bytes) = table.load_opt(COL_SNAPSHOT, key).await? {
        let decoded = spawn_blocking(move || {
            unpack_versioned(
                LOCAL_CONV_TYPE_NAME,
                LOCAL_CONV_SCHEMA_VERSION,
                &bytes,
                unpack_local_conversation,
            )
        })
        .await?;
        match decoded {
            Ok((decoded, _version)) => return Ok((decoded.conversation, decoded.pack_cache)),
            // Older envelope versions — or older SUBMODELS inside a current
            // envelope (e.g. pre-epoch feed meta) — are replaced; newer rows
            // are never discarded.
            Err(e)
                if matches!(
                    e.downcast_ref::<SchemaVersionError>(),
                    Some(SchemaVersionError::Older { .. })
                ) =>
            {
                nonfatal!(
                    "chat.storage",
                    "Replaced incompatible conversation snapshot from an older schema",
                    table.delete(COL_SNAPSHOT, key).await
                );
                return Ok((LocalConversation::default(), SubModelCache::default()));
            }
            Err(e) => return Err(e),
        }
    }
    Ok((LocalConversation::default(), SubModelCache::default()))
}

async fn save_with_key_owned(
    key: &[u8],
    conv: LocalConversation,
    pack_cache: SubModelCache,
) -> Result<(LocalConversation, SubModelCache)> {
    let (snapshot, conv, updated_cache) = spawn_blocking(move || {
        let mut cache = pack_cache;
        let bytes = pack_local_conversation(&conv, &mut cache)?;
        Ok::<_, Error>((bytes, conv, cache))
    })
    .await??;
    let table = Table::open(CONV_TABLE, DUAL_FORMAT_TABLE_COLS).await?;
    table.store(COL_SNAPSHOT, key, &snapshot).await?;
    Ok((conv, updated_cache))
}

fn notify_committed(account_id: &str, manifest_key: Option<&str>) {
    if UserCtx::try_current().is_none_or(|ctx| ctx.account_id.as_ref() != account_id) {
        return;
    }
    if let Some(key) = manifest_key {
        conversation::touch_last_feed_update(key);
    } else {
        conversation::touch_all_last_feed_updates();
    }
}
/// Bounds a conversation's local history at persist time.
///
/// Drops the oldest messages past [`config::chat::MAX_LOCAL_MESSAGES`], never
/// a [`MessageStatus::Pending`] one and never one at or after
/// `my_feed_meta.head_page_start_seq`. That pair is exactly what
/// `build_head_page` republishes (`sender == my_username && (seq >= head_start
/// || Pending)`, with Pending re-marked Sent), so every dropped message is
/// older head-page content that rotation already froze into the archive chain
/// — `resolve_head_and_meta` writes those pages before advancing
/// `head_page_start_seq`, and the chain only grows (it rolls to a new linked
/// generation rather than pruning). Peer messages below the boundary are
/// likewise recoverable from their sender's record, which
/// `archive_backfill_needed` already re-fetches.
///
/// Verified invariants this leans on (`chat/record/feed_sync` + `archive.rs`):
/// rotation archives a head page before `head_page_start_seq` moves past it;
/// an archive page is written before its index counts it; the chain never
/// prunes. What this cannot prove: messages from a member who has since left
/// the roster are only recoverable while the sender's device still serves the
/// chain, and `archive_backfill_needed` will keep flagging the trimmed prefix
/// as a gap, so archive-touching rounds re-merge the chain.
fn trim_conv_history(conv: &mut LocalConversation) {
    if conv.messages.len() <= config::chat::MAX_LOCAL_MESSAGES {
        return;
    }
    let head_start = conv.my_feed_meta.head_page_start_seq;
    let kept: Vec<FeedMessage> = conv
        .messages
        .iter()
        .filter(|message| message.status == MessageStatus::Pending || message.seq >= head_start)
        .cloned()
        .collect();
    if kept.len() >= conv.messages.len() {
        // Every message is protected (e.g. an old peer feed with sequences in
        // the same range): the cap cannot be enforced this pass, and a
        // pointless cache thrash is worse than the over-long history.
        return;
    }
    conv.messages = Arc::new(kept);
}

/// Commits `conv` under the per-row lock for `key`, updating the in-memory
/// read and pack caches on success or invalidating both on write failure so
/// unpersisted state is never served back to readers.
async fn persist_locked(
    account_id: &str,
    manifest_key: &str,
    key: Vec<u8>,
    mut conv: LocalConversation,
) -> Result<()> {
    trim_conv_history(&mut conv);
    let sub_cache = CACHES.take_pack(&key);
    match save_with_key_owned(&key, conv, sub_cache).await {
        Ok((committed, updated)) => {
            CACHES.put_pack(key.clone(), updated);
            CACHES.put(key, Arc::new(committed));
            notify_committed(account_id, Some(manifest_key));
            Ok(())
        }
        Err(e) => {
            // Never serve state the disk doesn't have.
            CACHES.remove(&key);
            Err(e)
        }
    }
}

/// Merge-on-persist save: loads the on-disk row and domain-merges `incoming`
/// into it before writing, so no save can ever lose messages — even one
/// racing a writer that bypassed the per-row lock.
///
/// Caller MUST hold the per-row lock for `storage_key(account_id, manifest_key)`
/// so the load→merge→write sequence is atomic against other writers.
///
/// On write failure the read cache entry is dropped so unpersisted state is
/// never served back to readers.
async fn persist_merged(
    account_id: &str,
    manifest_key: &str,
    incoming: LocalConversation,
) -> Result<()> {
    let key = storage_key(account_id, manifest_key);
    let (disk, sub_cache) = load_with_key(&key).await?;
    CACHES.put_pack(key.clone(), sub_cache);
    let mut merged = incoming;
    super::merge::merge_disk_into_incoming(&mut merged, disk);
    persist_locked(account_id, manifest_key, key, merged).await
}

/// Single choke-point for conversation mutations: per-row lock → load →
/// closure → merge-on-persist save.
///
/// Returns `Ok(true)` when the closure requested a save and it succeeded,
/// `Ok(false)` when the closure declined (no change / stale session). The
/// closure runs synchronously under the lock — no awaits inside; all DHT
/// work belongs outside the mutation.
pub async fn mutate_for_account(
    account_id: &str,
    manifest_key: &str,
    apply: impl FnOnce(&mut LocalConversation) -> bool,
) -> Result<bool> {
    let key = storage_key(account_id, manifest_key);
    let _guard = lock_store(&key).await;
    let mut conv = if let Some(cached) = CACHES.get(&key) {
        cached.as_ref().clone()
    } else {
        let (loaded, sub_cache) = load_with_key(&key).await?;
        CACHES.put_pack(key.clone(), sub_cache);
        loaded
    };
    if !apply(&mut conv) {
        return Ok(false);
    }
    persist_locked(account_id, manifest_key, key, conv).await?;
    Ok(true)
}

/// Load for a specific account — used by sync queue where the operation's
/// user may differ from the current `SESSION` signal.
///
/// Fast path: checks in-memory cache, then Cap'n Proto snapshot (col 0).
/// On cache miss, also seeds the pack cache from the raw sub-model bytes.
pub async fn load_for_account(
    account_id: &str,
    manifest_key: &str,
) -> Result<Arc<LocalConversation>> {
    let key = storage_key(account_id, manifest_key);
    let _guard = lock_store(&key).await;
    if let Some(cached) = CACHES.get(&key) {
        return Ok(cached);
    }
    let (conv, sub_cache) = load_with_key(&key).await?;
    let conv = Arc::new(conv);
    CACHES.put_pack(key.clone(), sub_cache);
    CACHES.put(key, Arc::clone(&conv));
    Ok(conv)
}

/// Save for a specific account — conversation creation only. Every other
/// writer must go through [`mutate_for_account`] so the per-row lock
/// serializes the read-modify-write. Takes the same per-row lock so a
/// creation is fully serialized against racing watcher updates: the merge
/// sees any row the watcher persisted, and its own write cannot clobber it.
pub async fn save_for_account(
    account_id: &str,
    manifest_key: &str,
    conv: LocalConversation,
) -> Result<()> {
    let key = storage_key(account_id, manifest_key);
    let _guard = lock_store(&key).await;
    persist_merged(account_id, manifest_key, conv).await
}

/// Delete a single conversation for a specific account.
/// Invalidates the in-memory cache entry.
pub async fn delete_for_account(account_id: &str, manifest_key: &str) -> Result<()> {
    let key = storage_key(account_id, manifest_key);
    let _guard = lock_store(&key).await;
    let table = Table::open(CONV_TABLE, DUAL_FORMAT_TABLE_COLS).await?;
    // Row, doc, and every reserved metadata entry in one batch (see
    // `store_trait::delete_row`): a surviving sidecar would be reconciled back
    // and re-marked current against a re-created snapshot.
    delete_row(&table, &key).await?;
    CACHES.remove(&key);
    notify_committed(account_id, Some(manifest_key));
    Ok(())
}

async fn snapshot_keys(table: &Table, account_id: &str) -> Result<Vec<(String, Bytes)>> {
    let raw = table.raw();
    let prefix = account_row_key(account_id, &[]);
    Ok(account_rows_in_column(&raw, COL_SNAPSHOT, &prefix)
        .await?
        .into_iter()
        .filter_map(|key| {
            let manifest_key = str::from_utf8(&key[prefix.len()..]).ok()?.to_owned();
            Some((manifest_key, key))
        })
        .collect())
}

/// Lists conversation manifest keys without loading their snapshot payloads.
pub async fn list_manifest_keys(account_id: &str) -> Result<Vec<String>> {
    let table = Table::open(CONV_TABLE, DUAL_FORMAT_TABLE_COLS).await?;
    Ok(snapshot_keys(&table, account_id)
        .await?
        .into_iter()
        .map(|(manifest_key, _)| manifest_key)
        .collect())
}

/// Export all conversations for an account as `(manifest_key, packed bytes)`.
///
/// Returns the raw Cap'n Proto snapshot rows — already versioned, so a later
/// import validates them on first load like any other stored row. Used by the
/// full-backup exporter.
pub async fn export_all_raw(account_id: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let table = Table::open(CONV_TABLE, DUAL_FORMAT_TABLE_COLS).await?;
    let loaded = try_join_all(snapshot_keys(&table, account_id).await?.into_iter().map(
        |(manifest_key, key)| {
            let table = &table;
            async move {
                Ok::<_, Error>(
                    table
                        .load_opt(COL_SNAPSHOT, &key)
                        .await?
                        .map(|bytes| (manifest_key, bytes.into())),
                )
            }
        },
    ))
    .await?;
    Ok(loaded.into_iter().flatten().collect())
}

/// Validate that raw bytes decode as a `LocalConversation` in a schema version
/// this build can read — an older envelope still decodes, a newer one is
/// refused. Used to check every row of a backup **before** any destructive
/// replace begins.
pub fn validate_raw(bytes: &[u8]) -> Result<()> {
    match unpack_versioned(
        LOCAL_CONV_TYPE_NAME,
        LOCAL_CONV_SCHEMA_VERSION,
        bytes,
        unpack_local_conversation,
    ) {
        Ok(_) => Ok(()),
        // Older is readable: Cap'n Proto fills fields added since with
        // defaults, the reader already yields that default for such a row, and
        // it re-hydrates from the DHT — so accept it rather than failing the
        // whole plan (identity, TOTP and contacts included, since `build_plan`
        // validates every section before applying any). Newer stays refused:
        // this build cannot see its fields.
        Err(e)
            if e.downcast_ref::<SchemaVersionError>()
                .is_some_and(SchemaVersionError::is_older) =>
        {
            Ok(())
        }
        Err(e) => Err(e).context("Backup conversation data failed to decode"),
    }
}

/// Bulk-import conversations from backup rows (`(manifest_key, packed bytes)`).
///
/// Every row is validated **before** the store is touched, then all deletes
/// and writes commit as **one atomic transaction** — a malformed backup or a
/// mid-restore failure can never leave the store half-wiped.
///
/// The `ConversationIndex` shares this table (its row key is the bare
/// `account_id`; per-conversation rows are `account_id\0manifest_key`), so a
/// replace-mode restore can pass the repacked index snapshot as
/// `index_snapshot` and the index + all conversation rows commit together.
/// The restored index is reconciled *into* its retained automerge doc, and the
/// resulting doc + currentness marker commit in that same transaction — so
/// entries absent from the backup become real CRDT deletes on the next
/// publish instead of a fresh-root doc that would race the DHT copy.
///
/// - `replace`: the account's existing conversation rows (snapshot,
///   automerge, and any marker sidecars) are deleted and every backup row
///   written, atomically. Conversation rows have no CRDT publish surface, so
///   their sidecars are simply dropped; only the index row's doc is retained
///   and reconciled into.
/// - merge (`replace = false`): only conversations that don't exist locally
///   are added; stale automerge state for those keys is cleared.
///   `index_snapshot` must be `None` — a merged index goes through CRDT merge.
///
/// Returns the number of conversation rows written.
fn validate_import_items(items: &[(String, Vec<u8>)]) -> Result<()> {
    let mut seen = HashSet::with_capacity(items.len());
    for (manifest_key, bytes) in items {
        ensure!(
            !manifest_key.is_empty(),
            "Backup conversation row has an empty key"
        );
        ensure!(
            seen.insert(manifest_key.as_str()),
            "Backup has duplicate conversation: {manifest_key}"
        );
        validate_raw(bytes).with_context(|| format!("Conversation {manifest_key}"))?;
    }
    Ok(())
}

/// Collect `(column, key)` deletes for every row of `account_id` in `table`.
///
/// The prefix scan itself lives in [`account_rows_in_column`]; this adds the
/// conversation table's special rule on top: the automerge column also
/// matches reserved-metadata sidecar prefixes, and snapshot rows are reported
/// separately so the caller can take their store locks first.
async fn collect_account_deletes(table: &Table, account_id: &str) -> Result<Vec<(u32, Vec<u8>)>> {
    let raw = table.raw();
    let prefix = account_row_key(account_id, &[]);
    // Reserved metadata prefixes for every row in this account's range — the
    // same enumeration `store_trait::delete_row` deletes for a single row, so a
    // range delete cannot leave a metadata entry the single-row wipe wouldn't.
    let meta_prefixes: Vec<Vec<u8>> = ReservedMeta::ALL
        .iter()
        .map(|meta| meta.key(&prefix))
        .collect();
    let mut deletes: Vec<(u32, Vec<u8>)> = account_rows_in_column(&raw, COL_SNAPSHOT, &prefix)
        .await?
        .into_iter()
        .map(|k| (COL_SNAPSHOT, k.into()))
        .collect();
    // Include orphaned automerge documents and metadata sidecars, not just rows
    // with snapshots, so stale CRDT state cannot survive deletion or replace.
    deletes.extend(
        account_rows_in_column(&raw, COL_AUTOMERGE, &prefix)
            .await?
            .into_iter()
            .filter(|k| k.starts_with(&prefix) || meta_prefixes.iter().any(|p| k.starts_with(p)))
            .map(|k| (COL_AUTOMERGE, k.into())),
    );
    Ok(deletes)
}

pub async fn import_all_raw(
    account_id: &str,
    items: &[(String, Vec<u8>)],
    index_snapshot: Option<&[u8]>,
    replace: bool,
) -> Result<usize> {
    ensure!(
        replace || index_snapshot.is_none(),
        "Index snapshot restore requires replace mode"
    );
    let owned_items: Vec<(String, Vec<u8>)> = items.to_vec();
    spawn_blocking(move || validate_import_items(&owned_items)).await??;

    let table = Table::open(CONV_TABLE, DUAL_FORMAT_TABLE_COLS).await?;

    // Hold the per-account store lock (shared with `sync::mutate`) across
    // scan → commit → cache invalidation, so a concurrent index reconcile
    // can't interleave with the restore. Ordering (account lock, then
    // `apply_batch` row locks) matches every other writer.
    let _account_guard = lock_store(account_id.as_bytes()).await;

    let mut deletes = if replace {
        collect_account_deletes(&table, account_id).await?
    } else {
        Vec::new()
    };

    let row_keys: BTreeSet<Vec<u8>> = items
        .iter()
        .map(|(key, _)| storage_key(account_id, key))
        .chain(
            deletes
                .iter()
                .filter(|(column, _)| *column == COL_SNAPSHOT)
                .map(|(_, key)| key.clone()),
        )
        .collect();
    let mut row_guards = Vec::with_capacity(row_keys.len());
    for key in row_keys {
        row_guards.push(lock_store(&key).await);
    }

    // Bare account_id key = the ConversationIndex CRDT row. Reconcile the
    // restored index *into* its retained doc (same mechanism as
    // `replace_local`) so entries absent from the backup become real CRDT
    // deletes on the next publish — a fresh-root doc would race the DHT copy
    // on `(lamport, actor)` and keep one root wholesale. The sidecar is owned
    // here so the borrows into `stores` stay valid until the batch commits.
    let index_sidecar = if let Some(snapshot) = index_snapshot {
        let existing_doc = table
            .load_opt(COL_AUTOMERGE, account_id.as_bytes())
            .await?
            .map(Into::into);
        Some(reconcile_snapshot_sidecar::<ConversationIndex>(existing_doc, snapshot).await?)
    } else {
        None
    };

    let mut stores: Vec<(u32, Vec<u8>, &[u8])> = Vec::new();
    if let (Some(snapshot), Some((doc, marker))) = (index_snapshot, &index_sidecar) {
        let index_key = account_id.as_bytes().to_vec();
        stores.push((COL_AUTOMERGE, index_key.clone(), doc.as_slice()));
        stores.push((
            COL_AUTOMERGE,
            automerge_marker_key(&index_key),
            marker.as_slice(),
        ));
        stores.push((COL_SNAPSHOT, index_key, snapshot));
    }
    let existing_keys: HashSet<Bytes> = if replace {
        HashSet::new()
    } else {
        table
            .raw()
            .get_keys(COL_SNAPSHOT)
            .await?
            .into_iter()
            .collect()
    };
    let mut written = 0usize;
    for (manifest_key, bytes) in items {
        let key = storage_key(account_id, manifest_key);
        if !replace {
            if existing_keys.contains(key.as_slice()) {
                continue;
            }
            // Stale automerge state (if any) belongs to the replaced snapshot:
            // the doc goes, and so does every reserved entry keyed to it —
            // a surviving watermark would gate the new doc against the old one.
            deletes.push((COL_AUTOMERGE, key.clone()));
            deletes.extend(
                ReservedMeta::ALL
                    .iter()
                    .map(|meta| (COL_AUTOMERGE, meta.key(&key))),
            );
        }
        stores.push((COL_SNAPSHOT, key, bytes.as_slice()));
        written += 1;
    }

    let delete_refs: Vec<(u32, &[u8])> = deletes.iter().map(|(c, k)| (*c, k.as_slice())).collect();
    let store_refs: Vec<(u32, &[u8], &[u8])> = stores
        .iter()
        .map(|(c, k, v)| (*c, k.as_slice(), *v))
        .collect();
    table.apply_batch(&delete_refs, &store_refs).await?;

    // Invalidate caches only after the commit succeeded.
    if replace {
        CACHES.remove_all_for_account(account_id);
    } else {
        for (manifest_key, _) in items {
            CACHES.remove(&storage_key(account_id, manifest_key));
        }
    }
    notify_committed(account_id, None);
    if index_snapshot.is_some() {
        notify_model_changed::<ConversationIndex>();
    }
    drop(row_guards);
    Ok(written)
}

/// Delete all conversations for a given account. Scans all keys in the table
/// and removes those prefixed with `{account_id}\0`.
/// Invalidates all cached entries for this account.
pub async fn delete_all_for_account(account_id: &str) -> Result<()> {
    let table = Table::open(CONV_TABLE, DUAL_FORMAT_TABLE_COLS).await?;
    let deletes = collect_account_deletes(&table, account_id).await?;
    let snapshot_keys: BTreeSet<Vec<u8>> = deletes
        .iter()
        .filter(|(column, _)| *column == COL_SNAPSHOT)
        .map(|(_, key)| key.clone())
        .collect();
    let mut row_guards = Vec::with_capacity(snapshot_keys.len());
    for key in snapshot_keys {
        row_guards.push(lock_store(&key).await);
    }

    let delete_refs: Vec<(u32, &[u8])> = deletes
        .iter()
        .map(|(col, key)| (*col, key.as_slice()))
        .collect();
    table.apply_batch(&delete_refs, &[]).await?;
    CACHES.remove_all_for_account(account_id);
    notify_committed(account_id, None);
    drop(row_guards);
    Ok(())
}
