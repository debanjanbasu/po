//! Identity-related sync operation handlers, dispatched from `sync/handler.rs`.

use std::str;

use anyhow::{Context, Error, Result, ensure};
use dioxus::logger::tracing::warn;
use tokio::try_join;
use veilid_core::{RecordKey, SharedSecret, bytes::Bytes};

use crate::{
    app::startup::hydrate_app_state_signals,
    chat::{contact::ContactList, conversation::ConversationIndex},
    helpers::store_lock::lock_store,
    models::{
        app_state::AppState,
        pending_operation::{OpType, PendingOperation},
        totp::TotpTokenList,
    },
    session::phase::mutate_authenticated_user,
    storage::store_trait::{load_local, locked_live_doc, save_local},
    sync::pending_ops,
    user::{
        self,
        ctx::UserCtx,
        identity_context::IdentityContext,
        password_rotation::{enqueue_pending_rotation, run_rotation},
        store::load_matching_user,
        sync_models::{SyncProfile, SyncSecurity},
    },
    veilid::core::parse_record_key,
};

pub async fn reencrypt_subkeys(op: &PendingOperation, ctx: &IdentityContext) -> Result<()> {
    ensure!(
        op.identity_key.as_ref() == ctx.user.account_id()
            && op.username.as_ref() == ctx.user.username(),
        "Password-rotation operation belongs to a different account"
    );
    run_rotation(ctx, Some(&op.payload)).await
}

pub async fn sync_profile_and_security(ctx: &IdentityContext) -> Result<()> {
    try_join!(
        ctx.sync_subkey(SyncProfile::apply_to),
        ctx.sync_subkey(SyncSecurity::apply_to),
    )?;
    Ok(())
}

pub async fn sync_app_state(ctx: &IdentityContext) -> Result<()> {
    let account_id = ctx.user.account_id();
    // No account pre-check: publish_live_doc folds only while the account
    // still exists, and a deleted account needs no local doc seeded either.
    let live = {
        if let Some(live) = locked_live_doc::<AppState>(ctx.key_bytes()).await? {
            live
        } else {
            save_local::<AppState>(ctx.key_bytes(), &AppState::default()).await?;
            locked_live_doc::<AppState>(ctx.key_bytes())
                .await?
                .context("Failed to initialize AppState live doc")?
        }
    };
    // `?` twice: the outer error is a local load/persist failure, the inner one
    // the DHT write's own outcome. `None` is a publish that folded nothing back
    // in — a write failure, or an account deleted or replaced mid-flight.
    let Some(merged) = ctx
        .publish_live_doc::<AppState>(&ctx.key, Some(&ctx.keypair), Some(&live))
        .await??
    else {
        return Ok(());
    };
    if UserCtx::try_current().is_some_and(|active| active.account_id.as_ref() == account_id) {
        hydrate_app_state_signals(&merged);
    }
    Ok(())
}

/// Static sync-surface registry: maps every pending-op type backed by a
/// [`crate::storage::store_trait::SyncSurface`] to its unified publish path.
///
/// This is the single place a new synced model is registered — the drain
/// loop dispatches through it instead of hand-enumerating per-type arms.
/// Ops with no surface (composite or non-CRDT handlers like
/// `ProfileAndSecurity`, `ReencryptSubkeys`, `Keypair`) return `None`.
pub async fn publish_registered_op(op_type: OpType, ctx: &IdentityContext) -> Option<Result<()>> {
    match op_type {
        OpType::Profile => Some(ctx.sync_subkey(SyncProfile::apply_to).await),
        OpType::Security => Some(ctx.sync_subkey(SyncSecurity::apply_to).await),
        OpType::AppState => Some(sync_app_state(ctx).await),
        OpType::Totp => Some(ctx.sync_child_crdt::<TotpTokenList>().await),
        OpType::Chat => Some(ctx.sync_child_crdt::<ConversationIndex>().await),
        OpType::ChatContacts => Some(ctx.sync_child_crdt::<ContactList>().await),
        _ => None,
    }
}

pub async fn create_identity(op: &PendingOperation) -> Result<()> {
    let Some(mut user) = ({
        let _guard = lock_store(op.username.as_bytes()).await;
        load_matching_user(&op.username, &op.identity_key).await?
    }) else {
        return Ok(());
    };
    if !user.dht_key().is_empty() {
        return commit_created_identity(op, parse_record_key(user.dht_key())?).await;
    }
    let (keypair, secret) = IdentityContext::crypto_for_user(&user).await?;
    user.runtime.keypair = Some(keypair);
    let key_from_payload = if op.payload.is_empty() {
        None
    } else {
        Some(parse_record_key(
            str::from_utf8(&op.payload).context("Invalid CreateIdentity payload encoding")?,
        )?)
    };
    let dht_key = ensure_identity_record(op, &user, &secret, key_from_payload).await?;

    commit_created_identity(op, dht_key).await
}

/// Stage: reuse the payload's record key when present, otherwise create the
/// identity record on the DHT and persist its key back into the op payload
/// so a retry reuses the same record instead of forking a second identity.
async fn ensure_identity_record(
    op: &PendingOperation,
    user: &user::User,
    secret: &SharedSecret,
    key_from_payload: Option<RecordKey>,
) -> Result<RecordKey> {
    if let Some(key) = key_from_payload {
        return Ok(key);
    }
    let app_state = load_local::<AppState>(user.account_id().as_bytes()).await?;
    let key = super::record::create_identity_record(user, &app_state, secret).await?;
    let _guard = lock_store(op.username.as_bytes()).await;
    if load_matching_user(&op.username, &op.identity_key)
        .await?
        .is_none()
    {
        return Ok(key);
    }
    let mut updated_op = op.clone();
    updated_op.payload = Bytes::from(key.to_string());
    pending_ops::update_op(updated_op, Some(op))
        .await
        .context("Failed to persist CreateIdentity provisional key")?;
    Ok(key)
}

/// Fence the local commit against deletion and same-name replacement, and
/// mutate the latest row rather than the snapshot captured before DHT I/O.
async fn commit_created_identity(op: &PendingOperation, dht_key: RecordKey) -> Result<()> {
    {
        let _guard = lock_store(op.username.as_bytes()).await;
        let Some(mut user) = load_matching_user(&op.username, &op.identity_key).await? else {
            return Ok(());
        };
        let key = dht_key.to_string();
        ensure!(
            user.dht_key().is_empty() || user.dht_key() == key,
            "CreateIdentity result conflicts with the current identity"
        );
        user.set_dht_key(&key);
        user::store::store_user(&user).await?;
        enqueue_pending_rotation(&user).await?;
        mutate_authenticated_user(|active| {
            if active.account_id() == op.identity_key.as_ref() {
                active.set_dht_key(&key);
            }
        });
    }
    if UserCtx::try_current().is_some_and(|ctx| ctx.account_id == op.identity_key) {
        super::identity_watch::reestablish().await;
    }
    Ok(())
}

/// Derives a DHT secret only from credentials matching the stored account.
pub async fn dht_secret_for_username(username: &str) -> Option<SharedSecret> {
    if let Some(secret) = user::get_session_crypto(username).map(|c| c.dht_secret.clone()) {
        return Some(secret);
    }
    let result = async {
        let user = user::require_user(username).await?;
        let (_, secret) = IdentityContext::crypto_for_user(&user).await?;
        Ok::<_, Error>(secret)
    }
    .await;
    match result {
        Ok(secret) => Some(secret),
        Err(error) => {
            warn!("DHT credentials unavailable for {username}: {error:#}");
            None
        }
    }
}
