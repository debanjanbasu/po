//! Credential flows: signup, password login, remembered-user hydration.

use crate::{
    helpers::{
        encryption::{
            PasswordHashForm, crypto_system, derive_dht_secret_from_hash, hash_password_for_account,
        },
        store_lock::lock_store,
    },
    nonfatal,
    user::{
        CURRENT_CRYPTO_VERSION, User, generate_account_id, load_user,
        password_rotation::recover_pending_rotations,
        session_crypto, store, store_user,
        validation::{validate_new_password, validate_username},
    },
};
use anyhow::{Error as AnyhowError, Result, anyhow, bail};
use thiserror::Error;
use tokio::join;
use zeroize::Zeroizing;

/// Outcome of a credential check against the local store.
#[derive(Debug)]
pub enum AuthOutcome {
    /// Credentials valid; keypair decrypted successfully.
    Authenticated(Box<User>),
    /// Password hash did not decrypt the stored keypair.
    Incorrect,
    /// No user record for the given username.
    NotFound,
}

/// Internal error type for login attempts.
#[derive(Debug, Error)]
pub(in crate::user) enum LoginResult {
    #[error("user not found")]
    NotFound,
    #[error("incorrect credentials")]
    Incorrect,
    #[error(transparent)]
    Other(#[from] AnyhowError),
}

/// Creates a new user: generates a Veilid keypair, encrypts it with the
/// password-derived secret, and stores the result in the table store.
pub async fn signup(username: &str, password: &str) -> Result<User> {
    if !validate_username(username) {
        bail!("Invalid username");
    }
    if !validate_new_password(password) {
        bail!("Invalid password");
    }
    crypto_system!(crypto);
    let keypair = crypto.generate_keypair();
    // Mint the account ID before hashing: it is the Argon2 salt, so the
    // credential hash — and the DHT secret derived from it — is exactly what a
    // joining device reproduces from the account ID in the pairing payload.
    let account_id = generate_account_id(&*crypto);
    let password_hash = Zeroizing::new(hash_password_for_account(password, &account_id).await?);
    let dht_secret = derive_dht_secret_from_hash(&password_hash, &account_id).await?;
    let encrypted_keypair = super::keypair_crypto::encrypt_keypair(&keypair, &dht_secret).await?;
    let mut user = User::new(username, "", account_id, encrypted_keypair, keypair);
    user.runtime.dht_secret = Some(dht_secret);
    let _guard = lock_store(username.as_bytes()).await;
    if load_user(username).await?.is_some() {
        bail!("User already exists");
    }
    store_user(&user).await?;
    user.runtime.password_hash = Some(password_hash);
    Ok(user)
}

async fn do_login(mut user: User, password_hash: &[u8]) -> Result<User, LoginResult> {
    if user.crypto_version() != CURRENT_CRYPTO_VERSION {
        return Err(LoginResult::Other(anyhow!(
            "Unsupported local crypto version {}",
            user.crypto_version()
        )));
    }
    let (keypair, shared_secret) = super::keypair_crypto::decrypt_keypair(
        password_hash,
        user.account_id(),
        &user.encrypted_keypair.ciphertext,
        &user.encrypted_keypair.nonce,
    )
    .await
    .map_err(|_| LoginResult::Incorrect)?;

    crypto_system!(crypto);
    if !crypto
        .validate_keypair(&keypair.key(), &keypair.secret())
        .map_err(|e| LoginResult::Other(e.into()))?
    {
        return Err(LoginResult::Incorrect);
    }

    user.runtime.dht_secret = Some(shared_secret);
    user.runtime.keypair = Some(keypair);
    Ok(user)
}

/// Opens `user` with `password`, trying the account-ID salt first and the
/// pepper salt second.
///
/// An account created before the account-ID salt derives its DHT secret from the
/// pepper-salted hash; its keypair is the oracle for which form it used, and
/// rehashing it as the new form would re-derive that secret and strand
/// everything the account has published. Returns the credential hash that opened
/// it (callers reuse it for remember-me staging) with its salt form.
pub(in crate::user) async fn login_with_password(
    user: User,
    password: &str,
) -> Result<(User, Zeroizing<String>, PasswordHashForm), LoginResult> {
    for form in PasswordHashForm::NEWEST_FIRST {
        let hash = Zeroizing::new(
            form.hash(password, user.account_id())
                .await
                .map_err(LoginResult::Other)?,
        );
        match do_login(user.clone(), hash.as_bytes()).await {
            Err(LoginResult::Incorrect) => {}
            Ok(login) => return Ok((login, hash, form)),
            Err(err) => return Err(err),
        }
    }
    Err(LoginResult::Incorrect)
}

/// Checks whether the given credentials match a stored user.
pub async fn check_credentials(username: &str, password: &str) -> Result<AuthOutcome> {
    let result = async {
        let user = load_user(username).await?.ok_or(LoginResult::NotFound)?;
        let (mut user, password_hash, _) = login_with_password(user, password).await?;
        user.runtime.password_hash = Some(password_hash);
        Ok(user)
    }
    .await;

    match result {
        Ok(user) => Ok(AuthOutcome::Authenticated(Box::new(user))),
        Err(LoginResult::NotFound) => Ok(AuthOutcome::NotFound),
        Err(LoginResult::Incorrect) => Ok(AuthOutcome::Incorrect),
        Err(LoginResult::Other(err)) => Err(err),
    }
}

/// Verifies `password` against the local user without mutating remember-me or
/// session state, returning the salt form that opened the account.
///
/// Callers that must derive the account's DHT secret later (pairing payload
/// generation) need the form, not the hash: a joining device cannot reproduce an
/// account-ID-salted hash before it knows the account ID.
pub async fn verify_password(username: &str, password: &str) -> Result<Option<PasswordHashForm>> {
    let Some(user) = load_user(username).await? else {
        return Ok(None);
    };
    if user.crypto_version() != CURRENT_CRYPTO_VERSION {
        return Ok(None);
    }
    match login_with_password(user, password).await {
        Ok((_, _, form)) => Ok(Some(form)),
        Err(LoginResult::Incorrect | LoginResult::NotFound) => Ok(None),
        Err(LoginResult::Other(err)) => Err(err),
    }
}

/// Attempts to auto-login a remembered user by loading a stored password hash.
pub async fn hydrate_remembered_user() -> Result<Option<User>> {
    store::retry_incomplete_cleanups().await?;
    recover_pending_rotations().await?;
    let Some(username) = store::load_last_active().await? else {
        return Ok(None);
    };
    let (password_hash, user) = join!(
        store::load_password_hash_opt(&username),
        load_user(&username),
    );
    let user = user?;
    if user
        .as_ref()
        .is_some_and(|user| user.crypto_version() != CURRENT_CRYPTO_VERSION)
    {
        return Ok(None);
    }
    let Some(password_hash) = password_hash? else {
        return Ok(None);
    };

    let login = match user {
        Some(user) => do_login(user, &password_hash).await,
        None => Err(LoginResult::NotFound),
    };
    match login {
        Ok(user) => {
            super::remember::touch_last_active(&username).await;
            Ok(Some(user))
        }
        Err(LoginResult::Incorrect | LoginResult::NotFound) => {
            nonfatal!(
                "user.auth",
                "Failed to delete stale password hash",
                store::delete_password_hash(&username).await
            );
            nonfatal!(
                "user.auth",
                "Failed to clear last active",
                store::clear_last_active().await
            );
            Ok(None)
        }
        Err(LoginResult::Other(err)) => Err(err),
    }
}

pub(super) async fn try_login_with_user_optional(
    user: User,
    username: &str,
    password_hash: &[u8],
) -> Result<Option<User>> {
    match do_login(user, password_hash).await {
        Ok(user) => {
            if let (Some(kp), Some(secret)) = (&user.runtime.keypair, &user.runtime.dht_secret) {
                session_crypto::set_session_crypto(username, kp.clone(), secret.clone());
            }
            Ok(Some(user))
        }
        Err(LoginResult::Incorrect | LoginResult::NotFound) => Ok(None),
        Err(LoginResult::Other(err)) => Err(err),
    }
}
