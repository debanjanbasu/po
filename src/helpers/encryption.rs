//! Account-bound password hashing and the application pepper.
//!
//! A credential hash is Argon2id over the password, salted with the account's
//! 16-byte ID — public and deterministic, so a joining device reproduces it from
//! the pairing payload and two accounts sharing a password never share a hash.
//! The pepper is not a salt: it is the `derive_shared_secret` key that turns the
//! hash into the account-bound DHT secret, so it stays unrecoverable from stored
//! bytes. Accounts hashed before the account-ID salt keep their pepper-salted
//! hash ([`PasswordHashForm::Pepper`]): their DHT secret derives from exactly
//! those bytes, so re-salting them would strand everything they published.

use crate::{helpers::async_utils::spawn_blocking, veilid::core::ensure_local_api};
use anyhow::{Context as _, Error, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::SigningKey;
use std::sync::LazyLock;
use tokio::sync::OnceCell;
use veilid_core::{
    BareKeyPair, BarePublicKey, BareSecretKey, Crypto, CryptoSystemGuard, KeyPair, Nonce,
    SharedSecret, VALID_CRYPTO_KINDS, VeilidAPI, VeilidComponentGuard,
};
use zeroize::Zeroizing;

/// Returns the application pepper bytes, or an error if `PO_APP_PEPPER` was not set at build time.
/// The pepper is expected to be base64-encoded in the environment variable.
///
/// Used as the `derive_shared_secret` key over an account's credential hash and
/// as the pairing payload key, never as the Argon2 salt of a new account.
pub fn app_pepper() -> Result<&'static [u8]> {
    APP_PEPPER.as_deref().context(
        "PO_APP_PEPPER must be set at build time, be a valid base64 string, and not be empty",
    )
}

/// Salt form of an account's credential hash.
///
/// The hash is both the credential and the input to the account-bound DHT secret
/// ([`derive_dht_secret_from_hash`]), so an account's form is fixed for life:
/// re-salting it re-derives the DHT secret and strands every record, subkey, and
/// backup already encrypted under the old one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordHashForm {
    /// Salt = the account's 16-byte ID. Every account created from now on.
    AccountId,
    /// Salt = `PO_APP_PEPPER`. Accounts created before the account-ID salt.
    Pepper,
}

impl PasswordHashForm {
    /// Both forms, newest first — the order a credential check should try.
    pub const NEWEST_FIRST: [Self; 2] = [Self::AccountId, Self::Pepper];

    /// The account's credential hash for `password`.
    pub async fn hash(self, password: &str, account_id: &str) -> Result<String> {
        match self {
            Self::AccountId => hash_password_for_account(password, account_id).await,
            Self::Pepper => legacy_password_hash(password).await,
        }
    }
}

/// Argon2id PHC hash of `password` salted with the account ID — the credential
/// hash, and hence the DHT-secret input, of every account created from now on.
pub async fn hash_password_for_account(password: &str, account_id: &str) -> Result<String> {
    let salt = account_id_bytes(account_id)?;
    argon2_hash(password, &salt).await
}

/// Argon2id PHC hash of `password` salted with `PO_APP_PEPPER`.
///
/// Needed in exactly two places: reproducing the credential hash of an account
/// created before the account-ID salt (its DHT secret derives from these bytes),
/// and keying a pairing payload, whose key both devices must derive before the
/// joining one can see the account ID. Never a stored credential for a new
/// account — that salt is shared by every install of a build.
pub async fn legacy_password_hash(password: &str) -> Result<String> {
    let salt = Zeroizing::new(app_pepper()?.to_vec());
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

/// Lazily initialized compile-time pepper for [`app_pepper`].
/// The pepper is stored base64-encoded in the environment and decoded at runtime.
static APP_PEPPER: LazyLock<Option<Vec<u8>>> = LazyLock::new(|| {
    option_env!("PO_APP_PEPPER")
        .filter(|s| !s.is_empty())
        .map(str::trim) // Trim whitespace/newlines that might be in the env var
        .and_then(|s| STANDARD.decode(s).ok())
});

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
        b"oo:dht-secret:v2\0".len() + account_id.len() + password_hash.len(),
    ));
    material.extend_from_slice(b"oo:dht-secret:v2\0");
    material.extend_from_slice(account_id);
    material.extend_from_slice(password_hash);
    material
}

/// Derives the v2 account-bound DHT secret from an already-computed Argon2 hash.
///
/// Unchanged across the salt change by design: the pepper stays the
/// `derive_shared_secret` key, so every account — account-ID-salted or
/// pepper-salted — derives the same secret it always published under.
pub async fn derive_dht_secret_from_hash(hash: &str, account_id: &str) -> Result<SharedSecret> {
    let account_id = account_id_bytes(account_id)?;
    drop(get_cached_crypto_system().await?);
    let pepper = Zeroizing::new(app_pepper()?.to_vec());
    let hash = Zeroizing::new(hash.as_bytes().to_vec());
    spawn_blocking(move || {
        let cs = crypto_system_sync()?;
        let material = dht_secret_material(hash.as_slice(), &account_id);
        Ok(cs.derive_shared_secret(material.as_slice(), pepper.as_slice())?)
    })
    .await?
}

/// Derives the v2 account-bound DHT secret directly from a plaintext password,
/// in the salt form that account was created with. Both are correct only
/// together: the form is not recoverable from the password.
pub async fn derive_dht_secret_from_password(
    password: &str,
    account_id: &str,
    form: PasswordHashForm,
) -> Result<SharedSecret> {
    let hash = Zeroizing::new(form.hash(password, account_id).await?);
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
const SCOPED_KEYPAIR_DOMAIN: &[u8] = b"oo.scoped-keypair.v1";
