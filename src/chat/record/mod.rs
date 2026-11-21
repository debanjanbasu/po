//! Chat DHT record CRUD — SMPL conversation record operations.
//!
//! Each conversation uses a single SMPL (multi-writer) DHT record. The owner
//! gets subkeys 0–2 (manifest, roster, key ring). Each member gets 2
//! subkeys (feed meta, head message page) at offset `3 + member_index * 2`.
//! See also: docs/dht-architecture.md, docs/sync-pipeline.md.

mod archive_backfill;
mod bootstrap;
mod catchup;
mod failed_hydrations;
mod feed_sync;
mod hydrate_all;
mod hydration;

use anyhow::Result;
use veilid_core::{KeyPair, ValueSubkeyRangeSet};

use crate::{
    chat::manifest::{ConversationKeyRing, ConversationManifest},
    helpers::{capnp::capnp_u32, encryption::derive_scoped_keypair},
};

// ── Shared constants ────────────────────────────────────────────────────

/// Owner subkey count in the SMPL schema (manifest, roster, key ring).
pub const OWNER_SUBKEY_COUNT: u16 = 3;

/// Subkeys per member (feed meta + head message page).
pub const MEMBER_SUBKEY_COUNT: u16 = 2;

// ── Shared helpers ──────────────────────────────────────────────────────

/// Computes the feed meta subkey for a member at the given roster index.
///
/// Overflow-safe against absurdly large indices; realistic rosters are
/// bounded by SMPL schema limits (max ~32 members).
pub fn member_feed_subkey(member_index: usize) -> u32 {
    u32::from(OWNER_SUBKEY_COUNT)
        .saturating_add(capnp_u32(member_index).saturating_mul(u32::from(MEMBER_SUBKEY_COUNT)))
}

/// Computes the head page subkey for a member at the given roster index.
pub fn member_page_subkey(member_index: usize) -> u32 {
    member_feed_subkey(member_index).saturating_add(1)
}

/// Maps either subkey belonging to a member to that member's roster index.
pub fn member_index_for_subkey(subkey: u32) -> Option<usize> {
    let offset = subkey.checked_sub(u32::from(OWNER_SUBKEY_COUNT))?;
    usize::try_from(offset / u32::from(MEMBER_SUBKEY_COUNT)).ok()
}

/// Derives the owner keypair for a conversation's SMPL DHT record.
///
/// Record keys are `hash(kind ‖ owner ‖ schema)` and the SMPL schema encodes only
/// the roster, so two conversations with the same roster owned by the raw
/// identity keypair would collide onto one record. Scoping the owner by the
/// manifest's creation timestamp makes every conversation record unique.
/// All of the owner's devices re-derive the same writer from the synced
/// identity keypair + the synced manifest.
pub async fn conv_owner_keypair(identity_keypair: &KeyPair, created_at: u64) -> Result<KeyPair> {
    derive_scoped_keypair(identity_keypair, &format!("conversation:{created_at}")).await
}

/// Derives the owner keypair for an archive record in a conversation feed's
/// archive chain. Scoped by conversation record key + chain epoch + generation
/// so every archive record is unique (they are all `DFLT(16)` — unscoped they
/// would all collide with each other and with the identity record). The
/// pre-epoch scope (`archive:{conv_key}:{generation}`) is kept verbatim so
/// records written before epochs existed remain reachable.
pub async fn archive_owner_keypair(
    identity_keypair: &KeyPair,
    conv_key: &str,
    archive_epoch: u64,
    generation: u32,
) -> Result<KeyPair> {
    let scope = if archive_epoch == 0 {
        format!("archive:{conv_key}:{generation}")
    } else {
        format!("archive:{conv_key}:e{archive_epoch}:{generation}")
    };
    derive_scoped_keypair(identity_keypair, &scope).await
}

/// Full subkey range covering every owner subkey (manifest, roster, key ring)
/// and every member's feed meta + head page subkeys.
///
/// Used when we need to (re-)watch the entire conversation record — e.g. on
/// full resync, watch renewal, or new-member join. Returns a single
/// contiguous range when there are no members (owner subkeys only).
pub fn full_conversation_subkeys(member_count: usize) -> ValueSubkeyRangeSet {
    let owner = ValueSubkeyRangeSet::single_range(
        ConversationManifest::SUBKEY,
        ConversationKeyRing::SUBKEY,
    );
    if member_count == 0 {
        return owner;
    }
    owner.union(&ValueSubkeyRangeSet::single_range(
        member_feed_subkey(0),
        member_page_subkey(member_count - 1),
    ))
}

// ── Re-exports (preserve public API) ────────────────────────────────────

pub use archive_backfill::{load_archive_chain, spawn_archive_backfill};
pub use bootstrap::{bootstrap_conversation_dht, build_local_conversation};
pub use catchup::{arm_and_catch_up, sweep_advanced_conversations, sweep_rearmed_conversation};
pub use failed_hydrations::{
    delete_all_for_account, retry_failed_hydrations, store_failed_hydrations,
};
pub use feed_sync::sync_conv_feed;
pub use hydrate_all::{HYDRATION_LOAD_FAILED_KEY, hydrate_all_conversations};
pub(in crate::chat) use hydration::decode_automerge;
use hydration::decode_page_messages;
pub use hydration::{hydrate_conversation, hydrate_conversation_heads};
