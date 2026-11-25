//! Syncable data models for DHT replication via automerge.
//!
//! These structs derive `Reconcile` + `Hydrate` from autosurgeon, enabling
//! automatic CRDT-based merging when the same identity is modified on
//! multiple devices. Subkey assignments live in
//! [`crate::user::identity_layout`] and are frozen.
//!
//! # Field evolution rules
//!
//! Documents written by older app versions must always hydrate on newer
//! ones. Autosurgeon's derived `Hydrate` *errors* on a missing field, so:
//!
//! - a new field must be `Option<T>` (hydrates as `None` when absent) or use
//!   a custom `hydrate_with` default — plain `String`/`bool`/numeric fields
//!   can never be added to a model that has already shipped;
//! - never remove or rename a field that another device may still write;
//! - never change a field's type in place — add an optional replacement
//!   field and migrate.

use std::{
    borrow::Borrow,
    collections::HashMap,
    convert::Infallible,
    fmt::{self, Display, Formatter},
    str::FromStr,
    sync::Arc,
};

use anyhow::{Result, anyhow};
use autosurgeon::{Hydrate, Reconcile};
use veilid_core::RecordKey;

use crate::{
    storage::store_trait::SyncSurface,
    user::{User, identity_layout},
    veilid::core::parse_record_key,
};

/// Profile data synced across devices (DHT subkey 0).
#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct SyncProfile {
    /// Carried for pairing convenience; identity, not synced state —
    /// `apply_to` never overwrites it.
    pub username: String,
    pub dob: i32,
    pub terms_accepted: bool,
}

impl From<&User> for SyncProfile {
    fn from(user: &User) -> Self {
        Self {
            username: user.username().to_string(),
            dob: user.dob_yyyymmdd(),
            terms_accepted: user.terms_accepted(),
        }
    }
}

impl SyncProfile {
    /// Apply the synced fields back onto a local user.
    pub const fn apply_to(&self, user: &mut User) {
        user.set_dob(self.dob);
        user.set_terms_accepted(self.terms_accepted);
    }
}

impl SyncSurface for SyncProfile {
    const ID: &'static str = "profile";
    const DISPLAY_NAME: &'static str = "SyncProfile";
    const SUBKEY: u32 = identity_layout::SUBKEY_PROFILE;
}

/// Security settings synced across devices (DHT subkey 1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct SyncSecurity {
    pub pin_hash: String,
    pub biometrics_enabled: bool,
}

impl From<&User> for SyncSecurity {
    fn from(user: &User) -> Self {
        Self {
            pin_hash: user.pin_hash().to_string(),
            biometrics_enabled: user.biometrics_enabled(),
        }
    }
}

impl SyncSecurity {
    /// Apply the synced fields back onto a local user.
    pub fn apply_to(&self, user: &mut User) {
        user.set_pin_hash(self.pin_hash.clone());
        user.set_biometrics_enabled(self.biometrics_enabled);
    }
}

impl SyncSurface for SyncSecurity {
    const ID: &'static str = "security";
    const DISPLAY_NAME: &'static str = "SyncSecurity";
    const SUBKEY: u32 = identity_layout::SUBKEY_SECURITY;
}

/// Identifies a feature stored in a child DHT record.
///
/// String-based to support both core features and plugins.
/// Core: `"totp"`, `"contacts"`, `"conversations"`.
/// Plugins: reverse-DNS like `"com.example.todos"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FeatureId(pub Arc<str>);

impl FeatureId {
    #[must_use]
    pub fn new(id: &str) -> Self {
        Self(Arc::from(id))
    }
}

impl Borrow<str> for FeatureId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl Display for FeatureId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for FeatureId {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Arc::from(s)))
    }
}

/// Pointer to a child DHT record and its expected layout size.
#[derive(Debug, Clone, PartialEq, Eq, Reconcile, Hydrate)]
pub struct ChildPointer {
    /// DHT record key as string.
    pub key: String,
    /// Number of subkeys the record was created with.
    pub subkey_count: u16,
}

impl ChildPointer {
    /// Parses the stored key into a typed `RecordKey`.
    pub fn record_key(&self) -> Result<RecordKey> {
        parse_record_key(&self.key).map_err(|e| anyhow!("invalid child pointer key: {e}"))
    }
}

/// Pointers to child DHT records (DHT subkey 4).
/// Each entry maps a feature identifier to its child record pointer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct SyncPointers {
    #[autosurgeon(with = "autosurgeon::map_with_parseable_keys")]
    pub pointers: HashMap<FeatureId, ChildPointer>,
}
