//! Account-bound password hashing and key derivation.
//!
//! A credential hash is Argon2id over the password, salted with the account's
//! 16-byte ID — public and deterministic, so a joining device reproduces it from
//! the pairing payload and two accounts sharing a password never share a hash.
//! The hash is turned into the account-bound DHT secret by
//! `derive_shared_secret(material, DHT_SECRET_DOMAIN_KEY)`: the key is a
//! compile-time constant domain separator, not a secret. All entropy lives in
//! the Argon2 hash, which an attacker without the password cannot compute, so
//! no build-time secret is needed and every build of the app derives the same
//! key for the same account.

use crate::{helpers::async_utils::spawn_blocking, veilid::core::ensure_local_api};
use anyhow::{Context as _, Error, Result, anyhow, bail};
use ed25519_dalek::SigningKey;
use tokio::sync::OnceCell;
use veilid_core::{
    BareKeyPair, BarePublicKey, BareSecretKey, Crypto, CryptoSystemGuard, KeyPair, Nonce,
    SharedSecret, VALID_CRYPTO_KINDS, VeilidAPI, VeilidComponentGuard,
};
use zeroize::Zeroizing;

/// Constant domain key for the account-bound DHT secret derivation.
///
/// This is a domain separator baked into the binary, not a secret: it only
/// keeps the app's derivations in their own domain. Secrecy comes from the
/// Argon2 credential hash in the material, which depends on the password and
/// the account ID. The domain key is a compile-time constant (this is the
/// initial, and only, version — the app is unreleased), identical in every
/// build: only someone who knows the password can derive the secret.
pub const DHT_SECRET_DOMAIN_KEY: &[u8] = b"po:dht-secret:v1:domain-key:0123456789abcdef";

/// Argon2id PHC hash of `password` salted with the account ID — the credential
/// hash, and hence the DHT-secret input, of every account.
pub async fn hash_password_for_account(password: &str, account_id: &str) -> Result<String> {
    let salt = account_id_bytes(account_id)?;
    argon2_hash(password, &salt).await
}

/// Argon2id PHC hash of `password` under `salt`.
async fn argon2_hash(password: &str, salt: &[u8]) -> Result<String> {
    // Ensure crypto cache is warm before entering blocking context
    drop(get_cached_crypto_system().await?);
    let password = Zeroizing::new(password.as_bytes().to_vec());
    let salt = Zeroizing::new(salt.to_vec());
    // Argon2 is intentionally slow — offload to background thread
    spawn_blocking(move || {
        let cs = crypto_system_sync()?;
        Ok(cs.hash_password(password.as_slice(), salt.as_slice())?)
    })
    .await?
}

/// Cached API handle — kept in a static so the `VeilidComponentGuard` borrows
/// have a stable `'static` reference. This is a separate cache from the main
/// `VEILID_API` Mutex; crypto operations survive a Veilid restart because the
/// `Crypto` subsystem is independent of network state.
static CRYPTO_API: OnceCell<VeilidAPI> = OnceCell::const_new();

/// Cached Crypto component guard (Arc internally, lives for the process).
static CRYPTO: OnceCell<VeilidComponentGuard<'static, Crypto>> = OnceCell::const_new();

/// Returns a sync crypto system guard, caching the Crypto component on first call.
pub async fn get_cached_crypto_system() -> Result<CryptoSystemGuard<'static>> {
    // Ensure the API handle is cached first (provides the 'static lifetime).
    CRYPTO_API
        .get_or_try_init(|| async { Ok::<_, Error>(ensure_local_api().await?) })
        .await?;

    let crypto = CRYPTO
        .get_or_try_init(|| async {
            let api = CRYPTO_API.get().context("crypto API not initialized")?;
            Ok::<_, Error>(api.crypto()?)
        })
        .await?;
    crypto
        .get(VALID_CRYPTO_KINDS[0])
        .ok_or_else(|| anyhow!("no supported crypto kind"))
}

/// Resolves the cached sync crypto system.
///
/// Usage: `crypto_system!(cs);` then use `cs.random_nonce()`, etc.
macro_rules! crypto_system {
    ($cs:ident) => {
        let $cs = crate::helpers::encryption::get_cached_crypto_system().await?;
    };
}

pub(crate) use crypto_system;

/// Decodes a canonical 128-bit account ID (32 lowercase hexadecimal characters).
pub fn account_id_bytes(account_id: &str) -> Result<[u8; 16]> {
    let bytes = account_id.as_bytes();
    if bytes.len() != 32
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        bail!("invalid account ID");
    }
    // Same validation as above, but the decode comes from the `hex` crate
    // instead of a manual radix parse.
    let mut out = [0u8; 16];
    hex::decode_to_slice(account_id, &mut out).map_err(|_| anyhow!("invalid account ID"))?;
    Ok(out)
}

/// Encodes a 128-bit account ID as 32 lowercase hexadecimal characters.
pub fn account_id_string(account_id: &[u8; 16]) -> String {
    hex::encode(account_id)
}

fn dht_secret_material(password_hash: &[u8], account_id: &[u8; 16]) -> Zeroizing<Vec<u8>> {
    let mut material = Zeroizing::new(Vec::with_capacity(
        b"po:dht-secret:v1\0".len() + account_id.len() + password_hash.len(),
    ));
    material.extend_from_slice(b"po:dht-secret:v1\0");
    material.extend_from_slice(account_id);
    material.extend_from_slice(password_hash);
    material
}

/// Derives the account-bound DHT secret from an already-computed Argon2
/// credential hash.
///
/// The `derive_shared_secret` key is the constant [`DHT_SECRET_DOMAIN_KEY`] —
/// a domain separator, not a secret — so every build derives the same secret
/// for the same account, and only the password hash (Argon2 over the password,
/// salted with the account ID) carries entropy.
pub async fn derive_dht_secret_from_hash(hash: &str, account_id: &str) -> Result<SharedSecret> {
    let account_id = account_id_bytes(account_id)?;
    drop(get_cached_crypto_system().await?);
    let hash = Zeroizing::new(hash.as_bytes().to_vec());
    spawn_blocking(move || {
        let cs = crypto_system_sync()?;
        let material = dht_secret_material(hash.as_slice(), &account_id);
        Ok(cs.derive_shared_secret(material.as_slice(), DHT_SECRET_DOMAIN_KEY)?)
    })
    .await?
}

/// Derives the account-bound DHT secret directly from a plaintext password.
pub async fn derive_dht_secret_from_password(
    password: &str,
    account_id: &str,
) -> Result<SharedSecret> {
    let hash = Zeroizing::new(hash_password_for_account(password, account_id).await?);
    derive_dht_secret_from_hash(&hash, account_id).await
}

/// Synchronous crypto system access (for use within `spawn_blocking`).
///
/// Returns a `CryptoSystemGuard` that can be used in blocking contexts
/// where async is not available.
pub fn crypto_system_sync() -> Result<CryptoSystemGuard<'static>> {
    CRYPTO
        .get()
        .and_then(|crypto| crypto.get(VALID_CRYPTO_KINDS[0]))
        .context("crypto system not initialized")
}

/// Encrypts payload with AEAD and prefixes nonce bytes.
pub fn encrypt_with_nonce(
    payload: &[u8],
    secret: &SharedSecret,
    crypto: &CryptoSystemGuard<'_>,
) -> Result<Vec<u8>> {
    encrypt_with_nonce_aad(payload, secret, crypto, None)
}

/// Like [`encrypt_with_nonce`] but binds associated data into the AEAD tag.
/// Use when a plaintext header must be tamper-evident (e.g. backup files).
pub fn encrypt_with_nonce_aad(
    payload: &[u8],
    secret: &SharedSecret,
    crypto: &CryptoSystemGuard<'_>,
    aad: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let nonce = crypto.random_nonce();
    let encrypted = crypto
        .encrypt_aead(payload, &nonce, secret, aad)
        .context("AEAD encryption failed")?;
    let nonce_bytes = nonce.bytes();
    let mut out = Vec::with_capacity(nonce_bytes.len() + encrypted.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend(encrypted);
    Ok(out)
}

/// Decrypts AEAD payload encoded as `[nonce || ciphertext]`.
pub fn decrypt_with_nonce(
    payload: &[u8],
    secret: &SharedSecret,
    crypto: &CryptoSystemGuard<'_>,
) -> Result<Vec<u8>> {
    decrypt_with_nonce_aad(payload, secret, crypto, None)
}

/// Like [`decrypt_with_nonce`] but authenticates associated data.
pub fn decrypt_with_nonce_aad(
    payload: &[u8],
    secret: &SharedSecret,
    crypto: &CryptoSystemGuard<'_>,
    aad: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let nonce_len = crypto.nonce_length();
    if payload.len() < nonce_len {
        bail!("ciphertext too short for nonce");
    }
    let nonce = Nonce::new(&payload[..nonce_len]);
    crypto
        .decrypt_aead(&payload[nonce_len..], &nonce, secret, aad)
        .context("AEAD decryption failed")
}

/// Derives a deterministic keypair scoped to a purpose string.
///
/// Veilid derives DHT record keys as `hash(owner_pubkey ‖ schema)` — records
/// created with the same owner keypair and schema collide onto one key. Every
/// logically distinct record we own therefore needs a distinct owner keypair.
/// This derives one from the identity keypair's secret plus a scope string,
/// so any device holding the identity keypair (they all do — it syncs via the
/// encrypted keypair blob) re-derives the same writer, while other users
/// never can.
///
/// Scope strings are conventions, not secrets: `child-record:{feature_id}`,
/// `conversation:{created_at}`, `archive:{conv_key}:{generation}`.
pub async fn derive_scoped_keypair(base: &KeyPair, scope: &str) -> Result<KeyPair> {
    let cs = get_cached_crypto_system().await?;

    // Domain-separated seed: hash(domain ‖ secret ‖ 0x00 ‖ scope).
    let secret_bytes = base.secret().value().bytes();
    let mut data = Zeroizing::new(Vec::with_capacity(
        SCOPED_KEYPAIR_DOMAIN.len() + secret_bytes.len() + 1 + scope.len(),
    ));
    data.extend_from_slice(SCOPED_KEYPAIR_DOMAIN);
    data.extend_from_slice(&secret_bytes);
    data.push(0);
    data.extend_from_slice(scope.as_bytes());
    let digest = cs.generate_hash(&data);

    let seed: Zeroizing<[u8; 32]> = Zeroizing::new(
        digest
            .value()
            .bytes()
            .as_ref()
            .try_into()
            .map_err(|_| anyhow!("hash digest is not 32 bytes"))?,
    );
    let signing = SigningKey::from_bytes(&seed);
    let public = signing.verifying_key().to_bytes();

    Ok(KeyPair::new(
        base.kind(),
        BareKeyPair::new(
            BarePublicKey::new(&public),
            BareSecretKey::new(seed.as_slice()),
        ),
    ))
}

/// Domain separator for [`derive_scoped_keypair`]. Never change — derived
/// record ownership depends on it.
const SCOPED_KEYPAIR_DOMAIN: &[u8] = b"po.scoped-keypair.v1";
