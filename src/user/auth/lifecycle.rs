//! Session lifecycle: admission routing, logout, and account deletion.

use crate::{
    chat::{manager::cancel_all_watches, signal as peer_signal},
    compression::samples::clear_collector,
    session::{
        epoch::{self as session_epoch},
        phase::{self as session_phase, SESSION},
    },
    sync::queue::mark_account_deleted,
    user::{User, biometric, child_watch, identity_watch, session_crypto, store},
    veilid::core::parse_record_key,
};
use anyhow::Result;
use dioxus::prelude::ReadableExt;
use std::sync::Arc;

/// Determines the second-factor path for a user.
enum SecondFactor {
    /// No second-factor needed (no PIN/biometrics or still onboarding).
    None,
    /// Biometrics enabled and hardware available — attempt Face ID / Touch ID.
    Biometric,
    /// Fall through to PIN gate.
    Pin,
}

async fn classify_second_factor(user: &User) -> SecondFactor {
    let has_pin = user.has_pin();
    let has_bio = user.biometrics_enabled();

    if (!has_pin && !has_bio) || user.needs_onboarding() {
        return SecondFactor::None;
    }
    if has_bio && biometric::can_check_async().await {
        return SecondFactor::Biometric;
    }
    SecondFactor::Pin
}

/// Routes a credential-verified user through second-factor (biometric or PIN gate).
/// Also triggers processing of any pending sync operations.
pub async fn admit_user(user: User) -> Result<()> {
    let factor = classify_second_factor(&user).await;
    match factor {
        SecondFactor::None => session_phase::complete_admission(user).await?,
        SecondFactor::Pin => session_phase::enter_second_factor(user),
        SecondFactor::Biometric => session_phase::enter_biometric(user),
    }
    Ok(())
}

/// Extracts both username and account ID from whichever auth phase is populated.
fn active_identity() -> (Option<Arc<str>>, Option<Arc<str>>) {
    SESSION.peek().user().map_or((None, None), |u| {
        (Some(u.username_arc()), Some(u.account_id_arc()))
    })
}

/// Holds both watch lifecycle locks across epoch-advance → stop → route release
/// so neither identity nor child watches can be re-armed during teardown.
async fn teardown_active_session(
    dht_key: Option<&str>,
    deleted_account_id: Option<&str>,
) -> (child_watch::TeardownGuard, identity_watch::TeardownGuard) {
    let child_teardown = child_watch::begin_teardown().await;
    let teardown = identity_watch::begin_teardown().await;
    session_epoch::advance_epoch();
    if let Some(aid) = deleted_account_id {
        mark_account_deleted(aid);
    }
    cancel_all_watches().await;
    child_watch::stop_all(&child_teardown).await;
    if let Some(key_str) = dht_key
        && let Ok(key) = parse_record_key(key_str)
    {
        identity_watch::stop(&teardown, &key).await;
    }
    peer_signal::release_local_route().await;
    peer_signal::clear_route_cache().await;
    (child_teardown, teardown)
}

/// Logs out the current user: bumps epoch to cancel in-flight sync,
/// cancels DHT watches, releases routes, removes the remembered
/// session, then clears all global signals.
pub async fn log_out() -> Result<()> {
    let (username, _) = active_identity();
    let dht_key = SESSION.peek().user().map(User::dht_key_arc);
    if let Some(name) = &username {
        // Clearing this durable restore marker is the commit point for logout.
        store::clear_last_active().await?;
        clear_collector();
        session_crypto::clear_session_crypto(name);
    }
    let (_child_teardown, _teardown) = teardown_active_session(dht_key.as_deref(), None).await;
    session_phase::clear_session();
    Ok(())
}

/// Permanently deletes the current user's account and all local data.
/// Bumps epoch to cancel in-flight sync, installs a deleted-account queue
/// tombstone so already-loaded ops cannot dispatch after local wipe, closes
/// the identity DHT record, cancels DHT watches, deletes local data, then
/// clears global signals.
pub async fn delete_current_account() -> Result<()> {
    let (username, account_id) = active_identity();
    let dht_key = SESSION.peek().user().map(User::dht_key_arc);
    let (_child_teardown, _teardown) =
        teardown_active_session(dht_key.as_deref(), account_id.as_deref()).await;
    let cleanup = if let (Some(name), Some(aid)) = (&username, &account_id) {
        clear_collector();
        session_crypto::clear_session_crypto(name);
        store::delete_all_user_data(name, aid).await
    } else {
        Ok(())
    };
    if cleanup.is_ok() {
        session_phase::clear_session();
    }
    cleanup
}
