//! Conversation manifest and roster models for group/direct chat records.

use std::collections::BTreeMap;

use crate::helpers::async_utils::now_epoch_millis;
use autosurgeon::{Hydrate, Reconcile};

/// Owner subkey indices within the SMPL record.
const SUBKEY_MANIFEST: u32 = 0;
const SUBKEY_ROSTER: u32 = 1;
const SUBKEY_KEY_RING: u32 = 2;

// ── Enums ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reconcile, Hydrate)]
pub enum MemberRole {
    Owner,
    #[default]
    Member,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reconcile, Hydrate)]
pub enum MemberStatus {
    #[default]
    Invited,
    Active,
}

// ── Structs ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Reconcile, Hydrate)]
pub struct MemberEntry {
    pub username: String,
    pub identity_key: String,
    pub role: MemberRole,
    pub status: MemberStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct ConversationManifest {
    pub title: String,
    pub owner_identity_key: String,
    pub created_at: u64,
}

impl ConversationManifest {
    pub fn new_direct(owner_key: &str, other_username: &str) -> Self {
        Self {
            title: other_username.to_string(),
            owner_identity_key: owner_key.to_string(),
            created_at: now_epoch_millis(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct ConversationRoster {
    pub members: Vec<MemberEntry>,
    pub last_updated: u64,
}

/// A single entry in the conversation key ring.
///
/// Contains a member's identity record address and the conversation key
/// HPKE-sealed to that identity's verified signing key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct KeyRingEntry {
    pub identity_key: String,
    pub wrapped_key: Vec<u8>,
}

/// Key ring for a conversation — distributes the shared symmetric key
/// to each member via HPKE. The outer envelope is public for bootstrapping;
/// the DHT record owner's signature authenticates it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct ConversationKeyRing {
    pub entries: BTreeMap<String, KeyRingEntry>,
    pub owner_identity_key: String,
}

impl ConversationKeyRing {
    /// Finds the wrapped key for a given identity key.
    pub fn find_entry(&self, identity_key: &str) -> Option<&KeyRingEntry> {
        self.entries.get(identity_key)
    }
}

impl ConversationKeyRing {
    /// Subkey index within the SMPL record.
    pub const SUBKEY: u32 = SUBKEY_KEY_RING;
}

impl ConversationRoster {
    pub fn add_member(&mut self, entry: MemberEntry) {
        if self
            .members
            .iter()
            .any(|m| m.identity_key == entry.identity_key)
        {
            return;
        }
        self.last_updated = now_epoch_millis();
        self.members.push(entry);
    }

    /// Index of `identity_key`'s member slot, active members only when
    /// `require_active` — `None` when the key is absent (or inactive and
    /// required active).
    #[must_use]
    pub fn my_member_index(&self, identity_key: &str, require_active: bool) -> Option<usize> {
        self.members.iter().position(|member| {
            member.identity_key == identity_key
                && (!require_active || member.status == MemberStatus::Active)
        })
    }

    #[must_use]
    pub fn peer_identity_keys(&self, exclude_identity: &str) -> Vec<String> {
        self.members
            .iter()
            .filter(|member| member.identity_key != exclude_identity)
            .map(|member| member.identity_key.clone())
            .collect()
    }
}

impl ConversationRoster {
    /// Subkey index within the SMPL record.
    pub const SUBKEY: u32 = SUBKEY_ROSTER;
}

impl ConversationManifest {
    /// Subkey index within the SMPL record.
    pub const SUBKEY: u32 = SUBKEY_MANIFEST;
}
