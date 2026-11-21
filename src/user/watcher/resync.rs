//! Scoped identity resyncs: how much of the record to pull, the nudge-driven
//! scope selection, and the startup-resync latch completion.

use crate::{
    config,
    helpers::async_utils::sleep_ms,
    platform::lifecycle::note_sync_activity,
    session::{epoch as session, phase::mutate_authenticated_user},
    storage::{background_errors::push_background_error, sync_status::refresh_sync_status},
    user,
    veilid::{dict_sync, store},
};

use super::{
    burst::mark_initial_resync_done,
    child_pull::{pull_child_for_feature, pull_identity_children},
    merge::{merge_and_notify, merge_app_state},
    shared::{current_identity, dict_only_sync, prepare_pull},
};

/// How much of the identity to pull on a resync.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ResyncScope {
    /// Core subkeys + child records + dictionary check + child watch re-arm.
    /// Login catch-up, watch death, unknown change scope.
    Full,
    /// Core subkeys + child records + child watch re-arm — a pointers change
    /// means child keys may have appeared or migrated. No dictionary check.
    Children,
    /// Core subkeys only — profile/security/app-state watch hints.
    Core,
}

/// Full pull of the authenticated user's identity record (and child
/// records) from the DHT, merged into local state. No-op when no session
/// is active.
pub async fn resync_identity() {
    resync_with_scope(ResyncScope::Full).await;
}

/// Scoped identity resync — see [`ResyncScope`] for what each level pulls.
pub(super) async fn resync_with_scope(scope: ResyncScope) -> bool {
    let guard = session::SessionGuard::capture();

    let Some((user_key, ctx)) = current_identity() else {
        return false;
    };
    let Some((identity, _crypto_lease)) = prepare_pull(&ctx, guard).await else {
        return false;
    };

    let result = user::record::sync_from_dht(user_key.clone(), &identity.secret).await;

    // Discard if session changed during async pull
    if !guard.is_current() {
        return false;
    }

    match result {
        Ok(pull) => {
            // A successful Full pull proves the startup sync landed;
            // stop the connectivity-edge retry loop.
            if scope == ResyncScope::Full {
                mark_initial_resync_done();
            }
            // Cache remote pointers so child record resolution uses fresh data
            let pointers = pull.pointers.clone();
            mutate_authenticated_user(|user| {
                if user.account_id() == ctx.account_id.as_ref() {
                    user.set_sync_pointers(pointers);
                }
            });

            let user_changed = merge_and_notify(&ctx, &pull, &guard).await;
            let app_state_changed = merge_app_state(&ctx, &pull, &guard).await;

            let mut any_child_changed = false;
            if scope != ResyncScope::Core {
                any_child_changed = pull_identity_children(
                    &ctx,
                    &user_key,
                    &identity.secret,
                    &identity.keypair,
                    guard,
                )
                .await;

                if scope == ResyncScope::Full
                    && let Err(e) =
                        dict_sync::check_and_sync(user_key.clone(), &identity.secret).await
                    && store::report_nonfatal_unless_network(
                        "identity.watcher",
                        "Dict sync check failed",
                        &e,
                    )
                    && guard.is_current()
                {
                    push_background_error(
                        "identity.watcher",
                        "Dictionary sync check failed. Compression sync may be stale.",
                    );
                }

                // Pointers may have appeared or migrated during the pull —
                // (re)arm the child record watches so future changes on
                // those records arrive push-style.
                if guard.is_current() {
                    user::child_watch::sync().await;
                }
            }

            let changed = user_changed || app_state_changed || any_child_changed;
            if changed && guard.is_current() {
                refresh_sync_status().await;
            }
            note_sync_activity();
            changed
        }
        Err(e) => {
            // Transient network failures self-heal (initial-resync latch +
            // inspect poll retry); only surface errors retries won't fix.
            if store::report_nonfatal_unless_network(
                "identity.watcher",
                "Failed to pull identity from DHT",
                &e,
            ) {
                push_background_error(
                    "identity.watcher",
                    "Could not refresh identity from network. Data may be outdated.",
                );
            }
            false
        }
    }
}

/// Handles a same-account sync nudge from a sibling device (or the inspect
/// poll finding lagging subkeys): pulls the scopes named by the
/// [`crate::config::nudge`] bits. When the pull finds nothing new, one
/// delayed retry covers the window where the sender's DHT write has not
/// yet propagated to the nodes this device reads from.
pub async fn handle_sync_nudge(scope: u8) {
    note_sync_activity();
    if pull_nudged_scopes(scope).await {
        return;
    }
    let guard = session::SessionGuard::capture();
    sleep_ms(config::nudge::RETRY_DELAY_MS).await;
    if guard.is_current() {
        pull_nudged_scopes(scope).await;
    }
}

/// Pulls each scope named by `scope` bits; returns whether anything changed.
async fn pull_nudged_scopes(scope: u8) -> bool {
    if scope & config::nudge::SCOPE_FULL != 0 {
        return resync_with_scope(ResyncScope::Full).await;
    }
    let mut changed = false;
    if scope & config::nudge::SCOPE_CORE != 0 {
        changed |= resync_with_scope(ResyncScope::Core).await;
    }
    if scope & config::nudge::SCOPE_TOTP != 0 {
        changed |= pull_child_for_feature("totp").await;
    }
    if scope & config::nudge::SCOPE_CONTACTS != 0 {
        changed |= pull_child_for_feature("contacts").await;
    }
    if scope & config::nudge::SCOPE_CONVERSATIONS != 0 {
        changed |= pull_child_for_feature("conversations").await;
    }
    if scope & config::nudge::SCOPE_DICT != 0 {
        dict_only_sync().await;
        // Dict sync reports no change signal — treat as handled (no retry).
        changed = true;
    }
    changed
}
