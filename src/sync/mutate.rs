//! Unified mutation API for CRDT models.
//!
//! Every data mutation goes through [`mutate`]. Handles locking, loading,
//! saving (as the Cap'n Proto snapshot), and sync queue enqueuing.

use anyhow::{Result, bail};

use crate::{
    helpers::store_lock::lock_store,
    storage::store_trait::{self as crdt, CrdtModel},
    user::ctx::UserCtx,
};

/// Result of a mutation closure.
pub struct MutationResult<R = ()> {
    /// Value to return to the caller.
    pub value: R,
    /// Whether the model was modified (controls save + enqueue).
    pub changed: bool,
}

impl<R> MutationResult<R> {
    /// Shorthand for a successful mutation that changed the model.
    pub const fn modified(value: R) -> Self {
        Self {
            value,
            changed: true,
        }
    }

    /// Shorthand for no change.
    pub const fn unchanged(value: R) -> Self {
        Self {
            value,
            changed: false,
        }
    }
}

/// Load, lock, mutate, save, and enqueue a [`CrdtModel`].
///
/// The closure receives `&mut T` and returns whether changes were made.
/// If changed, the model is reconciled into the local automerge doc and
/// a sync op is enqueued.
/// The username row lock precedes the account-key lock, so cleanup cannot
/// race a late mutation that would recreate deleted rows.
pub async fn mutate<T: CrdtModel, R>(
    ctx: &UserCtx,
    f: impl FnOnce(&mut T) -> Result<MutationResult<R>>,
) -> Result<R> {
    let Some(_user_lock) = ctx.lock_for_persisted_account().await? else {
        bail!("Mutation account was deleted or replaced");
    };
    let _guard = lock_store(ctx.storage_key()).await;
    let mut model = crdt::load_local::<T>(ctx.storage_key()).await?;
    let outcome = f(&mut model)?;

    if outcome.changed {
        // Save + enqueue the typed sync op in one step (the unified
        // `persist_and_sync` path every CrdtModel shares).
        model.persist_and_sync(ctx).await?;
    }

    Ok(outcome.value)
}
