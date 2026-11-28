//! Child-record sync: pulling a child record's subkey from the DHT and
//! merging it into the local dual-format row.

use anyhow::Result;
use veilid_core::{KeyPair, RecordKey, SharedSecret};

use crate::{
    config::e2e,
    session::epoch::SessionGuard,
    user::{ctx::UserCtx, record::resolve_child_key_for_id},
    veilid::{store, store::StoreMode},
};

use super::{
    CrdtModel,
    dual_row::{PullOutcome, merge_remote},
};

/// A pull that left local state untouched: the session ended, or the account
/// was deleted or replaced while the network read was in flight.
const PULL_UNCHANGED: PullOutcome = PullOutcome {
    changed: false,
    remote_stale: false,
};

/// Pull remote doc from DHT, merge with local automerge doc, save both formats.
pub async fn pull_and_merge<T: CrdtModel>(
    ctx: &UserCtx,
    guard: SessionGuard,
    user_key: &RecordKey,
    secret: &SharedSecret,
    keypair: &KeyPair,
) -> Result<PullOutcome> {
    if !guard.is_current() {
        return Ok(PULL_UNCHANGED);
    }
    let (child_key, _child_writer) =
        resolve_child_key_for_id(user_key.clone(), secret, keypair, T::ID, T::CHILD_SUBKEYS)
            .await?;
    // Forced network read: this is the resync pull path — a cached read
    // would return this device's own persisted copy and merge to a no-op,
    // silently dropping every remote change.
    let Some(remote_bytes) = store::load_opt(
        &child_key,
        T::SUBKEY,
        secret,
        true,
        StoreMode::StructuredEncrypted,
    )
    .await?
    else {
        // Record exists but was never written (fresh child record, or one
        // whose initial publish failed). Nothing to merge; mark the remote
        // stale so the caller re-publishes local state to initialize it.
        return Ok(PullOutcome {
            changed: false,
            remote_stale: guard.is_current(),
        });
    };
    let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
        return Ok(PULL_UNCHANGED);
    };
    if !guard.is_current() {
        return Ok(PULL_UNCHANGED);
    }
    let outcome = merge_remote::<T>(ctx.account_id.as_bytes(), &remote_bytes).await?;
    if outcome.changed {
        // Harness seam: a watched record's network copy was reconciled into
        // local state. `T::ID` is the app's own record/subkey identity
        // ("totp", "contacts", "conversations", "app_state"), not a raw DHT
        // key, so the marker stays stable across devices.
        e2e::event!("watch_reconciled", record = T::ID);
    }
    Ok(outcome)
}
