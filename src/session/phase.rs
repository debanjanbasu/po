//! Session phase state machine — single source of truth for auth state.
//!
//! Every phase that carries a user stores it directly in [`SESSION`], making
//! the phase signal the sole source of truth for authentication state.

use anyhow::Result;
use dioxus::{logger::tracing::warn, prelude::*};

use crate::{
    chat::{conversation, invite::clear_pending_invites, session_flow, storage},
    components::overlays::deep_link_toast::clear_deep_link_toast,
    global_state::{last_active_route, nav_bar_items},
    session::epoch as session,
    storage::{
        hydration_status::clear_hydration_status, pairing_sync_status,
        upgrade_status::clear_upgrade_status,
    },
    sync::queue::clear_account_deleted,
    user::{
        User, apply_pending_admission_effects, clear_pending_admission_effects,
        clear_session_crypto_if_matches, pairing_beacon::clear_active_share, set_session_crypto,
        start_session_services,
    },
};

/// The phases a user session moves through.
#[derive(Clone, Default)]
pub enum SessionPhase {
    /// Startup is restoring a remembered session (Veilid local init +
    /// hydration). The router shows a neutral splash so the login form
    /// never flashes before restore resolves.
    #[default]
    Restoring,
    /// No authenticated user.
    Anonymous,
    /// Credentials verified; biometric dialog is shown (OS-level).
    PendingBiometric(User),
    /// Credentials verified; awaiting PIN / second-factor.
    PendingSecondFactor(User),
    /// Fully authenticated.
    Authenticated(User),
}

impl SessionPhase {
    /// The user, if any phase holds one.
    pub const fn user(&self) -> Option<&User> {
        match self {
            Self::Restoring | Self::Anonymous => None,
            Self::PendingBiometric(u) | Self::PendingSecondFactor(u) | Self::Authenticated(u) => {
                Some(u)
            }
        }
    }

    /// The fully-admitted user. `None` during pending phases.
    ///
    /// Use this wherever only a completed admission may act (crypto, DHT,
    /// sync, authenticated UI). Call sites that act on completed admission
    /// must not widen to pending phases.
    pub const fn authenticated_user(&self) -> Option<&User> {
        match self {
            Self::Authenticated(u) => Some(u),
            _ => None,
        }
    }

    pub const fn pending_user(&self) -> Option<&User> {
        match self {
            Self::PendingBiometric(u) | Self::PendingSecondFactor(u) => Some(u),
            _ => None,
        }
    }
}

/// Global session phase signal.
pub static SESSION: GlobalSignal<SessionPhase> = Signal::global(SessionPhase::default);

// ── Transitions ─────────────────────────────────────────────────────────

fn enter_pending(phase: SessionPhase) {
    session::advance_epoch();
    SESSION.with_mut(|s| *s = phase);
}

/// Move to `PendingBiometric`. Advances the session epoch.
/// The router renders a blank screen during this phase so only the OS
/// biometric dialog is visible.
pub fn enter_biometric(user: User) {
    enter_pending(SessionPhase::PendingBiometric(user));
}

/// Move to `PendingSecondFactor`. Advances the session epoch.
pub fn enter_second_factor(user: User) {
    enter_pending(SessionPhase::PendingSecondFactor(user));
}

/// Complete admission: install crypto, apply staged post-admission side
/// effects, then promote to `Authenticated` (which triggers router
/// navigation and may cancel the calling task).
///
/// Admission effects (remember-me storage, pairing finalization) are applied
/// BEFORE setting the `SESSION` signal because that signal write
/// trigger reactive router navigation that unmounts the login component —
/// dropping any component-scoped task mid-execution.
pub async fn complete_admission(user: User) -> Result<()> {
    session::advance_epoch();
    let guard = session::SessionGuard::capture();
    // Account deletion installs a queue tombstone keyed by account_id; clear it
    // only when the exact account is intentionally re-admitted.
    clear_account_deleted(user.account_id());

    let installed_crypto =
        if let (Some(keypair), Some(secret)) = (&user.runtime.keypair, &user.runtime.dht_secret) {
            set_session_crypto(user.username(), keypair.clone(), secret.clone());
            true
        } else {
            false
        };
    // Clears this admission's crypto install (only while it is still ours)
    // so a failed or superseded admission never leaves orphaned key material
    // in the session-crypto map.
    let clear_installed_crypto = || {
        if installed_crypto
            && let (Some(keypair), Some(secret)) = (&user.runtime.keypair, &user.runtime.dht_secret)
        {
            clear_session_crypto_if_matches(user.username(), keypair, secret);
        }
    };

    // Apply staged admission effects (remember-me, pairing) BEFORE setting
    // navigation-triggering signals. Uses epoch-only guard since SESSION is
    // not yet in Authenticated state.
    let paired = match apply_pending_admission_effects(&user, &guard).await {
        Ok(paired) => paired,
        Err(error) => {
            clear_installed_crypto();
            return Err(error);
        }
    };

    // Transition the phase only after admission effects are complete.
    // WARNING: This signal write triggers reactive navigation. Any `.await` after
    // this point may never execute if the calling task is component-scoped
    // and the originating component unmounts.
    if !guard.is_current() {
        warn!("Skipping admission completion because the session changed");
        clear_installed_crypto();
        return Ok(());
    }
    SESSION.with_mut(|s| *s = SessionPhase::Authenticated(user));

    // Root-owned services survive the admission component's unmount.
    start_session_services(paired);
    Ok(())
}

/// Mutate the user held by the authenticated session phase.
pub fn mutate_authenticated_user(f: impl FnOnce(&mut User)) {
    SESSION.with_mut(|s| {
        if let SessionPhase::Authenticated(u) = s {
            f(u);
        }
    });
}

fn reset_to_anonymous() {
    clear_pending_admission_effects();
    SESSION.with_mut(|s| *s = SessionPhase::Anonymous);
    clear_pending_invites();
    clear_deep_link_toast();
    clear_hydration_status();
    pairing_sync_status::clear_active();
    clear_active_share();
}

/// Clear the session back to `Anonymous`, wiping user-scoped UI signals.
///
/// Call **after** async cleanup so the scope that spawned the work
/// stays mounted until cleanup finishes. Callers (`log_out`,
/// `delete_current_account`) advance the session epoch **before** their
/// async cleanup so in-flight sync is invalidated early; this function
/// does not advance it again.
pub fn clear_session() {
    // Captured before `reset_to_anonymous` drops the phase. The cancellation
    // flags guard bootstrap work for the session that is ending; callers
    // advance the epoch before this, so nothing that could read them is still
    // runnable, and a flag is only ever consulted for a contact still present
    // in the `ContactList`. Dropping them here is what bounds the set to one
    // session rather than the process lifetime.
    let ending_account = SESSION.with(|phase| {
        phase
            .authenticated_user()
            .map(|user| user.account_id().to_owned())
    });
    if let Some(account_id) = ending_account {
        session_flow::purge_canceled_contact_work_for_account(&account_id);
    }
    storage::clear_cache();
    reset_to_anonymous();
    nav_bar_items().write().clear();
    last_active_route().write().clear();
    conversation::clear_all_meta_entries();
    clear_upgrade_status();
}

/// Resolve the startup `Restoring` phase to `Anonymous` once local init
/// determines no remembered session will be admitted (none stored, or the
/// restore admission failed). No-op when another actor already moved the
/// session on. Does not advance the epoch: nothing was admitted, so there is
/// nothing to invalidate.
pub fn resolve_restore_to_anonymous() {
    SESSION.with_mut(|s| {
        if matches!(s, SessionPhase::Restoring) {
            *s = SessionPhase::Anonymous;
        }
    });
}

/// Abandon second-factor (e.g. "use a different account") without
/// full logout cleanup.
pub fn abandon_second_factor() {
    session::advance_epoch();
    reset_to_anonymous();
}
