//! Session-derived lookups shared by the dispatch, resync, and pull paths.

use anyhow::{Result, anyhow};
use tokio::sync::OwnedRwLockReadGuard;
use veilid_core::RecordKey;

use crate::{
    session::epoch::SessionGuard,
    user::{
        ctx::UserCtx,
        identity_context::IdentityContext,
        password::{PasswordRotationPending, lock_identity_sync, prepare_identity_sync},
    },
    veilid::{core::parse_record_key, dict_sync, store},
};

pub(super) fn current_identity() -> Option<(RecordKey, UserCtx)> {
    let ctx = UserCtx::try_current()?;
    let user_key = parse_record_key(&ctx.dht_key).ok()?;
    Some((user_key, ctx))
}

pub(in crate::user) async fn prepare_pull(
    ctx: &UserCtx,
    guard: SessionGuard,
) -> Option<(IdentityContext, OwnedRwLockReadGuard<()>)> {
    let lease = lock_identity_sync(&ctx.account_id).await;
    if !guard.is_current() {
        return None;
    }
    let result: Result<IdentityContext> = try {
        let identity = IdentityContext::for_user(&ctx.username).await?;
        if identity.user.account_id() != ctx.account_id.as_ref()
            || identity.user.dht_key() != ctx.dht_key.as_ref()
        {
            Err(anyhow!("Identity-pull account was deleted or replaced"))?;
        }
        prepare_identity_sync(&identity).await?;
        identity
    };
    if !guard.is_current() {
        return None;
    }
    match result {
        Ok(identity) => Some((identity, lease)),
        Err(error) if error.is::<PasswordRotationPending>() => None,
        Err(error) => {
            store::report_nonfatal_unless_network(
                "identity.watcher",
                "Failed to prepare identity refresh",
                &error,
            );
            None
        }
    }
}

/// Dictionary-only refresh for changes confined to the dict subkeys.
pub(super) async fn dict_only_sync() {
    let guard = SessionGuard::capture();
    let Some((user_key, ctx)) = current_identity() else {
        return;
    };
    let Some((identity, _crypto_lease)) = prepare_pull(&ctx, guard).await else {
        return;
    };
    if let Err(e) = dict_sync::check_and_sync(user_key, &identity.secret).await
        && guard.is_current()
    {
        store::report_nonfatal_unless_network("identity.watcher", "Dict sync check failed", &e);
    }
}
