//! The `User` domain model and its Cap'n Proto persistence schema.
use std::sync::Arc;

use anyhow::{Result, bail};
use bytes::Bytes;
use thiserror::Error;
use veilid_core::{KeyPair, SharedSecret};
use zeroize::Zeroizing;

use super::sync_models::SyncPointers;
use crate::{
    helpers::capnp::{pack, unpack},
    models::generated::user_capnp::User as UserCapnp,
    storage::schema_version::{SchemaVersionError, unpack_versioned},
};

/// Veilid VLD0 nonce length (XChaCha20-Poly1305).
pub(super) const NONCE_LEN: usize = 24;

/// A stored user row whose bytes are structurally unusable: a nonce that is
/// not [`NONCE_LEN`] long, or a blank `account_id`.
///
/// A retry cannot change these bytes — the same row fails identically on every
/// attempt — so `retry.rs`'s permanent-error classifier matches this marker
/// instead of looping forever on a row it can never read.
#[derive(Debug, Error)]
#[error("malformed stored user row: {0}")]
pub struct MalformedUserRow(pub &'static str);

pub const CURRENT_CRYPTO_VERSION: u32 = 2;

/// Current schema version for the `User` Cap'n Proto struct.
pub(super) const USER_SCHEMA_VERSION: u32 = 2;

/// Immutable identity fields set at signup.
#[derive(Clone, Debug, Default)]
pub struct UserIdentity {
    pub(crate) username: Arc<str>,
    pub(crate) dht_key: Arc<str>,
    pub(crate) account_id: Arc<str>,
}

/// Encrypted keypair blob (password-AEAD-encrypted).
#[derive(Clone, Debug)]
pub struct EncryptedKeypair {
    pub(crate) ciphertext: Bytes,
    /// AEAD nonce — always 24 bytes (XChaCha20-Poly1305).
    pub(crate) nonce: [u8; NONCE_LEN],
}

impl Default for EncryptedKeypair {
    fn default() -> Self {
        Self {
            ciphertext: Bytes::new(),
            nonce: [0u8; NONCE_LEN],
        }
    }
}

/// Profile fields synced to DHT subkey 0.
#[derive(Clone, Debug, Default)]
pub struct UserProfile {
    pub(crate) dob: i32,
    pub(crate) terms_accepted: bool,
}

/// Security fields synced to DHT subkey 1.
#[derive(Clone, Debug, Default)]
pub struct UserSecurity {
    pub(crate) pin_hash: String,
    pub(crate) biometrics_enabled: bool,
}

/// Runtime-only fields (never serialized to Cap'n Proto).
#[derive(Clone, Debug, Default)]
pub struct UserRuntime {
    /// Decrypted Veilid keypair; `None` until the user logs in.
    pub keypair: Option<KeyPair>,
    /// Cached pointers to child DHT records; `None` until fetched.
    pub sync_pointers: Option<SyncPointers>,
    /// Transient DHT secret — set during login, consumed during full admission.
    /// Never serialized to Cap'n Proto.
    pub dht_secret: Option<SharedSecret>,
    /// Runtime-only token tying staged post-admission side effects to the
    /// credential or pairing attempt that produced this user snapshot.
    pub admission_id: Option<u64>,
    /// Transient Argon2 password hash from the credential or pairing attempt
    /// that produced this snapshot; consumed by remember-me staging so the
    /// password is never hashed twice. Never serialized to Cap'n Proto.
    pub password_hash: Option<Zeroizing<String>>,
}

/// Local user profile, stored as packed Cap'n Proto in Veilid's table store.
///
/// Composed of domain sub-structs: identity, encrypted keypair, profile,
/// security, and runtime state.
#[derive(Clone, Debug, Default)]
pub struct User {
    pub identity: UserIdentity,
    pub(crate) crypto_version: u32,
    pub encrypted_keypair: EncryptedKeypair,
    pub profile: UserProfile,
    pub security: UserSecurity,
    pub runtime: UserRuntime,
}

impl User {
    #[must_use]
    pub const fn crypto_version(&self) -> u32 {
        self.crypto_version
    }
    #[must_use]
    pub fn username(&self) -> &str {
        &self.identity.username
    }

    #[must_use]
    pub fn username_arc(&self) -> Arc<str> {
        Arc::clone(&self.identity.username)
    }

    #[must_use]
    pub fn dht_key(&self) -> &str {
        &self.identity.dht_key
    }
    #[must_use]
    pub fn dht_key_arc(&self) -> Arc<str> {
        Arc::clone(&self.identity.dht_key)
    }
    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.identity.account_id
    }
    #[must_use]
    pub fn account_id_arc(&self) -> Arc<str> {
        Arc::clone(&self.identity.account_id)
    }
    #[must_use]
    pub const fn terms_accepted(&self) -> bool {
        self.profile.terms_accepted
    }
    #[must_use]
    pub const fn dob_yyyymmdd(&self) -> i32 {
        self.profile.dob
    }
    #[must_use]
    pub fn pin_hash(&self) -> &str {
        &self.security.pin_hash
    }
    #[must_use]
    pub const fn has_pin(&self) -> bool {
        !self.security.pin_hash.is_empty()
    }
    #[must_use]
    pub const fn biometrics_enabled(&self) -> bool {
        self.security.biometrics_enabled
    }
    #[must_use]
    pub const fn nonce(&self) -> &[u8] {
        &self.encrypted_keypair.nonce
    }
    #[must_use]
    pub fn keypair_encrypted(&self) -> &[u8] {
        &self.encrypted_keypair.ciphertext
    }
    pub const fn set_dob(&mut self, dob_yyyymmdd: i32) {
        self.profile.dob = dob_yyyymmdd;
    }
    pub fn set_pin_hash(&mut self, pin_hash: impl Into<String>) {
        self.security.pin_hash = pin_hash.into();
    }
    pub const fn set_biometrics_enabled(&mut self, enabled: bool) {
        self.security.biometrics_enabled = enabled;
    }
    pub fn set_dht_key(&mut self, key: &str) {
        self.identity.dht_key = key.into();
    }
    pub const fn set_terms_accepted(&mut self, accepted: bool) {
        self.profile.terms_accepted = accepted;
    }
    pub fn set_sync_pointers(&mut self, pointers: SyncPointers) {
        self.runtime.sync_pointers = Some(pointers);
    }
    /// Creates a new user with default profile/security and the given identity + keypair.
    pub(crate) fn new(
        username: impl Into<Arc<str>>,
        dht_key: impl Into<Arc<str>>,
        account_id: impl Into<Arc<str>>,
        encrypted_keypair: EncryptedKeypair,
        keypair: KeyPair,
    ) -> Self {
        Self {
            identity: UserIdentity {
                username: username.into(),
                dht_key: dht_key.into(),
                account_id: account_id.into(),
            },
            crypto_version: CURRENT_CRYPTO_VERSION,
            encrypted_keypair,
            profile: UserProfile::default(),
            security: UserSecurity::default(),
            runtime: UserRuntime {
                keypair: Some(keypair),
                sync_pointers: None,
                dht_secret: None,
                admission_id: None,
                password_hash: None,
            },
        }
    }

    /// Returns `true` if the user still needs to complete post-registration steps.
    #[must_use]
    pub const fn needs_onboarding(&self) -> bool {
        !onboarding_complete(
            self.profile.terms_accepted,
            self.profile.dob,
            self.has_pin(),
        )
    }
}

/// Single source of truth for onboarding completion: terms accepted, DOB set,
/// PIN hash present.
///
/// Shared by [`User::needs_onboarding`], identity-pull classification
/// ([`crate::user::record::IdentityPull::is_onboarded`]), and the watcher
/// merge backstop that refuses onboarding regressions.
#[must_use]
pub(super) const fn onboarding_complete(
    terms_accepted: bool,
    dob_yyyymmdd: i32,
    pin_hash_set: bool,
) -> bool {
    terms_accepted && dob_yyyymmdd != 0 && pin_hash_set
}

impl User {
    pub fn to_packed_capnp(&self) -> Result<Vec<u8>> {
        pack_user(self)
    }

    pub(super) fn from_packed_capnp(bytes: &[u8]) -> Result<Self> {
        match unpack_versioned("User", USER_SCHEMA_VERSION, bytes, unpack_user) {
            Ok((user, _version)) => Ok(user),
            // Older rows decode anyway, deliberately: Cap'n Proto fills fields
            // added since with schema defaults, and `crypto_version` (absent →
            // LEGACY_CRYPTO_VERSION) is what the crypto ladder keys on to
            // upgrade those records. Refusing them here makes the ladder
            // unreachable and leaves "clear local app data" — which destroys
            // the password-encrypted keypair — as the only remedy. Newer rows
            // are still refused: this build cannot see their fields.
            Err(e)
                if e.downcast_ref::<SchemaVersionError>()
                    .is_some_and(SchemaVersionError::is_older) =>
            {
                Ok(unpack_user(bytes)?.0)
            }
            Err(e) => Err(e),
        }
    }
}

// ── Cap'n Proto codec ──────────────────────────────────────────────────

pub(super) fn pack_user(user: &User) -> Result<Vec<u8>> {
    pack::<UserCapnp>(|mut root| {
        root.schema_version().set(USER_SCHEMA_VERSION);
        root.username().set_str(&user.identity.username);
        let mut key_pair = root.key_pair().init();
        key_pair
            .ciphertext()
            .set_slice(&user.encrypted_keypair.ciphertext);
        key_pair.nonce().set_slice(&user.encrypted_keypair.nonce);
        root.dht_key().set_str(&user.identity.dht_key);
        root.dob().set(user.profile.dob);
        root.pin_hash().set_str(&user.security.pin_hash);
        root.terms_accepted().set(user.profile.terms_accepted);
        root.biometrics_enabled()
            .set(user.security.biometrics_enabled);
        root.account_id().set_str(&user.identity.account_id);
        root.crypto_version().set(user.crypto_version);
    })
}

pub(super) fn unpack_user(bytes: &[u8]) -> Result<(User, u32)> {
    unpack::<UserCapnp, _>(bytes, |root| {
        let version = root.schema_version();
        let key_pair = root.key_pair().get();
        let nonce_slice = key_pair.nonce().get().as_slice();
        if nonce_slice.len() != NONCE_LEN {
            bail!(MalformedUserRow("nonce length mismatch"));
        }
        let account_id = root.account_id().get().as_str()?;
        if account_id.is_empty() {
            bail!(MalformedUserRow("account_id missing"));
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(nonce_slice);
        Ok((
            User {
                identity: UserIdentity {
                    username: root.username().get().as_str()?.into(),
                    dht_key: Arc::from(root.dht_key().get().as_str()?),
                    account_id: Arc::from(account_id),
                },
                crypto_version: root.crypto_version(),
                encrypted_keypair: EncryptedKeypair {
                    ciphertext: Bytes::copy_from_slice(key_pair.ciphertext().get().as_slice()),
                    nonce,
                },
                profile: UserProfile {
                    dob: root.dob(),
                    terms_accepted: root.terms_accepted(),
                },
                security: UserSecurity {
                    pin_hash: root.pin_hash().get().as_str()?.to_string(),
                    biometrics_enabled: root.biometrics_enabled(),
                },
                runtime: UserRuntime::default(),
            },
            version,
        ))
    })
}
