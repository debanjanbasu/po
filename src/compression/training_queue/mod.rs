//! Training queue for dictionary optimization.
//!
//! Persists collected pattern aggregates in `TableDB` using Cap'n Proto schemas.
//! See also: docs/compression.md.

mod patterns;
mod serialization;

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use bytes::Bytes;
use tokio::try_join;

use crate::{
    helpers::async_utils::{now_epoch_millis, now_epoch_secs},
    session::epoch::SessionGuard,
    storage::{account_table::account_row_key, schema_version::unpack_versioned},
    user::ctx::UserCtx,
    veilid::table_store::Table,
};

pub use self::patterns::{
    ChatPattern, StructuredPattern, cap_freq_map, extract_chat_pattern, extract_structured_pattern,
    patterns_to_training_samples,
};
use self::{
    patterns::{merge_chat_pattern, merge_structured_pattern},
    serialization::{
        deserialize_chat_patterns, deserialize_metadata, deserialize_structured_patterns,
        serialize_chat_patterns, serialize_metadata, serialize_structured_patterns,
    },
};

/// Table name for learned samples.
pub const SAMPLES_TABLE: &str = "po_learned_samples";

/// Number of columns in the samples table.
pub const SAMPLES_TABLE_COLS: u32 = 3;

/// Column indices.
const COL_CHAT_PATTERNS: u32 = 0;
const COL_STRUCTURED_PATTERNS: u32 = 1;
const COL_METADATA: u32 = 2;

const CHAT_PATTERNS_SCHEMA_VERSION: u32 = 1;
const STRUCTURED_PATTERNS_SCHEMA_VERSION: u32 = 1;
const TRAINING_METADATA_SCHEMA_VERSION: u32 = 1;

/// Metadata for the training queue.
#[derive(Debug, Clone, Default)]
pub(super) struct TrainingMetadata {
    pub(super) last_training: u64,
}

/// Aggregated patterns ready for training.
#[derive(Debug, Clone, Default)]
pub struct AggregatedPatterns {
    /// Word/n-gram frequencies.
    pub word_freq: HashMap<String, u64>,
    /// Emoji frequencies.
    pub emoji_freq: HashMap<String, u64>,
    /// Templates.
    pub templates: Vec<String>,
    /// Last updated timestamp.
    pub last_updated: u64,
}

// ── Table store persistence ──────────────────────────────────────────────────

const CHAT_PATTERNS_TYPE_NAME: &str = "LearnedSamples.ChatPatterns";
const STRUCTURED_PATTERNS_TYPE_NAME: &str = "LearnedSamples.StructuredPatterns";
const TRAINING_METADATA_TYPE_NAME: &str = "LearnedSamples.TrainingMetadata";

async fn load_versioned_column<T: Default>(
    col: u32,
    key: &[u8],
    type_name: &'static str,
    version: u32,
    unpack: fn(&[u8]) -> Result<(T, u32)>,
) -> Result<T> {
    let Some(bytes) = Table::open(SAMPLES_TABLE, SAMPLES_TABLE_COLS)
        .await?
        .load_opt(col, key)
        .await?
    else {
        return Ok(T::default());
    };
    let (value, _version) = unpack_versioned(type_name, version, &bytes, unpack)?;
    Ok(value)
}

/// Loads both aggregated pattern columns for an account concurrently.
async fn load_pattern_columns(
    chat_key: &[u8],
    structured_key: &[u8],
) -> Result<(AggregatedPatterns, AggregatedPatterns)> {
    try_join!(
        load_versioned_column(
            COL_CHAT_PATTERNS,
            chat_key,
            CHAT_PATTERNS_TYPE_NAME,
            CHAT_PATTERNS_SCHEMA_VERSION,
            deserialize_chat_patterns,
        ),
        load_versioned_column(
            COL_STRUCTURED_PATTERNS,
            structured_key,
            STRUCTURED_PATTERNS_TYPE_NAME,
            STRUCTURED_PATTERNS_SCHEMA_VERSION,
            deserialize_structured_patterns,
        ),
    )
}

/// Persist collected samples to the specified account's `TableDB` rows.
pub async fn persist_samples(
    ctx: &UserCtx,
    guard: SessionGuard,
    chat: &[ChatPattern],
    structured: &[StructuredPattern],
) -> Result<bool> {
    if chat.is_empty() && structured.is_empty() {
        return Ok(true);
    }
    let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
        return Ok(false);
    };
    let chat_key = account_row_key(&ctx.account_id, b"chat");
    let structured_key = account_row_key(&ctx.account_id, b"structured");
    let (mut aggregated_chat, mut aggregated_structured) =
        load_pattern_columns(&chat_key, &structured_key).await?;

    let mut existing_templates: HashSet<String> =
        aggregated_chat.templates.iter().cloned().collect();
    for pattern in chat {
        merge_chat_pattern(&mut aggregated_chat, pattern, &mut existing_templates);
    }
    for pattern in structured {
        merge_structured_pattern(&mut aggregated_structured, pattern);
    }
    aggregated_chat.cap_entries();
    aggregated_structured.cap_entries();

    // Keep revisions monotonic even when the wall clock moves backwards or
    // two flushes happen within the same millisecond.
    let now = now_epoch_millis()
        .max(aggregated_chat.last_updated.saturating_add(1))
        .max(aggregated_structured.last_updated.saturating_add(1));
    aggregated_chat.last_updated = now;
    aggregated_structured.last_updated = now;

    let chat_bytes = serialize_chat_patterns(&aggregated_chat)?;
    let struct_bytes = serialize_structured_patterns(&aggregated_structured)?;
    Table::open(SAMPLES_TABLE, SAMPLES_TABLE_COLS)
        .await?
        .apply_batch(
            &[],
            &[
                (COL_CHAT_PATTERNS, &chat_key, &chat_bytes),
                (COL_STRUCTURED_PATTERNS, &structured_key, &struct_bytes),
            ],
        )
        .await?;

    Ok(true)
}

/// Get the last training timestamp for an account.
pub async fn get_last_training(account_id: &str) -> Result<u64> {
    let metadata_key = account_row_key(account_id, b"metadata");
    Ok(load_versioned_column(
        COL_METADATA,
        &metadata_key,
        TRAINING_METADATA_TYPE_NAME,
        TRAINING_METADATA_SCHEMA_VERSION,
        deserialize_metadata,
    )
    .await?
    .last_training)
}

/// Record a training attempt without clearing accumulated patterns, so a
/// cycle that produced no dictionary swap still re-arms the interval debounce.
pub async fn touch_last_training(ctx: &UserCtx, guard: SessionGuard) -> Result<()> {
    let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
        return Ok(());
    };
    let metadata_key = account_row_key(&ctx.account_id, b"metadata");
    let bytes = serialize_metadata(&TrainingMetadata {
        last_training: now_epoch_secs(),
    })?;
    Table::open(SAMPLES_TABLE, SAMPLES_TABLE_COLS)
        .await?
        .store(COL_METADATA, &metadata_key, &bytes)
        .await?;
    Ok(())
}

/// Stamp a training attempt and clear its snapshot only if no later flush
/// changed either persisted pattern column for this account.
pub async fn reset_after_training(
    ctx: &UserCtx,
    guard: SessionGuard,
    expected_chat_updated: u64,
    expected_structured_updated: u64,
) -> Result<()> {
    let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
        return Ok(());
    };
    let chat_key = account_row_key(&ctx.account_id, b"chat");
    let structured_key = account_row_key(&ctx.account_id, b"structured");
    let metadata_key = account_row_key(&ctx.account_id, b"metadata");
    let (chat, structured) = load_pattern_columns(&chat_key, &structured_key).await?;

    let metadata_bytes = serialize_metadata(&TrainingMetadata {
        last_training: now_epoch_secs(),
    })?;
    if chat.last_updated != expected_chat_updated
        || structured.last_updated != expected_structured_updated
    {
        Table::open(SAMPLES_TABLE, SAMPLES_TABLE_COLS)
            .await?
            .store(COL_METADATA, &metadata_key, &metadata_bytes)
            .await?;
        return Ok(());
    }

    let empty = AggregatedPatterns::default();
    let empty_chat = serialize_chat_patterns(&empty)?;
    let empty_structured = serialize_structured_patterns(&empty)?;
    Table::open(SAMPLES_TABLE, SAMPLES_TABLE_COLS)
        .await?
        .apply_batch(
            &[],
            &[
                (COL_METADATA, &metadata_key, &metadata_bytes),
                (COL_CHAT_PATTERNS, &chat_key, &empty_chat),
                (COL_STRUCTURED_PATTERNS, &structured_key, &empty_structured),
            ],
        )
        .await?;

    Ok(())
}

/// Delete all training data for a given account.
///
/// One prefix scan per column (rows are `{account_id}\0{suffix}`), via the
/// shared [`AccountTable`] helper, instead of three hand-named keys — a new
/// suffix column can no longer be forgotten here.
pub async fn delete_all_for_account(account_id: &str) -> Result<()> {
    for col in [COL_CHAT_PATTERNS, COL_STRUCTURED_PATTERNS, COL_METADATA] {
        let table = Table::open(SAMPLES_TABLE, SAMPLES_TABLE_COLS).await?;
        let prefix = account_row_key(account_id, &[]);
        let keys: Vec<Bytes> = table
            .raw()
            .get_keys(col)
            .await?
            .into_iter()
            .filter(|key| key.starts_with(&prefix))
            .collect();
        let delete_refs: Vec<(u32, &[u8])> = keys.iter().map(|key| (col, key.as_ref())).collect();
        table.apply_batch(&delete_refs, &[]).await?;
    }
    Ok(())
}

/// Load all patterns for an account's training run.
pub async fn load_all_patterns(
    account_id: &str,
) -> Result<(AggregatedPatterns, AggregatedPatterns)> {
    let chat_key = account_row_key(account_id, b"chat");
    let structured_key = account_row_key(account_id, b"structured");
    load_pattern_columns(&chat_key, &structured_key).await
}
