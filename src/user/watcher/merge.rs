//! Account-bound identity merges; session guards protect only the live UI.

use anyhow::Result;
use tokio::try_join;

use crate::{
    config::e2e,
    helpers::store_lock::lock_store,
    models::{app_state::AppState, pending_operation::OpType},
    session::{epoch::SessionGuard, phase::mutate_authenticated_user},
    storage::{
        background_errors::report_nonfatal,
        store_trait::{self, load_local, merge_remote_doc, reconcile_local_doc},
    },
    ui::state::hydrate_nav_theme_route as hydrate_app_state_signals,
    user::{
        User,
        ctx::UserCtx,
        model::onboarding_complete,
        record::IdentityPull,
        store::{load_matching_user, store_user},
        sync_models::{SyncProfile, SyncSecurity},
    },
};

/// Harness seam: names the identity subkey surfaces the network resolved into
/// the local record. The surface ids are the app's own subkey identities
/// (`profile`, `security`), so the marker is stable across devices.
fn report_reconciled_surfaces(
    local_profile: &SyncProfile,
    profile: &SyncProfile,
    local_security: &SyncSecurity,
    security: &SyncSecurity,
) {
    if profile != local_profile {
        e2e::event!("watch_reconciled", record = "profile");
    }
    if security != local_security {
        e2e::event!("watch_reconciled", record = "security");
    }
}

/// Harness seam: names the profile fields the network resolved into the
/// local record. Only fields that actually changed and were applied are
/// reported — `username` is identity, not synced state, so it never merges
/// into the local record and is never named here.
fn report_merged_profile_fields(local: &SyncProfile, merged: &SyncProfile) {
    if merged.dob != local.dob {
        e2e::event!("profile_merged", field = "dob");
    }
    if merged.terms_accepted != local.terms_accepted {
        e2e::event!("profile_merged", field = "terms_accepted");
    }
}

pub(super) async fn merge_app_state(
    ctx: &UserCtx,
    pull: &IdentityPull,
    guard: &SessionGuard,
) -> bool {
    match merge_account_app_state(ctx, pull).await {
        Ok(Some((state, changed))) => {
            if changed && let Some(state) = state {
                guard.commit(|| {
                    if UserCtx::try_current()
                        .is_some_and(|active| active.account_id == ctx.account_id)
                    {
                        hydrate_app_state_signals(&state);
                    }
                });
            }
            if changed {
                // Harness seam: the app-state subkey was reconciled into local
                // state from the network.
                e2e::event!("watch_reconciled", record = "app_state");
            }
            changed
        }
        Ok(None) => false,
        Err(e) => {
            report_nonfatal(
                "identity.watcher",
                "Failed to merge app state after identity pull",
                e,
            );
            false
        }
    }
}

async fn merge_account_app_state(
    ctx: &UserCtx,
    pull: &IdentityPull,
) -> Result<Option<(Option<AppState>, bool)>> {
    let _user_lock = lock_store(ctx.username.as_bytes()).await;
    if load_matching_user(&ctx.username, &ctx.account_id)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let outcome =
        store_trait::merge_remote::<AppState>(ctx.storage_key(), &pull.app_state_doc).await?;
    let state = if outcome.changed {
        Some(load_local::<AppState>(ctx.storage_key()).await?)
    } else {
        None
    };
    if outcome.remote_stale {
        crate::nonfatal!(
            "identity.watcher",
            "Failed to re-publish locally committed app state",
            ctx.enqueue_touch(OpType::AppState).await
        );
    }
    Ok(Some((state, outcome.changed)))
}

pub(super) async fn merge_and_notify(
    ctx: &UserCtx,
    pull: &IdentityPull,
    guard: &SessionGuard,
) -> bool {
    match merge_account_user(ctx, pull).await {
        Ok(Some((local, changed))) => {
            if changed {
                guard.commit(|| {
                    mutate_authenticated_user(|user| {
                        if user.account_id() == ctx.account_id.as_ref() {
                            SyncProfile::from(&local).apply_to(user);
                            SyncSecurity::from(&local).apply_to(user);
                        }
                    });
                });
            }
            changed
        }
        Ok(None) => false,
        Err(e) => {
            report_nonfatal(
                "identity.watcher",
                "Failed to merge identity data after pull",
                e,
            );
            false
        }
    }
}

async fn merge_account_user(ctx: &UserCtx, pull: &IdentityPull) -> Result<Option<(User, bool)>> {
    let _user_lock = lock_store(ctx.username.as_bytes()).await;
    let Some(mut local) = load_matching_user(&ctx.username, &ctx.account_id).await? else {
        return Ok(None);
    };
    let _account_lock = lock_store(ctx.storage_key()).await;
    let local_profile = SyncProfile::from(&local);
    let local_security = SyncSecurity::from(&local);
    let ((profile, profile_stale), (security, security_stale)) = try_join!(
        async {
            reconcile_local_doc(&ctx.account_id, &local_profile).await?;
            merge_remote_doc::<SyncProfile>(&ctx.account_id, &pull.profile_doc).await
        },
        async {
            reconcile_local_doc(&ctx.account_id, &local_security).await?;
            merge_remote_doc::<SyncSecurity>(&ctx.account_id, &pull.security_doc).await
        },
    )?;
    let changed = profile != local_profile || security != local_security;

    // Independently bootstrapped CRDT roots must not undo completed onboarding.
    let regresses_onboarding = changed
        && !local.needs_onboarding()
        && !onboarding_complete(
            profile.terms_accepted,
            profile.dob,
            !security.pin_hash.is_empty(),
        );
    if regresses_onboarding {
        try_join!(
            reconcile_local_doc(&ctx.account_id, &local_profile),
            reconcile_local_doc(&ctx.account_id, &local_security),
        )?;
    } else if changed {
        profile.apply_to(&mut local);
        security.apply_to(&mut local);
        store_user(&local).await?;
        report_merged_profile_fields(&local_profile, &profile);
        report_reconciled_surfaces(&local_profile, &profile, &local_security, &security);
    }
    if profile_stale || security_stale || regresses_onboarding {
        crate::nonfatal!(
            "identity.watcher",
            "Failed to re-publish locally committed identity data",
            ctx.enqueue_touch(OpType::ProfileAndSecurity).await
        );
    }
    Ok(Some((local, changed && !regresses_onboarding)))
}
