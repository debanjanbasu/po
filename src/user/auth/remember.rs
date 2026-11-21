//! Remembered sessions: staged remember-me effects and last-active tracking.

use crate::{
    helpers::store_lock::recover_poison,
    nonfatal,
    session::epoch::SessionGuard,
    storage::background_errors::report_nonfatal,
    user::{User, next_admission_id, store},
};
use anyhow::anyhow;
use std::sync::{Arc, LazyLock, Mutex};
use zeroize::Zeroizing;

pub(in crate::user) enum RememberedSessionEffect {
    Store(Zeroizing<Vec<u8>>),
    Delete,
    Skip,
}

struct PendingLocalAuth {
    username: Arc<str>,
    admission_id: u64,
    remember_effect: RememberedSessionEffect,
}

static PENDING_LOCAL_AUTH: LazyLock<Mutex<Option<PendingLocalAuth>>> =
    LazyLock::new(|| Mutex::new(None));

/// Builds the deferred remember-me update for a just-verified admission
/// attempt without mutating persistent state yet.
///
/// The wrapped password hash is bound to this install's persistent device key
/// (`oo_device`). Pairing never transfers that wrapped blob across devices.
/// If the app is reinstalled or local device storage is cleared, remembered
/// sessions on this device become invalid until the user signs in again with
/// their password.
pub(in crate::user) fn build_remembered_session_effect(
    password_hash: Option<Zeroizing<String>>,
    remember_me: bool,
) -> RememberedSessionEffect {
    if !remember_me {
        return RememberedSessionEffect::Delete;
    }
    password_hash.map_or_else(
        || {
            report_nonfatal(
                "user.auth",
                "Missing cached password hash for remember-me",
                anyhow!("credential attempt produced no password hash"),
            );
            RememberedSessionEffect::Skip
        },
        |hash| RememberedSessionEffect::Store(Zeroizing::new(hash.as_bytes().to_vec())),
    )
}

pub fn stage_local_auth(user: &mut User, remember_me: bool) {
    let admission_id = next_admission_id();
    let remember_effect =
        build_remembered_session_effect(user.runtime.password_hash.take(), remember_me);
    user.runtime.admission_id = Some(admission_id);
    *recover_poison(PENDING_LOCAL_AUTH.lock()) = Some(PendingLocalAuth {
        username: user.username_arc(),
        admission_id,
        remember_effect,
    });
}

pub(in crate::user) fn clear_pending_local_auth() {
    *recover_poison(PENDING_LOCAL_AUTH.lock()) = None;
}

pub(in crate::user) fn take_matching_staged<T>(
    slot: &Mutex<Option<T>>,
    user: &User,
    key: impl FnOnce(&T) -> (&str, u64),
) -> Option<T> {
    let mut guard = recover_poison(slot.lock());
    let matches = guard.as_ref().is_some_and(|pending| {
        let (username, admission_id) = key(pending);
        username == user.username() && Some(admission_id) == user.runtime.admission_id
    });
    let taken = if matches { guard.take() } else { None };
    drop(guard);
    taken
}

pub(in crate::user) async fn apply_pending_local_auth(user: &User, guard: &SessionGuard) {
    if !guard.is_current() {
        return;
    }
    if let Some(PendingLocalAuth {
        remember_effect, ..
    }) = take_matching_staged(&PENDING_LOCAL_AUTH, user, |p| (&p.username, p.admission_id))
    {
        apply_remembered_session_effect(user, remember_effect, guard).await;
    }
}

pub(in crate::user) async fn apply_remembered_session_effect(
    user: &User,
    remember_effect: RememberedSessionEffect,
    guard: &SessionGuard,
) {
    if !guard.is_current() {
        return;
    }
    let username = user.username();
    match remember_effect {
        RememberedSessionEffect::Store(password_hash) => {
            nonfatal!(
                "user.auth",
                "Failed to store password hash for remember-me",
                store::store_password_hash(username, password_hash.as_slice()).await
            );
        }
        RememberedSessionEffect::Delete => {
            nonfatal!(
                "user.auth",
                "Failed to delete password hash",
                store::delete_password_hash(username).await
            );
        }
        RememberedSessionEffect::Skip => {}
    }
    if !guard.is_current() {
        return;
    }
    touch_last_active(username).await;
}

pub(super) async fn touch_last_active(username: &str) {
    nonfatal!(
        "user.auth",
        "Failed to store last active",
        store::store_last_active(username).await
    );
}
