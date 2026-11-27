//! Password hashing and change flows for local accounts.
use super::{
    auth::encrypt_keypair,
    password_rotation::{enqueue_pending_rotation, prepare_rotation, retry_rotation_enqueue},
    require_user,
    session_crypto::set_session_crypto,
    store,
};
use crate::{
    config,
    helpers::{
        async_utils::{self, now_epoch_secs},
        encryption::{self, crypto_system},
        store_lock,
    },
    session::phase::{SESSION, mutate_authenticated_user},
    user::{CURRENT_CRYPTO_VERSION, ctx::UserCtx, validation::validate_new_password},
};
use anyhow::{Context, Error as AnyhowError, Result, anyhow};
use dioxus::prelude::ReadableExt;
use thiserror::Error;
use zeroize::Zeroizing;

pub use super::password_rotation::{
    PasswordRotationPending, lock_identity_sync, prepare_identity_sync,
};

/// Typed error for password-change failures.
#[derive(Debug, Error)]
pub enum PasswordChangeError {
    #[error("Incorrect password")]
    IncorrectPassword,
    #[error("Invalid new password")]
    InvalidNewPassword,
    #[error("Password updated locally; sync handoff is retrying")]
    SyncPending(#[source] AnyhowError),
    #[error("Password update failed")]
    Internal(#[from] AnyhowError),
}

/// Atomically updates the local credential and encrypted key history, then
/// queues the replayable migration. No DHT I/O occurs in this flow.
pub async fn change_password(
    ctx: &UserCtx,
    old_password: &str,
    new_password: &str,
) -> Result<(), PasswordChangeError> {
    // New-credential rule, not the structural one: a changed password is
    // minted here, so invisible/non-retypeable characters are rejected even
    // though a legacy password containing one still verifies elsewhere.
    if !validate_new_password(new_password) {
        return Err(PasswordChangeError::InvalidNewPassword);
    }

    // The journal preserves keys needed by in-flight publishers. Only the
    // background migration waits for those publishers, never this local commit.
    let _user_lock = store_lock::lock_store(ctx.username.as_bytes()).await;
    let mut user = store::load_matching_user(&ctx.username, &ctx.account_id)
        .await?
        .context("Password-change account was deleted or replaced")?;
    if user.crypto_version() != CURRENT_CRYPTO_VERSION {
        return Err(PasswordChangeError::Internal(anyhow!(
            "Password changes require a current-crypto account"
        )));
    }
    let (login, _) = super::auth::login_with_password(user.clone(), old_password)
        .await
        .map_err(|_| PasswordChangeError::IncorrectPassword)?;
    let keypair = login
        .runtime
        .keypair
        .context("password login produced no keypair")?;
    let old_secret = login
        .runtime
        .dht_secret
        .context("password login produced no DHT secret")?;
    // The new password alone rotates the secret: the hash is always
    // account-ID-salted, so no form tracking is needed.
    let new_hash =
        Zeroizing::new(encryption::hash_password_for_account(new_password, &ctx.account_id).await?);
    let new_secret = encryption::derive_dht_secret_from_hash(&new_hash, &ctx.account_id).await?;
    if old_secret == new_secret {
        return Ok(());
    }
    let rotation = prepare_rotation(&user, &old_secret, &new_secret).await?;
    user.encrypted_keypair = encrypt_keypair(&keypair, &new_secret).await?;

    let has_remember_me = store::load_password_hash_opt(&ctx.username)
        .await?
        .is_some_and(|hash| !Zeroizing::new(hash).is_empty());
    store::store_password_change(
        &user,
        has_remember_me.then_some(new_hash.as_bytes()),
        &rotation,
    )
    .await?;

    if UserCtx::try_current().is_some_and(|active| active.account_id == ctx.account_id) {
        set_session_crypto(&ctx.username, keypair, new_secret.clone());
        mutate_authenticated_user(|active| {
            if active.account_id() == ctx.account_id.as_ref() {
                active.encrypted_keypair = user.encrypted_keypair.clone();
                active.runtime.dht_secret = Some(new_secret);
            }
        });
    }

    if let Err(error) = enqueue_pending_rotation(&user).await {
        retry_rotation_enqueue(&user);
        return Err(PasswordChangeError::SyncPending(error));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinStatus {
    Ready { attempts: u32 },
    Cooldown { attempts: u32, until: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinVerifyResult {
    Verified,
    Incorrect { attempts: u32 },
    Cooldown { attempts: u32, until: u64 },
}

pub fn exponential_cooldown_secs(attempts: u32) -> u64 {
    let threshold = config::pin_gate::LOCKOUT_THRESHOLD;
    if attempts < threshold {
        return 0;
    }
    let tier = (attempts / threshold).saturating_sub(1);
    let raw = config::pin_gate::COOLDOWN_SECS.saturating_mul(1 << tier.min(10));
    raw.min(config::pin_gate::MAX_COOLDOWN_SECS)
}

const fn pin_status_from_state(state: store::PinAttemptState, now: u64) -> PinStatus {
    if state.locked_until > now {
        PinStatus::Cooldown {
            attempts: state.count,
            until: state.locked_until,
        }
    } else {
        PinStatus::Ready {
            attempts: state.count,
        }
    }
}

pub async fn pin_status(username: &str) -> Result<PinStatus> {
    Ok(pin_status_from_state(
        store::load_pin_attempt_state(username).await?,
        now_epoch_secs(),
    ))
}

/// Hashes a PIN using Argon2 with a random salt.
///
/// Used during onboarding and PIN changes to store the PIN as an
/// Argon2 PHC string in the user profile.
pub(super) async fn hash_pin(pin: &str) -> Result<String> {
    crypto_system!(crypto);
    let nonce_bytes = crypto.random_nonce().bytes();
    let pin = Zeroizing::new(pin.as_bytes().to_vec());
    async_utils::spawn_blocking(move || {
        let cs = encryption::crypto_system_sync()?;
        Ok(cs.hash_password(pin.as_slice(), &nonce_bytes)?)
    })
    .await?
}

/// Verifies a plaintext PIN against a stored Argon2 PHC hash.
async fn verify_pin_hash(pin: &str, pin_hash: &str) -> Result<bool> {
    if pin_hash.is_empty() {
        return Ok(false);
    }
    // Warm the crypto cache before entering spawn_blocking — ensures the VeilidAPI
    // handle is cached so the blocking thread can call crypto_system_sync().
    drop(encryption::get_cached_crypto_system().await?);
    let pin = Zeroizing::new(pin.as_bytes().to_vec());
    let hash = pin_hash.to_string();
    async_utils::spawn_blocking(move || {
        let cs = encryption::crypto_system_sync()?;
        Ok(cs.verify_password(pin.as_slice(), &hash)?)
    })
    .await?
}

/// Verifies a PIN against the locally stored user hash, with durable
/// brute-force tracking and persisted cooldown metadata.
pub async fn verify_pin(username: &str, pin: &str) -> Result<PinVerifyResult> {
    verify_pin_for_identity(username, pin, None).await
}

/// Settings verification bound to the captured durable account, not a
/// same-name replacement or an unrelated pending admission.
pub async fn verify_pin_for_account(ctx: &UserCtx, pin: &str) -> Result<PinVerifyResult> {
    verify_pin_for_identity(&ctx.username, pin, Some(&ctx.account_id)).await
}

async fn verify_pin_for_identity(
    username: &str,
    pin: &str,
    account_id: Option<&str>,
) -> Result<PinVerifyResult> {
    // Lock the entire read→verify→increment cycle to prevent concurrent
    // attempts from producing non-monotonic or duplicate counts.
    let _guard = store_lock::lock_store(username.as_bytes()).await;

    let pin_hash = match account_id {
        Some(account_id) => store::load_matching_user(username, account_id)
            .await?
            .context("PIN-verification account was deleted or replaced")?
            .pin_hash()
            .to_owned(),
        None => load_pin_hash(username).await?,
    };
    let outcome = store::gated_attempt(&store::pin_key(username), async || {
        verify_pin_hash(pin, &pin_hash).await
    })
    .await?;

    Ok(match outcome {
        store::AttemptOutcome::Verified => PinVerifyResult::Verified,
        store::AttemptOutcome::Rejected { attempts } => PinVerifyResult::Incorrect { attempts },
        store::AttemptOutcome::Locked { attempts, until } => {
            PinVerifyResult::Cooldown { attempts, until }
        }
    })
}

/// Resolves the PIN hash for `username`: the in-memory session first (the
/// pending user too — on a freshly paired device the PIN gate runs before
/// admission and the user is not persisted locally until
/// `apply_pending_device_login` completes), then the local store.
async fn load_pin_hash(username: &str) -> Result<String> {
    let guard = SESSION.read();
    let hash = guard
        .authenticated_user()
        .or_else(|| guard.pending_user())
        .filter(|user| user.username() == username)
        .map(|user| user.pin_hash().to_string());
    drop(guard);
    match hash {
        Some(hash) => Ok(hash),
        None => Ok(require_user(username).await?.pin_hash().to_string()),
    }
}
