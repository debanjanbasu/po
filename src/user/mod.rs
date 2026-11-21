//! User domain — authentication, identity, credentials, and DHT sync.
//!
//! Consolidates user-related code into a single domain module following
//! the `chat/` pattern.
//!
//! # Dual-Format Architecture
//!
//! User data exists in two parallel representations:
//!
//! | Layer | Format | Purpose |
//! |-------|--------|---------|
//! | **Local storage** | Cap'n Proto (`User` struct) | Fast local reads, keyed by username |
//! | **DHT sync** | Automerge (`SyncProfile`, `SyncSecurity`) | CRDT merge across devices |
//!
//! This is architecturally intentional: autosurgeon requires `String` fields
//! for `Reconcile`/`Hydrate`, while local storage uses `Arc<str>` for cheap
//! cloning. The `From<&User>` impls on `SyncProfile`/`SyncSecurity` bridge
//! the two representations.
//!
//! # Relationship to `CrdtModel`
//!
//! Unlike `TotpTokenList`, `ContactList`, and `ConversationIndex`, the `User`
//! model does **not** implement `CrdtModel`. This is because:
//! - User records are keyed by username (not account ID)
//! - The keypair blob requires AEAD encryption (not generic compression)
//! - Multi-device pairing has unique hydration flows
//!
//! Instead, [`mutate_user`] provides equivalent concurrency safety (load →
//! lock → mutate → save) for the bespoke User storage path.
//!
//! Passwords are hashed via Veilid's crypto API with an app-level pepper. The
//! hash is then used to derive a shared secret that encrypts/decrypts the
//! user's Veilid `KeyPair` using AEAD.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use veilid_core::CryptoSystem;

use crate::{
    chat::{
        record::arm_and_catch_up,
        signal::{publish_local_route, start_route_heartbeat},
    },
    helpers::{encryption::account_id_string, store_lock::lock_store},
    nonfatal,
    session::{
        epoch::{SessionGuard, spawn_session_detached},
        phase::mutate_authenticated_user,
    },
};

pub mod auth;
pub mod biometric;
pub mod child_watch;
pub mod ctx;
pub mod identity_context;
mod identity_doc;
pub mod identity_layout;
pub mod identity_watch;
mod inspect_poll;
pub mod model;
mod pairing;
pub mod pairing_beacon;
mod pairing_crypto;
pub mod password;
mod password_rotation;
pub mod record;
mod session_crypto;
pub mod store;
pub mod sync_models;
pub mod sync_ops;
pub mod validation;
pub mod watcher;

// Re-export model types so `crate::user::User` etc. still resolve.
pub use model::{CURRENT_CRYPTO_VERSION, User};
use model::{EncryptedKeypair, NONCE_LEN};
use store::store_user;

// Re-export public API
pub use auth::{admit_user, hydrate_remembered_user, signup, update_onboarding};
// Cross-module: the peer-route paths (`chat::signal::routes`) report a
// push-path failure to the inspect poll, which is otherwise private here.
pub use inspect_poll::suspect_push_path;
pub use password::verify_pin;
pub use session_crypto::{
    clear_session_crypto_if_matches, get_session_crypto, get_verified_session_crypto,
    set_session_crypto,
};
pub use store::load_guest_theme_opt;
pub use sync_ops::dht_secret_for_username;

static NEXT_ADMISSION_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_admission_id() -> u64 {
    NEXT_ADMISSION_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn clear_pending_admission_effects() {
    auth::clear_pending_local_auth();
    pairing::clear_pending_device_login();
}

/// Applies staged admission effects (remember-me, pairing) for the user being admitted.
///
/// Called from `complete_admission` BEFORE setting the `SESSION` signal to avoid
/// component-scoped task cancellation: setting that signal triggers router navigation
/// which unmounts the login component and drops its spawned tasks.
///
/// Uses epoch-only guard (SESSION is not yet set at call time).
///
/// Returns `true` when the admission consumed a staged pairing (device-login)
/// commit — pairing hydration then covers the initial identity pull, so the
/// caller can skip the redundant post-admission resync.
pub async fn apply_pending_admission_effects(user: &User, guard: &SessionGuard) -> Result<bool> {
    if !guard.is_current() {
        return Ok(false);
    }
    auth::apply_pending_local_auth(user, guard).await;
    if !guard.is_current() {
        return Ok(false);
    }
    pairing::apply_pending_device_login(user, guard).await
}

/// Starts the session-scoped services for a fully admitted user.
///
/// Must be called after admission effects are applied and the `SESSION`
/// phase is `Authenticated` (services validate session state themselves):
///
/// - adaptive inspect polling — tier-3 sync backstop catching changes the
///   push paths (sibling nudges, DHT watches) miss on strict networks,
/// - route publication and heartbeat, rooted with the other session services,
/// - startup-resync latch + one identity watch/resync: watches only deliver
///   changes made while watching, so anything that happened while this
///   device was offline must be fetched explicitly; the resync also arms
///   the child record watches,
/// - session-owned conversation watches: arm every indexed conversation and
///   sweep for writes missed while logged out / offline.
///
/// The root-owned bootstrap survives caller unmount, but not an epoch change.
/// With `paired` (device-login admission), the Full resync is skipped:
/// `new_device_login` just pulled the identity record and
/// `post_pairing_hydration` is pulling child records, conversations, and
/// dict state concurrently. Only the child watches are armed so pushes land;
/// the initial-resync latch runs the verifying Full resync once the
/// hydration burst is over.
pub fn start_session_services(paired: bool) {
    // Mark the startup resync pending BEFORE spawning it: at cold boot the
    // network is often not usable yet, so these one-shot attempts fail and
    // the first-connectivity edge (updates::workers) retries until one
    // Full resync succeeds.
    watcher::reset_initial_resync();
    spawn_session_detached(async move {
        // Nested service tasks inherit the stable root scope, not the
        // authentication component that admission is about to unmount.
        publish_local_route();
        start_route_heartbeat();
        inspect_poll::start();
        nonfatal!(
            "user.identity_watch",
            "Failed to start identity watch after admission",
            identity_watch::start().await
        );
        if paired {
            child_watch::sync().await;
        } else {
            watcher::resync_identity().await;
        }
        arm_and_catch_up(true).await;
    });
}

/// Loads a user from local storage by username.
///
/// Load a user or fail with a "user not found" error.
pub async fn require_user(username: &str) -> Result<User> {
    load_user(username)
        .await?
        .context(format!("User not found: {username}"))
}

pub async fn load_user(username: &str) -> Result<Option<User>> {
    let Some(bytes) = store::users_table()
        .await?
        .load_opt(store::COL_USER, username.as_bytes())
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(User::from_packed_capnp(&bytes)?))
}

/// Generates a new account ID: 16 random bytes from Veilid crypto, hex-encoded (32 chars).
pub fn generate_account_id(crypto: &(impl CryptoSystem + ?Sized)) -> String {
    let nonce = crypto.random_nonce();
    let bytes = nonce.bytes();
    let mut account_id = [0; 16];
    account_id.copy_from_slice(&bytes[..16]);
    account_id_string(&account_id)
}

/// Load, lock, mutate, and save a user by username.
///
/// Mirrors [`sync::mutate::mutate`] but for the bespoke `User` storage
/// (Cap'n Proto in `oo_users`, keyed by username). Provides the same
/// concurrency safety via [`lock_store`].
///
/// The closure receives `&mut User` and returns an arbitrary value.
/// If it succeeds, the user is re-persisted to local storage.
pub async fn mutate_user<R>(username: &str, f: impl FnOnce(&mut User) -> Result<R>) -> Result<R> {
    let _guard = lock_store(username.as_bytes()).await;
    let mut user = require_user(username).await?;
    ensure!(!user.account_id().is_empty(), "Account ID is unavailable");
    ensure!(
        !store::is_cleanup_pending(user.account_id()).await?,
        "Account is being deleted or replaced"
    );
    let result = f(&mut user)?;
    store_user(&user).await?;
    mutate_authenticated_user(|active| {
        if active.account_id() == user.account_id() {
            active.identity = user.identity.clone();
            active.profile = user.profile.clone();
            active.security = user.security.clone();
            active.encrypted_keypair = user.encrypted_keypair.clone();
        }
    });
    // Record the mutation in the identity CRDT docs at mutation time so
    // cross-device merges resolve with faithful causal ordering.
    identity_doc::sync_docs_from_user(&user).await;
    Ok(result)
}
