//! Onboarding and profile updates: DOB/PIN/terms/biometrics persistence.

use crate::{
    compression::samples::collect_fields,
    helpers::store_lock::lock_store,
    models::pending_operation::OpType,
    nonfatal,
    session::phase::SESSION,
    user::{
        User,
        ctx::UserCtx,
        identity_doc, mutate_user,
        password::hash_pin,
        require_user, session_crypto, store, store_user,
        validation::{validate_dob, validate_pin},
    },
};
use anyhow::{Error as AnyhowError, Result, anyhow, bail, ensure};
use dioxus::prelude::ReadableExt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SecurityUpdateError {
    #[error("Security update failed")]
    Local(#[from] AnyhowError),
    #[error("Security updated locally; synchronization could not be queued")]
    SyncPending(#[source] AnyhowError),
}

/// Persists onboarding fields (DOB, PIN, terms, biometrics) for an existing user.
/// Also creates the DHT identity record if not yet set.
///
/// This flow is more complex than a simple `mutate_user` call because it
/// conditionally creates the DHT identity and may re-login the user.
pub async fn update_onboarding(
    username: &str,
    dob_yyyymmdd: i32,
    pin: Option<&str>,
    terms_accepted: bool,
    biometrics_enabled: bool,
) -> Result<User> {
    if let Some(p) = pin
        && !validate_pin(p)
    {
        bail!("Invalid PIN");
    }
    if let Some(msg) = validate_dob(dob_yyyymmdd) {
        bail!("{msg}");
    }
    let guard = lock_store(username.as_bytes()).await;
    let mut user = require_user(username).await?;
    session_crypto::fill_runtime_crypto_from_session(&mut user);
    if (user.runtime.keypair.is_none() || user.runtime.dht_secret.is_none())
        && let Some(session_user) = SESSION
            .peek()
            .user()
            .filter(|u| u.account_id() == user.account_id())
    {
        if user.runtime.keypair.is_none() {
            user.runtime.keypair = session_user.runtime.keypair.clone();
        }
        if user.runtime.dht_secret.is_none() {
            user.runtime.dht_secret = session_user.runtime.dht_secret.clone();
        }
    }
    if let Some(p) = pin {
        user.security.pin_hash = hash_pin(p).await?;
    } else if user.security.pin_hash.is_empty() {
        bail!("Invalid PIN");
    }
    user.profile.dob = dob_yyyymmdd;
    user.profile.terms_accepted = terms_accepted;
    user.security.biometrics_enabled = biometrics_enabled;

    // Queue identity creation + profile sync for background processing.
    // Persist setup-completing fields only after the operation is durable, so
    // an enqueue failure leaves needs_onboarding() true across restart.
    if user.dht_key().is_empty() {
        UserCtx::from_user(&user)
            .enqueue_touch(OpType::CreateIdentity)
            .await?;
    } else {
        // Onboarding sets both profile fields (dob/terms) and the PIN hash;
        // sync both subkeys or a paired device downloads an empty pin_hash
        // and shows onboarding again.
        UserCtx::from_user(&user)
            .enqueue_touch(OpType::ProfileAndSecurity)
            .await?;
    }

    store_user(&user).await?;
    // Record onboarding in the identity CRDT docs at mutation time so a
    // pull of the (still stale) network copy can never regress it.
    identity_doc::sync_docs_from_user(&user).await;
    drop(guard);
    collect_fields(user.account_id(), &["username", "dob", "terms_accepted"]);

    if session_crypto::fill_runtime_crypto_from_session(&mut user) {
        return Ok(user);
    }
    let remembered_hash = match store::load_password_hash_opt(username).await? {
        Some(h) if !h.is_empty() => h,
        _ => {
            session_crypto::fill_runtime_crypto_from_session(&mut user);
            return Ok(user);
        }
    };

    if let Some(user) =
        super::login_signup::try_login_with_user_optional(user, username, &remembered_hash).await?
    {
        return Ok(user);
    }

    restore_after_remember_failure(username).await
}

/// Recovers the login after a stale remembered-password hash was cleared:
/// restores runtime crypto from live session crypto, or fails the flow when
/// no live session exists.
async fn restore_after_remember_failure(username: &str) -> Result<User> {
    nonfatal!(
        "user.onboarding",
        "Failed to clear invalid remembered password hash",
        store::delete_password_hash(username).await
    );
    let mut user = require_user(username).await?;
    if session_crypto::fill_runtime_crypto_from_session(&mut user) {
        return Ok(user);
    }
    bail!("invalid credentials")
}

/// Updates only the supplied security fields for the captured account.
/// Publishes committed local state before handing synchronization to the queue.
pub async fn update_security(
    ctx: &UserCtx,
    pin: Option<&str>,
    biometrics_enabled: Option<bool>,
) -> Result<User, SecurityUpdateError> {
    if let Some(p) = pin
        && !validate_pin(p)
    {
        return Err(anyhow!("Invalid PIN").into());
    }
    // Pre-hash the PIN outside the lock (CPU-bound Argon2).
    let pin_hash = match pin {
        Some(p) => Some(hash_pin(p).await?),
        None => None,
    };
    let user = mutate_user(&ctx.username, |user| {
        ensure!(
            user.account_id() == ctx.account_id.as_ref(),
            "Security-update account was replaced"
        );
        if let Some(hash) = pin_hash {
            user.security.pin_hash = hash;
        }
        if let Some(enabled) = biometrics_enabled {
            user.security.biometrics_enabled = enabled;
        }
        Ok(user.clone())
    })
    .await?;
    collect_fields(&ctx.account_id, &["pin_hash", "biometrics_enabled"]);
    UserCtx::from_user(&user)
        .enqueue_touch(OpType::Security)
        .await
        .map_err(SecurityUpdateError::SyncPending)?;
    Ok(user)
}

/// Updates the user's date of birth.
/// Syncs profile subkey to DHT in the background or queues if offline.
pub async fn update_dob(username: &str, dob_yyyymmdd: i32) -> Result<()> {
    if let Some(msg) = validate_dob(dob_yyyymmdd) {
        bail!("{msg}");
    }
    let user_ctx = mutate_user(username, |user| {
        user.profile.dob = dob_yyyymmdd;
        Ok(UserCtx::from_user(user))
    })
    .await?;
    collect_fields(&user_ctx.account_id, &["username", "dob", "terms_accepted"]);
    user_ctx.enqueue_touch(OpType::Profile).await?;
    Ok(())
}
