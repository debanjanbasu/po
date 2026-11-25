//! Durable password-key history and account-local migration coordination.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, Weak},
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use dioxus::core::spawn_forever;
use thiserror::Error;
use tokio::{
    sync::{OwnedRwLockReadGuard, RwLock},
    try_join,
};
use veilid_core::{BareSharedSecret, SharedSecret};
use zeroize::Zeroizing;

use crate::{
    helpers::{
        async_utils::sleep_ms,
        encryption::{crypto_system, decrypt_with_nonce_aad, encrypt_with_nonce_aad},
        store_lock::{lock_store, recover_poison},
    },
    models::pending_operation::{OpType, PendingOperation},
    storage::background_errors::report_nonfatal,
    sync::pending_ops,
    user::{
        CURRENT_CRYPTO_VERSION, User,
        ctx::UserCtx,
        identity_context::IdentityContext,
        record,
        store::{self, COL_PASSWORD_ROTATION, COL_USER, load_matching_user, users_table},
    },
};

const JOURNAL_MAGIC: &[u8] = b"POR1";
const JOURNAL_AAD: &[u8] = b"po:password-rotation:v1\0";
const SECRET_LEN: usize = 32;

type SyncLocks = HashMap<Arc<str>, Weak<RwLock<()>>>;
static IDENTITY_SYNC_LOCKS: LazyLock<Mutex<SyncLocks>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn identity_sync_lock(account_id: &str) -> Arc<RwLock<()>> {
    let mut locks = recover_poison(IDENTITY_SYNC_LOCKS.lock());
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(account_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(RwLock::new(()));
    locks.insert(Arc::from(account_id), Arc::downgrade(&lock));
    lock
}

/// Hold across context resolution and DHT work. Independent accounts and
/// same-key-generation readers remain concurrent. Only background migrations
/// take the write side; local credential commits never wait on network work.
pub async fn lock_identity_sync(account_id: &str) -> OwnedRwLockReadGuard<()> {
    identity_sync_lock(account_id).read_owned().await
}

struct Journal {
    pending: bool,
    secrets: Vec<SharedSecret>,
}

#[derive(Debug, Error)]
#[error("Waiting for password-key migration")]
pub struct PasswordRotationPending;

fn journal_parts(bytes: &[u8]) -> Result<(bool, &[u8])> {
    let (&pending, ciphertext) = bytes
        .strip_prefix(JOURNAL_MAGIC)
        .and_then(|bytes| bytes.split_first())
        .context("unsupported password-rotation journal format")?;
    ensure!(pending <= 1, "invalid password-rotation journal state");
    ensure!(!ciphertext.is_empty(), "empty password-rotation journal");
    Ok((pending != 0, ciphertext))
}

fn journal_aad(account_id: &str, pending: bool) -> Vec<u8> {
    let mut aad = JOURNAL_AAD.to_vec();
    aad.extend_from_slice(account_id.as_bytes());
    aad.push(u8::from(pending));
    aad
}

impl Journal {
    async fn decode(bytes: &[u8], account_id: &str, secret: &SharedSecret) -> Result<Self> {
        let (pending, ciphertext) = journal_parts(bytes)?;
        crypto_system!(crypto);
        let plaintext = Zeroizing::new(
            decrypt_with_nonce_aad(
                ciphertext,
                secret,
                &crypto,
                Some(&journal_aad(account_id, pending)),
            )
            .context("password-rotation key history is unavailable for this account")?,
        );
        ensure!(
            !plaintext.is_empty() && plaintext.len().is_multiple_of(SECRET_LEN),
            "invalid password-rotation key history"
        );
        Ok(Self {
            pending,
            secrets: plaintext
                .as_chunks::<SECRET_LEN>()
                .0
                .iter()
                .map(|bytes| SharedSecret::new(secret.kind(), BareSharedSecret::new(bytes)))
                .collect(),
        })
    }

    async fn encode(&self, account_id: &str, secret: &SharedSecret) -> Result<Bytes> {
        crypto_system!(crypto);
        let mut plaintext = Zeroizing::new(Vec::with_capacity(self.secrets.len() * SECRET_LEN));
        for previous in &self.secrets {
            ensure!(
                previous.kind() == secret.kind(),
                "password-rotation crypto kind changed"
            );
            plaintext.extend_from_slice(&previous.value().bytes());
        }
        let ciphertext = encrypt_with_nonce_aad(
            &plaintext,
            secret,
            &crypto,
            Some(&journal_aad(account_id, self.pending)),
        )?;
        let mut bytes = JOURNAL_MAGIC.to_vec();
        bytes.push(u8::from(self.pending));
        bytes.extend(ciphertext);
        Ok(bytes.into())
    }
}

async fn latest_rotation_op(user: &User) -> Result<Option<PendingOperation>> {
    let (pending, dead) = try_join!(pending_ops::list_pending(), pending_ops::list_dead_letter(),)?;
    Ok(pending
        .into_iter()
        .chain(dead)
        .filter(|op| {
            op.op_type == OpType::ReencryptSubkeys
                && op.identity_key.as_ref() == user.account_id()
                && op.username.as_ref() == user.username()
        })
        .max_by_key(|op| op.sequence_number))
}

async fn load_decoded_journal(user: &User, secret: &SharedSecret) -> Result<Option<Journal>> {
    if let Some(bytes) = store::load_password_rotation(user.account_id()).await? {
        return Ok(Some(
            Journal::decode(&bytes, user.account_id(), secret).await?,
        ));
    }
    if let Some(op) = latest_rotation_op(user).await? {
        return Ok(Some(
            Journal::decode(&op.payload, user.account_id(), secret).await?,
        ));
    }
    Ok(None)
}

pub(super) async fn validate_rotation_secret(user: &User, secret: &SharedSecret) -> Result<()> {
    load_decoded_journal(user, secret).await.map(|_| ())
}

/// Called under the username lock, without waiting for DHT work.
/// Every replacement journal carries the complete history under the new key.
pub(super) async fn prepare_rotation(
    user: &User,
    old_secret: &SharedSecret,
    new_secret: &SharedSecret,
) -> Result<Bytes> {
    let mut journal = load_decoded_journal(user, old_secret)
        .await?
        .unwrap_or(Journal {
            pending: false,
            secrets: Vec::new(),
        });
    if !journal.secrets.contains(old_secret) {
        journal.secrets.push(old_secret.clone());
    }
    journal.pending = true;
    journal.encode(user.account_id(), new_secret).await
}

/// Local-only publication gate. The caller holds the read lease; pending
/// migrations defer ordinary publication without consuming its retry budget.
pub async fn prepare_identity_sync(ctx: &IdentityContext) -> Result<()> {
    let _user_lock = lock_store(ctx.user.username().as_bytes()).await;
    // The account's liveness was already checked by the caller (dispatch's
    // `ensure!`, or `prepare_pull`), so the common no-journal case can return
    // before the user row is loaded and decoded.
    let Some(bytes) = store::load_password_rotation(ctx.user.account_id()).await? else {
        return Ok(());
    };
    let current = load_matching_user(ctx.user.username(), ctx.user.account_id())
        .await?
        .context("Identity publication account was deleted or replaced")?;
    if journal_parts(&bytes)?.0 {
        enqueue_pending_rotation(&current).await?;
        return Err(PasswordRotationPending.into());
    }
    Journal::decode(&bytes, current.account_id(), &ctx.secret).await?;
    Ok(())
}

pub(super) async fn run_rotation(ctx: &IdentityContext, payload: Option<&[u8]>) -> Result<()> {
    // No queue read lease may be held here. Already-running publishers finish
    // first, and later publishers observe the pending journal before writing.
    let _migration = identity_sync_lock(ctx.user.account_id())
        .write_owned()
        .await;
    let Some((ctx, bytes)) = load_or_stage_journal(ctx, payload).await? else {
        return Ok(());
    };
    let mut journal = Journal::decode(&bytes, ctx.user.account_id(), &ctx.secret).await?;
    if !journal.pending {
        return Ok(());
    }
    record::reencrypt_subkeys(
        &ctx.key,
        &journal.secrets,
        &ctx.secret,
        &ctx.keypair,
        &ctx.user,
    )
    .await?;

    let _user_lock = lock_store(ctx.user.username().as_bytes()).await;
    let Some(current) = load_matching_user(ctx.user.username(), ctx.user.account_id()).await?
    else {
        return Ok(());
    };
    if store::load_password_rotation(current.account_id())
        .await?
        .as_ref()
        != Some(&bytes)
    {
        // A newer local password commit owns its own pending journal and op.
        return Ok(());
    }
    journal.pending = false;
    users_table()
        .await?
        .store(
            COL_PASSWORD_ROTATION,
            current.account_id().as_bytes(),
            &journal.encode(current.account_id(), &ctx.secret).await?,
        )
        .await
}

async fn load_or_stage_journal(
    ctx: &IdentityContext,
    payload: Option<&[u8]>,
) -> Result<Option<(IdentityContext, Bytes)>> {
    let _user_lock = lock_store(ctx.user.username().as_bytes()).await;
    let Some(current) = load_matching_user(ctx.user.username(), ctx.user.account_id()).await?
    else {
        return Ok(None);
    };
    let ctx = IdentityContext::from_stored_user(current).await?;
    if let Some(bytes) = store::load_password_rotation(ctx.user.account_id()).await? {
        return Ok(Some((ctx, bytes)));
    }
    let Some(payload) = payload else {
        return Ok(None);
    };
    let journal = Journal::decode(payload, ctx.user.account_id(), &ctx.secret).await?;
    let bytes = journal.encode(ctx.user.account_id(), &ctx.secret).await?;
    users_table()
        .await?
        .store(COL_PASSWORD_ROTATION, ctx.key_bytes(), &bytes)
        .await?;
    Ok(Some((ctx, bytes)))
}

pub(super) async fn enqueue_pending_rotation(user: &User) -> Result<()> {
    if user.crypto_version() != CURRENT_CRYPTO_VERSION {
        return Ok(());
    }
    if user.dht_key().is_empty() {
        return Ok(());
    }
    let Some(bytes) = store::load_password_rotation(user.account_id()).await? else {
        return Ok(());
    };
    if !journal_parts(&bytes)?.0 {
        return Ok(());
    }
    // Idempotence shortcut only — it is NOT what prevents two rotation ops
    // from coexisting. This op carries no `target_key`, so its queue dedup key
    // is `(account_id, ReencryptSubkeys, None)`: a *newer* journal enqueued
    // for the same account replaces and deletes this row — and any dead-letter
    // row for it — in one batch (`sync::pending_ops::enqueue` →
    // `store_op_replacing`). The full history lives in the newest journal, so
    // a superseded op can never become valid again and is never replayed.
    if pending_ops::list_pending().await?.iter().any(|op| {
        op.identity_key.as_ref() == user.account_id()
            && op.op_type == OpType::ReencryptSubkeys
            && op.payload == bytes
    }) {
        return Ok(());
    }
    UserCtx::from_user(user)
        .enqueue(OpType::ReencryptSubkeys, bytes)
        .await
}

/// Startup recovery also covers accounts that are not currently signed in.
pub(super) async fn recover_pending_rotations() -> Result<()> {
    let table = users_table().await?;
    for key in table.raw().get_keys(COL_USER).await? {
        let _user_lock = lock_store(&key).await;
        let Some(bytes) = table.load_opt(COL_USER, &key).await? else {
            continue;
        };
        // One row that will not decode must not abort the scan for every other
        // account: this runs on the startup path, so a `?` here fails local
        // init for a row that can never succeed, and the retry loop holds the
        // splash while it waits for it to (see `app::retry`). The row is
        // unreachable either way; skipping it keeps the rest of the device
        // working and reports the one account that needs attention.
        let user = match User::from_packed_capnp(&bytes) {
            Ok(user) => user,
            Err(error) => {
                report_nonfatal(
                    "user.password",
                    "Skipping an account row that could not be read during password-rotation recovery",
                    format!("key={}: {error:#}", String::from_utf8_lossy(&key)),
                );
                continue;
            }
        };
        if user.crypto_version() != CURRENT_CRYPTO_VERSION {
            continue;
        }
        if load_matching_user(user.username(), user.account_id())
            .await?
            .is_some()
        {
            enqueue_pending_rotation(&user).await?;
        }
    }
    Ok(())
}

/// The journal is already durable; retry only its local queue handoff.
/// This is deliberately root-owned, not tied to the active login epoch.
pub(super) fn retry_rotation_enqueue(user: &User) {
    let username = user.username_arc();
    let account_id = user.account_id_arc();
    spawn_forever(async move {
        let mut delay = 1_000;
        loop {
            sleep_ms(delay).await;
            let result: Result<()> = try {
                let _lock = lock_store(username.as_bytes()).await;
                if let Some(user) = load_matching_user(&username, &account_id).await? {
                    enqueue_pending_rotation(&user).await?;
                }
            };
            match result {
                Ok(()) => return,
                Err(error) => report_nonfatal(
                    "user.password",
                    "Password rotation is durable but queue handoff still needs retry",
                    error,
                ),
            }
            delay = (delay * 2).min(60_000);
        }
    });
}
