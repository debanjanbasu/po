//! Pairing payload generation and decryption.
//!
//! A pairing payload contains everything a new device needs to join an
//! existing identity: the DHT record key, shared random account ID, and
//! Veilid keypair, encrypted with a one-time pairing PIN plus the account
//! password. It is transported via QR codes, deep links, or landing-page URLs.
//!
//! Frame: `account_id(16) ‖ nonce ‖ ciphertext`. The account ID travels in
//! the clear — it is public by design (it is the Argon2 salt of the account's
//! credential hash and appears in the account's DHT identity) — because the
//! joining device needs it to reproduce the account-ID-salted credential hash
//! that keys the payload, before it can decrypt anything. The version byte
//! sits inside the encrypted body and names the frame layout.

use std::{borrow::Cow, cell::RefCell};

use crate::{
    config,
    helpers::{
        async_utils::now_epoch_secs,
        encryption::{
            account_id_bytes, account_id_string, crypto_system, crypto_system_sync,
            encrypt_with_nonce, hash_password_for_account,
        },
        spawn_blocking,
        store_lock::lock_store,
    },
    platform::share_urls,
    user::{
        store::{self, AttemptOutcome},
        validation::validate_pairing_pin,
    },
};
use anyhow::{Error as AnyhowError, Result as AnyhowResult, anyhow};
use base64::{DecodeError, Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use thiserror::Error as ThisError;
use veilid_core::{
    BareKeyPair, BareOpaqueRecordKey, BarePublicKey, BareRecordKey, BareSecretKey,
    BareSharedSecret, CryptoKind, KeyPair, Nonce, RecordKey, VeilidAPIError,
};
use zeroize::Zeroizing;

/// Pairing payload version. The version byte lives in the encrypted body and
/// names the frame layout; anything else is rejected. This is the initial
/// version — the app is unreleased, so there is no history to carry.
const PAIRING_VERSION: u8 = 1;
/// Constant domain key for the pairing payload key — a domain separator, not
/// a secret. The payload key's entropy is the account's Argon2 credential
/// hash (password + account ID) plus the one-time PIN.
const PAIRING_DOMAIN_KEY: &[u8] = b"po:pair:v1:domain-key:0123456789abcdef";
const PAIRING_ACCOUNT_ID_LEN: usize = 16;
/// Body layout: version(1) + expiry(8) + kind(4) + record key(65) +
/// keypair(64) + account ID(16). The outer frame repeats the account ID in
/// the clear so the joiner can key the AEAD before decrypting.
const PAIRING_PLAINTEXT_LEN: usize =
    config::pairing::PAIRING_PLAINTEXT_LEN + PAIRING_ACCOUNT_ID_LEN;
const PAIRING_PAYLOAD_LEN: usize = config::pairing::PAIRING_PAYLOAD_LEN + PAIRING_ACCOUNT_ID_LEN;
/// Outer frame: account ID in the clear, then `nonce ‖ ciphertext`.
const PAIRING_OUTER_HEADER_LEN: usize = PAIRING_ACCOUNT_ID_LEN;
const PIN_RANGE: u64 = 1_000_000_000_000;

/// Stashed decrypt result: zeroizing plaintext plus the credential hash used for it.
type DecryptedSecret = AnyhowResult<(Zeroizing<Vec<u8>>, Zeroizing<String>)>;

#[derive(Debug, ThisError)]
pub enum PairingError {
    #[error("pairing PIN must be exactly 12 digits")]
    InvalidPin,
    #[error("pairing payload too large")]
    PayloadTooLarge,
    #[error("invalid base64url payload")]
    InvalidBase64Payload(#[source] DecodeError),
    #[error("payload too short")]
    PayloadTooShort,
    #[error("incorrect pairing PIN or account password")]
    InvalidCredentials,
    #[error("pairing code expired")]
    Expired,
    #[error("pairing payload is empty")]
    EmptyPayload,
    #[error("unsupported pairing payload version {0}")]
    UnsupportedVersion(u8),
    #[error("pairing payload malformed (expected {PAYLOAD_LEN} bytes, got {0})", PAYLOAD_LEN = PAIRING_PAYLOAD_LEN)]
    MalformedPayload(usize),
    #[error("pairing payload contains invalid bytes")]
    InvalidPayloadBytes,
    #[error("too many pairing attempts ({attempts}); try again in {remaining_secs}s")]
    Cooldown { attempts: u32, remaining_secs: u64 },
    #[error(transparent)]
    Internal(#[from] AnyhowError),
}

/// Key material for the payload AEAD key: the account's credential hash
/// (Argon2id over the password, salted with the account ID) plus the PIN.
fn pairing_key_material(password_hash: &[u8], pin: &str) -> Zeroizing<Vec<u8>> {
    let mut material = Zeroizing::new(Vec::with_capacity(
        b"po:pair:v1:".len() + password_hash.len() + pin.len(),
    ));
    material.extend_from_slice(b"po:pair:v1:");
    material.extend_from_slice(password_hash);
    material.extend_from_slice(pin.as_bytes());
    material
}

/// Body layout: version(1) + expiry(8) + kind(4) + record key(65) +
/// keypair(64) + account ID(16).
fn encode_plaintext(
    dht_key: &RecordKey,
    keypair: &KeyPair,
    account_id: &[u8; 16],
    expires_at: u64,
) -> Vec<u8> {
    let mut raw = Vec::with_capacity(PAIRING_PLAINTEXT_LEN);
    raw.push(PAIRING_VERSION);
    raw.extend_from_slice(&expires_at.to_le_bytes());
    raw.extend_from_slice(dht_key.kind().bytes());
    raw.extend_from_slice(&dht_key.ref_value().key().bytes());
    if let Some(enc) = dht_key.ref_value().encryption_key() {
        raw.push(1);
        raw.extend_from_slice(&enc.bytes());
    } else {
        raw.push(0);
        raw.extend_from_slice(&[0u8; 32]);
    }
    raw.extend_from_slice(&keypair.ref_value().key().bytes());
    raw.extend_from_slice(&keypair.ref_value().secret().bytes());
    raw.extend_from_slice(account_id);
    raw
}

fn decode_plaintext(data: &[u8]) -> Result<(RecordKey, KeyPair, String, u64), PairingError> {
    let Some((&version, payload)) = data.split_first() else {
        return Err(PairingError::EmptyPayload);
    };
    if version != PAIRING_VERSION {
        return Err(PairingError::UnsupportedVersion(version));
    }
    if payload.len() != PAIRING_PAYLOAD_LEN {
        return Err(PairingError::MalformedPayload(payload.len()));
    }

    let expires_at = u64::from_le_bytes(
        payload[0..8]
            .try_into()
            .map_err(|_| PairingError::InvalidPayloadBytes)?,
    );
    let kind = CryptoKind::new(
        payload[8..12]
            .try_into()
            .map_err(|_| PairingError::InvalidPayloadBytes)?,
    );
    let encryption_key = match payload[44] {
        1 => Some(BareSharedSecret::new(&payload[45..77])),
        0 => None,
        _ => return Err(PairingError::InvalidPayloadBytes),
    };
    let dht_bare = BareRecordKey::new(BareOpaqueRecordKey::new(&payload[12..44]), encryption_key);
    let bare_kp = BareKeyPair::new(
        BarePublicKey::new(&payload[77..109]),
        BareSecretKey::new(&payload[109..141]),
    );
    let account_id_start = config::pairing::PAIRING_PAYLOAD_LEN;
    let account_id_end = account_id_start + PAIRING_ACCOUNT_ID_LEN;
    let account_id: &[u8; PAIRING_ACCOUNT_ID_LEN] = payload[account_id_start..account_id_end]
        .try_into()
        .map_err(|_| PairingError::InvalidPayloadBytes)?;
    let account_id = account_id_string(account_id);
    Ok((
        RecordKey::new(kind, dht_bare),
        KeyPair::new(kind, bare_kp),
        account_id,
        expires_at,
    ))
}

/// Generates an encrypted pairing payload from the user's identity.
///
/// It is protected by the account password plus a uniformly generated 12-digit
/// PIN (~40 bits), mitigating offline PIN guesses against a captured QR even
/// when its password is known. The key is derived from the account's own
/// credential hash — Argon2id over the password, salted with the account ID —
/// which both devices can compute: the sharing device knows both, the joining
/// device reads the account ID from the frame's plaintext header. The payload
/// key itself is `derive_shared_secret(material, PAIRING_DOMAIN_KEY)` under a
/// constant domain key. Returns `(payload, PIN)`.
pub async fn generate_pairing_payload(
    dht_key: &RecordKey,
    keypair: &KeyPair,
    account_id: &str,
    password: &str,
) -> Result<(String, String), PairingError> {
    crypto_system!(crypto);
    let account_id = account_id_bytes(account_id)?;

    let pin = loop {
        let nonce = crypto.random_nonce();
        let random = u64::from_le_bytes(
            nonce.bytes()[..8]
                .try_into()
                .map_err(|_| PairingError::Internal(anyhow!("invalid nonce length")))?,
        );
        // Reject the incomplete range so each 12-digit value has identical odds.
        let accepted_below = u64::MAX - (u64::MAX % PIN_RANGE);
        if random < accepted_below {
            break random % PIN_RANGE;
        }
    };
    let pin_str = format!("{pin:012}");
    // The account's credential hash — the same one the account's DHT secret
    // derives from — keys the payload. The joiner recomputes it from the
    // password and the account ID in the frame header.
    let password_hash =
        Zeroizing::new(hash_password_for_account(password, &account_id_string(&account_id)).await?);
    let material = pairing_key_material(password_hash.as_bytes(), &pin_str);
    // Via `e2e` so a harness build can shorten the window; the value is the
    // constant in every other build.
    let expires_at = now_epoch_secs().saturating_add(config::e2e::pairing_ttl_secs());
    let plaintext = Zeroizing::new(encode_plaintext(dht_key, keypair, &account_id, expires_at));
    let header = account_id;

    let encoded = spawn_blocking(move || -> AnyhowResult<String> {
        let cs = crypto_system_sync()?;
        let pin_secret = cs.derive_shared_secret(&material, PAIRING_DOMAIN_KEY)?;
        let body = encrypt_with_nonce(&plaintext, &pin_secret, &cs)?;
        let mut frame = Vec::with_capacity(PAIRING_OUTER_HEADER_LEN + body.len());
        frame.extend_from_slice(&header);
        frame.extend_from_slice(&body);
        Ok(URL_SAFE_NO_PAD.encode(&frame))
    })
    .await??;

    Ok((encoded, pin_str))
}

/// Decrypts a pairing payload using the provided PIN.
/// Returns `(RecordKey, KeyPair, account_id, password_hash)` on success; the
/// hash is the account's credential hash (Argon2id over the password, salted
/// with the account ID from the frame header), so callers reuse it for
/// account-bound DHT-secret derivation and remember-me staging instead of
/// hashing the password again.
pub async fn decrypt_pairing_payload(
    base64_payload: &str,
    pin: &str,
    password: &str,
) -> Result<(RecordKey, KeyPair, String, Zeroizing<String>), PairingError> {
    if !validate_pairing_pin(pin) {
        return Err(PairingError::InvalidPin);
    }

    // Layout is 16 header bytes + 158 plaintext bytes + 24-byte nonce + 16-byte
    // AEAD tag. 1024 chars remains a generous base64 bound.
    if base64_payload.len() > config::pairing::MAX_BASE64_PAYLOAD_LEN {
        return Err(PairingError::PayloadTooLarge);
    }

    let data = URL_SAFE_NO_PAD
        .decode(base64_payload)
        .map_err(PairingError::InvalidBase64Payload)?;

    if data.len() < config::pairing::MIN_PAYLOAD_LEN {
        return Err(PairingError::PayloadTooShort);
    }

    crypto_system!(crypto);
    let nonce_len = crypto.nonce_length();
    let header_len = PAIRING_OUTER_HEADER_LEN;
    if data.len() < header_len + nonce_len {
        return Err(PairingError::PayloadTooShort);
    }
    // The frame header carries the account ID in the clear; the joiner needs
    // it to reproduce the credential hash that keys the payload.
    let account_id_bytes: [u8; PAIRING_ACCOUNT_ID_LEN] = data[..header_len]
        .try_into()
        .map_err(|_| PairingError::InvalidPayloadBytes)?;
    let account_id = account_id_string(&account_id_bytes);
    let body = &data[header_len..];
    let attempt_key = store::pairing_attempt_key(&body[..nonce_len]);
    let _guard = lock_store(&attempt_key).await;

    // Brute-force gate first: a locked-out payload must not force an Argon2
    // re-hash. The closure runs only while unlocked; it stashes the decrypt
    // result so the success path can reuse the plaintext and password hash.
    let decrypted: RefCell<Option<DecryptedSecret>> = RefCell::new(None);
    let outcome = store::gated_attempt(attempt_key.as_slice(), async || {
        let verified = match decrypt_pairing_secret(body, &account_id, password, pin).await {
            Ok(secret) => {
                *decrypted.borrow_mut() = Some(Ok(secret));
                true
            }
            Err(error)
                if matches!(
                    error.downcast_ref::<PairingError>(),
                    Some(PairingError::InvalidCredentials)
                ) =>
            {
                *decrypted.borrow_mut() = Some(Err(error));
                false
            }
            Err(error) => return Err(error),
        };
        Ok::<bool, AnyhowError>(verified)
    })
    .await?;

    let (plaintext, password_hash) = match outcome {
        AttemptOutcome::Verified => decrypted
            .into_inner()
            .unwrap_or_else(|| Err(anyhow!("pairing verification did not run")))?,
        // A decryption failure counts as one failed attempt.
        AttemptOutcome::Rejected { .. } => return Err(PairingError::InvalidCredentials),
        AttemptOutcome::Locked { attempts, until } => {
            return Err(PairingError::Cooldown {
                attempts,
                remaining_secs: until.saturating_sub(now_epoch_secs()),
            });
        }
    };

    let (dht_key, keypair, body_account_id, expires_at) = decode_plaintext(&plaintext)?;
    // The body repeats the account ID under the AEAD tag; a mismatch means
    // the header and body came from different frames.
    if body_account_id != account_id {
        return Err(PairingError::InvalidPayloadBytes);
    }
    if now_epoch_secs() >= expires_at {
        return Err(PairingError::Expired);
    }

    Ok((dht_key, keypair, account_id, password_hash))
}

/// Derives the pairing key from the account's credential hash (recomputed
/// from the password and the account ID in the frame header) plus the PIN,
/// then decrypts the payload body. Returns the plaintext plus that credential
/// hash, which is the account's own hash — callers reuse it for DHT-secret
/// derivation and remember-me staging.
async fn decrypt_pairing_secret(
    data: &[u8],
    account_id: &str,
    password: &str,
    pin: &str,
) -> AnyhowResult<(Zeroizing<Vec<u8>>, Zeroizing<String>)> {
    let password_hash = Zeroizing::new(hash_password_for_account(password, account_id).await?);
    let material = pairing_key_material(password_hash.as_bytes(), pin);
    let data = data.to_vec();
    let plaintext = spawn_blocking(move || -> AnyhowResult<Zeroizing<Vec<u8>>> {
        let cs = crypto_system_sync()?;
        let pin_secret = cs.derive_shared_secret(&material, PAIRING_DOMAIN_KEY)?;
        let nonce_len = cs.nonce_length();
        if data.len() < nonce_len {
            return Err(anyhow!("ciphertext too short for nonce"));
        }
        let nonce = Nonce::new(&data[..nonce_len]);
        // Both typed inputs come from this crypto system, so `Generic` here is
        // AEAD authentication failure, not a key/nonce initialization failure.
        let plaintext = match cs.decrypt_aead(&data[nonce_len..], &nonce, &pin_secret, None) {
            Ok(plaintext) => plaintext,
            Err(VeilidAPIError::Generic { .. }) => {
                return Err(PairingError::InvalidCredentials.into());
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Zeroizing::new(plaintext))
    })
    .await??;
    Ok((plaintext, password_hash))
}

/// Extracts the base64 payload from pairing input.
///
/// Accepts multiline share bundles, `po://pair?data=...`, canonical
/// universal/app-link paths (`https://po-app.pages.dev/p/...`), and raw
/// payloads. URL parsing is delegated to [`share_urls::parse_share_url`] so
/// pairing intake and share generation stay in sync.
#[must_use]
pub fn extract_pairing_payload(input: &str) -> Option<Cow<'_, str>> {
    let input = input.trim();

    // Pasted share messages wrap the real link in prose (title lines, expiry
    // notes, lookalike words). Try every recognized candidate — one that
    // fails to parse must not mask a valid one later in the text.
    for token in share_urls::pairing_share_token_candidates(input) {
        if let Some(share_urls::ShareUrl::Pairing { payload }) = share_urls::parse_share_url(token)
        {
            return Some(payload);
        }
    }

    if let Some(share_urls::ShareUrl::Pairing { payload }) = share_urls::parse_share_url(input) {
        return Some(payload);
    }

    (!input.contains("://") && !input.contains('\n')).then_some(Cow::Borrowed(input))
}
