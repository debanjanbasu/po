//! Per-session crypto material (keypair + DHT secret) with zeroization on drop.
use std::{
    collections::HashMap,
    hint::black_box,
    sync::{Arc, LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use crate::{helpers::store_lock::recover_poison, user::auth::decrypt_keypair_with_secret};
use dioxus::logger::tracing::warn;
use veilid_core::{BareKeyPair, BareSharedSecret, KeyPair, SharedSecret, VALID_CRYPTO_KINDS};

/// In-memory crypto context for a user session.
///
/// `Clone` is intentionally **not** derived — the canonical copy lives in
/// `SESSION_CRYPTO` and callers receive owned snapshots from [`get_session_crypto`].
///
/// Implements `Drop` to overwrite key material with zeros before deallocation.
/// Uses `black_box` to inhibit dead-store elimination by the compiler.
pub struct SessionCrypto {
    pub keypair: KeyPair,
    pub dht_secret: SharedSecret,
}

// SessionCrypto intentionally avoids Clone; callers use get_session_crypto.

impl Drop for SessionCrypto {
    fn drop(&mut self) {
        let kind = VALID_CRYPTO_KINDS[0];
        self.keypair = black_box(KeyPair::new(kind, BareKeyPair::default()));
        self.dht_secret = black_box(SharedSecret::new(kind, BareSharedSecret::default()));
    }
}

type CryptoMap = HashMap<Arc<str>, SessionCrypto>;

static SESSION_CRYPTO: LazyLock<RwLock<CryptoMap>> = LazyLock::new(|| RwLock::new(HashMap::new()));

fn read_map() -> RwLockReadGuard<'static, CryptoMap> {
    recover_poison(SESSION_CRYPTO.read())
}

fn write_map() -> RwLockWriteGuard<'static, CryptoMap> {
    recover_poison(SESSION_CRYPTO.write())
}

pub fn set_session_crypto(username: &str, keypair: KeyPair, dht_secret: SharedSecret) {
    write_map().insert(
        Arc::from(username),
        SessionCrypto {
            keypair,
            dht_secret,
        },
    );
}

/// Get an owned snapshot of session crypto for a username.
pub fn get_session_crypto(username: &str) -> Option<SessionCrypto> {
    let map = read_map();
    let entry = map.get(username)?;
    let snapshot = SessionCrypto {
        keypair: entry.keypair.clone(),
        dht_secret: entry.dht_secret.clone(),
    };
    drop(map);
    Some(snapshot)
}

pub(super) fn clear_session_crypto(username: &str) {
    write_map().remove(username);
}

/// Remove only the exact credential generation installed by a failed
/// admission. A keypair alone is not a generation: password rotations retain it.
pub fn clear_session_crypto_if_matches(username: &str, keypair: &KeyPair, secret: &SharedSecret) {
    let mut map = write_map();
    if map
        .get(username)
        .is_some_and(|c| &c.keypair == keypair && &c.dht_secret == secret)
    {
        map.remove(username);
    }
}

pub fn get_verified_session_crypto(user: &super::User) -> Option<SessionCrypto> {
    let session = get_session_crypto(user.username())?;
    if !decrypt_keypair_with_secret(user.keypair_encrypted(), user.nonce(), &session.dht_secret)
        .is_ok_and(|keypair| keypair == session.keypair)
    {
        warn!(
            "Ignoring stale session crypto for account {}",
            user.account_id()
        );
        return None;
    }
    Some(session)
}

/// Fills any missing runtime-crypto fields on `user` from live session
/// crypto, leaving already-populated fields untouched.
///
/// Returns `true` when session crypto was available for the user's username;
/// `false` means the runtime state was left as-is and callers needing crypto
/// must fail or fall back themselves.
pub(super) fn fill_runtime_crypto_from_session(user: &mut super::User) -> bool {
    let Some(session) = get_verified_session_crypto(user) else {
        return false;
    };
    if user.runtime.keypair.is_none() {
        // Clone, don't move: `SessionCrypto` implements `Drop`.
        user.runtime.keypair = Some(session.keypair.clone());
    }
    if user.runtime.dht_secret.is_none() {
        user.runtime.dht_secret = Some(session.dht_secret.clone());
    }
    true
}
