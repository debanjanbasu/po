//! Keypair encryption/decryption primitives shared by signup, login,
//! pairing, password change, and backup restore.

use crate::{
    helpers::encryption::{
        PasswordHashForm, crypto_system, crypto_system_sync, derive_dht_secret_from_hash,
        derive_dht_secret_from_password,
    },
    user::{EncryptedKeypair, NONCE_LEN, session_crypto::get_session_crypto},
};
use anyhow::{Result, anyhow, bail};
use std::str::{self, FromStr};
use veilid_core::{KeyPair, Nonce, SharedSecret};
use zeroize::Zeroizing;

/// Encrypts a Veilid keypair with AEAD using a DHT secret.
///
/// Shared helper used by signup, pairing, and password change flows.
/// Returns the encrypted keypair blob ready for storage.
pub(in crate::user) async fn encrypt_keypair(
    keypair: &KeyPair,
    secret: &SharedSecret,
) -> Result<EncryptedKeypair> {
    crypto_system!(crypto);
    let nonce = crypto.random_nonce();
    let plaintext_bytes = Zeroizing::new(keypair.to_string().into_bytes());
    let ciphertext =
        crypto.encrypt_aead(&plaintext_bytes, &nonce, secret, Some(b"oo:keypair:v2"))?;
    let nonce_bytes: [u8; NONCE_LEN] = nonce
        .bytes()
        .as_ref()
        .try_into()
        .map_err(|_| anyhow!("nonce length mismatch"))?;
    Ok(EncryptedKeypair {
        ciphertext: ciphertext.into(),
        nonce: nonce_bytes,
    })
}

/// Decrypts a keypair from encrypted AEAD data using a password hash.
/// Shared helper used by login, password change, and pairing flows.
pub(in crate::user) async fn decrypt_keypair(
    password_hash: &[u8],
    account_id: &str,
    encrypted: &[u8],
    nonce_bytes: &[u8],
) -> Result<(KeyPair, SharedSecret)> {
    let hash = str::from_utf8(password_hash)?;
    let shared_secret = derive_dht_secret_from_hash(hash, account_id).await?;
    let keypair = decrypt_keypair_with_secret(encrypted, nonce_bytes, &shared_secret)?;
    Ok((keypair, shared_secret))
}

pub(in crate::user) fn decrypt_keypair_with_secret(
    encrypted: &[u8],
    nonce_bytes: &[u8],
    secret: &SharedSecret,
) -> Result<KeyPair> {
    let crypto = crypto_system_sync()?;
    let nonce = Nonce::new(nonce_bytes);
    let decrypted = Zeroizing::new(
        crypto
            .decrypt_aead(encrypted, &nonce, secret, Some(b"oo:keypair:v2"))
            .map_err(|_| anyhow!("decryption failed"))?,
    );
    Ok(KeyPair::from_str(str::from_utf8(&decrypted)?)?)
}

/// Verifies that a backup identity's encrypted keypair decrypts under the
/// backup password (or active session secret when unchanged) and matches the
/// active session's keypair for `username`.
pub async fn verify_backup_keypair(
    username: &str,
    backup_password: &str,
    backup_account_id: &str,
    backup_nonce: &[u8],
    backup_ciphertext: &[u8],
) -> Result<()> {
    let session =
        get_session_crypto(username).ok_or_else(|| anyhow!("No active session crypto"))?;
    let keypair = if let Ok(kp) =
        decrypt_keypair_with_secret(backup_ciphertext, backup_nonce, &session.dht_secret)
    {
        kp
    } else {
        // Which salt form the backup used is not recorded anywhere; the backup's
        // keypair is the oracle. A backup exported before the account-ID salt is
        // keyed by the pepper-salted form and would otherwise not import.
        let mut opened = None;
        for form in PasswordHashForm::NEWEST_FIRST {
            let secret =
                derive_dht_secret_from_password(backup_password, backup_account_id, form).await?;
            if let Ok(kp) = decrypt_keypair_with_secret(backup_ciphertext, backup_nonce, &secret) {
                opened = Some(kp);
                break;
            }
        }
        opened.ok_or_else(|| anyhow!("Backup keypair could not be decrypted"))?
    };
    if session.keypair != keypair {
        bail!("Backup keypair does not match this account");
    }
    Ok(())
}
